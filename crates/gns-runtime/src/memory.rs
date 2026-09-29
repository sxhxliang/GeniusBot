//! Three-tier memory service over markdown shards, laid out as the original
//! host does (`memory-service.ts`, `project-membership.ts`):
//!
//! * agent memory: `<agents>/<id>/memory/`
//! * shared user memory: `<root>/user-memory/agents/<id>/`
//! * project memory: `<root>/projects/<slug>/memory/agents/<id>/`, with
//!   `<root>/projects/<slug>/project.md` describing the project and each
//!   agent's membership in `<agents>/<id>/projects.json`.

use gns_core::memory::*;
use gns_core::text::now_ms;
use gns_core::*;
use gns_store::{AgentDirLayout, MemoryFiles, write_text_atomic};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// Everything recalled for one agent's prompt.
#[derive(Clone, Debug, Default)]
pub struct FullRecall {
    pub agent: MemoryRecall,
    /// (shard agent id, recall) for every assistant's user-memory shard.
    pub user_shards: Vec<(AgentId, MemoryRecall)>,
    /// slug → shards, for the projects this agent has joined.
    pub projects: Vec<(String, Vec<(AgentId, MemoryRecall)>)>,
}

/// [`MemoryService`] implementation.
#[derive(Clone, Debug)]
pub struct MemoryServiceImpl {
    layout: AgentDirLayout,
}

/// `isSafeFolderId`: a path segment usable as a project slug.
pub fn is_safe_folder_id(id: &str) -> bool {
    !id.is_empty() && !id.contains('/') && !id.contains('\\') && !id.contains('\0') && id != "." && id != ".."
}

impl MemoryServiceImpl {
    pub fn new(layout: AgentDirLayout) -> Self {
        Self { layout }
    }

    // ----- paths (original layout) -------------------------------------

    /// `<root>/user-memory`
    pub fn user_memory_dir(&self) -> PathBuf {
        self.layout.root().join("user-memory")
    }
    /// `<root>/user-memory/agents`
    pub fn user_memory_shards_dir(&self) -> PathBuf {
        self.user_memory_dir().join("agents")
    }
    /// `<root>/user-memory/agents/<id>`
    pub fn user_memory_shard_dir(&self, agent: &AgentId) -> PathBuf {
        self.user_memory_shards_dir().join(agent.as_str())
    }
    /// `<root>/projects`
    pub fn projects_root_dir(&self) -> PathBuf {
        self.layout.root().join("projects")
    }
    /// `<root>/projects/<slug>`
    pub fn project_dir(&self, slug: &str) -> PathBuf {
        self.projects_root_dir().join(slug)
    }
    /// `<root>/projects/<slug>/memory/agents`
    pub fn project_memory_shards_dir(&self, slug: &str) -> PathBuf {
        self.project_dir(slug).join("memory").join("agents")
    }
    /// `<root>/projects/<slug>/memory/agents/<id>`
    pub fn project_memory_shard_dir(&self, slug: &str, agent: &AgentId) -> PathBuf {
        self.project_memory_shards_dir(slug).join(agent.as_str())
    }
    /// `<agents>/<id>/projects.json`
    pub fn membership_path(&self, agent: &AgentId) -> PathBuf {
        self.layout.agent_dir(agent).join(PROJECT_MEMBERSHIP_FILENAME)
    }
    /// Whether `<root>/projects/<slug>` exists.
    pub fn project_exists(&self, slug: &str) -> bool {
        self.project_dir(slug).is_dir()
    }

    // ----- membership --------------------------------------------------

    /// The project slugs an agent has joined.
    pub fn memberships(&self, agent: &AgentId) -> BTreeSet<String> {
        #[derive(serde::Deserialize)]
        struct File {
            #[serde(default)]
            projects: Vec<serde_json::Value>,
        }
        std::fs::read_to_string(self.membership_path(agent))
            .ok()
            .and_then(|raw| serde_json::from_str::<File>(&raw).ok())
            .map(|f| f.projects.into_iter().filter_map(|v| v.as_str().map(str::to_owned)).filter(|s| is_safe_folder_id(s)).collect())
            .unwrap_or_default()
    }

