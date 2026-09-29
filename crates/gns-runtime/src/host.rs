//! The host: owns agents, groups, services and the event bus.
//!
//! `AgentHost` is the façade the original exposes through its gateway
//! (`sendPrompt`, `createAgent`, `createGroup`, `broadcastToAgents`,
//! `respondToWidget`, …); `HostInner` is the shared state behind `Arc` and
//! the `HostServices` implementation tools talk to.

use crate::actor::spawn_actor;
use crate::agent::{AgentHandle, EntryIdKind, RunJob};
use crate::config::AgentHostConfig;
use crate::groups::GroupHandle;
use crate::memory::MemoryServiceImpl;
use crate::messaging::AgentToAgentMessaging;
use crate::prompt::SystemPromptAssembler;
use crate::routines::{RoutineServiceImpl, spawn_scheduler};
use crate::runner::ApprovalOutcome;
use gns_core::groups::{GroupMember, parse_group_mentions};
use gns_core::memory::{
    MemoryExtraction, MemoryScope, MemoryTier, build_dream_system_prompt, build_dream_user_prompt, parse_memory_extraction,
};
use gns_core::messaging::{build_admin_broadcast_wake_prompt, build_mentioned_agents_context};
use gns_core::policy::{ApprovalRequest, ToolPolicy};
use gns_core::prompt::{
    PromptContext, PromptSection, SectionPosition, build_ack_redrive_prompt, build_unanswered_questions_note, kickstart_prompt,
    render_agent_profile_update,
};
use gns_core::text::now_ms;
use gns_core::*;
use gns_store::{AgentDb, AgentDirLayout, GroupFile, ProfileFile, Roster, SettingsFile};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, RwLock, Weak};
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// `memoryPromptSnapshot`: the memory block frozen for one compaction epoch.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MemoryPromptSnapshot {
    render: String,
    compaction_epoch: u64,
}

/// `agentProfilePromptSnapshot`: the profile the system prompt describes,
/// frozen for one compaction epoch; edits reach the model through an
/// `<agent_profile_update>` block until the next summary folds them in.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfilePromptSnapshot {
    profile: AgentProfile,
    compaction_epoch: u64,
}

/// `awaitingUserResponse`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AwaitingUserResponse {
    pub run_id: RunId,
    pub reason: String,
    pub since: i64,
}

/// An approval request plus the channel that resolves it.
type PendingApproval = (ApprovalRequest, oneshot::Sender<(bool, String)>);

/// One unacknowledged user send (`SandAckObligationStore`).
#[derive(Clone, Debug)]
pub struct AckObligation {
    pub created_at_ms: i64,
    pub coalesced_count: usize,
    pub redrive_attempts: usize,
    pub last_interrupt_at_ms: Option<i64>,
}

/// Options for [`AgentHost::send_prompt`].
#[derive(Clone, Debug, Default)]
pub struct SendOptions {
    /// Entry the message replies to (`replyToId`).
    pub reply_to: Option<EntryId>,
    /// Start a new thread from `reply_to` (`isFork`).
    pub is_fork: bool,
}

/// A send that was accepted; `result` resolves when its turn ends.
#[derive(Debug)]
pub struct AcceptedSend {
    pub entry_id: EntryId,
    pub result: oneshot::Receiver<RunResult>,
}

/// Shared runtime state (behind `Arc`).
pub struct HostInner {
    pub(crate) coordination: Arc<gns_store::CoordinationStore>,
    pub(crate) coordination_runtime: crate::coordination::CoordinationRuntime,
    pub config: AgentHostConfig,
    pub layout: AgentDirLayout,
    pub llm: Arc<dyn LlmProvider>,
    pub messaging: AgentToAgentMessaging,
    pub memory: Arc<MemoryServiceImpl>,
    pub routines: Arc<RoutineServiceImpl>,
    tools: RwLock<Vec<Arc<dyn Tool>>>,
    prompt: RwLock<SystemPromptAssembler>,
    middlewares: RwLock<Vec<Arc<dyn RunMiddleware>>>,
    policies: RwLock<Vec<Arc<dyn ToolPolicy>>>,
    approvals: Mutex<HashMap<String, PendingApproval>>,
    agents: RwLock<HashMap<AgentId, Arc<AgentHandle>>>,
    groups: RwLock<HashMap<GroupId, Arc<GroupHandle>>>,
    /// Agents deleted during this process (`deletedAgentIds`).
    deleted_agents: Mutex<HashSet<AgentId>>,
    events: broadcast::Sender<HostEvent>,
    shutdown: CancellationToken,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    actors: Mutex<HashMap<AgentId, JoinHandle<()>>>,
    pub(crate) mcp: crate::mcp::McpRegistry,
    room_buffers: Mutex<HashMap<RunId, (GroupId, Vec<OutboundMessage>)>>,
    run_cancels: Mutex<HashMap<RunId, CancellationToken>>,
    memory_versions: Mutex<HashMap<AgentId, u64>>,
    /// `<agent_profile_update>` blocks waiting for the agent's next turn.
    pending_profile_updates: Mutex<HashMap<AgentId, String>>,
    ack_obligations: Mutex<HashMap<AgentId, AckObligation>>,
    ack_timers: Mutex<HashMap<AgentId, u64>>,
    self_weak: Weak<HostInner>,
}

impl std::fmt::Debug for HostInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostInner").field("root", &self.layout.root()).finish_non_exhaustive()
    }
}

