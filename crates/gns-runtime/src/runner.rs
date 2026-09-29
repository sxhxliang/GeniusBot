//! The step loop: one turn = up to N LLM calls with tool round-trips.
//!
//! Mirrors the original `runner.run` → `runPreparedTurn` → `settle` chain:
//! the prompt is shaped like `assembleTurnAction`, the two reminder
//! middlewares run before every model call, tool calls of one step execute
//! concurrently, `ensureUserReply` nudges after a user turn, and settle
//! records memory synchronously before the caller sees the result.

use crate::agent::{AgentHandle, EntryIdKind, RunJob};
use crate::history;
use crate::host::HostInner;
use gns_core::memory::{
    EpisodeTurn, build_episode_system_prompt, build_episode_user_prompt, build_extraction_system_prompt, build_extraction_user_prompt,
    is_memorable_exchange, parse_memory_extraction, select_relevant_memories,
};
use gns_core::policy::{ApprovalRequest, PolicyDecision, approval_timed_out_result, blocked_tool_result, denied_tool_result};
use gns_core::prompt::{
    CLOSING_SEND_NUDGE_PROMPT, EARLY_RESULT_REMINDER_MESSAGE, REPLY_NUDGE_PROMPT, SEND_MESSAGE_REMINDER_MESSAGE,
    START_OF_TURN_ACK_REMINDER_MESSAGE,
};
use gns_core::text::{CHAR_HARD_LIMIT, now_ms, truncate_output};
use gns_core::*;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

/// Tools whose call counts as reaching the user (`DELIVERY_TOOL_NAMES`).
pub fn is_delivery_tool(name: &str) -> bool {
    name == SEND_MESSAGE_TOOL_NAME || name == REACT_TO_MESSAGE_TOOL_NAME
}

/// Run one job to completion (or cancellation).
pub(crate) async fn run_job(
    host: Arc<HostInner>,
    handle: Arc<AgentHandle>,
    run_id: RunId,
    job: RunJob,
    cancel: CancellationToken,
) -> RunResult {
    match host.register_coordination_run(&handle.id, &run_id, &job) {
        Ok(true) => {}
        Ok(false) => return RunResult { superseded: true, ..Default::default() },
        Err(e) => return RunResult { error: Some(e.to_string()), ..Default::default() },
    }
    handle.set_active_lane(Some(job.options.lane));
    host.register_run_cancel(&run_id, &cancel);
    host.emit(HostEvent::RunStarted {
        agent_id: handle.id.clone(),
        run_id: run_id.clone(),
        lane: job.options.lane,
        source: job.options.source,
    });
    let started_at = now_ms();
    let mut result = match run_inner(&host, &handle, &run_id, &job, &cancel, false).await {
        Ok(result) => result,
        Err(e) => {
            host.emit(HostEvent::Error { agent_id: Some(handle.id.clone()), run_id: Some(run_id.clone()), message: e.to_string() });
            RunResult { error: Some(e.to_string()), aborted: cancel.is_cancelled(), ..Default::default() }
        }
    };
    // A single bounded reminder makes missing task submission explicit.
    if !result.aborted && result.error.is_none() && host.task_for_run(&run_id).is_some_and(|t| t.status == TaskStatus::Running) {
        let mut nudge = job.clone();
        nudge.already_persisted = false;
        nudge.prompt = "Your task has no submitted result. Call CompleteTask with full output files, or UpdateTask with status=blocked and the exact reason. A chat message is not a submission.".to_owned();
        merge_nudge(&mut result, run_inner(&host, &handle, &run_id, &nudge, &cancel, false).await, &host, &handle, &run_id);
    }
    host.settle_task_run(&run_id, &result);
    host.emit(HostEvent::RunEnded { agent_id: handle.id.clone(), run_id: run_id.clone() });
    // `settleCompletedTurn` runs inside the turn, before nudges.
    if !result.aborted {
        settle_completed(&host, &handle, &run_id, &job, &result).await;
    }
    ensure_user_reply(&host, &handle, &run_id, &job, &mut result, &cancel).await;
    if job.options.group_id.is_some() {
        result.room_messages = host.take_room_buffer(&run_id);
    }
    if job.options.source == RunSource::Kickstart && !result.aborted && result.sent_message_count > 0 {
        let _ = handle.db.set_json(kv_keys::INTRODUCTION_PENDING, &false);
    }
    settle_after(&host, &handle, &run_id, &job, &result, started_at).await;
    host.unregister_run_cancel(&run_id);
    if let Ok(mut contexts) = host.coordination_runtime.runs.lock() {
        contexts.remove(&run_id);
    }
    handle.set_active_lane(None);
    host.emit(HostEvent::TurnEnded { agent_id: handle.id.clone(), run_id: run_id.clone(), result: result.clone() });
    result
}

