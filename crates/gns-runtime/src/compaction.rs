//! Context compaction: the original's background summarization
//! (`summarization-orchestrator` / `summarization-handler`) mapped onto the
//! transcript. A summary starts in the background when the unused context
//! window drops to `SUMMARIZATION_TRIGGER_TOKENS` or 10 %, splits at the
//! last user message (the tail from that message on is kept verbatim), and
//! is persisted as a `Divider` whose text is re-injected as
//! `[Previous conversation summary]: …`. When usage is far over the window
//! the next turn waits for the summary first.

use crate::agent::AgentHandle;
use crate::history;
use crate::host::HostInner;
use gns_core::prompt::{SUMMARIZATION_MORE_PROMPT, SUMMARIZATION_SYSTEM_PROMPT, SummaryPart, render_message_for_summary};
use gns_core::text::now_ms;
use gns_core::*;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Estimated tokens the next request would carry (last reported prompt
/// tokens, or a character estimate when the provider reported none).
fn used_tokens(handle: &AgentHandle) -> u64 {
    let last: u64 = handle.db.get_json(kv_keys::LAST_PROMPT_TOKENS).ok().flatten().unwrap_or(0);
    if last > 0 {
        return last;
    }
    (history::window_chars(&handle.history()) / 4) as u64
}

fn should_start(host: &HostInner, handle: &AgentHandle) -> bool {
    let max = host.config.context_window_tokens;
    let used = used_tokens(handle);
    let unused = max.saturating_sub(used);
    unused <= SUMMARIZATION_TRIGGER_TOKENS || (unused as f64) <= (max as f64) * SUMMARIZATION_TRIGGER_FRACTION
}

fn far_over(host: &HostInner, handle: &AgentHandle) -> bool {
    let max = host.config.context_window_tokens;
    let allowance = ((max as f64) * SUMMARIZATION_BLOCK_OVER_FRACTION).min(SUMMARIZATION_BLOCK_OVER_TOKENS as f64) as u64;
    used_tokens(handle) > max + allowance
}

/// After a turn: record the last prompt size and start a background summary when due.
pub(crate) fn maybe_compact_after_turn(host: Arc<HostInner>, handle: Arc<AgentHandle>, last_prompt_tokens: u64) {
    if last_prompt_tokens > 0 {
        let _ = handle.db.set_json(kv_keys::LAST_PROMPT_TOKENS, &last_prompt_tokens);
    }
    if !should_start(&host, &handle) {
        return;
    }
    tokio::spawn(async move {
        compact(host, handle).await;
    });
}

/// Before a turn: block on a summary when usage is far over the window.
pub(crate) async fn compact_if_far_over(host: &Arc<HostInner>, handle: &Arc<AgentHandle>) {
    if far_over(host, handle) {
        compact(host.clone(), handle.clone()).await;
    }
}

async fn compact(host: Arc<HostInner>, handle: Arc<AgentHandle>) {
    if handle.compacting.swap(true, Ordering::SeqCst) {
        return;
    }
    let outcome = compact_inner(&host, &handle).await;
    handle.compacting.store(false, Ordering::SeqCst);
    match outcome {
        Ok(true) => {
            let _ = handle.db.delete(kv_keys::LAST_PROMPT_TOKENS);
        }
        Ok(false) => {}
        Err(e) => {
            host.emit(HostEvent::Error { agent_id: Some(handle.id.clone()), run_id: None, message: format!("summarization failed: {e}") })
        }
    }
}

/// Render one entry as summarization input (`formatMessageForSummary`).
fn render_entries(entries: &[TranscriptEntry], previous_summary: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(summary) = previous_summary {
        out.push(render_message_for_summary(
            "user",
            &[SummaryPart::Text(format!("{}{summary}", gns_core::prompt::PREVIOUS_CONVERSATION_SUMMARY_PREFIX))],
        ));
    }
    let mut assistant_parts: Vec<SummaryPart> = Vec::new();
    let mut tool_results: Vec<SummaryPart> = Vec::new();
    let flush = |out: &mut Vec<String>, assistant_parts: &mut Vec<SummaryPart>, tool_results: &mut Vec<SummaryPart>| {
        if !assistant_parts.is_empty() {
            out.push(render_message_for_summary("assistant", &std::mem::take(assistant_parts)));
        }
        if !tool_results.is_empty() {
            out.push(render_message_for_summary("tool", &std::mem::take(tool_results)));
        }
    };
    for entry in entries {
        match entry {
            e if history::is_display_only(e) => continue,
            TranscriptEntry::Message { role: Role::User, content, from_agent, .. } => {
                flush(&mut out, &mut assistant_parts, &mut tool_results);
                let text = match from_agent {
                    Some(from) => format!("[agent] {} (id: {}): {content}", from.name, from.id),
                    None => content.clone(),
                };
                out.push(render_message_for_summary("user", &[SummaryPart::Text(text)]));
            }
            TranscriptEntry::Message { role: Role::System, content, .. } => {
                flush(&mut out, &mut assistant_parts, &mut tool_results);
                out.push(render_message_for_summary("user", &[SummaryPart::Text(content.clone())]));
            }
            TranscriptEntry::Message { role: Role::Assistant, content, to_agent, .. } => {
                flush(&mut out, &mut assistant_parts, &mut tool_results);
                let text = match to_agent {
                    Some(to) => format!("(to {}) {content}", to.name),
                    None => content.clone(),
                };
                out.push(render_message_for_summary("assistant", &[SummaryPart::Text(text)]));
            }
            TranscriptEntry::AssistantText { content, .. } => {
                flush(&mut out, &mut assistant_parts, &mut tool_results);
                assistant_parts.push(SummaryPart::Text(content.clone()));
            }
            TranscriptEntry::ToolCall { name, args, result, is_error, .. } => {
                assistant_parts.push(SummaryPart::ToolCall { name: name.clone(), args: args.to_string() });
                let result = match result {
                    Some(r) if *is_error => format!("Error: {r}"),
                    Some(r) => r.clone(),
                    None => "(interrupted)".to_owned(),
                };
                tool_results.push(SummaryPart::ToolResult { name: name.clone(), result });
            }
            TranscriptEntry::SendMessage { .. } => continue,
            TranscriptEntry::ProfileUpdate { summary, .. } => {
                flush(&mut out, &mut assistant_parts, &mut tool_results);
                out.push(render_message_for_summary("user", &[SummaryPart::Text(summary.clone())]));
            }
            TranscriptEntry::Divider { .. } => {}
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }
    flush(&mut out, &mut assistant_parts, &mut tool_results);
    out
}

/// Returns Ok(true) when a divider was written.
async fn compact_inner(host: &Arc<HostInner>, handle: &Arc<AgentHandle>) -> Result<bool, HostError> {
    let entries = handle.history();
    let (divider, start) = history::window_bounds(&entries);
    let body: Vec<&TranscriptEntry> = entries.iter().enumerate().skip(start).filter(|(i, _)| Some(*i) != divider).map(|(_, e)| e).collect();
    if body.is_empty() {
        return Ok(false);
    }
    // Split at the last user message; everything before it is summarized and
    // the tail (from that message on) stays verbatim. With nothing before it,
    // summarize everything.
    let owned: Vec<TranscriptEntry> = body.iter().map(|e| (*e).clone()).collect();
    let split = history::last_user_message_index(&owned).filter(|&i| i > 0).unwrap_or(owned.len());
    let (to_summarize, _tail) = owned.split_at(split);
    if to_summarize.is_empty() {
        return Ok(false);
    }
    let previous = match divider.map(|d| &entries[d]) {
        Some(TranscriptEntry::Divider { summary, .. }) => Some(summary.as_str()),
        _ => None,
    };
    let rendered = render_entries(to_summarize, previous);
    let mut budget = host.config.summarization_max_prompt_chars.min(SUMMARIZATION_MAX_PROMPT_CHARS);
    let mut last_error: Option<String> = None;
    let mut summary: Option<String> = None;
    for attempt in 0..3 {
        let mut body = rendered.join("\n\n");
        if body.chars().count() > budget {
            body = omit_oldest(&rendered, budget);
        }
        let user = format!("{body}\n\n<summarization_request>\n{SUMMARIZATION_MORE_PROMPT}\n</summarization_request>");
        match host.llm.complete_text(SUMMARIZATION_SYSTEM_PROMPT, &user).await {
            Ok(text) if !text.trim().is_empty() => {
                summary = Some(text.trim().to_owned());
                break;
            }
            Ok(_) => last_error = Some("empty summary".to_owned()),
            Err(e) => last_error = Some(e.to_string()),
        }
        budget = (budget / if attempt == 0 { 2 } else { 3 }).max(50_000);
    }
    let Some(summary) = summary else {
        return Err(HostError::Other(last_error.unwrap_or_else(|| "No summary generated".to_owned())));
    };
    let epoch: u64 = handle.db.get_json(kv_keys::COMPACTION_EPOCH)?.unwrap_or(0) + 1;
    handle.db.set_json(kv_keys::COMPACTION_EPOCH, &epoch)?;
    let summarized_through = to_summarize.last().map(|e| e.id().clone());
    let divider_entry =
        TranscriptEntry::Divider { id: EntryId::new(), summary, timestamp_ms: now_ms(), compaction_epoch: epoch, summarized_through };
    handle.append(&divider_entry)?;
    host.emit(HostEvent::EntryAppended { agent_id: handle.id.clone(), entry: divider_entry });
    // Prune the cache up to the first kept entry.
    let all = handle.history();
    let (_, keep_from) = history::window_bounds(&all);
    handle.prune_history(keep_from.min(all.len()));
    Ok(true)
}

/// `formatOmittedMessagesPreamble`-style fair truncation: drop the oldest
/// rendered messages until the rest fits, noting how many were omitted.
fn omit_oldest(rendered: &[String], budget: usize) -> String {
    let mut kept: Vec<&String> = rendered.iter().collect();
    let mut omitted = 0usize;
    while kept.len() > 1 && kept.iter().map(|s| s.chars().count() + 2).sum::<usize>() > budget {
        kept.remove(0);
        omitted += 1;
    }
    let mut out = String::new();
    if omitted > 0 {
        out.push_str(&format!("[{omitted} earlier messages omitted for length]\n\n"));
    }
    out.push_str(&kept.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n\n"));
    gns_core::text::clamp_block(&out, budget)
}
