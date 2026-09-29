// Typed client for the gns-server REST API.

export interface ArtifactRef {
  id: string; name: string; mediaType: string; size: number; sha256: string; creator: string; createdAt: number;
}
export interface TaskRecord {
  id: string; requester: string; executor: string; origin: { kind: "agent" | "group"; id: string };
  status: "queued" | "running" | "blocked" | "completed" | "failed" | "cancelled";
  summary: string; verification: string; artifacts: ArtifactRef[]; inputs: ArtifactRef[]; revision: number;
  instruction: string; requireFiles: boolean; updatedAt: number;
}

export interface AgentRow {
  id: string;
  name: string;
  description: string;
  activeLane: "user" | "agent" | "automation" | null;
  awaitingUser: boolean;
  pendingInbound: number;
}

export interface AgentAddress {
  id: string;
  name: string;
  description: string;
}

export interface GroupAddress extends AgentAddress {
  members: AgentAddress[];
}

export interface Usage {
  promptTokens: number;
  completionTokens: number;
  cachedPromptTokens: number;
  totalTokens: number;
  llmCalls: number;
}

export interface McpServerConfig {
  name: string;
  transport:
    | { type: "stdio"; command: string; args: string[]; env: Record<string, string>; cwd?: string | null }
    | { type: "http"; url: string; headers: Record<string, string> };
  enabled: boolean;
  allowedTools: string[];
  timeoutSecs?: number | null;
}

export interface AgentDetail {
  id: string;
  address: AgentAddress;
  profile: { name: string; description: string; title: string; avatarShape: string; avatarColor: string };
  settings: {
    notifyOnAgentUpdates: boolean;
    hiddenFromSidebar: boolean;
    tools: { disabled: string[] };
    mcpServers: McpServerConfig[];
  };
  dataDir: string;
  workspaceDir: string;
  activeLane: string | null;
  awaitingUser: boolean;
  pendingInbound: number;
  usage: Usage;
  groups: { id: string; name: string }[];
}

export interface AgentRefT {
  id: string;
  name: string;
}

export interface ImageRef {
  url: string;
  alt?: string;
}

export interface WidgetOption {
  label: string;
  value?: string;
  description?: string;
  style?: "default" | "primary" | "danger";
}

export interface OutboundMessage {
  artifacts?: ArtifactRef[];
  task?: TaskRecord;
  type: "text" | "attachment" | "widget" | "secret-request";
  content?: string;
  url?: string;
  alt?: string;
  images?: ImageRef[];
  widget?: { prompt: string; helpText?: string; options: WidgetOption[]; allowCustom: boolean };
  replyTo?: string;
  fileName?: string;
  secret?: { label: string; description?: string; connector?: string; field?: string };
}

export type TranscriptEntry =
  | {
      kind: "message";
      artifacts?: ArtifactRef[];
      id: string;
      role: "user" | "assistant" | "system";
      content: string;
      timestamp_ms: number;
      hidden?: boolean;
      run_id?: string;
      from_agent?: AgentRefT;
      to_agent?: AgentRefT;
      images?: ImageRef[];
      group_id?: string;
      priority?: boolean;
      user_name?: string;
      reply_to?: string;
    }
  | {
      kind: "send-message";
      id: string;
      message: OutboundMessage;
      timestamp_ms: number;
      run_id: string;
      group_id?: string;
      author?: AgentRefT;
      reply_to?: string;
      reactions?: { emoji: string; by: string }[];
      responded_value?: string;
      widget_skipped?: boolean;
      widget_dismissed?: boolean;
    }
  | { kind: "assistant-text"; id: string; content: string; timestamp_ms: number; run_id: string }
  | {
      kind: "tool-call";
      id: string;
      call_id: string;
      name: string;
      args: unknown;
      result?: string;
      is_error?: boolean;
      timestamp_ms: number;
      run_id: string;
    }
  | { kind: "profile-update"; id: string; summary: string; timestamp_ms: number }
  | { kind: "divider"; id: string; summary: string; timestamp_ms: number; compaction_epoch: number; summarized_through?: string };

export interface TranscriptItem {
  seq: number;
  entry: TranscriptEntry;
}