impl HostInner {
    pub(crate) fn arc(&self) -> Arc<HostInner> {
        self.self_weak.upgrade().expect("host dropped")
    }
    pub(crate) fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }
    pub(crate) fn emit(&self, event: HostEvent) {
        let _ = self.events.send(event);
    }
    pub(crate) fn services(&self) -> Arc<dyn HostServices> {
        self.arc()
    }
    pub(crate) fn middlewares(&self) -> Vec<Arc<dyn RunMiddleware>> {
        self.middlewares.read().map(|m| m.clone()).unwrap_or_default()
    }
    pub(crate) fn policies(&self) -> Vec<Arc<dyn ToolPolicy>> {
        self.policies.read().map(|p| p.clone()).unwrap_or_default()
    }
    pub(crate) fn is_agent_gone(&self, id: &str) -> bool {
        self.deleted_agents.lock().map(|d| d.contains(&AgentId::from(id))).unwrap_or(false)
    }

    /// Publish an approval request and wait for `AgentHost::approve`, the
    /// TTL, or cancellation of the turn.
    pub(crate) async fn request_approval(&self, request: ApprovalRequest, cancel: &CancellationToken) -> ApprovalOutcome {
        let (tx, rx) = oneshot::channel();
        let id = request.id.clone();
        let ttl = std::time::Duration::from_millis((request.expires_at - request.requested_at).max(0) as u64);
        if let Ok(mut a) = self.approvals.lock() {
            a.insert(id.clone(), (request.clone(), tx));
        }
        self.emit(HostEvent::ApprovalRequested { request });
        let outcome = tokio::select! {
            _ = cancel.cancelled() => ApprovalOutcome::Cancelled,
            _ = tokio::time::sleep(ttl) => ApprovalOutcome::TimedOut,
            decision = rx => match decision {
                Ok((true, _)) => ApprovalOutcome::Approved,
                Ok((false, reason)) => ApprovalOutcome::Denied(reason),
                Err(_) => ApprovalOutcome::Cancelled,
            },
        };
        if let Ok(mut a) = self.approvals.lock() {
            a.remove(&id);
        }
        outcome
    }
    pub(crate) fn agent(&self, id: &AgentId) -> Option<Arc<AgentHandle>> {
        self.agents.read().ok().and_then(|a| a.get(id).cloned())
    }
    pub(crate) fn group(&self, id: &GroupId) -> Option<Arc<GroupHandle>> {
        self.groups.read().ok().and_then(|g| g.get(id).cloned())
    }
    /// Agents ordered like the original roster: most recently updated first.
    pub(crate) fn all_agents(&self) -> Vec<Arc<AgentHandle>> {
        let mut agents: Vec<Arc<AgentHandle>> = self.agents.read().map(|a| a.values().cloned().collect()).unwrap_or_default();
        agents.sort_by_key(|a| std::cmp::Reverse(a.db.get_json::<i64>(kv_keys::LAST_TURN_AT).ok().flatten().unwrap_or(0)));
        agents
    }
    pub(crate) fn agent_ids(&self) -> Vec<AgentId> {
        self.agents.read().map(|a| a.keys().cloned().collect()).unwrap_or_default()
    }
    pub(crate) fn all_groups(&self) -> Vec<Arc<GroupHandle>> {
        let mut groups: Vec<Arc<GroupHandle>> = self.groups.read().map(|g| g.values().cloned().collect()).unwrap_or_default();
        groups.sort_by_key(|g| g.config().name.to_lowercase());
        groups
    }
    pub(crate) fn group_address(&self, group: &GroupHandle) -> GroupAddress {
        let config = group.config();
        GroupAddress {
            id: group.id.clone(),
            name: config.name,
            description: config.description,
            members: config.member_ids.iter().filter_map(|m| self.agent(m)).map(|a| a.address()).collect(),
        }
    }

    /// Resolve an agent by id or (unique) name.
    pub(crate) fn resolve_agent(&self, key: &str) -> Option<Arc<AgentHandle>> {
        if let Some(a) = self.agent(&AgentId::from(key)) {
            return Some(a);
        }
        let matches: Vec<Arc<AgentHandle>> = self.all_agents().into_iter().filter(|a| a.name().eq_ignore_ascii_case(key.trim())).collect();
        if matches.len() == 1 { matches.into_iter().next() } else { None }
    }
    pub(crate) fn resolve_group(&self, key: &str) -> Option<Arc<GroupHandle>> {
        if let Some(g) = self.group(&GroupId::from(key)) {
            return Some(g);
        }
        let matches: Vec<Arc<GroupHandle>> =
            self.all_groups().into_iter().filter(|g| g.config().name.eq_ignore_ascii_case(key.trim())).collect();
        if matches.len() == 1 { matches.into_iter().next() } else { None }
    }

    pub(crate) fn tools_for(&self, source: RunSource, in_group_room: bool, prompt_overridden: bool) -> Vec<Arc<dyn Tool>> {
        self.tools
            .read()
            .map(|t| t.iter().filter(|t| tool_available(t.as_ref(), source, in_group_room, prompt_overridden)).cloned().collect())
            .unwrap_or_default()
    }

    /// The tools one agent gets for a turn: registered built-ins minus the
    /// ones it switched off, plus the tools of its connected MCP servers
    /// (never for subagents).
    pub(crate) fn tools_for_agent(
        &self,
        handle: &AgentHandle,
        source: RunSource,
        in_group_room: bool,
        prompt_overridden: bool,
    ) -> Vec<Arc<dyn Tool>> {
        let disabled = handle.settings().tools.disabled;
        let mut tools: Vec<Arc<dyn Tool>> = self
            .tools_for(source, in_group_room, prompt_overridden)
            .into_iter()
            .filter(|t| !disabled.iter().any(|d| d == t.name()))
            .collect();
        if source != RunSource::Subagent {
            tools.extend(
                self.mcp.tools(&handle.id).into_iter().filter(|t| tool_available(t.as_ref(), source, in_group_room, prompt_overridden)),
            );
        }
        tools
    }

    /// Task mutations are only offered to the executor's active task run,
    /// never to a later chat turn that happens to contain its old task id.
    pub(crate) fn tools_for_run(&self, handle: &AgentHandle, options: &RunOptions) -> Vec<Arc<dyn Tool>> {
        let mut tools = self.tools_for_agent(handle, options.source, options.group_id.is_some(), options.base_prompt_override.is_some());
        tools.retain(|tool| match tool.name() {
            "UpdateTask" | "CompleteTask" => options.task_id.is_some(),
            "SendMessage" | "DelegateTask" => options.task_id.is_none(),
            _ => true,
        });
        tools
    }

    /// Every tool the agent could use, with its enabled flag.
    pub(crate) fn tool_listings(&self, handle: &AgentHandle) -> Vec<ToolListing> {
        let disabled = handle.settings().tools.disabled;
        let mut out: Vec<ToolListing> = self
            .tools
            .read()
            .map(|t| {
                t.iter()
                    .map(|t| ToolListing {
                        name: t.name().to_owned(),
                        description: gns_core::text::clamp_line(t.description(), 160),
                        source: ToolSource::Builtin,
                        enabled: !disabled.iter().any(|d| d == t.name()),
                    })
                    .collect()
            })
            .unwrap_or_default();
        for tool in self.mcp.tools(&handle.id) {
            let server = split_mcp_tool_name(tool.name()).map(|(s, _)| s.to_owned()).unwrap_or_default();
            out.push(ToolListing {
                name: tool.name().to_owned(),
                description: gns_core::text::clamp_line(tool.description(), 160),
                source: ToolSource::Mcp { server },
                enabled: true,
            });
        }
        out
    }

    fn extra_tools_render(&self, handle: &AgentHandle) -> Option<String> {
        let settings = handle.settings();
        render_extra_tools_section(&self.mcp.statuses(&handle.id, &settings), &settings.tools.disabled)
    }

    fn write_settings(&self, handle: &AgentHandle, settings: AgentSettings) -> Result<(), HostError> {
        SettingsFile::write(&self.layout.settings_json(&handle.id), &settings)?;
        handle.set_settings(settings);
        self.emit(HostEvent::ToolsChanged { agent_id: handle.id.clone() });
        Ok(())
    }

    pub(crate) fn register_run_cancel(&self, run_id: &RunId, cancel: &CancellationToken) {
        if let Ok(mut c) = self.run_cancels.lock() {
            c.insert(run_id.clone(), cancel.clone());
        }
    }
    pub(crate) fn unregister_run_cancel(&self, run_id: &RunId) {
        if let Ok(mut c) = self.run_cancels.lock() {
            c.remove(run_id);
        }
    }
    pub(crate) fn run_cancel_token(&self, run_id: &RunId) -> Option<CancellationToken> {
        self.run_cancels.lock().ok().and_then(|c| c.get(run_id).cloned())
    }

    /// Apply an extraction to the agent's own memory; returns how many
    /// facts changed. Removals only apply to facts the extractor was shown.
    pub(crate) fn apply_memory_extraction(&self, agent: &AgentId, extraction: &MemoryExtraction) -> usize {
        let mut applied = 0usize;
        for fact in &extraction.removals {
            if self.memory.forget(agent, fact, MemoryScope::Agent, None).is_ok() {
                applied += 1;
                self.emit(HostEvent::MemoryWritten { agent_id: agent.clone(), scope: "agent".into(), fact: format!("(forgot) {fact}") });
            }
        }
        for (content, tier) in &extraction.additions {
            // `addMemory(content, now, kind)`: notes already carry their
            // `[note] ` prefix and are stored as log facts.
            let tier = if *tier == MemoryTier::Note { MemoryTier::Log } else { *tier };
            if self.memory.write(agent, content, tier, MemoryScope::Agent, None).is_ok() {
                applied += 1;
                self.emit(HostEvent::MemoryWritten { agent_id: agent.clone(), scope: "agent".into(), fact: content.clone() });
            }
        }
        applied
    }

    /// Whether the agent is idle long enough, with enough new facts, to dream.
    pub(crate) fn should_dream(&self, agent: &AgentId, after: std::time::Duration) -> bool {
        let Some(handle) = self.agent(agent) else { return false };
        if handle.active_lane().is_some() || handle.dreaming.load(Ordering::SeqCst) {
            return false;
        }
        let last_turn: i64 = handle.db.get_json(kv_keys::LAST_TURN_AT).ok().flatten().unwrap_or(0);
        if last_turn == 0 || now_ms() - last_turn < after.as_millis() as i64 {
            return false;
        }
        let last_dream: i64 = handle.db.get_json(kv_keys::LAST_DREAM_AT).ok().flatten().unwrap_or(0);
        if last_dream >= last_turn {
            return false;
        }
        let Ok(recall) = self.memory.recall_all(&handle.id) else { return false };
        let dreamed_count: usize = handle.db.get_json(kv_keys::DREAM_LOG_COUNT).ok().flatten().unwrap_or(0);
        recall.agent.recent.len().saturating_sub(dreamed_count) >= MEMORY_DREAM_MIN_NEW_FACTS
    }

    /// Consolidate an agent's log into its profile (returns added, removed).
    pub(crate) async fn dream(&self, agent: &AgentId) -> Result<(usize, usize), HostError> {
        let handle = self.agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_string()))?;
        if handle.dreaming.swap(true, Ordering::SeqCst) {
            return Ok((0, 0));
        }
        let outcome = self.dream_inner(&handle).await;
        handle.dreaming.store(false, Ordering::SeqCst);
        outcome
    }

    async fn dream_inner(&self, handle: &AgentHandle) -> Result<(usize, usize), HostError> {
        let recall = self.memory.recall_all(&handle.id)?;
        let profile = recall.agent.profile.clone();
        let log: Vec<gns_core::memory::MemoryRecord> = recall.agent.recent.iter().rev().take(60).rev().cloned().collect();
        if log.is_empty() {
            return Ok((0, 0));
        }
        let raw = self.llm.complete_text(&build_dream_system_prompt(), &build_dream_user_prompt(&profile, &log)).await?;
        let known: Vec<String> = profile.iter().chain(log.iter()).map(|r| r.content.clone()).collect();
        let mut extraction = parse_memory_extraction(&raw, &known);
        for (_, tier) in &mut extraction.additions {
            *tier = MemoryTier::Profile;
        }
        let removed = extraction.removals.len();
        let added = extraction.additions.len();
        self.apply_memory_extraction(&handle.id, &extraction);
        handle.db.set_json(kv_keys::LAST_DREAM_AT, &now_ms())?;
        handle.db.set_json(kv_keys::DREAM_LOG_COUNT, &recall.agent.recent.len())?;
        self.emit(HostEvent::MemoryDreamed { agent_id: handle.id.clone(), added, removed });
        Ok((added, removed))
    }

    /// Agents the user @-mentioned in a message (excluding the recipient).
    fn mentioned_agents(&self, agent: &AgentId, text: &str) -> Vec<AgentAddress> {
        if !text.contains('@') {
            return Vec::new();
        }
        let members: Vec<GroupMember> = self
            .all_agents()
            .iter()
            .filter(|a| &a.id != agent)
            .map(|a| GroupMember { id: a.id.clone(), name: a.name(), description: String::new() })
            .collect();
        let mentions = parse_group_mentions(text, &members);
        self.all_agents().iter().filter(|a| mentions.member_ids.contains(&a.id)).map(|a| a.address()).collect()
    }

    pub(crate) fn open_room_buffer(&self, run_id: &RunId, group_id: &GroupId) {
        if let Ok(mut b) = self.room_buffers.lock() {
            b.insert(run_id.clone(), (group_id.clone(), Vec::new()));
        }
    }
    pub(crate) fn take_room_buffer(&self, run_id: &RunId) -> Vec<OutboundMessage> {
        self.room_buffers.lock().ok().and_then(|mut b| b.remove(run_id)).map(|(_, m)| m).unwrap_or_default()
    }

    pub(crate) fn bump_memory_version(&self, agent: &AgentId) {
        if let Ok(mut v) = self.memory_versions.lock() {
            *v.entry(agent.clone()).or_default() += 1;
        }
    }

    /// The `<agent_profile_update>` block owed to the agent's next turn, if any.
    pub(crate) fn take_profile_update_for_turn(&self, handle: &AgentHandle) -> Option<String> {
        self.pending_profile_updates.lock().ok().and_then(|mut p| p.remove(&handle.id))
    }

    /// `getAutomationStatusReminderForTurn`: the `<automation_status>` block
    /// when it changed since the last turn or the compaction epoch advanced.
    pub(crate) fn automation_status_reminder(
        &self,
        handle: &AgentHandle,
        compaction_epoch: u64,
        firing_id: Option<&str>,
    ) -> Option<String> {
        let rendered = self.routines.status_reminder(&handle.id, firing_id);
        let mut last = handle.last_automation_status.lock().ok()?;
        let advanced = last.as_ref().is_some_and(|(_, epoch)| compaction_epoch > *epoch);
        let out = match (&rendered, last.as_ref()) {
            (Some(text), Some((prev, _))) if prev == text && !advanced => None,
            (Some(text), _) => Some(text.clone()),
            (None, None) => None,
            (None, Some(_)) => {
                let clearing = gns_core::routine::render_automation_cleared_status_reminder();
                if last.as_ref().is_some_and(|(prev, _)| prev == &clearing) && !advanced { None } else { Some(clearing) }
            }
        };
        if let Some(text) = &out {
            *last = Some((text.clone(), compaction_epoch));
        }
        out
    }

    /// Assemble the system prompt for a turn. The memory block and the
    /// profile section are frozen per compaction epoch (prompt caching);
    /// `base_override` replaces only the base section (room turns).
    pub(crate) fn build_system_prompt(
        &self,
        handle: &AgentHandle,
        source: RunSource,
        base_override: Option<&str>,
    ) -> Result<String, HostError> {
        let epoch: u64 = handle.db.get_json(kv_keys::COMPACTION_EPOCH)?.unwrap_or(0);
        let snapshot: Option<MemoryPromptSnapshot> = handle.db.get_json(kv_keys::MEMORY_PROMPT_SNAPSHOT)?;
        let memory_render = match snapshot {
            Some(s) if s.compaction_epoch == epoch && !self.config.disable_memory_freeze => s.render,
            _ => {
                let recall = self.memory.recall_all(&handle.id)?;
                let has_facts = !recall.agent.profile.is_empty()
                    || !recall.agent.recent.is_empty()
                    || recall.user_shards.iter().any(|(_, r)| !r.profile.is_empty() || !r.recent.is_empty());
                let names: HashMap<AgentId, String> = self.all_agents().iter().map(|a| (a.id.clone(), a.name())).collect();
                let render = self.memory.render(&handle.id, &recall, &names);
                if has_facts && !self.config.disable_memory_freeze {
                    handle.db.set_json(
                        kv_keys::MEMORY_PROMPT_SNAPSHOT,
                        &MemoryPromptSnapshot { render: render.clone(), compaction_epoch: epoch },
                    )?;
                }
                render
            }
        };
        let profile_snapshot: Option<ProfilePromptSnapshot> = handle.db.get_json(kv_keys::AGENT_PROFILE_PROMPT_SNAPSHOT)?;
        let profile = match profile_snapshot {
            Some(s) if s.compaction_epoch == epoch => s.profile,
            _ => {
                let current = handle.profile();
                handle.db.set_json(
                    kv_keys::AGENT_PROFILE_PROMPT_SNAPSHOT,
                    &ProfilePromptSnapshot { profile: current.clone(), compaction_epoch: epoch },
                )?;
                current
            }
        };
        let others: Vec<AgentAddress> = self.all_agents().iter().filter(|a| a.id != handle.id).map(|a| a.address()).collect();
        let groups: Vec<GroupAddress> = crate::groups::groups_of(self, handle)
            .iter()
            .map(|g| {
                let mut address = self.group_address(g);
                address.members.retain(|m| m.id != handle.id);
                address
            })
            .collect();
        let ctx = PromptContext {
            agent_id: handle.id.clone(),
            profile,
            profile_path: handle.profile_path.clone(),
            data_dir: handle.data_dir.clone(),
            workspace_dir: handle.workspace_dir.clone(),
            agents_root: self.layout.agents_root(),
            memory_dir: self.layout.agent_memory_dir(&handle.id),
            automations_dir: self.layout.automations_dir(&handle.id),
            user_name: self.config.user_name.clone(),
            time_zone: self.config.time_zone.clone(),
            others,
            groups,
            source,
            extra_tools_render: self.extra_tools_render(handle),
            now_ms: now_ms(),
            memory_render: Some(memory_render),
            automations_render: Some(self.routines.render_section(&handle.id, self.config.time_zone.as_deref())),
            base_override: base_override.map(str::to_owned),
            is_subagent: source == RunSource::Subagent,
        };
        Ok(self.prompt.read().map(|p| p.render(&ctx)).unwrap_or_default())
    }

    fn load_agent(&self, id: AgentId) -> Result<Arc<AgentHandle>, HostError> {
        self.layout.ensure_agent_dirs(&id)?;
        let profile_path = self.layout.profile_json(&id);
        let profile = ProfileFile::read(&profile_path)?;
        let settings = SettingsFile::read(&self.layout.settings_json(&id))?;
        let db = Arc::new(AgentDb::open(&self.layout.store_db(&id))?);
        let all = db.all()?;
        let (_, start) = crate::history::window_bounds(&all);
        let history: Vec<TranscriptEntry> = all.into_iter().skip(start).collect();
        let paths =
            crate::agent::AgentPaths { data_dir: self.layout.agent_dir(&id), workspace_dir: self.layout.workspace_dir(&id), profile_path };
        let (handle, rx) = AgentHandle::new(id.clone(), paths, db, profile, settings, history);
        let task = spawn_actor(self.arc(), handle.clone(), rx);
        if let Ok(mut actors) = self.actors.lock() {
            actors.insert(id.clone(), task);
        }
        if let Ok(mut agents) = self.agents.write() {
            agents.insert(id, handle.clone());
        }
        Ok(handle)
    }

    /// A group is any agent directory whose `group.json` names members;
    /// its name and description live in the directory's `profile.json`.
    fn load_group(&self, id: GroupId) -> Result<Arc<GroupHandle>, HostError> {
        let mut config = GroupFile::read(&self.layout.group_json(&id))?.ok_or_else(|| HostError::GroupNotFound(id.to_string()))?;
        let profile = ProfileFile::read(&self.layout.profile_json(&AgentId::from(id.as_str()))).unwrap_or_default();
        config.name = profile.name;
        config.description = profile.description;
        let db = Arc::new(AgentDb::open(&self.layout.group_store_db(&id))?);
        let handle = GroupHandle::new(id.clone(), config, db);
        if let Ok(mut groups) = self.groups.write() {
            groups.insert(id, handle.clone());
        }
        Ok(handle)
    }

    fn write_group_files(&self, id: &GroupId, config: &GroupConfig) -> Result<(), HostError> {
        GroupFile::write(&self.layout.group_json(id), config)?;
        ProfileFile::write(
            &self.layout.profile_json(&AgentId::from(id.as_str())),
            &AgentProfile { name: config.name.clone(), description: config.description.clone(), ..Default::default() },
        )
    }

    /// `mintAgentSession` + `createAgent`: a fresh directory, `profile.json`,
    /// `settings.json`, `introductionPending` set; the kickstart runs only
    /// when requested (the UI's create path), never for tool-created agents.
    async fn create_agent_inner(&self, spec: AgentSpec, run_kickstart: bool) -> Result<Arc<AgentHandle>, HostError> {
        let name = spec.name.trim();
        let name = if name.is_empty() { SAND_DEFAULT_AGENT_NAME } else { name };
        let id = AgentId::new();
        self.layout.ensure_agent_dirs(&id)?;
        ProfileFile::write(
            &self.layout.profile_json(&id),
            &AgentProfile {
                name: name.to_owned(),
                description: spec.description.trim().to_owned(),
                title: spec.title.trim().to_owned(),
                avatar_shape: String::new(),
                avatar_color: String::new(),
            },
        )?;
        SettingsFile::write(&self.layout.settings_json(&id), &AgentSettings::default())?;
        let handle = self.load_agent(id)?;
        handle.db.set(kv_keys::AGENT_ORIGIN, "user")?;
        handle.db.set_json(kv_keys::INTRODUCTION_PENDING, &spec.kickstart)?;
        self.emit(HostEvent::AgentCreated { address: handle.address() });
        if run_kickstart && spec.kickstart && self.config.kickstart_new_agents {
            self.kickstart_if_pending(&handle)?;
        }
        Ok(handle)
    }

    /// `kickstartAgent`: run the hidden introduction on the user lane when
    /// it is still pending and no user message exists yet.
    pub(crate) fn kickstart_if_pending(&self, handle: &Arc<AgentHandle>) -> Result<bool, HostError> {
        let pending: bool = handle.db.get_json(kv_keys::INTRODUCTION_PENDING)?.unwrap_or(false);
        if !pending {
            return Ok(false);
        }
        if handle.history().iter().any(|e| matches!(e, TranscriptEntry::Message { role: Role::User, from_agent: None, .. })) {
            handle.db.set_json(kv_keys::INTRODUCTION_PENDING, &false)?;
            return Ok(false);
        }
        handle.enqueue(RunJob::hidden(kickstart_prompt(), RunOptions::kickstart()))?;
        Ok(true)
    }

    fn update_agent_inner(&self, id: &str, patch: ProfilePatch) -> Result<Option<AgentAddress>, HostError> {
        let Some(handle) = self.resolve_agent(id) else { return Ok(None) };
        let mut profile = handle.profile();
        let before = profile.clone();
        patch.apply(&mut profile);
        if profile == before {
            return Ok(Some(handle.address()));
        }
        ProfileFile::write(&handle.profile_path, &profile)?;
        handle.set_profile(profile.clone());
        if (before.name != profile.name || before.description != profile.description)
            && let Ok(mut p) = self.pending_profile_updates.lock()
        {
            p.insert(handle.id.clone(), render_agent_profile_update(&profile.name, &profile.description));
        }
        let address = handle.address();
        self.emit(HostEvent::AgentUpdated { address: address.clone() });
        Ok(Some(address))
    }

    // --- Ack obligations (`AckObligations`) ---

    fn record_ack_send(&self, agent: &AgentId, accepted_at_ms: i64) {
        self.clear_ack_timer(agent);
        if let Ok(mut o) = self.ack_obligations.lock() {
            match o.get_mut(agent) {
                Some(existing) => existing.coalesced_count += 1,
                None => {
                    o.insert(
                        agent.clone(),
                        AckObligation {
                            created_at_ms: accepted_at_ms,
                            coalesced_count: 0,
                            redrive_attempts: 0,
                            last_interrupt_at_ms: None,
                        },
                    );
                }
            }
        }
    }
    fn record_ack_interrupt(&self, agent: &AgentId, at_ms: i64) {
        if let Ok(mut o) = self.ack_obligations.lock()
            && let Some(existing) = o.get_mut(agent)
        {
            existing.last_interrupt_at_ms = Some(at_ms);
        }
    }
    pub(crate) fn fulfill_ack_obligation(&self, agent: &AgentId) {
        if let Ok(mut o) = self.ack_obligations.lock() {
            o.remove(agent);
        }
        self.clear_ack_timer(agent);
    }
    fn clear_ack_timer(&self, agent: &AgentId) {
        if let Ok(mut t) = self.ack_timers.lock() {
            t.remove(agent);
        }
    }
    /// Arm the idle redrive (`scheduleAckRedriveAfterIdle`).
    pub(crate) fn schedule_ack_redrive(self: &Arc<Self>, agent: &AgentId) {
        if self.ack_obligations.lock().map(|o| !o.contains_key(agent)).unwrap_or(true) {
            return;
        }
        let generation = now_ms() as u64;
        if let Ok(mut t) = self.ack_timers.lock() {
            t.insert(agent.clone(), generation);
        }
        let host = self.clone();
        let agent = agent.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(ACK_REDRIVE_IDLE_DELAY_MS)).await;
            if host.ack_timers.lock().map(|t| t.get(&agent) != Some(&generation)).unwrap_or(true) {
                return;
            }
            host.clear_ack_timer(&agent);
            host.redrive_ack_obligation(&agent);
        });
    }
    fn redrive_ack_obligation(self: &Arc<Self>, agent: &AgentId) {
        let Some(handle) = self.agent(agent) else {
            if let Ok(mut o) = self.ack_obligations.lock() {
                o.remove(agent);
            }
            return;
        };
        if handle.active_lane().is_some() {
            self.schedule_ack_redrive(agent);
            return;
        }
        let attempts = {
            let Ok(mut o) = self.ack_obligations.lock() else { return };
            let Some(existing) = o.get_mut(agent) else { return };
            if existing.redrive_attempts >= MAX_ACK_REDRIVES {
                o.remove(agent);
                tracing::warn!(agent = %agent, "ack obligation lost: max redrives");
                return;
            }
            existing.redrive_attempts += 1;
            existing.redrive_attempts
        };
        tracing::info!(agent = %agent, attempts, "ack redrive");
        let _ = handle.enqueue(RunJob::hidden(build_ack_redrive_prompt(), RunOptions::ack_redrive()));
    }

    /// `applySendRosterSideEffects`: name a default-named agent after its
    /// first message and clear a pending widget.
    fn apply_send_side_effects(&self, handle: &AgentHandle, trimmed: &str) -> Result<(), HostError> {
        let profile = handle.profile();
        let effective = if profile.name.trim().is_empty() { SAND_DEFAULT_AGENT_NAME } else { profile.name.trim() };
        let is_default = effective == SAND_DEFAULT_AGENT_NAME || effective == LEGACY_SAND_DEFAULT_AGENT_NAME;
        if is_default && handle.history().is_empty() {
            let seed = if trimmed.is_empty() { "New conversation" } else { trimmed };
            let seeded: String = seed.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(72).collect();
            let mut updated = profile.clone();
            updated.name = seeded;
            ProfileFile::write(&handle.profile_path, &updated)?;
            handle.set_profile(updated);
            self.emit(HostEvent::AgentUpdated { address: handle.address() });
        }
        if handle.db.get(kv_keys::AWAITING_USER_RESPONSE)?.is_some() {
            handle.db.delete(kv_keys::AWAITING_USER_RESPONSE)?;
        }
        Ok(())
    }

    /// `collectUnansweredQuestionPrompts`: widgets never answered are marked
    /// skipped and summarised for the next turn.
    fn collect_unanswered_questions(&self, handle: &AgentHandle) -> Option<String> {
        let mut skipped = Vec::new();
        let mut dismissed = Vec::new();
        for entry in handle.history() {
            let TranscriptEntry::SendMessage { message, responded_value, widget_skipped, widget_dismissed, .. } = &entry else { continue };
            if responded_value.is_some() || *widget_skipped || message.kind != OutboundKind::Widget {
                continue;
            }
            let Some(widget) = &message.widget else { continue };
            let summary = widget.prompt.trim().to_owned();
            if summary.is_empty() {
                continue;
            }
            if *widget_dismissed {
                dismissed.push(summary);
            } else {
                skipped.push(summary);
            }
            let mut marked = entry.clone();
            if let TranscriptEntry::SendMessage { widget_skipped, .. } = &mut marked {
                *widget_skipped = true;
            }
            let _ = handle.update(&marked);
            self.emit(HostEvent::EntryUpdated { agent_id: handle.id.clone(), entry: marked });
        }
        build_unanswered_questions_note(&skipped, &dismissed)
    }

    /// `SendPipeline.send` + `dispatchUserTurn`: persist the user message
    /// (`t<n>u`), apply the roster side effects, record the ack obligation,
    /// bump the send epoch, and queue the turn on the user lane. Returns at
    /// once; the receiver resolves when the turn ends.
    pub(crate) fn send_prompt(
        self: &Arc<Self>,
        handle: &Arc<AgentHandle>,
        text: String,
        images: Vec<ImageRef>,
        options: SendOptions,
    ) -> Result<AcceptedSend, HostError> {
        let trimmed = text.trim().to_owned();
        if trimmed.is_empty() && images.is_empty() {
            return Err(HostError::invalid("message is empty"));
        }
        handle.db.set_json(kv_keys::INTRODUCTION_PENDING, &false)?;
        self.apply_send_side_effects(handle, &trimmed)?;
        let reply_context = options.reply_to.as_ref().and_then(|target| {
            handle.history().iter().find(|e| e.id() == target).map(|e| {
                let quote = match e {
                    TranscriptEntry::Message { content, .. } => content.clone(),
                    TranscriptEntry::SendMessage { message, .. } => message.display_text(),
                    other => other.id().to_string(),
                };
                (target.clone(), gns_core::text::clamp_line(&quote, 120))
            })
        });
        let entry_id = handle.next_entry_id(EntryIdKind::UserMessage);
        let entry = TranscriptEntry::Message {
            artifacts: Vec::new(),
            id: entry_id.clone(),
            role: Role::User,
            content: trimmed.clone(),
            timestamp_ms: now_ms(),
            hidden: false,
            run_id: None,
            from_agent: None,
            to_agent: None,
            images: images.clone(),
            group_id: None,
            priority: false,
            user_name: None,
            reply_to: options.reply_to.clone(),
        };
        handle.append(&entry)?;
        self.emit(HostEvent::EntryAppended { agent_id: handle.id.clone(), entry });
        let accepted_at = now_ms();
        self.record_ack_send(&handle.id, accepted_at);
        let epoch = handle.next_turn_epoch();
        let carries_recovery = images.is_empty() && !options.is_fork;
        if carries_recovery {
            handle.latest_recovery_epoch.store(epoch, Ordering::SeqCst);
        } else {
            handle.recovery_break_epoch.store(epoch, Ordering::SeqCst);
        }
        let mut notes: Vec<String> = build_mentioned_agents_context(&self.mentioned_agents(&handle.id, &trimmed)).into_iter().collect();
        if let Some(unanswered) = self.collect_unanswered_questions(handle) {
            notes.insert(0, unanswered);
        }
        let job = RunJob {
            prompt: trimmed,
            options: RunOptions::user(),
            images,
            already_persisted: true,
            persist: true,
            context_notes: notes,
            message_id: Some(entry_id.clone()),
            epoch,
            carries_recovery,
            reply_context,
        };
        let was_active = handle.active_lane().is_some();
        if was_active {
            self.record_ack_interrupt(&handle.id, accepted_at);
        }
        let result = handle.enqueue_with_result(job)?;
        Ok(AcceptedSend { entry_id, result })
    }
}

