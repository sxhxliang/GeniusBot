//! Router tests against a real host with the offline mock model.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gns_runtime::{AgentHost, AgentHostConfig};
use gns_server::model::ModelSettings;
use gns_server::{AppState, EventBus, ModelHub, router};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

async fn app(root: &std::path::Path, token: Option<&str>) -> (axum::Router, AgentHost) {
    let bus = EventBus::new(100);
    let settings = ModelSettings { provider: "mock".into(), ..Default::default() };
    let hub = Arc::new(ModelHub::new(settings, root.join("gns-server.json"), 50, bus.clone(), None));
    let host = AgentHost::open(AgentHostConfig::new(root), hub.clone()).await.unwrap();
    bus.forward(host.subscribe());
    (router(AppState { host: host.clone(), hub, bus, token: token.map(str::to_owned) }), host)
}

async fn call(app: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let body = body.map(|b| Body::from(b.to_string())).unwrap_or_else(Body::empty);
    let response = app.clone().oneshot(request.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn send_is_answered_and_every_model_call_is_logged() {
    let dir = tempfile::tempdir().unwrap();
    let (app, host) = app(dir.path(), None).await;

    let (status, agent) = call(&app, "POST", "/api/agents", Some(json!({"name": "Planner", "kickstart": false}))).await;
    assert_eq!(status, StatusCode::OK);
    let id = agent["id"].as_str().unwrap().to_owned();

    let (status, sent) = call(&app, "POST", "/api/agents/Planner/messages", Some(json!({"text": "hello", "wait": true}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sent["result"]["sentMessageCount"], 1);

    let (_, page) = call(&app, "GET", "/api/agents/Planner/transcript", None).await;
    let replies: Vec<&str> = page["items"].as_array().unwrap().iter().filter_map(|i| i["entry"]["message"]["content"].as_str()).collect();
    assert_eq!(replies, ["(mock) you said: hello"]);

    let (_, logs) = call(&app, "GET", &format!("/api/llm-logs?agent={id}"), None).await;
    let logs = logs["logs"].as_array().unwrap();
    assert!(logs.iter().all(|l| l["purpose"] == "turn" && l["status"] == "ok"));
    assert_eq!(logs.last().unwrap()["toolCalls"], json!(["SendMessage"]));
    let first = logs.last().unwrap()["id"].as_u64().unwrap();
    let (status, detail) = call(&app, "GET", &format!("/api/llm-logs/{first}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(detail["request"]["system"].as_str().unwrap().contains("Planner"));
    assert_eq!(detail["response"]["tool_calls"][0]["name"], "SendMessage");
    host.shutdown().await;
}

#[tokio::test]
async fn model_settings_are_saved_without_echoing_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let (app, host) = app(dir.path(), None).await;

    let patch = json!({"provider": "openai", "model": "qwen-plus", "baseUrl": "http://127.0.0.1:9/v1", "apiKey": "sk-test-1234567890"});
    let (status, view) = call(&app, "PUT", "/api/model", Some(patch)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["status"]["ready"], true);
    assert_eq!(view["settings"]["apiKeySet"], true);
    assert!(!view.to_string().contains("1234567890"));
    let saved: ModelSettings = serde_json::from_str(&std::fs::read_to_string(dir.path().join("gns-server.json")).unwrap()).unwrap();
    assert_eq!(saved.api_key, "sk-test-1234567890");

    let (_, view) = call(&app, "PUT", "/api/model", Some(json!({"provider": "nonsense"}))).await;
    assert_eq!(view["status"]["ready"], false);
    assert!(view["status"]["error"].as_str().unwrap().contains("unknown provider"));
    host.shutdown().await;
}

#[tokio::test]
async fn token_guards_the_api_but_not_the_console() {
    let dir = tempfile::tempdir().unwrap();
    let (app, host) = app(dir.path(), Some("s3cret")).await;
    assert_eq!(call(&app, "GET", "/api/info", None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(call(&app, "GET", "/api/info?token=s3cret", None).await.0, StatusCode::OK);
    let request = Request::get("/api/agents").header("authorization", "Bearer s3cret").body(Body::empty()).unwrap();
    assert_eq!(app.clone().oneshot(request).await.unwrap().status(), StatusCode::OK);
    let index = app.clone().oneshot(Request::get("/agent/x/chat").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(index.status(), StatusCode::OK);
    assert_eq!(call(&app, "GET", "/api/agents/nobody?token=s3cret", None).await.0, StatusCode::NOT_FOUND);
    host.shutdown().await;
}

#[tokio::test]
async fn artifact_download_requires_auth_and_returns_original_binary_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let (app, host) = app(dir.path(), Some("download-secret")).await;
    let agent = host.create_agent(gns_core::AgentSpec::new("Files", "")).await.unwrap();
    let source = dir.path().join("payload.bin");
    let bytes: Vec<u8> = (0..20000).map(|i| (i % 256) as u8).collect();
    std::fs::write(&source, &bytes).unwrap();
    let store = gns_runtime::gns_store::CoordinationStore::open(dir.path()).unwrap();
    let a = store.snapshot(&agent.id, &source, 100000).unwrap();
    let url = format!("/api/artifacts/{}/download", a.id);
    assert_eq!(call(&app, "GET", &url, None).await.0, StatusCode::UNAUTHORIZED);
    let request = Request::get(&url).header("authorization", "Bearer download-secret").body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()["content-disposition"].to_str().unwrap().starts_with("attachment;"));
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(response.headers()["content-type"], "application/octet-stream");
    assert_eq!(axum::body::to_bytes(response.into_body(), 100000).await.unwrap().as_ref(), bytes);
    let request =
        Request::get("/api/artifacts/missing/download").header("authorization", "Bearer download-secret").body(Body::empty()).unwrap();
    assert!(!app.clone().oneshot(request).await.unwrap().status().is_success());
    host.shutdown().await;
}

#[tokio::test]
async fn task_api_creates_queries_and_cancels_without_unprotected_access() {
    let dir = tempfile::tempdir().unwrap();
    let (app, host) = app(dir.path(), Some("task-secret")).await;
    let a = host.create_agent(gns_core::AgentSpec::new("Planner", "")).await.unwrap();
    let b = host.create_agent(gns_core::AgentSpec::new("Coder", "")).await.unwrap();
    assert_eq!(call(&app, "GET", "/api/tasks", None).await.0, StatusCode::UNAUTHORIZED);
    let (status, task) = call(
        &app,
        "POST",
        "/api/tasks?token=task-secret",
        Some(json!({"requester":a.id,"target_id":b.id,"task":"Write a report","require_files":true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = task["id"].as_str().unwrap();
    let (_, fetched) = call(&app, "GET", &format!("/api/tasks/{id}?token=task-secret"), None).await;
    assert_eq!(fetched["origin"]["id"], a.id.as_str());
    let (status, cancelled) = call(&app, "POST", &format!("/api/tasks/{id}/cancel?token=task-secret"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cancelled["status"], "cancelled");
    host.shutdown().await;
}
