//! `update_state` (`sand-state-tool.ts`): memory / routine / workflow / profile
//! / settings / channel / project / avatar self-edits. Result strings mirror
//! `extensions/memory/agent-state.ts`; soft failures render as `Not saved — …`.

use async_trait::async_trait;
use gns_core::memory::{MemoryScope, MemoryTier};
use gns_core::routine::{
    AutomationRecord, AutomationSpec, Trigger, clamp_automation_name, describe_trigger_for_reply, parse_stored_trigger,
};
use gns_core::*;
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;

/// Which part of the agent's own state to change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum StateTarget {
    Memory,
    Routine,
    Workflow,
    Profile,
    Settings,
    Channel,
    Project,
    Avatar,
}

impl StateTarget {
    fn label(self) -> &'static str {
        match self {
            StateTarget::Memory => "memory",
            StateTarget::Routine => "routine",
            StateTarget::Workflow => "workflow",
            StateTarget::Profile => "profile",
            StateTarget::Settings => "settings",
            StateTarget::Channel => "channel",
            StateTarget::Project => "project",
            StateTarget::Avatar => "avatar",
        }
    }
    /// The actions each target takes, in `OPERATIONS` order.
    fn actions(self) -> &'static [StateAction] {
        use StateAction as A;
        match self {
            StateTarget::Memory => &[A::Write, A::Forget],
            StateTarget::Routine => &[A::Create, A::Update, A::Pause, A::Resume, A::Delete],
            StateTarget::Workflow => &[A::Write, A::Delete],
            StateTarget::Profile => &[A::Set],
            StateTarget::Settings => &[A::Set],
            StateTarget::Channel => &[A::Disconnect],
            StateTarget::Project => &[A::Create, A::Join, A::Leave],
            StateTarget::Avatar => &[A::Set, A::Clear],
        }
    }
}

/// What to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum StateAction {
    Write,
    Forget,
    Create,
    Update,
    Pause,
    Resume,
    Delete,
    Set,
    Disconnect,
    Join,
    Leave,
    Clear,
}

impl StateAction {
    fn label(self) -> &'static str {
        match self {
            StateAction::Write => "write",
            StateAction::Forget => "forget",
            StateAction::Create => "create",
            StateAction::Update => "update",
            StateAction::Pause => "pause",
            StateAction::Resume => "resume",
            StateAction::Delete => "delete",
            StateAction::Set => "set",
            StateAction::Disconnect => "disconnect",
            StateAction::Join => "join",
            StateAction::Leave => "leave",
            StateAction::Clear => "clear",
        }
    }
}

/// `OPERATIONS`: one line per (target, action) for the description.
const OPERATIONS: &[(&str, &str, &str)] = &[
    (
        "memory",
        "write",
        "save a durable fact (fact, tier, optional scope). scope \"agent\" (default) is your own memory; \"user\" is shared user-memory every assistant should know; \"project\" needs project=<slug> and writes your shard in that project. tier \"profile\" is foundational and kept in mind every turn; \"log\" (default) is dated history; \"note\" fades fast. Facts are deduped.",
    ),
    ("memory", "forget", "drop a fact by its EXACT recorded text (fact, same scope/project). Pair with a write for the corrected version."),
    (
        "routine",
        "create",
        "save a standing order (name, prompt, and either schedule or trigger). prompt is what you do each time it fires, written to your future self.",
    ),
    (
        "routine",
        "update",
        "rewrite an existing one in place (id, plus any of name/prompt/schedule/trigger/enabled you mean to change). Omitted fields keep their current values; it keeps its history.",
    ),
    ("routine", "pause", "(id) disarm one the user wants back later."),
    ("routine", "resume", "(id) rearm a paused one."),
    ("routine", "delete", "(id) remove a finite watch as soon as it has done its job."),
    (
        "workflow",
        "write",
        "save or rewrite a reusable skill (name, description, body; id to rewrite). The description is REQUIRED and is what a reader uses to decide whether the skill applies, so write it as \"use this when …\". A workflow has no trigger — a saved task that runs on a schedule is a routine.",
    ),
    ("workflow", "delete", "(id). Cursor-managed skills can't be edited or deleted."),
    ("profile", "set", "your name and/or description. For your picture use target avatar."),
    ("settings", "set", "hidden_from_sidebar, notify_on_updates. Only the fields you pass change."),
    ("channel", "disconnect", "(platform). The connector closes the live connection within a few seconds."),
    (
        "project",
        "create",
        "(project slug, name, optional description). Creates the folder + project.md and joins it; if the slug already exists this is create-is-join.",
    ),
    ("project", "join", "(project slug)."),
    ("project", "leave", "(project slug)."),
    ("avatar", "set", "(path to an image inside your workspace — write/download it first, then install it here)."),
    ("avatar", "clear", "back to the default picture."),
];

