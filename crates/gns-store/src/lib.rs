//! `gns-store` — persistence for the Genius Bot SDK.
//!
//! * [`AgentDb`]: one SQLite database per agent (`store.db`, WAL) holding a KV
//!   table and the transcript, matching the original `agent-db-schema`.
//! * [`AgentDirLayout`]: pure path computation for the on-disk layout.
//! * File stores for `profile.json`, `settings.json`, `group.json`, memory
//!   markdown files and `automations/<slug>/automation.json`.
//! * [`Roster`]: file-based discovery of agents and groups.

mod automation_files;
mod coordination;
mod db;
mod files;
mod layout;
mod memory_files;
mod roster;

pub use automation_files::*;
pub use coordination::*;
pub use db::*;
pub use files::*;
pub use layout::*;
pub use memory_files::*;
pub use roster::*;
