//! `RunSubagent`: delegate a bounded task to an ephemeral subagent.

use async_trait::async_trait;
use gns_core::*;
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;

/// Arguments of `RunSubagent`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunSubagentArgs {
    /// The complete, self-contained task: what to do, where, and what to report back. The subagent has no memory of this chat.
    pub task: String,
    /// Short label for the activity feed (e.g. "audit tests").
    #[serde(default)]
    pub label: Option<String>,
    /// Read-only subagent: it can only Read files (no Shell/Write/Edit). Use for research and audits.
    #[serde(default)]
    pub readonly: Option<bool>,
    /// Step budget (default 20, max 60).
    #[serde(default)]
    pub max_steps: Option<u32>,
}

#[derive(Debug, Default)]
pub struct RunSubagentTool;

impl RunSubagentTool {
    pub fn arc() -> Arc<dyn Tool> {
        Typed::arc(Self)
    }
}

#[async_trait]
impl TypedTool for RunSubagentTool {
    type Args = RunSubagentArgs;
    fn name(&self) -> &str {
        RUN_SUBAGENT_TOOL_NAME
    }
    fn description(&self) -> &str {
        "Delegate a bounded, self-contained task to an ephemeral subagent that works in your workspace with Read/Write/Edit/Shell and reports back in text. The call blocks until it finishes and returns its report; the subagent cannot talk to the user, message agents, or spawn subagents, and it knows nothing but the task you write. Use it to keep large exploration, audits or mechanical multi-file work out of your own context; write the task as if briefing a capable contractor (goal, constraints, exact paths, what the report must contain). Prefer readonly=true for research."
    }
    fn availability(&self) -> ToolAvailability {
        ToolAvailability::Always
    }
    async fn run(&self, ctx: &ToolContext, args: Self::Args) -> Result<ToolOutput, ToolError> {
        let task = args.task.trim();
        if task.is_empty() {
            return Err(ToolError::input("task is required"));
        }
        let label = args
            .label
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| gns_core::text::clamp_line(task, 40));
        let spec = SubagentSpec {
            task: task.to_owned(),
            label: label.clone(),
            readonly: args.readonly.unwrap_or(false),
            max_steps: args.max_steps.map(|n| (n as usize).clamp(1, 60)),
        };
        let result = ctx.services.run_subagent(&ctx.agent_id, &ctx.run_id, spec).await?;
        if result.aborted {
            return Err(ToolError::Cancelled);
        }
        let mut out = format!("Subagent \"{label}\" finished in {} step(s).", result.steps);
        if let Some(e) = &result.error {
            out.push_str(&format!(" It stopped with an error: {e}."));
        }
        out.push_str("\nReport:\n");
        out.push_str(if result.report.trim().is_empty() { "(the subagent produced no report)" } else { result.report.trim() });
        Ok(ToolOutput::text(out).with_summary(format!("subagent: {label}")))
    }
}