#[async_trait]
impl HostServices for HostInner {
    async fn prepare_artifacts(&self, agent: &AgentId, files: Vec<String>, ids: Vec<String>) -> Result<Vec<ArtifactRef>, HostError> {
        self.prepare_files(agent, files, ids).await
    }
    async fn fetch_artifact(&self, agent: &AgentId, id: &str) -> Result<String, HostError> {
        self.fetch_file(agent, id).await
    }
    async fn send_to_agent_with_artifacts(
        &self,
        from: &AgentId,
        target: &str,
        text: String,
        images: Vec<ImageRef>,
        artifacts: Vec<ArtifactRef>,
        priority: bool,
    ) -> Result<String, HostError> {
        for a in &artifacts {
            self.check_artifact(from, &a.id)?;
        }
        self.messaging.send_with_artifacts(&self.arc(), from, target, &text, images, artifacts, priority).await
    }
    async fn delegate_task(&self, from: &AgentId, run: &RunId, request: DelegateTaskRequest) -> Result<TaskRecord, HostError> {
        self.create_task(from, run, request).await
    }
    async fn update_task(
        &self,
        agent: &AgentId,
        run: &RunId,
        id: &str,
        status: TaskStatus,
        summary: String,
    ) -> Result<TaskRecord, HostError> {
        self.update_task_progress(agent, run, id, status, summary)
    }
    async fn complete_task(&self, agent: &AgentId, run: &RunId, id: &str, result: CompleteTaskRequest) -> Result<TaskRecord, HostError> {
        self.finish_task(agent, run, id, result).await
    }
    fn get_task(&self, agent: &AgentId, id: &str) -> Result<TaskRecord, HostError> {
        self.task_access(agent, id)
    }
    fn background_task_context(&self, run: &RunId) -> Option<TaskRunContext> {
        let contexts = self.coordination_runtime.runs.lock().ok()?;
        let context = contexts.get(run)?;
        Some(TaskRunContext { task_id: context.task.clone()?, attempt: context.attempt, run_id: run.clone() })
    }
    fn wake_agent_for_task(&self, agent: &AgentId, task: Option<&TaskRunContext>, prompt: String) -> Result<(), HostError> {
        self.task_background_wake(agent, task, prompt)
    }