/// The tool description (`createSandStateTool`).
pub fn update_state_description() -> String {
    let mut lines = vec![
        "Change your OWN durable state: what you remember (own, shared user, or project), the routines you run, the workflows you save, your profile and settings, which channels you're connected to, which projects you've joined, and your picture. Prefer this over editing those files with the shell — you still use shell tools to read and grep them.".to_owned(),
        String::new(),
        "target + action:".to_owned(),
    ];
    lines.extend(OPERATIONS.iter().map(|(target, action, text)| format!("- {target} {action}: {text}")));
    lines.push(String::new());
    lines.push("Just do it and mention it in passing — don't narrate a save or ask permission for an ordinary one. Creating or changing a ROUTINE may ask the user to confirm, since it's the one change that acts while they're away; if it does, they'll see a card and you'll get their answer back as the tool result.".to_owned());
    lines.join("\n")
}

/// Arguments of `update_state` (`sandUpdateStateParameters`).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateStateArgs {
    /// Which part of your own state to change.
    pub target: StateTarget,
    /// What to do. memory: write | forget. routine: create | update | pause | resume | delete. workflow: write | delete. profile: set. settings: set. channel: disconnect. project: create | join | leave. avatar: set | clear.
    pub action: StateAction,
    /// memory only. The fact, one self-contained sentence. For forget, the EXACT text of the recorded fact (read or grep the relevant memory folder first).
    #[serde(default)]
    pub fact: Option<String>,
    /// memory write only. Defaults to log. Keep profile small.
    #[serde(default)]
    pub tier: Option<MemoryTier>,
    /// memory only. Defaults to agent (your own memory).
    #[serde(default)]
    pub scope: Option<MemoryScope>,
    /// Project slug. Required for memory when scope is "project", and for every project action.
    #[serde(default)]
    pub project: Option<String>,
    /// The routine's folder or the workflow's id. Required for every routine action except create, and for workflow delete. Omit on a workflow write to create a new one.
    #[serde(default)]
    pub id: Option<String>,
    /// routine/workflow/project create: its name. Required on create and on a workflow write; on routine update, omit to keep the current name. profile: your new name.
    #[serde(default)]
    pub name: Option<String>,
    /// routine only. What you should do each time it fires, written to your future self. Write it as an INTENT, not a frozen tool recipe: a connector's schema can change between fires, so describe the goal and let each run look the tool up. Required on create; on update, omit to keep the current prompt.
    #[serde(default)]
    pub prompt: Option<String>,
    /// routine only. Shorthand for a cron trigger — "0 7 * * *", "@daily", "@every 2h" — interpreted in the user's local time. An hour with no minute takes the current minute off the <timestamp>: asked at 1:32, "daily at 2" is "32 2 * * *". Use this OR trigger, never both. On update, omit (with trigger) to keep the current fire condition.
    #[serde(default)]
    pub schedule: Option<String>,
    /// What fires the routine: an object with a "type" of cron ({schedule}), slack ({channel, match}), github ({repo, events, userAllowlist?, ciBranch?}), microsoftTeams, linear, sentry, pagerduty, or group ({listeners: [...]}); a bare array of those is shorthand for the group form (any one member fires the prompt). Prefer an event listener over polling on a cron when the event you care about is one of the listed shapes; never pass both this and the schedule argument.
    #[serde(default)]
    pub trigger: Option<serde_json::Value>,
    /// routine create/update only. On create, defaults to true. On update, omit to leave the current arming alone (use pause/resume to toggle).
    #[serde(default)]
    pub enabled: Option<bool>,
    /// workflow write: REQUIRED. One line on when to use the skill. profile: your new description. project create: optional summary.
    #[serde(default)]
    pub description: Option<String>,
    /// workflow write only. The recipe, in markdown.
    #[serde(default)]
    pub body: Option<String>,
    /// settings set only. Removes your row from the user's sidebar; you stay fully functional and reachable through Cmd-K and the Hidden chats manager.
    #[serde(default)]
    pub hidden_from_sidebar: Option<bool>,
    /// settings set only. The "Notify me about this assistant" toggle.
    #[serde(default)]
    pub notify_on_updates: Option<bool>,
    /// channel disconnect only. The platform to disconnect.
    #[serde(default)]
    pub platform: Option<String>,
    /// avatar set only. Absolute path to an image you already have (write or download it first with Shell, then install it here). png/jpg/webp/gif/svg under 5 MB.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug)]
