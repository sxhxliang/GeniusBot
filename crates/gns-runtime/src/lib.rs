//! `gns-runtime` — the host runtime.
//!
//! * [`AgentHost`]: the façade (create agents, send messages, groups, events).
//! * One actor task per agent with lanes, priority preemption and an
//!   exclusive run queue ([`actor`]).
//! * The step-loop runner mapping turns onto the LLM ([`runner`]).
//! * Cross-agent messaging ([`messaging`]), group chat ([`groups`]), memory
//!   ([`memory`]), routines ([`routines`]) and prompt assembly ([`prompt`]).

pub mod actor;
pub mod agent;
pub(crate) mod compaction;
pub mod config;
pub mod coordination;
pub mod groups;
pub mod history;
pub mod host;
pub(crate) mod mcp;
pub mod memory;
pub mod messaging;
pub mod prompt;
pub mod routines;
pub mod runner;
pub mod schedule;
pub mod subagent;

pub use agent::{AgentHandle, RunJob};
pub use config::AgentHostConfig;
pub use host::{AcceptedSend, AgentHost, SendOptions};
pub use prompt::SystemPromptAssembler;

pub use gns_core;
pub use gns_mcp;
pub use gns_store;
pub use gns_tools;
