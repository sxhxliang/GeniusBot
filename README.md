# Genius Bot — Multi-Agent SDK in Rust

Genius Bot is a multi-agent assistant and SDK in Rust. It provides
persistent agents with their own SQLite transcript, a step-loop runner where
`SendMessage` is the agent's only voice, asynchronous agent-to-agent messaging
with priority preemption, bounded group-chat rounds with `(pass)`, three-tier
memory with decay ranking and frozen prompt snapshots, cron routines, immutable
file artifacts, and durable delegated tasks with automatic result delivery.

LLM access goes through [`genai`](https://crates.io/crates/genai): OpenAI
(`gpt-*`), Kimi / Moonshot (`kimi-*`, `moonshot-*`) and any OpenAI-compatible
endpoint (custom base URL, key and model name via `OPENAI_BASE_URL`,
`OPENAI_API_KEY`, `OPENAI_MODEL`) are supported out of the box; other vendors
`genai` knows fall back to its own inference. Tools run on the local
host, isolated per agent by directory. Shell uses PowerShell on Windows
(`pwsh.exe` when available, otherwise Windows PowerShell) and `sh` elsewhere;
commands must use the corresponding shell syntax. MCP servers can be attached per
agent. No sandbox, no browser.

## Workspace

| crate | role |
| --- | --- |
| `gns-core` | Types and extension traits: ids, transcript entries, `Tool`/`TypedTool`, `LlmProvider`, `HostServices`, `PromptSection`, `RunMiddleware`, memory/group/routine models and the pure rule functions (mention parsing, memory ranking, prompt builders). No IO. |
| `gns-llm` | `GenaiProvider` (streaming, tool calls, usage, cancellation) and `MockLlm` (scripted, for tests). |
| `gns-store` | `AgentDb` (SQLite WAL: `kv` + `transcript_entries`), on-disk layout, atomic JSON files, markdown memory shards, `automation.json` store, file-based roster discovery. |
| `gns-tools` | Original chat, agent, shell and read tools, plus `FetchArtifact`, `DelegateTask`, `UpdateTask`, `CompleteTask`, `GetTask`. Files are edited through `Shell`. Depends only on `gns-core`. |
| `gns-mcp` | Minimal MCP (Model Context Protocol) client: stdio and streamable-HTTP transports, `initialize`/`tools/list`/`tools/call`, and an adapter exposing every server tool as a `Tool` named `mcp__<server>__<tool>`. |
| `gns-runtime` | `AgentHost`: one actor per agent (lanes, exclusive queue, preemption), the step-loop runner, A2A messaging, group orchestrator, memory service, routine scheduler, prompt assembly, event bus, per-agent tool sets and MCP registry. |
| `gns-cli` | REPL demo. |
| `gns-server` | HTTP API (REST + SSE) with an embedded React debug console: model / endpoint / key settings, model-call logs, agent inspection, chat, groups, approvals. See [`crates/gns-server/README.md`](crates/gns-server/README.md). |

Dependency direction is strictly `core ← llm/store/tools/mcp ← runtime ← cli/server`.

## Per-agent tools and MCP servers

Every agent has its own tool set, stored in its `settings.json`:

```json
{
  "tools": { "disabled": ["Shell"] },
  "mcpServers": [
    { "name": "fs", "transport": { "type": "stdio", "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp/shared"] } },
    { "name": "api", "transport": { "type": "http", "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer …" } }, "allowedTools": ["search"] }
  ]
}
```

* **Built-in tools** can be switched off per agent (`SendMessage` stays available on ordinary turns; task turns use `UpdateTask` / `CompleteTask`).
  `UpdateTask` and `CompleteTask` are offered only during an active delegated
  task run. A later chat message with an old task id does not resume that run;
  inspect it with `GetTask` and resume a blocked task from its task card.
* **MCP servers** are attached per agent and connected lazily when the agent's
  next turn starts (a failed server is retried once a minute and never blocks
  the turn). Each server tool becomes `mcp__<server>__<tool>`; results go
  through the same policies and approvals as built-in tools. The system
  prompt lists attached servers and their state.
* Configure from code (`host.add_mcp_server`, `host.set_tool_enabled`,
  `host.list_tools`, `host.mcp_servers`), from the REPL (`/tools`,
  `/mcp add <name> <command> [args..]`, `/mcp add-http <name> <url>`,
  `/mcp remove|enable|disable <name>`, `/mcp sync`), or let the agent do it
  itself through the host API. `update_state` no longer exposes `tools` /
  `mcp` targets to the model (the original manages MCP with separate tools).
* Transports: stdio (local process, inherits the host environment plus the
  configured `env`) and streamable HTTP (JSON or SSE responses,
  `Mcp-Session-Id` sessions). The legacy HTTP+SSE (`GET /sse`) transport is not
  supported.

## Quick start

Install the REPL or the server with its embedded web console from crates.io:

```bash
cargo install gns-cli --locked
cargo install gns-server --locked
gns-cli --mock
gns-server --mock                         # http://127.0.0.1:8787
```

To build from source, run these commands from the repository root:

```bash
cargo test --workspace                       # all offline (MockLlm, tempdir, fake SSE and MCP servers)
cargo run -p gns-runtime --example custom_tool   # offline: custom tool + prompt section

export OPENAI_API_KEY=sk-...
cargo run -p gns-cli -- --root ./.gns-data --model gpt-4o-mini --tz Asia/Shanghai

# Any OpenAI-compatible endpoint: custom address, key and model name
export OPENAI_BASE_URL=https://api.example.com/v1   # version path included
export OPENAI_API_KEY=sk-...
export OPENAI_MODEL=qwen-plus                       # or --model / GNS_MODEL
cargo run -p gns-cli -- --root ./.gns-data

# Keyless local server (vLLM, LM Studio, llama.cpp, Ollama's /v1): flags work too
cargo run -p gns-cli -- --base-url http://localhost:8000/v1 --model my-finetune

# Kimi (international platform, api.moonshot.ai)
export KIMI_API_KEY=sk-...
cargo run -p gns-cli -- --root ./.gns-data --model kimi-k2-turbo-preview --tz Asia/Shanghai

# Moonshot China platform (api.moonshot.cn) — same model names, different endpoint/key
export MOONSHOT_API_KEY=sk-...
cargo run -p gns-cli -- --root ./.gns-data --model kimi-k2-0905-preview --provider moonshot
```

### Model providers

| provider | model names | key env | base URL env | default endpoint |
| --- | --- | --- | --- | --- |
| `openai` | `gpt-*`, `o1`/`o3`/`o4-*`, and any name when a base URL is configured | `OPENAI_API_KEY` | `OPENAI_BASE_URL` | api.openai.com |
| `kimi` | `kimi-k2-turbo-preview`, `kimi-k2-thinking`, `kimi-k2-0905-preview`, `kimi-latest` | `KIMI_API_KEY` | `KIMI_BASE_URL` | api.moonshot.ai |
| `moonshot` | `moonshot-v1-8k/32k/128k`, `kimi-k2-*` | `MOONSHOT_API_KEY` | `MOONSHOT_BASE_URL` | api.moonshot.cn |
| `auto` | anything else `genai` recognises (`claude-*`, `gemini-*`, `deepseek-*`, `grok-*`, `glm-*`, `ollama::…`) | vendor default | — | vendor default |

The provider is inferred from the model name (`GenaiConfig::new("kimi-k2-turbo-preview")`)
or chosen explicitly (`GenaiConfig::kimi(..)`, `GenaiConfig::moonshot(..)`, `--provider moonshot`).
Kimi/Moonshot requests always send `temperature = 1` (the vendor rejects every
other value); tool calling, streaming and usage capture work the same
way as with OpenAI because the wire format is OpenAI-compatible.

#### Custom OpenAI-compatible endpoint

Three settings pick the endpoint: the **address** (`OPENAI_BASE_URL` or
`--base-url`, version path included, e.g. `https://api.example.com/v1`), the
**key** (`OPENAI_API_KEY`, `--api-key`, or `--api-key-env NAME`) and the
**model** (`OPENAI_MODEL`, `GNS_MODEL` or `--model`). Rules:

- A model name nobody recognises (`qwen-plus`, `llama-3.3-70b`, `my-finetune`,
  `Qwen/Qwen2.5-72B-Instruct`, …) is served from the configured base URL with
  the OpenAI wire protocol, so no `--provider` flag is needed.
- A name with a known vendor keeps that vendor (`kimi-*` → Kimi, `claude-*` →
  Anthropic, …). To push such a name through the gateway anyway, add
  `--provider openai` (or `GenaiConfig::openai_compatible(..)`).
- With a custom base URL the key is optional: a keyless local server works
  without one. The official endpoint still requires `OPENAI_API_KEY`.
- To keep local Ollama with `OPENAI_BASE_URL` set, namespace the model
  (`ollama::llama3.2`) or pass `--provider auto`.

In the REPL:

```text
/new Planner -- You break tasks down and delegate coding to your teammate Coder.
/new Coder -- Write files with Shell and submit complete paths with CompleteTask for delegated tasks.
/use Planner
Have Coder write hello.py in its workspace, run it, and tell me the output.
/group new Review Planner,Coder -- code review room
/post Review @Coder explain hello.py in one line
/routines            /log 20            /prompt            /quit
/approvals           /approve <id|all>  /deny <id> [reason]  /delete <agent>
/dream               /webhook github {"repo":"o/n","kind":"pr-opened","actor":"me","title":"x"}
/reply t3s0 thanks   /widget t3s0 Blue   /react t3s0 👍
```

Try `--mock` to explore the REPL without a key.

### Web console (`gns-server`)

```bash
(cd crates/gns-server/web && npm install && npm run build)   # or: GNS_SERVER_BUILD_WEB=1 cargo build -p gns-server
cargo run -p gns-server -- --root ./.gns-data                 # open http://127.0.0.1:8787
cargo run -p gns-server -- --mock                             # offline echo model
```

Set the model, base URL and API key in the console (⚙ Settings, saved to
`<root>/gns-server.json`) or with the same flags as `gns-cli`. Every model call
is logged with its full request and response (*Model calls*). The server binds
to loopback; `--token` is required to listen elsewhere.

## Library use

```bash
cargo add gns-core gns-llm gns-runtime   # library crates (gns-store, gns-tools, gns-mcp come with gns-runtime)
cargo install gns-cli                    # the REPL demo binary
```

```rust
use gns_core::*;
use gns_llm::{GenaiConfig, GenaiProvider};
use gns_runtime::{AgentHost, AgentHostConfig};
use std::sync::Arc;

# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
let llm = Arc::new(GenaiProvider::new(GenaiConfig::new("gpt-4o-mini"))?);   // or GenaiConfig::kimi("kimi-k2-turbo-preview")
// Custom OpenAI-compatible endpoint (address, key, model):
//   GenaiConfig::openai_compatible("https://api.example.com/v1", "qwen-plus").with_api_key("sk-…")
//   GenaiConfig::openai_from_env()   // OPENAI_BASE_URL / OPENAI_API_KEY / OPENAI_MODEL
let host = AgentHost::open(AgentHostConfig::new("./.gns-data"), llm).await?;

let mut events = host.subscribe();                     // HostEvent stream for UIs
let planner = host.create_agent(AgentSpec::new("Planner", "Plans and delegates.")).await?;
let result = host.send_user_message(planner.id.as_str(), "Hi!", vec![]).await?;   // or send_prompt(..) to return at once
assert!(result.sent_message_count > 0);
# Ok(()) }
```

### File transfer and task completion

Agent workspaces are separate. A message containing a sender's path does not
copy that file or let the recipient read it. `files` snapshots the actual bytes;
`artifact_ids` forwards an existing snapshot. The recipient gets its name, size,
SHA-256 and ID, then calls `FetchArtifact` to obtain a local path under
`workspace/incoming/<artifact-id>/`. No file body needs to pass through the model.
The 8,000-character A2A message limit applies to the accompanying text only.

For an ordinary file handoff, use:

```json
{"target_id":"<recipient-or-group-id>","message":"Full report attached","files":["report.pdf"]}
```

For work that needs a result, Planner calls `DelegateTask`:

```json
{"target_id":"<coder-id>","task":"Optimize all three sorting algorithms; include tests and benchmarks","require_files":true}
```

Coder creates and checks `optimized_sorts.py` using Shell, then calls `CompleteTask`:

```json
{"task_id":"<returned-task-id>","status":"completed","summary":"Three algorithms, tests and benchmarks submitted","verification":"Describe the checks actually run","files":["optimized_sorts.py"]}
```

The host commits task status, file grants and a delivery event together. It posts
a downloadable result card to the original DM or group, then wakes Planner to
review and explain. A group task does not create an extra result card in
Planner's private chat. `completed` means submitted; verification notes are the
executor's report, not an independent acceptance of correctness.

Use `UpdateTask` with `running` for progress, or `blocked` with the missing input.
The console offers **Resume task** and **Cancel task**. A file-required task
cannot succeed without actual attachments. An executor that finishes without
submitting gets one reminder, then the task becomes blocked. Task turns hide
`SendMessage` and `DelegateTask`; persistent delegation is one level deep.

SDK entry points:

```rust,ignore
let files = host.publish_artifacts("Coder", vec!["report.pdf".into()]).await?;
host.send_artifacts("Coder", planner.id.as_str(), "Full report", vec![files[0].id.clone()]).await?;
let path = host.fetch_artifact("Planner", &files[0].id).await?;
let task = host.delegate_task("Planner", DelegateTaskRequest {
    target_id: coder.id.to_string(), task: "Write a report".into(), require_files: true,
    ..Default::default()
}).await?;
// host.delegate_group_task(group_id, "Planner", request).await?
// host.task(&task.id), host.list_tasks(), host.cancel_task(&task.id)
// host.resume_task(&task.id, "Additional instructions".into())
```

Limits default to 100 MiB per file and 10 attachments; configure
`AgentHostConfig::artifact_max_bytes` / `artifact_max_files`. Only regular files
inside the sender's directories may be published. Snapshots survive source edits
and agent deletion. Fetch and download verify SHA-256; fetching refuses to
overwrite a recipient's edited copy. Group grants follow current membership;
copies already fetched or explicitly forwarded remain with their recipients.

On restart queued tasks resume, running tasks become blocked for explicit
resumption, and committed result events replay using a stable card ID. Identical
completion retries return the prior submission; conflicting retries fail. Review
wakes retry up to three times and report failure through host error events.
File/card delivery is durable; a crash can repeat model review and its side
effects. Deleted origins are never replaced with another conversation.

Artifacts currently remain until the owner removes the host data; automatic
garbage collection is not implemented. Directory checks are application-level,
not OS isolation of Shell. This feature is in the Rust SDK/runtime/server;
the reconstructed TypeScript desktop is unchanged.

Run `cargo run -p gns-server --example artifact_demo` for an offline console
demo on `127.0.0.1:8799` (token `artifact-demo`, temporary data). See
[`crates/gns-server/README.md`](crates/gns-server/README.md) for download and task APIs.

### Extension points

| Extension | How |
| --- | --- |
| Custom tool | Implement `TypedTool` (schema derived with `schemars`) or `Tool`; `host.register_tool(Typed::arc(MyTool))`. `availability()` controls which turn kinds offer it. |
| Model backend | Implement `LlmProvider` and pass it to `AgentHost::open`. |
| Prompt section | Implement `PromptSection`; `host.add_prompt_section(section, SectionPosition::After("profile".into()))`. |
| Turn middleware | Implement `RunMiddleware` (`before_llm_call`, `after_tool_call`, `on_turn_settled`); `host.add_middleware(..)`. |
| Subagents | `RunSubagent` (built-in tool) spawns an ephemeral, bounded step loop in the parent's workspace with only `ToolAvailability::Local` tools (Shell/AwaitShell/Read; `readonly` leaves just Read). It cannot talk to the user, message agents or nest, reports back in plain text, and its tool calls are not persisted. Custom tools opt in with `ToolAvailability::Local`. |
| Event triggers | The original's listener triggers (`slack`, `github`, `microsoftTeams`, `linear`, `sentry`, `pagerduty`, `group`) on a routine; an HTTP layer (or anything else) calls `host.fire_webhook(source, payload)` and every matching enabled routine wakes with the event rendered as a `<github_event>`-style block (750 ms debounce, ≤ 25 events per wake). |
| Tool policy / approvals | Implement `ToolPolicy` (`Allow` / `Deny(reason)` / `RequireApproval(reason)`); `host.add_policy(..)`. `RequireApproval` pauses the tool call, emits `HostEvent::ApprovalRequested`, and waits for `host.approve(id, bool, reason)` (10-minute TTL, cancelled with the turn). Built-ins: `gns_tools::ShellGuardPolicy` (dangerous shell patterns), `ApproveToolsPolicy` (always ask for the listed tools). |
| Storage | `TranscriptStore` / `KvStore` traits in `gns-core`; `AgentDb` is the SQLite implementation. |
| Events | `host.subscribe()` → `broadcast::Receiver<HostEvent>` (text deltas, tool calls, `SendMessage`, A2A, group posts, routines, interrupts, errors). |

## Behaviour summary

- **Turn loop**: up to 5 000 LLM steps (a safety net); the tool calls of one step run concurrently and are fed back in call order; every tool result is clamped to 100 000 characters and every call has a per-tool timeout. In-turn reminders (`SendMessageReminder` after more than 6 silent calls, one early-result reminder per silent streak, start-of-turn ack after more than 1 silent call) are injected into the model context only. After a user turn that owes a reply the agent is nudged up to 3 times (`REPLY_NUDGE_PROMPT`), then once more when it acknowledged and went silent behind tool calls. Plain assistant text is persisted as inner monologue, never delivered.
- **Reply delivery**: kickstarts, user acknowledgements (`enforce_start_of_turn_ack`, on by default), and reply/closing nudges require a native `SendMessage` call through provider tool choice until delivery succeeds. Printing its JSON arguments as assistant text does not send anything or satisfy closing delivery. Exhausted reply nudges emit an error instead of reporting a successful silent turn. Quiet background runs and delegated task turns are exempt.
- **Lanes and preemption**: three FIFO lanes, `user` > `agent` > `background`. A new user message interrupts a room turn always, and a 1:1 run once it has dispatched its model call (a superseded queued user turn is skipped and its text carried by the newer one). A `priority: true` peer message interrupts non-user work only. A user turn waiting more than 120 s behind an active run trips a watchdog (interrupt, 30 s grace, escape). An unacknowledged user message is redriven with a `[System recovery]` hidden turn 5 s after the agent goes idle, at most 3 times.
- **A2A**: `SendToAgent` validates (empty / self / unknown / 8 000-char clamp), records the outbound entry, queues the inbound message and wakes the recipient with a hidden `[agent]` turn where silence is allowed. Group targets post into the room instead (priority ignored, images dropped, with a note).
- **Groups**: up to 3 rounds, 10 member messages per room turn, 2 messages per member turn, at most 6 members; `@handle` since the last user message selects responders, `@everyone`/`@all` or no mention selects all; `(pass)` is dropped; a member preempted by a DM is retried up to 3 times with a redelivery note; a failed member turn is a pass. Room turns run on the lane of whatever posted (user or agent), un-hidden, with the member prompt layered on top of the regular system-prompt sections and the full tool set. Room history lives only in the room's `store.db` (the local user renders as `User:`); a member's own room turns (the `[Group chat: "…"]` prompt, tool calls and what it posted) are persisted in its transcript and stay in its 1:1 context.
- **Memory**: `update_state` writes to `profile.md` / `log/YYYY-MM.md` per scope (agent, shared user shard, joined project shard) with the original's file headers and `- (YYYY-MM-DD) fact` lines; the agent tier is recalled newest first, shared user facts by `log2(importance) + age/30d`; the rendered memory block is frozen in KV and reused until the compaction epoch changes (`GNS_DISABLE_MEMORY_FREEZE=1` turns that off); every 6 memorable user turns an `[episode]` summary is written. After every memorable, non-hidden user turn an extraction pass (`memory_extraction`, on by default) runs inside settle: it sees the in-prompt recall plus up to 10 relevant archive facts and applies `profile:` / `log:` / `note:` / `remove:` lines. Once an agent has been idle for `dream_after_idle` (30 min) with at least 3 new log facts, a consolidation pass ("dreaming") promotes durable facts into `profile.md`; `host.dream(agent)` runs it on demand.
- **Turn shaping**: user messages are addressed `t<n>u` and rendered with `<incoming_message_id>`, `<timestamp>` and `<user_query>` wrappers plus the reply reminder; `SendMessage` returns `Message sent to user. (id: t<n>s<k>)` so the model can `reply_to`; hidden prompts carry `[SAND_HIDDEN_PROMPT]` (and `[SAND_TRUSTED_AUTOMATION_PROMPT]` on schedule/manual routine wakes); an `<automation_status>` reminder and an `<agent_profile_update>` block are added to the turn when they changed. When the user @-mentions teammates, a hidden context note listing them (with ids) is persisted after the message.
- **Background shell**: `Shell` blocks up to `block_until_ms` (default 30 000; `0` starts in the background); a command that outlives it is moved to the background, not killed, its output streams to `workspace/.gns/terminals/<shellId>.txt` (`AwaitShell` polls it), and a hidden `[A background command just completed]` turn on the background lane delivers the outcome. The shell's cwd persists across calls per agent.
- **Routines**: `automation.json` + `runs.json` per routine (≤ 50 per agent, 20 runs kept), the original file format; 5-field cron (day-of-month OR day-of-week when both are set), `@hourly`…, `@every 30m`, `CRON_TZ=` / `TZ=` prefixes, local time when no zone is configured; listener triggers as above; fired as hidden `[routine]` turns on the background lane. A slot missed while the host was down fires once, late.
- **Compaction**: the summarization thresholds of the original (start in the background when the unused context is ≤ 10 000 tokens or ≤ 10 %, block before a turn when far over the window); the transcript is split at the last user message, the older part is summarised with the original 9-section summary prompt into a `Divider` entry (which records `summarized_through`, so nothing is duplicated and full history stays on disk), re-injected as `[Previous conversation summary]: …`, and the compaction epoch increments. `AgentHostConfig::context_window_tokens` (default 200 000) sets the window.
- **Local safety**: `Shell` starts with a cleared environment plus an allowlist (`PATH`, `HOME`, locale, …), so host secrets such as API keys never reach child processes; `Read` refuses files over 100 000 characters (use `offset`/`limit`); `file://` images must lie inside the agent's directories; add `ShellGuardPolicy` for command review with human approval.
- **Lifecycle**: agents are minted with `introductionPending`; the kickstart runs when requested (`AgentSpec::kickstart`) or via `host.kickstart_agent`, on the user lane, and a user message dispatched afterwards supersedes it like any other run. `delete_agent` stops the actor and deletes its directory (rooms skip a deleted member when they resolve members); `delete_group` / `update_group` manage rooms. `broadcast_to_agents(targets, text)` returns `(total, scheduled)`.
- **Widgets and reactions**: a widget or secret request ends the turn and blocks further sends until the user answers (`respond_to_widget` stamps the answer, `dismiss_widget` declines; unanswered widgets are summarised for the next turn); `react_to_message` toggles the user's emoji on an agent message, and the agent's `ReactToMessage` counts as delivery.

## On-disk layout

```text
<root>/
  coordination.db       artifacts + grants + tasks + result outbox (WAL)
  artifacts/<id>/content immutable snapshot, independent of agent folders
  agents/<agentId>/
    store.db            kv + transcript_entries (WAL)
    profile.json        { name, description, title, avatarShape, avatarColor }
    settings.json       { notifyOnAgentUpdates, hiddenFromSidebar, tools, mcpServers }
    projects.json       projects this agent has joined
    workspace/          Shell cwd; Read root (plus the agent dir); .gns/terminals/<shellId>.txt
    memory/profile.md   memory/log/YYYY-MM.md
    automations/<slug>/automation.json + runs.json
  agents/<groupId>/group.json   { version, memberIds }  + profile.json (name, description) + store.db (room history)
  user-memory/agents/<agentId>/           shared user memory, one shard per writer
  projects/<slug>/project.md              project name/description
  projects/<slug>/memory/agents/<agentId>/
```

## Development

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Crate and command names use `gns` (short for Genius). Product-specific
environment variables use `GNS_*`, including `GNS_MODEL`, `GNS_PROVIDER`,
`GNS_TZ`, `GNS_USER_NAME`, and `GNS_DISABLE_MEMORY_FREEZE`. The default data
directory is `.gns-data`; server settings are stored in `gns-server.json`.
To reuse an existing data directory, pass its path with `--root` and rename
its server settings file to `gns-server.json` while the server is stopped.
Stored transcript markers and database keys retain their original format
so existing agent data remains readable.
