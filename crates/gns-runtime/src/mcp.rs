//! Per-agent MCP servers: a registry of live connections and the tools they
//! contribute. Servers are (re)connected lazily when an agent's turn starts,
//! so a broken server costs one attempt per [`MCP_RETRY_AFTER_MS`] and never
//! blocks the rest of the fleet.

use crate::agent::AgentHandle;
use crate::host::HostInner;
use gns_core::text::now_ms;
use gns_core::*;
use gns_mcp::{McpClient, tools_for_server};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct ServerState {
    config: McpServerConfig,
    client: Option<Arc<McpClient>>,
    tools: Vec<Arc<dyn Tool>>,
    last_error: Option<String>,
    last_attempt_ms: i64,
}

/// Live MCP connections, keyed by agent then server name.
#[derive(Default)]
pub(crate) struct McpRegistry {
    servers: Mutex<HashMap<AgentId, HashMap<String, ServerState>>>,
}

impl std::fmt::Debug for McpRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpRegistry").finish_non_exhaustive()
    }
}

fn same_connection(a: &McpServerConfig, b: &McpServerConfig) -> bool {
    a.transport == b.transport && a.allowed_tools == b.allowed_tools && a.timeout_secs == b.timeout_secs
}

impl McpRegistry {
    /// Reconcile the agent's configured servers with the live connections:
    /// close detached/disabled/changed ones, connect missing ones (respecting
    /// the retry backoff unless `force`), refresh tool lists the server
    /// announced as changed.
    pub(crate) async fn sync(&self, host: &HostInner, handle: &Arc<AgentHandle>, force: bool) {
        let settings = handle.settings();
        let now = now_ms();
        let mut to_close: Vec<Arc<McpClient>> = Vec::new();
        let mut to_connect: Vec<McpServerConfig> = Vec::new();
        let mut to_refresh: Vec<(String, Arc<McpClient>, McpServerConfig)> = Vec::new();
        if let Ok(mut all) = self.servers.lock() {
            let entry = all.entry(handle.id.clone()).or_default();
            entry.retain(|name, state| {
                let keep = settings.mcp_servers.iter().any(|c| &c.name == name && c.enabled && same_connection(c, &state.config));
                if !keep && let Some(client) = state.client.take() {
                    to_close.push(client);
                }
                keep
            });
            for config in settings.mcp_servers.iter().filter(|c| c.enabled) {
                match entry.get(&config.name) {
                    Some(state) if state.client.as_ref().is_some_and(|c| !c.is_closed()) => {
                        if let Some(client) = &state.client
                            && client.tools_changed()
                        {
                            to_refresh.push((config.name.clone(), client.clone(), config.clone()));
                        }
                    }
                    Some(state) if !force && now - state.last_attempt_ms < MCP_RETRY_AFTER_MS => {}
                    _ => to_connect.push(config.clone()),
                }
            }
        }
        for client in to_close {
            client.close().await;
        }
        let mut changed = false;
        for config in to_connect {
            let outcome = McpClient::connect(&config, Some(&handle.workspace_dir), Duration::from_secs(MCP_CONNECT_TIMEOUT_SECS)).await;
            let state = match outcome {
                Ok(client) => {
                    let tools = tools_for_server(&client, &config);
                    host.emit(HostEvent::McpServerConnected {
                        agent_id: handle.id.clone(),
                        server: config.name.clone(),
                        tools: tools.iter().map(|t| t.name().to_owned()).collect(),
                    });
                    ServerState { config, client: Some(client), tools, last_error: None, last_attempt_ms: now }
                }
                Err(e) => {
                    tracing::warn!(agent = %handle.id, server = %config.name, "MCP connect failed: {e}");
                    host.emit(HostEvent::McpServerFailed {
                        agent_id: handle.id.clone(),
                        server: config.name.clone(),
                        error: e.to_string(),
                    });
                    ServerState { config, client: None, tools: Vec::new(), last_error: Some(e.to_string()), last_attempt_ms: now }
                }
            };
            changed = true;
            if let Ok(mut all) = self.servers.lock() {
                all.entry(handle.id.clone()).or_default().insert(state.config.name.clone(), state);
            }
        }
        for (name, client, config) in to_refresh {
            if let Err(e) = client.refresh_tools().await {
                tracing::warn!(agent = %handle.id, server = %name, "tools/list refresh failed: {e}");
                continue;
            }
            let tools = tools_for_server(&client, &config);
            if let Ok(mut all) = self.servers.lock()
                && let Some(state) = all.entry(handle.id.clone()).or_default().get_mut(&name)
            {
                state.tools = tools;
                changed = true;
            }
        }
        if changed {
            host.emit(HostEvent::ToolsChanged { agent_id: handle.id.clone() });
        }
    }

    /// Tools contributed by the agent's connected servers.
    pub(crate) fn tools(&self, agent: &AgentId) -> Vec<Arc<dyn Tool>> {
        self.servers
            .lock()
            .map(|all| all.get(agent).map(|servers| servers.values().flat_map(|s| s.tools.iter().cloned()).collect()).unwrap_or_default())
            .unwrap_or_default()
    }

    /// Connection state of every configured server (connected or not).
    pub(crate) fn statuses(&self, agent: &AgentId, settings: &AgentSettings) -> Vec<McpServerStatus> {
        let all = self.servers.lock().ok();
        let live = all.as_ref().and_then(|a| a.get(agent));
        settings
            .mcp_servers
            .iter()
            .map(|config| {
                let state = live.and_then(|s| s.get(&config.name));
                let connected = state.and_then(|s| s.client.as_ref()).is_some_and(|c| !c.is_closed());
                McpServerStatus {
                    agent_id: agent.clone(),
                    name: config.name.clone(),
                    enabled: config.enabled,
                    connected,
                    tools: if connected {
                        state.map(|s| s.tools.iter().map(|t| t.name().to_owned()).collect()).unwrap_or_default()
                    } else {
                        Vec::new()
                    },
                    last_error: state.and_then(|s| s.last_error.clone()),
                }
            })
            .collect()
    }

    /// Drop one server's connection (after it was detached).
    pub(crate) async fn forget_server(&self, agent: &AgentId, name: &str) {
        let client = self.servers.lock().ok().and_then(|mut all| all.get_mut(agent)?.remove(name)?.client);
        if let Some(client) = client {
            client.close().await;
        }
    }

    /// Close every connection of an agent (deletion).
    pub(crate) async fn disconnect_agent(&self, agent: &AgentId) {
        let clients: Vec<Arc<McpClient>> = self
            .servers
            .lock()
            .ok()
            .and_then(|mut all| all.remove(agent))
            .map(|s| s.into_values().filter_map(|s| s.client).collect())
            .unwrap_or_default();
        for client in clients {
            client.close().await;
        }
    }

    /// Close everything (host shutdown).
    pub(crate) async fn close_all(&self) {
        let clients: Vec<Arc<McpClient>> = self
            .servers
            .lock()
            .map(|mut all| all.drain().flat_map(|(_, s)| s.into_values().filter_map(|s| s.client)).collect())
            .unwrap_or_default();
        for client in clients {
            client.close().await;
        }
    }
}
