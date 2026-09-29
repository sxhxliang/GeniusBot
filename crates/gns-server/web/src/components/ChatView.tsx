// Conversation with one agent: transcript, live run state and the composer.

import { useEffect, useMemo, useRef, useState } from "react";
import { api, type ApprovalRequest, type BusEvent, type TranscriptEntry, type TranscriptItem } from "../api";
import { eventAgentId, useBusEvents, useDebounced } from "../bus";
import { Collapsible, ErrorText, Icon, JsonView, fmtNum, fmtTime, laneLabel, pretty, useAction } from "../ui";
import { ArtifactCards, TaskCard } from "./ArtifactCards";
import { ApprovalCard } from "./Approvals";

interface LiveTool {
  callId: string;
  name: string;
  args: unknown;
}

interface LiveState {
  running: boolean;
  lane?: string;
  source?: string;
  text: string;
  thinking: string;
  tools: LiveTool[];
  notes: { ts: number; text: string; error?: boolean }[];
  lastResult?: Record<string, any>;
}

const EMPTY_LIVE: LiveState = { running: false, text: "", thinking: "", tools: [], notes: [] };
const QUICK_REACTIONS = ["👍", "❤️", "😂", "🎉", "👀"];

export function ChatView({ agentId, agentName, approvals }: { agentId: string; agentName: string; approvals: ApprovalRequest[] }) {
  const [items, setItems] = useState<TranscriptItem[]>([]);
  const [hasMore, setHasMore] = useState(false);
  const [loadError, setLoadError] = useState<unknown>();
  const [live, setLive] = useState<LiveState>(EMPTY_LIVE);
  const [showHidden, setShowHidden] = useState(false);
  const [showThinking, setShowThinking] = useState(true);
  const [showTools, setShowTools] = useState(true);
  const [replyTo, setReplyTo] = useState<string | undefined>();
  const [input, setInput] = useState("");
  const send = useAction();
  const scroller = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);

  const merge = (incoming: TranscriptItem[]) =>
    setItems((prev) => {
      const bySeq = new Map(prev.map((i) => [i.seq, i]));
      incoming.forEach((i) => bySeq.set(i.seq, i));
      return [...bySeq.values()].sort((a, b) => a.seq - b.seq);
    });

  useEffect(() => {
    setItems([]);
    setLive(EMPTY_LIVE);
    setReplyTo(undefined);
    stickToBottom.current = true;
    api
      .transcript(agentId)
      .then((page) => {
        setItems(page.items);
        setHasMore(page.hasMore);
        setLoadError(undefined);
      })
      .catch(setLoadError);
    api
      .agent(agentId)
      .then((a) => setLive((l) => ({ ...l, running: !!a.activeLane, lane: a.activeLane ?? undefined })))
      .catch(() => {});
  }, [agentId]);

  const refreshTail = useDebounced(() => {
    api
      .transcript(agentId, undefined, 60)
      .then((page) => merge(page.items))
      .catch(() => {});
  }, 250);

  useBusEvents((e: BusEvent) => {
    if (eventAgentId(e) !== agentId || e.channel !== "host") return;
    const d = e.data as Record<string, any>;
    switch (e.kind) {
      case "run-started":
        setLive((l) => ({ ...l, running: true, lane: d.lane, source: d.source, text: "", thinking: "", tools: [] }));
        break;
      case "text-delta":
        setLive((l) => ({ ...l, text: l.text + d.text }));
        break;
      case "thinking-delta":
        setLive((l) => ({ ...l, thinking: l.thinking + d.text }));
        break;
      case "tool-call":
        if (d.status === "started") {
          setLive((l) => ({ ...l, text: "", thinking: "", tools: [...l.tools, { callId: d.call_id, name: d.name, args: d.args }] }));
        } else {
          setLive((l) => ({ ...l, tools: l.tools.filter((t) => t.callId !== d.call_id) }));
        }
        refreshTail();
        break;
      case "turn-ended":
        setLive((l) => ({ ...l, running: false, text: "", thinking: "", tools: [], lastResult: d.result }));
        refreshTail();
        break;
      case "interrupted":
        setLive((l) => ({ ...l, notes: [...l.notes, { ts: e.ts, text: `interrupted: ${d.reason}` }].slice(-5) }));
        break;
      case "retrying":
        setLive((l) => ({ ...l, notes: [...l.notes, { ts: e.ts, text: `retrying model call (${d.attempt}): ${d.error}`, error: true }].slice(-5) }));
        break;
      case "error":
        setLive((l) => ({ ...l, notes: [...l.notes, { ts: e.ts, text: d.message, error: true }].slice(-5) }));
        break;
      default:
        refreshTail();
    }
  });

  useEffect(() => {
    const el = scroller.current;
    if (el && stickToBottom.current) el.scrollTop = el.scrollHeight;
  }, [items, live]);

  const loadOlder = async () => {
    const first = items[0]?.seq;
    const el = scroller.current;
    const before = el ? el.scrollHeight - el.scrollTop : 0;
    const page = await api.transcript(agentId, first, 150);
    setHasMore(page.hasMore);
    merge(page.items);
    requestAnimationFrame(() => {
      if (el) el.scrollTop = el.scrollHeight - before;
    });
  };

  const submit = async () => {
    const text = input.trim();
    if (!text) return;
    const ok = await send.run(() => api.send(agentId, text, replyTo));
    if (ok) {
      setInput("");
      setReplyTo(undefined);
      stickToBottom.current = true;
      refreshTail();
    }
  };

  const visible = useMemo(
    () =>
      items.filter(({ entry }) => {
        if (entry.kind === "message" && entry.hidden && !showHidden) return false;
        if (entry.kind === "assistant-text" && !showThinking) return false;
        if (entry.kind === "tool-call" && !showTools) return false;
        return true;
      }),
    [items, showHidden, showThinking, showTools],
  );
  const myApprovals = approvals.filter((a) => a.agent_id === agentId);
  const byId = useMemo(() => new Map(items.map((i) => [i.entry.id, i.entry])), [items]);

  return (
    <div className="chat">
      <div className="chat-toolbar">
        <label className="chip">
          <input type="checkbox" checked={showThinking} onChange={(e) => setShowThinking(e.target.checked)} /> inner monologue
        </label>
        <label className="chip">
          <input type="checkbox" checked={showTools} onChange={(e) => setShowTools(e.target.checked)} /> tool calls
        </label>
        <label className="chip">
          <input type="checkbox" checked={showHidden} onChange={(e) => setShowHidden(e.target.checked)} /> hidden prompts
        </label>
        <span className="spacer" />
        {live.lastResult && !live.running && <RunSummary result={live.lastResult} />}
      </div>
      <div
        className="chat-scroll"
        ref={scroller}
        onScroll={(e) => {
          const el = e.currentTarget;
          stickToBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 60;
        }}
      >
        {hasMore && (
          <div className="center">
            <button className="btn small" onClick={loadOlder}>
              Load older
            </button>
          </div>
        )}
        <ErrorText error={loadError} />
        {visible.length === 0 && !loadError && (
          <div className="my-auto flex flex-col items-center gap-4 px-4 py-12 text-center">
            <span className="flex size-12 items-center justify-center rounded-2xl border border-line bg-surface text-accent"><Icon name="agent" className="size-6" /></span>
            <div><h2 className="text-lg">A conversation with {agentName}</h2><p className="mt-2 text-sm text-muted">Ask a question or give your agent something to work on.</p></div>
          </div>
        )}
        {visible.map(({ seq, entry }) => (
          <Entry key={seq} entry={entry} agentId={agentId} agentName={agentName} byId={byId} onReply={setReplyTo} />
        ))}
        {myApprovals.map((a) => (
          <ApprovalCard key={a.id} request={a} />
        ))}
        {live.running && <LiveBlock live={live} agentName={agentName} />}
        {live.notes.map((n, i) => (
          <div key={i} className={`note ${n.error ? "error" : ""}`}>
            {fmtTime(n.ts)} · {n.text}
          </div>
        ))}
      </div>
      <div className="composer">
        {replyTo && (
          <div className="reply-chip">
            Replying to <code>{replyTo}</code>
            <span className="muted"> {preview(byId.get(replyTo))}</span>
            <button className="btn small ghost" onClick={() => setReplyTo(undefined)}>
              ✕
            </button>
          </div>
        )}
        <div className="composer-row">
          <textarea
            id="agent-message"
            aria-label={`Message ${agentName}`}
            aria-describedby="agent-message-hint"
            value={input}
            placeholder={`Message ${agentName}…`}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
                e.preventDefault();
                submit();
              }
            }}
            rows={Math.min(8, Math.max(2, input.split("\n").length))}
          />
          <button className="btn primary" aria-label="Send message" disabled={send.busy || !input.trim()} onClick={submit}>
            {send.busy ? <span className="spinner" /> : <Icon name="arrow" />}<span className="hidden sm:inline">Send</span>
          </button>
        </div>
        <p id="agent-message-hint" className="mt-2 text-[10px] text-muted">Enter to send · Shift + Enter for a new line</p>
        <ErrorText error={send.error} />
      </div>
    </div>
  );
}

