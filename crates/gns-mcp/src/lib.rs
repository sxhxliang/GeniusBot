//! `gns-mcp` — a small Model Context Protocol client that turns the tools of
//! an MCP server into [`gns_core::Tool`]s.
//!
//! Supported transports: **stdio** (spawn a process, newline-delimited
//! JSON-RPC) and **streamable HTTP** (POST JSON-RPC, JSON or SSE responses,
//! `Mcp-Session-Id` sessions). The legacy HTTP+SSE transport (GET `/sse`) is
//! not supported.

mod client;
mod error;
mod jsonrpc;
mod sse;
mod tool;
mod transport;

pub use client::{McpClient, ServerInfo};
pub use error::McpError;
pub use sse::SseParser;
pub use tool::{McpTool, flatten_call_result, tools_for_server};