    fn write_memberships(&self, agent: &AgentId, slugs: &BTreeSet<String>) -> Result<(), HostError> {
        let body = serde_json::json!({ "projects": slugs.iter().collect::<Vec<_>>() });
        let mut text = serde_json::to_string_pretty(&body)?;
        text.push('\n');
        write_text_atomic(&self.membership_path(agent), &text)
    }

    fn join(&self, agent: &AgentId, slug: &str) -> Result<bool, HostError> {
        if !is_safe_folder_id(slug) {
            return Ok(false);
        }
        let mut slugs = self.memberships(agent);
        if slugs.insert(slug.to_owned()) {
            self.write_memberships(agent, &slugs)?;
        }
        Ok(true)
    }

    fn leave(&self, agent: &AgentId, slug: &str) -> Result<bool, HostError> {
        if !is_safe_folder_id(slug) {
            return Ok(false);
        }
        let mut slugs = self.memberships(agent);
        if slugs.remove(slug) {
            self.write_memberships(agent, &slugs)?;
        }
        Ok(true)
    }

    // ----- project actions (`agent-state.ts`) --------------------------

    /// `project create`: create the folder and `project.md`, then join
    /// (create-is-join when the folder already exists).
    pub fn create_project(&self, agent: &AgentId, slug: &str, name: &str, description: Option<&str>) -> Result<String, HostError> {
        let id = slug.trim();
        if !is_safe_folder_id(id) {
            return Err(HostError::invalid(format!("\"{id}\" is not a valid project slug — use a short kebab-case id.")));
        }
        if name.trim().is_empty() {
            return Err(HostError::invalid("a project needs a non-empty name."));
        }
        let existed = self.project_exists(id);
        if !existed {
            let dir = self.project_dir(id);
            std::fs::create_dir_all(&dir)?;
            write_text_atomic(&dir.join("project.md"), &serialize_project_file(name.trim(), description.map(str::trim).unwrap_or("")))?;
        }
        if self.join(agent, id)? {
            Ok(if existed {
                format!("Joined existing project \"{id}\" (create-is-join; project.md left as-is).")
            } else {
                format!("Created and joined project \"{}\" (folder {id}).", name.trim())
            })
        } else {
            Err(HostError::invalid(format!("could not join project \"{id}\" — the slug is not path-safe.")))
        }
    }

    /// `project join`.
    pub fn join_project(&self, agent: &AgentId, slug: &str) -> Result<String, HostError> {
        let id = slug.trim();
        if !is_safe_folder_id(id) {
            return Err(HostError::invalid(format!("\"{id}\" is not a valid project slug — use a short kebab-case id.")));
        }
        if !self.project_exists(id) {
            return Err(HostError::invalid(format!("no project \"{id}\" exists. Create it first (action \"create\").")));
        }
        if self.join(agent, id)? {
            Ok(format!("Joined project \"{id}\"."))
        } else {
            Err(HostError::invalid(format!("could not join project \"{id}\".")))
        }
    }

    /// `project leave`.
    pub fn leave_project(&self, agent: &AgentId, slug: &str) -> Result<String, HostError> {
        let id = slug.trim();
        if !is_safe_folder_id(id) {
            return Err(HostError::invalid(format!("\"{id}\" is not a valid project slug — use a short kebab-case id.")));
        }
        if self.leave(agent, id)? {
            Ok(format!("Left project \"{id}\"."))
        } else {
            Err(HostError::invalid(format!("could not leave project \"{id}\".")))
        }
    }

    // ----- shards ------------------------------------------------------