function preview(entry: TranscriptEntry | undefined): string {
  if (!entry) return "";
  const text = entry.kind === "send-message" ? entry.message.content ?? entry.message.widget?.prompt ?? "" : "content" in entry ? entry.content : "";
  return text.length > 80 ? `${text.slice(0, 80)}…` : text;
}

function RunSummary({ result }: { result: Record<string, any> }) {
  const usage = result.usage ?? {};
  return (
    <span className={`run-summary ${result.error ? "error" : ""}`} title={pretty(result)}>
      last turn: {result.steps} step(s) · {fmtNum(usage.totalTokens)} tokens · {result.sentMessageCount} sent
      {result.aborted ? " · aborted" : ""}
      {result.error ? ` · error: ${result.error}` : ""}
    </span>
  );
}

function LiveBlock({ live, agentName }: { live: LiveState; agentName: string }) {
  return (
    <div className="live">
      <div className="live-head">
        <span className="spinner" /> {agentName} is working
        {live.lane && <span className="tag">{laneLabel(live.lane)} lane</span>}
        {live.source && <span className="tag">{live.source}</span>}
      </div>
      {live.thinking && <div className="live-thinking">{live.thinking}</div>}
      {live.text && <div className="live-text">{live.text}</div>}
      {live.tools.map((t) => (
        <div key={t.callId} className="live-tool">
          ⚙ {t.name} <code>{pretty(t.args).slice(0, 200)}</code>
        </div>
      ))}
    </div>
  );
}

