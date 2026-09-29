//! Agent identity, profile and settings models (mirrors `profile.json`,
//! `settings.json` and `group.json`).

use crate::ids::{AgentId, GroupId};
use serde::{Deserialize, Serialize};

/// Persona metadata stored in `profile.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentProfile {
    pub name: String,
    pub description: String,
    pub title: String,
    pub avatar_shape: String,
    pub avatar_color: String,
}

/// Behaviour flags stored in `settings.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentSettings {
    pub notify_on_agent_updates: bool,
    pub hidden_from_sidebar: bool,
    /// Built-in tools this agent has switched off.
    pub tools: crate::mcp::ToolSettings,
    /// MCP servers attached to this agent as extra tools.
    pub mcp_servers: Vec<crate::mcp::McpServerConfig>,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self { notify_on_agent_updates: true, hidden_from_sidebar: false, tools: Default::default(), mcp_servers: Vec::new() }
    }
}

/// Partial update for a profile (only provided fields change).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilePatch {
    pub name: Option<String>,
    pub description: Option<String>,
    pub title: Option<String>,
}

impl ProfilePatch {
    /// True when nothing would change.
    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.description.is_none() && self.title.is_none()
    }
    /// Apply this patch onto a profile, ignoring empty strings.
    pub fn apply(&self, profile: &mut AgentProfile) {
        if let Some(name) = self.name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            profile.name = name.to_owned();
        }
        if let Some(desc) = self.description.as_deref().map(str::trim)
            && !desc.is_empty()
        {
            profile.description = desc.to_owned();
        }
        if let Some(title) = self.title.as_deref().map(str::trim) {
            profile.title = title.to_owned();
        }
    }
}

/// Partial update for settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsPatch {
    pub notify_on_agent_updates: Option<bool>,
    pub hidden_from_sidebar: Option<bool>,
}

impl SettingsPatch {
    /// Apply onto settings.
    pub fn apply(&self, settings: &mut AgentSettings) {
        if let Some(v) = self.notify_on_agent_updates {
            settings.notify_on_agent_updates = v;
        }
        if let Some(v) = self.hidden_from_sidebar {
            settings.hidden_from_sidebar = v;
        }
    }
}

/// Request to create an agent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpec {
    pub name: String,
    pub description: String,
    pub title: String,
    /// Run the hidden first-turn kickstart prompt after creation.
    pub kickstart: bool,
}

impl AgentSpec {
    /// Build a spec with a name and description.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self { name: name.into(), description: description.into(), ..Default::default() }
    }
}

/// A teammate as seen in the agent directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAddress {
    pub id: AgentId,
    pub name: String,
    pub description: String,
}

/// A group chat as seen in the agent directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupAddress {
    pub id: GroupId,
    pub name: String,
    pub description: String,
    pub members: Vec<AgentAddress>,
}

/// On-disk `group.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GroupConfig {
    /// Room name (stored in the group's `profile.json`, not in `group.json`).
    #[serde(skip)]
    pub name: String,
    /// Room description (stored in the group's `profile.json`).
    #[serde(skip)]
    pub description: String,
    pub member_ids: Vec<AgentId>,
    pub version: u32,
}

/// Request to create a group.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GroupSpec {
    pub name: String,
    pub description: String,
    pub members: Vec<AgentId>,
}
