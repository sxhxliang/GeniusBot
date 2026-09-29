//! Error types. Each crate owns its errors; these are the ones that cross
//! crate boundaries through the core traits.

use thiserror::Error;

/// Errors raised by tools. `Input` errors are shown to the model verbatim so it
/// can correct its call; `Failed` errors are also shown but flagged as failures.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ToolError {
    /// The model supplied invalid arguments.
    #[error("{0}")]
    Input(String),
    /// The tool ran but failed.
    #[error("{0}")]
    Failed(String),
    /// The operation was denied by an isolation or policy check.
    #[error("{0}")]
    Denied(String),
    /// The turn was cancelled while the tool was running.
    #[error("cancelled")]
    Cancelled,
    /// A host service call failed.
    #[error(transparent)]
    Host(#[from] HostError),
}

impl ToolError {
    /// Convenience constructor for input errors.
    pub fn input(msg: impl Into<String>) -> Self {
        ToolError::Input(msg.into())
    }
    /// Convenience constructor for failures.
    pub fn failed(msg: impl Into<String>) -> Self {
        ToolError::Failed(msg.into())
    }
    /// Convenience constructor for denials.
    pub fn denied(msg: impl Into<String>) -> Self {
        ToolError::Denied(msg.into())
    }
}

/// Errors raised by an [`crate::LlmProvider`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LlmError {
    /// Misconfiguration (missing key, unknown model, bad endpoint).
    #[error("llm configuration error: {0}")]
    Config(String),
    /// The provider returned an error. `retryable` hints at 429/5xx.
    #[error("llm provider error (status {status:?}): {message}")]
    Provider { status: Option<u16>, message: String, retryable: bool },
    /// The request was cancelled.
    #[error("llm request cancelled")]
    Cancelled,
    /// The response could not be interpreted.
    #[error("llm response error: {0}")]
    Response(String),
}

impl LlmError {
    /// Whether the runner may retry the call.
    pub fn is_retryable(&self) -> bool {
        matches!(self, LlmError::Provider { retryable: true, .. })
    }
}

/// Errors raised by the host runtime and its services.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HostError {
    #[error("agent not found: {0}")]
    AgentNotFound(String),
    #[error("group not found: {0}")]
    GroupNotFound(String),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error(transparent)]
    Llm(#[from] LlmError),
    #[error("runtime is shutting down")]
    ShuttingDown,
    #[error("{0}")]
    Other(String),
}

impl HostError {
    /// Convenience constructor for storage errors.
    pub fn storage(msg: impl std::fmt::Display) -> Self {
        HostError::Storage(msg.to_string())
    }
    /// Convenience constructor for invalid requests.
    pub fn invalid(msg: impl Into<String>) -> Self {
        HostError::Invalid(msg.into())
    }
}