function Entry({
  entry,
  agentId,
  agentName,
  byId,
  onReply,
}: {
  entry: TranscriptEntry;
  agentId: string;
  agentName: string;
  byId: Map<string, TranscriptEntry>;
  onReply: (id: string) => void;
}) {
  const time = fmtTime(entry.timestamp_ms);
  switch (entry.kind) {
    case "message": {
      const replyRef = entry.reply_to ? <ReplyRef id={entry.reply_to} byId={byId} /> : null;
      if (entry.hidden) {
        return (
          <Collapsible
            className="entry hidden-prompt"
            title={
              <>
                <span className="tag">hidden {entry.role}</span> {firstLine(entry.content)}
              </>
            }
            right={<span className="muted small">{time}</span>}
          >
            <pre className="code">{entry.content}</pre>
          </Collapsible>
        );
      }
      if (entry.from_agent) {
        return (
          <div className="entry row-left">
            <div className="bubble a2a-in">
              <div className="bubble-meta">
                ← from <b>{entry.from_agent.name}</b>
                {entry.priority && <span className="tag warn">priority</span>}
                {entry.group_id && <span className="tag">room</span>}
                <span className="muted"> · {time}</span>
                <IdTag id={entry.id} />
              </div>
              <div className="bubble-text">{entry.content}</div>
              <ArtifactCards artifacts={entry.artifacts} />
            </div>
          </div>
        );
      }
      if (entry.to_agent) {
        return (
          <div className="entry row-right">
            <div className="bubble a2a-out">
              <div className="bubble-meta">
                → to <b>{entry.to_agent.name}</b>
                <span className="muted"> · {time}</span>
                <IdTag id={entry.id} />
              </div>
              <div className="bubble-text">{entry.content}</div>
              <ArtifactCards artifacts={entry.artifacts} />
            </div>
          </div>
        );
      }
      return (
        <div className="entry row-right">
          <div className="bubble user">
            <div className="bubble-meta">
              {entry.group_id ? `${entry.user_name ?? "You"} (room)` : entry.user_name ?? "You"}
              <span className="muted"> · {time}</span>
              <IdTag id={entry.id} />
            </div>
            {replyRef}
            <div className="bubble-text">{entry.content}</div>
              <ArtifactCards artifacts={entry.artifacts} />
            <Images images={entry.images} />
          </div>
        </div>
      );
    }
    case "send-message": {
      const m = entry.message;
      const who = entry.author?.name ?? agentName;
      return (
        <div className="entry row-left">
          <div className="bubble agent">
            <div className="bubble-meta">
              <b>{who}</b>
              {entry.group_id && <span className="tag">room</span>}
              {m.type !== "text" && <span className="tag">{m.type}</span>}
              <span className="muted"> · {time}</span>
              <IdTag id={entry.id} />
              <span className="bubble-actions">
                <button className="btn tiny ghost" onClick={() => onReply(entry.id)} title="Reply">
                  ↩
                </button>
                {QUICK_REACTIONS.map((emoji) => (
                  <button key={emoji} className="btn tiny ghost" onClick={() => api.react(agentId, entry.id, emoji)} title={`React ${emoji}`}>
                    {emoji}
                  </button>
                ))}
              </span>
            </div>
            {entry.reply_to && <ReplyRef id={entry.reply_to} byId={byId} />}
            {m.task ? <TaskCard task={m.task} /> : <><div className="bubble-text">{m.content}</div><ArtifactCards artifacts={m.artifacts} /></>}
            {m.url && (
              <div className="attachment">
                📎 {m.fileName ?? m.url} {m.alt && <span className="muted">— {m.alt}</span>}
              </div>
            )}
            <Images images={m.images} />
            {m.widget && <WidgetView entry={entry} agentId={agentId} />}
            {m.secret && (
              <div className="widget">
                🔑 Secret requested: <b>{m.secret.label}</b> {m.secret.description && <span className="muted">— {m.secret.description}</span>}
                <div className="muted small">Secret input is not supported in the console; answer in chat or dismiss.</div>
              </div>
            )}
            {!!entry.reactions?.length && (
              <div className="reactions">
                {entry.reactions.map((r, i) => (
                  <span key={i} className="reaction" title={r.by === "me" ? "you" : r.by}>
                    {r.emoji}
                  </span>
                ))}
              </div>
            )}
          </div>
        </div>
      );
    }
    case "assistant-text":
      return (
        <div className="entry monologue" title="Plain assistant text is inner monologue: it is never delivered to the user.">
          <span className="tag">monologue</span> <span className="muted small">{time}</span>
          <div className="monologue-text">{entry.content}</div>
        </div>
      );
    case "tool-call":
      return (
        <Collapsible
          className={`entry tool ${entry.is_error ? "error" : ""}`}
          title={
            <>
              <span className="tool-name">⚙ {entry.name}</span>
              {entry.result === undefined && <span className="tag">running</span>}
              {entry.is_error && <span className="tag danger">error</span>}
              <span className="muted small tool-args">{compactArgs(entry.args)}</span>
            </>
          }
          right={<span className="muted small">{time}</span>}
        >
          <div className="label">arguments</div>
          <JsonView value={entry.args} maxHeight={300} />
          {entry.result !== undefined && (
            <>
              <div className="label">result</div>
              <pre className="code" style={{ maxHeight: 400 }}>
                {entry.result}
              </pre>
            </>
          )}
        </Collapsible>
      );
    case "profile-update":
      return <div className="note">✎ profile update: {entry.summary}</div>;
    case "divider":
      return (
        <Collapsible className="entry divider" title={<>— conversation summarised (epoch {entry.compaction_epoch}) —</>}>
          <pre className="code">{entry.summary}</pre>
        </Collapsible>
      );
    default:
      return <JsonView value={entry} />;
  }
}

