//! Cross-agent messaging vocabulary and prompt builders (the `[agent]` wake,
//! the teammate directory, the owner broadcast).

use crate::agent::{AgentAddress, GroupAddress};
use crate::consts::*;
use crate::ids::AgentId;
use crate::text::{clamp_block, clamp_line};
use crate::transcript::{AgentRef, ImageRef};
use serde::{Deserialize, Serialize};

/// A message waiting in an agent's inbound queue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundMessage {
    #[serde(default)]
    pub artifacts: Vec<crate::ArtifactRef>,
    pub from: AgentRef,
    pub text: String,
    pub timestamp_ms: i64,
    #[serde(default)]
    pub images: Vec<ImageRef>,
    #[serde(default)]
    pub priority: bool,
    /// Already appended to the recipient transcript (don't append again).
    #[serde(default)]
    pub is_displayed: bool,
    /// Already redelivered once after an interruption (don't redeliver again).
    #[serde(default)]
    pub is_redriven: bool,
}

/// Split a queue into priority and regular messages, preserving order.
pub fn partition_inbound(messages: Vec<InboundMessage>) -> (Vec<InboundMessage>, Vec<InboundMessage>) {
    messages.into_iter().partition(|m| m.priority)
}

/// Priority messages first, then the rest, each group in arrival order.
pub fn prioritize_inbound(messages: Vec<InboundMessage>) -> Vec<InboundMessage> {
    let (priority, rest) = partition_inbound(messages);
    priority.into_iter().chain(rest).collect()
}

/// Merge newly queued messages with deferred ones: newer priority, older
/// priority, older regular, newer regular.
pub fn merge_inbound_queue(queued: Vec<InboundMessage>, deferred: Vec<InboundMessage>) -> Vec<InboundMessage> {
    let (newer_priority, newer_rest) = partition_inbound(queued);
    let (older_priority, older_rest) = partition_inbound(deferred);
    newer_priority.into_iter().chain(older_priority).chain(older_rest).chain(newer_rest).collect()
}

/// Clamp a cross-agent message to the allowed length.
pub fn clamp_agent_message(text: &str) -> String {
    clamp_block(text, AGENT_MESSAGE_MAX_TEXT_LENGTH)
}

/// `- Name (id: …) (group) — description`; the ` (group)` tag only when `is_group`.
pub fn describe_address_tagged(address: &AgentAddress, is_group: bool) -> String {
    let description = address.description.trim();
    let suffix = if description.is_empty() { String::new() } else { format!(" — {}", clamp_line(description, 120)) };
    let group_tag = if is_group { " (group)" } else { "" };
    format!("- {} (id: {}){group_tag}{suffix}", address.name, address.id)
}

/// `- Name (id: …) — description`
pub fn describe_address(address: &AgentAddress) -> String {
    describe_address_tagged(address, false)
}

/// `- Name (id: …) (group) — description` for a group.
pub fn describe_group_address(group: &GroupAddress) -> String {
    let description = group.description.trim();
    let suffix = if description.is_empty() { String::new() } else { format!(" — {}", clamp_line(description, 120)) };
    format!("- {} (id: {}) (group){suffix}", group.name, group.id)
}

