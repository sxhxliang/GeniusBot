//! REST + SSE API over [`AgentHost`], mirroring the `gns-cli` commands.
//!
//! Everything lives under `/api`; ids accept an agent/group id or name,
//! like the REPL. Sends return at once — replies, tool calls and turn
//! results arrive on `GET /api/events` (Server-Sent Events).

use crate::events::EventBus;
use crate::model::{ModelHub, ModelSettingsPatch, ModelSettingsView, build_provider, list_remote_models};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures::{Stream, StreamExt};
use gns_core::*;
use gns_runtime::{AgentHost, SendOptions};
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::Arc;

/// Shared state of every handler.
#[derive(Clone, Debug)]
pub struct AppState {
    pub host: AgentHost,
    pub hub: Arc<ModelHub>,
    pub bus: EventBus,
    /// Required bearer token for `/api`, when set.
    pub token: Option<String>,
}

/// An error rendered as `{"error": "…"}`.
#[derive(Debug)]
pub struct ApiError(StatusCode, String);

impl ApiError {
    fn bad_request(msg: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, msg.into())
    }
    fn not_found(msg: impl Into<String>) -> Self {
        Self(StatusCode::NOT_FOUND, msg.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<HostError> for ApiError {
    fn from(e: HostError) -> Self {
        let status = match &e {
            HostError::AgentNotFound(_) | HostError::GroupNotFound(_) => StatusCode::NOT_FOUND,
            HostError::Invalid(_) => StatusCode::BAD_REQUEST,
            HostError::ShuttingDown => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self(status, e.to_string())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
    }
}

type ApiResult<T = Json<Value>> = Result<T, ApiError>;

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/info", get(info))
        .route("/model", get(model_get).put(model_put))
        .route("/model/test", post(model_test))
        .route("/model/models", post(model_list_models))
        .route("/llm-logs", get(llm_logs).delete(llm_logs_clear))
        .route("/llm-logs/{id}", get(llm_log))
        .route("/events", get(events_stream))
        .route("/events/recent", get(events_recent))
        .route("/agents", get(agents_list).post(agents_create))
        .route("/agents/{id}", get(agent_detail).patch(agent_update).delete(agent_delete))
        .route("/agents/{id}/settings", axum::routing::patch(agent_settings))
        .route("/agents/{id}/kickstart", post(agent_kickstart))
        .route("/agents/{id}/transcript", get(agent_transcript))
        .route("/agents/{id}/messages", post(agent_send))
        .route("/agents/{id}/prompt", get(agent_prompt))
        .route("/agents/{id}/memory", get(agent_memory).post(agent_memory_write))
        .route("/agents/{id}/dream", post(agent_dream))
        .route("/agents/{id}/tools", get(agent_tools))
        .route("/agents/{id}/tools/{tool}", put(agent_tool_toggle))
        .route("/agents/{id}/mcp", get(agent_mcp).post(agent_mcp_add))
        .route("/agents/{id}/mcp/sync", post(agent_mcp_sync))
        .route("/agents/{id}/mcp/{name}", put(agent_mcp_toggle).delete(agent_mcp_remove))
        .route("/agents/{id}/routines", get(agent_routines))
        .route("/agents/{id}/routines/{routine}/run", post(agent_routine_run))
        .route("/agents/{id}/widgets/{entry}", post(agent_widget_answer).delete(agent_widget_dismiss))
        .route("/agents/{id}/reactions", post(agent_react))
        .route("/groups", get(groups_list).post(groups_create))
        .route("/groups/{id}", get(group_detail).patch(group_update).delete(group_delete))
        .route("/groups/{id}/messages", post(group_post))
        .route("/tasks", get(tasks_list).post(task_create))
        .route("/tasks/{id}", get(task_get))
        .route("/tasks/{id}/cancel", post(task_cancel))
        .route("/tasks/{id}/resume", post(task_resume))
        .route("/artifacts/{id}", get(artifact_get))
        .route("/artifacts/{id}/download", get(artifact_download))
        .route("/approvals", get(approvals_list))
        .route("/approvals/{id}", post(approval_decide))
        .route("/broadcast", post(broadcast))
        .route("/webhooks/{name}", post(webhook))
        .fallback(|| async { ApiError::not_found("no such endpoint") })
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), require_token));
    Router::new().nest("/api", api).fallback(crate::assets::serve).with_state(state)
}

