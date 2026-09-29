//! Group chat: room state, posting and the bounded round-robin orchestrator
//! (`GroupChatOrchestrator` + `GroupChatGlue.runGroupMemberTurn`).
//!
//! Every post opens a new room epoch; the round robin asks the responders
//! (`resolveResponders`) in rotated order, at most `GROUP_MAX_ROUNDS` rounds,
//! `GROUP_MAX_MEMBER_TURNS` posts per room turn and
//! `GROUP_MAX_MESSAGES_PER_TURN` per member turn. A member turn preempted by
//! a direct message is retried (with a redrive note) up to three times; a
//! member turn that fails is a pass, not a room-wide failure. Room history
//! lives only in the room's `store.db`.

use crate::agent::{AgentHandle, RunJob};
use crate::host::HostInner;
use gns_core::groups::*;
use gns_core::messaging::clamp_agent_message;
use gns_core::text::now_ms;
use gns_core::*;
use gns_store::AgentDb;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// Runtime state of one group.
pub struct GroupHandle {
    pub id: GroupId,
    config: RwLock<GroupConfig>,
    /// Room transcript (`store.db` in the group folder).
    pub db: Arc<AgentDb>,
    /// Bumped on every new post; a running orchestrator stops when it changes.
    epoch: AtomicU64,
    /// Rounds of one room never overlap (a stale round exits at its next epoch check).
    round_lock: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for GroupHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupHandle").field("id", &self.id).finish_non_exhaustive()
    }
}

impl GroupHandle {
    pub(crate) fn new(id: GroupId, config: GroupConfig, db: Arc<AgentDb>) -> Arc<Self> {
        Arc::new(Self { id, config: RwLock::new(config), db, epoch: AtomicU64::new(0), round_lock: tokio::sync::Mutex::new(()) })
    }
    pub fn config(&self) -> GroupConfig {
        self.config.read().map(|c| c.clone()).unwrap_or_default()
    }
    pub(crate) fn set_config(&self, config: GroupConfig) {
        if let Ok(mut c) = self.config.write() {
            *c = config;
        }
    }
    pub fn description(&self) -> GroupDescription {
        let config = self.config();
        GroupDescription { name: config.name, description: config.description }
    }
    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
    fn bump_epoch(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Room history in chronological order.
    pub fn history(&self) -> Vec<GroupMessage> {
        self.db
            .all()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::Message { role: Role::User, content, timestamp_ms, from_agent: None, user_name, artifacts, .. } => {
                    Some(GroupMessage { artifacts, task: None, speaker: Speaker::User { name: user_name }, content, timestamp_ms })
                }
                TranscriptEntry::SendMessage { message, timestamp_ms, author: Some(author), .. } => Some(GroupMessage {
                    artifacts: message.artifacts.clone(),
                    task: message.task.clone(),
                    speaker: Speaker::Member { id: author.id, name: author.name },
                    content: message.body_text(),
                    timestamp_ms,
                }),
                TranscriptEntry::Message { content, timestamp_ms, from_agent: Some(from), artifacts, .. } => Some(GroupMessage {
                    artifacts,
                    task: None,
                    speaker: Speaker::Member { id: from.id, name: from.name },
                    content,
                    timestamp_ms,
                }),
                _ => None,
            })
            .collect()
    }
}

/// Append a post to the room's transcript and emit the event.
pub(crate) fn post_to_room(host: &HostInner, group: &GroupHandle, speaker: Speaker, content: String) -> Result<(), HostError> {
    post_message_to_room(host, group, speaker, OutboundMessage::text(content))
}

