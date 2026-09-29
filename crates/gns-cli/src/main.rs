//! `gns-cli` — a small REPL over [`gns_runtime::AgentHost`].
//!
//! ```text
//! OPENAI_API_KEY=... gns-cli --root ./.gns-data --model gpt-4o-mini
//! OPENAI_BASE_URL=https://api.example.com/v1 OPENAI_API_KEY=... OPENAI_MODEL=qwen-plus gns-cli   # any OpenAI-compatible endpoint
//! gns-cli --base-url http://localhost:8000/v1 --model my-finetune                                # keyless local server
//! KIMI_API_KEY=...   gns-cli --root ./.gns-data --model kimi-k2-turbo-preview
//! MOONSHOT_API_KEY=... gns-cli --model kimi-k2-0905-preview --provider moonshot   # api.moonshot.cn
//! ```
//!
//! Commands: `/agents`, `/new <name> -- <description>`, `/use <id|name>`,
//! `/groups`, `/group new <name> <member,member,...> -- <description>`,
//! `/post <group> <text>`, `/routines [agent]`, `/log [n]`, `/prompt`,
//! `/broadcast <text>`, `/reply <id> <text>`, `/widget <id> <value>`,
//! `/react <id> <emoji>`, `/quit`. Anything else is sent to the current agent.
//!
//! Sends are accepted at once (like the original `sendPrompt`); replies,
//! approvals and errors arrive through host events, so `/approve` works while
//! a turn is still running.

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use gns_core::*;
use gns_llm::{DEFAULT_MODEL, GenaiConfig, GenaiProvider, MockLlm, ModelProvider, OPENAI_MODEL_ENV};
use gns_runtime::{AgentHost, AgentHostConfig, SendOptions};
use std::io::{BufRead, Write};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(name = "gns-cli", version, about = "Genius Bot multi-agent REPL")]
struct Args {
    /// Root directory for agents and memory.
    #[arg(long, default_value = ".gns-data")]
    root: std::path::PathBuf,
    /// Model name: gpt-4o-mini, kimi-k2-turbo-preview, moonshot-v1-8k, or whatever a custom endpoint serves (qwen-plus, my-finetune, …).
    /// Defaults to $GNS_MODEL, then $OPENAI_MODEL, then gpt-4o-mini.
    #[arg(long)]
    model: Option<String>,
    /// Vendor: openai | kimi (api.moonshot.ai) | moonshot (api.moonshot.cn) | auto. Inferred from the model name when omitted;
    /// a name nobody recognises is served from the OpenAI-compatible endpoint when one is configured.
    #[arg(long, env = "GNS_PROVIDER")]
    provider: Option<String>,
    #[allow(rustdoc::bare_urls)]
    /// OpenAI-compatible base URL, version path included (https://api.example.com/v1, http://localhost:8000/v1).
    /// Defaults to $OPENAI_BASE_URL / $KIMI_BASE_URL / $MOONSHOT_BASE_URL per provider.
    #[arg(long)]
    base_url: Option<String>,
    /// API key; overrides --api-key-env. Prefer the environment variable, since command lines are visible to other processes.
    #[arg(long)]
    api_key: Option<String>,
    /// Environment variable holding the API key (default per provider: OPENAI_API_KEY, KIMI_API_KEY, MOONSHOT_API_KEY).
    /// A custom --base-url works without any key (local servers).
    #[arg(long)]
    api_key_env: Option<String>,
    /// IANA time zone, e.g. Asia/Shanghai.
    #[arg(long, env = "GNS_TZ")]
    tz: Option<String>,
    /// Your display name.
    #[arg(long, env = "GNS_USER_NAME")]
    user_name: Option<String>,
    /// Disable streaming.
    #[arg(long)]
    no_stream: bool,
    /// Use an offline mock model that only echoes (for trying the REPL without a key).
    #[arg(long)]
    mock: bool,
    /// Show plain assistant text (inner monologue) and tool calls.
    #[arg(long, short)]
    verbose: bool,
    /// Disable the default Shell guard policy (dangerous commands need /approve).
    #[arg(long)]
    no_guard: bool,
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_writer(std::io::stderr).init();
    let args = Args::parse();
    let llm: Arc<dyn LlmProvider> = if args.mock {
        Arc::new(MockLlm::new().with_responder(|req| {
            let last = req
                .messages
                .last()
                .map(|m| match m {
                    LlmMessage::User { text, .. } => text.clone(),
                    _ => String::new(),
                })
                .unwrap_or_default();
            if let Some(LlmMessage::ToolResults(_)) = req.messages.last() {
                LlmResponse::text("done")
            } else {
                // Echo the first line of the user's text, minus the turn wrappers.
                let said = last
                    .lines()
                    .map(str::trim)
                    .find(|l| {
                        let wrapper =
                            l.starts_with("<timestamp>") || l.starts_with("<incoming_message_id>") || l.starts_with("<user_query>");
                        let address = l.starts_with('[') && l.ends_with(']') && !l.contains(' ');
                        !(l.is_empty() || wrapper || address)
                    })
                    .unwrap_or("");
                let said = said.strip_prefix(gns_core::prompt::SAND_HIDDEN_PROMPT_MARKER).unwrap_or(said);
                LlmResponse::tool_call(
                    SEND_MESSAGE_TOOL_NAME,
                    serde_json::json!({"type": "text", "content": format!("(mock) you said: {said}")}),
                )
            }
        }))
    } else {
        let model = args
            .model
            .clone()
            .or_else(|| env_non_empty("GNS_MODEL"))
            .or_else(|| env_non_empty(OPENAI_MODEL_ENV))
            .unwrap_or_else(|| DEFAULT_MODEL.to_owned());
        let mut config = match args.provider.as_deref().map(|p| p.trim().to_ascii_lowercase()).as_deref() {
            None => GenaiConfig::new(model),
            Some("openai") => GenaiConfig::openai(model),
            Some("kimi") => GenaiConfig::kimi(model),
            Some("moonshot") => GenaiConfig::moonshot(model),
            Some("auto") => GenaiConfig::with_provider(model, ModelProvider::Auto),
            Some(other) => return Err(anyhow!("unknown --provider {other}; use openai | kimi | moonshot | auto")),
        };
        if let Some(env) = args.api_key_env.clone() {
            config.api_key_env = Some(env);
        }
        if let Some(key) = args.api_key.clone() {
            config.api_key = Some(key);
        }
        if let Some(url) = args.base_url.clone() {
            config.base_url = Some(url);
        }
        config.stream = !args.no_stream;
        let provider = GenaiProvider::new(config).context("configuring the model")?;
        eprintln!(
            "model {} via {:?}{}{}",
            provider.model_name(),
            provider.provider(),
            provider.base_url().map(|u| format!(" at {u}")).unwrap_or_default(),
            if provider.is_keyless() { " (no API key)" } else { "" }
        );
        Arc::new(provider)
    };
    let mut config = AgentHostConfig::new(&args.root);
    config.user_name = args.user_name.clone();
    config.time_zone = args.tz.clone();
    let host = AgentHost::open(config, llm).await.context("opening the host")?;
    if !args.no_guard {
        host.add_policy(Arc::new(gns_tools::ShellGuardPolicy::default()));
    }

