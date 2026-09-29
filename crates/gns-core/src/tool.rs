//! The extensible tool system. Implement [`Tool`] (dynamic JSON args) or
//! [`TypedTool`] (typed args with a derived JSON schema) and register the tool
//! on the host.

use crate::error::ToolError;
use crate::ids::{AgentId, GroupId, RunId};
use crate::run::{Lane, RunSource};
use crate::services::HostServices;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Which kinds of turns a tool is offered in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolAvailability {
    /// Every turn of a full agent (not offered to subagents).
    Always,
    /// Everything except group room turns (not offered to subagents).
    NotInGroupRoom,
    /// Only turns the user opened directly.
    UserTurnsOnly,
    /// Local, side-effect-contained work: every turn, subagents included.
    Local,
    /// Every turn of a full agent except when the run overrides the system
    /// prompt (the original hides `update_state` then); never for subagents.
    NotWhenSystemPromptOverridden,
}

/// Side effects a tool can signal to the runner.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TurnEffect {
    /// A message reached the user (or the room).
    MessageSent,
    /// A widget was posted; the turn must end and wait for the user.
    AwaitingUserSelection,
    /// The tool asks the runner to end the turn after this step.
    EndTurn,
    /// A `ReactToMessage` reached the user; counts as a delivery.
    Reacted,
}

/// What a tool returns: text for the model plus optional effects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub effects: Vec<TurnEffect>,
    /// Short activity summary for UIs (e.g. `target: <id>`).
    pub summary: Option<String>,
}

impl ToolOutput {
    /// Text-only output.
    pub fn text(content: impl Into<String>) -> Self {
        Self { content: content.into(), ..Default::default() }
    }
    /// Attach an effect.
    pub fn with_effect(mut self, effect: TurnEffect) -> Self {
        self.effects.push(effect);
        self
    }
    /// Attach a summary.
    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }
}

/// Everything a tool may need about the turn it runs in.
#[derive(Clone)]
pub struct ToolContext {
    pub agent_id: AgentId,
    pub agent_name: String,
    pub run_id: RunId,
    pub lane: Lane,
    pub source: RunSource,
    /// The agent's sandboxed working directory (Shell/Read/Write root).
    pub workspace_dir: PathBuf,
    /// The agent's data directory (profile, memory, automations, store.db).
    pub data_dir: PathBuf,
    /// Root of all agents (for file-based discovery).
    pub agents_root: PathBuf,
    pub cancel: CancellationToken,
    pub services: Arc<dyn HostServices>,
    pub group_id: Option<GroupId>,
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("agent_id", &self.agent_id)
            .field("run_id", &self.run_id)
            .field("lane", &self.lane)
            .field("source", &self.source)
            .field("workspace_dir", &self.workspace_dir)
            .field("group_id", &self.group_id)
            .finish_non_exhaustive()
    }
}

impl ToolContext {
    /// True when this is a group room turn.
    pub fn is_group_room(&self) -> bool {
        self.group_id.is_some()
    }
}

/// A tool callable by the model.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Stable tool name exposed to the model.
    fn name(&self) -> &str;
    /// Description shown to the model.
    fn description(&self) -> &str;
    /// JSON schema of the arguments object.
    fn parameters_schema(&self) -> serde_json::Value;
    /// When this tool is offered.
    fn availability(&self) -> ToolAvailability {
        ToolAvailability::Always
    }
    /// Execute the tool.
    async fn call(&self, ctx: &ToolContext, args: serde_json::Value) -> Result<ToolOutput, ToolError>;
}

/// A tool with typed arguments; wrap it in [`Typed`] to obtain a [`Tool`].
#[async_trait]
pub trait TypedTool: Send + Sync {
    /// Argument type; its schema is derived with `schemars`.
    type Args: JsonSchema + DeserializeOwned + Send;
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn availability(&self) -> ToolAvailability {
        ToolAvailability::Always
    }
    async fn run(&self, ctx: &ToolContext, args: Self::Args) -> Result<ToolOutput, ToolError>;
}

/// Adapter turning a [`TypedTool`] into a dynamic [`Tool`].
#[derive(Debug)]
pub struct Typed<T>(pub T);

impl<T: TypedTool> Typed<T> {
    /// Wrap into an `Arc<dyn Tool>`.
    pub fn arc(tool: T) -> Arc<dyn Tool>
    where
        T: 'static,
    {
        Arc::new(Typed(tool))
    }
}