/// Bearer token (or `?token=` for EventSource) when the server was started with one.
async fn require_token(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let Some(expected) = state.token.as_deref() else { return next.run(request).await };
    let header = request.headers().get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    let query = request.uri().query().and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=")));
    if header == Some(expected) || query == Some(expected) {
        next.run(request).await
    } else {
        ApiError(StatusCode::UNAUTHORIZED, "missing or wrong token".into()).into_response()
    }
}

// ----- info, model, logs, events --------------------------------------

async fn info(State(s): State<AppState>) -> Json<Value> {
    let config = s.host.config();
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "rootDir": config.root_dir,
        "productName": config.product_name,
        "userName": config.user_name,
        "timeZone": config.time_zone,
        "contextWindowTokens": config.context_window_tokens,
        "maxSteps": config.max_steps,
        "toolNames": s.host.tool_names(),
        "model": s.hub.status(),
    }))
}

fn model_view(s: &AppState) -> Json<Value> {
    Json(json!({ "settings": ModelSettingsView::from(&s.hub.settings()), "status": s.hub.status() }))
}

async fn model_get(State(s): State<AppState>) -> Json<Value> {
    model_view(&s)
}

async fn model_put(State(s): State<AppState>, Json(patch): Json<ModelSettingsPatch>) -> ApiResult {
    s.hub.update(patch)?;
    Ok(model_view(&s))
}

/// Ping the model described by the saved settings plus an unsaved draft.
async fn model_test(State(s): State<AppState>, body: Option<Json<ModelSettingsPatch>>) -> Json<Value> {
    let mut settings = s.hub.settings();
    if let Some(Json(patch)) = body {
        patch.apply(&mut settings);
    }
    let (provider, status) = build_provider(&settings);
    let started = std::time::Instant::now();
    let reply =
        provider.complete_text("You are a connectivity check. Answer with one short sentence.", "Say hello and name your model.").await;
    let latency_ms = started.elapsed().as_millis() as u64;
    match reply {
        Ok(reply) => Json(json!({ "ok": true, "reply": reply, "latencyMs": latency_ms, "status": status })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string(), "latencyMs": latency_ms, "status": status })),
    }
}

