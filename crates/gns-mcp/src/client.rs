//! The MCP client: handshake, tool listing, tool calls.

use crate::error::McpError;
use crate::jsonrpc;
use crate::transport::{HttpTransport, StdioTransport, Transport};
use gns_core::{MCP_CALL_TIMEOUT_SECS, McpServerConfig, McpToolDef, McpTransport};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Protocol versions we speak, newest first.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-03-26", "2024-11-05"];

/// What the server told us about itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub protocol_version: String,
    pub instructions: Option<String>,
}

/// A connected MCP server.
pub struct McpClient {
    name: String,
    transport: Transport,
    next_id: AtomicI64,
    call_timeout: Duration,
    info: ServerInfo,
    tools: RwLock<Vec<McpToolDef>>,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient").field("name", &self.name).field("info", &self.info).finish_non_exhaustive()
    }
}

impl McpClient {
    /// Start (or reach) the server, run the `initialize` handshake and fetch
    /// its tool list. `workspace` is the default working directory of stdio
    /// servers.
    pub async fn connect(config: &McpServerConfig, workspace: Option<&Path>, connect_timeout: Duration) -> Result<Arc<Self>, McpError> {
        config.validate().map_err(McpError::Protocol)?;
        let transport = match &config.transport {
            McpTransport::Stdio { command, args, env, cwd } => {
                let cwd = cwd.as_deref().map(Path::new).or(workspace);
                Transport::Stdio(StdioTransport::spawn(&config.name, command, args, env, cwd)?)
            }
            McpTransport::Http { url, headers } => Transport::Http(HttpTransport::new(url, headers)?),
        };
        let client = Self {
            name: config.name.clone(),
            transport,
            next_id: AtomicI64::new(1),
            call_timeout: Duration::from_secs(config.timeout_secs.unwrap_or(MCP_CALL_TIMEOUT_SECS)),
            info: ServerInfo::default(),
            tools: RwLock::new(Vec::new()),
        };
        let handshake = async {
            let init = client
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": PROTOCOL_VERSIONS[0],
                        "capabilities": {},
                        "clientInfo": {"name": "gns-mcp", "title": "Genius Bot", "version": env!("CARGO_PKG_VERSION")},
                    }),
                )
                .await?;
            let info = ServerInfo {
                name: init.pointer("/serverInfo/name").and_then(Value::as_str).unwrap_or(&config.name).to_owned(),
                version: init.pointer("/serverInfo/version").and_then(Value::as_str).unwrap_or("").to_owned(),
                protocol_version: init.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSIONS[0]).to_owned(),
                instructions: init.get("instructions").and_then(Value::as_str).map(str::to_owned),
            };
            client.transport.send(jsonrpc::notification("notifications/initialized", json!({})), None).await?;
            Ok::<ServerInfo, McpError>(info)
        };
        let info = match tokio::time::timeout(connect_timeout, handshake).await {
            Ok(Ok(info)) => info,
            Ok(Err(e)) => {
                client.transport.close().await;
                return Err(e);
            }
            Err(_) => {
                client.transport.close().await;
                return Err(McpError::Timeout(format!("initialize took longer than {}s", connect_timeout.as_secs())));
            }
        };
        let client = Arc::new(Self { info, ..client });
        match tokio::time::timeout(connect_timeout, client.refresh_tools()).await {
            Ok(Ok(())) => Ok(client),
            Ok(Err(e)) => {
                client.transport.close().await;
                Err(e)
            }
            Err(_) => {
                client.transport.close().await;
                Err(McpError::Timeout("tools/list took too long".into()))
            }
        }
    }

    /// The server handle this client was configured with.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Handshake details.
    pub fn info(&self) -> &ServerInfo {
        &self.info
    }
    /// The cached tool list (from the last refresh).
    pub fn tools(&self) -> Vec<McpToolDef> {
        self.tools.read().map(|t| t.clone()).unwrap_or_default()
    }
    /// True once the server announced a changed tool list (cleared by [`refresh_tools`](Self::refresh_tools)).
    pub fn tools_changed(&self) -> bool {
        self.transport.state().tools_changed.load(Ordering::SeqCst)
    }
    /// True when the connection is gone.
    pub fn is_closed(&self) -> bool {
        self.transport.state().closed.load(Ordering::SeqCst)
    }

    /// Re-fetch `tools/list` (all pages).
    pub async fn refresh_tools(&self) -> Result<(), McpError> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let page = self.request("tools/list", params).await?;
            let tools: Vec<McpToolDef> = serde_json::from_value(page.get("tools").cloned().unwrap_or(Value::Array(Vec::new())))
                .map_err(|e| McpError::Protocol(format!("bad tools/list: {e}")))?;
            all.extend(tools);
            cursor = page.get("nextCursor").and_then(Value::as_str).map(str::to_owned);
            if cursor.is_none() || all.len() > 500 {
                break;
            }
        }
        if let Ok(mut t) = self.tools.write() {
            *t = all;
        }
        self.transport.state().tools_changed.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Invoke a tool. Returns the raw `tools/call` result object.
    pub async fn call_tool(&self, tool: &str, arguments: Value, cancel: CancellationToken) -> Result<Value, McpError> {
        let arguments = if arguments.is_object() { arguments } else { json!({}) };
        let call = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        tokio::select! {
            _ = cancel.cancelled() => Err(McpError::Cancelled),
            r = tokio::time::timeout(self.call_timeout, call) => match r {
                Ok(r) => r,
                Err(_) => Err(McpError::Timeout(format!("{tool} exceeded {}s", self.call_timeout.as_secs()))),
            },
        }
    }

    /// Stop the server process / end the session.
    pub async fn close(&self) {
        self.transport.close().await;
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.transport.send(jsonrpc::request(id, method, params), Some(id)).await?.ok_or(McpError::Protocol("missing response".into()))
    }
}
