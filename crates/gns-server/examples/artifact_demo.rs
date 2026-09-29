//! Offline, isolated task/file console demo: cargo run -p gns-server --example artifact_demo
use gns_core::*;
use gns_llm::MockLlm;
use gns_runtime::{AgentHost, AgentHostConfig};
use gns_server::{AppState, EventBus, ModelHub, model::ModelSettings};
use serde_json::json;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let model = MockLlm::new().with_responder(|req| {
        if let Some(id) = req.system.split("## Delegated task ").nth(1).and_then(|s| s.lines().next()) {
            if req.messages.iter().rev().find_map(|m| match m { LlmMessage::User { text, .. } => Some(text.contains("Need source data")), _ => None }).unwrap_or(false) {
                return LlmResponse::tool_call("UpdateTask", json!({"task_id":id,"status":"blocked","summary":"Please provide the source data, then resume this task."}));
            }
            return LlmResponse::tool_call("CompleteTask", json!({"task_id":id,"status":"completed","summary":"The complete sorting example is ready to download.","verification":"Offline fixture; this demo does not execute the Python program.","files":["optimized_sorts.py"]}));
        }
        if matches!(req.messages.last(), Some(LlmMessage::ToolResults(_))) { return LlmResponse::text("done"); }
        LlmResponse::tool_call("SendMessage", json!({"type":"text","content":"The result is available in the task card above."}))
    });
    let mut config = AgentHostConfig::new(root.path());
    config.kickstart_new_agents = false;
    config.memory_extraction = false;
    config.dream_after_idle = None;
    let host = AgentHost::open(config, Arc::new(model)).await?;
    let bus = EventBus::new(1000);
    let forwarder = bus.forward(host.subscribe());
    let planner = host.create_agent(AgentSpec::new("Planner", "Review submitted files.")).await?;
    let coder = host.create_agent(AgentSpec::new("Coder", "Deliver full files.")).await?;
    std::fs::write(
        root.path().join("agents").join(coder.id.as_str()).join("workspace/optimized_sorts.py"),
        include_bytes!("../../gns-runtime/tests/fixtures/optimized_sorts.py"),
    )?;
    let request = DelegateTaskRequest {
        target_id: coder.id.to_string(),
        task: "Deliver the sorting example".into(),
        require_files: true,
        ..Default::default()
    };
    host.delegate_task(planner.id.as_str(), request.clone()).await?;
    let group = host
        .create_group(GroupSpec {
            name: "File review".into(),
            description: "Review task output files".into(),
            members: vec![planner.id.clone(), coder.id.clone()],
        })
        .await?;
    host.delegate_group_task(group.id.as_str(), planner.id.as_str(), request.clone()).await?;
    host.delegate_task(planner.id.as_str(), DelegateTaskRequest { task: "Need source data".into(), ..request }).await?;
    let hub = Arc::new(ModelHub::new(
        ModelSettings { provider: "mock".into(), ..Default::default() },
        root.path().join("gns-server.json"),
        50,
        bus.clone(),
        None,
    ));
    let state = AppState { host: host.clone(), hub, bus, token: Some("artifact-demo".into()) };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8799").await?;
    eprintln!("Demo: http://127.0.0.1:8799  token: artifact-demo  temporary root: {}", root.path().display());
    axum::serve(listener, gns_server::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    host.shutdown().await;
    forwarder.abort();
    Ok(())
}