export interface ToolListing {
  name: string;
  description: string;
  source: { kind: "builtin" } | { kind: "mcp"; server: string };
  enabled: boolean;
}

export interface McpServerStatus {
  agentId: string;
  name: string;
  enabled: boolean;
  connected: boolean;
  tools: string[];
  lastError: string | null;
}

export interface MemoryRecord {
  content: string;
  created_at: number;
  kind: string;
}

export interface MemoryRecall {
  profile: MemoryRecord[];
  recent: MemoryRecord[];
}

export interface MemoryView {
  agent: MemoryRecall;
  userShards: { agentId: string; recall: MemoryRecall }[];
  projects: { slug: string; shards: { agentId: string; recall: MemoryRecall }[] }[];
}

export interface Routine {
  id: string;
  name: string;
  prompt: string;
  trigger: unknown;
  schedule: string | null;
  triggerDescription: string;
  isEnabled: boolean;
  createdAt: number;
  lastRunAt: number | null;
  nextRunAt: number | null;
  runs: Record<string, unknown>[];
  filePath: string;
}

export interface GroupMessage {
  artifacts?: ArtifactRef[];
  task?: TaskRecord;
  speaker: { kind: "user"; name: string | null } | { kind: "member"; id: string; name: string };
  content: string;
  timestamp_ms: number;
}

export interface ApprovalRequest {
  id: string;
  agent_id: string;
  agent_name: string;
  run_id: string;
  tool: string;
  args: unknown;
  reason: string;
  policy: string;
  requested_at: number;
  expires_at: number;
}

export interface ModelStatus {
  ready: boolean;
  model: string;
  provider: string;
  baseUrl: string | null;
  keyless: boolean;
  error: string | null;
}

export interface ModelSettingsView {
  provider: string;
  model: string;
  baseUrl: string;
  apiKeySet: boolean;
  apiKeyPreview: string;
  apiKeyEnv: string;
  stream: boolean;
  temperature: number | null;
  maxTokens: number | null;
}

export interface ModelSettingsPatch {
  provider?: string;
  model?: string;
  baseUrl?: string;
  apiKey?: string;
  apiKeyEnv?: string;
  stream?: boolean;
  temperature?: number | null;
  maxTokens?: number | null;
}

export interface ServerInfo {
  version: string;
  rootDir: string;
  productName: string;
  userName: string | null;
  timeZone: string | null;
  contextWindowTokens: number;
  maxSteps: number;
  toolNames: string[];
  model: ModelStatus;
}

export interface LlmLogSummary {
  id: number;
  startedAtMs: number;
  durationMs: number | null;
  status: "pending" | "ok" | "error" | "cancelled";
  purpose: string;
  agentId: string | null;
  model: string;
  messageCount: number;
  toolCount: number;
  systemChars: number;
  usage: Usage | null;
  toolCalls: string[];
  textPreview: string | null;
  stopReason: string | null;
  error: string | null;
}

export type LlmMessage =
  | { User: { text: string; images: ImageRef[] } }
  | { Assistant: { text: string | null; tool_calls: { id: string; name: string; arguments: unknown }[] } }
  | { ToolResults: { call_id: string; name: string; content: string }[] };

export interface LlmLog extends LlmLogSummary {
  request: {
    system: string;
    messages: LlmMessage[];
    tools: { name: string; description: string; parameters: unknown }[];
    options: Record<string, unknown>;
  };
  response: {
    text: string | null;
    reasoning: string | null;
    tool_calls: { id: string; name: string; arguments: unknown }[];
    usage: Usage;
    stop_reason: string | null;
  } | null;
}

export interface BusEvent {
  seq: number;
  ts: number;
  channel: "host" | "server";
  kind: string;
  data: Record<string, unknown> & { type?: string };
}

// ----- transport --------------------------------------------------------

const TOKEN_KEY = "gns-console-token";

export function getToken(): string {
  try {
    return localStorage.getItem(TOKEN_KEY) ?? "";
  } catch {
    return "";
  }
}

export function setToken(token: string) {
  try {
    localStorage.setItem(TOKEN_KEY, token);
  } catch {
    // private mode: the token lasts for this page only
  }
}

export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

