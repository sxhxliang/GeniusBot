//! Wire-level checks against a tiny fake OpenAI-compatible SSE server.

use gns_core::*;
use gns_llm::{GenaiConfig, GenaiProvider};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// Serve one HTTP response body (as SSE) and return the base URL.
async fn serve_once(body: &'static str) -> String {
    serve_once_with("200 OK", "text/event-stream", body).await.0
}

/// Serve one HTTP response; return the base URL and the raw request received.
async fn serve_once_with(status: &'static str, content_type: &'static str, body: &'static str) -> (String, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (received_tx, received) = oneshot::channel();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 65536];
        let mut read = 0;
        loop {
            let n = sock.read(&mut buf[read..]).await.unwrap();
            read += n;
            let head = String::from_utf8_lossy(&buf[..read]);
            if let Some(idx) = head.find("\r\n\r\n") {
                let len: usize = head
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                    .unwrap_or(0);
                if read >= idx + 4 + len {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        received_tx.send(String::from_utf8_lossy(&buf[..read]).into_owned()).ok();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(response.as_bytes()).await.unwrap();
        sock.shutdown().await.ok();
    });
    (format!("http://127.0.0.1:{port}/v1"), received)
}

fn request() -> LlmRequest {
    LlmRequest { system: "s".into(), messages: vec![LlmMessage::user("hi")], tools: vec![], options: Default::default() }
}

const TOOL_CHUNK: &str = r#"data: {"id":"x","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"SendMessage","arguments":"{\"type\":\"text\",\"content\":\"hi\"}"}}]},"finish_reason":null}]}

data: {"id":"x","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}

"#;

async fn provider(base: String) -> Arc<GenaiProvider> {
    Arc::new(GenaiProvider::new(GenaiConfig::openai_compatible(base, "fake-model").with_api_key("k")).unwrap())
}

#[tokio::test]
async fn stream_with_done_sentinel_yields_tool_calls() {
    let body: &'static str = Box::leak(format!("{TOOL_CHUNK}data: [DONE]\n\n").into_boxed_str());
    let base = serve_once(body).await;
    let p = provider(base).await;
    let r = p.complete(request(), CancellationToken::new(), &|_| {}).await.unwrap();
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].name, "SendMessage");
}

#[tokio::test]
async fn stream_closed_without_done_is_a_retryable_error_not_a_silent_text_turn() {
    let base = serve_once(TOOL_CHUNK).await;
    let p = provider(base).await;
    let err = p.complete(request(), CancellationToken::new(), &|_| {}).await.unwrap_err();
    assert!(err.is_retryable(), "{err}");
    assert!(err.to_string().contains("incomplete"), "{err}");
}

#[tokio::test]
async fn connection_refused_is_retryable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let p = provider(format!("http://127.0.0.1:{port}/v1")).await;
    let err = p.complete(request(), CancellationToken::new(), &|_| {}).await.unwrap_err();
    assert!(err.is_retryable(), "{err}");
}

#[tokio::test]
async fn gateway_requests_carry_no_prompt_cache_key() {
    let body: &'static str = Box::leak(format!("{TOOL_CHUNK}data: [DONE]\n\n").into_boxed_str());
    let (base, received) = serve_once_with("200 OK", "text/event-stream", body).await;
    let p = provider(base).await;
    let mut req = request();
    req.options.prompt_cache_key = Some("agent-1".into());
    p.complete(req, CancellationToken::new(), &|_| {}).await.unwrap();
    let raw = received.await.unwrap();
    assert!(raw.contains("\"model\":\"fake-model\""), "the body was captured: {raw}");
    assert!(!raw.contains("prompt_cache_key"), "{raw}");
    assert!(!raw.contains("tool_choice"), "ordinary requests keep automatic tool selection: {raw}");
}

#[tokio::test]
async fn required_send_message_reaches_streaming_and_non_streaming_gateways() {
    let stream_body: &'static str = Box::leak(format!("{TOOL_CHUNK}data: [DONE]\n\n").into_boxed_str());
    let json_body = r#"{"id":"x","object":"chat.completion","model":"fake-model","choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"SendMessage","arguments":"{\"type\":\"text\",\"content\":\"hi\"}"}}]},"finish_reason":"tool_calls"}]}"#;
    for stream in [true, false] {
        let (base, received) = serve_once_with(
            "200 OK",
            if stream { "text/event-stream" } else { "application/json" },
            if stream { stream_body } else { json_body },
        )
        .await;
        let mut config = GenaiConfig::openai_compatible(base, "fake-model").with_api_key("k");
        config.stream = stream;
        let p = GenaiProvider::new(config).unwrap();
        let mut req = request();
        req.tools.push(ToolSpec {
            name: SEND_MESSAGE_TOOL_NAME.into(),
            description: "Send a message".into(),
            parameters: serde_json::json!({"type":"object","properties":{"type":{"type":"string"},"content":{"type":"string"}},"required":["type","content"]}),
        });
        req.options.required_tool = Some(SEND_MESSAGE_TOOL_NAME.into());
        let response = p.complete(req, CancellationToken::new(), &|_| {}).await.unwrap();
        assert_eq!(response.tool_calls[0].name, SEND_MESSAGE_TOOL_NAME);
        let raw = received.await.unwrap();
        let body: serde_json::Value = serde_json::from_str(raw.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["tool_choice"], serde_json::json!({"type":"function","function":{"name":"SendMessage"}}));
        assert_eq!(body["tools"][0]["function"]["name"], SEND_MESSAGE_TOOL_NAME);
    }
}

#[tokio::test]
async fn http_errors_on_a_stream_keep_their_status() {
    let rejected = r#"{"message":"Validation: Unsupported parameter(s): `prompt_cache_key`","type":"Bad Request","code":400}"#;
    let (base, _) = serve_once_with("400 Bad Request", "application/json", rejected).await;
    let err = provider(base).await.complete(request(), CancellationToken::new(), &|_| {}).await.unwrap_err();
    assert!(matches!(err, LlmError::Provider { status: Some(400), retryable: false, .. }), "{err}");

    let (base, _) = serve_once_with("429 Too Many Requests", "application/json", r#"{"error":"slow down"}"#).await;
    let err = provider(base).await.complete(request(), CancellationToken::new(), &|_| {}).await.unwrap_err();
    assert!(matches!(err, LlmError::Provider { status: Some(429), retryable: true, .. }), "{err}");
}