    fn roster(&self) -> Vec<AgentAddress> {
        self.all_agents().iter().map(|a| a.address()).collect()
    }
    fn groups(&self) -> Vec<GroupAddress> {
        self.all_groups().iter().map(|g| self.group_address(g)).collect()
    }
    async fn deliver_message(&self, from: &AgentId, run: &RunId, message: OutboundMessage) -> Result<EntryId, HostError> {
        let handle = self.agent(from).ok_or_else(|| HostError::AgentNotFound(from.to_string()))?;
        if self.task_for_run(run).is_some() {
            return Err(HostError::invalid("use UpdateTask or CompleteTask in a delegated task, not SendMessage"));
        }
        for artifact in &message.artifacts {
            self.check_artifact(from, &artifact.id)?;
        }
        let destination = self
            .coordination_runtime
            .runs
            .lock()
            .map_err(HostError::storage)?
            .get(run)
            .map(|c| c.origin.clone())
            .unwrap_or_else(|| ConversationRef::Agent(from.clone()));
        self.coordination.grant(&message.artifacts, &destination)?;
        let room = self.room_buffers.lock().ok().and_then(|mut b| {
            b.get_mut(run).map(|(gid, msgs)| {
                msgs.push(message.clone());
                gid.clone()
            })
        });
        if room.is_none() && handle.db.get(kv_keys::AWAITING_USER_RESPONSE)?.is_some() {
            return Err(HostError::invalid(gns_core::prompt::SAND_AWAITING_USER_SEND_MESSAGE_BLOCKED));
        }
        let entry_id = handle.next_entry_id(EntryIdKind::SendMessage);
        let entry = TranscriptEntry::SendMessage {
            id: entry_id.clone(),
            message: message.clone(),
            timestamp_ms: now_ms(),
            run_id: run.clone(),
            group_id: room.clone(),
            author: None,
            reply_to: message.reply_to.as_deref().map(EntryId::from),
            reactions: vec![],
            responded_value: None,
            widget_skipped: false,
            widget_dismissed: false,
        };
        handle.append(&entry)?;
        if room.is_none() {
            self.fulfill_ack_obligation(from);
            if matches!(message.kind, OutboundKind::Widget | OutboundKind::SecretRequest) {
                handle.db.set_json(
                    kv_keys::AWAITING_USER_RESPONSE,
                    &AwaitingUserResponse { run_id: run.clone(), reason: format!("{:?}", message.kind).to_lowercase(), since: now_ms() },
                )?;
            }
        }
        self.emit(HostEvent::EntryAppended { agent_id: from.clone(), entry });
        self.emit(HostEvent::SendMessage {
            agent_id: from.clone(),
            agent_name: handle.name(),
            run_id: run.clone(),
            entry_id: entry_id.clone(),
            message,
            group_id: room,
        });
        Ok(entry_id)
    }
    async fn send_to_agent(
        &self,
        from: &AgentId,
        target_id: &str,
        text: String,
        images: Vec<ImageRef>,
        priority: bool,
    ) -> Result<String, HostError> {
        self.messaging.send_to_agent(&self.arc(), from, target_id, &text, images, priority).await
    }
    async fn create_agent(&self, spec: AgentSpec) -> Result<AgentAddress, HostError> {
        // `createBackgroundAgent`: introduction pending, never kickstarted here.
        let spec = AgentSpec { kickstart: true, ..spec };
        Ok(self.create_agent_inner(spec, false).await?.address())
    }
    async fn update_agent(&self, id: &str, patch: ProfilePatch) -> Result<Option<AgentAddress>, HostError> {
        if self.group(&GroupId::from(id)).is_some() {
            return Ok(None);
        }
        self.update_agent_inner(id, patch)
    }
    fn update_settings(&self, id: &AgentId, patch: SettingsPatch) -> Result<(), HostError> {
        let handle = self.agent(id).ok_or_else(|| HostError::AgentNotFound(id.to_string()))?;
        let mut settings = handle.settings();
        patch.apply(&mut settings);
        SettingsFile::write(&self.layout.settings_json(id), &settings)?;
        handle.set_settings(settings);
        Ok(())
    }
    fn memory(&self) -> Arc<dyn MemoryService> {
        let host = self.arc();
        Arc::new(VersionedMemory { host })
    }
    fn routines(&self) -> Arc<dyn RoutineService> {
        self.routines.clone()
    }
    fn emit(&self, event: HostEvent) {
        HostInner::emit(self, event);
    }
    fn time_zone(&self) -> Option<String> {
        self.config.time_zone.clone()
    }
    async fn run_subagent(&self, parent: &AgentId, run: &RunId, spec: SubagentSpec) -> Result<SubagentResult, HostError> {
        let handle = self.agent(parent).ok_or_else(|| HostError::AgentNotFound(parent.to_string()))?;
        let cancel = self.run_cancel_token(run).unwrap_or_else(|| self.shutdown.child_token());
        Ok(crate::subagent::run_subagent(&self.arc(), &handle, run, spec, cancel).await)
    }
    fn wake_agent(&self, agent: &AgentId, prompt: String) -> Result<(), HostError> {
        let handle = self.agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_string()))?;
        handle.enqueue(RunJob::hidden(prompt, RunOptions::background_wake()))
    }
    fn list_tools(&self, agent: &AgentId) -> Result<Vec<ToolListing>, HostError> {
        let handle = self.agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_string()))?;
        Ok(self.tool_listings(&handle))
    }
    fn set_tool_enabled(&self, agent: &AgentId, tool: &str, enabled: bool) -> Result<(), HostError> {
        let handle = self.agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_string()))?;
        let tool = tool.trim();
        let known = self.tools.read().map(|t| t.iter().any(|t| t.name() == tool)).unwrap_or(false);
        if !known {
            return Err(HostError::invalid(format!("no built-in tool named \"{tool}\"")));
        }
        if !enabled && PROTECTED_TOOLS.contains(&tool) {
            return Err(HostError::invalid(format!("{tool} cannot be switched off")));
        }
        let mut settings = handle.settings();
        settings.tools.disabled.retain(|d| d != tool);
        if !enabled {
            settings.tools.disabled.push(tool.to_owned());
        }
        self.write_settings(&handle, settings)
    }
    fn mcp_servers(&self, agent: &AgentId) -> Result<Vec<McpServerStatus>, HostError> {
        let handle = self.agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_string()))?;
        Ok(self.mcp.statuses(&handle.id, &handle.settings()))
    }
    async fn add_mcp_server(&self, agent: &AgentId, config: McpServerConfig) -> Result<McpServerStatus, HostError> {
        let handle = self.agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_string()))?;
        config.validate().map_err(HostError::invalid)?;
        let mut settings = handle.settings();
        settings.mcp_servers.retain(|s| s.name != config.name);
        settings.mcp_servers.push(config.clone());
        self.write_settings(&handle, settings)?;
        self.mcp.sync(self, &handle, true).await;
        let statuses = self.mcp.statuses(&handle.id, &handle.settings());
        statuses.into_iter().find(|s| s.name == config.name).ok_or_else(|| HostError::Other("server vanished after attach".into()))
    }
    async fn remove_mcp_server(&self, agent: &AgentId, name: &str) -> Result<bool, HostError> {
        let handle = self.agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_string()))?;
        let mut settings = handle.settings();
        let before = settings.mcp_servers.len();
        settings.mcp_servers.retain(|s| s.name != name);
        if settings.mcp_servers.len() == before {
            return Ok(false);
        }
        self.write_settings(&handle, settings)?;
        self.mcp.forget_server(&handle.id, name).await;
        Ok(true)
    }
    async fn set_mcp_server_enabled(&self, agent: &AgentId, name: &str, enabled: bool) -> Result<McpServerStatus, HostError> {
        let handle = self.agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_string()))?;
        let mut settings = handle.settings();
        let server = settings
            .mcp_servers
            .iter_mut()
            .find(|s| s.name == name)
            .ok_or_else(|| HostError::invalid(format!("no MCP server named \"{name}\"")))?;
        server.enabled = enabled;
        self.write_settings(&handle, settings)?;
        self.mcp.sync(self, &handle, true).await;
        let statuses = self.mcp.statuses(&handle.id, &handle.settings());
        statuses.into_iter().find(|s| s.name == name).ok_or_else(|| HostError::Other("server vanished".into()))
    }
    async fn react_to_message(&self, from: &AgentId, _run: &RunId, message_address: &str, emoji: &str) -> Result<String, HostError> {
        let handle = self.agent(from).ok_or_else(|| HostError::AgentNotFound(from.to_string()))?;
        let address = message_address.trim();
        let emoji = emoji.trim();
        let entry = handle.db.entry(&EntryId::from(address.to_owned()))?;
        match entry {
            Some(TranscriptEntry::Message { role: Role::User, hidden: false, .. }) => {}
            _ => {
                return Err(HostError::invalid(format!(
                    "\"{address}\" isn't one of the user's messages. React with the [t3u]-style tag shown on the user's message."
                )));
            }
        }
        // Reactions live in the agent's KV store keyed by message address and toggle.
        let mut reactions: std::collections::BTreeMap<String, Vec<String>> = handle.db.get_json(REACTIONS_KV_KEY)?.unwrap_or_default();
        let slot = reactions.entry(address.to_owned()).or_default();
        if let Some(pos) = slot.iter().position(|e| e == emoji) {
            slot.remove(pos);
        } else {
            slot.push(emoji.to_owned());
        }
        if slot.is_empty() {
            reactions.remove(address);
        }
        handle.db.set_json(REACTIONS_KV_KEY, &reactions)?;
        Ok(format!("Reacted {emoji} on {address}. (Reactions toggle: react the same emoji again to take it back.)"))
    }
    fn create_project(&self, agent: &AgentId, slug: &str, name: &str, description: Option<&str>) -> Result<String, HostError> {
        let out = self.memory.create_project(agent, slug, name, description)?;
        self.invalidate_memory_snapshots(agent, gns_core::memory::MemoryScope::Project);
        Ok(out)
    }
    fn join_project(&self, agent: &AgentId, slug: &str) -> Result<String, HostError> {
        let out = self.memory.join_project(agent, slug)?;
        self.invalidate_memory_snapshots(agent, gns_core::memory::MemoryScope::Project);
        Ok(out)
    }
    fn leave_project(&self, agent: &AgentId, slug: &str) -> Result<String, HostError> {
        let out = self.memory.leave_project(agent, slug)?;
        self.invalidate_memory_snapshots(agent, gns_core::memory::MemoryScope::Project);
        Ok(out)
    }
    fn set_avatar(&self, agent: &AgentId, path: &str) -> Result<String, HostError> {
        let handle = self.agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_string()))?;
        let requested = path.trim();
        if requested.is_empty() {
            return Err(HostError::invalid("pass the path of an image file to install."));
        }
        let absolute = {
            let p = std::path::Path::new(requested);
            if p.is_absolute() { p.to_path_buf() } else { handle.workspace_dir.join(p) }
        };
        let basename = absolute.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| requested.to_owned());
        // Only images inside the agent's own directories are accepted.
        let inside = absolute.canonicalize().ok().is_some_and(|real| {
            [&handle.workspace_dir, &handle.data_dir].iter().any(|root| root.canonicalize().map(|r| real.starts_with(r)).unwrap_or(false))
        });
        let bytes = if inside { std::fs::read(&absolute).ok() } else { None };
        let Some(bytes) = bytes else {
            return Err(HostError::invalid(format!(
                "could not read \"{basename}\" — download or write the image somewhere inside your workspace first, then pass that path."
            )));
        };
        if bytes.is_empty() || bytes.len() > AVATAR_MAX_BYTES {
            return Err(HostError::invalid(format!("the image must be under {} MB and non-empty.", AVATAR_MAX_BYTES / (1024 * 1024))));
        }
        let Some(ext) = crate::memory::sniff_avatar_extension(&bytes) else {
            return Err(HostError::invalid("that file is not a recognized image (png, jpg, webp, gif, or svg)."));
        };
        std::fs::create_dir_all(&handle.data_dir)?;
        for name in crate::memory::list_conventional_avatar_filenames(&handle.data_dir) {
            let _ = std::fs::remove_file(handle.data_dir.join(name));
        }
        let filename = if ext == "png" { CANONICAL_AVATAR_FILENAME.to_owned() } else { format!("avatar.{ext}") };
        std::fs::write(handle.data_dir.join(&filename), &bytes)?;
        Ok(format!("Updated your picture ({filename}). Source {basename} can be deleted if you no longer need it."))
    }
    fn clear_avatar(&self, agent: &AgentId) -> Result<String, HostError> {
        let handle = self.agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_string()))?;
        let files = crate::memory::list_conventional_avatar_filenames(&handle.data_dir);
        if files.is_empty() {
            return Err(HostError::invalid("you already have the default picture."));
        }
        for name in files {
            let _ = std::fs::remove_file(handle.data_dir.join(name));
        }
        Ok("Cleared your picture — back to the default.".to_owned())
    }
}