pub struct UpdateStateTool {
    description: String,
}

impl Default for UpdateStateTool {
    fn default() -> Self {
        Self { description: update_state_description() }
    }
}

impl UpdateStateTool {
    pub fn arc() -> Arc<dyn Tool> {
        Typed::arc(Self::default())
    }
}

fn trimmed(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

/// `'<field>' is required for <target> <action>.`
fn need<'a>(value: Option<&'a str>, field: &str, target: StateTarget, action: StateAction) -> Result<&'a str, ToolError> {
    value.ok_or_else(|| ToolError::input(format!("'{field}' is required for {} {}.", target.label(), action.label())))
}

/// `resolveTrigger`: `schedule` (cron) or `trigger` (event/group), never both.
fn resolve_trigger(
    args: &UpdateStateArgs,
    fallback: Option<&Trigger>,
    target: StateTarget,
    action: StateAction,
) -> Result<Trigger, ToolError> {
    let schedule = trimmed(&args.schedule);
    let trigger = args.trigger.as_ref().filter(|t| !t.is_null());
    if schedule.is_some() && trigger.is_some() {
        return Err(ToolError::input("pass either 'schedule' (a cron routine) or 'trigger' (an event-driven one), never both."));
    }
    if let Some(schedule) = schedule {
        return Ok(Trigger::cron(schedule));
    }
    if let Some(value) = trigger {
        return parse_stored_trigger(value).ok_or_else(|| {
            ToolError::input(
                "that trigger isn't usable — check the channel, repo (one concrete \"owner/name\"), event names, the ciBranch a ci-passed/ci-failed listener needs, and the tenantId plus at least one team id a microsoftTeams trigger needs.",
            )
        });
    }
    match fallback {
        Some(existing) => Ok(existing.clone()),
        None => Err(ToolError::input(format!("'schedule' or 'trigger' is required for {} {}.", target.label(), action.label()))),
    }
}

/// `describeAutomation`: `Saved routine "<name>" (folder <id>) — <trigger>[, paused].`
fn describe_automation(record: &AutomationRecord, verb: &str) -> String {
    format!(
        "{verb} routine \"{}\" (folder {}) — {}{}.",
        record.name,
        record.id,
        describe_trigger_for_reply(record),
        if record.is_enabled { "" } else { ", paused" }
    )
}

/// Map a service outcome to the tool text (`Not saved — …` for soft failures).
fn outcome(result: Result<String, HostError>) -> Result<ToolOutput, ToolError> {
    Ok(ToolOutput::text(render_state_outcome(result)?))
}

