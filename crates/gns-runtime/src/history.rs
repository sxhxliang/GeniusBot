//! Rebuild the model-facing message list from a transcript.
//!
//! The transcript is the source of truth; this module turns it back into
//! the message list the original agent core would hold: user messages
//! wrapped like `state.ts` does (`<timestamp>`, `<incoming_message_id>`,
//! `<user_query>`), inbound peer messages as their `[agent]` wake prompt,
//! and a compaction summary as `[Previous conversation summary]: …`.

use chrono::Offset;
use gns_core::prompt::{
    PREVIOUS_CONVERSATION_SUMMARY_PREFIX, SAND_HIDDEN_PROMPT_MARKER, append_user_reply_reminder, build_reply_context_note,
    build_user_message_address_note,
};
use gns_core::*;

/// Per-render inputs.
#[derive(Clone, Debug, Default)]
pub struct RenderContext {
    /// IANA zone for `<timestamp>` rendering (UTC when unset).
    pub time_zone: Option<String>,
    /// The entry id of the prompt this turn answers (its timestamp is "now").
    pub current_prompt_id: Option<EntryId>,
    /// The prompt text when it is not persisted (subagents).
    pub current_prompt_text: Option<String>,
    /// Extra text appended to the current prompt only (`<automation_status>`
    /// reminder, `<agent_profile_update>` block), placed below the message.
    pub current_prompt_extra: Option<String>,
}

/// The context window of a transcript: the index of the newest `Divider`
/// (rendered as a summary) and the index of the first entry sent verbatim.
/// A divider with `summarized_through` keeps the entries after that id (the
/// verbatim tail written before the divider) in the window.
pub fn window_bounds(entries: &[TranscriptEntry]) -> (Option<usize>, usize) {
    let Some(d) = entries.iter().rposition(|e| matches!(e, TranscriptEntry::Divider { .. })) else { return (None, 0) };
    let through = match &entries[d] {
        TranscriptEntry::Divider { summarized_through, .. } => summarized_through.clone(),
        _ => None,
    };
    let start = match through.and_then(|id| entries[..d].iter().rposition(|e| e.id() == &id)) {
        Some(k) => k + 1,
        None if through_is_missing(&entries[d]) => 0,
        None => d + 1,
    };
    (Some(d), start)
}

fn through_is_missing(divider: &TranscriptEntry) -> bool {
    matches!(divider, TranscriptEntry::Divider { summarized_through: Some(_), .. })
}

/// Entries that are in the display transcript but not (yet) in the model
/// context: room posts, and user-side messages (typed or inbound from a
/// peer) whose run has not started. A run claims its own message when it
/// starts, and inbound wakes add their `[agent]` prompt as a hidden entry.
pub fn is_display_only(entry: &TranscriptEntry) -> bool {
    matches!(
        entry,
        TranscriptEntry::Message { role: Role::User, run_id: None, .. } | TranscriptEntry::SendMessage { group_id: Some(_), .. }
    )
}

/// Characters an entry contributes to the context window.
pub fn window_len(entry: &TranscriptEntry) -> usize {
    if is_display_only(entry) { 0 } else { entry.char_len() }
}

/// Characters of the entries inside the window (the divider excluded).
pub fn window_chars(entries: &[TranscriptEntry]) -> usize {
    let (divider, start) = window_bounds(entries);
    entries.iter().enumerate().skip(start).filter(|(i, _)| Some(*i) != divider).map(|(_, e)| window_len(e)).sum()
}

/// `buildCurrentTimestamp`: `Monday, Sep 23, 2026, 10:32 AM (UTC+8)`.
pub fn format_timestamp(ms: i64, time_zone: Option<&str>) -> String {
    let Some(utc) = chrono::DateTime::from_timestamp_millis(ms) else { return String::new() };
    let (formatted, offset_secs) = match time_zone.and_then(|tz| tz.parse::<chrono_tz::Tz>().ok()) {
        Some(tz) => {
            let local = utc.with_timezone(&tz);
            (local.format("%A, %b %-d, %Y, %-I:%M %p").to_string(), local.offset().fix().local_minus_utc())
        }
        None => (utc.format("%A, %b %-d, %Y, %-I:%M %p").to_string(), 0),
    };
    let offset = if offset_secs == 0 {
        "UTC".to_owned()
    } else {
        let sign = if offset_secs < 0 { '-' } else { '+' };
        let total = offset_secs.unsigned_abs();
        let hours = total / 3600;
        let minutes = (total % 3600) / 60;
        if minutes == 0 { format!("UTC{sign}{hours}") } else { format!("UTC{sign}{hours}:{minutes:02}") }
    };
    format!("{formatted} ({offset})")
}

fn timestamp_prefix(ms: i64, time_zone: Option<&str>) -> String {
    format!("<timestamp>{}</timestamp>\n", format_timestamp(ms, time_zone))
}