    let printer = tokio::spawn(print_events(host.subscribe(), args.verbose));
    println!("Genius Bot (gns-cli) ready. Root: {}. Type /help for commands.", args.root.display());
    let mut current: Option<AgentAddress> = host.list_agents().into_iter().next();
    if let Some(agent) = &current {
        println!("Current agent: {} ({})", agent.name, agent.id);
    } else {
        println!("No agents yet. Create one with: /new Planner -- You plan work and delegate to teammates.");
    }

    // Read stdin on a plain thread so the REPL never blocks the runtime.
    let (lines_tx, mut lines_rx) = mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        loop {
            let mut line = String::new();
            match stdin.lock().read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if lines_tx.send(line).is_err() {
                        break;
                    }
                }
            }
        }
    });
    loop {
        {
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "\n{}> ", current.as_ref().map(|a| a.name.as_str()).unwrap_or("-"));
            let _ = out.flush();
        }
        let Some(line) = lines_rx.recv().await else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match handle_line(&host, &mut current, line).await {
            Ok(true) => break,
            Ok(false) => {}
            Err(e) => eprintln!("error: {e:#}"),
        }
    }
    host.shutdown().await;
    printer.abort();
    Ok(())
}

/// Accept a send without waiting for the turn; the result is reported by the
/// event printer when the turn ends.
fn send_detached(host: &AgentHost, agent: &str, text: &str, options: SendOptions) -> Result<()> {
    let accepted = host.send_prompt(agent, text, vec![], options)?;
    tokio::spawn(async move {
        if let Ok(result) = accepted.result.await {
            if result.awaiting_user_selection {
                println!("\n(waiting for your answer: /widget <id> <value>, or just type your reply)");
            }
            if let Some(e) = result.error {
                eprintln!("\n! turn error: {e}");
            }
        }
    });
    Ok(())
}