#[async_trait]
impl TypedTool for UpdateStateTool {
    type Args = UpdateStateArgs;
    fn name(&self) -> &str {
        UPDATE_STATE_TOOL_NAME
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn availability(&self) -> ToolAvailability {
        ToolAvailability::NotWhenSystemPromptOverridden
    }
    async fn run(&self, ctx: &ToolContext, args: Self::Args) -> Result<ToolOutput, ToolError> {
        use StateAction as A;
        use StateTarget as T;
        let (target, action) = (args.target, args.action);
        if !target.actions().contains(&action) {
            let allowed = target.actions().iter().map(|a| a.label()).collect::<Vec<_>>().join(" | ");
            return Err(ToolError::input(format!(
                "'{}' is not an action on {}. {} takes: {allowed}.",
                action.label(),
                target.label(),
                target.label()
            )));
        }
        let scope = args.scope.unwrap_or(MemoryScope::Agent);
        let project = trimmed(&args.project);
        match (target, action) {
            (T::Memory, A::Write) => {
                let fact = need(trimmed(&args.fact), "fact", target, action)?;
                let tier = args.tier.unwrap_or(MemoryTier::Log);
                let result = ctx.services.memory().write(&ctx.agent_id, fact, tier, scope, project);
                if result.is_ok() {
                    ctx.services.emit(HostEvent::MemoryWritten {
                        agent_id: ctx.agent_id.clone(),
                        scope: scope.label().to_owned(),
                        fact: fact.to_owned(),
                    });
                }
                Ok(outcome(result)?.with_summary(scope.label()))
            }
            (T::Memory, A::Forget) => {
                let fact = need(trimmed(&args.fact), "fact", target, action)?;
                outcome(ctx.services.memory().forget(&ctx.agent_id, fact, scope, project))
            }
            (T::Routine, A::Create) | (T::Routine, A::Update) => {
                let is_update = action == A::Update;
                let id = if is_update { Some(need(trimmed(&args.id), "id", target, action)?) } else { None };
                let existing = match id {
                    Some(id) => Some(ctx.services.routines().list(&ctx.agent_id)?.into_iter().find(|r| r.id == id).ok_or_else(|| {
                        ToolError::input(format!("no routine with folder \"{id}\" exists — list the automations folder, then pass its id."))
                    })?),
                    None => None,
                };
                let name = match trimmed(&args.name) {
                    Some(name) => clamp_automation_name(name),
                    None => need(existing.as_ref().map(|e| e.name.as_str()), "name", target, action)?.to_owned(),
                };
                let prompt = match trimmed(&args.prompt) {
                    Some(prompt) => prompt.to_owned(),
                    None => need(existing.as_ref().map(|e| e.prompt.as_str()), "prompt", target, action)?.to_owned(),
                };
                let trigger = resolve_trigger(&args, existing.as_ref().map(|e| &e.trigger), target, action)?;
                let is_enabled = match args.enabled {
                    Some(enabled) => Some(enabled),
                    None if is_update => None,
                    None => Some(true),
                };
                let spec = AutomationSpec { name, prompt, trigger, is_enabled };
                let result = match id {
                    Some(id) => ctx.services.routines().update(&ctx.agent_id, id, spec).map(|r| describe_automation(&r, "Updated")),
                    None => ctx.services.routines().create(&ctx.agent_id, spec).map(|r| describe_automation(&r, "Saved")),
                };
                let summary = id.map(str::to_owned);
                let out = outcome(result)?;
                Ok(match summary {
                    Some(s) => out.with_summary(s),
                    None => out,
                })
            }
            (T::Routine, A::Pause) | (T::Routine, A::Resume) => {
                let id = need(trimmed(&args.id), "id", target, action)?;
                let enabled = action == A::Resume;
                let result = ctx
                    .services
                    .routines()
                    .set_enabled(&ctx.agent_id, id, enabled)
                    .map(|r| format!("{} routine \"{}\" (folder {}).", if enabled { "Resumed" } else { "Paused" }, r.name, r.id));
                outcome(result)
            }
            (T::Routine, A::Delete) => {
                let id = need(trimmed(&args.id), "id", target, action)?;
                let name = ctx
                    .services
                    .routines()
                    .list(&ctx.agent_id)?
                    .into_iter()
                    .find(|r| r.id == id)
                    .map(|r| r.name)
                    .unwrap_or_else(|| id.to_owned());
                let result =
                    ctx.services.routines().delete(&ctx.agent_id, id).map(|()| format!("Deleted routine \"{name}\" (folder {id})."));
                outcome(result)
            }
            (T::Workflow, A::Write) => {
                need(trimmed(&args.name), "name", target, action)?;
                need(trimmed(&args.body), "body", target, action)?;
                need(trimmed(&args.description), "description", target, action)?;
                Ok(ToolOutput::text("Not saved — workflows are not available in this host."))
            }
            (T::Workflow, A::Delete) => {
                need(trimmed(&args.id), "id", target, action)?;
                Ok(ToolOutput::text("Not saved — workflows are not available in this host."))
            }
            (T::Profile, A::Set) => {
                if args.name.is_none() && args.description.is_none() {
                    return Ok(ToolOutput::text("Not saved — nothing to change — pass at least one of name or description."));
                }
                if args.name.as_deref().is_some_and(|n| n.trim().is_empty()) {
                    return Ok(ToolOutput::text("Not saved — a blank name is not allowed."));
                }
                let changed: Vec<&str> =
                    [args.name.as_ref().map(|_| "name"), args.description.as_ref().map(|_| "description")].into_iter().flatten().collect();
                let patch = ProfilePatch {
                    name: args.name.as_deref().map(|s| s.trim().to_owned()),
                    description: args.description.as_deref().map(|s| s.trim().to_owned()),
                    title: None,
                };
                match ctx.services.update_agent(ctx.agent_id.as_str(), patch).await? {
                    Some(_) => Ok(ToolOutput::text(format!("Updated your {}.", changed.join(", ")))),
                    None => Err(ToolError::failed("could not update your profile")),
                }
            }
            (T::Settings, A::Set) => {
                let patch =
                    SettingsPatch { notify_on_agent_updates: args.notify_on_updates, hidden_from_sidebar: args.hidden_from_sidebar };
                let changed: Vec<&str> =
                    [args.hidden_from_sidebar.map(|_| "hiddenFromSidebar"), args.notify_on_updates.map(|_| "notifyOnAgentUpdates")]
                        .into_iter()
                        .flatten()
                        .collect();
                if changed.is_empty() {
                    return Ok(ToolOutput::text("Not saved — nothing to change — pass at least one setting field."));
                }
                ctx.services.update_settings(&ctx.agent_id, patch)?;
                Ok(ToolOutput::text(format!("Updated your settings: {}.", changed.join(", "))))
            }
            (T::Channel, A::Disconnect) => {
                need(trimmed(&args.platform), "platform", target, action)?;
                Ok(ToolOutput::text("Not saved — channels are not available in this host."))
            }
            (T::Project, A::Create) => {
                let slug = need(project, "project", target, action)?;
                let name = need(trimmed(&args.name), "name", target, action)?;
                let out = outcome(ctx.services.create_project(&ctx.agent_id, slug, name, args.description.as_deref()))?;
                Ok(out.with_summary(slug))
            }
            (T::Project, A::Join) => {
                let slug = need(project, "project", target, action)?;
                Ok(outcome(ctx.services.join_project(&ctx.agent_id, slug))?.with_summary(slug))
            }
            (T::Project, A::Leave) => {
                let slug = need(project, "project", target, action)?;
                Ok(outcome(ctx.services.leave_project(&ctx.agent_id, slug))?.with_summary(slug))
            }
            (T::Avatar, A::Set) => {
                let path = need(trimmed(&args.path), "path", target, action)?;
                Ok(outcome(ctx.services.set_avatar(&ctx.agent_id, path))?.with_summary("avatar"))
            }
            (T::Avatar, A::Clear) => Ok(outcome(ctx.services.clear_avatar(&ctx.agent_id))?.with_summary("avatar")),
            _ => unreachable!("route validated above"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(target: StateTarget, action: StateAction) -> UpdateStateArgs {
        UpdateStateArgs {
            target,
            action,
            fact: None,
            tier: None,
            scope: None,
            project: None,
            id: None,
            name: None,
            prompt: None,
            schedule: None,
            trigger: None,
            enabled: None,
            description: None,
            body: None,
            hidden_from_sidebar: None,
            notify_on_updates: None,
            platform: None,
            path: None,
        }
    }

    #[test]
    fn description_lists_every_operation_in_order() {
        let text = update_state_description();
        assert!(text.starts_with("Change your OWN durable state:"));
        assert!(text.contains("\n- memory write: save a durable fact"));
        assert!(text.contains("\n- avatar clear: back to the default picture.\n\nJust do it and mention it in passing"));
        let schema = schema_for_args::<UpdateStateArgs>();
        let targets: Vec<&str> = schema["properties"]["target"]["enum"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(targets, ["memory", "routine", "workflow", "profile", "settings", "channel", "project", "avatar"]);
        assert!(schema["properties"].get("notify_on_updates").is_some());
        assert!(schema["properties"].get("tools").is_none() && schema["properties"].get("mcp").is_none());
    }

    #[test]
    fn trigger_resolution_follows_the_original() {
        let mut a = args(StateTarget::Routine, StateAction::Create);
        let err = resolve_trigger(&a, None, StateTarget::Routine, StateAction::Create).unwrap_err();
        assert_eq!(err.to_string(), "'schedule' or 'trigger' is required for routine create.");
        a.schedule = Some("@daily".into());
        assert_eq!(resolve_trigger(&a, None, StateTarget::Routine, StateAction::Create).unwrap(), Trigger::cron("@daily"));
        a.trigger = Some(serde_json::json!({"type": "cron", "schedule": "@hourly"}));
        assert!(resolve_trigger(&a, None, StateTarget::Routine, StateAction::Create).unwrap_err().to_string().contains("never both"));
        a.schedule = None;
        assert_eq!(resolve_trigger(&a, None, StateTarget::Routine, StateAction::Create).unwrap(), Trigger::cron("@hourly"));
        a.trigger = Some(serde_json::json!({"type": "nonsense"}));
        assert!(
            resolve_trigger(&a, None, StateTarget::Routine, StateAction::Create)
                .unwrap_err()
                .to_string()
                .contains("that trigger isn't usable")
        );
        a.trigger = None;
        let existing = Trigger::cron("@weekly");
        assert_eq!(resolve_trigger(&a, Some(&existing), StateTarget::Routine, StateAction::Update).unwrap(), existing);
    }
}
