//! Turn middleware hooks for auditing, rate limiting or custom reminders.

use crate::ids::{AgentId, RunId};
use crate::llm::LlmRequest;
use crate::run::{RunOptions, RunResult};
use crate::transcript::TranscriptEntry;
use async_trait::async_trait;

/// Hooks invoked by the runner. All methods have no-op defaults.
#[async_trait]
pub trait RunMiddleware: Send + Sync {
    /// Called before every LLM call; may mutate the request.
    async fn before_llm_call(&self, _agent: &AgentId, _run: &RunId, _step: usize, _request: &mut LlmRequest) {}
    /// Called after every tool call with the persisted entry.
    async fn after_tool_call(&self, _agent: &AgentId, _run: &RunId, _entry: &TranscriptEntry) {}
    /// Called when a turn settles.
    async fn on_turn_settled(&self, _agent: &AgentId, _run: &RunId, _options: &RunOptions, _result: &RunResult) {}
}