async fn handle_line(host: &AgentHost, current: &mut Option<AgentAddress>, line: &str) -> Result<bool> {
    let (cmd, rest) = match line.strip_prefix('/') {
        Some(cmd) => {
            let (c, r) = cmd.split_once(' ').unwrap_or((cmd, ""));
            (c, r.trim())
        }
        None => {
            let agent = current.as_ref().ok_or_else(|| anyhow!("no current agent; /new one first"))?;
            send_detached(host, agent.id.as_str(), line, SendOptions::default())?;
            return Ok(false);
        }
    };
    match cmd {
        "quit" | "exit" | "q" => return Ok(true),
        "help" => println!(
            "/agents | /new <name> -- <description> | /use <id|name> | /groups | /group new <name> <a,b,...> -- <desc> | /post <group> <text> | /reply <id> <text> | /widget <id> <value> | /react <id> <emoji> | /routines [agent] | /log [n] | /prompt | /usage | /broadcast <text> | /approvals | /approve <id|all> | /deny <id|all> [reason] | /delete <agent> | /dream | /webhook <source> [json] | /tools [enable|disable <tool>] | /mcp [add <name> <command> [args..] | add-http <name> <url> | remove|enable|disable <name> | sync] | /quit"
        ),
        "agents" => {
            for a in host.list_agents() {
                let marker = if current.as_ref().is_some_and(|c| c.id == a.id) { "*" } else { " " };
                println!("{marker} {} ({}) — {}", a.name, a.id, gns_core::text::clamp_line(&a.description, 80));
            }
        }
        "new" => {
            let (name, description) = rest.split_once("--").map(|(n, d)| (n.trim(), d.trim())).unwrap_or((rest, ""));
            if name.is_empty() {
                return Err(anyhow!("usage: /new <name> -- <description>"));
            }
            let mut spec = AgentSpec::new(name, description);
            spec.kickstart = true;
            let address = host.create_agent(spec).await?;
            println!("created {} ({})", address.name, address.id);
            *current = Some(address);
        }
        "use" => {
            let address = host.find_agent(rest).ok_or_else(|| anyhow!("no agent {rest}"))?;
            println!("using {} ({})", address.name, address.id);
            *current = Some(address);
        }
        "groups" => {
            for g in host.list_groups() {
                println!("  {} ({}) — {}", g.name, g.id, g.members.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", "));
            }
        }
        "group" => {
            let rest = rest.strip_prefix("new").ok_or_else(|| anyhow!("usage: /group new <name> <a,b,...> -- <description>"))?.trim();
            let (head, description) = rest.split_once("--").map(|(h, d)| (h.trim(), d.trim())).unwrap_or((rest, ""));
            let (name, members) = head.rsplit_once(' ').ok_or_else(|| anyhow!("usage: /group new <name> <a,b,...> -- <description>"))?;
            let members: Vec<AgentId> = members
                .split(',')
                .map(|m| host.find_agent(m.trim()).map(|a| a.id).ok_or_else(|| anyhow!("no agent {m}")))
                .collect::<Result<_>>()?;
            let group = host.create_group(GroupSpec { name: name.trim().to_owned(), description: description.to_owned(), members }).await?;
            println!("created group {} ({})", group.name, group.id);
        }
        "post" => {
            let (group, text) = rest.split_once(' ').ok_or_else(|| anyhow!("usage: /post <group> <text>"))?;
            host.post_to_group_as_user(group, text.trim()).await?;
        }
        "reply" => {
            let agent = current.as_ref().ok_or_else(|| anyhow!("no current agent"))?;
            let (id, text) = rest.split_once(' ').ok_or_else(|| anyhow!("usage: /reply <id> <text>"))?;
            send_detached(host, agent.id.as_str(), text.trim(), SendOptions { reply_to: Some(EntryId::from(id.trim())), is_fork: false })?;
        }
        "widget" => {
            let agent = current.as_ref().ok_or_else(|| anyhow!("no current agent"))?;
            let (id, value) = rest.split_once(' ').ok_or_else(|| anyhow!("usage: /widget <id> <value>"))?;
            let host2 = host.clone();
            let (agent_id, id, value) = (agent.id.to_string(), id.trim().to_owned(), value.trim().to_owned());
            tokio::spawn(async move {
                if let Err(e) = host2.respond_to_widget(&agent_id, &id, value).await {
                    eprintln!("\n! widget answer failed: {e}");
                }
            });
        }
        "react" => {
            let agent = current.as_ref().ok_or_else(|| anyhow!("no current agent"))?;
            let (id, emoji) = rest.split_once(' ').ok_or_else(|| anyhow!("usage: /react <id> <emoji>"))?;
            host.react_to_message(agent.id.as_str(), id.trim(), emoji.trim())?;
            println!("reacted {} on {}", emoji.trim(), id.trim());
        }
        "tools" => {
            let agent = current.as_ref().ok_or_else(|| anyhow!("no current agent"))?;
            let mut words = rest.split_whitespace();
            match (words.next(), words.next()) {
                (Some(verb @ ("enable" | "disable")), Some(tool)) => {
                    host.set_tool_enabled(agent.id.as_str(), tool, verb == "enable")?;
                    println!("{tool} {verb}d for {}", agent.name);
                }
                (None, _) => {
                    for t in host.list_tools(agent.id.as_str())? {
                        let source = match &t.source {
                            ToolSource::Builtin => "built-in".to_owned(),
                            ToolSource::Mcp { server } => format!("mcp:{server}"),
                        };
                        println!("  [{}] {} ({source}) — {}", if t.enabled { "on " } else { "off" }, t.name, t.description);
                    }
                }
                _ => return Err(anyhow!("usage: /tools | /tools enable <tool> | /tools disable <tool>")),
            }
        }
        "mcp" => {
            let agent = current.as_ref().ok_or_else(|| anyhow!("no current agent"))?;
            let mut words = rest.split_whitespace();
            let print_status = |s: &McpServerStatus| {
                let state = if !s.enabled {
                    "disabled".to_owned()
                } else if s.connected {
                    format!("connected, {} tool(s): {}", s.tools.len(), s.tools.join(", "))
                } else {
                    format!("not connected{}", s.last_error.as_deref().map(|e| format!(" — {e}")).unwrap_or_default())
                };
                println!("  {} — {state}", s.name);
            };
            match words.next() {
                None => {
                    let servers = host.mcp_servers(agent.id.as_str())?;
                    if servers.is_empty() {
                        println!("no MCP servers attached to {}", agent.name);
                    }
                    servers.iter().for_each(print_status);
                }
                Some("add") => {
                    let name = words.next().ok_or_else(|| anyhow!("usage: /mcp add <name> <command> [args..]"))?;
                    let command = words.next().ok_or_else(|| anyhow!("usage: /mcp add <name> <command> [args..]"))?;
                    let args: Vec<String> = words.map(str::to_owned).collect();
                    let status = host.add_mcp_server(agent.id.as_str(), McpServerConfig::stdio(name, command, args)).await?;
                    print_status(&status);
                }
                Some("add-http") => {
                    let name = words.next().ok_or_else(|| anyhow!("usage: /mcp add-http <name> <url>"))?;
                    let url = words.next().ok_or_else(|| anyhow!("usage: /mcp add-http <name> <url>"))?;
                    let status = host.add_mcp_server(agent.id.as_str(), McpServerConfig::http(name, url)).await?;
                    print_status(&status);
                }
                Some("remove") => {
                    let name = words.next().ok_or_else(|| anyhow!("usage: /mcp remove <name>"))?;
                    let removed = host.remove_mcp_server(agent.id.as_str(), name).await?;
                    println!("{}", if removed { "removed" } else { "no such server" });
                }
                Some(verb @ ("enable" | "disable")) => {
                    let name = words.next().ok_or_else(|| anyhow!("usage: /mcp {verb} <name>"))?;
                    print_status(&host.set_mcp_server_enabled(agent.id.as_str(), name, verb == "enable").await?);
                }
                Some("sync") => host.sync_mcp(agent.id.as_str()).await?.iter().for_each(print_status),
                Some(other) => return Err(anyhow!("unknown /mcp subcommand: {other}")),
            }
        }
        "routines" => {
            let agent = if rest.is_empty() {
                current.as_ref().map(|a| a.id.to_string()).ok_or_else(|| anyhow!("no current agent"))?
            } else {
                rest.to_owned()
            };
            for r in host.routines(&agent)? {
                println!(
                    "  {} [{}] {} — next {:?}; runs {}",
                    r.id,
                    if r.is_enabled { "enabled" } else { "paused" },
                    r.trigger_description,
                    r.next_run_at.map(gns_core::text::format_timestamp),
                    r.runs.len()
                );
            }
        }
        "log" => {
            let agent = current.as_ref().ok_or_else(|| anyhow!("no current agent"))?;
            let n: usize = rest.parse().unwrap_or(20);
            for entry in host.transcript(agent.id.as_str(), n)? {
                println!("  {}", describe_entry(&entry));
            }
        }
        "prompt" => {
            let agent = current.as_ref().ok_or_else(|| anyhow!("no current agent"))?;
            println!("{}", host.system_prompt(agent.id.as_str())?);
        }
        "usage" => {
            let agent = current.as_ref().ok_or_else(|| anyhow!("no current agent"))?;
            println!("{:?}", host.usage(agent.id.as_str())?);
        }
        "broadcast" => {
            let (total, scheduled) = host.broadcast_to_agents(None, rest)?;
            println!("broadcast to {scheduled}/{total} agents");
        }
        "approvals" => {
            let pending = host.pending_approvals();
            if pending.is_empty() {
                println!("no pending approvals");
            }
            for r in pending {
                println!(
                    "  {} — {} wants {} {} ({})",
                    r.id,
                    r.agent_name,
                    r.tool,
                    gns_core::text::clamp_line(&r.args.to_string(), 100),
                    r.reason
                );
            }
        }
        "approve" | "deny" => {
            let approved = cmd == "approve";
            let (id, reason) = rest.split_once(' ').map(|(i, r)| (i.trim(), r.trim())).unwrap_or((rest, ""));
            let reason = if reason.is_empty() { if approved { "approved" } else { "denied by user" } } else { reason };
            let ids: Vec<String> =
                if id == "all" { host.pending_approvals().into_iter().map(|r| r.id).collect() } else { vec![id.to_owned()] };
            if ids.is_empty() {
                return Err(anyhow!("nothing pending"));
            }
            for id in ids {
                println!(
                    "{} {id}: {}",
                    if approved { "approved" } else { "denied" },
                    if host.approve(&id, approved, reason) { "ok" } else { "unknown or expired id" }
                );
            }
        }
        "dream" => {
            let agent = current.as_ref().ok_or_else(|| anyhow!("no current agent"))?;
            let (added, removed) = host.dream(agent.id.as_str()).await?;
            println!("dream: {added} promoted to profile, {removed} removed");
        }
        "webhook" => {
            let (name, payload) = rest.split_once(' ').unwrap_or((rest, "{}"));
            let payload: serde_json::Value =
                serde_json::from_str(payload.trim()).unwrap_or_else(|_| serde_json::Value::String(payload.trim().to_owned()));
            let n = host.fire_webhook(name.trim(), payload)?;
            println!("event {name}: {n} routine(s) fired");
        }
        "delete" => {
            let address = host.find_agent(rest).ok_or_else(|| anyhow!("no agent {rest}"))?;
            host.delete_agent(address.id.as_str()).await?;
            println!("deleted {} ({})", address.name, address.id);
            if current.as_ref().is_some_and(|c| c.id == address.id) {
                *current = host.list_agents().into_iter().next();
            }
        }
        other => return Err(anyhow!("unknown command /{other}; try /help")),
    }
    Ok(false)
}

