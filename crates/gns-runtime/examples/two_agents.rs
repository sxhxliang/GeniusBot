//! Two agents collaborating with durable tasks and complete file delivery.
//!
//! ```text
//! OPENAI_API_KEY=... cargo run -p gns-runtime --example two_agents
//! OPENAI_BASE_URL=http://localhost:8000/v1 OPENAI_MODEL=qwen3-32b cargo run -p gns-runtime --example two_agents
//! GNS_MODEL=kimi-k2-turbo-preview KIMI_API_KEY=... cargo run -p gns-runtime --example two_agents
//! ```

use gns_core::*;
use gns_llm::{GenaiConfig, GenaiProvider};
use gns_runtime::{AgentHost, AgentHostConfig};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let model =
        std::env::var("GNS_MODEL").or_else(|_| std::env::var(gns_llm::OPENAI_MODEL_ENV)).unwrap_or_else(|_| gns_llm::DEFAULT_MODEL.into());
    let llm = Arc::new(GenaiProvider::new(GenaiConfig::new(model))?);
    let root = std::env::temp_dir().join("gns-two-agents");
    let mut config = AgentHostConfig::new(&root);
    config.kickstart_new_agents = false;
    let host = AgentHost::open(config, llm).await?;

    let mut rx = host.subscribe();
    let printer = tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            match event {
                HostEvent::SendMessage { agent_name, message, .. } => println!("[{agent_name}] {}", message.display_text()),
                HostEvent::A2ASent { from, to, text, .. } => println!("· {from} → {to}: {text}"),
                HostEvent::ToolCall { name, status: ToolCallStatus::Started, agent_id, .. } => println!("· {agent_id} calls {name}"),
                HostEvent::TaskUpdated { task } => {
                    println!("Task {}: {:?} — {}{}", task.id, task.status, task.summary, artifact_prompt(&task.artifacts))
                }
                _ => {}
            }
        }
    });

    let planner = match host.find_agent("Planner") {
        Some(a) => a,
        None => {
            host.create_agent(AgentSpec::new(
                "Planner",
                "Delegate coding to Coder with DelegateTask and require_files=true. After the host delivers the files, use FetchArtifact to review them and explain the result to the user.",
            ))
            .await?
        }
    };
    let coder = match host.find_agent("Coder") {
        Some(a) => a,
        None => host.create_agent(AgentSpec::new("Coder", "Write files in your workspace with Shell, verify them, then submit the complete file paths and verification notes with CompleteTask. The host delivers them to the requester automatically.")).await?,
    };
    println!("Planner {} / Coder {} / root {}", planner.id, coder.id, root.display());

    host.send_user_message(
        planner.id.as_str(),
        "Have Coder create hello.py in their workspace that prints 'hello from Genius Bot', run it, and tell me the output.",
        vec![],
    )
    .await?;
    // Let the asynchronous back-and-forth play out.
    tokio::time::sleep(Duration::from_secs(45)).await;
    host.shutdown().await;
    printer.abort();
    Ok(())
}