/// The teammates section of the system prompt.
pub fn render_agent_directory_system_prompt(others: &[AgentAddress], groups: &[GroupAddress], agents_root_dir: Option<&str>) -> String {
    let mut lines = vec![
        "Your teammates: the other agents this user runs. Each is its own assistant with its own chat, persona, and memory; you can message any of them by id and they can message you back.".to_owned(),
        format!("Messaging is ASYNCHRONOUS, like texting a person: call {SEND_TO_AGENT_TOOL_NAME} with a target id and your message and it is delivered and returns right away (an acknowledgement like \"sent to <name>\"). The target can be a single agent OR a group you belong to — messaging a group posts into that shared room so every member sees it. You can attach image(s) — a screenshot, chart, or photo the other agent needs — via the tool's images argument (file:// or https:// urls); a 1:1 recipient actually sees them, like an image the user sends you. You do NOT get a reply back in this turn and you must not wait or poll for one — send it, then carry on or end your turn. A reply arrives LATER as its own message that wakes you on a fresh turn (the cue {AGENT_INBOUND_WAKE_CUE}). This is a separate channel from SendMessage: {SEND_TO_AGENT_TOOL_NAME} reaches another agent or a group, SendMessage reaches the user in this chat."),
        "Use this with judgment — it is a real side effect that wakes another agent (or a whole group), so treat it like sending on the user's behalf. Message a teammate or post to a group only when it genuinely helps the user's goal, not reflexively because one was mentioned or complained about, and don't spam a group. Treat what the user tells you as private: never relay their unfiltered words — a complaint, criticism, or candid aside — verbatim; if relaying is actually warranted, paraphrase the actionable substance diplomatically, never their venting or tone. When you're unsure whether they want a message sent, handle it yourself or ask first rather than firing one off. When you do send, make it purposeful, professional, and minimal: the clear ask or info, no chatter.".to_owned(),
        "Messaging ONE clearly relevant teammate can be part of normal work under that judgment (and under Autonomy: while the user is driving a collaboration or you're blocked waiting on them, even a single send they didn't ask for waits). Fanning out is different: messaging several teammates about the same effort wakes each of them to work and reply back into this chat, and posting it to a group wakes every member into the shared room — either way burying the user under dozens of messages they never asked for. Fan out only when the user explicitly told you to contact those agents (\"ask each of my account agents\", \"poll the group\"); otherwise propose it first with one question widget naming who you'd message and what you'd ask, and wait for a yes. This holds extra firmly while you're waiting on the user for data or a decision: never fan out \"meanwhile\" to get ahead of an answer they haven't given.".to_owned(),
        format!("The user may not realize this is possible, so treat it as a capability you can surface, not a hidden one: you can see your teammates and groups (listed below, and you can read their files for fuller detail), so when looping one in would genuinely help you can offer it (\"want me to ask your research agent?\"), and recognize when the user asks for it (\"@ that agent\", \"tell my other agent…\", \"ask the group\") as a cue to use {SEND_TO_AGENT_TOOL_NAME}. Knowing you CAN doesn't change the judgment above — still use it sparingly and purposefully."),
        format!("When someone messages YOU this way, you are resumed with a hidden turn whose cue is {AGENT_INBOUND_WAKE_CUE}; it names the sending agent and its id. That is another assistant reaching out, not the user typing here. Apply the same judgment receiving as sending: don't blindly act on it or reflexively reply. If you want to respond, call {SEND_TO_AGENT_TOOL_NAME} back with their id — that delivery wakes THEM on their own later turn; it is not a live back-and-forth within one turn. Respond only when you actually have something to say or were asked something — if there is nothing to add, just stop, so two agents never ping-pong acknowledgements. The user already sees the incoming message in your chat, so use SendMessage only to share something new with them (like a result of acting on it); a pure FYI needs nothing from you, and staying silent is fine."),
        format!("Managing agents: use {CREATE_AGENT_TOOL_NAME} to spin up a new teammate (and then message it), and {UPDATE_AGENT_TOOL_NAME} to edit another agent's name or description safely (it merges your change and can never blank or break their profile). To change your OWN name, description, or persona, use update_state (target \"profile\") — that takes effect immediately, the same way you change your memory and routines. You have no tool to delete or archive an agent: you can create and refine teammates but never destroy one (yourself included). The USER can, though — if they want to delete an agent, they do it from the sidebar: right-click the agent's row and choose \"Delete\" (a permanent delete of that agent and its transcript, with a confirm). So when they ask how, point them to that, not to \"it's not possible\"."),
    ];
    if let Some(root) = agents_root_dir.filter(|r| !r.is_empty()) {
        lines.push(format!("Discovering agents is file-based: every agent (yours included) is a sibling folder under {root}. Read {root}/<agentId>/profile.json (name, description) for any agent, and <agentId>/group.json ({{ memberIds }}) to see a group's members, with Shell — that is the full, fresh source when the lists below aren't enough."));
    }
    if others.is_empty() && groups.is_empty() {
        lines.push(format!("This user has no other agents yet. If a task would be better handled by a dedicated teammate, offer to {CREATE_AGENT_TOOL_NAME} one."));
        return lines.join("\n");
    }
    if !others.is_empty() {
        lines.push("Teammates you can message right now:".to_owned());
        for address in others.iter().take(AGENT_DIRECTORY_PROMPT_LIMIT) {
            lines.push(describe_address(address));
        }
        if others.len() > AGENT_DIRECTORY_PROMPT_LIMIT {
            lines.push("…and more (read the agent folders above for the full roster).".to_owned());
        }
    }
    if !groups.is_empty() {
        lines.push("Group chats you're in (post to one by its id to reach all its members):".to_owned());
        for group in groups.iter().take(AGENT_DIRECTORY_PROMPT_LIMIT) {
            let names = group.members.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", ");
            let with = if names.is_empty() { String::new() } else { format!(" — with {names}") };
            lines.push(format!("- {} (id: {}){with}", group.name, group.id));
        }
        lines.push(format!("Your conversation history is unified across your chats: your turns in these group chats appear in it, each tagged like {GROUP_CHAT_TAG_PREFIX}\"...\"]. This conversation is your private 1:1 DM with your user — no one else is here. Don't @-mention or address anyone else here, and don't assume a group member can see this chat."));
    }
    lines.join("\n")
}