pub(crate) fn post_message_to_room(
    host: &HostInner,
    group: &GroupHandle,
    speaker: Speaker,
    message: OutboundMessage,
) -> Result<(), HostError> {
    let content = message.display_text();
    let timestamp_ms = now_ms();
    let all = group.db.all().unwrap_or_default();
    let (entry, speaker_name) = match &speaker {
        Speaker::User { name } => (
            TranscriptEntry::Message {
                artifacts: Vec::new(),
                id: crate::agent::next_entry_id(&all, crate::agent::EntryIdKind::UserMessage),
                role: Role::User,
                content: content.clone(),
                timestamp_ms,
                hidden: false,
                run_id: None,
                from_agent: None,
                to_agent: None,
                images: vec![],
                group_id: Some(group.id.clone()),
                priority: false,
                user_name: name.clone(),
                reply_to: None,
            },
            name.clone().unwrap_or_else(|| "User".to_owned()),
        ),
        Speaker::Member { id, name } => (
            TranscriptEntry::SendMessage {
                id: crate::agent::next_entry_id(&all, crate::agent::EntryIdKind::SendMessage),
                message,
                timestamp_ms,
                run_id: RunId::new(),
                group_id: Some(group.id.clone()),
                author: Some(AgentRef { id: id.clone(), name: name.clone() }),
                reply_to: None,
                reactions: vec![],
                responded_value: None,
                widget_skipped: false,
                widget_dismissed: false,
            },
            name.clone(),
        ),
    };
    group.db.append_entry(&entry)?;
    host.emit(HostEvent::GroupPosted { group_id: group.id.clone(), speaker: speaker_name, content });
    Ok(())
}

/// Post as the user and start a room round on the user lane.
pub(crate) async fn post_as_user(host: &Arc<HostInner>, group: &Arc<GroupHandle>, content: String) -> Result<(), HostError> {
    post_to_room(host, group, Speaker::User { name: None }, content)?;
    start_round(host, group, Lane::User);
    Ok(())
}

/// `SharedRooms.postToGroup`: an agent posting into a room. Returns the
/// acknowledgement for the sender's model.
pub(crate) async fn post_to_group_with_artifacts(
    host: &Arc<HostInner>,
    from: &AgentId,
    group: &Arc<GroupHandle>,
    message: &str,
    artifacts: Vec<ArtifactRef>,
) -> Result<String, HostError> {
    let message = clamp_agent_message(message);
    if message.is_empty() {
        return Ok("Message was empty; nothing was sent.".to_owned());
    }
    if is_pass_content(&message) {
        return Ok("Nothing was posted: \"(pass)\" means staying silent in a group chat.".to_owned());
    }
    let config = group.config();
    if !config.member_ids.contains(from) {
        return Ok("You can only post to a group you're a member of.".to_owned());
    }
    host.coordination.grant(&artifacts, &ConversationRef::Group(group.id.clone()))?;
    let sender = host.agent(from);
    let sender_name = sender.as_ref().map(|s| s.name()).unwrap_or_else(|| "An agent".to_owned());
    if let Some(sender) = &sender {
        let entry = TranscriptEntry::Message {
            artifacts: artifacts.clone(),
            id: sender.next_entry_id(crate::agent::EntryIdKind::AssistantMessage),
            role: Role::Assistant,
            content: message.clone(),
            timestamp_ms: now_ms(),
            hidden: false,
            run_id: None,
            from_agent: None,
            to_agent: Some(AgentRef { id: AgentId::from(group.id.as_str()), name: group_display_name(&group.description()) }),
            images: vec![],
            group_id: Some(group.id.clone()),
            priority: false,
            user_name: None,
            reply_to: None,
        };
        sender.append(&entry)?;
        host.emit(HostEvent::EntryAppended { agent_id: sender.id.clone(), entry });
    }
    let mut output = OutboundMessage::text(message);
    output.artifacts = artifacts;
    post_message_to_room(host, group, Speaker::Member { id: from.clone(), name: sender_name }, output)?;
    start_round(host, group, Lane::Agent);
    Ok(format!("Posted to \"{}\". Its members will see it and reply on their own turns.", group_display_name(&group.description())))
}

fn start_round(host: &Arc<HostInner>, group: &Arc<GroupHandle>, lane: Lane) {
    let epoch = group.bump_epoch();
    let host = host.clone();
    let group = group.clone();
    tokio::spawn(async move {
        run_room_round(host, group, epoch, lane).await;
    });
}