/// `ensureUserReply` / `ensureHiddenTurnReply`: re-run the agent with a
/// hidden nudge while it still owes the user a reply (up to
/// [`MAX_REPLY_NUDGES`] times on user turns, once on hidden turns that owe a
/// reply), then once more when it acknowledged and went silent behind tool calls.
async fn ensure_user_reply(
    host: &Arc<HostInner>,
    handle: &Arc<AgentHandle>,
    run_id: &RunId,
    job: &RunJob,
    result: &mut RunResult,
    cancel: &CancellationToken,
) {
    let options = &job.options;
    if options.is_silence_allowed
        || options.group_id.is_some()
        || options.task_id.is_some()
        || !job.persist
        || result.aborted
        || result.superseded
        || result.error.is_some()
    {
        return;
    }
    let epoch_current = || options.source != RunSource::User || job.epoch == handle.current_turn_epoch();
    let nudge_options = RunOptions { hidden: true, is_silence_allowed: false, append_reply_reminder: false, ..options.clone() };
    let max_nudges = if options.source == RunSource::User { MAX_REPLY_NUDGES } else { 1 };
    let mut attempts = 0;
    while result.delivery_owed() && attempts < max_nudges && !cancel.is_cancelled() && epoch_current() {
        attempts += 1;
        tracing::info!(agent = %handle.id, attempt = attempts, "reply nudge: turn ended without SendMessage");
        let nudge = RunJob::hidden(REPLY_NUDGE_PROMPT, nudge_options.clone());
        merge_nudge(result, run_inner(host, handle, run_id, &nudge, cancel, true).await, host, handle, run_id);
        if result.aborted || result.error.is_some() {
            break;
        }
    }
    let mut closing_delivery_missing = false;
    if options.source == RunSource::User
        && result.ended_on_silent_tool_calls
        && !result.aborted
        && result.error.is_none()
        && !result.awaiting_user_selection
        && !cancel.is_cancelled()
        && epoch_current()
    {
        tracing::info!(agent = %handle.id, "closing send nudge: acknowledged, then silent tool calls");
        let nudge = RunJob::hidden(CLOSING_SEND_NUDGE_PROMPT, nudge_options);
        let sent_before = result.sent_message_count;
        merge_nudge(result, run_inner(host, handle, run_id, &nudge, cancel, true).await, host, handle, run_id);
        closing_delivery_missing = result.sent_message_count == sent_before;
    }
    if (result.delivery_owed() || closing_delivery_missing)
        && result.error.is_none()
        && !result.aborted
        && !cancel.is_cancelled()
        && epoch_current()
    {
        let message = "The model ended without delivering a reply through SendMessage, even after the reply reminders. Plain assistant text was not sent to the user.".to_owned();
        host.emit(HostEvent::Error { agent_id: Some(handle.id.clone()), run_id: Some(run_id.clone()), message: message.clone() });
        result.error = Some(message);
    }
}

fn merge_nudge(
    result: &mut RunResult,
    nudged: Result<RunResult, HostError>,
    host: &Arc<HostInner>,
    handle: &Arc<AgentHandle>,
    run_id: &RunId,
) {
    match nudged {
        Ok(n) => {
            result.sent_message_count += n.sent_message_count;
            result.reacted |= n.reacted;
            result.usage.add(&n.usage);
            result.steps += n.steps;
            result.text = n.text;
            result.aborted = n.aborted;
            result.awaiting_user_selection = n.awaiting_user_selection;
            result.ended_on_silent_tool_calls = n.ended_on_silent_tool_calls;
            if n.error.is_some() {
                result.error = n.error;
            }
        }
        Err(e) => {
            host.emit(HostEvent::Error {
                agent_id: Some(handle.id.clone()),
                run_id: Some(run_id.clone()),
                message: format!("reply nudge failed: {e}"),
            });
            result.error = Some(e.to_string());
        }
    }
}

/// Per-step bookkeeping for `turnEndedOnSilentToolCalls`.
#[derive(Default)]
struct TurnShape {
    /// Steps with tool calls seen so far: (had a delivery call, every delivery call errored).
    tool_steps: Vec<(bool, bool)>,
}

