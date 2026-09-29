//! Model settings, a hot-swappable [`LlmProvider`] and the model-call log.
//!
//! The host is opened once with an [`Arc<ModelHub>`]; the hub forwards every
//! call to whatever provider the current [`ModelSettings`] describe, so the
//! console can change model, endpoint and key without restarting agents.
//! Every call (turn steps, subagents, summaries, memory passes) is recorded
//! with its full request and response.

use crate::events::EventBus;
use async_trait::async_trait;
use gns_core::*;
use gns_llm::{DEFAULT_MODEL, GenaiConfig, GenaiProvider, MockLlm, ModelProvider, OPENAI_MODEL_ENV};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// Persisted model configuration (`<root>/gns-server.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelSettings {
    /// `""` (infer from the model name), `openai`, `kimi`, `moonshot`, `auto` or `mock`.
    pub provider: String,
    /// Empty: `$GNS_MODEL`, then `$OPENAI_MODEL`, then the SDK default.
    pub model: String,
    /// OpenAI-compatible base URL, version path included. Empty: the provider's env var.
    pub base_url: String,
    /// Explicit key. Empty: `api_key_env`, then the provider's default variable.
    pub api_key: String,
    pub api_key_env: String,
    pub stream: bool,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
}

impl Default for ModelSettings {
    fn default() -> Self {
        Self {
            provider: String::new(),
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            api_key_env: String::new(),
            stream: true,
            temperature: None,
            max_tokens: None,
        }
    }
}

/// A settings edit from the console; absent fields keep their value, and
/// `apiKey: ""` clears the stored key.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSettingsPatch {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub stream: Option<bool>,
    #[serde(default, deserialize_with = "double_option")]
    pub temperature: Option<Option<f64>>,
    #[serde(default, deserialize_with = "double_option")]
    pub max_tokens: Option<Option<u32>>,
}

/// Distinguish a missing field (keep) from `null` (clear).
fn double_option<'de, T: Deserialize<'de>, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

impl ModelSettingsPatch {
    pub fn apply(self, settings: &mut ModelSettings) {
        let trim = |s: String| s.trim().to_owned();
        if let Some(v) = self.provider {
            settings.provider = trim(v).to_ascii_lowercase();
        }
        if let Some(v) = self.model {
            settings.model = trim(v);
        }
        if let Some(v) = self.base_url {
            settings.base_url = trim(v);
        }
        if let Some(v) = self.api_key {
            settings.api_key = trim(v);
        }
        if let Some(v) = self.api_key_env {
            settings.api_key_env = trim(v);
        }
        if let Some(v) = self.stream {
            settings.stream = v;
        }
        if let Some(v) = self.temperature {
            settings.temperature = v;
        }
        if let Some(v) = self.max_tokens {
            settings.max_tokens = v;
        }
    }
}

/// What the console shows about the settings: the key is never sent back.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSettingsView {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub api_key_set: bool,
    /// `sk-…abcd`
    pub api_key_preview: String,
    pub api_key_env: String,
    pub stream: bool,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
}

impl From<&ModelSettings> for ModelSettingsView {
    fn from(s: &ModelSettings) -> Self {
        Self {
            provider: s.provider.clone(),
            model: s.model.clone(),
            base_url: s.base_url.clone(),
            api_key_set: !s.api_key.is_empty(),
            api_key_preview: mask_key(&s.api_key),
            api_key_env: s.api_key_env.clone(),
            stream: s.stream,
            temperature: s.temperature,
            max_tokens: s.max_tokens,
        }
    }
}

fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    match chars.len() {
        0 => String::new(),
        n if n <= 8 => "•".repeat(n),
        n => format!("{}…{}", chars[..3].iter().collect::<String>(), chars[n - 4..].iter().collect::<String>()),
    }
}

/// The provider the current settings resolved to.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatus {
    pub ready: bool,
    pub model: String,
    pub provider: String,
    pub base_url: Option<String>,
    pub keyless: bool,
    /// Why the provider could not be built (missing key, bad URL, …).
    pub error: Option<String>,
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// The effective model name for these settings.
pub fn effective_model(settings: &ModelSettings) -> String {
    Some(settings.model.clone())
        .filter(|m| !m.is_empty())
        .or_else(|| env_non_empty("GNS_MODEL"))
        .or_else(|| env_non_empty(OPENAI_MODEL_ENV))
        .unwrap_or_else(|| DEFAULT_MODEL.to_owned())
}

