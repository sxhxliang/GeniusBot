// Agent inspection tabs: info, system prompt, memory, tools & MCP, routines, raw transcript.

import { useState } from "react";
import { api, type McpServerConfig, type MemoryRecall, type TranscriptItem } from "../api";
import { eventAgentId, useBusEvents, useDebounced } from "../bus";
import { Collapsible, CopyButton, ErrorText, JsonView, fmtNum, fmtTime, laneLabel, useAction, useLoad } from "../ui";

export function InfoTab({ agentId, onDeleted }: { agentId: string; onDeleted: () => void }) {
  const { data: a, error, reload } = useLoad(() => api.agent(agentId), [agentId]);
  const refresh = useDebounced(reload, 500);
  useBusEvents((e) => {
    if (eventAgentId(e) === agentId && ["run-started", "turn-ended", "agent-updated", "a2a-queued"].includes(e.kind)) refresh();
  });
  const [edit, setEdit] = useState<{ name: string; title: string; description: string } | null>(null);
  const action = useAction();
  if (!a) return <ErrorText error={error} />;

  const saveProfile = async () => {
    if (!edit) return;
    if (await action.run(() => api.updateAgent(agentId, edit))) {
      setEdit(null);
      reload();
    }
  };
  const toggleSetting = (key: "notify_on_agent_updates" | "hidden_from_sidebar", value: boolean) =>
    action.run(() => api.updateAgentSettings(agentId, { [key]: value })).then(reload);

  return (
    <div className="panel flex flex-col gap-5 [&>section]:shrink-0">
      <section className="card">
        <div className="card-head">
          <h3>Profile</h3>
          <span className="spacer" />
          {!edit && (
            <button className="btn small" onClick={() => setEdit({ name: a.profile.name, title: a.profile.title, description: a.profile.description })}>
              Edit
            </button>
          )}
        </div>
        {edit ? (
          <div className="form">
            <label>
              Name
              <input value={edit.name} onChange={(e) => setEdit({ ...edit, name: e.target.value })} />
            </label>
            <label>
              Title
              <input value={edit.title} onChange={(e) => setEdit({ ...edit, title: e.target.value })} />
            </label>
            <label>
              Description (persona)
              <textarea rows={5} value={edit.description} onChange={(e) => setEdit({ ...edit, description: e.target.value })} />
            </label>
            <div className="row">
              <button className="btn primary small" disabled={action.busy} onClick={saveProfile}>
                Save
              </button>
              <button className="btn small ghost" onClick={() => setEdit(null)}>
                Cancel
              </button>
            </div>
          </div>
        ) : (
          <dl className="kv">
            <dt>id</dt>
            <dd className="mono">
              {a.id} <CopyButton text={a.id} />
            </dd>
            <dt>name</dt>
            <dd>{a.profile.name}</dd>
            <dt>title</dt>
            <dd>{a.profile.title || <span className="muted">—</span>}</dd>
            <dt>description</dt>
            <dd className="pre">{a.profile.description || <span className="muted">—</span>}</dd>
            <dt>avatar</dt>
            <dd>
              {a.profile.avatarShape || "—"} {a.profile.avatarColor && <span className="swatch" style={{ background: a.profile.avatarColor }} />}
            </dd>
          </dl>
        )}
      </section>

      <section className="card">
        <h3>State</h3>
        <dl className="kv">
          <dt>run</dt>
          <dd>{a.activeLane ? `running on the ${laneLabel(a.activeLane)} lane` : "idle"}</dd>
          <dt>awaiting user</dt>
          <dd>{a.awaitingUser ? "yes (widget or secret request open)" : "no"}</dd>
          <dt>pending inbound</dt>
          <dd>{a.pendingInbound}</dd>
          <dt>groups</dt>
          <dd>{a.groups.length ? a.groups.map((g) => g.name).join(", ") : "—"}</dd>
          <dt>usage</dt>
          <dd>
            {fmtNum(a.usage.llmCalls)} calls · {fmtNum(a.usage.promptTokens)} prompt ({fmtNum(a.usage.cachedPromptTokens)} cached) ·{" "}
            {fmtNum(a.usage.completionTokens)} completion · {fmtNum(a.usage.totalTokens)} total
          </dd>
          <dt>data dir</dt>
          <dd className="mono small">
            {a.dataDir} <CopyButton text={a.dataDir} />
          </dd>
          <dt>workspace</dt>
          <dd className="mono small">
            {a.workspaceDir} <CopyButton text={a.workspaceDir} />
          </dd>
        </dl>
      </section>

      <section className="card">
        <h3>Settings</h3>
        <label className="check">
          <input
            type="checkbox"
            checked={a.settings.notifyOnAgentUpdates}
            onChange={(e) => toggleSetting("notify_on_agent_updates", e.target.checked)}
          />
          notify on agent updates
        </label>
        <label className="check">
          <input type="checkbox" checked={a.settings.hiddenFromSidebar} onChange={(e) => toggleSetting("hidden_from_sidebar", e.target.checked)} />
          hidden from sidebar
        </label>
        <Collapsible title="settings.json">
          <JsonView value={a.settings} />
        </Collapsible>
      </section>

      <section className="card">
        <h3>Actions</h3>
        <div className="row wrap">
          <button className="btn small" onClick={() => action.run(() => api.kickstart(agentId))}>
            Run kickstart
          </button>
          <button
            className="btn small"
            onClick={async () => {
              const r = await action.run(() => api.dream(agentId));
              if (r) alert(`Dream: ${r.added} fact(s) promoted to the profile, ${r.removed} removed.`);
            }}
          >
            Dream (consolidate memory)
          </button>
          <button
            className="btn small danger"
            onClick={async () => {
              if (!confirm(`Delete ${a.profile.name}? This removes its directory, transcript and memory.`)) return;
              try {
                await api.deleteAgent(agentId);
                onDeleted();
              } catch (e) {
                action.setError(e);
              }
            }}
          >
            Delete agent
          </button>
        </div>
        <ErrorText error={action.error} />
      </section>
    </div>
  );
}

