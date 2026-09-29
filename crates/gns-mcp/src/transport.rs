//! Transports: stdio (child process) and streamable HTTP.

use crate::error::McpError;
use crate::jsonrpc::{self, Incoming};
use crate::sse::SseParser;
use futures::StreamExt;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::oneshot;

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, McpError>>>>>;

/// Flags shared by both transports.
#[derive(Debug, Default)]
pub(crate) struct TransportState {
    /// The server announced `notifications/tools/list_changed`.
    pub tools_changed: AtomicBool,
    pub closed: AtomicBool,
}

pub(crate) enum Transport {
    Stdio(StdioTransport),
    Http(HttpTransport),
}

impl Transport {
    /// Send a request (`id` set) and wait for its response, or send a
    /// notification (`id` = None) and return `None`.
    pub(crate) async fn send(&self, message: Value, id: Option<i64>) -> Result<Option<Value>, McpError> {
        match self {
            Transport::Stdio(t) => t.send(message, id).await,
            Transport::Http(t) => t.send(message, id).await,
        }
    }
    pub(crate) fn state(&self) -> &TransportState {
        match self {
            Transport::Stdio(t) => &t.state,
            Transport::Http(t) => &t.state,
        }
    }
    pub(crate) async fn close(&self) {
        match self {
            Transport::Stdio(t) => t.close().await,
            Transport::Http(t) => t.close().await,
        }
    }
}

// ---------------------------------------------------------------- stdio

pub(crate) struct StdioTransport {
    child: Mutex<Option<Child>>,
    stdin: Arc<tokio::sync::Mutex<Option<ChildStdin>>>,
    pending: Pending,
    pub(crate) state: Arc<TransportState>,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl StdioTransport {
    pub(crate) fn spawn(
        server: &str,
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: Option<&Path>,
    ) -> Result<Self, McpError> {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .envs(env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let mut child = cmd.spawn().map_err(|e| McpError::Spawn(format!("{command}: {e}")))?;
        let stdin = child.stdin.take().ok_or_else(|| McpError::Spawn("no stdin pipe".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| McpError::Spawn("no stdout pipe".into()))?;
        let stderr = child.stderr.take();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let state = Arc::new(TransportState::default());
        let stdin = Arc::new(tokio::sync::Mutex::new(Some(stdin)));

        // Drain stderr so the child never blocks on a full pipe.
        if let Some(stderr) = stderr {
            let name = server.to_owned();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(mcp = %name, "stderr: {line}");
                }
            });
        }
        let reader = {
            let (pending, state, stdin, name) = (pending.clone(), state.clone(), stdin.clone(), server.to_owned());
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let Ok(value) = serde_json::from_str::<Value>(line) else {
                        tracing::debug!(mcp = %name, "ignoring non-JSON line: {}", gns_core::text::clamp_line(line, 120));
                        continue;
                    };
                    match jsonrpc::classify(value) {
                        Some(Incoming::Response { id, result }) => {
                            if let Some(tx) = pending.lock().ok().and_then(|mut p| p.remove(&id)) {
                                let _ = tx.send(result);
                            }
                        }
                        Some(Incoming::Request { id, method }) => {
                            let reply = server_request_reply(&id, &method);
                            if let Some(stdin) = stdin.lock().await.as_mut() {
                                let _ = write_line(stdin, &reply).await;
                            }
                        }
                        Some(Incoming::Notification { method }) if method == "notifications/tools/list_changed" => {
                            state.tools_changed.store(true, Ordering::SeqCst);
                        }
                        Some(Incoming::Notification { .. }) | None => {}
                    }
                }
                state.closed.store(true, Ordering::SeqCst);
                if let Ok(mut p) = pending.lock() {
                    for (_, tx) in p.drain() {
                        let _ = tx.send(Err(McpError::Closed));
                    }
                }
            })
        };
        Ok(Self { child: Mutex::new(Some(child)), stdin, pending, state, reader: Mutex::new(Some(reader)) })
    }

    async fn send(&self, message: Value, id: Option<i64>) -> Result<Option<Value>, McpError> {
        if self.state.closed.load(Ordering::SeqCst) {
            return Err(McpError::Closed);
        }
        let rx = id.map(|id| {
            let (tx, rx) = oneshot::channel();
            if let Ok(mut p) = self.pending.lock() {
                p.insert(id, tx);
            }
            rx
        });
        {
            let mut guard = self.stdin.lock().await;
            let stdin = guard.as_mut().ok_or(McpError::Closed)?;
            write_line(stdin, &message).await?;
        }
        match rx {
            None => Ok(None),
            Some(rx) => rx.await.map_err(|_| McpError::Closed)?.map(Some),
        }
    }

    async fn close(&self) {
        self.state.closed.store(true, Ordering::SeqCst);
        if let Some(mut stdin) = self.stdin.lock().await.take() {
            let _ = stdin.shutdown().await;
        }
        let child = self.child.lock().ok().and_then(|mut c| c.take());
        if let Some(mut child) = child {
            let _ = tokio::time::timeout(std::time::Duration::from_millis(500), child.wait()).await;
            let _ = child.kill().await;
        }
        if let Some(reader) = self.reader.lock().ok().and_then(|mut r| r.take()) {
            reader.abort();
        }
    }
}