    /// The shard a write/forget lands in plus its label for the model
    /// (`shardFor`). Soft failures are `HostError::Invalid`.
    fn shard_for(&self, agent: &AgentId, scope: MemoryScope, project: Option<&str>) -> Result<(MemoryFiles, String), HostError> {
        match scope {
            MemoryScope::Agent => Ok((MemoryFiles::new(self.layout.agent_memory_dir(agent)), "your memory".to_owned())),
            MemoryScope::User => Ok((MemoryFiles::new(self.user_memory_shard_dir(agent)), "shared user memory".to_owned())),
            MemoryScope::Project => {
                let slug = project
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .ok_or_else(|| HostError::invalid("'project' (the slug) is required when scope is project."))?;
                if !is_safe_folder_id(slug) {
                    return Err(HostError::invalid(format!("\"{slug}\" is not a valid project slug — use a short kebab-case id.")));
                }
                if !self.project_exists(slug) {
                    return Err(HostError::invalid(format!(
                        "no project \"{slug}\" exists yet. Create or join it first (target \"project\")."
                    )));
                }
                if !self.memberships(agent).contains(slug) {
                    return Err(HostError::invalid(format!(
                        "you haven't joined project \"{slug}\" yet. Join it first (target \"project\", action \"join\")."
                    )));
                }
                Ok((MemoryFiles::new(self.project_memory_shard_dir(slug, agent)), format!("project \"{slug}\" memory")))
            }
        }
    }

    /// Read every tier for an agent.
    pub fn recall_all(&self, agent: &AgentId) -> Result<FullRecall, HostError> {
        let agent_recall = MemoryFiles::new(self.layout.agent_memory_dir(agent)).recall()?;
        let user_shards = read_shards(&self.user_memory_shards_dir())?;
        let mut projects = Vec::new();
        for slug in self.memberships(agent) {
            let shards = read_shards(&self.project_memory_shards_dir(&slug))?;
            if !shards.is_empty() {
                projects.push((slug, shards));
            }
        }
        Ok(FullRecall { agent: agent_recall, user_shards, projects })
    }

    /// The agent's own facts, profile first then newest first (`listMemories`).
    pub fn list_agent_memories(&self, agent: &AgentId, limit: usize) -> Result<Vec<MemoryRecord>, HostError> {
        let recall = MemoryFiles::new(self.layout.agent_memory_dir(agent)).recall()?;
        let mut profile = recall.profile;
        profile.reverse();
        let mut recent = recall.recent;
        recent.reverse();
        Ok(profile.into_iter().chain(recent).take(limit).collect())
    }

    /// Render the memory block for a prompt. `names` maps shard ids to agent names.
    pub fn render(&self, agent: &AgentId, recall: &FullRecall, names: &HashMap<AgentId, String>) -> String {
        let via = |id: &AgentId| names.get(id).cloned().unwrap_or_else(|| id.to_string());
        let mut blocks = Vec::new();
        let shards: Vec<(String, MemoryRecall)> = recall.user_shards.iter().map(|(id, r)| (via(id), r.clone())).collect();
        let (profile, recent) = merge_user_memory_shards(&shards, MEMORY_USER_PROFILE_PROMPT_LIMIT, MEMORY_USER_RECENT_PROMPT_LIMIT);
        blocks.push(render_user_memory_system_prompt(
            &profile,
            &recent,
            Some(&self.user_memory_dir().display().to_string()),
            Some(&self.user_memory_shard_dir(agent).display().to_string()),
        ));
        let projects: Vec<(String, Vec<ProvenancedMemory>, Vec<ProvenancedMemory>)> = recall
            .projects
            .iter()
            .map(|(slug, shards)| {
                let shards: Vec<(String, MemoryRecall)> = shards.iter().map(|(id, r)| (via(id), r.clone())).collect();
                let (p, r) = merge_user_memory_shards(&shards, MEMORY_PROJECT_PROFILE_PROMPT_LIMIT, MEMORY_PROJECT_RECENT_PROMPT_LIMIT);
                (slug.clone(), p, r)
            })
            .collect();
        if let Some(block) = render_project_memory_system_prompt(&projects) {
            blocks.push(block);
        }
        blocks.push(render_memory_system_prompt(&recall.agent, Some(&self.layout.agent_memory_dir(agent).display().to_string())));
        blocks.join("\n\n")
    }

    /// Append an episode summary to the agent log, dated now.
    pub fn write_episode(&self, agent: &AgentId, summary: &str) -> Result<(), HostError> {
        self.write_episode_at(agent, summary, now_ms())
    }

