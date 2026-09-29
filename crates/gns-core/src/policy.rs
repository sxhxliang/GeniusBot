//! Tool-call policy: allow, deny, or require a human decision before a tool
//! runs. Policies are consulted in registration order; the first non-`Allow`
//! decision wins.

use crate::ids::{AgentId, RunId};
use crate::tool::ToolContext;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Outcome of a policy check.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PolicyDecision {
    /// Run the tool.
    Allow,
    /// Refuse; the reason is shown to the model.
    Deny(String),
    /// Pause the turn until a human approves or denies.
    RequireApproval(String),
}

/// A guard consulted before every tool call.
#[async_trait]
pub trait ToolPolicy: Send + Sync {
    /// Stable name for logs and events.
    fn name(&self) -> &str;
    /// Decide whether `tool` may run with `args`.
    async fn check(&self, ctx: &ToolContext, tool: &str, args: &serde_json::Value) -> PolicyDecision;
}

/// A tool call waiting for a decision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub agent_id: AgentId,
    pub agent_name: String,
    pub run_id: RunId,
    pub tool: String,
    pub args: serde_json::Value,
    pub reason: String,
    pub policy: String,
    pub requested_at: i64,
    pub expires_at: i64,
}

/// Text handed back to the model when a policy blocks a call.
pub fn blocked_tool_result(reason: &str) -> String {
    format!(
        "Policy blocked this action: {reason}. Do not retry the same action via a more evasive route (different flags, encoding, or another tool). Explain to the user what you wanted to do and why it was blocked, and ask how they'd like to proceed."
    )
}

/// Text handed back to the model when a human denied the call.
pub fn denied_tool_result(reason: &str) -> String {
    format!("The user declined this action ({reason}). Do not retry it; tell the user what you were trying to do and continue without it.")
}

/// Text handed back to the model when nobody answered in time.
pub fn approval_timed_out_result() -> String {
    "No decision arrived in time, so this action was not run. Tell the user it needs their approval and continue with what you can."
        .to_owned()
}
