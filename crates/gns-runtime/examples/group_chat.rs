//! A group chat round: the user posts into a room, only the @-mentioned
//! member answers, the others pass.
//!
//! ```text
//! OPENAI_API_KEY=... cargo run -p gns-runtime --example group_chat
//! OPENAI_BASE_URL=http://localhost:8000/v1 OPENAI_MODEL=qwen3-32b cargo run -p gns-runtime --example group_chat
//! GNS_MODEL=kimi-k2-turbo-preview KIMI_API_KEY=... cargo run -p gns-runtime --example group_chat
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
    let root = std::env::temp_dir().join("gns-group-chat");
    let mut config = AgentHostConfig::new(&root);
    config.kickstart_new_agents = false;
    let host = AgentHost::open(config, llm).await?;
    let mut rx = host.subscribe();
    let printer = tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            if let HostEvent::GroupPosted { speaker, content, .. } = event {
                println!("#room {speaker}: {content}");
            }
        }
    });

    let backend = host.create_agent(AgentSpec::new("Backend", "Senior backend engineer focused on databases and caching.")).await?;
    let frontend = host.create_agent(AgentSpec::new("Frontend", "Frontend lead focused on UI performance.")).await?;
    let security = host.create_agent(AgentSpec::new("Security", "Application security reviewer.")).await?;
    let room = host
        .create_group(GroupSpec {
            name: "War Room".into(),
            description: "Architecture & release planning".into(),
            members: vec![backend.id, frontend.id, security.id],
        })
        .await?;

    host.post_to_group_as_user(room.id.as_str(), "@Backend what's your take on moving sessions from Redis to Postgres?").await?;
    tokio::time::sleep(Duration::from_secs(30)).await;
    host.post_to_group_as_user(room.id.as_str(), "@everyone one risk each, one line.").await?;
    tokio::time::sleep(Duration::from_secs(60)).await;
    host.shutdown().await;
    printer.abort();
    Ok(())
}