    /// Append `[episode] <narrative>` to the agent log with an explicit
    /// `created_at` (the last exchange's time, `turn-memory.ts`).
    pub fn write_episode_at(&self, agent: &AgentId, narrative: &str, created_at_ms: i64) -> Result<(), HostError> {
        let content = format!("{MEMORY_EPISODE_PREFIX}{}", narrative.trim());
        MemoryFiles::new(self.layout.agent_memory_dir(agent)).write(&content, MemoryKind::Log, created_at_ms)?;
        Ok(())
    }
}

/// `project.md` with a `name`/`description` frontmatter (`serializeWorkflowFile`
/// for an empty body).
fn serialize_project_file(name: &str, description: &str) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("name: {}\n", yaml_scalar(name)));
    if !description.is_empty() {
        out.push_str(&format!("description: {}\n", yaml_scalar(description)));
    }
    out.push_str("---\n\n");
    out
}

fn yaml_scalar(value: &str) -> String {
    let needs_quotes = value.is_empty()
        || value.contains(':')
        || value.contains('#')
        || value.contains('\n')
        || value.starts_with(['"', '\'', '[', '{', '&', '*', '!', '|', '>', '%', '@', '`', '-', '?'])
        || value != value.trim();
    if needs_quotes { serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\"")) } else { value.to_owned() }
}

/// `avatar.<ext>` files in an agent directory, canonical `avatar.png` first.
pub fn list_conventional_avatar_filenames(dir: &std::path::Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.to_ascii_lowercase().strip_prefix("avatar.").is_some_and(|ext| CONVENTIONAL_AVATAR_EXTENSIONS.contains(&ext)))
        .collect();
    let rank = |n: &str| -> usize {
        if n == CANONICAL_AVATAR_FILENAME {
            return 0;
        }
        let ext = n.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        CONVENTIONAL_AVATAR_EXTENSIONS.iter().position(|e| *e == ext).map(|p| p + 1).unwrap_or(usize::MAX)
    };
    names.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
    names
}