/// Memory service wrapper that keeps the version counter (used by tests and
/// the dream path); the frozen prompt block itself only refreshes per
/// compaction epoch, as in the original.
struct VersionedMemory {
    host: Arc<HostInner>,
}

impl MemoryService for VersionedMemory {
    fn write(&self, agent: &AgentId, fact: &str, tier: MemoryTier, scope: MemoryScope, project: Option<&str>) -> Result<String, HostError> {
        let out = self.host.memory.write(agent, fact, tier, scope, project)?;
        self.host.invalidate_memory_snapshots(agent, scope);
        Ok(out)
    }
    fn forget(&self, agent: &AgentId, fact: &str, scope: MemoryScope, project: Option<&str>) -> Result<String, HostError> {
        let out = self.host.memory.forget(agent, fact, scope, project)?;
        self.host.invalidate_memory_snapshots(agent, scope);
        Ok(out)
    }
}

impl HostInner {
    fn invalidate_memory_snapshots(&self, agent: &AgentId, scope: MemoryScope) {
        match scope {
            MemoryScope::Agent => self.bump_memory_version(agent),
            _ => {
                for id in self.agent_ids() {
                    self.bump_memory_version(&id);
                }
            }
        }
    }
}

/// `isSameMemberSet`.
fn is_same_member_set(a: &[AgentId], b: &[AgentId]) -> bool {
    a.len() == b.len() && b.iter().all(|id| a.contains(id))
}