fn describe_entry(entry: &TranscriptEntry) -> String {
    match entry {
        TranscriptEntry::Message { id, role, content, hidden, from_agent, to_agent, group_id, .. } => {
            let who = match (from_agent, to_agent, group_id) {
                (Some(f), _, Some(_)) => format!("#room {}", f.name),
                (None, _, Some(_)) if *hidden => "#room turn".to_owned(),
                (None, _, Some(_)) => "#room user".to_owned(),
                (Some(f), _, _) => format!("← {}", f.name),
                (_, Some(t), _) => format!("→ {}", t.name),
                _ => format!("{role:?}"),
            };
            let tag = if id.as_str().starts_with('t') { format!("[{id}] ") } else { String::new() };
            format!("{tag}{who}{}: {}", if *hidden { " (hidden)" } else { "" }, gns_core::text::clamp_line(content, 100))
        }
        TranscriptEntry::SendMessage { id, message, reactions, .. } => {
            let marks = if reactions.is_empty() {
                String::new()
            } else {
                format!("  {}", reactions.iter().map(|r| r.emoji.as_str()).collect::<Vec<_>>().join(" "))
            };
            format!("[{id}] SendMessage: {}{marks}", gns_core::text::clamp_line(&message.display_text(), 100))
        }
        TranscriptEntry::AssistantText { content, .. } => format!("(thinking) {}", gns_core::text::clamp_line(content, 100)),
        TranscriptEntry::ToolCall { name, args, is_error, .. } => {
            format!("tool {name}{} {}", if *is_error { " (error)" } else { "" }, gns_core::text::clamp_line(&args.to_string(), 80))
        }
        TranscriptEntry::ProfileUpdate { summary, .. } => format!("profile: {summary}"),
        TranscriptEntry::Divider { summary, .. } => format!("---- summary: {}", gns_core::text::clamp_line(summary, 100)),
        #[allow(unreachable_patterns)]
        _ => "?".to_owned(),
    }
}

