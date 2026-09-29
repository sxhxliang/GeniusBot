//! Per-agent tool customisation and MCP servers as extra tools.

use gns_core::*;
use gns_llm::MockLlm;
use gns_runtime::{AgentHost, AgentHostConfig};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

fn fixture() -> String {
    format!("{}/../gns-mcp/tests/fixtures/fake_mcp_server.py", env!("CARGO_MANIFEST_DIR"))
}

fn have_python() -> bool {
    let ok = std::process::Command::new("python3").arg("--version").output().is_ok();
    if !ok {
        eprintln!("python3 not found; skipping");
    }
    ok
}

fn send_message(text: &str) -> LlmResponse {
    LlmResponse::tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type": "text", "content": text}))
}

fn last_user(req: &LlmRequest) -> String {
    match req.messages.last() {
        Some(LlmMessage::User { text, .. }) => text.clone(),
        _ => String::new(),
    }
}

fn tool_results(req: &LlmRequest) -> Vec<LlmToolResult> {
    match req.messages.last() {
        Some(LlmMessage::ToolResults(r)) => r.clone(),
        _ => Vec::new(),
    }
}

async fn open_host(mock: MockLlm) -> (AgentHost, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = AgentHostConfig::new(dir.path());
    config.kickstart_new_agents = false;
    config.memory_extraction = false;
    config.dream_after_idle = None;
    let host = AgentHost::open(config, Arc::new(mock)).await.unwrap();
    (host, dir)
}

#[tokio::test]
async fn attached_server_tools_are_offered_called_and_described_in_the_prompt() {
    if !have_python() {
        return;
    }
    let mock = MockLlm::new().with_responder(|req| {
        let results = tool_results(req);
        if let Some(echo) = results.iter().find(|r| r.name == "mcp__fake__echo") {
            return send_message(&format!("The server said: {}", echo.content));
        }
        if !results.is_empty() {
            return LlmResponse::text("done");
        }
        if last_user(req).contains("echo") {
            return send_message("Asking the server.").and_tool_call("mcp__fake__echo", json!({"text": "hi there"}));
        }
        send_message("hi")
    });
    let (host, dir) = open_host(mock.clone()).await;
    let mut rx = host.subscribe();
    let agent = host.create_agent(AgentSpec::new("Tooler", "")).await.unwrap();

    let status = host.add_mcp_server(agent.id.as_str(), McpServerConfig::stdio("fake", "python3", vec![fixture()])).await.unwrap();
    assert!(status.connected, "{status:?}");
    assert_eq!(status.tools, vec!["mcp__fake__echo", "mcp__fake__fail", "mcp__fake__slow", "mcp__fake__env"]);
    // Persisted in settings.json.
    let settings = std::fs::read_to_string(dir.path().join("agents").join(agent.id.as_str()).join("settings.json")).unwrap();
    assert!(settings.contains("\"mcpServers\"") && settings.contains("fake_mcp_server.py"), "{settings}");

    let result = host.send_user_message(agent.id.as_str(), "please echo something", vec![]).await.unwrap();
    assert_eq!(result.sent_message_count, 2);
    let sent: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|e| match e {
            HostEvent::SendMessage { message, .. } => message.content,
            _ => None,
        })
        .collect();
    assert!(sent.iter().any(|s| s == "The server said: echo: hi there"), "{sent:?}");
    let first = &mock.requests()[0];
    assert!(first.tools.iter().any(|t| t.name == "mcp__fake__echo" && t.description.starts_with("[MCP fake]")));
    assert!(first.system.contains("Extra tools from MCP servers"), "{}", first.system);
    assert!(first.system.contains("fake: 4 tool(s)"));

    let listing = host.list_tools(agent.id.as_str()).unwrap();
    assert!(listing.iter().any(|t| t.name == "mcp__fake__echo" && matches!(&t.source, ToolSource::Mcp { server } if server == "fake")));

    assert!(host.remove_mcp_server(agent.id.as_str(), "fake").await.unwrap());
    assert!(host.list_tools(agent.id.as_str()).unwrap().iter().all(|t| !t.name.starts_with("mcp__")));
    assert!(!host.remove_mcp_server(agent.id.as_str(), "fake").await.unwrap());
    host.shutdown().await;
}

#[tokio::test]
async fn built_in_tools_can_be_switched_off_except_send_message() {
    let mock =
        MockLlm::new().with_responder(|req| if tool_results(req).is_empty() { send_message("ok") } else { LlmResponse::text("done") });
    let (host, _dir) = open_host(mock.clone()).await;
    let agent = host.create_agent(AgentSpec::new("Quiet", "")).await.unwrap();
    host.set_tool_enabled(agent.id.as_str(), SHELL_TOOL_NAME, false).unwrap();
    assert!(host.set_tool_enabled(agent.id.as_str(), SEND_MESSAGE_TOOL_NAME, false).is_err());
    assert!(host.set_tool_enabled(agent.id.as_str(), "NoSuchTool", false).is_err());
    let listing = host.list_tools(agent.id.as_str()).unwrap();
    assert!(listing.iter().any(|t| t.name == SHELL_TOOL_NAME && !t.enabled));
    host.send_user_message(agent.id.as_str(), "hello", vec![]).await.unwrap();
    let req = &mock.requests()[0];
    assert!(req.tools.iter().all(|t| t.name != SHELL_TOOL_NAME));
    assert!(req.tools.iter().any(|t| t.name == SEND_MESSAGE_TOOL_NAME));
    assert!(req.system.contains("Built-in tools currently switched off for you: Shell"));
    host.set_tool_enabled(agent.id.as_str(), SHELL_TOOL_NAME, true).unwrap();
    host.send_user_message(agent.id.as_str(), "hello again", vec![]).await.unwrap();
    assert!(mock.requests().last().unwrap().tools.iter().any(|t| t.name == SHELL_TOOL_NAME));
    host.shutdown().await;
}

