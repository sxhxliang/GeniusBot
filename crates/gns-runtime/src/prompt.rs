//! System prompt assembly with ordered, replaceable sections. The default
//! order mirrors `system-prompt-assembly.ts` (base, untrusted content,
//! profile, user identity, time, memory, automations, agent directory), with
//! a local workspace section standing in for "Your box" and the extra-tools
//! section last.

use gns_core::messaging::render_agent_directory_system_prompt;
use gns_core::prompt::*;
use gns_core::run::RunSource;
use std::sync::Arc;

/// Ordered list of [`PromptSection`]s joined with blank lines.
#[derive(Clone)]
pub struct SystemPromptAssembler {
    sections: Vec<Arc<dyn PromptSection>>,
}

impl std::fmt::Debug for SystemPromptAssembler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.sections.iter().map(|s| s.id())).finish()
    }
}

impl SystemPromptAssembler {
    /// The default section order (mirrors the original assembly).
    pub fn standard(product_name: impl Into<String>) -> Self {
        let product_name = product_name.into();
        Self {
            sections: vec![
                Arc::new(BaseSection { product_name: product_name.clone() }),
                Arc::new(UntrustedContentSection { product_name }),
                Arc::new(ProfileSection),
                Arc::new(UserIdentitySection),
                Arc::new(TimeZoneSection),
                Arc::new(MemorySection),
                Arc::new(AutomationsSection),
                Arc::new(AgentDirectorySection),
                Arc::new(WorkspaceSection),
                Arc::new(ExtraToolsSection),
            ],
        }
    }

    /// Insert a section at a position.
    pub fn insert(&mut self, section: Arc<dyn PromptSection>, position: SectionPosition) {
        let index = match &position {
            SectionPosition::Before(id) => self.sections.iter().position(|s| s.id() == id).unwrap_or(self.sections.len()),
            SectionPosition::After(id) => self.sections.iter().position(|s| s.id() == id).map(|i| i + 1).unwrap_or(self.sections.len()),
            SectionPosition::End => self.sections.len(),
        };
        self.sections.insert(index, section);
    }

    /// Remove a section by id.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.sections.len();
        self.sections.retain(|s| s.id() != id);
        before != self.sections.len()
    }

    /// Section ids in order.
    pub fn ids(&self) -> Vec<String> {
        self.sections.iter().map(|s| s.id().to_owned()).collect()
    }

    /// Render the full system prompt.
    pub fn render(&self, ctx: &PromptContext) -> String {
        self.sections.iter().filter_map(|s| s.render(ctx)).filter(|s| !s.trim().is_empty()).collect::<Vec<_>>().join("\n\n")
    }
}

/// The behavioural constitution, or `PromptContext::base_override` verbatim
/// when a turn (a group room, say) supplies its own base.
#[derive(Debug)]
pub struct BaseSection {
    pub product_name: String,
}
impl PromptSection for BaseSection {
    fn id(&self) -> &str {
        section_ids::BASE
    }
    fn render(&self, ctx: &PromptContext) -> Option<String> {
        Some(ctx.base_override.clone().unwrap_or_else(|| build_base_system_prompt(&self.product_name)))
    }
}

/// `## Untrusted content` (tool-result spotlighting rules).
#[derive(Debug)]
pub struct UntrustedContentSection {
    pub product_name: String,
}
impl PromptSection for UntrustedContentSection {
    fn id(&self) -> &str {
        section_ids::UNTRUSTED_CONTENT
    }
    fn render(&self, ctx: &PromptContext) -> Option<String> {
        Some(spotlight_prompt_section(!ctx.is_subagent, &self.product_name))
    }
}

/// Agent profile (the reduced variant on group-room turns).
#[derive(Debug)]
pub struct ProfileSection;
impl PromptSection for ProfileSection {
    fn id(&self) -> &str {
        section_ids::PROFILE
    }
    fn render(&self, ctx: &PromptContext) -> Option<String> {
        let settings_path = ctx.data_dir.join("settings.json");
        render_profile_section(
            &ctx.profile,
            &ctx.profile_path.display().to_string(),
            &settings_path.display().to_string(),
            ctx.source == RunSource::Group,
        )
    }
}

/// Who the user is.
#[derive(Debug)]
pub struct UserIdentitySection;
impl PromptSection for UserIdentitySection {
    fn id(&self) -> &str {
        section_ids::USER_IDENTITY
    }
    fn render(&self, ctx: &PromptContext) -> Option<String> {
        render_user_identity_system_prompt(ctx.user_name.as_deref())
    }
}

/// `## Time`: the user's zone and its current UTC offset (omitted without a zone).
#[derive(Debug)]
pub struct TimeZoneSection;
impl PromptSection for TimeZoneSection {
    fn id(&self) -> &str {
        section_ids::TIME_ZONE
    }
    fn render(&self, ctx: &PromptContext) -> Option<String> {
        let zone = ctx.time_zone.as_deref().filter(|z| !z.is_empty())?;
        let offset = zone.parse::<chrono_tz::Tz>().ok().and_then(|tz| {
            use chrono::Offset;
            let now = chrono::DateTime::from_timestamp_millis(ctx.now_ms)?;
            Some(format_utc_offset_label(now.with_timezone(&tz).offset().fix().local_minus_utc()))
        });
        render_time_zone_system_prompt(Some(zone), offset.as_deref())
    }
}

