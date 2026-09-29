//! On-disk layout:
//!
//! ```text
//! <root>/
//!   agents/<agentId>/{store.db, profile.json, settings.json, workspace/,
//!                    memory/{profile.md, log/YYYY-MM.md}, automations/<slug>/automation.json}
//!   agents/<groupId>/{group.json, store.db}
//!   memory/user/<agentId>/{profile.md, log/}
//!   memory/projects/<slug>/<agentId>/{profile.md, log/}
//! ```

use gns_core::{AgentId, GroupId};
use std::path::{Path, PathBuf};

/// Path computation for one root directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDirLayout {
    root: PathBuf,
}

impl AgentDirLayout {
    /// Create a layout rooted at `root` (not created on disk yet).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn agents_root(&self) -> PathBuf {
        self.root.join("agents")
    }
    pub fn agent_dir(&self, id: &AgentId) -> PathBuf {
        self.agents_root().join(id.as_str())
    }
    pub fn group_dir(&self, id: &GroupId) -> PathBuf {
        self.agents_root().join(id.as_str())
    }
    pub fn store_db(&self, id: &AgentId) -> PathBuf {
        self.agent_dir(id).join("store.db")
    }
    pub fn group_store_db(&self, id: &GroupId) -> PathBuf {
        self.group_dir(id).join("store.db")
    }
    pub fn profile_json(&self, id: &AgentId) -> PathBuf {
        self.agent_dir(id).join("profile.json")
    }
    pub fn settings_json(&self, id: &AgentId) -> PathBuf {
        self.agent_dir(id).join("settings.json")
    }
    pub fn group_json(&self, id: &GroupId) -> PathBuf {
        self.group_dir(id).join("group.json")
    }
    pub fn workspace_dir(&self, id: &AgentId) -> PathBuf {
        self.agent_dir(id).join("workspace")
    }
    pub fn automations_dir(&self, id: &AgentId) -> PathBuf {
        self.agent_dir(id).join("automations")
    }
    pub fn agent_memory_dir(&self, id: &AgentId) -> PathBuf {
        self.agent_dir(id).join("memory")
    }
    pub fn user_memory_root(&self) -> PathBuf {
        self.root.join("memory").join("user")
    }
    pub fn user_memory_shard(&self, id: &AgentId) -> PathBuf {
        self.user_memory_root().join(id.as_str())
    }
    pub fn projects_memory_root(&self) -> PathBuf {
        self.root.join("memory").join("projects")
    }
    pub fn project_memory_root(&self, slug: &str) -> PathBuf {
        self.projects_memory_root().join(slug)
    }
    pub fn project_memory_shard(&self, slug: &str, id: &AgentId) -> PathBuf {
        self.project_memory_root(slug).join(id.as_str())
    }

    /// Create every directory an agent needs.
    pub fn ensure_agent_dirs(&self, id: &AgentId) -> std::io::Result<()> {
        std::fs::create_dir_all(self.agent_dir(id))?;
        std::fs::create_dir_all(self.workspace_dir(id))?;
        std::fs::create_dir_all(self.automations_dir(id))?;
        std::fs::create_dir_all(self.agent_memory_dir(id).join("log"))?;
        Ok(())
    }

    /// Create every directory a group needs.
    pub fn ensure_group_dirs(&self, id: &GroupId) -> std::io::Result<()> {
        std::fs::create_dir_all(self.group_dir(id))
    }
}