impl TurnShape {
    fn ended_on_silent_tool_calls(&self) -> bool {
        let Some(&(tail_delivery, _)) = self.tool_steps.last() else { return false };
        if tail_delivery {
            return false;
        }
        let mut acked_first = false;
        for &(delivery, all_errored) in &self.tool_steps {
            if !acked_first {
                if !delivery {
                    return false;
                }
                acked_first = true;
            } else if delivery && !all_errored {
                return false;
            }
        }
        acked_first
    }
}

async fn run_inner(
    host: &Arc<HostInner>,
    handle: &Arc<AgentHandle>,
    run_id: &RunId,
    job: &RunJob,
    cancel: &CancellationToken,
    require_reply: bool,
) -> Result<RunResult, HostError> {
    let options = &job.options;
    let persist = job.persist;
    if let Some(group_id) = &options.group_id {
        host.open_room_buffer(run_id, group_id);
    }
    let compaction_epoch: u64 = handle.db.get_json(kv_keys::COMPACTION_EPOCH)?.unwrap_or(0);

    // 1. Shape the prompt like `assembleTurnAction`. A user message was
    //    persisted raw at send time (its address line, reminders and extras
    //    are rendered when the history is rebuilt); a hidden prompt is stored
    //    already shaped.
    let extras = turn_extras(host, handle, job, compaction_epoch);
    let shaped = if job.already_persisted { job.prompt.clone() } else { shape_hidden_prompt(job, extras.as_deref()) };
    let prompt_entry_id = if persist && !job.already_persisted {
        // Room turns and hidden wakes are not user-typed: stored hidden, no address.
        let internal = options.hidden || options.group_id.is_some();
        let id = handle.next_entry_id(if internal { EntryIdKind::Internal } else { EntryIdKind::UserMessage });
        let entry = TranscriptEntry::Message {
            artifacts: Vec::new(),
            id: id.clone(),
            role: Role::User,
            content: shaped.clone(),
            timestamp_ms: now_ms(),
            hidden: internal,
            run_id: Some(run_id.clone()),
            from_agent: None,
            to_agent: None,
            images: job.images.clone(),
            group_id: options.group_id.clone(),
            priority: false,
            user_name: None,
            reply_to: job.reply_context.as_ref().map(|(id, _)| id.clone()),
        };
        handle.append(&entry)?;
        host.emit(HostEvent::EntryAppended { agent_id: handle.id.clone(), entry });
        Some(id)
    } else {
        // A user send was persisted at send time: claim it (and any earlier
        // typed message a superseded turn left unclaimed, like
        // `prependUserMessages`) so they enter the model context now.
        if persist && let Some(id) = &job.message_id {
            for mut entry in handle.history() {
                let TranscriptEntry::Message { id: eid, role: Role::User, run_id: slot @ None, from_agent: None, group_id: None, .. } =
                    &mut entry
                else {
                    continue;
                };
                let is_own = eid == id;
                *slot = Some(run_id.clone());
                handle.update(&entry)?;
                if is_own {
                    break;
                }
            }
        }
        job.message_id.clone()
    };
    if persist {
        for note in &job.context_notes {
            handle.append(&TranscriptEntry::Message {
                artifacts: Vec::new(),
                id: EntryId::new(),
                role: Role::System,
                content: note.clone(),
                timestamp_ms: now_ms(),
                hidden: true,
                run_id: Some(run_id.clone()),
                from_agent: None,
                to_agent: None,
                images: Vec::new(),
                group_id: None,
                priority: false,
                user_name: None,
                reply_to: None,
            })?;
        }
    }

    // Far over the window: wait for a summary before this turn (`WaitForCompletion`).
    if persist && options.source != RunSource::Subagent {
        crate::compaction::compact_if_far_over(host, handle).await;
    }

    // 2. Rebuild history from the cached window.
    let mut messages = if persist {
        history::build_messages(
            &handle.history(),
            &history::RenderContext {
                time_zone: host.config.time_zone.clone(),
                current_prompt_id: prompt_entry_id.clone(),
                current_prompt_text: None,
                current_prompt_extra: if job.already_persisted { extras.clone() } else { None },
            },
        )
    } else {
        vec![LlmMessage::User { text: shaped.clone(), images: job.images.clone() }]
    };
    if messages.is_empty() {
        messages.push(LlmMessage::User { text: shaped.clone(), images: job.images.clone() });
    }

    // 3. System prompt and tools.
    let mut system = host.build_system_prompt(handle, options.source, options.base_prompt_override.as_deref())?;
    if let Some(task) = host.task_for_run(run_id) {
        system.push_str(&format!("\n\n## Delegated task {}\nYou are the assigned executor. For this task UpdateTask and CompleteTask are your voice, overriding general SendMessage guidance. Do not use SendMessage or SendToAgent to claim completion. Submit actual full files using CompleteTask; the host delivers to the original conversation. If blocked, explain with UpdateTask. require_files={}. Never paste a full file into a message or invent sandbox URLs.", task.id, task.require_files));
    } else {
        system.push_str("\n\n## Tasks and files\nUse DelegateTask for work assigned to teammates, with require_files=true when files are requested. Use SendToAgent for ordinary messages. Publish files using files paths or forward artifact_ids on SendMessage/SendToAgent, never by pasting their contents or inventing sandbox links. FetchArtifact obtains a local file for Read/Shell. Task results and downloads are delivered automatically to the originating conversation; review and explain them as needed.");
    }
    if options.source != RunSource::Subagent {
        host.mcp.sync(host, handle, false).await;
    }
    let mut tools = host.tools_for_agent(handle, options.source, options.group_id.is_some(), options.base_prompt_override.is_some());
    if options.task_id.is_some() {
        tools.retain(|t| !matches!(t.name(), "SendMessage" | "DelegateTask"));
    }
    let mut specs: Vec<ToolSpec> = tools.iter().map(|t| tool_spec(t.as_ref())).collect();
    let ctx = ToolContext {
        agent_id: handle.id.clone(),
        agent_name: handle.name(),
        run_id: run_id.clone(),
        lane: options.lane,
        source: options.source,
        workspace_dir: handle.workspace_dir.clone(),
        data_dir: handle.data_dir.clone(),
        agents_root: host.layout.agents_root(),
        cancel: cancel.clone(),
        services: host.services(),
        group_id: options.group_id.clone(),
    };

    // 4. Step loop.
    let max_steps = options.max_steps.unwrap_or(host.config.max_steps);
    let mut result = RunResult::default();
    // Reminder middlewares (`SendMessageReminderMiddleware`,
    // `StartOfTurnAckReminderMiddleware`): active on every non-subagent,
    // non-silence turn.
    let reminders_active = !options.is_silence_allowed && options.source != RunSource::Subagent;
    let require_initial_reply = require_reply
        || (reminders_active
            && options.group_id.is_none()
            && options.task_id.is_none()
            && (options.source == RunSource::Kickstart || (options.source == RunSource::User && host.config.enforce_start_of_turn_ack)));
    let mut non_send_calls_since_send = 0usize;
    let mut text_send_this_turn = false;
    let mut any_send_this_turn = false;
    let mut early_reminder_fired_this_streak = false;
    let mut shape = TurnShape::default();
    let mut step = 0usize;
    loop {
        if step >= max_steps {
            // The original loop simply stops at the cap.
            break;
        }
        if cancel.is_cancelled() {
            result.aborted = true;
            break;
        }
        step += 1;
        result.steps = step;
        if step > 1 {
            // Tools may change mid-turn (update_state attaching an MCP server).
            tools = host.tools_for_agent(handle, options.source, options.group_id.is_some(), options.base_prompt_override.is_some());
            if options.task_id.is_some() {
                tools.retain(|t| !matches!(t.name(), "SendMessage" | "DelegateTask"));
            }
            specs = tools.iter().map(|t| tool_spec(t.as_ref())).collect();
        }
        // Reminders go in front of the model call, never twice in a row.
        if reminders_active {
            let reminder = if non_send_calls_since_send > SEND_MESSAGE_REMINDER_THRESHOLD {
                Some(SEND_MESSAGE_REMINDER_MESSAGE)
            } else if non_send_calls_since_send > 0 && any_send_this_turn && !early_reminder_fired_this_streak {
                early_reminder_fired_this_streak = true;
                Some(EARLY_RESULT_REMINDER_MESSAGE)
            } else {
                None
            };
            if let Some(text) = reminder {
                messages.push(LlmMessage::user(text));
            }
            if !text_send_this_turn && non_send_calls_since_send > START_OF_TURN_ACK_THRESHOLD {
                messages.push(LlmMessage::user(START_OF_TURN_ACK_REMINDER_MESSAGE));
            }
        }
        let mut request = LlmRequest {
            system: system.clone(),
            messages: messages.clone(),
            tools: specs.clone(),
            options: LlmOptions {
                prompt_cache_key: Some(handle.id.to_string()),
                required_tool: (require_initial_reply && result.delivery_owed() && specs.iter().any(|s| s.name == SEND_MESSAGE_TOOL_NAME))
                    .then(|| SEND_MESSAGE_TOOL_NAME.to_owned()),
                ..Default::default()
            },
        };
        for middleware in host.middlewares() {
            middleware.before_llm_call(&handle.id, run_id, step, &mut request).await;
        }
        handle.dispatched.store(true, Ordering::SeqCst);
        let response = match complete_with_retry(host, handle, run_id, request, cancel).await {
            Ok(r) => r,
            Err(LlmError::Cancelled) => {
                result.aborted = true;
                break;
            }
            Err(e) => return Err(e.into()),
        };
        result.usage.add(&response.usage);
        result.last_prompt_tokens = response.usage.prompt_tokens;
        if let Some(text) = response.text.as_ref().filter(|t| !t.trim().is_empty()) {
            result.text = text.clone();
            if persist {
                handle.append(&TranscriptEntry::AssistantText {
                    id: EntryId::new(),
                    content: text.clone(),
                    timestamp_ms: now_ms(),
                    run_id: run_id.clone(),
                })?;
            }
        }

        if response.tool_calls.is_empty() {
            break;
        }

        // Execute the step's tool calls concurrently, results in call order
        // (`tool-stream-executor`), with each call recorded before it starts.
        let mut pending = Vec::new();
        for call in &response.tool_calls {
            pending.push(execute_tool_call(host, handle, &ctx, &tools, &specs, call, persist));
        }
        let executed: Vec<Result<ExecutedCall, HostError>> = futures::future::join_all(pending).await;
        let mut tool_results = Vec::new();
        let mut end_turn = false;
        let mut step_delivery = false;
        let mut step_delivery_all_errored = true;
        for (call, executed) in response.tool_calls.iter().zip(executed) {
            let executed = executed?;
            if is_delivery_tool(&call.name) {
                step_delivery = true;
                if !executed.is_error {
                    step_delivery_all_errored = false;
                }
            }
            if executed.cancelled {
                result.aborted = true;
            }
            for effect in executed.effects {
                match effect {
                    TurnEffect::MessageSent => {
                        result.sent_message_count += 1;
                        any_send_this_turn = true;
                        if send_message_text(call).is_some() {
                            text_send_this_turn = true;
                        }
                    }
                    TurnEffect::Reacted => result.reacted = true,
                    TurnEffect::AwaitingUserSelection => {
                        result.awaiting_user_selection = true;
                        end_turn = true;
                    }
                    TurnEffect::EndTurn => end_turn = true,
                    #[allow(unreachable_patterns)]
                    _ => {}
                }
            }
            tool_results.push(executed.result);
        }
        shape.tool_steps.push((step_delivery, step_delivery_all_errored));
        messages.push(LlmMessage::Assistant { text: response.text.clone(), tool_calls: response.tool_calls.clone() });
        messages.push(LlmMessage::ToolResults(tool_results));
        // `countToolCallsSinceLastSendMessage`: non-SendMessage calls after
        // the last assistant message that contained a SendMessage.
        let step_sends = response.tool_calls.iter().filter(|c| c.name == SEND_MESSAGE_TOOL_NAME).count();
        if step_sends > 0 {
            non_send_calls_since_send = 0;
            early_reminder_fired_this_streak = false;
        } else {
            non_send_calls_since_send += response.tool_calls.len();
        }
        if result.aborted || end_turn {
            break;
        }
    }
    result.ended_on_silent_tool_calls = shape.ended_on_silent_tool_calls() && !result.aborted;
    Ok(result)
}

