//! Group chat rules: mention parsing, responder resolution, round ordering,
//! `(pass)` detection and the room prompts. Pure functions.

use crate::consts::*;
use crate::ids::AgentId;
use serde::{Deserialize, Serialize};

/// A member as the room sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMember {
    pub id: AgentId,
    pub name: String,
    pub description: String,
}

/// Room metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupDescription {
    pub name: String,
    pub description: String,
}

/// Who said something in the room.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Speaker {
    User { name: Option<String> },
    Member { id: AgentId, name: String },
}

/// One room message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMessage {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<crate::ArtifactRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<Box<crate::TaskRecord>>,
    pub speaker: Speaker,
    pub content: String,
    pub timestamp_ms: i64,
}

/// Rotate the speaker order by round so a different member opens each round.
pub fn order_round_speakers<T: Clone>(member_ids: &[T], round: usize) -> Vec<T> {
    if member_ids.is_empty() {
        return Vec::new();
    }
    let offset = round % member_ids.len();
    member_ids[offset..].iter().chain(member_ids[..offset].iter()).cloned().collect()
}

/// Handles a member can be @-mentioned by: full name, name without spaces, first word.
pub fn member_mention_handles(name: &str) -> Vec<String> {
    let lower = name.trim().to_lowercase();
    if lower.is_empty() {
        return Vec::new();
    }
    let mut handles = vec![lower.clone()];
    let no_space: String = lower.split_whitespace().collect();
    if !handles.contains(&no_space) {
        handles.push(no_space);
    }
    if let Some(first) = lower.split_whitespace().next()
        && !handles.iter().any(|h| h == first)
    {
        handles.push(first.to_owned());
    }
    handles
}

fn is_word_char(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_ascii_alphanumeric())
}

fn has_mention_at(lower: &str, handle: &str) -> bool {
    let needle = format!("@{handle}");
    let chars: Vec<char> = lower.chars().collect();
    let needle_chars: Vec<char> = needle.chars().collect();
    if needle_chars.len() > chars.len() {
        return false;
    }
    for start in 0..=chars.len() - needle_chars.len() {
        if chars[start..start + needle_chars.len()] == needle_chars[..] {
            let before = if start == 0 { None } else { Some(chars[start - 1]) };
            let after = chars.get(start + needle_chars.len()).copied();
            if !is_word_char(before) && !is_word_char(after) {
                return true;
            }
        }
    }
    false
}

/// Result of scanning a message for mentions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mentions {
    pub is_everyone: bool,
    pub member_ids: Vec<AgentId>,
}

/// Parse `@handle`, `@everyone` and `@all` mentions.
pub fn parse_group_mentions(text: &str, members: &[GroupMember]) -> Mentions {
    let lower = text.to_lowercase();
    let mut member_ids = Vec::new();
    for member in members {
        if member_mention_handles(&member.name).iter().any(|h| has_mention_at(&lower, h)) && !member_ids.contains(&member.id) {
            member_ids.push(member.id.clone());
        }
    }
    let is_everyone = has_mention_at(&lower, "everyone") || has_mention_at(&lower, "all");
    Mentions { is_everyone, member_ids }
}

/// Members who should speak this round: those mentioned since the last user
/// message, or everyone when nobody (or @everyone) was mentioned.
pub fn resolve_responders(members: &[GroupMember], history: &[GroupMessage]) -> Vec<GroupMember> {
    let start = history.iter().rposition(|m| matches!(m.speaker, Speaker::User { .. })).unwrap_or(0);
    let mut everyone = false;
    let mut mentioned: Vec<AgentId> = Vec::new();
    for message in &history[start..] {
        let targets = parse_group_mentions(&message.content, members);
        everyone |= targets.is_everyone;
        for id in targets.member_ids {
            if !mentioned.contains(&id) {
                mentioned.push(id);
            }
        }
    }
    if everyone || mentioned.is_empty() {
        members.to_vec()
    } else {
        members.iter().filter(|m| mentioned.contains(&m.id)).cloned().collect()
    }
}

/// `(pass)`, `pass`, `(pass).` and empty strings mean "nothing to add".
pub fn is_pass_content(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return true;
    }
    let inner = trimmed.strip_suffix('.').unwrap_or(trimmed).trim();
    let inner = inner.strip_prefix('(').unwrap_or(inner);
    let inner = inner.strip_suffix(')').unwrap_or(inner);
    inner.trim().eq_ignore_ascii_case("pass")
}

/// Note appended when a room turn is redelivered after a DM interruption.
pub fn build_group_redrive_note() -> &'static str {
    "\n(Redelivery: your previous attempt at this turn was interrupted by a direct message to you. The room has NOT seen any reply from you for the messages above — anything you said or did while handling that direct message stayed in that private chat. If you already did the work, send the result to this room with SendMessage now; otherwise take the turn normally.)"
}

/// Render one history line from `viewer`'s perspective.
pub fn format_group_line(message: &GroupMessage, viewer: &AgentId) -> String {
    let content = format!("{}{}", message.content, crate::artifact_prompt(&message.artifacts));
    match &message.speaker {
        Speaker::User { name: Some(name) } => format!("{name} (user): {}", content),
        Speaker::User { name: None } => format!("User: {}", content),
        Speaker::Member { id, name } => {
            let you = if id == viewer { " (you)" } else { "" };
            format!("{name}{you}: {}", content)
        }
    }
}