/// The public façade.
#[derive(Clone, Debug)]
pub struct AgentHost {
    pub(crate) inner: Arc<HostInner>,
}

impl AgentHost {
    /// Open a host at `config.root_dir`: loads every agent and group on disk,
    /// starts their actors and the routine scheduler.
    pub async fn open(mut config: AgentHostConfig, llm: Arc<dyn LlmProvider>) -> Result<Self, HostError> {
        std::fs::create_dir_all(&config.root_dir)?;
        // Keep canonical paths usable by native Windows shells and commands;
        // std::fs::canonicalize introduces a \\?\ prefix even for ordinary paths.
        config.root_dir = dunce::canonicalize(&config.root_dir)?;
        let layout = AgentDirLayout::new(&config.root_dir);
        std::fs::create_dir_all(layout.agents_root())?;
        let (events, _) = broadcast::channel(config.event_capacity);
        let memory = Arc::new(MemoryServiceImpl::new(layout.clone()));
        let routines = Arc::new(RoutineServiceImpl::new(layout.clone(), config.time_zone.as_deref()));
        let tools: Vec<Arc<dyn Tool>> = gns_tools::builtin_tools();
        let coordination = Arc::new(gns_store::CoordinationStore::open(&config.root_dir)?);
        let inner = Arc::new_cyclic(|weak| HostInner {
            coordination,
            coordination_runtime: crate::coordination::CoordinationRuntime::default(),
            prompt: RwLock::new(SystemPromptAssembler::standard(config.product_name.clone())),
            config,
            layout: layout.clone(),
            llm,
            messaging: AgentToAgentMessaging::default(),
            memory,
            routines,
            tools: RwLock::new(tools),
            middlewares: RwLock::new(Vec::new()),
            policies: RwLock::new(Vec::new()),
            approvals: Mutex::new(HashMap::new()),
            agents: RwLock::new(HashMap::new()),
            groups: RwLock::new(HashMap::new()),
            deleted_agents: Mutex::new(HashSet::new()),
            events,
            shutdown: CancellationToken::new(),
            tasks: Mutex::new(Vec::new()),
            actors: Mutex::new(HashMap::new()),
            mcp: crate::mcp::McpRegistry::default(),
            room_buffers: Mutex::new(HashMap::new()),
            run_cancels: Mutex::new(HashMap::new()),
            memory_versions: Mutex::new(HashMap::new()),
            pending_profile_updates: Mutex::new(HashMap::new()),
            ack_obligations: Mutex::new(HashMap::new()),
            ack_timers: Mutex::new(HashMap::new()),
            self_weak: weak.clone(),
        });
        let roster = Roster::new(layout);
        let (agents, groups) = roster.scan()?;
        for address in agents {
            if let Err(e) = inner.load_agent(address.id.clone()) {
                tracing::warn!(agent = %address.id, "skipping agent that failed to load: {e}");
            }
        }
        for group in groups {
            if let Err(e) = inner.load_group(group.id.clone()) {
                tracing::warn!(group = %group.id, "skipping group that failed to load: {e}");
            }
        }
        inner.recover_tasks()?;
        let coordination_worker = crate::coordination::spawn_coordination_worker(inner.clone());
        let scheduler = spawn_scheduler(inner.clone());
        if let Ok(mut t) = inner.tasks.lock() {
            t.push(scheduler);
            t.push(coordination_worker);
        }
        Ok(Self { inner })
    }

    /// Subscribe to host events.
    pub fn subscribe(&self) -> broadcast::Receiver<HostEvent> {
        self.inner.events.subscribe()
    }
    /// The configuration.
    pub fn config(&self) -> &AgentHostConfig {
        &self.inner.config
    }
    /// Register a custom tool (replacing any built-in with the same name).
    pub fn register_tool(&self, tool: Arc<dyn Tool>) {
        if let Ok(mut tools) = self.inner.tools.write() {
            tools.retain(|t| t.name() != tool.name());
            tools.push(tool);
        }
    }
    /// Remove a tool by name.
    pub fn unregister_tool(&self, name: &str) {
        if let Ok(mut tools) = self.inner.tools.write() {
            tools.retain(|t| t.name() != name);
        }
    }
    /// Names of registered tools.
    pub fn tool_names(&self) -> Vec<String> {
        self.inner.tools.read().map(|t| t.iter().map(|t| t.name().to_owned()).collect()).unwrap_or_default()
    }
    /// Insert a custom prompt section.
    pub fn add_prompt_section(&self, section: Arc<dyn PromptSection>, position: SectionPosition) {
        if let Ok(mut prompt) = self.inner.prompt.write() {
            prompt.insert(section, position);
        }
    }
    /// Remove a prompt section by id.
    pub fn remove_prompt_section(&self, id: &str) {
        if let Ok(mut prompt) = self.inner.prompt.write() {
            prompt.remove(id);
        }
    }
    /// Add a turn middleware.
    pub fn add_middleware(&self, middleware: Arc<dyn RunMiddleware>) {
        if let Ok(mut m) = self.inner.middlewares.write() {
            m.push(middleware);
        }
    }
    /// Add a tool policy (consulted before every tool call, in order).
    pub fn add_policy(&self, policy: Arc<dyn ToolPolicy>) {
        if let Ok(mut p) = self.inner.policies.write() {
            p.push(policy);
        }
    }
    /// Approval requests currently waiting for a decision.
    pub fn pending_approvals(&self) -> Vec<ApprovalRequest> {
        self.inner.approvals.lock().map(|a| a.values().map(|(r, _)| r.clone()).collect()).unwrap_or_default()
    }
    /// Resolve an approval request. Returns false when the id is unknown or expired.
    pub fn approve(&self, id: &str, approved: bool, reason: impl Into<String>) -> bool {
        let reason = reason.into();
        let Some((_, tx)) = self.inner.approvals.lock().ok().and_then(|mut a| a.remove(id)) else { return false };
        let delivered = tx.send((approved, reason.clone())).is_ok();
        self.inner.emit(HostEvent::ApprovalResolved { id: id.to_owned(), approved, reason });
        delivered
    }

