//! `gns-tools` — the built-in tool set, aligned 1:1 with the original host's
//! model-facing tools (names, schemas, descriptions and result strings).
//!
//! Every tool is a [`gns_core::TypedTool`] with a `schemars`-derived schema
//! and depends only on [`gns_core::HostServices`], so it can be reused by any
//! runtime. Local tools (`Shell`, `AwaitShell`, `Read`) are confined to the
//! agent's workspace and data directories by [`sandbox_path`]; files are
//! written through `Shell`, as in the original (there is no Write/Edit tool).

mod communicate;
mod coordination;
mod local;
mod management;
mod paths;
mod policies;
mod shell;
mod state;
mod subagent;

pub use communicate::*;
pub use coordination::*;
pub use local::*;
pub use management::*;
pub use paths::*;
pub use policies::*;
pub use shell::*;
pub use state::*;
pub use subagent::*;

use gns_core::Tool;
use std::sync::Arc;

/// Every built-in tool, ready to register on a host.
///
/// Availability (see [`gns_core::ToolAvailability`]): `SendMessage`,
/// `SendToAgent`, `ReactToMessage`, `CreateAgent`, `UpdateAgent` and
/// `RunSubagent` are offered in every main turn including local group-room
/// turns but never to subagents; `update_state` additionally disappears when
/// the run overrides the system prompt; `Shell`, `AwaitShell` and `Read` are
/// `Local` (subagents included).
pub fn builtin_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        SendMessageTool::arc(),
        SendToAgentTool::arc(),
        DelegateTask::arc(),
        UpdateTask::arc(),
        CompleteTask::arc(),
        GetTask::arc(),
        FetchArtifact::arc(),
        ReactToMessageTool::arc(),
        CreateAgentTool::arc(),
        UpdateAgentTool::arc(),
        UpdateStateTool::arc(),
        RunSubagentTool::arc(),
        ShellTool::arc(ShellConfig::default()),
        AwaitShellTool::arc(),
        ReadTool::arc(),
    ]
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_builtin_schema_is_vendor_safe() {
        for tool in super::builtin_tools() {
            let text = tool.parameters_schema().to_string();
            for forbidden in ["$ref", "$defs", "anyOf", "oneOf", "\"null\""] {
                assert!(!text.contains(forbidden), "{} schema contains {forbidden}: {text}", tool.name());
            }
        }
    }

    #[test]
    fn builtin_inventory_matches_the_original() {
        let names: Vec<String> = super::builtin_tools().iter().map(|t| t.name().to_owned()).collect();
        assert_eq!(
            names,
            [
                "SendMessage",
                "SendToAgent",
                "DelegateTask",
                "UpdateTask",
                "CompleteTask",
                "GetTask",
                "FetchArtifact",
                "ReactToMessage",
                "CreateAgent",
                "UpdateAgent",
                "update_state",
                "RunSubagent",
                "Shell",
                "AwaitShell",
                "Read"
            ]
        );
    }
}