/// `renderIncomingMessageIdTag`.
fn incoming_message_id_tag(id: &str) -> String {
    format!("<incoming_message_id>{}</incoming_message_id>", id.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"))
}

/// Wrap a persisted user prompt the way `assembleTurnAction` + the agent
/// core do before sending it: `[address]`, `[In reply to …]`, the text, the
/// per-turn extras, the reply reminder, then `<timestamp>`/`<user_query>`.
fn render_user_prompt(
    id: &EntryId,
    content: &str,
    hidden: bool,
    timestamp_ms: i64,
    reply_note: Option<String>,
    ctx: &RenderContext,
) -> String {
    let is_current = ctx.current_prompt_id.as_ref() == Some(id);
    let ts = if is_current { gns_core::text::now_ms() } else { timestamp_ms };
    let prefix = timestamp_prefix(ts, ctx.time_zone.as_deref());
    if hidden || content.starts_with(SAND_HIDDEN_PROMPT_MARKER) {
        return format!("{prefix}{content}");
    }
    let addressed = id.as_str().starts_with('t');
    let address = if addressed { build_user_message_address_note(id.as_str()) } else { String::new() };
    let head: Vec<&str> = [address.as_str(), reply_note.as_deref().unwrap_or("")].into_iter().filter(|s| !s.is_empty()).collect();
    let mut text = if head.is_empty() {
        content.trim().to_owned()
    } else if content.trim().is_empty() {
        head.join("\n")
    } else {
        format!("{}\n{}", head.join("\n"), content.trim())
    };
    if is_current && let Some(extra) = ctx.current_prompt_extra.as_deref().filter(|e| !e.is_empty()) {
        text = if text.is_empty() { extra.to_owned() } else { format!("{text}\n\n{extra}") };
    }
    let text = append_user_reply_reminder(&text);
    let tag = if addressed { format!("{}\n", incoming_message_id_tag(id.as_str())) } else { String::new() };
    format!("{tag}{prefix}<user_query>\n{text}\n</user_query>")
}

/// Quote for a reply context (`describeMessageQuote`).
fn reply_quote(entries: &[TranscriptEntry], target: &EntryId) -> Option<String> {
    let entry = entries.iter().find(|e| e.id() == target)?;
    let quote = match entry {
        TranscriptEntry::Message { content, .. } => content.clone(),
        TranscriptEntry::SendMessage { message, .. } => message.display_text(),
        _ => return None,
    };
    Some(gns_core::text::clamp_line(&quote, 120))
}

/// Convert transcript entries (chronological) into LLM messages inside the
/// context window (see [`window_bounds`]).
pub fn build_messages(entries: &[TranscriptEntry], ctx: &RenderContext) -> Vec<LlmMessage> {
    let (divider_index, start) = window_bounds(entries);

    let mut messages: Vec<LlmMessage> = Vec::new();
    if let Some(TranscriptEntry::Divider { summary, .. }) = divider_index.map(|i| &entries[i]) {
        messages.push(LlmMessage::user(format!("{PREVIOUS_CONVERSATION_SUMMARY_PREFIX}{summary}")));
    }

    let mut pending_text: Option<String> = None;
    let mut pending_run: Option<RunId> = None;
    let mut pending_calls: Vec<LlmToolCall> = Vec::new();
    let mut pending_results: Vec<LlmToolResult> = Vec::new();

    fn flush(messages: &mut Vec<LlmMessage>, text: &mut Option<String>, calls: &mut Vec<LlmToolCall>, results: &mut Vec<LlmToolResult>) {
        if text.is_none() && calls.is_empty() {
            return;
        }
        let tool_calls = std::mem::take(calls);
        let had_calls = !tool_calls.is_empty();
        messages.push(LlmMessage::Assistant { text: text.take(), tool_calls });
        if had_calls {
            messages.push(LlmMessage::ToolResults(std::mem::take(results)));
        }
    }

    // A typed user message enters the model context when its run starts
    // (the original appends the prompt at run time), so a message claimed by
    // a later run is rendered right before that run's first entry rather
    // than at its append position.
    let mut deferred: Vec<(RunId, &TranscriptEntry)> = Vec::new();
    let mut ordered: Vec<&TranscriptEntry> = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate().skip(start) {
        if let TranscriptEntry::Message { role: Role::User, run_id: Some(run), from_agent: None, group_id: None, .. } = entry {
            // Defer only when the run's first other entry comes later with
            // entries of other runs in between.
            let first_of_run = entries[index + 1..].iter().position(|e| entry_run_id(e) == Some(run));
            if first_of_run.is_some_and(|offset| offset > 0) {
                deferred.push((run.clone(), entry));
                continue;
            }
        }
        if let Some(run) = entry_run_id(entry) {
            let mut i = 0;
            while i < deferred.len() {
                if &deferred[i].0 == run {
                    ordered.push(deferred.remove(i).1);
                } else {
                    i += 1;
                }
            }
        }
        ordered.push(entry);
    }
    ordered.extend(deferred.into_iter().map(|(_, e)| e));

    for entry in ordered {
        match entry {
            e if is_display_only(e) => continue,
            TranscriptEntry::Message { role: Role::User, group_id: Some(_), run_id: Some(_), content, timestamp_ms, .. } => {
                // The member's own room turn: a plain query, no address and no reply reminder.
                flush(&mut messages, &mut pending_text, &mut pending_calls, &mut pending_results);
                let prefix = timestamp_prefix(*timestamp_ms, ctx.time_zone.as_deref());
                messages.push(LlmMessage::user(format!("{prefix}<user_query>\n{content}\n</user_query>")));
            }
            TranscriptEntry::Message { id, role: Role::User, hidden, content, images, timestamp_ms, reply_to, .. } => {
                flush(&mut messages, &mut pending_text, &mut pending_calls, &mut pending_results);
                let reply_note = reply_to
                    .as_ref()
                    .and_then(|t| reply_quote(entries, t).map(|q| build_reply_context_note(t.as_str(), &q)))
                    .filter(|n| !n.is_empty());
                let text = render_user_prompt(id, content, *hidden, *timestamp_ms, reply_note, ctx);
                messages.push(LlmMessage::User { text, images: images.clone() });
            }
            TranscriptEntry::Message { role: Role::System, content, .. } => {
                flush(&mut messages, &mut pending_text, &mut pending_calls, &mut pending_results);
                messages.push(LlmMessage::user(format!("{SAND_HIDDEN_PROMPT_MARKER}{content}")));
            }
            TranscriptEntry::Message { role: Role::Assistant, .. } | TranscriptEntry::SendMessage { .. } => continue,
            TranscriptEntry::AssistantText { content, run_id, .. } => {
                flush(&mut messages, &mut pending_text, &mut pending_calls, &mut pending_results);
                pending_text = Some(content.clone());
                pending_run = Some(run_id.clone());
            }
            TranscriptEntry::ToolCall { call_id, name, args, result, is_error, run_id, .. } => {
                if pending_run.as_ref() != Some(run_id) && (!pending_calls.is_empty() || pending_text.is_some()) {
                    flush(&mut messages, &mut pending_text, &mut pending_calls, &mut pending_results);
                }
                pending_run = Some(run_id.clone());
                pending_calls.push(LlmToolCall { id: call_id.clone(), name: name.clone(), arguments: args.clone() });
                let content = match result {
                    Some(r) if *is_error => format!("Error: {r}"),
                    Some(r) => r.clone(),
                    None => "(interrupted before the tool finished)".to_owned(),
                };
                pending_results.push(LlmToolResult { call_id: call_id.clone(), name: name.clone(), content });
            }
            TranscriptEntry::ProfileUpdate { summary, .. } => {
                flush(&mut messages, &mut pending_text, &mut pending_calls, &mut pending_results);
                messages.push(LlmMessage::user(format!("{SAND_HIDDEN_PROMPT_MARKER}{summary}")));
            }
            TranscriptEntry::Divider { .. } => {}
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }
    flush(&mut messages, &mut pending_text, &mut pending_calls, &mut pending_results);
    if let Some(text) = &ctx.current_prompt_text
        && ctx.current_prompt_id.is_none()
    {
        messages.push(LlmMessage::user(text.clone()));
    }
    messages
}

/// The run an entry belongs to, if any.
fn entry_run_id(entry: &TranscriptEntry) -> Option<&RunId> {
    match entry {
        TranscriptEntry::Message { run_id, .. } => run_id.as_ref(),
        TranscriptEntry::SendMessage { run_id, .. }
        | TranscriptEntry::AssistantText { run_id, .. }
        | TranscriptEntry::ToolCall { run_id, .. } => Some(run_id),
        _ => None,
    }
}

/// Rough size of a rendered message, in characters.
pub fn message_chars(message: &LlmMessage) -> usize {
    match message {
        LlmMessage::User { text, .. } => text.chars().count(),
        LlmMessage::Assistant { text, tool_calls } => {
            text.as_ref().map(|t| t.chars().count()).unwrap_or(0) + tool_calls.iter().map(|c| c.arguments.to_string().len()).sum::<usize>()
        }
        LlmMessage::ToolResults(results) => results.iter().map(|r| r.content.chars().count()).sum(),
        #[allow(unreachable_patterns)]
        _ => 0,
    }
}

/// Index of the entry that opens the last user message of the window, i.e.
/// where a summary would split (`preserveLastUserMessage`).
pub fn last_user_message_index(entries: &[TranscriptEntry]) -> Option<usize> {
    entries.iter().rposition(|e| matches!(e, TranscriptEntry::Message { role: Role::User, .. }) && !is_display_only(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> TranscriptEntry {
        TranscriptEntry::Message {
            artifacts: Vec::new(),
            id: EntryId::new(),
            role: Role::User,
            content: text.into(),
            timestamp_ms: 0,
            hidden: false,
            run_id: Some(RunId::new()),
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
    fn groups_tool_calls_with_results() {
        let run = RunId::new();
        let entries = vec![
            user("hi"),
            TranscriptEntry::AssistantText { id: EntryId::new(), content: "thinking".into(), timestamp_ms: 0, run_id: run.clone() },
            TranscriptEntry::ToolCall {
                id: EntryId::new(),
                call_id: "c1".into(),
                name: "Shell".into(),
                args: serde_json::json!({"command":"ls"}),
                result: Some("ok".into()),
                is_error: false,
                timestamp_ms: 0,
                run_id: run.clone(),
            },
            TranscriptEntry::ToolCall {
                id: EntryId::new(),
                call_id: "c2".into(),
                name: "SendMessage".into(),
                args: serde_json::json!({}),
                result: Some("Message sent to user.".into()),
                is_error: false,
                timestamp_ms: 0,
                run_id: run.clone(),
            },
            user("thanks"),
        ];
        let messages = build_messages(&entries, &RenderContext::default());
        assert_eq!(messages.len(), 4);
        assert!(
            matches!(&messages[0], LlmMessage::User { text, .. } if text.contains("<user_query>\nhi\n\n<system_reminder>") && text.contains("<timestamp>"))
        );
        assert!(matches!(&messages[1], LlmMessage::Assistant { text: Some(t), tool_calls } if t == "thinking" && tool_calls.len() == 2));
        assert!(matches!(&messages[2], LlmMessage::ToolResults(r) if r.len() == 2));
    }

    #[test]
    fn window_keeps_tail_before_divider() {
        let a = user("a");
        let b = user("b");
        let c = user("c");
        let divider = TranscriptEntry::Divider {
            id: EntryId::new(),
            summary: "sum".into(),
            timestamp_ms: 0,
            compaction_epoch: 1,
            summarized_through: Some(a.id().clone()),
        };
        let d = user("d");
        let entries = vec![a, b.clone(), c, divider, d];
        let (div, start) = window_bounds(&entries);
        assert_eq!((div, start), (Some(3), 1));
        let messages = build_messages(&entries, &RenderContext::default());
        assert_eq!(messages.len(), 4, "summary + b + c + d");
        assert!(matches!(&messages[0], LlmMessage::User { text, .. } if text.starts_with("[Previous conversation summary]: ")));
        assert!(matches!(&messages[1], LlmMessage::User { text, .. } if text.contains("<user_query>\nb\n")));
        assert_eq!(window_chars(&entries), 3);
    }

    #[test]
    fn claimed_user_message_renders_where_its_run_starts() {
        let kick = RunId::new();
        let user_run = RunId::new();
        let mut hello = user("hello");
        if let TranscriptEntry::Message { id, run_id, .. } = &mut hello {
            *id = EntryId::from("t0u");
            *run_id = Some(user_run.clone());
        }
        let mut kickstart = user("[SAND_HIDDEN_PROMPT][first run] hi");
        if let TranscriptEntry::Message { run_id, hidden, .. } = &mut kickstart {
            *run_id = Some(kick.clone());
            *hidden = true;
        }
        let entries = vec![
            hello,
            kickstart,
            TranscriptEntry::AssistantText { id: EntryId::new(), content: "greeting".into(), timestamp_ms: 0, run_id: kick.clone() },
            TranscriptEntry::AssistantText { id: EntryId::new(), content: "answer".into(), timestamp_ms: 0, run_id: user_run.clone() },
        ];
        let messages = build_messages(&entries, &RenderContext::default());
        let texts: Vec<String> = messages
            .iter()
            .map(|m| match m {
                LlmMessage::User { text, .. } => text.clone(),
                LlmMessage::Assistant { text, .. } => text.clone().unwrap_or_default(),
                _ => String::new(),
            })
            .collect();
        assert!(texts[0].contains("[first run]"), "{texts:?}");
        assert_eq!(texts[1], "greeting");
        assert!(texts[2].contains("\nhello\n"), "{texts:?}");
        assert_eq!(texts[3], "answer");
    }

    #[test]
    fn formats_timestamps_like_intl() {
        // 2026-09-23T02:32:00Z
        let ms = 1790130720000;
        assert_eq!(format_timestamp(ms, None), "Wednesday, Sep 23, 2026, 2:32 AM (UTC)");
        assert_eq!(format_timestamp(ms, Some("Asia/Shanghai")), "Wednesday, Sep 23, 2026, 10:32 AM (UTC+8)");
    }
}
