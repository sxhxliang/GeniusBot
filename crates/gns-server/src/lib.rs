//! `gns-server` — an HTTP API over [`gns_runtime::AgentHost`] with an
//! embedded React console for debugging agents: model settings, model-call
//! logs, agent details and live conversations.

pub mod api;
pub mod assets;
pub mod events;
pub mod model;

pub use api::{AppState, router};
pub use events::EventBus;
pub use model::{ModelHub, ModelSettings};
