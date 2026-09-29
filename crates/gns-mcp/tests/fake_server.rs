//! End-to-end checks against the Python fake MCP server (stdio and HTTP).

use gns_core::*;
use gns_mcp::{McpClient, flatten_call_result, tools_for_server};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn fixture() -> String {
    format!("{}/tests/fixtures/fake_mcp_server.py", env!("CARGO_MANIFEST_DIR"))
}

fn python() -> Option<String> {
    let found = std::process::Command::new("python3").arg("--version").output().is_ok();
    if !found {
        eprintln!("python3 not found; skipping MCP fake-server test");
    }
    found.then(|| "python3".to_owned())
}

#[tokio::test]
async fn stdio_server_lists_paginated_tools_and_calls_them() {
    let Some(py) = python() else { return };
    let config = McpServerConfig::stdio("fake", py, vec![fixture()]);
    let client = McpClient::connect(&config, None, Duration::from_secs(20)).await.unwrap();
    assert_eq!(client.info().name, "fake");
    assert_eq!(client.info().instructions.as_deref(), Some("be nice"));
    let names: Vec<String> = client.tools().iter().map(|t| t.name.clone()).collect();
    assert_eq!(names, vec!["echo", "fail", "slow", "env"], "both pages were fetched");

    let result = client.call_tool("echo", serde_json::json!({"text": "hi"}), CancellationToken::new()).await.unwrap();
    assert_eq!(flatten_call_result(&result), ("echo: hi".to_owned(), false));
    let result = client.call_tool("fail", serde_json::json!({}), CancellationToken::new()).await.unwrap();
    assert_eq!(flatten_call_result(&result), ("boom".to_owned(), true));
    let err = client.call_tool("nope", serde_json::json!({}), CancellationToken::new()).await.unwrap_err();
    assert!(err.to_string().contains("unknown tool"), "{err}");

    // Cancellation and the adapter's naming.
    let cancel = CancellationToken::new();
    let slow = client.call_tool("slow", serde_json::json!({"seconds": 5}), cancel.clone());
    cancel.cancel();
    assert!(matches!(slow.await, Err(gns_mcp::McpError::Cancelled)));
    let tools = tools_for_server(&client, &McpServerConfig { allowed_tools: vec!["echo".into()], ..config.clone() });
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name(), "mcp__fake__echo");
    assert!(tools[0].description().starts_with("[MCP fake] Echo"));
    client.close().await;
}

#[tokio::test]
async fn http_server_handles_json_and_sse_responses_with_a_session() {
    let Some(py) = python() else { return };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut server =
        tokio::process::Command::new(py).arg(fixture()).arg("--http").arg(port.to_string()).kill_on_drop(true).spawn().unwrap();
    // Wait for the port.
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let config = McpServerConfig::http("web", format!("http://127.0.0.1:{port}/mcp"));
    let client = McpClient::connect(&config, None, Duration::from_secs(20)).await.unwrap();
    assert_eq!(client.tools().len(), 4);
    let result = client.call_tool("echo", serde_json::json!({"text": "over http"}), CancellationToken::new()).await.unwrap();
    assert_eq!(flatten_call_result(&result).0, "echo: over http");
    client.close().await;
    let _ = server.kill().await;
}

#[tokio::test]
async fn a_server_that_dies_fails_fast() {
    let config = McpServerConfig::stdio("dead", "sh", vec!["-c".into(), "exit 3".into()]);
    let err = McpClient::connect(&config, None, Duration::from_secs(5)).await.unwrap_err();
    assert!(matches!(err, gns_mcp::McpError::Closed | gns_mcp::McpError::Transport(_)), "{err}");
    let missing = McpServerConfig::stdio("missing", "/definitely/not/here", vec![]);
    assert!(matches!(McpClient::connect(&missing, None, Duration::from_secs(5)).await.unwrap_err(), gns_mcp::McpError::Spawn(_)));
}
