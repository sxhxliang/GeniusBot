//! Capability interfaces that tools use to reach the host without depending on
//! the runtime crate. Implementations MUST be non-blocking with respect to
//! other agents' turns: enqueue and return, never wait for another agent.

use crate::agent::{AgentAddress, AgentSpec, GroupAddress, ProfilePatch, SettingsPatch};
use crate::error::HostError;
use crate::event::HostEvent;
use crate::ids::{AgentId, EntryId, RunId};
use crate::mcp::{McpServerConfig, McpServerStatus, ToolListing};
use crate::memory::{MemoryScope, MemoryTier};
use crate::routine::{AutomationRecord, AutomationSpec};
use crate::run::UsageTotals;
use crate::transcript::{ImageRef, OutboundMessage};
use async_trait::async_trait;
use std::sync::Arc;

/// Memory operations available to `update_state`.
///
/// Outcome convention (shared by every state-writing service method): `Ok(text)`
/// is the success line shown to the model; `Err(HostError::Invalid(reason))`
/// is a *soft* failure the tool renders as `Not saved — <reason>` (the
/// original's `{ ok: false, reason }`); any other error is a real failure.
pub trait MemoryService: Send + Sync {
    /// Save a fact (`Remembered in <label> (<tier>): <fact>`). A duplicate or
    /// empty fact is a soft failure.
    fn write(&self, agent: &AgentId, fact: &str, tier: MemoryTier, scope: MemoryScope, project: Option<&str>) -> Result<String, HostError>;
    /// Drop the first fact recorded with exactly this text (`Forgot from <label>: <fact>`).
    fn forget(&self, agent: &AgentId, fact: &str, scope: MemoryScope, project: Option<&str>) -> Result<String, HostError>;
}

/// Render a state-write outcome the way `update_state` shows it to the model.
pub fn render_state_outcome(outcome: Result<String, HostError>) -> Result<String, HostError> {
    match outcome {
        Ok(text) => Ok(text),
        Err(HostError::Invalid(reason)) => Ok(format!("Not saved — {reason}")),
        Err(e) => Err(e),
    }
}

/// Routine operations available to `update_state`. Records come back with
/// their derived prose (`trigger_description`, `schedule`) and next run time;
/// `describe_trigger_for_reply` renders the trigger for a tool reply.
pub trait RoutineService: Send + Sync {
    /// Every routine of the agent, soonest next run first.
    fn list(&self, agent: &AgentId) -> Result<Vec<AutomationRecord>, HostError>;
    /// One routine by folder id (`None` when unknown).
    fn get(&self, agent: &AgentId, id: &str) -> Result<Option<AutomationRecord>, HostError> {
        Ok(self.list(agent)?.into_iter().find(|r| r.id == id))
    }
    fn create(&self, agent: &AgentId, spec: AutomationSpec) -> Result<AutomationRecord, HostError>;
    fn update(&self, agent: &AgentId, id: &str, spec: AutomationSpec) -> Result<AutomationRecord, HostError>;
    fn set_enabled(&self, agent: &AgentId, id: &str, enabled: bool) -> Result<AutomationRecord, HostError>;
    fn delete(&self, agent: &AgentId, id: &str) -> Result<(), HostError>;
}

/// Request to run an ephemeral subagent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentSpec {
    /// What the subagent must accomplish; it reports back in plain text.
    pub task: String,
    /// Short label for events and logs.
    pub label: String,
    /// Offer only `Read` (no Shell/Write/Edit).
    pub readonly: bool,
    pub max_steps: Option<usize>,
}

/// Outcome of a subagent run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubagentResult {
    /// The subagent's final report (its plain text).
    pub report: String,
    pub steps: usize,
    pub usage: UsageTotals,
    pub aborted: bool,
    pub error: Option<String>,
}

