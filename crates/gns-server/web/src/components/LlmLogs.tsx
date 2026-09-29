// Model-call log: every request the host sent to the model, with its response.

import { useEffect, useMemo, useState } from "react";
import { api, type AgentRow, type LlmLog, type LlmLogSummary, type LlmMessage } from "../api";
import { useBusEvents } from "../bus";
import { Collapsible, CopyButton, ErrorText, JsonView, fmtDuration, fmtNum, fmtTime, pretty, useLoad } from "../ui";

export function LlmLogsView({ agents, agentId }: { agents: AgentRow[]; agentId?: string }) {
  const { data, error, reload, setData } = useLoad(() => api.llmLogs(agentId), [agentId]);
  const [selected, setSelected] = useState<number | undefined>();
  const [purpose, setPurpose] = useState("");
  const [status, setStatus] = useState("");
  const [agentFilter, setAgentFilter] = useState("");
  const names = useMemo(() => new Map(agents.map((a) => [a.id, a.name])), [agents]);

  // Upsert live summaries (each call is published when it starts and when it ends).
  useBusEvents((e) => {
    if (e.kind !== "llm-log") return;
    const summary = e.data as unknown as LlmLogSummary;
    if (agentId && summary.agentId !== agentId) return;
    setData((prev) => {
      const logs = prev?.logs ?? [];
      const i = logs.findIndex((l) => l.id === summary.id);
      const next = i >= 0 ? logs.map((l) => (l.id === summary.id ? summary : l)) : [summary, ...logs];
      return { logs: next.slice(0, 500) };
    });
  });

  const logs = (data?.logs ?? []).filter(
    (l) =>
      (!purpose || l.purpose === purpose) &&
      (!status || l.status === status) &&
      (!agentFilter || (agentFilter === "-" ? !l.agentId : l.agentId === agentFilter)),
  );
  const purposes = [...new Set((data?.logs ?? []).map((l) => l.purpose))].sort();
  const totals = logs.reduce(
    (acc, l) => ({ prompt: acc.prompt + (l.usage?.promptTokens ?? 0), completion: acc.completion + (l.usage?.completionTokens ?? 0) }),
    { prompt: 0, completion: 0 },
  );

  return (
    <div className={`split ${selected !== undefined ? "with-detail" : ""}`}>
      <div className="split-main">
        <div className="mb-5"><h2>Model calls</h2><p className="mt-1 text-xs text-muted">Inspect requests, responses, and token usage.</p></div>
        <div className="toolbar">
          <select value={purpose} onChange={(e) => setPurpose(e.target.value)}>
            <option value="">all purposes</option>
            {purposes.map((p) => (
              <option key={p}>{p}</option>
            ))}
          </select>
          <select value={status} onChange={(e) => setStatus(e.target.value)}>
            <option value="">all statuses</option>
            {["pending", "ok", "error", "cancelled"].map((s) => (
              <option key={s}>{s}</option>
            ))}
          </select>
          {!agentId && (
            <select value={agentFilter} onChange={(e) => setAgentFilter(e.target.value)}>
              <option value="">all agents</option>
              <option value="-">no agent (auxiliary)</option>
              {agents.map((a) => (
                <option key={a.id} value={a.id}>
                  {a.name}
                </option>
              ))}
            </select>
          )}
          <span className="muted small">
            {logs.length} call(s) · {fmtNum(totals.prompt)} in / {fmtNum(totals.completion)} out tokens
          </span>
          <span className="spacer" />
          <button className="btn small" onClick={reload}>
            Refresh
          </button>
          {!agentId && (
            <button
              className="btn small danger"
              onClick={async () => {
                await api.clearLlmLogs();
                setSelected(undefined);
                reload();
              }}
            >
              Clear
            </button>
          )}
        </div>
        <ErrorText error={error} />
        <div className="table-wrap">
          <table className="table">
            <thead>
              <tr>
                <th>#</th>
                <th>time</th>
                <th>purpose</th>
                {!agentId && <th>agent</th>}
                <th>model</th>
                <th>status</th>
                <th>duration</th>
                <th>msgs</th>
                <th>tokens in/out</th>
                <th>output</th>
              </tr>
            </thead>
            <tbody>
              {logs.map((l) => (
                <tr key={l.id} className={`${selected === l.id ? "selected" : ""} clickable`} onClick={() => setSelected(l.id)}>
                  <td className="mono"><button className="rounded px-1 font-medium text-accent underline decoration-accent/30 underline-offset-4" onClick={(e) => { e.stopPropagation(); setSelected(l.id); }} aria-label={`Inspect model call ${l.id}`}>{l.id}</button></td>
                  <td className="nowrap">{fmtTime(l.startedAtMs)}</td>
                  <td>
                    <span className={`tag purpose-${l.purpose}`}>{l.purpose}</span>
                  </td>
                  {!agentId && <td>{l.agentId ? names.get(l.agentId) ?? l.agentId : <span className="muted">—</span>}</td>}
                  <td className="mono small">{l.model}</td>
                  <td>
                    <span className={`status status-${l.status}`}>{l.status}</span>
                  </td>
                  <td className="nowrap">{fmtDuration(l.durationMs)}</td>
                  <td>{l.messageCount}</td>
                  <td className="nowrap">
                    {l.usage ? `${fmtNum(l.usage.promptTokens)} / ${fmtNum(l.usage.completionTokens)}` : "—"}
                    {!!l.usage?.cachedPromptTokens && <span className="muted small"> ({fmtNum(l.usage.cachedPromptTokens)} cached)</span>}
                  </td>
                  <td className="ellipsis">
                    {l.error ? (
                      <span className="error-inline">{l.error}</span>
                    ) : (
                      <>
                        {l.toolCalls.map((t, i) => (
                          <span key={i} className="tag">
                            {t}
                          </span>
                        ))}
                        <span className="muted">{l.textPreview}</span>
                      </>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {logs.length === 0 && <div className="empty">No model calls recorded yet.</div>}
        </div>
      </div>
      {selected !== undefined && <LlmLogDetail id={selected} names={names} onClose={() => setSelected(undefined)} />}
    </div>
  );
}

function LlmLogDetail({ id, names, onClose }: { id: number; names: Map<string, string>; onClose: () => void }) {
  const { data: log, error, reload } = useLoad(() => api.llmLog(id), [id]);
  const [raw, setRaw] = useState(false);

  // A pending call fills in when it finishes.
  useBusEvents((e) => {
    if (e.kind === "llm-log" && (e.data as { id?: number }).id === id && (e.data as { status?: string }).status !== "pending") reload();
  });
  useEffect(() => setRaw(false), [id]);

  return (
    <div className="split-detail">
      <div className="detail-head">
        <h3>Model call #{id}</h3>
        <span className="spacer" />
        <label className="chip">
          <input type="checkbox" checked={raw} onChange={(e) => setRaw(e.target.checked)} /> raw JSON
        </label>
        {log && <CopyButton text={pretty(log)} label="Copy JSON" />}
        <button className="btn ghost" onClick={onClose} aria-label="Back to model calls">
          Close
        </button>
      </div>
      <ErrorText error={error} />
      {log && (raw ? <JsonView value={log} /> : <LogBody log={log} names={names} />)}
    </div>
  );
}

function LogBody({ log, names }: { log: LlmLog; names: Map<string, string> }) {
  const { request, response } = log;
  return (
    <div className="detail-body">
      <dl className="kv">
        <dt>status</dt>
        <dd>
          <span className={`status status-${log.status}`}>{log.status}</span> {log.stopReason && <span className="muted">stop: {log.stopReason}</span>}
        </dd>
        <dt>purpose</dt>
        <dd>{log.purpose}</dd>
        <dt>agent</dt>
        <dd>{log.agentId ? `${names.get(log.agentId) ?? ""} (${log.agentId})` : "—"}</dd>
        <dt>model</dt>
        <dd className="mono">{log.model}</dd>
        <dt>started</dt>
        <dd>
          {fmtTime(log.startedAtMs, true)} · {fmtDuration(log.durationMs)}
        </dd>
        <dt>usage</dt>
        <dd>
          {log.usage
            ? `${fmtNum(log.usage.promptTokens)} prompt (${fmtNum(log.usage.cachedPromptTokens)} cached) · ${fmtNum(log.usage.completionTokens)} completion · ${fmtNum(log.usage.totalTokens)} total`
            : "—"}
        </dd>
        <dt>options</dt>
        <dd className="mono small">{JSON.stringify(request.options)}</dd>
      </dl>
      {log.error && <div className="error-text">{log.error}</div>}

      <h4>Response</h4>
      {response ? (
        <div className="stack">
          {response.reasoning && (
            <Collapsible title="reasoning" defaultOpen>
              <pre className="code">{response.reasoning}</pre>
            </Collapsible>
          )}
          {response.text && <pre className="code">{response.text}</pre>}
          {response.tool_calls.map((c) => (
            <Collapsible key={c.id} defaultOpen title={<>⚙ {c.name}</>} right={<code className="muted small">{c.id}</code>}>
              <JsonView value={c.arguments} />
            </Collapsible>
          ))}
          {!response.text && !response.tool_calls.length && <div className="muted">(empty response)</div>}
        </div>
      ) : (
        <div className="muted">{log.status === "pending" ? "waiting for the model…" : "no response"}</div>
      )}

      <h4>
        Request <span className="muted small">({request.messages.length} messages, {request.tools.length} tools)</span>
      </h4>
      <Collapsible title={`system prompt (${fmtNum(request.system.length)} chars)`} right={<CopyButton text={request.system} />}>
        <pre className="code">{request.system}</pre>
      </Collapsible>
      {request.tools.length > 0 && (
        <Collapsible title={`tools: ${request.tools.map((t) => t.name).join(", ")}`}>
          {request.tools.map((t) => (
            <Collapsible key={t.name} title={<b>{t.name}</b>}>
              <div className="small">{t.description}</div>
              <JsonView value={t.parameters} />
            </Collapsible>
          ))}
        </Collapsible>
      )}
      <div className="messages">
        {request.messages.map((m, i) => (
          <LlmMessageView key={i} message={m} index={i} last={i === request.messages.length - 1} />
        ))}
      </div>
    </div>
  );
}

function LlmMessageView({ message, index, last }: { message: LlmMessage; index: number; last: boolean }) {
  if ("User" in message) {
    const { text, images } = message.User;
    return (
      <Collapsible defaultOpen={last} className="msg msg-user" title={<>#{index} user · {text.length} chars</>} right={<CopyButton text={text} />}>
        <pre className="code">{text}</pre>
        {images.length > 0 && <JsonView value={images} />}
      </Collapsible>
    );
  }
  if ("Assistant" in message) {
    const { text, tool_calls } = message.Assistant;
    return (
      <Collapsible
        defaultOpen={last}
        className="msg msg-assistant"
        title={
          <>
            #{index} assistant {tool_calls.map((c) => <span key={c.id} className="tag">{c.name}</span>)}
          </>
        }
      >
        {text && <pre className="code">{text}</pre>}
        {tool_calls.map((c) => (
          <div key={c.id}>
            <div className="label">
              {c.name} <code className="muted">{c.id}</code>
            </div>
            <JsonView value={c.arguments} />
          </div>
        ))}
      </Collapsible>
    );
  }
  const results = message.ToolResults;
  return (
    <Collapsible defaultOpen={last} className="msg msg-tool" title={<>#{index} tool results · {results.map((r) => r.name).join(", ")}</>}>
      {results.map((r) => (
        <div key={r.call_id}>
          <div className="label">
            {r.name} <code className="muted">{r.call_id}</code>
          </div>
          <pre className="code">{r.content}</pre>
        </div>
      ))}
    </Collapsible>
  );
}