/// The per-turn extras of `assembleTurnAction`: the `<automation_status>`
/// reminder and a pending `<agent_profile_update>` block.
fn turn_extras(host: &Arc<HostInner>, handle: &Arc<AgentHandle>, job: &RunJob, compaction_epoch: u64) -> Option<String> {
    if !job.persist {
        return None;
    }
    let mut parts = Vec::new();
    if let Some(reminder) = host.automation_status_reminder(handle, compaction_epoch, job.options.automation_id.as_deref()) {
        parts.push(reminder);
    }
    if let Some(update) = host.take_profile_update_for_turn(handle) {
        parts.push(update);
    }
    if parts.is_empty() { None } else { Some(parts.join("\n\n")) }
}

/// Shape a hidden (or subagent) prompt: extras above the text on
/// silence-allowed turns, below otherwise, then the hidden-prompt markers.
fn shape_hidden_prompt(job: &RunJob, extras: Option<&str>) -> String {
    use gns_core::prompt::{SAND_HIDDEN_PROMPT_MARKER, SAND_TRUSTED_AUTOMATION_PROMPT_MARKER};
    let options = &job.options;
    let mut text = job.prompt.trim().to_owned();
    if let Some(extra) = extras.filter(|e| !e.is_empty()) {
        text = if text.is_empty() {
            extra.to_owned()
        } else if options.is_silence_allowed {
            format!("{extra}\n\n{text}")
        } else {
            format!("{text}\n\n{extra}")
        };
    }
    if options.hidden {
        let trusted = if options.automation_id.is_some() && !options.contains_untrusted_event_text {
            SAND_TRUSTED_AUTOMATION_PROMPT_MARKER
        } else {
            ""
        };
        text = format!("{SAND_HIDDEN_PROMPT_MARKER}{trusted}{text}");
    }
    text
}