    /// `deleteAgents`: stop the actor, drop its queues and delete its
    /// directory. Rooms keep their member list; a deleted member is skipped
    /// when the room resolves its members.
    pub async fn delete_agent(&self, agent: &str) -> Result<(), HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        if let Ok(mut d) = self.inner.deleted_agents.lock() {
            d.insert(handle.id.clone());
        }
        if let Ok(mut agents) = self.inner.agents.write() {
            agents.remove(&handle.id);
        }
        self.inner.fulfill_ack_obligation(&handle.id);
        handle.shutdown(); // cancels the running turn and drains the mailbox
        let actor = self.inner.actors.lock().ok().and_then(|mut a| a.remove(&handle.id));
        if let Some(actor) = actor
            && tokio::time::timeout(std::time::Duration::from_secs(10), actor).await.is_err()
        {
            tracing::warn!(agent = %handle.id, "actor did not stop within 10s; deleting files anyway");
        }
        self.inner.executor_deleted(&handle.id)?;
        self.inner.messaging.forget(&handle.id);
        self.inner.mcp.disconnect_agent(&handle.id).await;
        if let Ok(mut versions) = self.inner.memory_versions.lock() {
            versions.remove(&handle.id);
        }
        let dir = self.inner.layout.agent_dir(&handle.id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        self.inner.emit(HostEvent::AgentDeleted { agent_id: handle.id.clone() });
        Ok(())
    }
    /// Delete a group and its room history (the original deletes rooms like agents).
    pub fn delete_group(&self, group: &str) -> Result<(), HostError> {
        let handle = self.inner.resolve_group(group).ok_or_else(|| HostError::GroupNotFound(group.to_owned()))?;
        if let Ok(mut groups) = self.inner.groups.write() {
            groups.remove(&handle.id);
        }
        let dir = self.inner.layout.group_dir(&handle.id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        self.inner.emit(HostEvent::GroupDeleted { group_id: handle.id.clone() });
        Ok(())
    }
    /// Rename a group or change its description (`updateAgent` on a room).
    pub fn update_group(&self, group: &str, name: Option<String>, description: Option<String>) -> Result<GroupAddress, HostError> {
        let handle = self.inner.resolve_group(group).ok_or_else(|| HostError::GroupNotFound(group.to_owned()))?;
        let mut config = handle.config();
        if let Some(name) = name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()) {
            config.name = name;
        }
        if let Some(description) = description {
            config.description = description.trim().to_owned();
        }
        self.inner.write_group_files(&handle.id, &config)?;
        handle.set_config(config);
        Ok(self.inner.group_address(&handle))
    }

