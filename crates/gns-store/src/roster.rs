//! File-based discovery of agents and groups under `<root>/agents/`.

use crate::files::{GroupFile, ProfileFile};
use crate::layout::AgentDirLayout;
use gns_core::{AgentAddress, AgentId, GroupAddress, GroupId, HostError};

/// A discovered entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RosterEntry {
    Agent(AgentAddress),
    Group(GroupAddress),
}

/// Scan the agents root.
#[derive(Clone, Debug)]
pub struct Roster {
    layout: AgentDirLayout,
}

impl Roster {
    pub fn new(layout: AgentDirLayout) -> Self {
        Self { layout }
    }

    /// Agents and groups currently on disk (sorted by name).
    pub fn scan(&self) -> Result<(Vec<AgentAddress>, Vec<GroupAddress>), HostError> {
        let mut agents = Vec::new();
        let mut group_configs = Vec::new();
        let rd = match std::fs::read_dir(self.layout.agents_root()) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((agents, Vec::new())),
            Err(e) => return Err(e.into()),
        };
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            // One corrupt or half-written file must not hide every other agent.
            if path.join("group.json").exists() {
                match GroupFile::read(&path.join("group.json")) {
                    Ok(Some(config)) => group_configs.push((GroupId::from(id), config)),
                    Ok(None) => {}
                    Err(e) => tracing::warn!(path = %path.display(), error = %e, "skipping unreadable group.json"),
                }
            } else if path.join("profile.json").exists() {
                match ProfileFile::read(&path.join("profile.json")) {
                    Ok(profile) => {
                        agents.push(AgentAddress { id: AgentId::from(id), name: profile.name, description: profile.description })
                    }
                    Err(e) => tracing::warn!(path = %path.display(), error = %e, "skipping unreadable profile.json"),
                }
            }
        }
        agents.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
        let mut groups: Vec<GroupAddress> = group_configs
            .into_iter()
            .map(|(id, config)| GroupAddress {
                id,
                name: config.name,
                description: config.description,
                members: config.member_ids.iter().filter_map(|m| agents.iter().find(|a| &a.id == m).cloned()).collect(),
            })
            .collect();
        groups.sort_by_key(|a| a.name.to_lowercase());
        Ok((agents, groups))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gns_core::{AgentProfile, GroupConfig};

    #[test]
    fn scan_finds_agents_and_groups() {
        let dir = tempfile::tempdir().unwrap();
        let layout = AgentDirLayout::new(dir.path());
        let a = AgentId::new();
        let b = AgentId::new();
        layout.ensure_agent_dirs(&a).unwrap();
        layout.ensure_agent_dirs(&b).unwrap();
        ProfileFile::write(&layout.profile_json(&a), &AgentProfile { name: "Zed".into(), ..Default::default() }).unwrap();
        ProfileFile::write(&layout.profile_json(&b), &AgentProfile { name: "amy".into(), ..Default::default() }).unwrap();
        let g = GroupId::new();
        layout.ensure_group_dirs(&g).unwrap();
        GroupFile::write(
            &layout.group_json(&g),
            &GroupConfig { name: "Room".into(), member_ids: vec![a.clone(), b.clone()], ..Default::default() },
        )
        .unwrap();
        let (agents, groups) = Roster::new(layout).scan().unwrap();
        assert_eq!(agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), vec!["amy", "Zed"]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].members.len(), 2);
    }

    #[test]
    fn corrupt_files_are_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let layout = AgentDirLayout::new(dir.path());
        let good = AgentId::new();
        let bad = AgentId::new();
        layout.ensure_agent_dirs(&good).unwrap();
        layout.ensure_agent_dirs(&bad).unwrap();
        ProfileFile::write(&layout.profile_json(&good), &AgentProfile { name: "ok".into(), ..Default::default() }).unwrap();
        std::fs::write(layout.profile_json(&bad), "{\"name\": \"half-writ").unwrap();
        let g = GroupId::new();
        layout.ensure_group_dirs(&g).unwrap();
        std::fs::write(layout.group_json(&g), "not json").unwrap();
        let (agents, groups) = Roster::new(layout).scan().unwrap();
        assert_eq!(agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), vec!["ok"]);
        assert!(groups.is_empty());
    }
}
