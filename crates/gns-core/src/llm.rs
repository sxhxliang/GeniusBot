//! Provider-agnostic LLM request/response model and the [`LlmProvider`] trait.
//! `gns-llm` adapts this onto `genai`; tests use a scripted mock.

use crate::error::LlmError;
use crate::run::UsageTotals;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// A tool call requested by the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LlmToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// A tool result fed back to the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LlmToolResult {
    pub call_id: String,
    pub name: String,
    pub content: String,
}

/// One message in the conversation sent to the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum LlmMessage {
    User { text: String, images: Vec<crate::transcript::ImageRef> },
    Assistant { text: Option<String>, tool_calls: Vec<LlmToolCall> },
    ToolResults(Vec<LlmToolResult>),
}

impl LlmMessage {
    /// Plain user text.
    pub fn user(text: impl Into<String>) -> Self {
        LlmMessage::User { text: text.into(), images: Vec::new() }
    }
    /// Plain assistant text.
    pub fn assistant_text(text: impl Into<String>) -> Self {
        LlmMessage::Assistant { text: Some(text.into()), tool_calls: Vec::new() }
    }
}

/// A tool exposed to the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Per-request generation options.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LlmOptions {
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    /// Stable key for provider-side prompt caching (e.g. the agent id). A
    /// hint: providers drop it for endpoints that do not accept it.
    pub prompt_cache_key: Option<String>,
}

/// A complete request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LlmRequest {
    pub system: String,
    pub messages: Vec<LlmMessage>,
    pub tools: Vec<ToolSpec>,
    pub options: LlmOptions,
}

/// A complete response.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LlmResponse {
    pub text: Option<String>,
    pub reasoning: Option<String>,
    pub tool_calls: Vec<LlmToolCall>,
    pub usage: UsageTotals,
    pub stop_reason: Option<String>,
}

impl LlmResponse {
    /// Text-only response helper (handy in tests).
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: Some(text.into()), ..Default::default() }
    }
    /// Tool-call response helper (handy in tests).
    pub fn tool_call(name: impl Into<String>, arguments: serde_json::Value) -> Self {
        Self {
            tool_calls: vec![LlmToolCall { id: format!("call_{}", uuid::Uuid::new_v4().simple()), name: name.into(), arguments }],
            ..Default::default()
        }
    }
    /// Add another tool call to a response.
    pub fn and_tool_call(mut self, name: impl Into<String>, arguments: serde_json::Value) -> Self {
        self.tool_calls.push(LlmToolCall { id: format!("call_{}", uuid::Uuid::new_v4().simple()), name: name.into(), arguments });
        self
    }
}

/// Streaming increments surfaced while a response is generated.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum LlmDelta {
    Text(String),
    Reasoning(String),
}

/// Callback receiving streaming deltas.
pub type DeltaSink<'a> = &'a (dyn Fn(LlmDelta) + Send + Sync);

/// A chat-completion backend.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Run one completion. Implementations must return [`LlmError::Cancelled`]
    /// promptly when `cancel` fires.
    async fn complete(&self, request: LlmRequest, cancel: CancellationToken, on_delta: DeltaSink<'_>) -> Result<LlmResponse, LlmError>;

    /// Lightweight text-in/text-out helper for summarisation tasks.
    async fn complete_text(&self, system: &str, user: &str) -> Result<String, LlmError> {
        let response = self
            .complete(
                LlmRequest {
                    system: system.to_owned(),
                    messages: vec![LlmMessage::user(user)],
                    tools: Vec::new(),
                    options: LlmOptions::default(),
                },
                CancellationToken::new(),
                &|_| {},
            )
            .await?;
        Ok(response.text.unwrap_or_default())
    }

    /// Human-readable model identifier (for events and logs).
    fn model_name(&self) -> String {
        "unknown".to_owned()
    }
}