    /// Create an agent (runs the kickstart turn when `spec.kickstart`).
    pub async fn create_agent(&self, spec: AgentSpec) -> Result<AgentAddress, HostError> {
        Ok(self.inner.create_agent_inner(spec, true).await?.address())
    }
    /// Run the pending introduction of an agent, if any (`kickstartAgent`).
    pub fn kickstart_agent(&self, agent: &str) -> Result<bool, HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        self.inner.kickstart_if_pending(&handle)
    }
    /// Patch an agent's profile by id or name.
    pub async fn update_agent(&self, id: &str, patch: ProfilePatch) -> Result<AgentAddress, HostError> {
        self.inner.update_agent_inner(id, patch)?.ok_or_else(|| HostError::AgentNotFound(id.to_owned()))
    }
    /// Patch an agent's settings.
    pub fn update_agent_settings(&self, id: &str, patch: SettingsPatch) -> Result<(), HostError> {
        let handle = self.inner.resolve_agent(id).ok_or_else(|| HostError::AgentNotFound(id.to_owned()))?;
        HostServices::update_settings(self.inner.as_ref(), &handle.id, patch)
    }
    /// All agents.
    pub fn list_agents(&self) -> Vec<AgentAddress> {
        self.inner.roster()
    }
    /// All groups.
    pub fn list_groups(&self) -> Vec<GroupAddress> {
        HostServices::groups(&*self.inner)
    }
    /// Resolve an agent by id or unique name.
    pub fn find_agent(&self, key: &str) -> Option<AgentAddress> {
        self.inner.resolve_agent(key).map(|a| a.address())
    }
    /// Resolve a group by id or unique name.
    pub fn find_group(&self, key: &str) -> Option<GroupAddress> {
        self.inner.resolve_group(key).map(|g| self.inner.group_address(&g))
    }
    /// Low-level handle (transcript, settings, queue).
    pub fn agent_handle(&self, key: &str) -> Option<Arc<AgentHandle>> {
        self.inner.resolve_agent(key)
    }

    /// `sendPrompt`: accept a user message and return at once (the receiver
    /// resolves with the turn's result). A message to a group posts into
    /// the room and starts a round instead.
    pub fn send_prompt(
        &self,
        agent: &str,
        text: impl Into<String>,
        images: Vec<ImageRef>,
        options: SendOptions,
    ) -> Result<AcceptedSend, HostError> {
        let text = text.into();
        if let Some(group) = self.inner.resolve_group(agent) {
            let inner = self.inner.clone();
            let (tx, rx) = oneshot::channel();
            let entry_id = EntryId::new();
            tokio::spawn(async move {
                let _ = crate::groups::post_as_user(&inner, &group, text).await;
                let _ = tx.send(RunResult::default());
            });
            return Ok(AcceptedSend { entry_id, result: rx });
        }
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        self.inner.send_prompt(&handle, text, images, options)
    }
    /// Send a user message and wait for the turn.
    pub async fn send_user_message(&self, agent: &str, text: impl Into<String>, images: Vec<ImageRef>) -> Result<RunResult, HostError> {
        let accepted = self.send_prompt(agent, text, images, SendOptions::default())?;
        accepted.result.await.map_err(|_| HostError::ShuttingDown)
    }

    /// Consolidate an agent's memory now (normally runs when the agent is idle).
    /// Returns (facts added to the profile, facts removed).
    pub async fn dream(&self, agent: &str) -> Result<(usize, usize), HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        self.inner.dream(&handle.id).await
    }

    /// Deliver an external event. Every enabled routine (of any agent) with a
    /// matching trigger is fired with the payload. Returns how many.
    pub fn fire_webhook(&self, name: &str, payload: serde_json::Value) -> Result<usize, HostError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(HostError::invalid("webhook name is required"));
        }
        let event = RoutineServiceImpl::webhook_to_event(name, payload);
        let fired = crate::routines::fire_event(&self.inner, event);
        self.inner.emit(HostEvent::WebhookFired { name: name.to_owned(), routines: fired });
        Ok(fired)
    }
    /// `respondToWidget`: stamp the chosen value on the widget entry and
    /// deliver it as the user's reply.
    pub async fn respond_to_widget(&self, agent: &str, entry_id: &str, value: impl Into<String>) -> Result<RunResult, HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        let value = value.into();
        if let Some(entry) = handle.history().into_iter().find(|e| e.id().as_str() == entry_id) {
            let mut updated = entry;
            if let TranscriptEntry::SendMessage { responded_value, .. } = &mut updated {
                *responded_value = Some(value.trim().to_owned());
            }
            handle.update(&updated)?;
            self.inner.emit(HostEvent::EntryUpdated { agent_id: handle.id.clone(), entry: updated });
        }
        self.send_user_message(agent, value, Vec::new()).await
    }
    /// `dismissWidget`: the user declined to answer.
    pub fn dismiss_widget(&self, agent: &str, entry_id: &str) -> Result<(), HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        if let Some(entry) = handle.history().into_iter().find(|e| e.id().as_str() == entry_id) {
            let mut updated = entry;
            if let TranscriptEntry::SendMessage { widget_dismissed, .. } = &mut updated {
                *widget_dismissed = true;
            }
            handle.update(&updated)?;
            self.inner.emit(HostEvent::EntryUpdated { agent_id: handle.id.clone(), entry: updated });
        }
        handle.db.delete(kv_keys::AWAITING_USER_RESPONSE)?;
        Ok(())
    }
    /// Answer the newest unanswered widget (delivered as the user's reply).
    pub async fn answer_widget(&self, agent: &str, value: impl Into<String>) -> Result<RunResult, HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        let value = value.into();
        let pending = handle.history().into_iter().rev().find(|e| {
            matches!(e, TranscriptEntry::SendMessage { message, responded_value: None, widget_skipped: false, widget_dismissed: false, .. } if message.kind == OutboundKind::Widget)
        });
        match pending {
            Some(entry) => self.respond_to_widget(agent, entry.id().as_str(), value).await,
            None => self.send_user_message(agent, value, Vec::new()).await,
        }
    }
    /// `reactToMessage`: toggle the user's emoji reaction on an agent message.
    pub fn react_to_message(&self, agent: &str, entry_id: &str, emoji: &str) -> Result<(), HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        let Some(entry) = handle.history().into_iter().find(|e| e.id().as_str() == entry_id) else {
            return Err(HostError::invalid("no such message"));
        };
        let mut updated = entry;
        if let TranscriptEntry::SendMessage { reactions, .. } = &mut updated {
            if let Some(pos) = reactions.iter().position(|r| r.emoji == emoji && r.by == "me") {
                reactions.remove(pos);
            } else {
                reactions.push(Reaction { emoji: emoji.to_owned(), by: "me".to_owned() });
            }
        }
        handle.update(&updated)?;
        self.inner.emit(HostEvent::EntryUpdated { agent_id: handle.id.clone(), entry: updated });
        Ok(())
    }
    /// Whether the agent is waiting on a widget answer.
    pub fn is_awaiting_user(&self, agent: &str) -> bool {
        self.inner.resolve_agent(agent).and_then(|h| h.db.get(kv_keys::AWAITING_USER_RESPONSE).ok().flatten()).is_some()
    }
    /// Queue a hidden turn without waiting.
    pub fn enqueue(&self, agent: &str, job: RunJob) -> Result<(), HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        handle.enqueue(job)
    }
    /// `broadcastToAgents`: a hidden `[broadcast]` turn on the background
    /// lane for every target (or every agent). Returns (total, scheduled).
    pub fn broadcast_to_agents(&self, targets: Option<&[String]>, text: &str) -> Result<(usize, usize), HostError> {
        let text = gns_core::messaging::clamp_agent_message(text);
        if text.is_empty() {
            return Ok((0, 0));
        }
        let prompt = build_admin_broadcast_wake_prompt(&text);
        let ids: Vec<AgentId> = match targets {
            Some(list) => {
                let mut seen = Vec::new();
                for id in list {
                    let id = AgentId::from(id.as_str());
                    if !seen.contains(&id) {
                        seen.push(id);
                    }
                }
                seen
            }
            None => self.inner.agent_ids(),
        };
        let mut scheduled = 0usize;
        for id in &ids {
            if self.inner.is_agent_gone(id.as_str()) {
                continue;
            }
            let Some(handle) = self.inner.agent(id) else { continue };
            if handle.enqueue(RunJob::hidden(prompt.clone(), RunOptions::broadcast())).is_ok() {
                scheduled += 1;
            }
        }
        Ok((ids.len(), scheduled))
    }

    /// `createGroup`: unknown members are dropped, groups cannot nest, at most
    /// `GROUP_MAX_MEMBERS`; an existing group with the same member set is
    /// returned instead of creating a duplicate.
    pub async fn create_group(&self, spec: GroupSpec) -> Result<GroupAddress, HostError> {
        let mut requested: Vec<AgentId> = Vec::new();
        for member in &spec.members {
            if !requested.contains(member) {
                requested.push(member.clone());
            }
        }
        let nested: Vec<&AgentId> = requested.iter().filter(|m| self.inner.group(&GroupId::from(m.as_str())).is_some()).collect();
        if !nested.is_empty() {
            return Err(HostError::invalid(format!(
                "A group chat can only contain individual agents, not other group chats. Remove the group chat{} from the member list.",
                if nested.len() == 1 { "" } else { "s" }
            )));
        }
        let member_ids: Vec<AgentId> = requested.into_iter().filter(|m| self.inner.agent(m).is_some()).take(GROUP_MAX_MEMBERS).collect();
        if member_ids.is_empty() {
            return Err(HostError::invalid("A group needs at least one existing member agent."));
        }
        if let Some(existing) = self.inner.all_groups().into_iter().find(|g| is_same_member_set(&g.config().member_ids, &member_ids)) {
            return Ok(self.inner.group_address(&existing));
        }
        let id = GroupId::new();
        self.inner.layout.ensure_group_dirs(&id)?;
        let config =
            GroupConfig { name: spec.name.trim().to_owned(), description: spec.description.trim().to_owned(), member_ids, version: 1 };
        self.inner.write_group_files(&id, &config)?;
        let group = self.inner.load_group(id)?;
        let address = self.inner.group_address(&group);
        self.inner.emit(HostEvent::GroupCreated { group: address.clone() });
        Ok(address)
    }
    /// `setGroupMembers`: unknown ids and the group itself are dropped, at
    /// most `GROUP_MAX_MEMBERS`; an empty result keeps the current members.
    pub fn set_group_members(&self, group: &str, members: Vec<AgentId>) -> Result<GroupAddress, HostError> {
        let handle = self.inner.resolve_group(group).ok_or_else(|| HostError::GroupNotFound(group.to_owned()))?;
        let mut requested: Vec<AgentId> = Vec::new();
        for m in members {
            if !requested.contains(&m) {
                requested.push(m);
            }
        }
        if requested.iter().any(|m| self.inner.group(&GroupId::from(m.as_str())).is_some()) {
            return Err(HostError::invalid("A group chat can only contain individual agents, not other group chats."));
        }
        let cleaned: Vec<AgentId> = requested
            .into_iter()
            .filter(|m| m.as_str() != handle.id.as_str() && self.inner.agent(m).is_some())
            .take(GROUP_MAX_MEMBERS)
            .collect();
        if !cleaned.is_empty() {
            let mut config = handle.config();
            config.member_ids = cleaned;
            self.inner.write_group_files(&handle.id, &config)?;
            handle.set_config(config);
        }
        Ok(self.inner.group_address(&handle))
    }
    /// Post into a room as the user and start a round of member turns.
    pub async fn post_to_group_as_user(&self, group: &str, text: impl Into<String>) -> Result<(), HostError> {
        let handle = self.inner.resolve_group(group).ok_or_else(|| HostError::GroupNotFound(group.to_owned()))?;
        crate::groups::post_as_user(&self.inner, &handle, text.into()).await
    }
    /// Room history.
    pub fn group_history(&self, group: &str) -> Result<Vec<gns_core::groups::GroupMessage>, HostError> {
        let handle = self.inner.resolve_group(group).ok_or_else(|| HostError::GroupNotFound(group.to_owned()))?;
        Ok(handle.history())
    }

    /// Newest `limit` transcript entries for an agent.
    pub fn transcript(&self, agent: &str, limit: usize) -> Result<Vec<TranscriptEntry>, HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        handle.db.tail(limit)
    }
    /// A page of transcript entries before `before_seq` (newest first),
    /// like `getAgentTranscriptPage`.
    pub fn transcript_page(&self, agent: &str, before_seq: Option<i64>, limit: usize) -> Result<Vec<(i64, TranscriptEntry)>, HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        handle.db.page_before(before_seq, limit.clamp(1, 5_000))
    }
    /// Accumulated token usage for an agent.
    pub fn usage(&self, agent: &str) -> Result<UsageTotals, HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        Ok(handle.db.get_json(kv_keys::USAGE_TOTALS)?.unwrap_or_default())
    }
    /// Routines of an agent.
    pub fn routines(&self, agent: &str) -> Result<Vec<gns_core::routine::AutomationRecord>, HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        self.inner.routines.list(&handle.id)
    }
    /// The agent's tool list: built-ins with their enabled flag plus MCP tools.
    pub fn list_tools(&self, agent: &str) -> Result<Vec<ToolListing>, HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        HostServices::list_tools(self.inner.as_ref(), &handle.id)
    }
    /// Switch a built-in tool on or off for one agent (SendMessage stays on).
    pub fn set_tool_enabled(&self, agent: &str, tool: &str, enabled: bool) -> Result<(), HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        HostServices::set_tool_enabled(self.inner.as_ref(), &handle.id, tool, enabled)
    }
    /// MCP servers attached to an agent and whether they are connected.
    pub fn mcp_servers(&self, agent: &str) -> Result<Vec<McpServerStatus>, HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        HostServices::mcp_servers(self.inner.as_ref(), &handle.id)
    }
    /// Attach (or replace) an MCP server and connect it now.
    pub async fn add_mcp_server(&self, agent: &str, config: McpServerConfig) -> Result<McpServerStatus, HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        HostServices::add_mcp_server(self.inner.as_ref(), &handle.id, config).await
    }
    /// Detach an MCP server (its process is stopped).
    pub async fn remove_mcp_server(&self, agent: &str, name: &str) -> Result<bool, HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        HostServices::remove_mcp_server(self.inner.as_ref(), &handle.id, name).await
    }
    /// Enable or disable an attached MCP server.
    pub async fn set_mcp_server_enabled(&self, agent: &str, name: &str, enabled: bool) -> Result<McpServerStatus, HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        HostServices::set_mcp_server_enabled(self.inner.as_ref(), &handle.id, name, enabled).await
    }
    /// Reconnect the agent's MCP servers now (ignores the retry backoff).
    pub async fn sync_mcp(&self, agent: &str) -> Result<Vec<McpServerStatus>, HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        self.inner.mcp.sync(self.inner.as_ref(), &handle, true).await;
        Ok(self.inner.mcp.statuses(&handle.id, &handle.settings()))
    }
    /// Fire a routine now (manual run, `runAgentAutomationNow`).
    pub async fn run_routine_now(&self, agent: &str, routine_id: &str) -> Result<RunResult, HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        let record =
            self.inner.routines.get(&handle.id, routine_id)?.ok_or_else(|| HostError::invalid(format!("no routine {routine_id}")))?;
        crate::routines::fire_routine(
            &self.inner,
            &handle.id,
            record,
            gns_core::routine::AutomationRunTrigger::Manual,
            Vec::new(),
            Vec::new(),
        )
        .await
    }
    /// Record a fact in an agent's own memory (same path as `update_state`).
    pub fn memory_write(&self, agent: &str, fact: &str, tier: MemoryTier) -> Result<String, HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        let out = self.inner.memory.write(&handle.id, fact, tier, MemoryScope::Agent, None)?;
        self.inner.bump_memory_version(&handle.id);
        Ok(out)
    }
    /// Full memory recall for an agent.
    pub fn memory_recall(&self, agent: &str) -> Result<crate::memory::FullRecall, HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        self.inner.memory.recall_all(&handle.id)
    }
    /// Pending cross-agent messages for an agent.
    pub fn pending_inbound(&self, agent: &str) -> usize {
        self.inner.resolve_agent(agent).map(|h| self.inner.messaging.pending_count(&h.id)).unwrap_or(0)
    }
    /// The assembled system prompt (for inspection).
    pub fn system_prompt(&self, agent: &str) -> Result<String, HostError> {
        let handle = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        self.inner.build_system_prompt(&handle, RunSource::User, None)
    }

    /// Stop every actor and background task.
    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        self.inner.mcp.close_all().await;
        for handle in self.inner.all_agents() {
            handle.shutdown();
        }
        let mut tasks: Vec<JoinHandle<()>> = self.inner.tasks.lock().map(|mut t| std::mem::take(&mut *t)).unwrap_or_default();
        tasks.extend(self.inner.actors.lock().map(|mut a| a.drain().map(|(_, t)| t).collect::<Vec<_>>()).unwrap_or_default());
        for task in tasks {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
        }
    }
}
