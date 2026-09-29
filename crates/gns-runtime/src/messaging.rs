//! Cross-agent messaging (`AgentToAgentMessaging`): validation, the
//! in-memory per-recipient inbound queue, priority steering and the wake loop.

use crate::agent::{EntryIdKind, RunJob};
use crate::host::HostInner;
use gns_core::messaging::*;
use gns_core::text::now_ms;
use gns_core::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

/// Queue state shared by the host (in memory, like the original's `pendingAgentInbound`).
#[derive(Debug, Default)]
pub struct AgentToAgentMessaging {
    pending: Mutex<HashMap<AgentId, Vec<InboundMessage>>>,
    reviving: Mutex<HashSet<AgentId>>,
}

impl AgentToAgentMessaging {
    /// Number of queued messages for an agent.
    pub fn pending_count(&self, agent: &AgentId) -> usize {
        self.pending.lock().map(|p| p.get(agent).map(|v| v.len()).unwrap_or(0)).unwrap_or(0)
    }

    fn take_pending(&self, agent: &AgentId) -> Vec<InboundMessage> {
        self.pending.lock().ok().and_then(|mut p| p.remove(agent)).unwrap_or_default()
    }

    fn has_pending_priority(&self, agent: &AgentId) -> bool {
        self.pending.lock().map(|p| p.get(agent).is_some_and(|v| v.iter().any(|m| m.priority))).unwrap_or(false)
    }

    fn requeue(&self, agent: &AgentId, deferred: Vec<InboundMessage>) {
        if let Ok(mut p) = self.pending.lock() {
            let queued = p.remove(agent).unwrap_or_default();
            p.insert(agent.clone(), merge_inbound_queue(queued, deferred));
        }
    }

    /// Drop everything queued for a deleted agent.
    pub(crate) fn forget(&self, agent: &AgentId) {
        if let Ok(mut p) = self.pending.lock() {
            p.remove(agent);
        }
        if let Ok(mut r) = self.reviving.lock() {
            r.remove(agent);
        }
    }

    /// Validate and enqueue a 1:1 message, or post to a group. Returns the
    /// acknowledgement text for the sender's model (`sendToAgent`).
    pub async fn send_to_agent(
        &self,
        host: &Arc<HostInner>,
        from: &AgentId,
        target_id: &str,
        text: &str,
        images: Vec<ImageRef>,
        priority: bool,
    ) -> Result<String, HostError> {
        self.send_with_artifacts(host, from, target_id, text, images, Vec::new(), priority).await
    }