function WidgetView({ entry, agentId }: { entry: Extract<TranscriptEntry, { kind: "send-message" }>; agentId: string }) {
  const w = entry.message.widget!;
  const [custom, setCustom] = useState("");
  const action = useAction();
  const settled = entry.responded_value !== undefined || entry.widget_dismissed || entry.widget_skipped;
  const answer = (value: string) => action.run(() => api.answerWidget(agentId, entry.id, value));
  return (
    <div className="widget">
      <div className="widget-prompt">{w.prompt}</div>
      {w.helpText && <div className="muted small">{w.helpText}</div>}
      <div className="widget-options">
        {w.options.map((o, i) => (
          <button
            key={i}
            className={`btn small ${o.style === "primary" ? "primary" : o.style === "danger" ? "danger" : ""} ${
              entry.responded_value === (o.value ?? o.label) ? "chosen" : ""
            }`}
            disabled={settled || action.busy}
            title={o.description}
            onClick={() => answer(o.value ?? o.label)}
          >
            {o.label}
          </button>
        ))}
      </div>
      {w.allowCustom && !settled && (
        <div className="row">
          <input value={custom} placeholder="Custom answer" onChange={(e) => setCustom(e.target.value)} />
          <button className="btn small" disabled={!custom.trim()} onClick={() => answer(custom.trim())}>
            Answer
          </button>
        </div>
      )}
      {!settled && (
        <button className="btn tiny ghost" onClick={() => action.run(() => api.dismissWidget(agentId, entry.id))}>
          Dismiss
        </button>
      )}
      {entry.responded_value !== undefined && <div className="muted small">answered: {entry.responded_value}</div>}
      {entry.widget_dismissed && <div className="muted small">dismissed</div>}
      {entry.widget_skipped && <div className="muted small">skipped (you moved on)</div>}
      <ErrorText error={action.error} />
    </div>
  );
}

function ReplyRef({ id, byId }: { id: string; byId: Map<string, TranscriptEntry> }) {
  return (
    <div className="reply-ref">
      ↪ <code>{id}</code> {preview(byId.get(id))}
    </div>
  );
}

function IdTag({ id }: { id: string }) {
  return <code className="id-tag">{id}</code>;
}

function Images({ images }: { images?: { url: string; alt?: string }[] }) {
  if (!images?.length) return null;
  return (
    <div className="images">
      {images.map((img, i) =>
        img.url.startsWith("http") ? (
          <img key={i} src={img.url} alt={img.alt ?? ""} title={img.alt} />
        ) : (
          <span key={i} className="attachment">
            🖼 {img.url}
          </span>
        ),
      )}
    </div>
  );
}

function firstLine(text: string): string {
  const line = text.split("\n").find((l) => l.trim()) ?? "";
  return line.length > 140 ? `${line.slice(0, 140)}…` : line;
}

function compactArgs(args: unknown): string {
  const s = typeof args === "string" ? args : JSON.stringify(args);
  return s && s.length > 120 ? `${s.slice(0, 120)}…` : s ?? "";
}