async fn write_line(stdin: &mut ChildStdin, message: &Value) -> Result<(), McpError> {
    let mut line = serde_json::to_string(message).map_err(|e| McpError::Protocol(e.to_string()))?;
    line.push('\n');
    stdin.write_all(line.as_bytes()).await.map_err(|e| McpError::Transport(e.to_string()))?;
    stdin.flush().await.map_err(|e| McpError::Transport(e.to_string()))
}

/// Answer the few requests a server may send to a client.
fn server_request_reply(id: &Value, method: &str) -> Value {
    match method {
        "ping" => jsonrpc::response(id, serde_json::json!({})),
        "roots/list" => jsonrpc::response(id, serde_json::json!({"roots": []})),
        _ => jsonrpc::error_response(id, -32601, &format!("client does not support {method}")),
    }
}

// ---------------------------------------------------------------- streamable HTTP

pub(crate) struct HttpTransport {
    client: reqwest::Client,
    url: String,
    headers: BTreeMap<String, String>,
    session: Mutex<Option<String>>,
    pub(crate) state: Arc<TransportState>,
}

impl HttpTransport {
    pub(crate) fn new(url: &str, headers: &BTreeMap<String, String>) -> Result<Self, McpError> {
        let mut builder = reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(10));
        // Local servers must not be routed through an http_proxy from the environment.
        let is_local = reqwest::Url::parse(url)
            .ok()
            .and_then(|u| u.host().map(|h| h.to_string()))
            .is_some_and(|h| h == "localhost" || h.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback()));
        if is_local {
            builder = builder.no_proxy();
        }
        let client = builder.build().map_err(|e| McpError::Transport(e.to_string()))?;
        Ok(Self {
            client,
            url: url.to_owned(),
            headers: headers.clone(),
            session: Mutex::new(None),
            state: Arc::new(TransportState::default()),
        })
    }

    fn session(&self) -> Option<String> {
        self.session.lock().ok().and_then(|s| s.clone())
    }

    async fn send(&self, message: Value, id: Option<i64>) -> Result<Option<Value>, McpError> {
        let mut request = self
            .client
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .json(&message);
        for (k, v) in &self.headers {
            request = request.header(k, v);
        }
        if let Some(session) = self.session() {
            request = request.header("Mcp-Session-Id", session);
        }
        let response = request.send().await.map_err(|e| {
            let mut chain = e.to_string();
            let mut source = std::error::Error::source(&e);
            while let Some(s) = source {
                chain.push_str(&format!(": {s}"));
                source = s.source();
            }
            McpError::Transport(chain)
        })?;
        if let Some(session) = response.headers().get("mcp-session-id").and_then(|v| v.to_str().ok())
            && let Ok(mut s) = self.session.lock()
        {
            *s = Some(session.to_owned());
        }
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND
            && let Ok(mut s) = self.session.lock()
        {
            *s = None; // the session expired; the next connect starts a fresh one
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(McpError::Transport(format!("HTTP {status}: {}", gns_core::text::clamp_line(&body, 200))));
        }
        let Some(id) = id else {
            return Ok(None); // notification: 202/204 (or any success) is fine
        };
        if status == reqwest::StatusCode::ACCEPTED || status == reqwest::StatusCode::NO_CONTENT {
            return Err(McpError::Protocol("server accepted the request but sent no response".into()));
        }
        let content_type = response.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
        if content_type.starts_with("text/event-stream") {
            let mut parser = SseParser::default();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| McpError::Transport(e.to_string()))?;
                for data in parser.feed(&chunk) {
                    if let Some(result) = self.handle_payload(&data, id) {
                        return result.map(Some);
                    }
                }
            }
            if let Some(data) = parser.finish()
                && let Some(result) = self.handle_payload(&data, id)
            {
                return result.map(Some);
            }
            Err(McpError::Protocol("event stream ended before the response arrived".into()))
        } else {
            let body = response.text().await.map_err(|e| McpError::Transport(e.to_string()))?;
            self.handle_payload(&body, id)
                .unwrap_or_else(|| Err(McpError::Protocol("response did not contain a reply to the request".into())))
                .map(Some)
        }
    }

    /// Parse one JSON payload (object or batch); return the reply to `id` if present.
    fn handle_payload(&self, payload: &str, id: i64) -> Option<Result<Value, McpError>> {
        let value: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(e) => return Some(Err(McpError::Protocol(format!("invalid JSON from server: {e}")))),
        };
        let items = match value {
            Value::Array(items) => items,
            other => vec![other],
        };
        for item in items {
            match jsonrpc::classify(item) {
                Some(Incoming::Response { id: got, result }) if got == id => return Some(result),
                Some(Incoming::Notification { method }) if method == "notifications/tools/list_changed" => {
                    self.state.tools_changed.store(true, Ordering::SeqCst);
                }
                _ => {}
            }
        }
        None
    }

    async fn close(&self) {
        self.state.closed.store(true, Ordering::SeqCst);
        if let Some(session) = self.session() {
            let _ = self.client.delete(&self.url).header("Mcp-Session-Id", session).send().await;
        }
    }
}
