//! Per-agent handle: cached profile/settings, database and the actor mailbox.

use gns_core::*;
use gns_store::AgentDb;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{mpsc, oneshot};

/// A unit of work for an agent's actor (the original's `runner.run(prompt, options)`
/// plus the scheduler's task metadata).
#[derive(Clone, Debug)]
pub struct RunJob {
    /// The prompt for this turn (user text or a hidden cue prompt).
    pub prompt: String,
    pub options: RunOptions,
    pub images: Vec<ImageRef>,
    /// The prompt's transcript entry already exists (user messages are
    /// persisted at send time, inbound agent messages before the wake).
    pub already_persisted: bool,
    /// Persist this turn to the transcript (false for subagent runs).
    pub persist: bool,
    /// Extra hidden system notes persisted right after the prompt (e.g. the
    /// teammates the user @-mentioned).
    pub context_notes: Vec<String>,
    /// The persisted user entry this turn answers (its address `[t3u]`).
    pub message_id: Option<EntryId>,
    /// Send epoch of a user turn; an older epoch than the session's current
    /// one marks a queued turn as superseded.
    pub epoch: u64,
    /// A plain-text user send that a later turn can recover by prepending
    /// (no attachments, not a fork): only such sends interrupt an undispatched run.
    pub carries_recovery: bool,
    /// Reply context (`[In reply to <id>: "<quote>"]`).
    pub reply_context: Option<(EntryId, String)>,
}

impl RunJob {
    /// A visible user turn.
    pub fn user(prompt: impl Into<String>, images: Vec<ImageRef>) -> Self {
        let images_empty = images.is_empty();
        Self {
            prompt: prompt.into(),
            options: RunOptions::user(),
            images,
            already_persisted: false,
            persist: true,
            context_notes: Vec::new(),
            message_id: None,
            epoch: 0,
            carries_recovery: images_empty,
            reply_context: None,
        }
    }
    /// Attach context notes (persisted as hidden system entries).
    pub fn with_context_notes(mut self, notes: Vec<String>) -> Self {
        self.context_notes = notes;
        self
    }
    /// A hidden turn with the given options.
    pub fn hidden(prompt: impl Into<String>, options: RunOptions) -> Self {
        Self {
            prompt: prompt.into(),
            options,
            images: Vec::new(),
            already_persisted: false,
            persist: true,
            context_notes: Vec::new(),
            message_id: None,
            epoch: 0,
            carries_recovery: false,
            reply_context: None,
        }
    }
}

/// Messages understood by an agent actor.
#[derive(Debug)]
pub(crate) enum AgentCommand {
    Run {
        job: Box<RunJob>,
        reply: oneshot::Sender<RunResult>,
    },
    /// Cancel the current turn when its lane is strictly below the threshold
    /// (`steerRecipientForPriorityPeer`: never a user turn).
    Interrupt {
        reason: String,
        only_if_lane_below: Lane,
    },
    Shutdown,
}

/// Filesystem locations of one agent.
#[derive(Clone, Debug)]
pub(crate) struct AgentPaths {
    pub data_dir: PathBuf,
    pub workspace_dir: PathBuf,
    pub profile_path: PathBuf,
}