/// The text a `SendMessage` call would deliver (type text only).
fn send_message_text(call: &LlmToolCall) -> Option<String> {
    if call.name != SEND_MESSAGE_TOOL_NAME {
        return None;
    }
    let kind = call.arguments.get("type").and_then(|v| v.as_str()).unwrap_or("text");
    if kind != "text" {
        return None;
    }
    call.arguments.get("content").and_then(|v| v.as_str()).map(str::trim).filter(|t| !t.is_empty()).map(str::to_owned)
}

/// Outcome of one tool call.
pub(crate) struct ExecutedCall {
    pub result: LlmToolResult,
    pub effects: Vec<TurnEffect>,
    pub cancelled: bool,
    pub is_error: bool,
}

/// Run one tool call through policies, persistence, events, the per-tool
/// timeout and middleware. Output is clamped to `CHAR_HARD_LIMIT`.
pub(crate) async fn execute_tool_call(
    host: &Arc<HostInner>,
    handle: &Arc<AgentHandle>,
    ctx: &ToolContext,
    tools: &[Arc<dyn Tool>],
    specs: &[ToolSpec],
    call: &LlmToolCall,
    persist: bool,
) -> Result<ExecutedCall, HostError> {
    let run_id = &ctx.run_id;
    host.emit(HostEvent::ToolCall {
        agent_id: handle.id.clone(),
        run_id: run_id.clone(),
        call_id: call.id.clone(),
        name: call.name.clone(),
        args: call.arguments.clone(),
        status: ToolCallStatus::Started,
        summary: None,
    });
    let mut entry = TranscriptEntry::ToolCall {
        id: EntryId::new(),
        call_id: call.id.clone(),
        name: call.name.clone(),
        args: call.arguments.clone(),
        result: None,
        is_error: false,
        timestamp_ms: now_ms(),
        run_id: run_id.clone(),
    };
    if persist {
        handle.append(&entry)?;
    }
    let mut cancelled = false;
    let (content, is_error, summary, effects) = match check_policies(host, handle, ctx, call).await {
        PolicyOutcome::Blocked(text) => (text, true, Some("blocked by policy".to_owned()), Vec::new()),
        PolicyOutcome::Proceed => match tools.iter().find(|t| t.name() == call.name) {
            None => (
                format!(
                    "Unknown tool \"{}\". Available tools: {}.",
                    call.name,
                    specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")
                ),
                true,
                None,
                Vec::new(),
            ),
            Some(tool) => {
                let timeout = tool_execution_timeout(&call.name);
                let outcome = tokio::select! {
                    r = tool.call(ctx, call.arguments.clone()) => Some(r),
                    _ = tokio::time::sleep(timeout) => None,
                };
                match outcome {
                    Some(Ok(output)) => {
                        let (text, _) = truncate_output(&output.content, CHAR_HARD_LIMIT, false);
                        (text, false, output.summary, output.effects)
                    }
                    Some(Err(ToolError::Cancelled)) => {
                        cancelled = true;
                        ("(cancelled)".to_owned(), true, None, Vec::new())
                    }
                    Some(Err(e)) => (truncate_output(&e.to_string(), CHAR_HARD_LIMIT, false).0, true, None, Vec::new()),
                    None => (tool_execution_timeout_message(&call.name, timeout), true, Some("timed out".to_owned()), Vec::new()),
                }
            }
        },
    };
    if let TranscriptEntry::ToolCall { result: r, is_error: e, .. } = &mut entry {
        *r = Some(content.clone());
        *e = is_error;
    }
    if persist {
        handle.update(&entry)?;
    }
    host.emit(HostEvent::ToolCall {
        agent_id: handle.id.clone(),
        run_id: run_id.clone(),
        call_id: call.id.clone(),
        name: call.name.clone(),
        args: call.arguments.clone(),
        status: if is_error { ToolCallStatus::Failed } else { ToolCallStatus::Finished },
        summary,
    });
    for middleware in host.middlewares() {
        middleware.after_tool_call(&handle.id, run_id, &entry).await;
    }
    Ok(ExecutedCall {
        result: LlmToolResult {
            call_id: call.id.clone(),
            name: call.name.clone(),
            content: if is_error { format!("Error: {content}") } else { content },
        },
        effects,
        cancelled,
        is_error,
    })
}