#[async_trait]
impl<T: TypedTool> Tool for Typed<T> {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn description(&self) -> &str {
        self.0.description()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema_for_args::<T::Args>()
    }
    fn availability(&self) -> ToolAvailability {
        self.0.availability()
    }
    async fn call(&self, ctx: &ToolContext, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
        let parsed: T::Args = serde_json::from_value(args).map_err(|e| ToolError::input(format!("{}: {e}", self.0.name())))?;
        self.0.run(ctx, parsed).await
    }
}

/// Derive a parameters schema for `A` in the conservative subset every
/// OpenAI-compatible vendor accepts (see [`normalize_tool_schema`]).
pub fn schema_for_args<A: JsonSchema>() -> serde_json::Value {
    let schema = schemars::schema_for!(A);
    let mut value = serde_json::to_value(schema).unwrap_or_else(|_| serde_json::json!({"type": "object"}));
    normalize_tool_schema(&mut value);
    value
}

/// Rewrite a JSON schema into the flat dialect tool-calling APIs agree on.
/// Some vendors (Moonshot/Kimi among them) reject `$ref`, `anyOf`/`oneOf`
/// and `type: [.., "null"]`, so:
///
/// * `$ref` pointers into `$defs`/`definitions` are inlined and the
///   definitions dropped;
/// * `anyOf`/`oneOf` made only of string enums/consts collapse into one
///   `enum`; a `[X, {type: null}]` pair collapses into `X` (optionality is
///   already expressed by `required`);
/// * `type: [T, "null"]` becomes `type: T`;
/// * `default: null`, `$schema` and `title` are removed;
/// * the root always carries `type: object`.
pub fn normalize_tool_schema(schema: &mut serde_json::Value) {
    use serde_json::{Map, Value};

    fn resolve_ref<'a>(defs: &'a Map<String, Value>, reference: &str) -> Option<&'a Value> {
        let name = reference.rsplit('/').next()?;
        defs.get(name)
    }

    fn walk(node: &mut Value, defs: &Map<String, Value>, depth: usize) {
        if depth > 32 {
            return;
        }
        let Some(obj) = node.as_object_mut() else {
            if let Some(items) = node.as_array_mut() {
                for item in items {
                    walk(item, defs, depth + 1);
                }
            }
            return;
        };
        // Inline $ref, keeping sibling keys (description overrides).
        if let Some(Value::String(reference)) = obj.remove("$ref")
            && let Some(target) = resolve_ref(defs, &reference).cloned()
        {
            let mut merged = target.as_object().cloned().unwrap_or_default();
            for (k, v) in std::mem::take(obj) {
                merged.insert(k, v);
            }
            *obj = merged;
        }
        obj.remove("$schema");
        obj.remove("title");
        obj.remove("$defs");
        obj.remove("definitions");
        if matches!(obj.get("default"), Some(Value::Null)) {
            obj.remove("default");
        }
        // type: [T, "null"] → T
        if let Some(Value::Array(types)) = obj.get("type") {
            let non_null: Vec<Value> = types.iter().filter(|t| t != &&Value::String("null".into())).cloned().collect();
            match non_null.len() {
                1 => {
                    obj.insert("type".into(), non_null[0].clone());
                }
                0 => {
                    obj.remove("type");
                }
                _ => {}
            }
        }
        // anyOf / oneOf collapsing.
        for key in ["anyOf", "oneOf"] {
            let Some(Value::Array(mut branches)) = obj.remove(key) else { continue };
            for branch in &mut branches {
                walk(branch, defs, depth + 1);
            }
            branches.retain(|b| !matches!(b.get("type"), Some(Value::String(t)) if t == "null"));
            let all_string_enums =
                !branches.is_empty() && branches.iter().all(|b| b.get("enum").is_some_and(|e| e.is_array()) || b.get("const").is_some());
            if all_string_enums {
                let mut values = Vec::new();
                for b in &branches {
                    if let Some(Value::Array(e)) = b.get("enum") {
                        values.extend(e.iter().cloned());
                    } else if let Some(c) = b.get("const") {
                        values.push(c.clone());
                    }
                }
                obj.insert("type".into(), Value::String("string".into()));
                obj.insert("enum".into(), Value::Array(values));
            } else if branches.len() == 1 {
                let branch = branches.remove(0);
                if let Some(branch_obj) = branch.as_object() {
                    for (k, v) in branch_obj {
                        obj.entry(k.clone()).or_insert(v.clone());
                    }
                }
            } else if !branches.is_empty() {
                obj.insert(key.into(), Value::Array(branches));
            }
        }
        // Recurse.
        if let Some(Value::Object(props)) = obj.get_mut("properties") {
            for prop in props.values_mut() {
                walk(prop, defs, depth + 1);
            }
        }
        for key in ["items", "additionalProperties"] {
            if let Some(child) = obj.get_mut(key)
                && child.is_object()
            {
                walk(child, defs, depth + 1);
            }
        }
        if let Some(Value::Array(children)) = obj.get_mut("prefixItems") {
            for child in children {
                walk(child, defs, depth + 1);
            }
        }
    }

    let defs: Map<String, Value> = schema
        .as_object()
        .and_then(|o| o.get("$defs").or_else(|| o.get("definitions")))
        .and_then(|d| d.as_object())
        .cloned()
        .unwrap_or_default();
    walk(schema, &defs, 0);
    if let Some(obj) = schema.as_object_mut() {
        if !obj.contains_key("type") {
            obj.insert("type".into(), Value::String("object".into()));
        }
        if !obj.contains_key("properties") {
            obj.insert("properties".into(), Value::Object(Map::new()));
        }
    }
}