/// `sniffAvatarMimeType` mapped to a file extension.
pub fn sniff_avatar_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[137, 80, 78, 71, 13, 10, 26, 10]) {
        return Some("png");
    }
    if bytes.starts_with(&[255, 216, 255]) {
        return Some("jpg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("gif");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).trim_start_matches('\u{FEFF}').trim_start().to_ascii_lowercase();
    if head.starts_with('<') && head.contains("<svg") { Some("svg") } else { None }
}

fn read_shards(root: &Path) -> Result<Vec<(AgentId, MemoryRecall)>, HostError> {
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    let mut dirs: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    for dir in dirs {
        let id = AgentId::from(dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        out.push((id, MemoryFiles::new(&dir).recall()?));
    }
    Ok(out)
}

fn tier_label(tier: MemoryTier) -> &'static str {
    match tier {
        MemoryTier::Profile => "profile",
        MemoryTier::Log => "log",
        MemoryTier::Note => "note",
    }
}

impl MemoryService for MemoryServiceImpl {
    fn write(&self, agent: &AgentId, fact: &str, tier: MemoryTier, scope: MemoryScope, project: Option<&str>) -> Result<String, HostError> {
        let (files, label) = self.shard_for(agent, scope, project)?;
        let content = match tier {
            MemoryTier::Note => format!("{MEMORY_TOOL_NOTE_PREFIX}{}", fact.trim()),
            _ => fact.to_owned(),
        };
        match files.write(&content, tier.kind(), now_ms())? {
            Some(record) => Ok(format!("Remembered in {label} ({}): {}", tier_label(tier), record.content)),
            None => Err(HostError::invalid(format!(
                "nothing was saved to {label} — the fact was empty or already recorded. Grep the memory folder to see what is already there."
            ))),
        }
    }

    fn forget(&self, agent: &AgentId, fact: &str, scope: MemoryScope, project: Option<&str>) -> Result<String, HostError> {
        let (files, label) = self.shard_for(agent, scope, project)?;
        if files.forget(fact)? {
            Ok(format!("Forgot from {label}: {}", normalize_memory_content(fact)))
        } else {
            Err(HostError::invalid(format!(
                "no fact with exactly that text is recorded in {label}. Read or grep the folder for the exact wording first."
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> (tempfile::TempDir, MemoryServiceImpl) {
        let dir = tempfile::tempdir().unwrap();
        let service = MemoryServiceImpl::new(AgentDirLayout::new(dir.path()));
        (dir, service)
    }

    #[test]
    fn write_and_forget_use_the_original_result_strings() {
        let (_dir, memory) = service();
        let agent = AgentId::from("a1".to_owned());
        let saved = memory.write(&agent, "  Likes   tea ", MemoryTier::Log, MemoryScope::Agent, None).unwrap();
        assert_eq!(saved, "Remembered in your memory (log): Likes tea");
        let dup = memory.write(&agent, "likes TEA", MemoryTier::Profile, MemoryScope::Agent, None).unwrap_err();
        assert!(matches!(dup, HostError::Invalid(ref r) if r.starts_with("nothing was saved to your memory — ")), "{dup}");
        let note = memory.write(&agent, "trying vitest", MemoryTier::Note, MemoryScope::User, None).unwrap();
        assert_eq!(note, "Remembered in shared user memory (note): Note: trying vitest");
        assert!(memory.user_memory_shard_dir(&agent).join("log").is_dir());
        assert_eq!(memory.forget(&agent, "LIKES tea", MemoryScope::Agent, None).unwrap(), "Forgot from your memory: LIKES tea");
        let missing = memory.forget(&agent, "LIKES tea", MemoryScope::Agent, None).unwrap_err();
        assert!(
            matches!(missing, HostError::Invalid(ref r) if r.starts_with("no fact with exactly that text is recorded in your memory."))
        );
    }

    #[test]
    fn project_memory_requires_an_existing_joined_project() {
        let (dir, memory) = service();
        let agent = AgentId::from("a1".to_owned());
        let err = memory.write(&agent, "x", MemoryTier::Log, MemoryScope::Project, None).unwrap_err();
        assert!(matches!(err, HostError::Invalid(ref r) if r == "'project' (the slug) is required when scope is project."));
        let err = memory.write(&agent, "x", MemoryTier::Log, MemoryScope::Project, Some("nope")).unwrap_err();
        assert!(matches!(err, HostError::Invalid(ref r) if r.starts_with("no project \"nope\" exists yet.")));
        assert!(
            matches!(memory.join_project(&agent, "nope"), Err(HostError::Invalid(ref r)) if r.starts_with("no project \"nope\" exists."))
        );
        assert!(matches!(memory.create_project(&agent, "bad/slug", "Bad", None), Err(HostError::Invalid(_))));
        assert_eq!(
            memory.create_project(&agent, "acme", "Acme launch", Some("Q4 push")).unwrap(),
            "Created and joined project \"Acme launch\" (folder acme)."
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("projects/acme/project.md")).unwrap(),
            "---\nname: Acme launch\ndescription: Q4 push\n---\n\n"
        );
        assert_eq!(memory.memberships(&agent), BTreeSet::from(["acme".to_owned()]));
        let other = AgentId::from("a2".to_owned());
        let err = memory.write(&other, "x", MemoryTier::Log, MemoryScope::Project, Some("acme")).unwrap_err();
        assert!(matches!(err, HostError::Invalid(ref r) if r.starts_with("you haven't joined project \"acme\" yet.")));
        assert_eq!(
            memory.create_project(&other, "acme", "Whatever", None).unwrap(),
            "Joined existing project \"acme\" (create-is-join; project.md left as-is)."
        );
        assert_eq!(
            memory.write(&other, "Deadline is Friday", MemoryTier::Log, MemoryScope::Project, Some("acme")).unwrap(),
            "Remembered in project \"acme\" memory (log): Deadline is Friday"
        );
        assert!(dir.path().join("projects/acme/memory/agents/a2/log").is_dir());
        let recall = memory.recall_all(&other).unwrap();
        assert_eq!(recall.projects.len(), 1);
        assert_eq!(recall.projects[0].0, "acme");
        assert_eq!(memory.leave_project(&other, "acme").unwrap(), "Left project \"acme\".");
        assert!(memory.recall_all(&other).unwrap().projects.is_empty(), "left projects are not recalled");
        assert_eq!(memory.join_project(&other, "acme").unwrap(), "Joined project \"acme\".");
        let listed = memory.list_agent_memories(&agent, 10).unwrap();
        assert!(listed.is_empty());
    }
}
