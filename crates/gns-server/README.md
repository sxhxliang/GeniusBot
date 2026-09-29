# Genius Bot Server (`gns-server`) — API + web debug console

An HTTP API over `gns_runtime::AgentHost` (the same host `gns-cli` drives)
with a React console compiled into the binary. Start the server and open the
browser: no separate frontend process.

中文：`gns-server` 为 SDK 提供 REST + SSE 接口，并内置 React 调试前端（模型 / 接口地址 / Key
设置、模型调用日志、Agent 信息、会话交互、群聊、审批、事件流）。前端打包进二进制，启动服务即可在浏览器使用。

## Build and run

Install the published package, including the prebuilt console:

```bash
cargo install gns-server --locked
gns-server --mock
```

Or build from the repository root:

```bash
cd crates/gns-server/web
npm install
npm run build                  # → web/dist, embedded by build.rs
cd ../../..
cargo run -p gns-server -- --root ./.gns-data     # http://127.0.0.1:8787

# one step: build.rs runs npm itself
GNS_SERVER_BUILD_WEB=1 cargo build -p gns-server --release
./target/release/gns-server --root ./.gns-data
```

Without `web/dist` the binary still builds and the API works; `/` shows how to
build the console.

Model flags are the same as `gns-cli` (`--model`, `--provider`, `--base-url`,
`--api-key`, `--api-key-env`, `--no-stream`, `--mock`) and override the saved
settings. Usually you set them in the console instead (⚙ Settings): provider,
model (with **Fetch list** from `GET {base}/models`), base URL, API key, key
variable, temperature, max tokens and streaming, plus **Test connection**.
They are saved to `<root>/gns-server.json` (mode 0600, the key in plain text)
and apply to the next model call of every agent — no restart.

| flag | default | |
| --- | --- | --- |
| `--root` | `.gns-data` | data directory (agents, memory, settings) |
| `--host` / `--port` | `127.0.0.1` / `8787` | listen address (`GNS_SERVER_HOST`, `GNS_SERVER_PORT`) |
| `--token` | — | require `Authorization: Bearer <token>` (or `?token=`) on `/api`; **mandatory off loopback**, since agents run shell commands |
| `--llm-log-capacity` | 300 | model calls kept in memory |
| `--llm-log-file` | — | also append every call (full request + response) as JSON Lines |
| `--no-guard` | off | drop the default `ShellGuardPolicy` (dangerous commands need approval) |
| `--tz`, `--user-name` | — | as in `gns-cli` |

## Console

* **Chat** per agent: user messages, `SendMessage` replies (widgets with
  buttons, attachments, reactions, reply threads), inner monologue, tool calls
  with arguments and results, hidden prompts (toggle), A2A traffic, summaries;
  live streaming text and running tools while a turn is in flight; inline
  approval cards.
* **Info / System prompt / Memory / Tools & MCP / Routines / Transcript /
  Model calls** tabs per agent: profile and settings editing, usage, the
  assembled system prompt, three-tier memory (write facts, dream), tool
  toggles, MCP servers (add stdio/http, enable, remove, sync), routines (run
  now, webhook events), raw transcript entries.
* **Model calls**: every request the host sent (turn steps, subagents,
  summaries, memory extraction, dreams, episodes) with purpose, agent,
  duration, tokens, the full system prompt, messages, tool schemas and the
  response.
* **Groups**, **Events** (raw host event stream), **Approvals**, **Broadcast**.
* **Task cards and files** in chats and groups: status, executor verification
  notes, authenticated downloads, and resume/cancel actions. Cards update in
  place as the task progresses. A group task delivers to its original group.

For UI work run `npm run dev` in `web/` (Vite on :5173, proxying `/api` to
`GNS_SERVER_URL`, default `http://127.0.0.1:8787`).

## API

All JSON under `/api`. `{id}` accepts an agent/group id or name. Errors are
`{"error": "…"}` with 400/401/404/500.