export function PromptTab({ agentId }: { agentId: string }) {
  const { data, error, reload, loading } = useLoad(() => api.prompt(agentId), [agentId]);
  const prompt = data?.prompt ?? "";
  return (
    <div className="panel">
      <div className="toolbar">
        <span className="muted small">
          {fmtNum(prompt.length)} chars · ~{fmtNum(Math.round(prompt.length / 4))} tokens
        </span>
        <span className="spacer" />
        <CopyButton text={prompt} />
        <button className="btn small" onClick={reload} disabled={loading}>
          Refresh
        </button>
      </div>
      <ErrorText error={error} />
      <pre className="code tall">{prompt}</pre>
    </div>
  );
}

function RecallView({ recall }: { recall: MemoryRecall }) {
  const list = (records: MemoryRecall["profile"]) =>
    records.length ? (
      <ul className="facts">
        {records.map((r, i) => (
          <li key={i}>
            <span className="muted small">{fmtTime(r.created_at, true)}</span> {r.content}
          </li>
        ))}
      </ul>
    ) : (
      <div className="muted small">none</div>
    );
  return (
    <div className="recall">
      <div className="label">profile ({recall.profile.length})</div>
      {list(recall.profile)}
      <div className="label">recent log ({recall.recent.length})</div>
      {list(recall.recent)}
    </div>
  );
}

