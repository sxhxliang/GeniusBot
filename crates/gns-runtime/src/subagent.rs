//! Ephemeral subagents: a bounded step loop in the parent's workspace that
//! reports back in text. No transcript, no user channel, no nesting.

use crate::agent::AgentHandle;
use crate::host::HostInner;
use crate::runner::{complete_with_retry, execute_tool_call};
use gns_core::prompt::build_subagent_system_prompt;
use gns_core::*;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(crate) async fn run_subagent(
    host: &Arc<HostInner>,
    parent: &Arc<AgentHandle>,
    parent_run: &RunId,
    spec: SubagentSpec,
    cancel: CancellationToken,
) -> SubagentResult {
    let run_id = RunId::new();
    host.emit(HostEvent::SubagentStarted { parent_id: parent.id.clone(), run_id: run_id.clone(), label: spec.label.clone() });
    host.emit(HostEvent::RunStarted {
        agent_id: parent.id.clone(),
        run_id: run_id.clone(),
        lane: Lane::Agent,
        source: RunSource::Subagent,
    });
    let mut result = run_inner(host, parent, &run_id, &spec, &cancel).await;
    if let Some(e) = result.error.clone() {
        host.emit(HostEvent::Error {
            agent_id: Some(parent.id.clone()),
            run_id: Some(run_id.clone()),
            message: format!("subagent {}: {e}", spec.label),
        });
    }
    let mut totals: UsageTotals = parent.db.get_json(kv_keys::USAGE_TOTALS).ok().flatten().unwrap_or_default();
    totals.add(&result.usage);
    let _ = parent.db.set_json(kv_keys::USAGE_TOTALS, &totals);
    result.aborted |= cancel.is_cancelled();
    host.emit(HostEvent::SubagentEnded {
        parent_id: parent.id.clone(),
        run_id: run_id.clone(),
        label: spec.label.clone(),
        steps: result.steps,
        aborted: result.aborted,
    });
    let _ = parent_run;
    result
}

async fn run_inner(
    host: &Arc<HostInner>,
    parent: &Arc<AgentHandle>,
    run_id: &RunId,
    spec: &SubagentSpec,
    cancel: &CancellationToken,
) -> SubagentResult {
    let system = build_subagent_system_prompt(&parent.name(), &parent.workspace_dir.display().to_string(), spec.readonly);
    let tools: Vec<Arc<dyn Tool>> =
        host.tools_for(RunSource::Subagent, false, false).into_iter().filter(|t| !spec.readonly || t.name() == READ_TOOL_NAME).collect();
    let specs: Vec<ToolSpec> = tools.iter().map(|t| tool_spec(t.as_ref())).collect();
    let ctx = ToolContext {
        agent_id: parent.id.clone(),
        agent_name: format!("{} (subagent: {})", parent.name(), spec.label),
        run_id: run_id.clone(),
        lane: Lane::Agent,
        source: RunSource::Subagent,
        workspace_dir: parent.workspace_dir.clone(),
        data_dir: parent.data_dir.clone(),
        agents_root: host.layout.agents_root(),
        cancel: cancel.clone(),
        services: host.services(),
        group_id: None,
    };
    let max_steps = spec.max_steps.unwrap_or(host.config.subagent_max_steps).max(1);
    let mut messages = vec![LlmMessage::user(spec.task.clone())];
    let mut result = SubagentResult::default();
    let mut step = 0usize;
    loop {
        if step >= max_steps {
            result.error = Some(format!("subagent exceeded {max_steps} steps"));
            break;
        }
        if cancel.is_cancelled() {
            result.aborted = true;
            break;
        }
        step += 1;
        result.steps = step;
        let request =
            LlmRequest { system: system.clone(), messages: messages.clone(), tools: specs.clone(), options: LlmOptions::default() };
        let response = match complete_with_retry(host, parent, run_id, request, cancel).await {
            Ok(r) => r,
            Err(LlmError::Cancelled) => {
                result.aborted = true;
                break;
            }
            Err(e) => {
                result.error = Some(e.to_string());
                break;
            }
        };
        result.usage.add(&response.usage);
        if let Some(text) = response.text.as_ref().filter(|t| !t.trim().is_empty()) {
            result.report = text.clone();
        }
        if response.tool_calls.is_empty() {
            break;
        }
        let mut tool_results = Vec::new();
        for call in &response.tool_calls {
            if cancel.is_cancelled() {
                result.aborted = true;
                break;
            }
            match execute_tool_call(host, parent, &ctx, &tools, &specs, call, false).await {
                Ok(executed) => {
                    if executed.cancelled {
                        result.aborted = true;
                    }
                    tool_results.push(executed.result);
                }
                Err(e) => {
                    result.error = Some(e.to_string());
                    break;
                }
            }
        }
        messages.push(LlmMessage::Assistant { text: response.text.clone(), tool_calls: response.tool_calls.clone() });
        messages.push(LlmMessage::ToolResults(tool_results));
        if result.aborted || result.error.is_some() {
            break;
        }
    }
    result
}