    // Keep the existing send_to_agent arguments together and add structured attachments.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_with_artifacts(
        &self,
        host: &Arc<HostInner>,
        from: &AgentId,
        target_id: &str,
        text: &str,
        images: Vec<ImageRef>,
        artifacts: Vec<ArtifactRef>,
        priority: bool,
    ) -> Result<String, HostError> {
        let message = clamp_agent_message(text);
        if message.is_empty() {
            return Ok("Message was empty; nothing was sent.".to_owned());
        }
        if target_id == from.as_str() {
            return Ok("An agent can't message itself.".to_owned());
        }
        if host.is_agent_gone(target_id) {
            return Ok("That agent no longer exists.".to_owned());
        }
        if let Some(group) = host.group(&GroupId::from(target_id)) {
            let ack = crate::groups::post_to_group_with_artifacts(host, from, &group, &message, artifacts).await?;
            let mut notes: Vec<String> = Vec::new();
            if !images.is_empty() {
                notes.push(format!(
                    "Note: the attached image{} NOT delivered — group messages are text-only for now; send images to an agent directly.",
                    if images.len() == 1 { " was" } else { "s were" }
                ));
            }
            if priority {
                notes.push("Note: priority is 1:1 only — this post did not interrupt members.".to_owned());
            }
            return Ok(if notes.is_empty() { ack } else { format!("{ack} {}", notes.join(" ")) });
        }
        let Some(target) = host.agent(&AgentId::from(target_id)) else {
            return Ok(format!("No agent found with id {target_id}."));
        };
        host.coordination.grant(&artifacts, &ConversationRef::Agent(target.id.clone()))?;
        let sender = host.agent(from);
        let sender_name = sender.as_ref().map(|s| s.name()).unwrap_or_else(|| "An agent".to_owned());
        let target_name = target.name();
        if let Some(sender) = &sender {
            sender.add_conversation_partner(&target.id);
            let entry = TranscriptEntry::Message {
                artifacts: artifacts.clone(),
                id: sender.next_entry_id(EntryIdKind::AssistantMessage),
                role: Role::Assistant,
                content: message.clone(),
                timestamp_ms: now_ms(),
                hidden: false,
                run_id: None,
                from_agent: None,
                to_agent: Some(agent_ref(&target.id, &target_name)),
                images: images.clone(),
                group_id: None,
                priority,
                user_name: None,
                reply_to: None,
            };
            sender.append(&entry)?;
            host.emit(HostEvent::EntryAppended { agent_id: sender.id.clone(), entry });
        }
        let inbound = InboundMessage {
            artifacts: artifacts.clone(),
            from: agent_ref(from, &sender_name),
            text: message.clone(),
            timestamp_ms: now_ms(),
            images,
            priority,
            is_displayed: false,
            is_redriven: false,
        };
        {
            let mut pending = self.pending.lock().map_err(|_| HostError::Other("messaging lock poisoned".into()))?;
            let queue = pending.entry(target.id.clone()).or_default();
            if priority {
                queue.insert(0, inbound);
            } else {
                queue.push(inbound);
            }
        }
        host.emit(HostEvent::A2ASent { from: from.clone(), to: target.id.to_string(), priority, text: message });
        host.emit(HostEvent::A2AQueued { to: target.id.clone(), pending: self.pending_count(&target.id) });
        if priority {
            // `steerRecipientForPriorityPeer`: interrupt the recipient's
            // current non-user work (never a user turn).
            target.interrupt("superseded by a priority agent message", Lane::User);
        }
        self.revive(host.clone(), target.id.clone());
        Ok(send_ack(&target_name, priority))
    }

    /// Drain a recipient's queue on a background task (idempotent per agent).
    pub fn revive(&self, host: Arc<HostInner>, agent_id: AgentId) {
        {
            let Ok(mut reviving) = self.reviving.lock() else { return };
            if !reviving.insert(agent_id.clone()) {
                return;
            }
        }
        tokio::spawn(async move {
            loop {
                let messages = prioritize_inbound(host.messaging.take_pending(&agent_id));
                if messages.is_empty() {
                    break;
                }
                host.messaging.run_inbound_wake(&host, &agent_id, messages).await;
            }
            if let Ok(mut reviving) = host.messaging.reviving.lock() {
                reviving.remove(&agent_id);
            }
            // A message may have landed between the last take and the removal.
            if host.messaging.pending_count(&agent_id) > 0 {
                host.messaging.revive(host.clone(), agent_id);
            }
        });
    }

    /// `runAgentInboundWake`: append every message of the batch up front,
    /// then run one hidden, silence-allowed wake per message on the agent
    /// lane. A priority message arriving mid-batch defers the rest; an
    /// interrupted wake is redelivered only when a direct message (or a
    /// priority peer) preempted it.
    async fn run_inbound_wake(&self, host: &Arc<HostInner>, agent_id: &AgentId, messages: Vec<InboundMessage>) {
        let Some(handle) = host.agent(agent_id) else { return };
        for message in messages.iter().filter(|m| !m.is_displayed) {
            handle.add_conversation_partner(&message.from.id);
            let entry = TranscriptEntry::Message {
                artifacts: message.artifacts.clone(),
                id: handle.next_entry_id(EntryIdKind::UserMessage),
                role: Role::User,
                content: message.text.clone(),
                timestamp_ms: message.timestamp_ms,
                hidden: false,
                run_id: None,
                from_agent: Some(message.from.clone()),
                to_agent: None,
                images: message.images.clone(),
                group_id: None,
                priority: message.priority,
                user_name: None,
                reply_to: None,
            };
            if let Err(e) = handle.append(&entry) {
                host.emit(HostEvent::Error {
                    agent_id: Some(agent_id.clone()),
                    run_id: None,
                    message: format!("failed to record inbound message: {e}"),
                });
                return;
            }
            host.emit(HostEvent::EntryAppended { agent_id: agent_id.clone(), entry });
        }
        handle.dm_preempted_wake.store(false, Ordering::SeqCst);
        for (index, message) in messages.iter().enumerate() {
            if index > 0 && self.has_pending_priority(agent_id) {
                let deferred = messages[index..].iter().map(|m| InboundMessage { is_displayed: true, ..m.clone() }).collect();
                self.requeue(agent_id, deferred);
                return;
            }
            // The wake prompt enters the model context as a hidden entry when
            // its run starts (the inbound entry above is display-only).
            let job = RunJob {
                prompt: build_agent_inbound_wake_prompt(message),
                options: RunOptions::agent_wake(),
                images: message.images.clone(),
                already_persisted: false,
                persist: true,
                context_notes: Vec::new(),
                message_id: None,
                epoch: 0,
                carries_recovery: false,
                reply_context: None,
            };
            match handle.run(job).await {
                Ok(result) if result.aborted => {
                    let preempted = handle.dm_preempted_wake.swap(false, Ordering::SeqCst);
                    if host.shutdown_token().is_cancelled() || !preempted || host.is_agent_gone(agent_id.as_str()) {
                        return;
                    }
                    let redrivable: Vec<InboundMessage> = messages[index..]
                        .iter()
                        .filter(|m| !m.is_redriven)
                        .map(|m| InboundMessage { is_displayed: true, is_redriven: true, ..m.clone() })
                        .collect();
                    if !redrivable.is_empty() {
                        self.requeue(agent_id, redrivable);
                    }
                    return;
                }
                Ok(_) => {
                    handle.dm_preempted_wake.store(false, Ordering::SeqCst);
                }
                Err(e) => {
                    host.emit(HostEvent::Error {
                        agent_id: Some(agent_id.clone()),
                        run_id: None,
                        message: format!("message from another agent failed: {e}"),
                    });
                    return;
                }
            }
        }
    }
}
