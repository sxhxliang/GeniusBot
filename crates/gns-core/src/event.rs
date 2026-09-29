//! Events published by the host for UIs, CLIs and tests.

use crate::agent::{AgentAddress, GroupAddress};
use crate::ids::{AgentId, EntryId, GroupId, RunId};
use crate::policy::ApprovalRequest;
use crate::run::{Lane, RunResult, RunSource};
use crate::transcript::{OutboundMessage, ToolCallStatus};
use serde::{Deserialize, Serialize};

/// A host event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum HostEvent {
    TaskUpdated {
        task: Box<crate::TaskRecord>,
    },
    RunStarted {
        agent_id: AgentId,
        run_id: RunId,
        lane: Lane,
        source: RunSource,
    },
    TextDelta {
        agent_id: AgentId,
        run_id: RunId,
        text: String,
    },
    ThinkingDelta {
        agent_id: AgentId,
        run_id: RunId,
        text: String,
    },
    ToolCall {
        agent_id: AgentId,
        run_id: RunId,
        call_id: String,
        name: String,
        args: serde_json::Value,
        status: ToolCallStatus,
        summary: Option<String>,
    },
    SendMessage {
        agent_id: AgentId,
        agent_name: String,
        run_id: RunId,
        entry_id: EntryId,
        message: OutboundMessage,
        group_id: Option<GroupId>,
    },
    /// The run's model loop finished (`runLifecycle "ended"`), before settle and nudges.
    RunEnded {
        agent_id: AgentId,
        run_id: RunId,
    },
    /// A transient model error is being retried.
    Retrying {
        agent_id: AgentId,
        run_id: RunId,
        attempt: u32,
        error: String,
    },
    TurnEnded {
        agent_id: AgentId,
        run_id: RunId,
        result: RunResult,
    },
    /// A transcript entry was persisted (user messages, hidden prompts, room posts, dividers).
    EntryAppended {
        agent_id: AgentId,
        entry: crate::transcript::TranscriptEntry,
    },
    /// A transcript entry was replaced (tool results, widget answers, reactions).
    EntryUpdated {
        agent_id: AgentId,
        entry: crate::transcript::TranscriptEntry,
    },
    /// A queued user turn was skipped because a newer send supersedes it.
    TurnSuperseded {
        agent_id: AgentId,
    },
    Interrupted {
        agent_id: AgentId,
        run_id: RunId,
        reason: String,
    },
    AgentCreated {
        address: AgentAddress,
    },
    AgentUpdated {
        address: AgentAddress,
    },
    GroupCreated {
        group: GroupAddress,
    },
    A2ASent {
        from: AgentId,
        to: String,
        priority: bool,
        text: String,
    },
    A2AQueued {
        to: AgentId,
        pending: usize,
    },
    GroupPosted {
        group_id: GroupId,
        speaker: String,
        content: String,
    },
    RoutineFired {
        agent_id: AgentId,
        automation_id: String,
        name: String,
    },
    MemoryWritten {
        agent_id: AgentId,
        scope: String,
        fact: String,
    },
    AgentDeleted {
        agent_id: AgentId,
    },
    SubagentStarted {
        parent_id: AgentId,
        run_id: RunId,
        label: String,
    },
    SubagentEnded {
        parent_id: AgentId,
        run_id: RunId,
        label: String,
        steps: usize,
        aborted: bool,
    },
    BackgroundJobStarted {
        agent_id: AgentId,
        job_id: String,
        command: String,
    },
    BackgroundJobFinished {
        agent_id: AgentId,
        job_id: String,
        exit_code: i32,
    },
    WebhookFired {
        name: String,
        routines: usize,
    },
    MemoryDreamed {
        agent_id: AgentId,
        added: usize,
        removed: usize,
    },
    GroupDeleted {
        group_id: GroupId,
    },
    /// A tool call is waiting for a human decision (see `AgentHost::approve`).
    ApprovalRequested {
        request: ApprovalRequest,
    },
    ApprovalResolved {
        id: String,
        approved: bool,
        reason: String,
    },
    Error {
        agent_id: Option<AgentId>,
        run_id: Option<RunId>,
        message: String,
    },
    /// An MCP server attached to an agent is connected and its tools are available.
    McpServerConnected {
        agent_id: AgentId,
        server: String,
        tools: Vec<String>,
    },
    /// An MCP server could not be reached; the agent runs without its tools.
    McpServerFailed {
        agent_id: AgentId,
        server: String,
        error: String,
    },
    /// The agent's effective tool list changed (settings edit or server (dis)connect).
    ToolsChanged {
        agent_id: AgentId,
    },
}
