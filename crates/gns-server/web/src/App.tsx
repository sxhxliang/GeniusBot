// App shell: header, sidebar, routed main view (state kept in the URL hash).

import { useCallback, useEffect, useMemo, useState } from "react";
import { api, getToken, setToken as storeToken, setUnauthorizedHandler, type AgentRow, type ApprovalRequest, type GroupAddress, type ServerInfo } from "./api";
import { BusProvider, useBusEvents, useConnected, useDebounced } from "./bus";
import { InfoTab, MemoryTab, PromptTab, RoutinesTab, ToolsTab, TranscriptTab } from "./components/AgentPanels";
import { ApprovalsPanel } from "./components/Approvals";
import { ChatView } from "./components/ChatView";
import { BroadcastDialog, NewAgentDialog, NewGroupDialog, SettingsDialog, TokenDialog } from "./components/Dialogs";
import { EventsView } from "./components/EventsView";
import { GroupView } from "./components/GroupView";
import { LlmLogsView } from "./components/LlmLogs";
import { Icon, StatusDot } from "./ui";

const AGENT_TABS = [
  ["chat", "Chat"],
  ["info", "Info"],
  ["prompt", "System prompt"],
  ["memory", "Memory"],
  ["tools", "Tools & MCP"],
  ["routines", "Routines"],
  ["transcript", "Transcript"],
  ["calls", "Model calls"],
] as const;

type Route =
  | { view: "agent"; id: string; tab: string }
  | { view: "group"; id: string }
  | { view: "logs" }
  | { view: "events" }
  | { view: "approvals" }
  | { view: "home" };