/// Build a provider from settings; the same rules as `gns-cli`'s flags.
pub fn build_provider(settings: &ModelSettings) -> (Arc<dyn LlmProvider>, ModelStatus) {
    if settings.provider == "mock" {
        let status = ModelStatus { ready: true, model: "mock".into(), provider: "mock".into(), base_url: None, keyless: true, error: None };
        return (Arc::new(mock_llm()), status);
    }
    let model = effective_model(settings);
    let config = match settings.provider.as_str() {
        "" => Ok(GenaiConfig::new(model.clone())),
        "openai" => Ok(GenaiConfig::openai(model.clone())),
        "kimi" => Ok(GenaiConfig::kimi(model.clone())),
        "moonshot" => Ok(GenaiConfig::moonshot(model.clone())),
        "auto" => Ok(GenaiConfig::with_provider(model.clone(), ModelProvider::Auto)),
        other => Err(format!("unknown provider {other}; use openai | kimi | moonshot | auto | mock")),
    };
    let built = config.and_then(|mut config| {
        if !settings.api_key_env.is_empty() {
            config.api_key_env = Some(settings.api_key_env.clone());
        }
        if !settings.api_key.is_empty() {
            config.api_key = Some(settings.api_key.clone());
        }
        if !settings.base_url.is_empty() {
            config.base_url = Some(settings.base_url.clone());
        }
        config.stream = settings.stream;
        config.temperature = settings.temperature;
        config.max_tokens = settings.max_tokens;
        GenaiProvider::new(config).map_err(|e| e.to_string())
    });
    match built {
        Ok(provider) => {
            let status = ModelStatus {
                ready: true,
                model: provider.model_name(),
                provider: format!("{:?}", provider.provider()).to_lowercase(),
                base_url: provider.base_url().map(str::to_owned),
                keyless: provider.is_keyless(),
                error: None,
            };
            (Arc::new(provider), status)
        }
        Err(error) => {
            let status = ModelStatus {
                ready: false,
                model,
                provider: settings.provider.clone(),
                base_url: Some(settings.base_url.clone()).filter(|u| !u.is_empty()),
                keyless: false,
                error: Some(error.clone()),
            };
            (Arc::new(Unconfigured(error)), status)
        }
    }
}

/// Stands in until the settings describe a working provider.
struct Unconfigured(String);

#[async_trait]
impl LlmProvider for Unconfigured {
    async fn complete(&self, _: LlmRequest, _: CancellationToken, _: DeltaSink<'_>) -> Result<LlmResponse, LlmError> {
        Err(LlmError::Config(format!("{} (open Settings in the console to configure the model)", self.0)))
    }
}

/// An offline model that answers every user turn with `(mock) you said: …`.
fn mock_llm() -> MockLlm {
    MockLlm::new().with_responder(|req| {
        if let Some(LlmMessage::ToolResults(_)) = req.messages.last() {
            return LlmResponse::text("done");
        }
        let last = match req.messages.last() {
            Some(LlmMessage::User { text, .. }) => text.as_str(),
            _ => "",
        };
        // The first line of the user's text, minus the turn wrappers.
        let said = last
            .lines()
            .map(str::trim)
            .find(|l| {
                let wrapper = l.starts_with("<timestamp>") || l.starts_with("<incoming_message_id>") || l.starts_with("<user_query>");
                let address = l.starts_with('[') && l.ends_with(']') && !l.contains(' ');
                !(l.is_empty() || wrapper || address)
            })
            .unwrap_or("");
        let said = said.strip_prefix(gns_core::prompt::SAND_HIDDEN_PROMPT_MARKER).unwrap_or(said);
        LlmResponse::tool_call(SEND_MESSAGE_TOOL_NAME, serde_json::json!({"type": "text", "content": format!("(mock) you said: {said}")}))
    })
}

/// System prompts of the runtime's text-only passes, by purpose.
static AUX_PROMPTS: LazyLock<[(&'static str, String); 4]> = LazyLock::new(|| {
    [
        ("summarization", gns_core::prompt::SUMMARIZATION_SYSTEM_PROMPT.to_owned()),
        ("memory-extraction", gns_core::memory::build_extraction_system_prompt()),
        ("dream", gns_core::memory::build_dream_system_prompt()),
        ("episode", gns_core::memory::build_episode_system_prompt()),
    ]
});

/// What a model call was for, guessed from its request: turn steps carry
/// the agent id as cache key, subagents carry tools, the rest are text passes.
fn classify(request: &LlmRequest) -> &'static str {
    if request.options.prompt_cache_key.is_some() {
        "turn"
    } else if !request.tools.is_empty() {
        "subagent"
    } else {
        AUX_PROMPTS.iter().find(|(_, prompt)| *prompt == request.system).map(|(purpose, _)| *purpose).unwrap_or("text")
    }
}

/// One recorded model call.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmLog {
    #[serde(flatten)]
    pub summary: LlmLogSummary,
    pub request: LlmRequest,
    pub response: Option<LlmResponse>,
}

