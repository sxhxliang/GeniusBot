//! `gns-core` — domain types and extension traits for the Genius Bot multi-agent SDK.
//!
//! This crate is deliberately IO-free: it defines the vocabulary shared by every
//! other crate (ids, transcript entries, tool/LLM/prompt/middleware traits, memory
//! and routine models) plus the pure functions that implement the coordination
//! rules (mention parsing, memory ranking, prompt text builders).

pub mod agent;
pub mod consts;
pub mod coordination;
pub mod error;
pub mod event;
pub mod groups;
pub mod ids;
pub mod llm;
pub mod mcp;
pub mod memory;
pub mod messaging;
pub mod middleware;
pub mod policy;
pub mod prompt;
pub mod routine;
pub mod run;
pub mod services;
pub mod store;
pub mod text;
pub mod tool;
pub mod transcript;

pub use agent::*;
pub use consts::*;
pub use coordination::*;
pub use error::*;
pub use event::*;
pub use ids::*;
pub use llm::*;
pub use mcp::*;
pub use middleware::*;
pub use policy::*;
pub use prompt::*;
pub use run::*;
pub use services::*;
pub use store::*;
pub use tool::*;
pub use transcript::*;

/// Re-exported for downstream crates implementing async traits.
pub use async_trait::async_trait;
/// Re-exported cancellation primitive used across the SDK.
pub use tokio_util::sync::CancellationToken;