/// Runtime state of one agent.
pub struct AgentHandle {
    pub id: AgentId,
    pub data_dir: PathBuf,
    pub workspace_dir: PathBuf,
    pub profile_path: PathBuf,
    pub db: Arc<AgentDb>,
    profile: RwLock<AgentProfile>,
    settings: RwLock<AgentSettings>,
    mailbox: mpsc::UnboundedSender<AgentCommand>,
    active_lane: Mutex<Option<Lane>>,
    /// The active run has sent its first model request (`noteDispatched`).
    pub(crate) dispatched: AtomicBool,
    /// The active 1:1 run was interrupted by a user message or a priority
    /// peer (`dmPreemptedWakeAgentIds`): inbound wakes may be redelivered.
    pub(crate) dm_preempted_wake: AtomicBool,
    /// The active room turn was interrupted by a direct message
    /// (`dmPreemptedGroupMemberIds`): the member turn may be retried.
    pub(crate) dm_preempted_group: AtomicBool,
    /// Send epoch of the newest user message (`currentTurnEpoch`).
    pub(crate) turn_epoch: AtomicU64,
    /// Epoch of the newest recovery-carrying send (`latestRecoverySends`).
    pub(crate) latest_recovery_epoch: AtomicU64,
    /// Epoch of the newest non-recoverable send (`recoveryBreakEpochs`).
    pub(crate) recovery_break_epoch: AtomicU64,
    /// In-memory copy of the transcript window (avoids re-reading the DB every step).
    history: Mutex<Vec<TranscriptEntry>>,
    /// A background compaction is in flight.
    pub(crate) compacting: AtomicBool,
    /// A dream (memory consolidation) is in flight.
    pub(crate) dreaming: AtomicBool,
    /// Last `<automation_status>` reminder sent, with the compaction epoch it was sent at.
    pub(crate) last_automation_status: Mutex<Option<(String, u64)>>,
}

impl std::fmt::Debug for AgentHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentHandle").field("id", &self.id).field("name", &self.name()).finish_non_exhaustive()
    }
}

impl AgentHandle {
    pub(crate) fn new(
        id: AgentId,
        paths: AgentPaths,
        db: Arc<AgentDb>,
        profile: AgentProfile,
        settings: AgentSettings,
        history: Vec<TranscriptEntry>,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<AgentCommand>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = Arc::new(Self {
            id,
            data_dir: paths.data_dir,
            workspace_dir: paths.workspace_dir,
            profile_path: paths.profile_path,
            db,
            profile: RwLock::new(profile),
            settings: RwLock::new(settings),
            mailbox: tx,
            active_lane: Mutex::new(None),
            dispatched: AtomicBool::new(false),
            dm_preempted_wake: AtomicBool::new(false),
            dm_preempted_group: AtomicBool::new(false),
            turn_epoch: AtomicU64::new(0),
            latest_recovery_epoch: AtomicU64::new(0),
            recovery_break_epoch: AtomicU64::new(0),
            history: Mutex::new(history),
            compacting: AtomicBool::new(false),
            dreaming: AtomicBool::new(false),
            last_automation_status: Mutex::new(None),
        });
        (handle, rx)
    }

    /// Append a transcript entry (database + cache).
    pub fn append(&self, entry: &TranscriptEntry) -> Result<i64, HostError> {
        let seq = self.db.append_entry(entry)?;
        if let Ok(mut h) = self.history.lock() {
            h.push(entry.clone());
        }
        Ok(seq)
    }

    /// Replace a transcript entry by id (database + cache).
    pub fn update(&self, entry: &TranscriptEntry) -> Result<bool, HostError> {
        let updated = self.db.update_entry(entry)?;
        if let Ok(mut h) = self.history.lock()
            && let Some(slot) = h.iter_mut().find(|e| e.id() == entry.id())
        {
            *slot = entry.clone();
        }
        Ok(updated)
    }

    /// The cached transcript window (chronological).
    pub fn history(&self) -> Vec<TranscriptEntry> {
        self.history.lock().map(|h| h.clone()).unwrap_or_default()
    }

    /// Mint the next entry id in the original's address scheme
    /// (`t<n>u`, `t<turn>s<k>`, `t<turn>a<k>`), unique within the transcript.
    pub fn next_entry_id(&self, kind: EntryIdKind) -> EntryId {
        let all = self.db.all().unwrap_or_default();
        next_entry_id(&all, kind)
    }

    /// Drop cached entries before `keep_from` (after a compaction).
    pub(crate) fn prune_history(&self, keep_from: usize) {
        if let Ok(mut h) = self.history.lock()
            && keep_from <= h.len()
        {
            h.drain(..keep_from);
        }
    }