pub(crate) async fn complete_with_retry(
    host: &Arc<HostInner>,
    handle: &Arc<AgentHandle>,
    run_id: &RunId,
    request: LlmRequest,
    cancel: &CancellationToken,
) -> Result<LlmResponse, LlmError> {
    let mut attempt = 0u32;
    loop {
        let agent_id = handle.id.clone();
        let run = run_id.clone();
        let host2 = host.clone();
        let on_delta = move |delta: LlmDelta| match delta {
            LlmDelta::Text(text) => host2.emit(HostEvent::TextDelta { agent_id: agent_id.clone(), run_id: run.clone(), text }),
            LlmDelta::Reasoning(text) => host2.emit(HostEvent::ThinkingDelta { agent_id: agent_id.clone(), run_id: run.clone(), text }),
            #[allow(unreachable_patterns)]
            _ => {}
        };
        match host.llm.complete(request.clone(), cancel.clone(), &on_delta).await {
            Ok(r) => return Ok(r),
            Err(e) if e.is_retryable() && attempt < host.config.llm_retries => {
                attempt += 1;
                let backoff = std::time::Duration::from_millis(500 * 2u64.pow(attempt));
                tracing::warn!(agent = %handle.id, attempt, error = %e, "retrying llm call");
                host.emit(HostEvent::Retrying { agent_id: handle.id.clone(), run_id: run_id.clone(), attempt, error: e.to_string() });
                tokio::select! {
                    _ = cancel.cancelled() => return Err(LlmError::Cancelled),
                    _ = tokio::time::sleep(backoff) => {}
                }
            }
            Err(e) => return Err(e),
        }
    }
}