/** Called on 401 so the app can ask for a token. */
export let onUnauthorized: () => void = () => {};
export function setUnauthorizedHandler(f: () => void) {
  onUnauthorized = f;
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {};
  const token = getToken();
  if (token) headers.Authorization = `Bearer ${token}`;
  if (body !== undefined) headers["Content-Type"] = "application/json";
  const res = await fetch(`/api${path}`, { method, headers, body: body === undefined ? undefined : JSON.stringify(body) });
  if (res.status === 401) onUnauthorized();
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  let data: unknown = undefined;
  try {
    data = text ? JSON.parse(text) : undefined;
  } catch {
    data = text;
  }
  if (!res.ok) {
    const message = (data as { error?: string })?.error ?? (typeof data === "string" ? data : res.statusText);
    throw new ApiError(res.status, message);
  }
  return data as T;
}

export function eventsUrl(after?: number): string {
  const params = new URLSearchParams();
  if (after !== undefined) params.set("after", String(after));
  const token = getToken();
  if (token) params.set("token", token);
  const q = params.toString();
  return `/api/events${q ? `?${q}` : ""}`;
}

const enc = encodeURIComponent;

export const api = {
  tasks: () => request<{ tasks: TaskRecord[] }>("GET", "/tasks"),
  task: (id: string) => request<TaskRecord>("GET", `/tasks/${enc(id)}`),
  cancelTask: (id: string) => request<TaskRecord>("POST", `/tasks/${enc(id)}/cancel`),
  resumeTask: (id: string, instructions: string) => request<TaskRecord>("POST", `/tasks/${enc(id)}/resume`, { instructions }),
  info: () => request<ServerInfo>("GET", "/info"),

  model: () => request<{ settings: ModelSettingsView; status: ModelStatus }>("GET", "/model"),
  saveModel: (patch: ModelSettingsPatch) => request<{ settings: ModelSettingsView; status: ModelStatus }>("PUT", "/model", patch),
  testModel: (patch: ModelSettingsPatch) =>
    request<{ ok: boolean; reply?: string; error?: string; latencyMs: number; status: ModelStatus }>("POST", "/model/test", patch),
  listModels: (patch: ModelSettingsPatch) => request<{ models: string[] }>("POST", "/model/models", patch),

  llmLogs: (agent?: string, limit = 300) =>
    request<{ logs: LlmLogSummary[] }>("GET", `/llm-logs?limit=${limit}${agent ? `&agent=${enc(agent)}` : ""}`),
  llmLog: (id: number) => request<LlmLog>("GET", `/llm-logs/${id}`),
  clearLlmLogs: () => request<void>("DELETE", "/llm-logs"),

  recentEvents: (limit = 500) => request<{ events: BusEvent[] }>("GET", `/events/recent?limit=${limit}`),

  agents: () => request<{ agents: AgentRow[] }>("GET", "/agents"),
  createAgent: (body: { name: string; description: string; title?: string; kickstart?: boolean }) =>
    request<AgentRow>("POST", "/agents", body),
  agent: (id: string) => request<AgentDetail>("GET", `/agents/${enc(id)}`),
  updateAgent: (id: string, patch: { name?: string; description?: string; title?: string }) =>
    request<AgentRow>("PATCH", `/agents/${enc(id)}`, patch),
  updateAgentSettings: (id: string, patch: { notify_on_agent_updates?: boolean; hidden_from_sidebar?: boolean }) =>
    request<unknown>("PATCH", `/agents/${enc(id)}/settings`, patch),
  deleteAgent: (id: string) => request<void>("DELETE", `/agents/${enc(id)}`),
  kickstart: (id: string) => request<{ started: boolean }>("POST", `/agents/${enc(id)}/kickstart`),
  transcript: (id: string, before?: number, limit = 150) =>
    request<{ items: TranscriptItem[]; hasMore: boolean }>(
      "GET",
      `/agents/${enc(id)}/transcript?limit=${limit}${before !== undefined ? `&before=${before}` : ""}`,
    ),
  send: (id: string, text: string, replyTo?: string) =>
    request<{ entryId: string }>("POST", `/agents/${enc(id)}/messages`, { text, replyTo }),
  prompt: (id: string) => request<{ prompt: string }>("GET", `/agents/${enc(id)}/prompt`),
  memory: (id: string) => request<MemoryView>("GET", `/agents/${enc(id)}/memory`),
  writeMemory: (id: string, fact: string, tier: "profile" | "log" | "note") =>
    request<{ result: string }>("POST", `/agents/${enc(id)}/memory`, { fact, tier }),
  dream: (id: string) => request<{ added: number; removed: number }>("POST", `/agents/${enc(id)}/dream`),
  tools: (id: string) => request<{ tools: ToolListing[] }>("GET", `/agents/${enc(id)}/tools`),
  setTool: (id: string, tool: string, enabled: boolean) =>
    request<{ tools: ToolListing[] }>("PUT", `/agents/${enc(id)}/tools/${enc(tool)}`, { enabled }),
  mcp: (id: string) => request<{ servers: McpServerStatus[]; configs: McpServerConfig[] }>("GET", `/agents/${enc(id)}/mcp`),
  addMcp: (id: string, config: McpServerConfig) => request<McpServerStatus>("POST", `/agents/${enc(id)}/mcp`, config),
  setMcp: (id: string, name: string, enabled: boolean) =>
    request<McpServerStatus>("PUT", `/agents/${enc(id)}/mcp/${enc(name)}`, { enabled }),
  removeMcp: (id: string, name: string) => request<{ removed: boolean }>("DELETE", `/agents/${enc(id)}/mcp/${enc(name)}`),
  syncMcp: (id: string) => request<{ servers: McpServerStatus[] }>("POST", `/agents/${enc(id)}/mcp/sync`),
  routines: (id: string) => request<{ routines: Routine[] }>("GET", `/agents/${enc(id)}/routines`),
  runRoutine: (id: string, routine: string) => request<{ started: boolean }>("POST", `/agents/${enc(id)}/routines/${enc(routine)}/run`),
  answerWidget: (id: string, entry: string, value: string) =>
    request<{ accepted: boolean }>("POST", `/agents/${enc(id)}/widgets/${enc(entry)}`, { value }),
  dismissWidget: (id: string, entry: string) => request<void>("DELETE", `/agents/${enc(id)}/widgets/${enc(entry)}`),
  react: (id: string, entryId: string, emoji: string) => request<void>("POST", `/agents/${enc(id)}/reactions`, { entryId, emoji }),

  groups: () => request<{ groups: GroupAddress[] }>("GET", "/groups"),
  createGroup: (body: { name: string; description: string; members: string[] }) => request<GroupAddress>("POST", "/groups", body),
  group: (id: string) => request<{ group: GroupAddress; history: GroupMessage[] }>("GET", `/groups/${enc(id)}`),
  updateGroup: (id: string, patch: { name?: string; description?: string; members?: string[] }) =>
    request<GroupAddress>("PATCH", `/groups/${enc(id)}`, patch),
  deleteGroup: (id: string) => request<void>("DELETE", `/groups/${enc(id)}`),
  postToGroup: (id: string, text: string) => request<{ accepted: boolean }>("POST", `/groups/${enc(id)}/messages`, { text }),

  approvals: () => request<{ approvals: ApprovalRequest[] }>("GET", "/approvals"),
  decide: (id: string, approved: boolean, reason?: string) =>
    request<{ ok: boolean }>("POST", `/approvals/${enc(id)}`, { approved, reason }),
  broadcast: (text: string, targets?: string[]) =>
    request<{ total: number; scheduled: number }>("POST", "/broadcast", { text, targets }),
  webhook: (name: string, payload: unknown) => request<{ routines: number }>("POST", `/webhooks/${enc(name)}`, payload),
};

export async function downloadArtifact(file: ArtifactRef): Promise<void> {
  const token = getToken();
  const res = await fetch(`/api/artifacts/${encodeURIComponent(file.id)}/download`, { headers: token ? { Authorization: `Bearer ${token}` } : {} });
  if (res.status === 401) onUnauthorized();
  if (!res.ok) {
    const data = await res.json().catch(() => ({}));
    throw new ApiError(res.status, data.error ?? "Download failed");
  }
  const url = URL.createObjectURL(await res.blob());
  const anchor = document.createElement("a");
  anchor.href = url; anchor.download = file.name; anchor.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