export function MemoryTab({ agentId, names }: { agentId: string; names: Map<string, string> }) {
  const { data, error, reload } = useLoad(() => api.memory(agentId), [agentId]);
  const refresh = useDebounced(reload, 500);
  useBusEvents((e) => {
    if (eventAgentId(e) === agentId && (e.kind === "memory-written" || e.kind === "memory-dreamed")) refresh();
  });
  const [fact, setFact] = useState("");
  const [tier, setTier] = useState<"profile" | "log" | "note">("log");
  const action = useAction();
  const write = async () => {
    const r = await action.run(() => api.writeMemory(agentId, fact, tier));
    if (r) {
      setFact("");
      reload();
    }
  };
  return (
    <div className="panel flex flex-col gap-5 [&>section]:shrink-0">
      <section className="card">
        <h3>Write a fact</h3>
        <div className="flex flex-wrap items-center gap-2">
          <input id="memory-fact" aria-label="Memory fact" className="basis-full md:min-w-48 md:flex-1 md:basis-auto" value={fact} placeholder="e.g. The user prefers concise answers" onChange={(e) => setFact(e.target.value)} />
          <select id="memory-tier" aria-label="Memory tier" value={tier} onChange={(e) => setTier(e.target.value as typeof tier)}>
            <option value="profile">profile</option>
            <option value="log">log</option>
            <option value="note">note</option>
          </select>
          <button className="btn primary small" disabled={!fact.trim() || action.busy} onClick={write}>
            Write
          </button>
          <button className="btn small" onClick={reload}>
            Refresh
          </button>
        </div>
        <ErrorText error={action.error} />
      </section>
      <ErrorText error={error} />
      {data && (
        <>
          <section className="card">
            <h3>Agent memory</h3>
            <RecallView recall={data.agent} />
          </section>
          <section className="card">
            <h3>Shared user memory</h3>
            {data.userShards.length === 0 && <div className="muted small">none</div>}
            {data.userShards.map((s) => (
              <Collapsible key={s.agentId} defaultOpen={s.agentId === agentId} title={`via ${names.get(s.agentId) ?? s.agentId}`}>
                <RecallView recall={s.recall} />
              </Collapsible>
            ))}
          </section>
          <section className="card">
            <h3>Projects</h3>
            {data.projects.length === 0 && <div className="muted small">not a member of any project</div>}
            {data.projects.map((p) => (
              <Collapsible key={p.slug} title={p.slug}>
                {p.shards.map((s) => (
                  <Collapsible key={s.agentId} title={`via ${names.get(s.agentId) ?? s.agentId}`}>
                    <RecallView recall={s.recall} />
                  </Collapsible>
                ))}
              </Collapsible>
            ))}
          </section>
        </>
      )}
    </div>
  );
}

export function ToolsTab({ agentId }: { agentId: string }) {
  const tools = useLoad(() => api.tools(agentId), [agentId]);
  const mcp = useLoad(() => api.mcp(agentId), [agentId]);
  const refresh = useDebounced(() => {
    tools.reload();
    mcp.reload();
  }, 400);
  useBusEvents((e) => {
    if (eventAgentId(e) === agentId && ["tools-changed", "mcp-server-connected", "mcp-server-failed"].includes(e.kind)) refresh();
  });
  const action = useAction();
  const run = (f: () => Promise<unknown>) => action.run(f).then(refresh);

  return (
    <div className="panel flex flex-col gap-5 [&>section]:shrink-0">
      <section className="card">
        <h3>Tools</h3>
        <ErrorText error={tools.error} />
        <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th>on</th>
              <th>name</th>
              <th>source</th>
              <th>description</th>
            </tr>
          </thead>
          <tbody>
            {tools.data?.tools.map((t) => (
              <tr key={t.name}>
                <td>
                  <input
                    type="checkbox"
                    checked={t.enabled}
                    disabled={t.name === "SendMessage" || t.source.kind === "mcp"}
                    onChange={(e) => run(() => api.setTool(agentId, t.name, e.target.checked))}
                  />
                </td>
                <td className="mono">{t.name}</td>
                <td>{t.source.kind === "mcp" ? `mcp:${t.source.server}` : "built-in"}</td>
                <td className="small">{t.description.length > 220 ? `${t.description.slice(0, 220)}…` : t.description}</td>
              </tr>
            ))}
          </tbody>
        </table>
        </div>
      </section>

      <section className="card">
        <div className="card-head">
          <h3>MCP servers</h3>
          <span className="spacer" />
          <button className="btn small" onClick={() => run(() => api.syncMcp(agentId))}>
            Sync / reconnect
          </button>
        </div>
        <ErrorText error={mcp.error || action.error} />
        {mcp.data?.servers.length === 0 && <div className="muted small">No MCP servers attached.</div>}
        {mcp.data?.servers.map((s) => {
          const config = mcp.data?.configs.find((c) => c.name === s.name);
          return (
            <div key={s.name} className="mcp-row">
              <span className={`dot ${!s.enabled ? "idle" : s.connected ? "ok" : "bad"}`} />
              <b>{s.name}</b>
              <span className="muted small">
                {!s.enabled ? "disabled" : s.connected ? `${s.tools.length} tool(s): ${s.tools.join(", ")}` : `not connected ${s.lastError ?? ""}`}
              </span>
              <span className="spacer" />
              <button className="btn tiny" onClick={() => run(() => api.setMcp(agentId, s.name, !s.enabled))}>
                {s.enabled ? "Disable" : "Enable"}
              </button>
              <button className="btn tiny danger" onClick={() => confirm(`Remove ${s.name}?`) && run(() => api.removeMcp(agentId, s.name))}>
                Remove
              </button>
              {config && (
                <Collapsible title="config">
                  <JsonView value={config} />
                </Collapsible>
              )}
            </div>
          );
        })}
        <AddMcpForm onAdd={(config) => run(() => api.addMcp(agentId, config))} busy={action.busy} />
      </section>
    </div>
  );
}