    pub fn profile(&self) -> AgentProfile {
        self.profile.read().map(|p| p.clone()).unwrap_or_default()
    }
    pub fn name(&self) -> String {
        self.profile().name
    }
    pub fn settings(&self) -> AgentSettings {
        self.settings.read().map(|s| s.clone()).unwrap_or_default()
    }
    pub(crate) fn set_profile(&self, profile: AgentProfile) {
        if let Ok(mut p) = self.profile.write() {
            *p = profile;
        }
    }
    pub(crate) fn set_settings(&self, settings: AgentSettings) {
        if let Ok(mut s) = self.settings.write() {
            *s = settings;
        }
    }
    /// The address of this agent.
    pub fn address(&self) -> AgentAddress {
        let profile = self.profile();
        AgentAddress { id: self.id.clone(), name: profile.name, description: profile.description }
    }
    /// Lane of the turn currently executing, if any.
    pub fn active_lane(&self) -> Option<Lane> {
        self.active_lane.lock().ok().and_then(|l| *l)
    }
    pub(crate) fn set_active_lane(&self, lane: Option<Lane>) {
        if let Ok(mut l) = self.active_lane.lock() {
            *l = lane;
        }
    }

    /// Bump and return the send epoch (`nextTurnEpoch`).
    pub(crate) fn next_turn_epoch(&self) -> u64 {
        self.turn_epoch.fetch_add(1, Ordering::SeqCst) + 1
    }
    pub(crate) fn current_turn_epoch(&self) -> u64 {
        self.turn_epoch.load(Ordering::SeqCst)
    }

    /// Queue a turn and wait for its result.
    pub async fn run(&self, job: RunJob) -> Result<RunResult, HostError> {
        let (reply, rx) = oneshot::channel();
        self.mailbox.send(AgentCommand::Run { job: Box::new(job), reply }).map_err(|_| HostError::ShuttingDown)?;
        rx.await.map_err(|_| HostError::ShuttingDown)
    }

    /// Queue a turn without waiting.
    pub fn enqueue(&self, job: RunJob) -> Result<(), HostError> {
        let (reply, _rx) = oneshot::channel();
        self.mailbox.send(AgentCommand::Run { job: Box::new(job), reply }).map_err(|_| HostError::ShuttingDown)
    }

    /// Queue a turn and get a receiver for its result (non-blocking send).
    pub fn enqueue_with_result(&self, job: RunJob) -> Result<oneshot::Receiver<RunResult>, HostError> {
        let (reply, rx) = oneshot::channel();
        self.mailbox.send(AgentCommand::Run { job: Box::new(job), reply }).map_err(|_| HostError::ShuttingDown)?;
        Ok(rx)
    }

    /// Cancel the current turn if its lane is below `only_if_lane_below`.
    pub fn interrupt(&self, reason: impl Into<String>, only_if_lane_below: Lane) {
        let _ = self.mailbox.send(AgentCommand::Interrupt { reason: reason.into(), only_if_lane_below });
    }

    pub(crate) fn shutdown(&self) {
        let _ = self.mailbox.send(AgentCommand::Shutdown);
    }

    /// Record a conversation partner in KV.
    pub(crate) fn add_conversation_partner(&self, other: &AgentId) {
        let mut partners: Vec<AgentId> = self.db.get_json(kv_keys::CONVERSATION_PARTNERS).ok().flatten().unwrap_or_default();
        if !partners.contains(other) {
            partners.push(other.clone());
            partners.sort();
            let _ = self.db.set_json(kv_keys::CONVERSATION_PARTNERS, &partners);
        }
    }
}

/// Which address family an entry id belongs to (`transcript-entry-ids.ts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryIdKind {
    /// `t<n>u`
    UserMessage,
    /// `t<turn>a<k>`
    AssistantMessage,
    /// `t<turn>s<k>`
    SendMessage,
    /// Hidden bookkeeping entries (tool calls, monologue, dividers): a UUID.
    Internal,
}