/// Render the last `limit` messages.
pub fn format_group_history(history: &[GroupMessage], viewer: &AgentId, limit: usize) -> String {
    let start = history.len().saturating_sub(limit);
    let recent = &history[start..];
    if recent.is_empty() {
        "(no messages yet)".to_owned()
    } else {
        recent.iter().map(|m| format_group_line(m, viewer)).collect::<Vec<_>>().join("\n")
    }
}

/// Display name with fallback.
pub fn group_display_name(group: &GroupDescription) -> String {
    let name = group.name.trim();
    if name.is_empty() { "the group".to_owned() } else { name.to_owned() }
}

/// `"name" — description`
pub fn describe_group(group: &GroupDescription) -> String {
    let name = group_display_name(group);
    let description = group.description.trim();
    if description.is_empty() { format!("\"{name}\"") } else { format!("\"{name}\" — {description}") }
}

/// `[Group chat: "name" - with A, B]`
pub fn format_group_chat_tag(group: &GroupDescription, peers: &[GroupMember]) -> String {
    let with = if peers.is_empty() {
        String::new()
    } else {
        format!(" - with {}", peers.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "))
    };
    format!("{GROUP_CHAT_TAG_PREFIX}\"{}\"{with}]", group_display_name(group))
}

/// System prompt for a member speaking in the room.
pub fn build_group_member_system_prompt(member: &GroupMember, group: &GroupDescription, peers: &[GroupMember]) -> String {
    let mut lines = vec![format!("You are {}, one participant in a group chat ({}).", member.name, describe_group(group))];
    if !member.description.trim().is_empty() {
        lines.push(format!("Your persona: {}", member.description.trim()));
    }
    if !peers.is_empty() {
        lines.push(String::new());
        lines.push("Other participants in the room:".to_owned());
        for peer in peers {
            let desc = peer.description.trim();
            lines.push(if desc.is_empty() { format!("- {}", peer.name) } else { format!("- {} ({desc})", peer.name) });
        }
    }
    lines.push(String::new());
    lines.push(if peers.is_empty() {
        "Right now you are speaking in this group chat.".to_owned()
    } else {
        format!(
            "Right now you are speaking in this group chat, with {}.",
            peers.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
        )
    });
    lines.push("You have your full toolkit in this room. Do the work first, then deliver the result with SendMessage.".to_owned());
    lines.push(String::new());
    lines.push(format!("Stay fully in character as {}. The ONLY way to say something the room can see is the SendMessage tool. Keep each message short and conversational. If you have nothing new worth adding, send exactly \"(pass)\". Never reveal private one-on-one context.", member.name));
    lines.join("\n")
}

/// Messages posted after `member` last spoke (or the whole history).
pub fn messages_since_member_last_spoke<'a>(history: &'a [GroupMessage], member: &AgentId) -> &'a [GroupMessage] {
    match history.iter().rposition(|m| matches!(&m.speaker, Speaker::Member { id, .. } if id == member)) {
        Some(index) => &history[index + 1..],
        None => history,
    }
}

/// The turn prompt handed to a member.
pub fn build_group_turn_prompt(
    member: &GroupMember,
    group: &GroupDescription,
    peers: &[GroupMember],
    new_messages: &[GroupMessage],
    redrive: bool,
) -> String {
    let mut lines = vec![format_group_chat_tag(group, peers)];
    lines.push(if new_messages.is_empty() {
        "No new messages in the room since your last turn.".to_owned()
    } else {
        format!("New messages in the room (oldest first):\n{}", format_group_history(new_messages, &member.id, GROUP_PROMPT_HISTORY_LIMIT))
    });
    lines.push(String::new());
    lines.push(format!("It's your turn, {}. Reply in character with a single SendMessage if you have something worth adding, or send \"(pass)\" if you don't.", member.name));
    let mut out = lines.join("\n");
    if redrive {
        out.push_str(build_group_redrive_note());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: &str, name: &str) -> GroupMember {
        GroupMember { id: AgentId::from(id), name: name.into(), description: String::new() }
    }
    fn user(text: &str) -> GroupMessage {
        GroupMessage { artifacts: Vec::new(), task: None, speaker: Speaker::User { name: None }, content: text.into(), timestamp_ms: 0 }
    }

    #[test]
    fn pass_detection() {
        for s in ["(pass)", "pass", " (pass). ", "PASS", ""] {
            assert!(is_pass_content(s), "{s:?}");
        }
        assert!(!is_pass_content("passing by"));
    }

    #[test]
    fn mentions_pick_named_members() {
        let members = vec![member("a", "Alice Smith"), member("b", "Bob")];
        let m = parse_group_mentions("hey @alice, thoughts?", &members);
        assert_eq!(m.member_ids, vec![AgentId::from("a")]);
        assert!(!m.is_everyone);
        assert!(parse_group_mentions("@everyone go", &members).is_everyone);
        assert!(parse_group_mentions("mail@bob.com", &members).member_ids.is_empty());
    }

    #[test]
    fn responders_default_to_all() {
        let members = vec![member("a", "Alice"), member("b", "Bob")];
        assert_eq!(resolve_responders(&members, &[user("hello all")]).len(), 2);
        assert_eq!(resolve_responders(&members, &[user("@bob explain")]).len(), 1);
    }

    #[test]
    fn round_rotation() {
        assert_eq!(order_round_speakers(&[1, 2, 3], 1), vec![2, 3, 1]);
        assert_eq!(order_round_speakers(&[1, 2, 3], 3), vec![1, 2, 3]);
    }
}
