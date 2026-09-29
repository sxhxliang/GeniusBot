//! `CreateAgent` and `UpdateAgent` (`sand-agent-management-tools.ts`).

use async_trait::async_trait;
use gns_core::*;
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;

/// Arguments of `CreateAgent`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateAgentArgs {
    /// A short, human-readable name for the new agent.
    pub name: String,
    /// The new agent's persona / instructions: what it is for and how it should behave. This becomes its profile and shapes its replies. Optional but strongly recommended.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Default)]
pub struct CreateAgentTool;

impl CreateAgentTool {
    pub fn arc() -> Arc<dyn Tool> {
        Typed::arc(Self)
    }
}

#[async_trait]
impl TypedTool for CreateAgentTool {
    type Args = CreateAgentArgs;
    fn name(&self) -> &str {
        CREATE_AGENT_TOOL_NAME
    }
    fn description(&self) -> &str {
        "Create a new agent (a new teammate assistant) for your user, with a name and an optional persona/description. Returns the new agent's id so you can immediately message it with SendToAgent. Use this to spin up a focused teammate for a job. You have no tool to delete an agent, so only create one when it is genuinely useful; the user can delete an agent themselves from the sidebar (right-click the agent → \"Delete\")."
    }
    async fn run(&self, ctx: &ToolContext, args: Self::Args) -> Result<ToolOutput, ToolError> {
        let name = args.name.trim();
        if name.is_empty() {
            return Err(ToolError::input("name is required"));
        }
        let created = ctx.services.create_agent(AgentSpec::new(name, args.description.unwrap_or_default().trim())).await?;
        Ok(ToolOutput::text(format!("Created agent \"{}\" (id: {}). Message it with SendToAgent using that id.", created.name, created.id)))
    }
}

/// Arguments of `UpdateAgent`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateAgentArgs {
    /// The id of the agent to update.
    pub agent_id: String,
    /// A new name for the agent. Omit to leave the name unchanged.
    #[serde(default)]
    pub name: Option<String>,
    /// A new persona/description for the agent. Omit to leave it unchanged.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Default)]
pub struct UpdateAgentTool;

impl UpdateAgentTool {
    pub fn arc() -> Arc<dyn Tool> {
        Typed::arc(Self)
    }
}

#[async_trait]
impl TypedTool for UpdateAgentTool {
    type Args = UpdateAgentArgs;
    fn name(&self) -> &str {
        UPDATE_AGENT_TOOL_NAME
    }
    fn description(&self) -> &str {
        "Edit an existing agent's profile: its name and/or description. Only the fields you provide are changed; the rest are left exactly as they were, and there is no way to clear or delete an agent through this tool. Use it to refine a teammate you (or the user) created."
    }
    async fn run(&self, ctx: &ToolContext, args: Self::Args) -> Result<ToolOutput, ToolError> {
        let patch = ProfilePatch {
            name: args.name.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()),
            description: args.description.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()),
            title: None,
        };
        if patch.is_empty() {
            return Ok(ToolOutput::text("Nothing to update: provide a new name and/or description."));
        }
        let id = args.agent_id.trim().to_owned();
        Ok(match ctx.services.update_agent(&id, patch).await? {
            Some(updated) => {
                ToolOutput::text(format!("Updated agent \"{}\" (id: {}).", updated.name, updated.id)).with_summary(format!("target: {id}"))
            }
            None => ToolOutput::text(format!("No agent found with id {id}.")),
        })
    }
}