function parseHash(): Route {
  const [view, id, tab] = window.location.hash.replace(/^#\/?/, "").split("/").map(decodeURIComponent);
  if (view === "agent" && id) return { view, id, tab: tab || "chat" };
  if (view === "group" && id) return { view, id };
  if (view === "logs" || view === "events" || view === "approvals") return { view };
  return { view: "home" };
}

function toHash(r: Route): string {
  switch (r.view) {
    case "agent":
      return `#/agent/${encodeURIComponent(r.id)}/${r.tab}`;
    case "group":
      return `#/group/${encodeURIComponent(r.id)}`;
    default:
      return `#/${r.view}`;
  }
}

export default function App() {
  const [token, setToken] = useState(getToken());
  const [needToken, setNeedToken] = useState(false);
  useEffect(() => setUnauthorizedHandler(() => setNeedToken(true)), []);
  return (
    <BusProvider token={token}>
      <Console key={token} />
      {needToken && (
        <TokenDialog
          onSave={(t) => {
            storeToken(t);
            setToken(t);
            setNeedToken(false);
          }}
        />
      )}
    </BusProvider>
  );
}

function Console() {
  const [route, setRouteState] = useState<Route>(parseHash);
  const [info, setInfo] = useState<ServerInfo>();
  const [agents, setAgents] = useState<AgentRow[]>([]);
  const [groups, setGroups] = useState<GroupAddress[]>([]);
  const [approvals, setApprovals] = useState<ApprovalRequest[]>([]);
  const [dialog, setDialog] = useState<"settings" | "agent" | "group" | "broadcast" | null>(null);
  const connected = useConnected();
  const [navOpen, setNavOpen] = useState(false);

  const setRoute = useCallback((r: Route) => {
    setNavOpen(false);
    window.history.replaceState(null, "", toHash(r));
    setRouteState(r);
  }, []);
  useEffect(() => {
    const onHash = () => setRouteState(parseHash());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  const loadInfo = useCallback(() => api.info().then(setInfo).catch(() => {}), []);
  const loadAgents = useCallback(() => api.agents().then((r) => setAgents(r.agents)).catch(() => {}), []);
  const loadGroups = useCallback(() => api.groups().then((r) => setGroups(r.groups)).catch(() => {}), []);
  const loadApprovals = useCallback(() => api.approvals().then((r) => setApprovals(r.approvals)).catch(() => {}), []);
  useEffect(() => {
    loadInfo();
    loadAgents();
    loadGroups();
    loadApprovals();
  }, [loadInfo, loadAgents, loadGroups, loadApprovals]);
  // Reload everything after a reconnect: events may have been missed.
  useEffect(() => {
    if (connected) {
      loadAgents();
      loadGroups();
      loadApprovals();
    }
  }, [connected, loadAgents, loadGroups, loadApprovals]);

  const refreshAgents = useDebounced(loadAgents, 300);
  const refreshGroups = useDebounced(loadGroups, 300);
  useBusEvents((e) => {
    switch (e.kind) {
      case "agent-created":
      case "agent-updated":
      case "agent-deleted":
      case "run-started":
      case "turn-ended":
      case "a2a-queued":
        refreshAgents();
        if (e.kind === "agent-deleted" || e.kind === "agent-updated") refreshGroups();
        break;
      case "group-created":
      case "group-deleted":
        refreshGroups();
        break;
      case "approval-requested":
      case "approval-resolved":
        loadApprovals();
        break;
      case "model-changed":
        loadInfo();
        break;
    }
  });

  // Follow renames and deletions of the selected agent.
  const agentName = useMemo(() => new Map(agents.map((a) => [a.id, a.name])), [agents]);
  const current = route.view === "agent" ? agents.find((a) => a.id === route.id || a.name === route.id) : undefined;
  const model = info?.model;

  return (
    <div className="grid h-dvh min-h-0 grid-cols-1 grid-rows-[auto_minmax(0,1fr)] overflow-hidden md:grid-cols-[248px_minmax(0,1fr)]">
      <header className="col-span-full flex min-w-0 flex-wrap items-center gap-2 border-b border-line bg-surface px-4 py-3 md:gap-4 md:px-5">
        <button type="button" className="btn ghost md:hidden" aria-label={navOpen ? "Close navigation" : "Open navigation"} aria-expanded={navOpen} aria-controls="workspace-navigation" onClick={() => setNavOpen(!navOpen)}>
          <Icon name={navOpen ? "close" : "menu"} />
        </button>
        <button type="button" className="flex items-center gap-2.5 text-left md:w-[212px]" onClick={() => setRoute({ view: "home" })}>
          <span className="flex size-8 items-center justify-center rounded-lg bg-accent-soft text-accent"><Icon name="agent" className="size-5" /></span>
          <span className="text-sm font-semibold tracking-tight">Genius<span className="ml-1 font-normal text-muted">Console</span></span>
        </button>
        <button className={`model-badge max-w-[240px] ${model?.ready ? "ok" : "bad"} order-last w-full sm:order-none sm:w-auto`} onClick={() => setDialog("settings")} title={model?.error ?? "Model settings"}>
          <span className={`dot ${model?.ready ? "ok" : "waiting"}`} />
          <span className="truncate">{model ? (model.ready ? `${model.model} · ${model.provider}` : "Configure your model") : "Connecting…"}</span>
        </button>
        <span className="flex-1" />
        <button className={`btn ghost ${approvals.length ? "attention" : ""}`} onClick={() => setRoute({ view: "approvals" })} aria-label={`Approvals${approvals.length ? ` (${approvals.length})` : ""}`} title="Approvals">
          <Icon name="check" /><span className="hidden lg:inline">Approvals</span>{approvals.length > 0 && <span className="count">{approvals.length}</span>}
        </button>
        <button className="btn ghost" onClick={() => setDialog("broadcast")} aria-label="Broadcast" title="Broadcast">
          <Icon name="broadcast" /><span className="hidden lg:inline">Broadcast</span>
        </button>
        <button className="btn ghost" onClick={() => setDialog("settings")} aria-label="Settings" title="Settings">
          <Icon name="settings" /><span className="hidden lg:inline">Settings</span>
        </button>
        <span className="hidden items-center gap-2 border-l border-line pl-4 text-[11px] text-muted sm:flex" role="status">
          <span className={`dot ${connected ? "ok" : "bad"}`} />{connected ? "Live" : "Offline"}
        </span>
      </header>

      <aside id="workspace-navigation" aria-label="Workspace navigation" className={`${navOpen ? "flex" : "hidden"} min-h-0 flex-col gap-6 overflow-y-auto border-r border-line bg-surface p-4 md:flex ${navOpen ? "col-start-1 row-start-2" : ""}`}>
        <button className={`side-item ${route.view === "home" ? "active" : ""}`} aria-current={route.view === "home" ? "page" : undefined} onClick={() => setRoute({ view: "home" })}>
          <Icon name="grid" />Workspace
        </button>
        <div>
          <div className="side-head">Agents<button className="btn tiny ghost" onClick={() => setDialog("agent")} aria-label="New agent" title="New agent"><Icon name="plus" /></button></div>
          <div className="flex flex-col gap-1">
            {agents.map((a) => (
              <button key={a.id} className={`side-item ${current?.id === a.id ? "active" : ""}`} aria-current={current?.id === a.id ? "page" : undefined} onClick={() => setRoute({ view: "agent", id: a.id, tab: route.view === "agent" ? route.tab : "chat" })} title={a.description}>
                <StatusDot lane={a.activeLane} awaiting={a.awaitingUser} /><span className="side-name">{a.name}</span>
                {a.pendingInbound > 0 && <span className="count">{a.pendingInbound}</span>}
              </button>
            ))}
            {agents.length === 0 && <div className="side-empty">Your agents will appear here.</div>}
          </div>
        </div>
        <div>
          <div className="side-head">Groups<button className="btn tiny ghost" onClick={() => setDialog("group")} aria-label="New group" title="New group"><Icon name="plus" /></button></div>
          <div className="flex flex-col gap-1">
            {groups.map((g) => (
              <button key={g.id} className={`side-item ${route.view === "group" && route.id === g.id ? "active" : ""}`} aria-current={route.view === "group" && route.id === g.id ? "page" : undefined} onClick={() => setRoute({ view: "group", id: g.id })} title={g.members.map((m) => m.name).join(", ")}>
                <span className="hash">#</span><span className="side-name">{g.name}</span>
              </button>
            ))}
            {groups.length === 0 && <div className="side-empty">No groups yet.</div>}
          </div>
        </div>
        <div>
          <div className="side-head">Inspect</div>
          <button className={`side-item ${route.view === "logs" ? "active" : ""}`} aria-current={route.view === "logs" ? "page" : undefined} onClick={() => setRoute({ view: "logs" })}><Icon name="logs" />Model calls</button>
          <button className={`side-item ${route.view === "events" ? "active" : ""}`} aria-current={route.view === "events" ? "page" : undefined} onClick={() => setRoute({ view: "events" })}><Icon name="events" />Events</button>
        </div>
        <div className="mt-auto border-t border-line px-3 pt-4 text-[11px] leading-relaxed text-muted">
          <span className="font-medium text-ink">Your agent workspace</span><br />Conversations, tools & automation.
        </div>
      </aside>

      <main className={`${navOpen ? "hidden md:flex" : "flex"} min-h-0 min-w-0 flex-col overflow-hidden`}>
        {route.view === "agent" &&
          (current ? (
            <>
              <nav className="tabs" aria-label={`${current.name} sections`}>
                <span className="tabs-title">
                  <StatusDot lane={current.activeLane} awaiting={current.awaitingUser} /> {current.name}
                </span>
                {AGENT_TABS.map(([tab, label]) => (
                  <button key={tab} aria-current={route.tab === tab ? "page" : undefined} className={`tab ${route.tab === tab ? "active" : ""}`} onClick={() => setRoute({ ...route, id: current.id, tab })}>
                    {label}
                  </button>
                ))}
              </nav>
              <div className="tab-body">
                {route.tab === "chat" && <ChatView agentId={current.id} agentName={current.name} approvals={approvals} />}
                {route.tab === "info" && <InfoTab agentId={current.id} onDeleted={() => setRoute({ view: "home" })} />}
                {route.tab === "prompt" && <PromptTab agentId={current.id} />}
                {route.tab === "memory" && <MemoryTab agentId={current.id} names={agentName} />}
                {route.tab === "tools" && <ToolsTab agentId={current.id} />}
                {route.tab === "routines" && <RoutinesTab agentId={current.id} />}
                {route.tab === "transcript" && <TranscriptTab agentId={current.id} />}
                {route.tab === "calls" && <LlmLogsView agents={agents} agentId={current.id} />}
              </div>
            </>
          ) : (
            <div className="empty">{agents.length ? "Agent not found." : "Loading…"}</div>
          ))}
        {route.view === "group" && <GroupView key={route.id} groupId={route.id} agents={agents} onDeleted={() => setRoute({ view: "home" })} />}
        {route.view === "logs" && <LlmLogsView agents={agents} />}
        {route.view === "events" && <EventsView agents={agents} />}
        {route.view === "approvals" && (
          <div className="panel">
            <ApprovalsPanel approvals={approvals} />
          </div>
        )}
        {route.view === "home" && (
          <Home info={info} agents={agents} onNewAgent={() => setDialog("agent")} onSettings={() => setDialog("settings")} onOpen={(id) => setRoute({ view: "agent", id, tab: "chat" })} />
        )}
      </main>

      {dialog === "settings" && (
        <SettingsDialog
          info={info}
          onClose={() => {
            setDialog(null);
            loadInfo();
          }}
        />
      )}
      {dialog === "agent" && (
        <NewAgentDialog
          onClose={() => setDialog(null)}
          onCreated={(id) => {
            setDialog(null);
            loadAgents();
            setRoute({ view: "agent", id, tab: "chat" });
          }}
        />
      )}
      {dialog === "group" && (
        <NewGroupDialog
          agents={agents}
          onClose={() => setDialog(null)}
          onCreated={(id) => {
            setDialog(null);
            loadGroups();
            setRoute({ view: "group", id });
          }}
        />
      )}
      {dialog === "broadcast" && <BroadcastDialog agents={agents} onClose={() => setDialog(null)} />}
    </div>
  );
}

function Home({
  info,
  agents,
  onNewAgent,
  onSettings,
  onOpen,
}: {
  info?: ServerInfo;
  agents: AgentRow[];
  onNewAgent: () => void;
  onSettings: () => void;
  onOpen: (id: string) => void;
}) {
  return (
    <div className="min-h-0 flex-1 overflow-y-auto p-5 md:p-10">
      <div className="mx-auto flex max-w-6xl flex-col gap-8">
        <div className="flex flex-wrap items-end justify-between gap-5 border-b border-line pb-7">
          <div className="max-w-xl">
            <p className="mb-2 text-[10px] font-semibold tracking-[0.18em] text-accent uppercase">Genius workspace</p>
            <h1>Your agents, in focus.</h1>
            <p className="mt-3 text-sm leading-6 text-muted">Start a conversation, follow the work, and stay in control of every tool call.</p>
          </div>
          <button className="btn primary" onClick={onNewAgent}><Icon name="plus" />New agent</button>
        </div>
        {info && !info.model.ready && (
          <div className="flex flex-wrap items-center justify-between gap-4 rounded-xl border border-line bg-surface p-5">
            <div className="min-w-0 flex-1">
              <h2 className="text-sm">Connect a model to get started</h2>
              <p className="mt-1 text-xs text-muted">{info.model.error || "Choose a provider and model in settings to start working with your agents."}</p>
            </div>
            <button className="btn" onClick={onSettings}><Icon name="settings" />Model settings</button>
          </div>
        )}
        <section aria-labelledby="agents-heading">
          <div className="flex items-center justify-between gap-4">
            <h2 id="agents-heading">Agents <span className="ml-1 font-mono text-xs font-normal text-muted">{agents.length}</span></h2>
            <span className="text-xs text-muted">Your conversations start here</span>
          </div>
          {agents.length > 0 ? (
            <div className="cards">
              {agents.map((a) => (
                <button key={a.id} className="group flex min-w-0 flex-col gap-5 rounded-xl border border-line bg-surface p-5 text-left transition-colors hover:border-accent" onClick={() => onOpen(a.id)}>
                  <span className="flex w-full min-w-0 items-center gap-3">
                    <span className="flex size-10 shrink-0 items-center justify-center rounded-xl bg-subtle text-accent"><Icon name="agent" className="size-5" /></span>
                    <span className="min-w-0 flex-1 break-words text-sm font-semibold">{a.name}</span>
                    <Icon name="arrow" className="size-4 text-muted transition-transform group-hover:translate-x-1" />
                  </span>
                  <span className="min-h-10 text-xs leading-5 text-muted [overflow-wrap:anywhere]">{a.description || "Ready for your next conversation."}</span>
                  <span className="flex w-full items-center gap-2 border-t border-line pt-3 text-[11px] text-muted">
                    <StatusDot lane={a.activeLane} awaiting={a.awaitingUser} />{a.activeLane ? `Running · ${a.activeLane}` : a.awaitingUser ? "Needs your answer" : "Ready"}
                    {a.pendingInbound > 0 && <span className="ml-auto">{a.pendingInbound} queued</span>}
                  </span>
                </button>
              ))}
            </div>
          ) : (
            <div className="mt-5 flex flex-col items-center gap-4 rounded-2xl border border-dashed border-line px-6 py-14 text-center">
              <span className="flex size-14 items-center justify-center rounded-2xl bg-accent-soft text-accent"><Icon name="agent" className="size-7" /></span>
              <div><h3 className="text-base">Make room for your first agent</h3><p className="max-w-sm text-sm text-muted">Give it a name and a purpose. Then start a conversation and watch it work.</p></div>
              <button className="btn primary" onClick={onNewAgent}><Icon name="plus" />Create an agent</button>
            </div>
          )}
        </section>
        <div className="flex flex-wrap items-center gap-2 border-t border-line pt-4 text-[11px] text-muted">
          <span>Local workspace</span><code className="min-w-0 text-[10px] [overflow-wrap:anywhere]">{info?.rootDir ?? "Connecting to server…"}</code>
        </div>
      </div>
    </div>
  );
}