async fn model_list_models(State(s): State<AppState>, body: Option<Json<ModelSettingsPatch>>) -> ApiResult {
    let mut settings = s.hub.settings();
    if let Some(Json(patch)) = body {
        patch.apply(&mut settings);
    }
    let models = list_remote_models(&settings).await.map_err(|e| ApiError(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(Json(json!({ "models": models })))
}

#[derive(Deserialize)]
struct LogQuery {
    agent: Option<String>,
    limit: Option<usize>,
}

async fn llm_logs(State(s): State<AppState>, Query(q): Query<LogQuery>) -> Json<Value> {
    let agent = q.agent.as_deref().map(|a| s.host.find_agent(a).map(|x| x.id.to_string()).unwrap_or_else(|| a.to_owned()));
    Json(json!({ "logs": s.hub.log_summaries(agent.as_deref(), q.limit.unwrap_or(200)) }))
}

async fn llm_log(State(s): State<AppState>, Path(id): Path<u64>) -> ApiResult {
    let log = s.hub.log(id).ok_or_else(|| ApiError::not_found(format!("no model log {id} (older logs are dropped)")))?;
    Ok(Json(serde_json::to_value(log).unwrap_or_default()))
}

async fn llm_logs_clear(State(s): State<AppState>) -> StatusCode {
    s.hub.clear_logs();
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct EventsQuery {
    /// Replay buffered events after this sequence number first.
    after: Option<u64>,
    limit: Option<usize>,
}

async fn events_recent(State(s): State<AppState>, Query(q): Query<EventsQuery>) -> Json<Value> {
    Json(json!({ "events": s.bus.recent(q.after, q.limit.unwrap_or(500)) }))
}

async fn events_stream(State(s): State<AppState>, Query(q): Query<EventsQuery>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Subscribe before taking the snapshot so nothing falls between the two.
    let live = tokio_stream::wrappers::BroadcastStream::new(s.bus.subscribe());
    let replay = q.after.map(|after| s.bus.recent(Some(after), q.limit.unwrap_or(1000))).unwrap_or_default();
    let replayed_through = replay.last().map(|e| e.seq).unwrap_or(0);
    let to_sse = |e: &crate::events::BusEvent| Ok(Event::default().event(e.channel).data(serde_json::to_string(e).unwrap_or_default()));
    let replay = futures::stream::iter(replay.into_iter().map(move |e| to_sse(&e)));
    let live = live.filter_map(move |e| {
        let item = e.ok().filter(|e| e.seq > replayed_through).map(|e| to_sse(&e));
        async move { item }
    });
    Sse::new(replay.chain(live)).keep_alive(KeepAlive::default())
}

// ----- agents -----------------------------------------------------------

fn handle(s: &AppState, id: &str) -> Result<Arc<gns_runtime::AgentHandle>, ApiError> {
    s.host.agent_handle(id).ok_or_else(|| ApiError::not_found(format!("agent not found: {id}")))
}

fn agent_row(s: &AppState, address: &AgentAddress) -> Value {
    let id = address.id.as_str();
    let active_lane = s.host.agent_handle(id).and_then(|h| h.active_lane());
    json!({
        "id": address.id,
        "name": address.name,
        "description": address.description,
        "activeLane": active_lane,
        "awaitingUser": s.host.is_awaiting_user(id),
        "pendingInbound": s.host.pending_inbound(id),
    })
}

async fn agents_list(State(s): State<AppState>) -> Json<Value> {
    let agents: Vec<Value> = s.host.list_agents().iter().map(|a| agent_row(&s, a)).collect();
    Json(json!({ "agents": agents }))
}

#[derive(Deserialize)]
struct CreateAgent {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    title: String,
    /// Run the hidden introduction turn (default true, like `/new`).
    kickstart: Option<bool>,
}

async fn agents_create(State(s): State<AppState>, Json(body): Json<CreateAgent>) -> ApiResult {
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request("name is required"));
    }
    let mut spec = AgentSpec::new(body.name.trim(), body.description.trim());
    spec.title = body.title;
    spec.kickstart = body.kickstart.unwrap_or(true);
    let address = s.host.create_agent(spec).await?;
    Ok(Json(agent_row(&s, &address)))
}

async fn agent_detail(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let h = handle(&s, &id)?;
    let key = h.id.to_string();
    let groups: Vec<Value> = s
        .host
        .list_groups()
        .into_iter()
        .filter(|g| g.members.iter().any(|m| m.id == h.id))
        .map(|g| json!({ "id": g.id, "name": g.name }))
        .collect();
    Ok(Json(json!({
        "id": h.id,
        "address": h.address(),
        "profile": h.profile(),
        "settings": h.settings(),
        "dataDir": h.data_dir,
        "workspaceDir": h.workspace_dir,
        "activeLane": h.active_lane(),
        "awaitingUser": s.host.is_awaiting_user(&key),
        "pendingInbound": s.host.pending_inbound(&key),
        "usage": s.host.usage(&key)?,
        "groups": groups,
    })))
}

async fn agent_update(State(s): State<AppState>, Path(id): Path<String>, Json(patch): Json<ProfilePatch>) -> ApiResult {
    let key = handle(&s, &id)?.id.to_string();
    let address = s.host.update_agent(&key, patch).await?;
    Ok(Json(agent_row(&s, &address)))
}

async fn agent_settings(State(s): State<AppState>, Path(id): Path<String>, Json(patch): Json<SettingsPatch>) -> ApiResult {
    let h = handle(&s, &id)?;
    s.host.update_agent_settings(h.id.as_str(), patch)?;
    Ok(Json(serde_json::to_value(h.settings()).unwrap_or_default()))
}

async fn agent_delete(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let key = handle(&s, &id)?.id.to_string();
    s.host.delete_agent(&key).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn agent_kickstart(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let key = handle(&s, &id)?.id.to_string();
    Ok(Json(json!({ "started": s.host.kickstart_agent(&key)? })))
}

#[derive(Deserialize)]
struct TranscriptQuery {
    before: Option<i64>,
    limit: Option<usize>,
}

/// Oldest first; `hasMore` means `?before=<first seq>` returns more.
async fn agent_transcript(State(s): State<AppState>, Path(id): Path<String>, Query(q): Query<TranscriptQuery>) -> ApiResult {
    let key = handle(&s, &id)?.id.to_string();
    let limit = q.limit.unwrap_or(200).clamp(1, 5_000);
    let mut page = s.host.transcript_page(&key, q.before, limit)?;
    let has_more = page.len() == limit;
    page.reverse();
    let items: Vec<Value> = page.into_iter().map(|(seq, entry)| json!({ "seq": seq, "entry": entry })).collect();
    Ok(Json(json!({ "items": items, "hasMore": has_more })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendBody {
    text: String,
    #[serde(default)]
    images: Vec<ImageRef>,
    reply_to: Option<String>,
    #[serde(default)]
    is_fork: bool,
    /// Wait for the turn and return its result.
    #[serde(default)]
    wait: bool,
}

async fn agent_send(State(s): State<AppState>, Path(id): Path<String>, Json(body): Json<SendBody>) -> ApiResult {
    if body.text.trim().is_empty() && body.images.is_empty() {
        return Err(ApiError::bad_request("text is required"));
    }
    let key = handle(&s, &id)?.id.to_string();
    let options = SendOptions { reply_to: body.reply_to.map(EntryId::from), is_fork: body.is_fork };
    let accepted = s.host.send_prompt(&key, body.text, body.images, options)?;
    if body.wait {
        let result = accepted.result.await.map_err(|_| ApiError::from(HostError::ShuttingDown))?;
        return Ok(Json(json!({ "entryId": accepted.entry_id, "result": result })));
    }
    Ok(Json(json!({ "entryId": accepted.entry_id })))
}

async fn agent_prompt(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let key = handle(&s, &id)?.id.to_string();
    Ok(Json(json!({ "prompt": s.host.system_prompt(&key)? })))
}

async fn agent_memory(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let key = handle(&s, &id)?.id.to_string();
    let recall = s.host.memory_recall(&key)?;
    let shards = |shards: &[(AgentId, gns_core::memory::MemoryRecall)]| -> Vec<Value> {
        shards.iter().map(|(agent, recall)| json!({ "agentId": agent, "recall": recall })).collect()
    };
    let projects: Vec<Value> = recall.projects.iter().map(|(slug, s)| json!({ "slug": slug, "shards": shards(s) })).collect();
    Ok(Json(json!({ "agent": recall.agent, "userShards": shards(&recall.user_shards), "projects": projects })))
}

#[derive(Deserialize)]
struct MemoryWrite {
    fact: String,
    tier: gns_core::memory::MemoryTier,
}

async fn agent_memory_write(State(s): State<AppState>, Path(id): Path<String>, Json(body): Json<MemoryWrite>) -> ApiResult {
    let key = handle(&s, &id)?.id.to_string();
    Ok(Json(json!({ "result": s.host.memory_write(&key, &body.fact, body.tier)? })))
}

async fn agent_dream(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let key = handle(&s, &id)?.id.to_string();
    let (added, removed) = s.host.dream(&key).await?;
    Ok(Json(json!({ "added": added, "removed": removed })))
}

async fn agent_tools(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let key = handle(&s, &id)?.id.to_string();
    Ok(Json(json!({ "tools": s.host.list_tools(&key)? })))
}

#[derive(Deserialize)]
struct Toggle {
    enabled: bool,
}

async fn agent_tool_toggle(State(s): State<AppState>, Path((id, tool)): Path<(String, String)>, Json(t): Json<Toggle>) -> ApiResult {
    let key = handle(&s, &id)?.id.to_string();
    s.host.set_tool_enabled(&key, &tool, t.enabled)?;
    Ok(Json(json!({ "tools": s.host.list_tools(&key)? })))
}

async fn agent_mcp(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let h = handle(&s, &id)?;
    Ok(Json(json!({ "servers": s.host.mcp_servers(h.id.as_str())?, "configs": h.settings().mcp_servers })))
}

async fn agent_mcp_add(State(s): State<AppState>, Path(id): Path<String>, Json(config): Json<McpServerConfig>) -> ApiResult {
    let key = handle(&s, &id)?.id.to_string();
    config.validate().map_err(ApiError::bad_request)?;
    Ok(Json(serde_json::to_value(s.host.add_mcp_server(&key, config).await?).unwrap_or_default()))
}

async fn agent_mcp_toggle(State(s): State<AppState>, Path((id, name)): Path<(String, String)>, Json(t): Json<Toggle>) -> ApiResult {
    let key = handle(&s, &id)?.id.to_string();
    Ok(Json(serde_json::to_value(s.host.set_mcp_server_enabled(&key, &name, t.enabled).await?).unwrap_or_default()))
}

async fn agent_mcp_remove(State(s): State<AppState>, Path((id, name)): Path<(String, String)>) -> ApiResult {
    let key = handle(&s, &id)?.id.to_string();
    Ok(Json(json!({ "removed": s.host.remove_mcp_server(&key, &name).await? })))
}

async fn agent_mcp_sync(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let key = handle(&s, &id)?.id.to_string();
    Ok(Json(json!({ "servers": s.host.sync_mcp(&key).await? })))
}

async fn agent_routines(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let key = handle(&s, &id)?.id.to_string();
    Ok(Json(json!({ "routines": s.host.routines(&key)? })))
}

/// Fire a routine now; the turn runs in the background.
async fn agent_routine_run(State(s): State<AppState>, Path((id, routine)): Path<(String, String)>) -> ApiResult {
    let key = handle(&s, &id)?.id.to_string();
    if !s.host.routines(&key)?.iter().any(|r| r.id == routine) {
        return Err(ApiError::not_found(format!("no routine {routine}")));
    }
    let host = s.host.clone();
    tokio::spawn(async move {
        if let Err(e) = host.run_routine_now(&key, &routine).await {
            tracing::warn!(agent = %key, routine = %routine, "manual routine run failed: {e}");
        }
    });
    Ok(Json(json!({ "started": true })))
}

#[derive(Deserialize)]
struct WidgetAnswer {
    value: String,
}

async fn agent_widget_answer(
    State(s): State<AppState>,
    Path((id, entry)): Path<(String, String)>,
    Json(body): Json<WidgetAnswer>,
) -> ApiResult {
    let key = handle(&s, &id)?.id.to_string();
    let host = s.host.clone();
    tokio::spawn(async move {
        if let Err(e) = host.respond_to_widget(&key, &entry, body.value).await {
            tracing::warn!(agent = %key, "widget answer failed: {e}");
        }
    });
    Ok(Json(json!({ "accepted": true })))
}

async fn agent_widget_dismiss(State(s): State<AppState>, Path((id, entry)): Path<(String, String)>) -> ApiResult<StatusCode> {
    let key = handle(&s, &id)?.id.to_string();
    s.host.dismiss_widget(&key, &entry)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReactBody {
    entry_id: String,
    emoji: String,
}

async fn agent_react(State(s): State<AppState>, Path(id): Path<String>, Json(body): Json<ReactBody>) -> ApiResult<StatusCode> {
    let key = handle(&s, &id)?.id.to_string();
    s.host.react_to_message(&key, &body.entry_id, &body.emoji)?;
    Ok(StatusCode::NO_CONTENT)
}

// ----- groups -----------------------------------------------------------

fn group(s: &AppState, id: &str) -> Result<GroupAddress, ApiError> {
    s.host.find_group(id).ok_or_else(|| ApiError::not_found(format!("group not found: {id}")))
}

fn resolve_members(s: &AppState, members: &[String]) -> Result<Vec<AgentId>, ApiError> {
    members
        .iter()
        .map(|m| s.host.find_agent(m.trim()).map(|a| a.id).ok_or_else(|| ApiError::bad_request(format!("no agent {m}"))))
        .collect()
}

async fn groups_list(State(s): State<AppState>) -> Json<Value> {
    Json(json!({ "groups": s.host.list_groups() }))
}

#[derive(Deserialize)]
struct CreateGroup {
    name: String,
    #[serde(default)]
    description: String,
    /// Agent ids or names.
    members: Vec<String>,
}

async fn groups_create(State(s): State<AppState>, Json(body): Json<CreateGroup>) -> ApiResult {
    let members = resolve_members(&s, &body.members)?;
    let group = s.host.create_group(GroupSpec { name: body.name.trim().to_owned(), description: body.description, members }).await?;
    Ok(Json(serde_json::to_value(group).unwrap_or_default()))
}

async fn group_detail(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let group = group(&s, &id)?;
    let history = s.host.group_history(group.id.as_str())?;
    Ok(Json(json!({ "group": group, "history": history })))
}

#[derive(Deserialize)]
struct UpdateGroup {
    name: Option<String>,
    description: Option<String>,
    members: Option<Vec<String>>,
}

async fn group_update(State(s): State<AppState>, Path(id): Path<String>, Json(body): Json<UpdateGroup>) -> ApiResult {
    let key = group(&s, &id)?.id.to_string();
    let mut updated = s.host.update_group(&key, body.name, body.description)?;
    if let Some(members) = body.members {
        updated = s.host.set_group_members(&key, resolve_members(&s, &members)?)?;
    }
    Ok(Json(serde_json::to_value(updated).unwrap_or_default()))
}

async fn group_delete(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let key = group(&s, &id)?.id.to_string();
    s.host.delete_group(&key)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct PostBody {
    text: String,
}

/// Post as the user; the round runs in the background.
async fn group_post(State(s): State<AppState>, Path(id): Path<String>, Json(body): Json<PostBody>) -> ApiResult {
    let key = group(&s, &id)?.id.to_string();
    if body.text.trim().is_empty() {
        return Err(ApiError::bad_request("text is required"));
    }
    let host = s.host.clone();
    tokio::spawn(async move {
        if let Err(e) = host.post_to_group_as_user(&key, body.text).await {
            tracing::warn!(group = %key, "group post failed: {e}");
        }
    });
    Ok(Json(json!({ "accepted": true })))
}

// ----- approvals, broadcast, webhooks -----------------------------------

async fn approvals_list(State(s): State<AppState>) -> Json<Value> {
    Json(json!({ "approvals": s.host.pending_approvals() }))
}

#[derive(Deserialize)]
struct Decision {
    approved: bool,
    reason: Option<String>,
}

async fn approval_decide(State(s): State<AppState>, Path(id): Path<String>, Json(d): Json<Decision>) -> ApiResult {
    let reason =
        d.reason.filter(|r| !r.trim().is_empty()).unwrap_or_else(|| (if d.approved { "approved" } else { "denied by user" }).into());
    if !s.host.approve(&id, d.approved, reason) {
        return Err(ApiError::not_found("unknown or expired approval id"));
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct BroadcastBody {
    text: String,
    targets: Option<Vec<String>>,
}

async fn broadcast(State(s): State<AppState>, Json(body): Json<BroadcastBody>) -> ApiResult {
    let (total, scheduled) = s.host.broadcast_to_agents(body.targets.as_deref(), &body.text)?;
    Ok(Json(json!({ "total": total, "scheduled": scheduled })))
}

async fn webhook(State(s): State<AppState>, Path(name): Path<String>, headers: HeaderMap, body: String) -> ApiResult {
    let is_json = headers.get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("json"));
    let payload = if is_json || body.trim_start().starts_with(['{', '[']) {
        serde_json::from_str(&body).map_err(|e| ApiError::bad_request(format!("invalid JSON: {e}")))?
    } else {
        Value::String(body)
    };
    Ok(Json(json!({ "routines": s.host.fire_webhook(&name, payload)? })))
}

// Owner-authenticated coordination endpoints. File paths never come from download requests.
async fn tasks_list(State(s): State<AppState>) -> ApiResult {
    Ok(Json(json!({ "tasks": s.host.list_tasks()? })))
}
async fn task_get(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(serde_json::to_value(s.host.task(&id)?)?))
}
#[derive(Deserialize)]
struct TaskCreateBody {
    requester: String,
    #[serde(default)]
    group: Option<String>,
    #[serde(flatten)]
    request: DelegateTaskRequest,
}
async fn task_create(State(s): State<AppState>, Json(body): Json<TaskCreateBody>) -> ApiResult {
    let task = match body.group {
        Some(group) => s.host.delegate_group_task(&group, &body.requester, body.request).await?,
        None => s.host.delegate_task(&body.requester, body.request).await?,
    };
    Ok(Json(serde_json::to_value(task)?))
}
async fn task_cancel(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(serde_json::to_value(s.host.cancel_task(&id)?)?))
}
#[derive(Deserialize)]
struct TaskResumeBody {
    #[serde(default)]
    instructions: String,
}
async fn task_resume(State(s): State<AppState>, Path(id): Path<String>, Json(body): Json<TaskResumeBody>) -> ApiResult {
    Ok(Json(serde_json::to_value(s.host.resume_task(&id, body.instructions)?)?))
}
async fn artifact_get(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(serde_json::to_value(s.host.artifact(&id)?)?))
}
async fn artifact_download(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    let artifact = s.host.artifact(&id)?;
    let path = s.host.artifact_path(&id).await?;
    let file = tokio::fs::File::open(path).await.map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let stream = futures::stream::try_unfold(file, |mut file| async move {
        use tokio::io::AsyncReadExt;
        let mut buffer = vec![0u8; 65536];
        let n = file.read(&mut buffer).await?;
        if n == 0 {
            Ok::<_, std::io::Error>(None)
        } else {
            buffer.truncate(n);
            Ok(Some((buffer, file)))
        }
    });
    let encoded: String = artifact
        .name
        .as_bytes()
        .iter()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._".contains(b) { (*b as char).to_string() } else { format!("%{b:02X}") })
        .collect();
    let mut response = Response::new(axum::body::Body::from_stream(stream));
    let headers = response.headers_mut();
    headers.insert("content-type", "application/octet-stream".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("cache-control", "private, no-store".parse().unwrap());
    headers.insert("content-length", artifact.size.to_string().parse().map_err(|_| ApiError::bad_request("invalid artifact size"))?);
    headers.insert(
        "content-disposition",
        format!("attachment; filename*=UTF-8''{encoded}").parse().map_err(|_| ApiError::bad_request("invalid filename"))?,
    );
    Ok(response)
}