function AddMcpForm({ onAdd, busy }: { onAdd: (c: McpServerConfig) => void; busy: boolean }) {
  const [kind, setKind] = useState<"stdio" | "http">("stdio");
  const [name, setName] = useState("");
  const [command, setCommand] = useState("");
  const [url, setUrl] = useState("");
  const [extra, setExtra] = useState("");
  const [allowed, setAllowed] = useState("");
  const [error, setError] = useState("");

  const submit = () => {
    let map: Record<string, string> = {};
    try {
      map = extra.trim() ? JSON.parse(extra) : {};
    } catch {
      setError(`${kind === "stdio" ? "env" : "headers"} must be a JSON object`);
      return;
    }
    setError("");
    const parts = command.trim().split(/\s+/);
    const transport: McpServerConfig["transport"] =
      kind === "stdio" ? { type: "stdio", command: parts[0] ?? "", args: parts.slice(1), env: map } : { type: "http", url: url.trim(), headers: map };
    onAdd({
      name: name.trim(),
      transport,
      enabled: true,
      allowedTools: allowed.split(",").map((s) => s.trim()).filter(Boolean),
    });
  };
  return (
    <Collapsible title="Add server">
      <div className="form">
        <div className="row">
          <select value={kind} onChange={(e) => setKind(e.target.value as "stdio" | "http")}>
            <option value="stdio">stdio</option>
            <option value="http">http</option>
          </select>
          <input value={name} placeholder="name (letters, digits, _ -)" onChange={(e) => setName(e.target.value)} />
        </div>
        {kind === "stdio" ? (
          <input value={command} placeholder="npx -y @modelcontextprotocol/server-filesystem /tmp" onChange={(e) => setCommand(e.target.value)} />
        ) : (
          <input value={url} placeholder="https://example.com/mcp" onChange={(e) => setUrl(e.target.value)} />
        )}
        <input
          value={extra}
          placeholder={kind === "stdio" ? 'env JSON, e.g. {"TOKEN":"…"}' : 'headers JSON, e.g. {"Authorization":"Bearer …"}'}
          onChange={(e) => setExtra(e.target.value)}
        />
        <input value={allowed} placeholder="allowed tools, comma separated (empty = all)" onChange={(e) => setAllowed(e.target.value)} />
        <div className="row">
          <button className="btn primary small" disabled={busy || !name.trim()} onClick={submit}>
            Add
          </button>
          {error && <span className="error-inline">{error}</span>}
        </div>
      </div>
    </Collapsible>
  );
}