async fn print_events(mut rx: broadcast::Receiver<HostEvent>, verbose: bool) {
    let mut streaming_agent: Option<AgentId> = None;
    loop {
        let event = match rx.recv().await {
            Ok(e) => e,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        };
        match event {
            HostEvent::TaskUpdated { task } => {
                println!("\n· task {} {:?}: {}", task.id, task.status, task.summary);
                for a in &task.artifacts {
                    println!("  {} ({} bytes), artifact_id={} — FetchArtifact to obtain a local path", a.name, a.size, a.id);
                }
            }
            HostEvent::SendMessage { agent_name, message, group_id, entry_id, .. } => {
                end_stream(&mut streaming_agent);
                let room = group_id.map(|_| "#room ").unwrap_or("");
                println!("\n{room}[{agent_name}] {}  ({entry_id})", message.display_text());
            }
            HostEvent::GroupPosted { speaker, content, .. } => {
                end_stream(&mut streaming_agent);
                println!("\n#room {speaker}: {content}");
            }
            HostEvent::A2ASent { from, to, priority, text } => {
                end_stream(&mut streaming_agent);
                println!("\n· {from} → {to}{}: {}", if priority { " (priority)" } else { "" }, gns_core::text::clamp_line(&text, 120));
            }
            HostEvent::ToolCall { name, args, status: ToolCallStatus::Started, agent_id, .. } if verbose => {
                end_stream(&mut streaming_agent);
                println!("\n· {agent_id} tool {name} {}", gns_core::text::clamp_line(&args.to_string(), 120));
            }
            HostEvent::TextDelta { agent_id, text, .. } if verbose => {
                if streaming_agent.as_ref() != Some(&agent_id) {
                    end_stream(&mut streaming_agent);
                    print!("\n(thinking {agent_id}) ");
                    streaming_agent = Some(agent_id);
                }
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            HostEvent::RoutineFired { name, agent_id, .. } => println!("\n· routine \"{name}\" fired for {agent_id}"),
            HostEvent::Interrupted { agent_id, reason, .. } => println!("\n· {agent_id} interrupted: {reason}"),
            HostEvent::AgentCreated { address } => println!("\n· agent created: {} ({})", address.name, address.id),
            HostEvent::MemoryWritten { scope, fact, .. } => println!("\n· memory[{scope}] {}", gns_core::text::clamp_line(&fact, 100)),
            HostEvent::MemoryDreamed { added, removed, .. } => println!("\n· dream: +{added} profile facts, -{removed} stale"),
            HostEvent::SubagentStarted { label, .. } => println!("\n· subagent \"{label}\" started"),
            HostEvent::SubagentEnded { label, steps, aborted, .. } => {
                println!("\n· subagent \"{label}\" ended after {steps} step(s){}", if aborted { " (aborted)" } else { "" })
            }
            HostEvent::BackgroundJobStarted { job_id, command, .. } => {
                println!("\n· background job {job_id}: {}", gns_core::text::clamp_line(&command, 80))
            }
            HostEvent::BackgroundJobFinished { job_id, exit_code, .. } => {
                println!("\n· background job {job_id} finished (exit {exit_code})")
            }
            HostEvent::WebhookFired { name, routines } => println!("\n· webhook {name}: {routines} routine(s)"),
            HostEvent::McpServerConnected { server, tools, .. } => println!("\n· mcp \"{server}\" connected: {} tool(s)", tools.len()),
            HostEvent::McpServerFailed { server, error, .. } => eprintln!("\n! mcp \"{server}\" failed: {error}"),
            HostEvent::ApprovalRequested { request } => {
                end_stream(&mut streaming_agent);
                println!(
                    "\n? APPROVAL {} — {} wants to run {} {}\n  reason: {}\n  /approve {} | /deny {} [reason]",
                    request.id,
                    request.agent_name,
                    request.tool,
                    gns_core::text::clamp_line(&request.args.to_string(), 160),
                    request.reason,
                    request.id,
                    request.id
                );
            }
            HostEvent::Error { message, .. } => eprintln!("\n! {message}"),
            HostEvent::Retrying { attempt, error, .. } => eprintln!("\n· retrying model call ({attempt}): {error}"),
            HostEvent::TurnEnded { .. } => end_stream(&mut streaming_agent),
            _ => {}
        }
    }
}

fn end_stream(streaming: &mut Option<AgentId>) {
    if streaming.take().is_some() {
        println!();
    }
}