pub(crate) enum PolicyOutcome {
    Proceed,
    Blocked(String),
}

/// Consult registered policies; run the approval flow when one asks for it.
pub(crate) async fn check_policies(
    host: &Arc<HostInner>,
    handle: &Arc<AgentHandle>,
    ctx: &ToolContext,
    call: &LlmToolCall,
) -> PolicyOutcome {
    for policy in host.policies() {
        match policy.check(ctx, &call.name, &call.arguments).await {
            PolicyDecision::Allow => continue,
            PolicyDecision::Deny(reason) => return PolicyOutcome::Blocked(blocked_tool_result(&reason)),
            PolicyDecision::RequireApproval(reason) => {
                let now = now_ms();
                let request = ApprovalRequest {
                    id: RunId::new().0,
                    agent_id: handle.id.clone(),
                    agent_name: handle.name(),
                    run_id: ctx.run_id.clone(),
                    tool: call.name.clone(),
                    args: call.arguments.clone(),
                    reason: reason.clone(),
                    policy: policy.name().to_owned(),
                    requested_at: now,
                    expires_at: now + (APPROVAL_TTL_SECS * 1000) as i64,
                };
                return match host.request_approval(request, &ctx.cancel).await {
                    ApprovalOutcome::Approved => PolicyOutcome::Proceed,
                    ApprovalOutcome::Denied(why) => PolicyOutcome::Blocked(denied_tool_result(&why)),
                    ApprovalOutcome::TimedOut => PolicyOutcome::Blocked(approval_timed_out_result()),
                    ApprovalOutcome::Cancelled => PolicyOutcome::Blocked("(cancelled)".to_owned()),
                };
            }
            #[allow(unreachable_patterns)]
            _ => continue,
        }
    }
    PolicyOutcome::Proceed
}

/// Result of waiting for a human decision.
#[derive(Debug)]
pub(crate) enum ApprovalOutcome {
    Approved,
    Denied(String),
    TimedOut,
    Cancelled,
}

