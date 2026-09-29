// Dialogs: model settings, new agent, new group, broadcast, API token.

import { useEffect, useState } from "react";
import { api, type AgentRow, type ModelSettingsPatch, type ServerInfo } from "../api";
import { ErrorText, Modal, fmtDuration, useAction, useLoad } from "../ui";

const PROVIDERS: [string, string][] = [
  ["", "infer from model name"],
  ["openai", "OpenAI / OpenAI-compatible"],
  ["kimi", "Kimi (api.moonshot.ai)"],
  ["moonshot", "Moonshot (api.moonshot.cn)"],
  ["auto", "auto (genai: claude-*, gemini-*, deepseek-*, …)"],
  ["mock", "mock (offline echo)"],
];

export function SettingsDialog({ onClose, info }: { onClose: () => void; info?: ServerInfo }) {
  const { data, error } = useLoad(() => api.model(), []);
  const [draft, setDraft] = useState<ModelSettingsPatch>({});
  const [apiKey, setApiKey] = useState("");
  const [clearKey, setClearKey] = useState(false);
  const [models, setModels] = useState<string[]>([]);
  const [test, setTest] = useState<{ ok: boolean; reply?: string; error?: string; latencyMs: number } | undefined>();
  const save = useAction();
  const probe = useAction();

  useEffect(() => {
    if (!data) return;
    const s = data.settings;
    setDraft({
      provider: s.provider,
      model: s.model,
      baseUrl: s.baseUrl,
      apiKeyEnv: s.apiKeyEnv,
      stream: s.stream,
      temperature: s.temperature,
      maxTokens: s.maxTokens,
    });
  }, [data]);

  // Only send the key when it was typed (or explicitly cleared).
  const patch = (): ModelSettingsPatch => ({ ...draft, ...(clearKey ? { apiKey: "" } : apiKey ? { apiKey } : {}) });
  const set = (p: ModelSettingsPatch) => setDraft((d) => ({ ...d, ...p }));
  const numberOrNull = (v: string) => (v.trim() === "" ? null : Number(v));
  const status = data?.status;

  return (
    <Modal title="Settings" onClose={onClose} wide>
      <ErrorText error={error} />
      {status && (
        <div className={`status-box ${status.ready ? "ok" : "bad"}`}>
          {status.ready ? (
            <>
              Active: <b>{status.model}</b> via {status.provider}
              {status.baseUrl && <> at {status.baseUrl}</>}
              {status.keyless && " (no API key)"}
            </>
          ) : (
            <>Model not ready: {status.error}</>
          )}
        </div>
      )}
      <div className="form grid2">
        <label>
          Provider
          <select value={draft.provider ?? ""} onChange={(e) => set({ provider: e.target.value })}>
            {PROVIDERS.map(([value, label]) => (
              <option key={value} value={value}>
                {label}
              </option>
            ))}
          </select>
        </label>
        <label>
          Model
          <div className="row">
            <input
              list="model-options"
              value={draft.model ?? ""}
              placeholder="gpt-4o-mini, kimi-k2-turbo-preview, qwen-plus, … (empty: $GNS_MODEL / $OPENAI_MODEL)"
              onChange={(e) => set({ model: e.target.value })}
            />
            <button
              className="btn small"
              disabled={probe.busy}
              title="GET {base URL}/models"
              onClick={async () => {
                const r = await probe.run(() => api.listModels(patch()));
                if (r) setModels(r.models);
              }}
            >
              Fetch list
            </button>
          </div>
          <datalist id="model-options">
            {models.map((m) => (
              <option key={m} value={m} />
            ))}
          </datalist>
          {models.length > 0 && <span className="muted small">{models.length} model(s) available — pick from the input's list</span>}
        </label>
        <label className="span2">
          Base URL (OpenAI-compatible endpoint, version path included)
          <input
            value={draft.baseUrl ?? ""}
            placeholder="https://api.example.com/v1 — empty: $OPENAI_BASE_URL / $KIMI_BASE_URL / vendor default"
            onChange={(e) => set({ baseUrl: e.target.value })}
          />
        </label>
        <label>
          API key
          <input
            type="password"
            autoComplete="off"
            value={apiKey}
            disabled={clearKey}
            placeholder={data?.settings.apiKeySet ? `saved: ${data.settings.apiKeyPreview} (type to replace)` : "not set — falls back to the env variable"}
            onChange={(e) => setApiKey(e.target.value)}
          />
          {data?.settings.apiKeySet && (
            <label className="check small">
              <input type="checkbox" checked={clearKey} onChange={(e) => setClearKey(e.target.checked)} /> remove the saved key
            </label>
          )}
        </label>
        <label>
          API key env variable
          <input value={draft.apiKeyEnv ?? ""} placeholder="OPENAI_API_KEY / KIMI_API_KEY / MOONSHOT_API_KEY" onChange={(e) => set({ apiKeyEnv: e.target.value })} />
        </label>
        <label>
          Temperature
          <input
            type="number"
            step="0.1"
            value={draft.temperature ?? ""}
            placeholder="model default"
            onChange={(e) => set({ temperature: numberOrNull(e.target.value) })}
          />
        </label>
        <label>
          Max tokens
          <input
            type="number"
            value={draft.maxTokens ?? ""}
            placeholder="model default"
            onChange={(e) => set({ maxTokens: numberOrNull(e.target.value) })}
          />
        </label>
        <label className="check">
          <input type="checkbox" checked={draft.stream ?? true} onChange={(e) => set({ stream: e.target.checked })} /> stream responses
        </label>
      </div>
      <div className="muted small">
        Saved to <code>{info?.rootDir ?? "<root>"}/gns-server.json</code> (the key in plain text, file mode 0600). Changes apply to the next model call of every
        agent.
      </div>
      <div className="mt-5 flex flex-wrap items-center gap-3 border-t border-line pt-5">
        <button
          className="btn primary"
          disabled={save.busy}
          onClick={async () => {
            if (await save.run(() => api.saveModel(patch()))) onClose();
          }}
        >
          Save
        </button>
        <button
          className="btn"
          disabled={probe.busy}
          onClick={async () => {
            setTest(undefined);
            const r = await probe.run(() => api.testModel(patch()));
            if (r) setTest(r);
          }}
        >
          {probe.busy ? "Testing…" : "Test connection"}
        </button>
        <span className="muted small">Test and Fetch list use the form as typed, without saving.</span>
      </div>
      {test && (
        <div className={`status-box ${test.ok ? "ok" : "bad"}`}>
          {test.ok ? `OK in ${fmtDuration(test.latencyMs)}: ${test.reply || "(empty reply)"}` : `Failed after ${fmtDuration(test.latencyMs)}: ${test.error}`}
        </div>
      )}
      <ErrorText error={save.error || probe.error} />
      {info && (
        <details className="server-info">
          <summary>Server</summary>
          <dl className="kv">
            <dt>version</dt>
            <dd>{info.version}</dd>
            <dt>root</dt>
            <dd className="mono small">{info.rootDir}</dd>
            <dt>user</dt>
            <dd>{info.userName ?? "—"}</dd>
            <dt>time zone</dt>
            <dd>{info.timeZone ?? "local"}</dd>
            <dt>context window</dt>
            <dd>{info.contextWindowTokens.toLocaleString()} tokens</dd>
            <dt>host tools</dt>
            <dd className="small">{info.toolNames.join(", ")}</dd>
          </dl>
        </details>
      )}
    </Modal>
  );
}

export function NewAgentDialog({ onClose, onCreated }: { onClose: () => void; onCreated: (id: string) => void }) {
  const [name, setName] = useState("");
  const [title, setTitle] = useState("");
  const [description, setDescription] = useState("");
  const [kickstart, setKickstart] = useState(true);
  const action = useAction();
  const create = async () => {
    const agent = await action.run(() => api.createAgent({ name, title, description, kickstart }));
    if (agent) onCreated(agent.id);
  };
  return (
    <Modal title="New agent" onClose={onClose}>
      <p className="mb-5 text-sm text-muted">Give your agent a name, a role, and a clear purpose.</p>
      <div className="form">
        <label>
          Name
          <input autoFocus value={name} placeholder="Planner" onChange={(e) => setName(e.target.value)} />
        </label>
        <label>
          Title
          <input value={title} placeholder="optional" onChange={(e) => setTitle(e.target.value)} />
        </label>
        <label>
          Description (persona)
          <textarea
            rows={5}
            value={description}
            placeholder="You break tasks down and delegate coding to your teammate Coder."
            onChange={(e) => setDescription(e.target.value)}
          />
        </label>
        <label className="check">
          <input type="checkbox" checked={kickstart} onChange={(e) => setKickstart(e.target.checked)} /> run the introduction (kickstart) turn
        </label>
        <div className="row">
          <button className="btn primary" disabled={!name.trim() || action.busy} onClick={create}>
            Create
          </button>
        </div>
        <ErrorText error={action.error} />
      </div>
    </Modal>
  );
}

export function NewGroupDialog({ agents, onClose, onCreated }: { agents: AgentRow[]; onClose: () => void; onCreated: (id: string) => void }) {
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [members, setMembers] = useState<string[]>([]);
  const action = useAction();
  const create = async () => {
    const group = await action.run(() => api.createGroup({ name, description, members }));
    if (group) onCreated(group.id);
  };
  return (
    <Modal title="New group room" onClose={onClose}>
      <div className="form">
        <label>
          Name
          <input autoFocus value={name} placeholder="Review" onChange={(e) => setName(e.target.value)} />
        </label>
        <label>
          Description
          <input value={description} placeholder="code review room" onChange={(e) => setDescription(e.target.value)} />
        </label>
        <div className="label">Members (at most 6)</div>
        <div className="row wrap">
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
          {agents.length === 0 && <span className="muted small">Create agents first.</span>}
        </div>
        <div className="row">
          <button className="btn primary" disabled={!name.trim() || members.length === 0 || action.busy} onClick={create}>
            Create
          </button>
        </div>
        <ErrorText error={action.error} />
      </div>
    </Modal>
  );
}

export function BroadcastDialog({ agents, onClose }: { agents: AgentRow[]; onClose: () => void }) {
  const [text, setText] = useState("");
  const [targets, setTargets] = useState<string[]>([]);
  const [result, setResult] = useState<string>();
  const action = useAction();
  return (
    <Modal title="Broadcast to agents" onClose={onClose}>
      <div className="form">
        <div className="muted small">A hidden owner broadcast on the background lane; each agent may reply once. No selection = every agent.</div>
        <textarea rows={4} value={text} onChange={(e) => setText(e.target.value)} placeholder="Stand-up: post what you are working on." />
        <div className="row wrap">
          {agents.map((a) => (
            <label key={a.id} className="chip">
              <input
                type="checkbox"
                checked={targets.includes(a.id)}
                onChange={(e) => setTargets(e.target.checked ? [...targets, a.id] : targets.filter((t) => t !== a.id))}
              />
              {a.name}
            </label>
          ))}
        </div>
        <div className="row">
          <button
            className="btn primary"
            disabled={!text.trim() || action.busy}
            onClick={async () => {
              const r = await action.run(() => api.broadcast(text, targets.length ? targets : undefined));
              if (r) setResult(`scheduled for ${r.scheduled} of ${r.total} agent(s)`);
            }}
          >
            Send
          </button>
          {result && <span className="muted">{result}</span>}
        </div>
        <ErrorText error={action.error} />
      </div>
    </Modal>
  );
}

export function TokenDialog({ onSave }: { onSave: (token: string) => void }) {
  const [token, setToken] = useState("");
  return (
    <Modal title="API token required" onClose={() => {}}>
      <div className="form">
        <div className="muted small">This server was started with --token. Enter it to continue (kept in this browser's local storage).</div>
        <input id="server-token" aria-label="Server API token" type="password" autoFocus value={token} onChange={(e) => setToken(e.target.value)} onKeyDown={(e) => e.key === "Enter" && onSave(token)} />
        <button className="btn primary" onClick={() => onSave(token)}>
          Continue
        </button>
      </div>
    </Modal>
  );
}
