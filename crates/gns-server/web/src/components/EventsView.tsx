// Raw event stream: every host and server event, newest first.

import { useEffect, useState } from "react";
import { api, type AgentRow, type BusEvent } from "../api";
import { eventAgentId, useBusEvents } from "../bus";
import { Collapsible, JsonView, fmtTime } from "../ui";

const MAX = 2000;

export function EventsView({ agents }: { agents: AgentRow[] }) {
  const [events, setEvents] = useState<BusEvent[]>([]);
  const [paused, setPaused] = useState(false);
  const [deltas, setDeltas] = useState(false);
  const [filter, setFilter] = useState("");
  const [agent, setAgent] = useState("");
  const names = new Map(agents.map((a) => [a.id, a.name]));

  useEffect(() => {
    api
      .recentEvents(500)
      .then((r) => setEvents(r.events.reverse()))
      .catch(() => {});
  }, []);

  useBusEvents((e) => {
    if (paused) return;
    if (!deltas && (e.kind === "text-delta" || e.kind === "thinking-delta")) return;
    setEvents((prev) => [e, ...prev.filter((p) => p.seq !== e.seq)].slice(0, MAX));
  });

  const shown = events.filter(
    (e) =>
      (!agent || eventAgentId(e) === agent) &&
      (!filter || e.kind.includes(filter) || JSON.stringify(e.data).toLowerCase().includes(filter.toLowerCase())),
  );
  const kinds = [...new Set(events.map((e) => e.kind))].sort();

  return (
    <div className="panel">
      <div className="mb-6"><h2>Event stream</h2><p className="mt-1 text-xs text-muted">Follow host and server activity as it happens.</p></div>
      <div className="toolbar">
        <input list="event-kinds" value={filter} placeholder="filter by kind or text" onChange={(e) => setFilter(e.target.value)} style={{ maxWidth: 260 }} />
        <datalist id="event-kinds">
          {kinds.map((k) => (
            <option key={k} value={k} />
          ))}
        </datalist>
        <select value={agent} onChange={(e) => setAgent(e.target.value)}>
          <option value="">all agents</option>
          {agents.map((a) => (
            <option key={a.id} value={a.id}>
              {a.name}
            </option>
          ))}
        </select>
        <label className="chip">
          <input type="checkbox" checked={deltas} onChange={(e) => setDeltas(e.target.checked)} /> streaming deltas
        </label>
        <label className="chip">
          <input type="checkbox" checked={paused} onChange={(e) => setPaused(e.target.checked)} /> pause
        </label>
        <span className="muted small">{shown.length} event(s)</span>
        <span className="spacer" />
        <button className="btn small" onClick={() => setEvents([])}>
          Clear
        </button>
      </div>
      <div className="flex flex-col gap-2">
        {shown.length === 0 && <div className="empty">No matching events. New activity will appear here.</div>}
        {shown.map((e) => {
          const id = eventAgentId(e);
          return (
            <Collapsible
              key={e.seq}
              title={
                <>
                  <span className="mono small muted">#{e.seq}</span> <span className="muted small">{fmtTime(e.ts)}</span>{" "}
                  <span className={`tag ${e.channel === "server" ? "" : "kind"} ${e.kind === "error" ? "danger" : ""}`}>{e.kind}</span>{" "}
                  {id && <b className="small">{names.get(id) ?? id}</b>} <span className="muted small">{summarize(e)}</span>
                </>
              }
            >
              <JsonView value={e.data} />
            </Collapsible>
          );
        })}
      </div>
    </div>
  );
}

function summarize(e: BusEvent): string {
  const d = e.data as Record<string, any>;
  const s =
    d.message?.content ?? d.text ?? d.content ?? d.reason ?? d.error ?? d.name ?? d.message ?? d.entry?.content ?? d.fact ?? d.command ?? "";
  const text = typeof s === "string" ? s : JSON.stringify(s);
  return text.length > 140 ? `${text.slice(0, 140)}…` : text;
}