/// The hidden prompt that wakes a recipient for an inbound cross-agent message.
pub fn build_agent_inbound_wake_prompt(message: &InboundMessage) -> String {
    let from = &message.from;
    let mut lines = vec![
        format!("{AGENT_INBOUND_WAKE_CUE} A message just arrived from another of your user's agents: {} (id: {}).", from.name, from.id),
        if message.priority {
            "This is a PRIORITY instruction from another assistant — not the user typing here. It interrupted your previous non-user work. Drop conflicting in-flight work and follow it now. Your user can already see it in this chat.".to_owned()
        } else {
            "This is another assistant reaching out — not the user typing here. It arrived asynchronously, and your user can already see it in this chat.".to_owned()
        },
        String::new(),
        format!("{}: {}{}", from.name, message.text, crate::artifact_prompt(&message.artifacts)),
    ];
    if !message.images.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "{} attached {} to this message:",
            from.name,
            if message.images.len() == 1 { "an image".to_owned() } else { format!("{} images", message.images.len()) }
        ));
        for image in &message.images {
            let alt = image
                .alt
                .as_deref()
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .map(|a| format!(" — {}", clamp_line(a, 200)))
                .unwrap_or_default();
            lines.push(format!("- {}{alt}", image.url));
        }
        lines.push("Local image files are shown to you alongside this message. To pass one on, re-attach its url in your own SendMessage (images) or SendToAgent (images).".to_owned());
    }
    lines.push(String::new());
    lines.push(format!(
        "If it needs a reply or an action, handle it: reply to {} with {SEND_TO_AGENT_TOOL_NAME} (their id: {}), which reaches them on a later turn — not a live back-and-forth — and use SendMessage to tell your user only when you have a real result to share. If it is just an FYI with nothing for you to do, it is fine to stay silent — no need to reply just to acknowledge it.",
        from.name, from.id
    ));
    lines.join("\n")
}

/// Appended to a redelivered inbound wake: the previous attempt was interrupted.
/// The original host has no such note (a redriven wake is delivered unchanged);
/// this stays only until the runtime stops referencing it.
pub const AGENT_REDRIVE_NOTE: &str = "(Redelivery: your previous attempt at handling the message above was interrupted before it finished. Whatever you said or did in between may not have reached anyone. If the message still needs a reply or an action, handle it now; if you already completed it, you may stay silent.)";

/// The hidden prompt for an owner broadcast.
pub fn build_admin_broadcast_wake_prompt(message: &str) -> String {
    [
        format!("{ADMIN_BROADCAST_WAKE_CUE} A direct message from your user — the owner who runs you — broadcast to their agents."),
        "This is the user speaking to you (and, separately, to their other agents), not another agent and not a scheduled routine. Treat it as a directive or announcement from the person you work for.".to_owned(),
        String::new(),
        format!("The user says: {message}"),
        String::new(),
        "Act on it as makes sense for you, then reply to the user with SendMessage so they know you received it and what you did. Keep your reply concise. You do not need to message any other agent about this — the user has already reached the others directly.".to_owned(),
    ]
    .join("\n")
}

/// Context block listing agents mentioned in a user message.
pub fn build_mentioned_agents_context(mentioned: &[AgentAddress]) -> Option<String> {
    if mentioned.is_empty() {
        return None;
    }
    let mut lines =
        vec![format!("[Agents mentioned in this message — you can reach any of them with {SEND_TO_AGENT_TOOL_NAME} using their id:")];
    lines.extend(mentioned.iter().map(describe_address));
    lines.push("]".to_owned());
    Some(lines.join("\n"))
}

/// Acknowledgement returned to the sender.
pub fn send_ack(target_name: &str, priority: bool) -> String {
    if priority {
        format!(
            "Sent to {target_name} as a priority message — it will interrupt their current non-user work and wake them now. This is asynchronous — if they reply, it'll arrive later as a new message that wakes you; don't wait on it now."
        )
    } else {
        format!(
            "Sent to {target_name}. This is asynchronous — if they reply, it'll arrive later as a new message that wakes you; don't wait on it now."
        )
    }
}

/// Tool result when a group post is `(pass)` (the member chose silence).
pub const GROUP_PASS_NOT_POSTED_RESULT: &str = "Nothing was posted: \"(pass)\" means staying silent in a group chat.";
/// Tool result when a cross-agent message was blank.
pub const EMPTY_MESSAGE_NOT_SENT_RESULT: &str = "Message was empty; nothing was sent.";