fn is_user_message(entry: &TranscriptEntry) -> bool {
    matches!(entry, TranscriptEntry::Message { role: Role::User, .. })
}

fn count_trailing(entries: &[TranscriptEntry], pred: impl Fn(&TranscriptEntry) -> bool) -> usize {
    let mut count = 0;
    for entry in entries.iter().rev() {
        if is_user_message(entry) {
            break;
        }
        if pred(entry) {
            count += 1;
        }
    }
    count
}

/// Mint the next entry id the way the original host does: user messages
/// are `t<n>u` (n = number of user messages so far), assistant messages and
/// sends are numbered within the current turn (`t<turn>a<k>` / `t<turn>s<k>`,
/// turn `b` before the first user message).
pub fn next_entry_id(entries: &[TranscriptEntry], kind: EntryIdKind) -> EntryId {
    if kind == EntryIdKind::Internal {
        return EntryId::new();
    }
    let ids: std::collections::HashSet<&str> = entries.iter().map(|e| e.id().as_str()).collect();
    let users = entries.iter().filter(|e| is_user_message(e)).count();
    let turn = if users == 0 { "b".to_owned() } else { (users - 1).to_string() };
    let (mint, start): (Box<dyn Fn(usize) -> String>, usize) = match kind {
        EntryIdKind::UserMessage => (Box::new(|n| format!("t{n}u")), users),
        EntryIdKind::AssistantMessage => (
            Box::new(move |n| format!("t{turn}a{n}")),
            count_trailing(entries, |e| matches!(e, TranscriptEntry::Message { role: Role::Assistant, .. })),
        ),
        EntryIdKind::SendMessage => {
            (Box::new(move |n| format!("t{turn}s{n}")), count_trailing(entries, |e| matches!(e, TranscriptEntry::SendMessage { .. })))
        }
        EntryIdKind::Internal => unreachable!(),
    };
    let mut index = start;
    let mut id = mint(index);
    while ids.contains(id.as_str()) {
        index += 1;
        id = mint(index);
    }
    EntryId::from(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: &str) -> TranscriptEntry {
        TranscriptEntry::Message {
            artifacts: Vec::new(),
            id: EntryId::from(id),
            role: Role::User,
            content: "x".into(),
            timestamp_ms: 0,
            hidden: false,
            run_id: None,
            from_agent: None,
            to_agent: None,
            images: vec![],
            group_id: None,
            priority: false,
            user_name: None,
            reply_to: None,
        }
    }

    #[test]
    fn mints_original_addresses() {
        let mut entries = Vec::new();
        assert_eq!(next_entry_id(&entries, EntryIdKind::UserMessage).as_str(), "t0u");
        assert_eq!(next_entry_id(&entries, EntryIdKind::SendMessage).as_str(), "tbs0");
        entries.push(user("t0u"));
        assert_eq!(next_entry_id(&entries, EntryIdKind::UserMessage).as_str(), "t1u");
        assert_eq!(next_entry_id(&entries, EntryIdKind::SendMessage).as_str(), "t0s0");
        entries.push(TranscriptEntry::SendMessage {
            id: EntryId::from("t0s0"),
            message: OutboundMessage::text("hi"),
            timestamp_ms: 0,
            run_id: RunId::new(),
            group_id: None,
            author: None,
            reply_to: None,
            reactions: vec![],
            responded_value: None,
            widget_skipped: false,
            widget_dismissed: false,
        });
        assert_eq!(next_entry_id(&entries, EntryIdKind::SendMessage).as_str(), "t0s1");
        entries.push(user("t1u"));
        assert_eq!(next_entry_id(&entries, EntryIdKind::SendMessage).as_str(), "t1s0");
        assert_eq!(next_entry_id(&entries, EntryIdKind::AssistantMessage).as_str(), "t1a0");
    }
}