| method | path | |
| --- | --- | --- |
| GET | `/info` | version, root, config, host tools, model status |
| GET / PUT | `/model` | settings (key masked) + status; PUT a partial patch (`apiKey: ""` clears the key, `null` clears temperature/maxTokens) |
| POST | `/model/test`, `/model/models` | ping / list models with the saved settings plus an unsaved draft body |
| GET / DELETE | `/llm-logs?agent=&limit=` | model-call summaries, newest first |
| GET | `/llm-logs/{n}` | one call with full request and response |
| GET | `/events?after=<seq>` | Server-Sent Events: `event: host` (a `HostEvent`) and `event: server` (`llm-log`, `model-changed`), each `{seq, ts, channel, kind, data}` |
| GET | `/events/recent?limit=` | buffered recent events (no deltas) |
| GET / POST | `/agents` | list (with running lane) / create `{name, description, title?, kickstart?}` |
| GET / PATCH / DELETE | `/agents/{id}` | detail / profile patch `{name?, description?, title?}` / delete |
| PATCH | `/agents/{id}/settings` | `{notify_on_agent_updates?, hidden_from_sidebar?}` |
| POST | `/agents/{id}/messages` | `{text, replyTo?, isFork?, images?, wait?}` → `{entryId}` (or `{entryId, result}` with `wait`) |
| GET | `/agents/{id}/transcript?before=&limit=` | `{items: [{seq, entry}], hasMore}` oldest first |
| GET | `/agents/{id}/prompt` | assembled system prompt |
| GET / POST | `/agents/{id}/memory` | recall / write `{fact, tier: profile\|log\|note}` |
| POST | `/agents/{id}/dream`, `/agents/{id}/kickstart` | |
| GET / PUT | `/agents/{id}/tools`, `/agents/{id}/tools/{tool}` | list / `{enabled}` |
| GET / POST | `/agents/{id}/mcp` | status + configs / attach `McpServerConfig` |
| PUT / DELETE | `/agents/{id}/mcp/{name}` | `{enabled}` / remove; POST `/agents/{id}/mcp/sync` reconnects |
| GET / POST | `/agents/{id}/routines`, `/agents/{id}/routines/{rid}/run` | |
| POST / DELETE | `/agents/{id}/widgets/{entryId}` | answer `{value}` / dismiss |
| POST | `/agents/{id}/reactions` | `{entryId, emoji}` (toggles) |
| GET / POST | `/groups` | list / create `{name, description, members: [id\|name]}` |
| GET / PATCH / DELETE | `/groups/{id}` | `{group, history}` / `{name?, description?, members?}` / delete |
| POST | `/groups/{id}/messages` | post as the user `{text}` (the round runs in the background) |
| GET / POST | `/approvals`, `/approvals/{id}` | pending / `{approved, reason?}` |
| POST | `/broadcast` | `{text, targets?}` |
| POST | `/webhooks/{source}` | fire listener-triggered routines with the body as payload |
| GET / POST | `/tasks` | list / delegate `{requester, target_id, task, require_files?, files?, artifact_ids?, group?}`; both agents must be members when `group` is supplied |
| GET | `/tasks/{id}` | task status, origin and artifact references |
| POST | `/tasks/{id}/cancel` | cancel unfinished work |
| POST | `/tasks/{id}/resume` | resume blocked work with `{instructions?: string}` |
| GET | `/artifacts/{id}` | immutable file metadata (name, size, SHA-256, creator) |
| GET | `/artifacts/{id}/download` | verified original bytes with attachment disposition; same owner authentication as other APIs |

```bash
curl -s localhost:8787/api/agents -d '{"name":"Planner","description":"You plan."}' -H 'content-type: application/json'
curl -s localhost:8787/api/agents/Planner/messages -d '{"text":"hi","wait":true}' -H 'content-type: application/json'
curl -N localhost:8787/api/events
```

`POST /tasks` queues work and returns immediately. Completion automatically
posts the files to the original conversation and wakes the requester. Download
requests accept artifact IDs only, never filesystem paths. The console sends
the configured bearer token; authenticated owners can access all host artifacts,
while agents use grant-checked `FetchArtifact` in their own tool context.

For an isolated offline demo, run `cargo run -p gns-server --example artifact_demo`,
open `http://127.0.0.1:8799`, and enter token `artifact-demo`. It demonstrates DM
and group delivery, plus a blocked task that can be resumed or cancelled. The
Python file is a fixture, not executed by the demo. All data is temporary.