/// `settleCompletedTurn`: memory (extraction + episodes) recorded
/// synchronously for memorable, non-hidden, non-superseded user turns. The
/// exchange is the user prompt versus the turn's text sends plus its final
/// assistant text.
async fn settle_completed(host: &Arc<HostInner>, handle: &Arc<AgentHandle>, run_id: &RunId, job: &RunJob, result: &RunResult) {
    let options = &job.options;
    let prompt = job.prompt.trim();
    if options.hidden || prompt.is_empty() || !job.persist || result.superseded {
        return;
    }
    if options.source == RunSource::User && job.epoch != handle.current_turn_epoch() {
        return; // `isRunSuperseded`
    }
    if !is_memorable_exchange(prompt) {
        return;
    }
    let mut parts: Vec<String> = handle
        .history()
        .into_iter()
        .filter_map(|e| match e {
            TranscriptEntry::SendMessage { message, run_id: r, .. } if &r == run_id => {
                if message.kind == OutboundKind::Text {
                    message.content.clone()
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect();
    if !result.text.trim().is_empty() {
        parts.push(result.text.trim().to_owned());
    }
    let agent_text = parts.join("\n");
    if host.config.memory_extraction {
        extract_memories(host, handle, prompt, &agent_text).await;
    }
    // Episodes.
    let mut pending: Vec<EpisodeTurn> = handle.db.get_json(kv_keys::EPISODE_PENDING).ok().flatten().unwrap_or_default();
    pending.push(EpisodeTurn {
        ts: now_ms(),
        user: gns_core::text::clamp_block(prompt, EPISODE_TURN_MAX_CHARS),
        agent: gns_core::text::clamp_block(&agent_text, EPISODE_TURN_MAX_CHARS),
    });
    if pending.len() > EPISODE_PENDING_MAX_TURNS {
        let drop = pending.len() - EPISODE_PENDING_MAX_TURNS;
        pending.drain(..drop);
    }
    if pending.len() >= host.config.episode_interval.max(1) {
        let turns = std::mem::take(&mut pending);
        let last_ts = turns.last().map(|t| t.ts).unwrap_or_else(now_ms);
        let _ = handle.db.set_json(kv_keys::EPISODE_PENDING, &pending);
        if let Ok(summary) = host.llm.complete_text(&build_episode_system_prompt(), &build_episode_user_prompt(&turns)).await {
            let summary = summary.trim();
            if !summary.is_empty()
                && summary != MEMORY_EXTRACTION_NONE_SENTINEL
                && host.memory.write_episode_at(&handle.id, summary, last_ts).is_ok()
            {
                host.emit(HostEvent::MemoryWritten {
                    agent_id: handle.id.clone(),
                    scope: "agent".into(),
                    fact: format!("{MEMORY_EPISODE_PREFIX}{summary}"),
                });
            }
        }
        return;
    }
    let _ = handle.db.set_json(kv_keys::EPISODE_PENDING, &pending);
}

/// Post-turn bookkeeping: usage totals, middleware, compaction.
async fn settle_after(
    host: &Arc<HostInner>,
    handle: &Arc<AgentHandle>,
    run_id: &RunId,
    job: &RunJob,
    result: &RunResult,
    _started_at: i64,
) {
    if job.persist {
        let mut totals: UsageTotals = handle.db.get_json(kv_keys::USAGE_TOTALS).ok().flatten().unwrap_or_default();
        totals.add(&result.usage);
        let _ = handle.db.set_json(kv_keys::USAGE_TOTALS, &totals);
        let _ = handle.db.set_json(kv_keys::LAST_TURN_AT, &now_ms());
    }
    for middleware in host.middlewares() {
        middleware.on_turn_settled(&handle.id, run_id, &job.options, result).await;
    }
    if job.persist && !result.aborted {
        crate::compaction::maybe_compact_after_turn(host.clone(), handle.clone(), result.last_prompt_tokens);
    }
    // `endSessionRun`: arm the ack redrive once the agent goes idle.
    if job.persist {
        host.schedule_ack_redrive(&handle.id);
    }
}

/// Per-turn fact extraction, awaited in settle. The existing list the
/// extractor sees is the in-prompt recall plus up to ten archive facts
/// relevant to the exchange; removals only apply to facts in that list.
async fn extract_memories(host: &Arc<HostInner>, handle: &Arc<AgentHandle>, user_message: &str, agent_message: &str) {
    let Ok(recall) = host.memory.recall_all(&handle.id) else { return };
    let in_prompt: Vec<String> = recall.agent.profile.iter().chain(recall.agent.recent.iter()).map(|r| r.content.clone()).collect();
    let archive: Vec<String> =
        host.memory.list_agent_memories(&handle.id, 500).unwrap_or_default().into_iter().map(|r| r.content).collect();
    let mut existing = in_prompt.clone();
    for fact in select_relevant_memories(&archive, &format!("{user_message}\n{agent_message}"), 10) {
        if !existing.contains(&fact) {
            existing.push(fact);
        }
    }
    let Ok(raw) = host
        .llm
        .complete_text(&build_extraction_system_prompt(), &build_extraction_user_prompt(user_message, agent_message, &existing))
        .await
    else {
        return;
    };
    let extraction = parse_memory_extraction(&raw, &existing);
    if extraction.is_empty() {
        return;
    }
    host.apply_memory_extraction(&handle.id, &extraction);
}