/// Convert a tool into the spec sent to the model.
pub fn tool_spec(tool: &dyn Tool) -> crate::llm::ToolSpec {
    crate::llm::ToolSpec { name: tool.name().to_owned(), description: tool.description().to_owned(), parameters: tool.parameters_schema() }
}

/// Filter helper: is `tool` offered for a turn of this shape?
///
/// Mirrors `turn-toolset.ts`: only subagents get a reduced (`Local`) set; a
/// local group-room turn sees the full main-turn tool set, and
/// [`ToolAvailability::NotWhenSystemPromptOverridden`] tools disappear when
/// the run overrides the system prompt.
pub fn tool_available(tool: &dyn Tool, source: RunSource, in_group_room: bool, system_prompt_overridden: bool) -> bool {
    let subagent = matches!(source, RunSource::Subagent);
    match tool.availability() {
        ToolAvailability::Always => !subagent,
        ToolAvailability::NotInGroupRoom => !in_group_room && !subagent,
        ToolAvailability::UserTurnsOnly => matches!(source, RunSource::User) && !in_group_room,
        ToolAvailability::Local => true,
        ToolAvailability::NotWhenSystemPromptOverridden => !subagent && !system_prompt_overridden,
        #[allow(unreachable_patterns)]
        _ => !subagent,
    }
}

/// Per-tool-call execution timeout (`tool-execution-timeout.ts`): subagent
/// style tools get the long tier, everything else the short tier. When the
/// call carries `block_until_ms`, use [`tool_execution_timeout_for_args`]
/// so the guard grows with the requested wait.
pub fn tool_execution_timeout(tool_name: &str) -> std::time::Duration {
    tool_execution_timeout_for_args(tool_name, None)
}

/// [`tool_execution_timeout`] taking the call's `block_until_ms` (if any) into
/// account: `suggested = block + 60s`, snapped up to the next tier, minus a
/// 60 s headroom, but never below `block + 30s`.
pub fn tool_execution_timeout_for_args(tool_name: &str, block_until_ms: Option<u64>) -> std::time::Duration {
    use crate::consts::*;
    const TIMEOUT_BUFFER_MS: u64 = 60 * 1_000;
    const GUARD_HEADROOM_MS: u64 = 60 * 1_000;
    const GUARD_BLOCK_GRACE_MS: u64 = 30 * 1_000;
    let lower = tool_name.to_ascii_lowercase();
    let suggested = if LONG_RUNNING_TOOL_NAMES.contains(&lower.as_str()) {
        LONG_TOOL_TIMEOUT_MS
    } else {
        block_until_ms.map(|b| b + TIMEOUT_BUFFER_MS).unwrap_or(SHORT_TOOL_TIMEOUT_MS)
    };
    let tiers =
        [EXTRA_SHORT_TOOL_TIMEOUT_MS, SHORT_TOOL_TIMEOUT_MS, MEDIUM_TOOL_TIMEOUT_MS, LONG_TOOL_TIMEOUT_MS, EXTRA_LONG_TOOL_TIMEOUT_MS];
    let tier = tiers.into_iter().find(|t| *t >= suggested).unwrap_or(EXTRA_LONG_TOOL_TIMEOUT_MS);
    let headroom = tier - GUARD_HEADROOM_MS;
    let guard = match block_until_ms {
        None => headroom,
        Some(block) => {
            let requested = headroom.max(block + GUARD_BLOCK_GRACE_MS);
            if requested >= tier { headroom } else { requested }
        }
    };
    std::time::Duration::from_millis(guard)
}