#[tokio::test]
async fn a_broken_server_is_reported_and_does_not_break_turns() {
    let mock = MockLlm::new()
        .with_responder(|req| if tool_results(req).is_empty() { send_message("still here") } else { LlmResponse::text("done") });
    let (host, _dir) = open_host(mock.clone()).await;
    let mut rx = host.subscribe();
    let agent = host.create_agent(AgentSpec::new("Brave", "")).await.unwrap();
    let status = host.add_mcp_server(agent.id.as_str(), McpServerConfig::stdio("broken", "/no/such/binary", vec![])).await.unwrap();
    assert!(!status.connected && status.last_error.is_some(), "{status:?}");
    let result = host.send_user_message(agent.id.as_str(), "hello", vec![]).await.unwrap();
    assert_eq!(result.sent_message_count, 1);
    assert!(mock.requests()[0].system.contains("broken: not connected"));
    let failed =
        std::iter::from_fn(|| rx.try_recv().ok()).any(|e| matches!(e, HostEvent::McpServerFailed { server, .. } if server == "broken"));
    assert!(failed);
    assert!(host.mcp_servers(agent.id.as_str()).unwrap()[0].last_error.is_some());
    host.shutdown().await;
}

#[tokio::test]
async fn a_server_attached_by_the_host_is_usable_and_can_be_disabled() {
    if !have_python() {
        return;
    }
    let fixture = fixture();
    let mock = MockLlm::new().with_responder(move |req| {
        let results = tool_results(req);
        let env_result = results.iter().find(|r| r.name == "mcp__fake__env").map(|r| r.content.clone());
        if let Some(value) = env_result {
            return send_message(&format!("marker={value}"));
        }
        if !results.is_empty() {
            return LlmResponse::text("done");
        }
        if last_user(req).contains("read the marker") {
            assert!(req.tools.iter().any(|t| t.name == "mcp__fake__env"), "attached tools are offered");
            return send_message("Reading.").and_tool_call("mcp__fake__env", json!({"name": "GNS_TEST_MARKER"}));
        }
        send_message("hi")
    });
    let (host, _dir) = open_host(mock.clone()).await;
    let mut rx = host.subscribe();
    let agent = host.create_agent(AgentSpec::new("Self", "")).await.unwrap();
    let status = host
        .add_mcp_server(
            agent.id.as_str(),
            McpServerConfig {
                name: "fake".into(),
                transport: McpTransport::Stdio {
                    command: "python3".into(),
                    args: vec![fixture],
                    env: [("GNS_TEST_MARKER".to_owned(), "yes".to_owned())].into_iter().collect(),
                    cwd: None,
                },
                enabled: true,
                allowed_tools: Vec::new(),
                timeout_secs: None,
            },
        )
        .await
        .unwrap();
    assert!(status.connected, "{status:?}");
    let result = host.send_user_message(agent.id.as_str(), "read the marker", vec![]).await.unwrap();
    assert!(result.error.is_none(), "{result:?}");
    let sent: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|e| match e {
            HostEvent::SendMessage { message, .. } => message.content,
            _ => None,
        })
        .collect();
    assert!(sent.iter().any(|s| s == "marker=yes"), "{sent:?}");
    // Disabling keeps the config but drops the tools; the process is stopped.
    let status = host.set_mcp_server_enabled(agent.id.as_str(), "fake", false).await.unwrap();
    assert!(!status.enabled && !status.connected);
    assert!(host.list_tools(agent.id.as_str()).unwrap().iter().all(|t| !t.name.starts_with("mcp__")));
    host.shutdown().await;
}

#[tokio::test]
async fn servers_reconnect_after_a_restart_and_a_dead_server_is_retried() {
    if !have_python() {
        return;
    }
    let mock =
        MockLlm::new().with_responder(|req| if tool_results(req).is_empty() { send_message("ok") } else { LlmResponse::text("done") });
    let dir = tempfile::tempdir().unwrap();
    let mut config = AgentHostConfig::new(dir.path());
    config.kickstart_new_agents = false;
    config.memory_extraction = false;
    config.dream_after_idle = None;
    let host = AgentHost::open(config.clone(), Arc::new(mock.clone())).await.unwrap();
    let agent = host.create_agent(AgentSpec::new("Persist", "")).await.unwrap();
    host.add_mcp_server(agent.id.as_str(), McpServerConfig::stdio("fake", "python3", vec![fixture()])).await.unwrap();
    host.shutdown().await;

    let host = AgentHost::open(config, Arc::new(mock.clone())).await.unwrap();
    assert!(!host.mcp_servers(agent.id.as_str()).unwrap()[0].connected, "connected lazily, on the first turn");
    host.send_user_message(agent.id.as_str(), "hello", vec![]).await.unwrap();
    assert!(host.mcp_servers(agent.id.as_str()).unwrap()[0].connected);
    assert!(mock.requests().last().unwrap().tools.iter().any(|t| t.name == "mcp__fake__echo"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    host.shutdown().await;
}