/// The list view of a model call.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmLogSummary {
    pub id: u64,
    pub started_at_ms: i64,
    pub duration_ms: Option<u64>,
    /// `pending`, `ok`, `error` or `cancelled`.
    pub status: &'static str,
    pub purpose: &'static str,
    pub agent_id: Option<String>,
    pub model: String,
    pub message_count: usize,
    pub tool_count: usize,
    pub system_chars: usize,
    pub usage: Option<UsageTotals>,
    pub tool_calls: Vec<String>,
    pub text_preview: Option<String>,
    pub stop_reason: Option<String>,
    pub error: Option<String>,
}

/// The hot-swappable provider and its call log.
pub struct ModelHub {
    active: RwLock<(Arc<dyn LlmProvider>, ModelStatus)>,
    settings: RwLock<ModelSettings>,
    settings_path: PathBuf,
    logs: Mutex<VecDeque<LlmLog>>,
    capacity: usize,
    next_id: AtomicU64,
    bus: EventBus,
    /// Append every finished call as JSON Lines, when set.
    log_file: Option<PathBuf>,
}

impl std::fmt::Debug for ModelHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelHub").field("status", &self.status()).finish()
    }
}

impl ModelHub {
    pub fn new(settings: ModelSettings, settings_path: PathBuf, capacity: usize, bus: EventBus, log_file: Option<PathBuf>) -> Self {
        let active = build_provider(&settings);
        Self {
            active: RwLock::new(active),
            settings: RwLock::new(settings),
            settings_path,
            logs: Mutex::new(VecDeque::new()),
            capacity: capacity.max(1),
            next_id: AtomicU64::new(1),
            bus,
            log_file,
        }
    }

    /// Read `<root>/gns-server.json`, when present.
    pub fn load_settings(path: &std::path::Path) -> anyhow::Result<Option<ModelSettings>> {
        match std::fs::read_to_string(path) {
            Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn status(&self) -> ModelStatus {
        self.active.read().expect("model lock").1.clone()
    }

    pub fn settings(&self) -> ModelSettings {
        self.settings.read().expect("settings lock").clone()
    }

    /// Apply an edit, rebuild the provider and persist the settings. A
    /// provider that cannot be built is still saved (the status says why).
    pub fn update(&self, patch: ModelSettingsPatch) -> anyhow::Result<ModelStatus> {
        let mut settings = self.settings();
        patch.apply(&mut settings);
        let active = build_provider(&settings);
        let status = active.1.clone();
        self.save(&settings)?;
        *self.settings.write().expect("settings lock") = settings;
        *self.active.write().expect("model lock") = active;
        self.bus.publish_server("model-changed", serde_json::to_value(&status).unwrap_or_default());
        Ok(status)
    }

    fn save(&self, settings: &ModelSettings) -> anyhow::Result<()> {
        if let Some(dir) = self.settings_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.settings_path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(settings)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &self.settings_path)?;
        Ok(())
    }

    /// Summaries, newest first, optionally for one agent.
    pub fn log_summaries(&self, agent: Option<&str>, limit: usize) -> Vec<LlmLogSummary> {
        let logs = self.logs.lock().expect("log lock");
        logs.iter()
            .rev()
            .filter(|l| agent.is_none_or(|a| l.summary.agent_id.as_deref() == Some(a)))
            .take(limit)
            .map(|l| l.summary.clone())
            .collect()
    }

    pub fn log(&self, id: u64) -> Option<LlmLog> {
        self.logs.lock().expect("log lock").iter().find(|l| l.summary.id == id).cloned()
    }

    pub fn clear_logs(&self) {
        self.logs.lock().expect("log lock").clear();
    }

    fn record(&self, log: LlmLog) {
        let mut logs = self.logs.lock().expect("log lock");
        if let Some(existing) = logs.iter_mut().find(|l| l.summary.id == log.summary.id) {
            *existing = log;
        } else {
            logs.push_back(log);
            while logs.len() > self.capacity {
                logs.pop_front();
            }
        }
    }

    fn append_to_file(&self, log: &LlmLog) {
        let Some(path) = &self.log_file else { return };
        let line = match serde_json::to_string(log) {
            Ok(l) => l,
            Err(_) => return,
        };
        use std::io::Write;
        let written = std::fs::OpenOptions::new().create(true).append(true).open(path).and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = written {
            tracing::warn!(path = %path.display(), "writing the model log failed: {e}");
        }
    }
}

#[async_trait]
impl LlmProvider for ModelHub {
    async fn complete(&self, request: LlmRequest, cancel: CancellationToken, on_delta: DeltaSink<'_>) -> Result<LlmResponse, LlmError> {
        let (provider, status) = self.active.read().expect("model lock").clone();
        let mut summary = LlmLogSummary {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            started_at_ms: gns_core::text::now_ms(),
            duration_ms: None,
            status: "pending",
            purpose: classify(&request),
            agent_id: request.options.prompt_cache_key.clone(),
            model: status.model.clone(),
            message_count: request.messages.len(),
            tool_count: request.tools.len(),
            system_chars: request.system.chars().count(),
            usage: None,
            tool_calls: Vec::new(),
            text_preview: None,
            stop_reason: None,
            error: None,
        };
        self.record(LlmLog { summary: summary.clone(), request: request.clone(), response: None });
        self.bus.publish_server("llm-log", serde_json::to_value(&summary).unwrap_or_default());

        let started = Instant::now();
        let result = provider.complete(request.clone(), cancel, on_delta).await;
        summary.duration_ms = Some(started.elapsed().as_millis() as u64);
        let response = match &result {
            Ok(response) => {
                summary.status = "ok";
                summary.usage = Some(response.usage);
                summary.tool_calls = response.tool_calls.iter().map(|c| c.name.clone()).collect();
                summary.text_preview = response.text.as_deref().map(|t| gns_core::text::clamp_line(t, 160));
                summary.stop_reason = response.stop_reason.clone();
                Some(response.clone())
            }
            Err(LlmError::Cancelled) => {
                summary.status = "cancelled";
                None
            }
            Err(e) => {
                summary.status = "error";
                summary.error = Some(e.to_string());
                None
            }
        };
        let log = LlmLog { summary: summary.clone(), request, response };
        self.append_to_file(&log);
        self.record(log);
        self.bus.publish_server("llm-log", serde_json::to_value(&summary).unwrap_or_default());
        result
    }

