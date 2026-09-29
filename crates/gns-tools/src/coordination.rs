//! Model-facing task and artifact operations. The host owns routing and file IO.
use async_trait::async_trait;
use gns_core::*;
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FetchArtifactArgs {
    pub artifact_id: String,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTaskArgs {
    pub task_id: String,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateTaskArgs {
    pub task_id: String,
    /// running for progress, blocked when input or resumption is needed.
    pub status: TaskStatus,
    pub summary: String,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CompleteTaskArgs {
    pub task_id: String,
    /// completed or failed; a successful file task must include the entire output files.
    pub status: TaskStatus,
    pub summary: String,
    #[serde(default)]
    pub verification: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub artifact_ids: Vec<String>,
}

fn task_output(task: &TaskRecord) -> Result<ToolOutput, ToolError> {
    Ok(ToolOutput::text(serde_json::to_string(task).map_err(|e| ToolError::failed(e.to_string()))?)
        .with_summary(format!("task {}: {:?}", task.id, task.status)))
}

macro_rules! task_tool {
    ($name:ident, $args:ty, $description:literal, $ctx:ident, $a:ident, $body:block) => {
        #[derive(Debug, Default)] pub struct $name;
        impl $name { pub fn arc() -> Arc<dyn Tool> { Typed::arc(Self) } }
        #[async_trait] impl TypedTool for $name {
            type Args = $args;
            fn name(&self) -> &str { stringify!($name) }
            fn description(&self) -> &str { $description }
            async fn run(&self, $ctx: &ToolContext, $a: Self::Args) -> Result<ToolOutput, ToolError> $body
        }
    }
}

task_tool!(
    DelegateTask,
    DelegateTaskRequest,
    "Delegate work to ONE teammate and return immediately with a durable task_id. Use this instead of SendToAgent when you need work and results back. Include the actual input files with files or artifact_ids; other chats are not shared. Set require_files=true for code, reports, datasets and other file deliverables. The host remembers this conversation: completed files appear here automatically, then you are woken to review them. Do not poll or wait in this turn. Single-level delegation only.",
    ctx,
    args,
    { task_output(&ctx.services.delegate_task(&ctx.agent_id, &ctx.run_id, args).await?) }
);
task_tool!(
    FetchArtifact,
    FetchArtifactArgs,
    "Obtain an authorized file by artifact_id in your own workspace. Returns a local absolute path, not its contents. Then use Read or Shell when you need to inspect it. To forward a file use artifact_ids on SendMessage or SendToAgent; no read or reconstruction is needed.",
    ctx,
    args,
    {
        let path = ctx.services.fetch_artifact(&ctx.agent_id, &args.artifact_id).await?;
        Ok(ToolOutput::text(format!("File available at {path}. Use Read or Shell if needed.")).with_summary(path))
    }
);
task_tool!(
    GetTask,
    GetTaskArgs,
    "Inspect a known delegated task, its status and output artifact references. Results are pushed automatically: do not repeatedly poll.",
    ctx,
    args,
    { task_output(&ctx.services.get_task(&ctx.agent_id, &args.task_id)?) }
);
task_tool!(
    UpdateTask,
    UpdateTaskArgs,
    "Report meaningful progress with status=running, or status=blocked plus the exact missing input. A blocked task stops this turn and can be resumed from its task card. This updates the original conversation, not your private chat. Only the assigned executor can update a task.",
    ctx,
    args,
    {
        let task = ctx.services.update_task(&ctx.agent_id, &ctx.run_id, &args.task_id, args.status, args.summary).await?;
        let mut out = task_output(&task)?;
        if task.status == TaskStatus::Blocked {
            out.effects.push(TurnEffect::EndTurn);
        }
        Ok(out)
    }
);
task_tool!(
    CompleteTask,
    CompleteTaskArgs,
    "Submit a delegated task exactly once with status=completed or failed, a summary and verification notes. Supply actual full output file paths in files (or existing artifact_ids), including tests and logs when relevant. The host snapshots the bytes, delivers downloadable files to the original conversation and wakes the requester. Do NOT paste file contents, invent sandbox links or separately SendMessage/SendToAgent a completion. A successful file task requires files. This ends the task turn.",
    ctx,
    args,
    {
        let task = ctx
            .services
            .complete_task(
                &ctx.agent_id,
                &ctx.run_id,
                &args.task_id,
                CompleteTaskRequest {
                    status: args.status,
                    summary: args.summary,
                    verification: args.verification,
                    files: args.files,
                    artifact_ids: args.artifact_ids,
                },
            )
            .await?;
        Ok(task_output(&task)?.with_effect(TurnEffect::EndTurn))
    }
);
