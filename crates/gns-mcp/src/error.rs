use gns_core::{HostError, ToolError};

/// Failures talking to an MCP server.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("could not start MCP server: {0}")]
    Spawn(String),
    #[error("MCP transport error: {0}")]
    Transport(String),
    #[error("MCP protocol error: {0}")]
    Protocol(String),
    #[error("MCP server error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("MCP request timed out: {0}")]
    Timeout(String),
    #[error("MCP server connection closed")]
    Closed,
    #[error("cancelled")]
    Cancelled,
}

impl From<McpError> for ToolError {
    fn from(e: McpError) -> Self {
        match e {
            McpError::Cancelled => ToolError::Cancelled,
            other => ToolError::failed(other.to_string()),
        }
    }
}

impl From<McpError> for HostError {
    fn from(e: McpError) -> Self {
        HostError::Other(e.to_string())
    }
}