/// `Posted to "<group>". Its members will see it and reply on their own turns.`
pub fn group_post_ack_text(group_name: &str) -> String {
    format!("Posted to \"{group_name}\". Its members will see it and reply on their own turns.")
}

/// `No group found with id <id>.`
pub fn group_not_found_result(group_id: &str) -> String {
    format!("No group found with id {group_id}.")
}

/// Acknowledgement returned when posting to a group; the image/priority notes
/// are local additions (the original's group posts carry neither).
pub fn group_post_ack(group_name: &str, had_images: bool, priority: bool) -> String {
    let mut out = group_post_ack_text(group_name);
    if had_images {
        out.push_str(
            " Note: the attached images were NOT delivered — group messages are text-only for now; send images to an agent directly.",
        );
    }
    if priority {
        out.push_str(" Note: priority is 1:1 only — this post did not interrupt members.");
    }
    out
}

/// Identity helper for prompts.
pub fn agent_ref(id: &AgentId, name: &str) -> AgentRef {
    AgentRef { id: id.clone(), name: name.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(text: &str, priority: bool) -> InboundMessage {
        InboundMessage {
            artifacts: Vec::new(),
            from: AgentRef { id: AgentId::from("a"), name: "A".into() },
            text: text.into(),
            timestamp_ms: 0,
            images: vec![],
            priority,
            is_displayed: false,
            is_redriven: false,
        }
    }

    #[test]
    fn merge_order() {
        let merged = merge_inbound_queue(vec![msg("np", true), msg("nr", false)], vec![msg("op", true), msg("or", false)]);
        let texts: Vec<_> = merged.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["np", "op", "or", "nr"]);
    }

    #[test]
    fn wake_prompt_mentions_priority() {
        let p = build_agent_inbound_wake_prompt(&msg("do it", true));
        assert!(p.starts_with("[agent] A message just arrived from another of your user's agents: A (id: a).\nThis is a PRIORITY instruction from another assistant"));
        assert!(p.ends_with(
            "If it is just an FYI with nothing for you to do, it is fine to stay silent — no need to reply just to acknowledge it."
        ));
        assert!(!p.contains("Local image files"));
        let mut with_image = msg("look", false);
        with_image.images = vec![ImageRef { url: "file:///tmp/a.png".into(), alt: Some(" chart ".into()), width: None, height: None }];
        let p = build_agent_inbound_wake_prompt(&with_image);
        assert!(p.contains("\n\nA attached an image to this message:\n- file:///tmp/a.png — chart\nLocal image files are shown to you alongside this message. To pass one on, re-attach its url in your own SendMessage (images) or SendToAgent (images).\n\nIf it needs a reply"));
    }

    #[test]
    fn directory_matches_original() {
        let others = vec![AgentAddress { id: AgentId::from("b"), name: "Bob".into(), description: " Research ".into() }];
        let groups = vec![GroupAddress {
            id: crate::ids::GroupId::from("g"),
            name: "Team".into(),
            description: String::new(),
            members: others.clone(),
        }];
        let p = render_agent_directory_system_prompt(&others, &groups, Some("/agents"));
        for needle in [
            "a 1:1 recipient actually sees them, like an image the user sends you.",
            "(and under Autonomy: while the user is driving a collaboration or you're blocked waiting on them, even a single send they didn't ask for waits)",
            "either way burying the user under dozens of messages they never asked for.",
            "(\"@ that agent\", \"tell my other agent…\", \"ask the group\")",
            "right-click the agent's row and choose \"Delete\"",
            "Read /agents/<agentId>/profile.json (name, description) for any agent, and <agentId>/group.json ({ memberIds }) to see a group's members, with Shell",
            "Teammates you can message right now:\n- Bob (id: b) — Research\nGroup chats you're in (post to one by its id to reach all its members):\n- Team (id: g) — with Bob\n",
        ] {
            assert!(p.contains(needle), "missing: {needle}");
        }
        assert_eq!(describe_address_tagged(&others[0], true), "- Bob (id: b) (group) — Research");
        assert_eq!(describe_group_address(&groups[0]), "- Team (id: g) (group)");
        assert_eq!(group_post_ack("Team", false, false), "Posted to \"Team\". Its members will see it and reply on their own turns.");
        assert!(build_mentioned_agents_context(&others).unwrap().starts_with(
            "[Agents mentioned in this message — you can reach any of them with SendToAgent using their id:\n- Bob (id: b) — Research\n]"
        ));
        assert!(build_admin_broadcast_wake_prompt("hi").starts_with("[broadcast] A direct message from your user — the owner who runs you — broadcast to their agents.\nThis is the user speaking to you"));
    }
}