    fn model_name(&self) -> String {
        self.status().model
    }
}

/// `GET {base}/models` on an OpenAI-compatible endpoint.
pub async fn list_remote_models(settings: &ModelSettings) -> anyhow::Result<Vec<String>> {
    let provider = match settings.provider.as_str() {
        "" => ModelProvider::infer(&effective_model(settings)),
        "kimi" => ModelProvider::Kimi,
        "moonshot" => ModelProvider::Moonshot,
        "mock" => return Ok(vec!["mock".to_owned()]),
        _ => ModelProvider::OpenAi,
    };
    let default_base = match provider {
        ModelProvider::Kimi => "https://api.moonshot.ai/v1",
        ModelProvider::Moonshot => "https://api.moonshot.cn/v1",
        _ => "https://api.openai.com/v1",
    };
    let base = Some(settings.base_url.clone())
        .filter(|u| !u.is_empty())
        .or_else(|| provider.default_base_url_env().and_then(env_non_empty))
        .unwrap_or_else(|| default_base.to_owned());
    let key = Some(settings.api_key.clone())
        .filter(|k| !k.is_empty())
        .or_else(|| Some(settings.api_key_env.clone()).filter(|e| !e.is_empty()).and_then(|e| env_non_empty(&e)))
        .or_else(|| provider.default_api_key_env().and_then(env_non_empty));
    let url = format!("{}/models", base.trim_end_matches('/'));
    let mut request = reqwest::Client::new().get(&url).timeout(std::time::Duration::from_secs(20));
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await?;
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("{url} returned {status}: {}", gns_core::text::clamp_line(&body.to_string(), 300));
    }
    let mut models: Vec<String> = body["data"]
        .as_array()
        .or_else(|| body["models"].as_array())
        .map(|items| items.iter().filter_map(|m| m["id"].as_str().or_else(|| m["name"].as_str()).map(str::to_owned)).collect())
        .unwrap_or_default();
    models.sort();
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_keeps_absent_fields_and_clears_nulls() {
        let mut settings =
            ModelSettings { model: "gpt-4o".into(), api_key: "sk-secret".into(), temperature: Some(0.2), ..Default::default() };
        let patch: ModelSettingsPatch = serde_json::from_str(r#"{"baseUrl":" http://x/v1 ","temperature":null}"#).unwrap();
        patch.apply(&mut settings);
        assert_eq!(settings.model, "gpt-4o");
        assert_eq!(settings.api_key, "sk-secret");
        assert_eq!(settings.base_url, "http://x/v1");
        assert_eq!(settings.temperature, None);
    }

    #[test]
    fn view_masks_the_key() {
        let view = ModelSettingsView::from(&ModelSettings { api_key: "sk-1234567890abcd".into(), ..Default::default() });
        assert!(view.api_key_set);
        assert_eq!(view.api_key_preview, "sk-…abcd");
        assert!(!serde_json::to_string(&view).unwrap().contains("567890"));
    }
}