/// Bounded round robin for one room turn (`GroupChatOrchestrator.run`).
async fn run_room_round(host: Arc<HostInner>, group: Arc<GroupHandle>, epoch: u64, lane: Lane) {
    let description = group.description();
    let members: Vec<GroupMember> = group
        .config()
        .member_ids
        .iter()
        .filter_map(|id| host.agent(id))
        .map(|h| {
            let profile = h.profile();
            GroupMember { id: h.id.clone(), name: profile.name, description: profile.description }
        })
        .collect();
    if members.is_empty() {
        return;
    }
    let is_current = || group.epoch() == epoch;
    let _serialized = group.round_lock.lock().await;
    if !is_current() {
        return; // a newer post started a fresher round while we waited
    }
    let mut total_messages = 0usize;

    for round in 0..GROUP_MAX_ROUNDS {
        if !is_current() {
            return;
        }
        let responders: Vec<AgentId> = resolve_responders(&members, &group.history()).into_iter().map(|m| m.id).collect();
        let mut messages_this_round = 0usize;
        for member_id in order_round_speakers(&responders, round) {
            if total_messages >= GROUP_MAX_MEMBER_TURNS || !is_current() {
                return;
            }
            let Some(member) = members.iter().find(|m| m.id == member_id) else { continue };
            let sent = run_one_turn(&host, &group, &description, member, &members, lane, &is_current).await;
            let mut hit_cap = false;
            for content in sent {
                if post_message_to_room(&host, &group, Speaker::Member { id: member.id.clone(), name: member.name.clone() }, content)
                    .is_err()
                {
                    return;
                }
                total_messages += 1;
                messages_this_round += 1;
                if total_messages >= GROUP_MAX_MEMBER_TURNS {
                    hit_cap = true;
                    break;
                }
            }
            if hit_cap {
                return;
            }
        }
        if messages_this_round == 0 {
            return;
        }
    }
}

/// `runOneTurn` + `runGroupMemberTurn`: up to three attempts while a direct
/// message keeps preempting the member; `(pass)` and empty replies are
/// dropped and at most `GROUP_MAX_MESSAGES_PER_TURN` are kept.
async fn run_one_turn(
    host: &Arc<HostInner>,
    group: &Arc<GroupHandle>,
    description: &GroupDescription,
    member: &GroupMember,
    members: &[GroupMember],
    lane: Lane,
    is_current: &impl Fn() -> bool,
) -> Vec<OutboundMessage> {
    let Some(handle) = host.agent(&member.id) else { return Vec::new() };
    let peers: Vec<GroupMember> = members.iter().filter(|m| m.id != member.id).cloned().collect();
    let history = group.history();
    let new_messages = messages_since_member_last_spoke(&history, &member.id);
    let system_prompt = build_group_member_system_prompt(member, description, &peers);
    let base_prompt = build_group_turn_prompt(member, description, &peers, new_messages, false);
    let mut sent: Vec<OutboundMessage> = Vec::new();
    for attempt in 1..=GROUP_MEMBER_MAX_ATTEMPTS {
        if attempt > 1 && !is_current() {
            break;
        }
        let prompt = if attempt == 1 { base_prompt.clone() } else { format!("{base_prompt}{}", build_group_redrive_note()) };
        handle.dm_preempted_group.store(false, Ordering::SeqCst);
        let job = RunJob {
            prompt,
            options: RunOptions::group_turn(group.id.clone(), system_prompt.clone(), lane),
            images: vec![],
            already_persisted: false,
            persist: true,
            context_notes: Vec::new(),
            message_id: None,
            epoch: 0,
            carries_recovery: false,
            reply_context: None,
        };
        let result = match handle.run(job).await {
            Ok(r) => r,
            Err(_) => break, // a failed member turn is a pass, not a room-wide failure
        };
        for content in result.room_messages {
            if is_pass_content(&content.display_text()) {
                continue;
            }
            let trimmed = content.display_text().trim().to_owned();
            if trimmed.is_empty() {
                continue;
            }
            sent.push(content);
            if sent.len() >= GROUP_MAX_MESSAGES_PER_TURN {
                break;
            }
        }
        let preempted = handle.dm_preempted_group.swap(false, Ordering::SeqCst);
        if !preempted || !sent.is_empty() || result.reacted || attempt >= GROUP_MEMBER_MAX_ATTEMPTS || !is_current() {
            break;
        }
    }
    sent
}

/// Whether `handle` is a member of any group (for directory rendering).
pub(crate) fn groups_of(host: &HostInner, handle: &AgentHandle) -> Vec<Arc<GroupHandle>> {
    host.all_groups().into_iter().filter(|g| g.config().member_ids.contains(&handle.id)).collect()
}