/// Memory (pre-rendered by the runtime, frozen-snapshot aware).
#[derive(Debug)]
pub struct MemorySection;
impl PromptSection for MemorySection {
    fn id(&self) -> &str {
        section_ids::MEMORY
    }
    fn render(&self, ctx: &PromptContext) -> Option<String> {
        ctx.memory_render.clone()
    }
}

/// Routines (pre-rendered by the runtime).
#[derive(Debug)]
pub struct AutomationsSection;
impl PromptSection for AutomationsSection {
    fn id(&self) -> &str {
        section_ids::AUTOMATIONS
    }
    fn render(&self, ctx: &PromptContext) -> Option<String> {
        ctx.automations_render.clone()
    }
}

/// Teammates and groups (omitted for subagents).
#[derive(Debug)]
pub struct AgentDirectorySection;
impl PromptSection for AgentDirectorySection {
    fn id(&self) -> &str {
        section_ids::AGENT_DIRECTORY
    }
    fn render(&self, ctx: &PromptContext) -> Option<String> {
        if ctx.is_subagent {
            return None;
        }
        Some(render_agent_directory_system_prompt(&ctx.others, &ctx.groups, Some(&ctx.agents_root.display().to_string())))
    }
}

/// Extra tools (MCP servers, switched-off built-ins), pre-rendered by the runtime.
#[derive(Debug)]
pub struct ExtraToolsSection;
impl PromptSection for ExtraToolsSection {
    fn id(&self) -> &str {
        section_ids::EXTRA_TOOLS
    }
    fn render(&self, ctx: &PromptContext) -> Option<String> {
        ctx.extra_tools_render.clone()
    }
}

/// Local directories (stands in for the original "Your box" section).
#[derive(Debug)]
pub struct WorkspaceSection;
impl PromptSection for WorkspaceSection {
    fn id(&self) -> &str {
        section_ids::WORKSPACE
    }
    fn render(&self, ctx: &PromptContext) -> Option<String> {
        Some(render_workspace_section(
            &ctx.workspace_dir.display().to_string(),
            &ctx.data_dir.display().to_string(),
            &ctx.agents_root.display().to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gns_core::agent::AgentProfile;
    use gns_core::ids::AgentId;
    use std::path::PathBuf;

    fn ctx() -> PromptContext {
        PromptContext {
            agent_id: AgentId::from("a1"),
            profile: AgentProfile {
                name: "Ada".into(),
                description: "Math".into(),
                title: String::new(),
                avatar_shape: String::new(),
                avatar_color: String::new(),
            },
            profile_path: PathBuf::from("/d/a1/profile.json"),
            data_dir: PathBuf::from("/d/a1"),
            workspace_dir: PathBuf::from("/w/a1"),
            agents_root: PathBuf::from("/d"),
            memory_dir: PathBuf::from("/d/a1/memory"),
            automations_dir: PathBuf::from("/d/a1/automations"),
            user_name: Some("Ian".into()),
            time_zone: Some("America/Los_Angeles".into()),
            others: vec![],
            groups: vec![],
            source: RunSource::User,
            now_ms: 1_750_000_000_000,
            memory_render: Some("MEMORY".into()),
            automations_render: Some("ROUTINES".into()),
            extra_tools_render: Some("EXTRA".into()),
            base_override: None,
            is_subagent: false,
        }
    }

    #[test]
    fn standard_order_matches_original_assembly() {
        let assembler = SystemPromptAssembler::standard("Genius Bot");
        assert_eq!(
            assembler.ids(),
            vec![
                "base",
                "untrusted-content",
                "profile",
                "user-identity",
                "time-zone",
                "memory",
                "automations",
                "agent-directory",
                "workspace",
                "extra-tools"
            ]
        );
        let rendered = assembler.render(&ctx());
        let idx = |needle: &str| rendered.find(needle).unwrap_or_else(|| panic!("missing {needle}"));
        assert!(rendered.starts_with("You are Genius Bot, a warm, concise desktop assistant."));
        assert!(idx("## Untrusted content") < idx("Agent profile:\nTitle: Ada"));
        assert!(idx("Agent profile:") < idx("Your user is Ian;"));
        assert!(
            idx("Your user is Ian;")
                < idx("## Time\nYour box and tools run on a UTC clock, but the user lives in America/Los_Angeles (currently UTC-7).")
        );
        assert!(idx("## Time") < idx("MEMORY"));
        assert!(idx("MEMORY") < idx("ROUTINES"));
        assert!(idx("ROUTINES") < idx("Your teammates:"));
        assert!(idx("Your teammates:") < idx("## Your workspace"));
        assert!(idx("## Your workspace") < idx("EXTRA"));
        assert!(rendered.ends_with("EXTRA"));
    }

    #[test]
    fn overrides_and_omissions() {
        let assembler = SystemPromptAssembler::standard("Genius Bot");
        let mut c = ctx();
        c.base_override = Some("ROOM BASE".into());
        c.source = RunSource::Group;
        c.time_zone = None;
        let rendered = assembler.render(&c);
        assert!(rendered.starts_with("ROOM BASE\n\n## Untrusted content"));
        assert!(rendered.contains("Agent profile:\nTitle: Ada\nDescription: Math\n\n"));
        assert!(!rendered.contains("## Time"));
        let mut s = ctx();
        s.is_subagent = true;
        let sub = assembler.render(&s);
        assert!(sub.contains("report what it asked in your final answer"));
        assert!(!sub.contains("Your teammates:"));
    }
}