export function RoutinesTab({ agentId }: { agentId: string }) {
  const { data, error, reload } = useLoad(() => api.routines(agentId), [agentId]);
  const refresh = useDebounced(reload, 500);
  useBusEvents((e) => {
    if (eventAgentId(e) === agentId && (e.kind === "routine-fired" || e.kind === "turn-ended")) refresh();
  });
  const [webhookName, setWebhookName] = useState("github");
  const [payload, setPayload] = useState('{"repo":"o/n","kind":"pr-opened","actor":"me","title":"x"}');
  const action = useAction();
  return (
    <div className="panel flex flex-col gap-5 [&>section]:shrink-0">
      <div className="toolbar">
        <span className="muted small">Routines are created by the agent itself (ask it to schedule something).</span>
        <span className="spacer" />
        <button className="btn small" onClick={reload}>
          Refresh
        </button>
      </div>
      <ErrorText error={error || action.error} />
      {data?.routines.length === 0 && <div className="empty">No routines.</div>}
      {data?.routines.map((r) => (
        <section key={r.id} className="card">
          <div className="card-head">
            <h3>{r.name}</h3>
            <span className={`tag ${r.isEnabled ? "ok" : ""}`}>{r.isEnabled ? "enabled" : "paused"}</span>
            <span className="spacer" />
            <button className="btn small" onClick={() => action.run(() => api.runRoutine(agentId, r.id))}>
              Run now
            </button>
          </div>
          <dl className="kv">
            <dt>id</dt>
            <dd className="mono">{r.id}</dd>
            <dt>trigger</dt>
            <dd>{r.triggerDescription}</dd>
            <dt>next run</dt>
            <dd>{fmtTime(r.nextRunAt, true)}</dd>
            <dt>last run</dt>
            <dd>{fmtTime(r.lastRunAt, true)}</dd>
            <dt>file</dt>
            <dd className="mono small">{r.filePath}</dd>
          </dl>
          <Collapsible title="prompt">
            <pre className="code">{r.prompt}</pre>
          </Collapsible>
          <Collapsible title={`runs (${r.runs.length})`}>
            <JsonView value={r.runs} maxHeight={300} />
          </Collapsible>
          <Collapsible title="trigger JSON">
            <JsonView value={r.trigger} />
          </Collapsible>
        </section>
      ))}
      <section className="card">
        <h3>Fire a webhook event</h3>
        <div className="muted small">Wakes every enabled routine with a matching listener trigger (github, slack, linear, …).</div>
        <div className="row">
          <input value={webhookName} onChange={(e) => setWebhookName(e.target.value)} style={{ maxWidth: 160 }} />
          <input value={payload} onChange={(e) => setPayload(e.target.value)} />
          <button
            className="btn small"
            onClick={async () => {
              let body: unknown = payload;
              try {
                body = JSON.parse(payload);
              } catch {
                // sent as a plain string
              }
              const r = await action.run(() => api.webhook(webhookName, body));
              if (r) alert(`${r.routines} routine(s) fired`);
            }}
          >
            Fire
          </button>
        </div>
      </section>
    </div>
  );
}

export function TranscriptTab({ agentId }: { agentId: string }) {
  const [items, setItems] = useState<TranscriptItem[]>([]);
  const [hasMore, setHasMore] = useState(false);
  const [kind, setKind] = useState("");
  const [search, setSearch] = useState("");
  const { error, reload } = useLoad(async () => {
    const page = await api.transcript(agentId, undefined, 300);
    setItems(page.items);
    setHasMore(page.hasMore);
    return page;
  }, [agentId]);
  const older = async () => {
    const page = await api.transcript(agentId, items[0]?.seq, 300);
    setItems([...page.items, ...items]);
    setHasMore(page.hasMore);
  };
  const shown = items
    .filter((i) => !kind || i.entry.kind === kind)
    .filter((i) => !search || JSON.stringify(i.entry).toLowerCase().includes(search.toLowerCase()))
    .reverse();
  return (
    <div className="panel">
      <div className="toolbar">
        <select value={kind} onChange={(e) => setKind(e.target.value)}>
          <option value="">all kinds</option>
          {["message", "send-message", "assistant-text", "tool-call", "profile-update", "divider"].map((k) => (
            <option key={k}>{k}</option>
          ))}
        </select>
        <input value={search} placeholder="search" onChange={(e) => setSearch(e.target.value)} style={{ maxWidth: 260 }} />
        <span className="muted small">
          {shown.length} / {items.length} entries (newest first)
        </span>
        <span className="spacer" />
        {hasMore && (
          <button className="btn small" onClick={older}>
            Load older
          </button>
        )}
        <button className="btn small" onClick={reload}>
          Refresh
        </button>
      </div>
      <ErrorText error={error} />
      <div className="stack">
        {shown.map(({ seq, entry }) => (
          <Collapsible
            key={seq}
            title={
              <>
                <span className="mono small muted">#{seq}</span> <span className="tag">{entry.kind}</span> <code>{entry.id}</code>{" "}
                <span className="muted small">{fmtTime(entry.timestamp_ms, true)}</span>
              </>
            }
          >
            <JsonView value={entry} />
          </Collapsible>
        ))}
      </div>
    </div>
  );
}
