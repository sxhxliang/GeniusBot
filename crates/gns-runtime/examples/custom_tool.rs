//! Register a custom typed tool and a custom prompt section, then run a turn
//! against the offline mock model (no API key needed).
//!
//! ```text
//! cargo run -p gns-runtime --example custom_tool
//! ```

use gns_core::prompt::{PromptContext, PromptSection, SectionPosition, section_ids};
use gns_core::*;
use gns_llm::MockLlm;
use gns_runtime::{AgentHost, AgentHostConfig};
use serde::Deserialize;
use std::sync::Arc;

/// A tool with typed arguments; the JSON schema is derived automatically.
#[derive(Debug)]
struct WeatherTool;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WeatherArgs {
    /// City name.
    city: String,
}

#[async_trait]
impl TypedTool for WeatherTool {
    type Args = WeatherArgs;
    fn name(&self) -> &str {
        "GetWeather"
    }
    fn description(&self) -> &str {
        "Look up today's weather for a city."
    }
    async fn run(&self, _ctx: &ToolContext, args: Self::Args) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(format!("{}: 22°C, sunny", args.city)))
    }
}

/// A custom system prompt block.
#[derive(Debug)]
struct HouseRules;

impl PromptSection for HouseRules {
    fn id(&self) -> &str {
        "house-rules"
    }
    fn render(&self, _ctx: &PromptContext) -> Option<String> {
        Some("House rules: always mention the data source when you report numbers.".to_owned())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A scripted model: call the tool, then answer the user.
    let mock = MockLlm::scripted(vec![
        LlmResponse::tool_call(SEND_MESSAGE_TOOL_NAME, serde_json::json!({"type":"text","content":"Checking the weather."})),
        LlmResponse::tool_call("GetWeather", serde_json::json!({"city":"Tokyo"})),
        LlmResponse::tool_call(
            SEND_MESSAGE_TOOL_NAME,
            serde_json::json!({"type":"text","content":"Tokyo is 22°C and sunny (source: GetWeather)."}),
        ),
        LlmResponse::text("done"),
    ]);
    let root = tempfile_dir();
    let mut config = AgentHostConfig::new(&root);
    config.kickstart_new_agents = false;
    let host = AgentHost::open(config, Arc::new(mock)).await?;
    host.register_tool(Typed::arc(WeatherTool));
    host.add_prompt_section(Arc::new(HouseRules), SectionPosition::After(section_ids::PROFILE.into()));

    let mut rx = host.subscribe();
    let agent = host.create_agent(AgentSpec::new("Forecaster", "You answer weather questions.")).await?;
    let result = host.send_user_message(agent.id.as_str(), "What's the weather in Tokyo?", vec![]).await?;
    while let Ok(event) = rx.try_recv() {
        match event {
            HostEvent::SendMessage { agent_name, message, .. } => println!("[{agent_name}] {}", message.display_text()),
            HostEvent::ToolCall { name, args, status: ToolCallStatus::Started, .. } => println!("· tool {name} {args}"),
            _ => {}
        }
    }
    println!("steps: {}, messages sent: {}", result.steps, result.sent_message_count);
    println!("tools available: {:?}", host.tool_names());
    host.shutdown().await;
    Ok(())
}

fn tempfile_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("gns-custom-tool-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}