/// Host capabilities exposed to tools.
#[async_trait]
pub trait HostServices: Send + Sync {
    /// Snapshot owned files or resolve authorized references. No file content is returned.
    async fn prepare_artifacts(
        &self,
        _agent: &AgentId,
        files: Vec<String>,
        ids: Vec<String>,
    ) -> Result<Vec<crate::ArtifactRef>, HostError> {
        if files.is_empty() && ids.is_empty() {
            return Ok(Vec::new());
        }
        Err(HostError::invalid("artifact publishing is not supported by this host"))
    }
    async fn fetch_artifact(&self, _agent: &AgentId, _id: &str) -> Result<String, HostError> {
        Err(HostError::invalid("artifact fetching is not supported by this host"))
    }
    async fn send_to_agent_with_artifacts(
        &self,
        from: &AgentId,
        target: &str,
        text: String,
        images: Vec<ImageRef>,
        artifacts: Vec<crate::ArtifactRef>,
        priority: bool,
    ) -> Result<String, HostError> {
        if !artifacts.is_empty() {
            return Err(HostError::invalid("artifact messaging is not supported by this host"));
        }
        self.send_to_agent(from, target, text, images, priority).await
    }
    async fn delegate_task(
        &self,
        _from: &AgentId,
        _run: &RunId,
        _request: crate::DelegateTaskRequest,
    ) -> Result<crate::TaskRecord, HostError> {
        Err(HostError::invalid("task delegation is not supported by this host"))
    }
    async fn update_task(
        &self,
        _agent: &AgentId,
        _run: &RunId,
        _id: &str,
        _status: crate::TaskStatus,
        _summary: String,
    ) -> Result<crate::TaskRecord, HostError> {
        Err(HostError::invalid("task updates are not supported by this host"))
    }
    async fn complete_task(
        &self,
        _agent: &AgentId,
        _run: &RunId,
        _id: &str,
        _result: crate::CompleteTaskRequest,
    ) -> Result<crate::TaskRecord, HostError> {
        Err(HostError::invalid("task completion is not supported by this host"))
    }
    fn get_task(&self, _agent: &AgentId, _id: &str) -> Result<crate::TaskRecord, HostError> {
        Err(HostError::invalid("task queries are not supported by this host"))
    }
    /// Other agents (excluding groups).
    fn roster(&self) -> Vec<AgentAddress>;
    /// Groups the host knows about.
    fn groups(&self) -> Vec<GroupAddress>;
    /// Record and deliver a `SendMessage` payload for the user or the room.
    async fn deliver_message(&self, from: &AgentId, run: &RunId, message: OutboundMessage) -> Result<EntryId, HostError>;
    /// Cross-agent send. Returns the acknowledgement text for the model.
    async fn send_to_agent(
        &self,
        from: &AgentId,
        target_id: &str,
        text: String,
        images: Vec<ImageRef>,
        priority: bool,
    ) -> Result<String, HostError>;
    /// Create a teammate.
    async fn create_agent(&self, spec: AgentSpec) -> Result<AgentAddress, HostError>;
    /// Patch a teammate (or self). `None` when the id is unknown.
    async fn update_agent(&self, id: &str, patch: ProfilePatch) -> Result<Option<AgentAddress>, HostError>;
    /// Patch this agent's settings.
    fn update_settings(&self, id: &AgentId, patch: SettingsPatch) -> Result<(), HostError>;
    /// Memory service.
    fn memory(&self) -> Arc<dyn MemoryService>;
    /// Routine service.
    fn routines(&self) -> Arc<dyn RoutineService>;
    /// Publish an event to subscribers.
    fn emit(&self, event: HostEvent);
    /// Run an ephemeral subagent on behalf of `parent`, blocking until it
    /// reports (or the parent's turn is cancelled).
    async fn run_subagent(&self, parent: &AgentId, run: &RunId, spec: SubagentSpec) -> Result<SubagentResult, HostError>;
    /// Queue a hidden wake for an agent (background job results). Never waits.
    fn wake_agent(&self, agent: &AgentId, prompt: String) -> Result<(), HostError>;
    fn background_task_context(&self, _run: &RunId) -> Option<crate::TaskRunContext> {
        None
    }
    fn wake_agent_for_task(&self, agent: &AgentId, _task: Option<&crate::TaskRunContext>, prompt: String) -> Result<(), HostError> {
        self.wake_agent(agent, prompt)
    }
    /// User's IANA time zone, when known.
    fn time_zone(&self) -> Option<String> {
        None
    }
    /// The agent's effective tool list (built-ins and MCP tools, with enabled flags).
    fn list_tools(&self, agent: &AgentId) -> Result<Vec<ToolListing>, HostError> {
        let _ = agent;
        Err(HostError::Other("tool customisation is not supported by this host".into()))
    }
    /// Switch a built-in tool on or off for this agent.
    fn set_tool_enabled(&self, agent: &AgentId, tool: &str, enabled: bool) -> Result<(), HostError> {
        let _ = (agent, tool, enabled);
        Err(HostError::Other("tool customisation is not supported by this host".into()))
    }
    /// MCP servers attached to the agent and their connection state.
    fn mcp_servers(&self, agent: &AgentId) -> Result<Vec<McpServerStatus>, HostError> {
        let _ = agent;
        Err(HostError::Other("MCP is not supported by this host".into()))
    }
    /// Attach (or replace) an MCP server; it is connected on the agent's next turn.
    async fn add_mcp_server(&self, agent: &AgentId, config: McpServerConfig) -> Result<McpServerStatus, HostError> {
        let _ = (agent, config);
        Err(HostError::Other("MCP is not supported by this host".into()))
    }
    /// Detach an MCP server. Returns false when no such server was attached.
    async fn remove_mcp_server(&self, agent: &AgentId, name: &str) -> Result<bool, HostError> {
        let _ = (agent, name);
        Err(HostError::Other("MCP is not supported by this host".into()))
    }
    /// Enable or disable an attached MCP server without detaching it.
    async fn set_mcp_server_enabled(&self, agent: &AgentId, name: &str, enabled: bool) -> Result<McpServerStatus, HostError> {
        let _ = (agent, name, enabled);
        Err(HostError::Other("MCP is not supported by this host".into()))
    }
    /// Toggle an emoji reaction on one of the user's messages (`ReactToMessage`).
    /// `message_address` is the user entry's id (`t<n>u`). Returns the
    /// acknowledgement text for the model.
    async fn react_to_message(&self, from: &AgentId, run: &RunId, message_address: &str, emoji: &str) -> Result<String, HostError> {
        let _ = (from, run, message_address, emoji);
        Err(HostError::Other("reactions are not supported by this host".into()))
    }
    /// `update_state` target `project`, action `create` (create-is-join).
    /// Follows the [`MemoryService`] outcome convention.
    fn create_project(&self, agent: &AgentId, slug: &str, name: &str, description: Option<&str>) -> Result<String, HostError> {
        let _ = (agent, slug, name, description);
        Err(HostError::Other("projects are not supported by this host".into()))
    }
    /// `update_state` target `project`, action `join`.
    fn join_project(&self, agent: &AgentId, slug: &str) -> Result<String, HostError> {
        let _ = (agent, slug);
        Err(HostError::Other("projects are not supported by this host".into()))
    }
    /// `update_state` target `project`, action `leave`.
    fn leave_project(&self, agent: &AgentId, slug: &str) -> Result<String, HostError> {
        let _ = (agent, slug);
        Err(HostError::Other("projects are not supported by this host".into()))
    }
    /// `update_state` target `avatar`, action `set`: install the image at
    /// `path` as the agent's picture. Follows the [`MemoryService`] outcome convention.
    fn set_avatar(&self, agent: &AgentId, path: &str) -> Result<String, HostError> {
        let _ = (agent, path);
        Err(HostError::Other("avatars are not supported by this host".into()))
    }
    /// `update_state` target `avatar`, action `clear`.
    fn clear_avatar(&self, agent: &AgentId) -> Result<String, HostError> {
        let _ = agent;
        Err(HostError::Other("avatars are not supported by this host".into()))
    }
}
