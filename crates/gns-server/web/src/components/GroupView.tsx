// A group chat room: history, members and the user's post box.

import { useEffect, useRef, useState } from "react";
import { ArtifactCards, TaskCard } from "./ArtifactCards";
import { api, type AgentRow } from "../api";
import { useBusEvents, useDebounced } from "../bus";
import { ErrorText, fmtTime, useAction, useLoad } from "../ui";

export function GroupView({ groupId, agents, onDeleted }: { groupId: string; agents: AgentRow[]; onDeleted: () => void }) {
  const { data, error, reload } = useLoad(() => api.group(groupId), [groupId]);
  const refresh = useDebounced(reload, 250);
  const [text, setText] = useState("");
  const [editing, setEditing] = useState(false);
  const [members, setMembers] = useState<string[]>([]);
  const action = useAction();
  const scroller = useRef<HTMLDivElement>(null);
  const memberIds = new Set(data?.group.members.map((m) => m.id));
  const busy = agents.filter((a) => memberIds.has(a.id) && a.activeLane);

  useBusEvents((e) => {
    const d = e.data as Record<string, any>;
    if (d.entry?.group_id === groupId || d.task?.origin?.id === groupId || d.group_id === groupId || d.group?.id === groupId || (e.kind === "turn-ended" && memberIds.has(d.agent_id))) refresh();
  });
  useEffect(() => {
    if (scroller.current) scroller.current.scrollTop = scroller.current.scrollHeight;
  }, [data]);

  const post = async () => {
    if (!text.trim()) return;
    if (await action.run(() => api.postToGroup(groupId, text.trim()))) {
      setText("");
      refresh();
    }
  };

  if (!data) return <ErrorText error={error} />;
  const { group, history } = data;
  return (
    <div className="chat">
      <div className="chat-toolbar">
        <b className="text-base tracking-tight [overflow-wrap:anywhere]"># {group.name}</b>
        <span className="muted small">{group.description}</span>
        <span className="spacer" />
        {!editing && (
          <>
            <span className="small">{group.members.map((m) => m.name).join(", ")}</span>
            <button
              className="btn small"
              onClick={() => {
                setMembers(group.members.map((m) => m.id));
                setEditing(true);
              }}
            >
              Members
            </button>
            <button
              className="btn small danger"
              onClick={async () => {
                if (!confirm(`Delete room ${group.name}?`)) return;
                if ((await action.run(() => api.deleteGroup(groupId).then(() => true))) === true) onDeleted();
              }}
            >
              Delete
            </button>
          </>
        )}
      </div>
      {editing && (
        <div className="member-editor">
          {agents.map((a) => (
            <label key={a.id} className="chip">
              <input
                type="checkbox"
                checked={members.includes(a.id)}
                onChange={(e) => setMembers(e.target.checked ? [...members, a.id] : members.filter((m) => m !== a.id))}
              />
              {a.name}
            </label>
          ))}
          <button
            className="btn small primary"
            onClick={async () => {
              if (await action.run(() => api.updateGroup(groupId, { members }))) {
                setEditing(false);
                reload();
              }
            }}
          >
            Save
          </button>
          <button className="btn small ghost" onClick={() => setEditing(false)}>
            Cancel
          </button>
        </div>
      )}
      <div className="chat-scroll" ref={scroller}>
        {history.length === 0 && <div className="empty">No messages in this room yet. @mention members to pick who answers.</div>}
        {history.map((m, i) => {
          const mine = m.speaker.kind === "user";
          const who = m.speaker.kind === "user" ? m.speaker.name ?? "You" : m.speaker.name;
          return (
            <div key={i} className={`entry ${mine ? "row-right" : "row-left"}`}>
              <div className={`bubble ${mine ? "user" : "agent"}`}>
                <div className="bubble-meta">
                  <b>{who}</b>
                  <span className="muted"> · {fmtTime(m.timestamp_ms)}</span>
                </div>
                {m.task ? <TaskCard task={m.task} /> : <><div className="bubble-text">{m.content}</div><ArtifactCards artifacts={m.artifacts} /></>}
              </div>
            </div>
          );
        })}
        {busy.length > 0 && (
          <div className="live">
            <span className="spinner" /> {busy.map((a) => a.name).join(", ")} working…
          </div>
        )}
      </div>
      <div className="composer">
        <div className="composer-row">
          <textarea
            id="group-message"
            aria-label={`Message ${group.name}`}
            value={text}
            rows={2}
            placeholder="Post to the room (@Name to address someone, @everyone for all)"
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
                e.preventDefault();
                post();
              }
            }}
          />
          <button className="btn primary" disabled={action.busy || !text.trim()} onClick={post}>
            Post
          </button>
        </div>
        <ErrorText error={action.error || error} />
      </div>
    </div>
  );
}