/// Read `block_until_ms` out of a tool call's arguments (non-negative finite number).
pub fn parse_block_until_ms(args: &serde_json::Value) -> Option<u64> {
    let raw = args.get("block_until_ms")?;
    if let Some(n) = raw.as_u64() {
        return Some(n);
    }
    raw.as_f64().filter(|f| f.is_finite() && *f >= 0.0).map(|f| f as u64)
}

/// The tool result shown when a call exceeds its execution timeout
/// (`buildToolCallExecutionTimedOutMessage`).
pub fn tool_execution_timeout_message(tool_name: &str, timeout: std::time::Duration) -> String {
    let shell_hint = if tool_name.eq_ignore_ascii_case("shell") {
        " For long-running commands, re-run with block_until_ms set to a small value (or 0) so the command runs in the background, then poll its output instead of blocking on it."
    } else {
        ""
    };
    let secs = (timeout.as_millis() as f64 / 1_000.0).round() as u64;
    if timeout.is_zero() {
        format!(
            "The {tool_name} tool call could not start because activity setup exceeded the per-call time limit. The execution environment may be slow or overloaded.{shell_hint}"
        )
    } else {
        format!(
            "The {tool_name} tool call timed out after {secs} seconds and was terminated. The execution environment may be unresponsive, or the operation needs longer than the per-call time limit.{shell_hint}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize, JsonSchema)]
    #[serde(rename_all = "lowercase")]
    enum Tier {
        Profile,
        Log,
        /// documented variant forces oneOf in schemars
        Note,
    }

    #[derive(Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct Args {
        tier: Option<Tier>,
        required_tier: Tier,
        name: Option<String>,
        tags: Option<Vec<Tier>>,
    }

    #[test]
    fn schema_is_flat_and_ref_free() {
        let schema = schema_for_args::<Args>();
        let text = schema.to_string();
        assert!(!text.contains("$ref"), "{text}");
        assert!(!text.contains("$defs"), "{text}");
        assert!(!text.contains("anyOf"), "{text}");
        assert!(!text.contains("oneOf"), "{text}");
        assert!(!text.contains("\"null\""), "{text}");
        let tier = &schema["properties"]["tier"];
        assert_eq!(tier["type"], "string");
        assert_eq!(tier["enum"].as_array().unwrap().len(), 3);
        assert_eq!(schema["properties"]["required_tier"]["enum"].as_array().unwrap().len(), 3);
        assert_eq!(schema["properties"]["name"]["type"], "string");
        assert_eq!(schema["properties"]["tags"]["type"], "array");
        assert_eq!(schema["properties"]["tags"]["items"]["type"], "string");
        assert_eq!(schema["type"], "object");
        let required: Vec<&str> = schema["required"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(required, vec!["required_tier"]);
        let _ = (Tier::Profile, Tier::Log, Tier::Note);
    }

    #[test]
    fn execution_timeouts_follow_the_original_tiers() {
        use crate::consts::*;
        let secs = |d: std::time::Duration| d.as_secs();
        assert_eq!(secs(tool_execution_timeout("Shell")), (SHORT_TOOL_TIMEOUT_MS - 60_000) / 1000);
        assert_eq!(secs(tool_execution_timeout("RunSubagent")), (LONG_TOOL_TIMEOUT_MS - 60_000) / 1000);
        // block 30 s → suggested 90 s → tier 5 min → headroom 4 min, but at least block + 30 s.
        assert_eq!(secs(tool_execution_timeout_for_args("Shell", Some(30_000))), 240);
        // block 10 min → suggested 11 min → tier 15 min → max(14 min, 10.5 min) = 14 min.
        assert_eq!(secs(tool_execution_timeout_for_args("Shell", Some(600_000))), 840);
        assert_eq!(parse_block_until_ms(&serde_json::json!({"block_until_ms": 1500})), Some(1500));
        assert_eq!(parse_block_until_ms(&serde_json::json!({})), None);
        let msg = tool_execution_timeout_message("Shell", std::time::Duration::from_secs(840));
        assert!(msg.starts_with("The Shell tool call timed out after 840 seconds and was terminated."));
        assert!(msg.contains("re-run with block_until_ms"));
        assert!(!tool_execution_timeout_message("Read", std::time::Duration::from_secs(1)).contains("block_until_ms"));
    }
}
