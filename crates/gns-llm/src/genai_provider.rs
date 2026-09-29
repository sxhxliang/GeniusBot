//! `genai` adapter.
//!
//! Any OpenAI-compatible endpoint can be used by giving a base URL: the
//! official API, a gateway (OpenRouter, OneAPI, SiliconFlow, …) or a local
//! server (vLLM, LM Studio, llama.cpp, Ollama's `/v1`). Address, key and
//! model come from `OPENAI_BASE_URL`, `OPENAI_API_KEY` and `OPENAI_MODEL`,
//! from [`GenaiConfig`] fields, or from the `gns-cli` flags.

use async_trait::async_trait;
use futures::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{
    Binary, ChatMessage, ChatOptions, ChatRequest, ChatStreamEvent, ContentPart, MessageContent, Tool, ToolCall, ToolResponse,
};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget};
use gns_core::{DeltaSink, LlmDelta, LlmError, LlmMessage, LlmProvider, LlmRequest, LlmResponse, LlmToolCall, UsageTotals};
use tokio_util::sync::CancellationToken;

/// Environment variable holding the API key for OpenAI or any OpenAI-compatible endpoint.
pub const OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";
/// Environment variable holding a custom OpenAI-compatible base URL, e.g. `https://api.example.com/v1`.
pub const OPENAI_BASE_URL_ENV: &str = "OPENAI_BASE_URL";
/// Environment variable holding the model name for [`GenaiConfig::openai_from_env`].
pub const OPENAI_MODEL_ENV: &str = "OPENAI_MODEL";
/// Model used when nothing else is configured.
pub const DEFAULT_MODEL: &str = "gpt-4o-mini";

/// Bearer token sent to a custom endpoint when no key is configured. Local
/// servers ignore it; a server that does need a key rejects it with a clear 401.
const NO_API_KEY_PLACEHOLDER: &str = "no-api-key";

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// Which vendor a model is served by. Drives the default API key variable,
/// the default endpoint and vendor-specific request constraints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModelProvider {
    /// OpenAI (`gpt-*`, `o1`/`o3`/`o4-*`) or any OpenAI-compatible endpoint, key `OPENAI_API_KEY`.
    OpenAi,
    /// Kimi on the international Moonshot platform (`kimi-*`, api.moonshot.ai), key `KIMI_API_KEY`.
    Kimi,
    /// Moonshot China platform (`moonshot-*`, api.moonshot.cn), key `MOONSHOT_API_KEY`.
    Moonshot,
    /// Any other provider `genai` can infer from the model name (Anthropic, Gemini, DeepSeek, …).
    Auto,
}

impl ModelProvider {
    /// Infer the provider from a model name alone (no environment lookups).
    pub fn infer(model: &str) -> Self {
        let lower = model.trim().to_ascii_lowercase();
        let (namespace, name) = lower.split_once("::").map(|(ns, n)| (Some(ns), n)).unwrap_or((None, lower.as_str()));
        match namespace {
            Some("openai") => return ModelProvider::OpenAi,
            Some("kimi") => return ModelProvider::Kimi,
            Some("moonshot") => return ModelProvider::Moonshot,
            Some(_) => return ModelProvider::Auto,
            None => {}
        }
        if name.starts_with("kimi") {
            ModelProvider::Kimi
        } else if name.starts_with("moonshot") {
            ModelProvider::Moonshot
        } else if name.starts_with("gpt")
            || name.starts_with("o1")
            || name.starts_with("o3")
            || name.starts_with("o4")
            || name.starts_with("chatgpt")
        {
            ModelProvider::OpenAi
        } else {
            ModelProvider::Auto
        }
    }

    /// Whether `genai` can route this model name by itself: a `vendor::`
    /// namespace, or a name with a native adapter (`gpt-*`, `claude-*`,
    /// `gemini-*`, `deepseek-*`, `grok-*`, `glm-*`, …). Anything else
    /// (`qwen-plus`, `llama-3.3-70b`, `Qwen/Qwen2.5-72B-Instruct`, …) falls
    /// back to `genai`'s local-Ollama adapter, which is only right when no
    /// custom endpoint is configured.
    pub fn is_known_to_genai(model: &str) -> bool {
        let model = model.trim();
        model.contains("::") || !matches!(AdapterKind::from_model(model), Ok(AdapterKind::Ollama))
    }

    /// Default environment variable holding the API key.
    pub fn default_api_key_env(self) -> Option<&'static str> {
        match self {
            ModelProvider::OpenAi => Some(OPENAI_API_KEY_ENV),
            ModelProvider::Kimi => Some("KIMI_API_KEY"),
            ModelProvider::Moonshot => Some("MOONSHOT_API_KEY"),
            ModelProvider::Auto => None,
        }
    }

    /// Default environment variable for a base URL override.
    pub fn default_base_url_env(self) -> Option<&'static str> {
        match self {
            ModelProvider::OpenAi => Some(OPENAI_BASE_URL_ENV),
            ModelProvider::Kimi => Some("KIMI_BASE_URL"),
            ModelProvider::Moonshot => Some("MOONSHOT_BASE_URL"),
            ModelProvider::Auto => None,
        }
    }

    /// The `genai` adapter to force when a custom base URL is used.
    fn adapter_kind(self) -> Option<AdapterKind> {
        match self {
            ModelProvider::OpenAi => Some(AdapterKind::OpenAI),
            ModelProvider::Kimi => Some(AdapterKind::Kimi),
            ModelProvider::Moonshot => Some(AdapterKind::Moonshot),
            ModelProvider::Auto => None,
        }
    }

    /// Vendor-imposed temperature constraint, when any.
    fn clamp_temperature(self, temperature: f64) -> f64 {
        match self {
            // Kimi/Moonshot models reject every value except 1
            // ("invalid temperature: only 1 is allowed for this model").
            ModelProvider::Kimi | ModelProvider::Moonshot => 1.0,
            _ => temperature,
        }
    }

    /// Vendor-required default temperature when the caller sets none.
    fn default_temperature(self) -> Option<f64> {
        match self {
            ModelProvider::Kimi | ModelProvider::Moonshot => Some(1.0),
            _ => None,
        }
    }
}

/// Configuration for [`GenaiProvider`].
///
/// Three things pick the endpoint: the model name, the provider and the base
/// URL. The precedence for the API key is `api_key`, then the variable named
/// by `api_key_env`, then the provider's default variable.
#[derive(Clone)]
pub struct GenaiConfig {
    /// Model name, e.g. `gpt-4o-mini`, `kimi-k2-turbo-preview`, `moonshot-v1-8k`,
    /// or whatever a custom endpoint serves (`qwen-plus`, `my-finetune`, …).
    pub model: String,
    /// Which vendor serves the model (inferred from the name by default).
    pub provider: ModelProvider,
    /// Environment variable holding the API key (`None` lets `genai` use the
    /// provider's own default, e.g. `ANTHROPIC_API_KEY`).
    pub api_key_env: Option<String>,
    /// Explicit API key; takes precedence over `api_key_env`.
    pub api_key: Option<String>,
    /// OpenAI-compatible base URL override, version path included
    /// (`https://api.example.com/v1`, `http://localhost:8000/v1`). When set,
    /// every request goes to this endpoint using the provider's adapter
    /// (OpenAI wire format). Defaults to `OPENAI_BASE_URL` / `KIMI_BASE_URL` /
    /// `MOONSHOT_BASE_URL` for the matching provider.
    pub base_url: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    /// Use streaming (default true).
    pub stream: bool,
}

impl std::fmt::Debug for GenaiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenaiConfig")
            .field("model", &self.model)
            .field("provider", &self.provider)
            .field("api_key_env", &self.api_key_env)
            .field("api_key", &self.api_key.as_ref().map(|_| "REDACTED"))
            .field("base_url", &self.base_url)
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .field("stream", &self.stream)
            .finish()
    }
}

impl Default for GenaiConfig {
    fn default() -> Self {
        Self::new(DEFAULT_MODEL)
    }
}

impl GenaiConfig {
    /// Config for a model name. The vendor is inferred from the name
    /// (`kimi-*` → Kimi, `moonshot-*` → Moonshot, `gpt-*` → OpenAI, `claude-*`,
    /// `gemini-*`, … → `genai`'s own inference). A name nobody recognises
    /// (`qwen-plus`, `llama-3.3-70b`, …) is served from `OPENAI_BASE_URL` with
    /// the OpenAI protocol when that variable is set.
    pub fn new(model: impl Into<String>) -> Self {
        let model = model.into();
        let mut config = Self::with_provider(model.clone(), ModelProvider::infer(&model));
        if config.provider == ModelProvider::Auto && config.base_url.is_none() && !ModelProvider::is_known_to_genai(&config.model) {
            config.base_url = env_non_empty(OPENAI_BASE_URL_ENV);
        }
        config.resolve_routing();
        config
    }

    /// Config for a model with an explicit provider. The base URL defaults to
    /// the provider's variable (`OPENAI_BASE_URL`, `KIMI_BASE_URL`, `MOONSHOT_BASE_URL`).
    pub fn with_provider(model: impl Into<String>, provider: ModelProvider) -> Self {
        let base_url = provider.default_base_url_env().and_then(env_non_empty);
        Self {
            model: model.into(),
            provider,
            api_key_env: provider.default_api_key_env().map(str::to_owned),
            api_key: None,
            base_url,
            temperature: None,
            max_tokens: None,
            stream: true,
        }
    }

    /// Any OpenAI-compatible endpoint: every request goes to `base_url`
    /// (version path included, e.g. `https://api.example.com/v1` or
    /// `http://localhost:8000/v1`) with the OpenAI wire protocol, whatever the
    /// model name. The key comes from [`with_api_key`](Self::with_api_key),
    /// else `OPENAI_API_KEY`; a keyless local server works without either.
    pub fn openai_compatible(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self::openai(model).with_base_url(base_url)
    }

    /// Config from the environment only: `OPENAI_MODEL` (default
    /// `gpt-4o-mini`), `OPENAI_BASE_URL` (default api.openai.com) and
    /// `OPENAI_API_KEY`. Always the OpenAI protocol.
    pub fn openai_from_env() -> Self {
        Self::openai(env_non_empty(OPENAI_MODEL_ENV).unwrap_or_else(|| DEFAULT_MODEL.to_owned()))
    }

    /// Kimi (Moonshot international, api.moonshot.ai). Example models:
    /// `kimi-k2-turbo-preview`, `kimi-k2-thinking`, `kimi-k2-0905-preview`.
    pub fn kimi(model: impl Into<String>) -> Self {
        Self::with_provider(model, ModelProvider::Kimi)
    }

    /// Moonshot China platform (api.moonshot.cn). Example models:
    /// `moonshot-v1-8k`, `kimi-k2-0905-preview` (same names, .cn endpoint).
    pub fn moonshot(model: impl Into<String>) -> Self {
        Self::with_provider(model, ModelProvider::Moonshot)
    }

    /// OpenAI, or an OpenAI-compatible gateway when `OPENAI_BASE_URL` is set.
    pub fn openai(model: impl Into<String>) -> Self {
        Self::with_provider(model, ModelProvider::OpenAi)
    }

    /// Override the API key.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Name the environment variable holding the API key.
    pub fn with_api_key_env(mut self, env: impl Into<String>) -> Self {
        self.api_key_env = Some(env.into());
        self
    }

    /// Override the base URL (version path included, e.g. `https://host/v1`).
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self.resolve_routing();
        self
    }

    /// A model `genai` cannot place on its own, sent to an explicit endpoint,
    /// is served with the OpenAI protocol (and `OPENAI_API_KEY` by default).
    /// Idempotent; [`GenaiProvider::new`] applies it too, so setting the
    /// fields directly works as well.
    fn resolve_routing(&mut self) {
        if self.provider == ModelProvider::Auto && self.base_url.is_some() && !ModelProvider::is_known_to_genai(&self.model) {
            self.provider = ModelProvider::OpenAi;
            if self.api_key.is_none() && self.api_key_env.is_none() {
                self.api_key_env = Some(OPENAI_API_KEY_ENV.to_owned());
            }
        }
    }

    /// Model name as `genai` should see it: with the explicit provider
    /// namespace (`kimi::kimi-k2-turbo-preview`) so routing never depends
    /// on prefix guessing.
    fn routed_model_name(&self) -> String {
        if self.model.contains("::") {
            return self.model.clone();
        }
        match self.provider {
            ModelProvider::OpenAi => format!("openai::{}", self.model),
            ModelProvider::Kimi => format!("kimi::{}", self.model),
            ModelProvider::Moonshot => format!("moonshot::{}", self.model),
            ModelProvider::Auto => self.model.clone(),
        }
    }
}

/// Trim, require an `http(s)://` scheme and end with `/` so that `genai`
/// joins `chat/completions` onto the full path (`…/v1/` + `chat/completions`).
fn normalize_base_url(url: &str) -> Result<String, LlmError> {
    let url = url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(LlmError::Config(format!("base URL must start with http:// or https://, got {url:?}")));
    }
    Ok(if url.ends_with('/') { url.to_owned() } else { format!("{url}/") })
}

/// Whether a normalised base URL is OpenAI's own API rather than a server
/// that merely speaks its protocol.
fn is_openai_api_url(url: &str) -> bool {
    url.strip_prefix("https://api.openai.com").is_some_and(|rest| rest.starts_with('/'))
}

/// [`LlmProvider`] backed by `genai`.
pub struct GenaiProvider {
    client: Client,
    config: GenaiConfig,
    routed_model: String,
    /// Requests go to a custom endpoint without any API key.
    keyless: bool,
    /// Requests carry [`LlmOptions::prompt_cache_key`](gns_core::LlmOptions::prompt_cache_key).
    sends_prompt_cache_key: bool,
}

impl std::fmt::Debug for GenaiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenaiProvider")
            .field("model", &self.config.model)
            .field("provider", &self.config.provider)
            .field("base_url", &self.config.base_url)
            .field("keyless", &self.keyless)
            .finish()
    }
}

impl GenaiProvider {
    /// Build a client. Fails when the base URL is malformed or when an API key
    /// is required but cannot be found. A custom endpoint without any key is
    /// used without one (keyless local servers); the official endpoints require it.
    pub fn new(mut config: GenaiConfig) -> Result<Self, LlmError> {
        config.resolve_routing();
        let base_url = config.base_url.as_deref().map(normalize_base_url).transpose()?;
        config.base_url = base_url.clone();
        // `prompt_cache_key` is an OpenAI extension; strict OpenAI-compatible
        // servers reject it with a 400 ("Unsupported parameter(s): `prompt_cache_key`").
        let sends_prompt_cache_key = config.provider == ModelProvider::OpenAi && base_url.as_deref().is_none_or(is_openai_api_url);
        let explicit_key = config.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned);
        let mut keyless = false;
        let key: Option<String> = match (explicit_key, &config.api_key_env) {
            (Some(k), _) => Some(k),
            (None, Some(env)) => match env_non_empty(env) {
                Some(k) => Some(k),
                None if base_url.is_some() => {
                    tracing::warn!(
                        env = %env,
                        base_url = base_url.as_deref().unwrap_or_default(),
                        "no API key configured; sending requests to the custom endpoint without one"
                    );
                    keyless = true;
                    Some(NO_API_KEY_PLACEHOLDER.to_owned())
                }
                None => {
                    return Err(LlmError::Config(format!(
                        "environment variable {env} is not set (needed for {:?}); set it, pass an explicit key, or set a base URL for a keyless endpoint",
                        config.provider
                    )));
                }
            },
            // Auto provider without an explicit key: genai resolves the vendor default (e.g. ANTHROPIC_API_KEY).
            (None, None) => None,
        };
        let mut builder = Client::builder();
        match (base_url, key) {
            (Some(base_url), key) => {
                let forced_kind = config.provider.adapter_kind();
                let resolver = ServiceTargetResolver::from_resolver_fn(
                    move |target: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
                        let ServiceTarget { model, auth, .. } = target;
                        let kind = forced_kind.unwrap_or(model.adapter_kind);
                        Ok(ServiceTarget {
                            endpoint: Endpoint::from_owned(base_url.clone()),
                            auth: key.clone().map(AuthData::from_single).unwrap_or(auth),
                            model: ModelIden::new(kind, model.model_name),
                        })
                    },
                );
                builder = builder.with_service_target_resolver(resolver);
            }
            (None, Some(key)) => {
                builder = builder.with_auth_resolver_fn(move |_model: ModelIden| Ok(Some(AuthData::from_single(key.clone()))));
            }
            (None, None) => {}
        }
        let client = builder.build().map_err(|e| LlmError::Config(e.to_string()))?;
        let routed_model = config.routed_model_name();
        Ok(Self { client, config, routed_model, keyless, sends_prompt_cache_key })
    }

    /// The effective provider (after routing an unknown model to a custom endpoint).
    pub fn provider(&self) -> ModelProvider {
        self.config.provider
    }

    /// The effective base URL (normalised, trailing `/`), when a custom endpoint is used.
    pub fn base_url(&self) -> Option<&str> {
        self.config.base_url.as_deref()
    }

    /// The effective configuration.
    pub fn config(&self) -> &GenaiConfig {
        &self.config
    }

    /// True when requests go to a custom endpoint without any API key
    /// (nothing explicit, nothing in the key variable).
    pub fn is_keyless(&self) -> bool {
        self.keyless
    }

    fn chat_options(&self, request: &LlmRequest) -> ChatOptions {
        let mut options = ChatOptions::default()
            .with_capture_usage(true)
            .with_capture_content(true)
            .with_capture_tool_calls(true)
            .with_capture_reasoning_content(true);
        if let Some(t) = request.options.temperature.or(self.config.temperature).or(self.config.provider.default_temperature()) {
            options = options.with_temperature(self.config.provider.clamp_temperature(t));
        }
        if let Some(m) = request.options.max_tokens.or(self.config.max_tokens) {
            options = options.with_max_tokens(m);
        }
        if self.sends_prompt_cache_key {
            options.prompt_cache_key = request.options.prompt_cache_key.clone();
        }
        options
    }
}

fn to_chat_request(request: &LlmRequest) -> ChatRequest {
    let mut chat = ChatRequest::default();
    if !request.system.trim().is_empty() {
        chat = chat.with_system(request.system.clone());
    }
    for message in &request.messages {
        match message {
            LlmMessage::User { text, images } => {
                if images.is_empty() {
                    chat = chat.append_message(ChatMessage::user(text.clone()));
                } else {
                    let mut parts = vec![ContentPart::Text(text.clone())];
                    for image in images {
                        if let Some(part) = image_part(&image.url) {
                            parts.push(part);
                        }
                    }
                    chat = chat.append_message(ChatMessage::user(MessageContent::from_parts(parts)));
                }
            }
            LlmMessage::Assistant { text, tool_calls } => {
                let mut parts: Vec<ContentPart> = Vec::new();
                if let Some(text) = text.as_ref().filter(|t| !t.is_empty()) {
                    parts.push(ContentPart::Text(text.clone()));
                }
                for call in tool_calls {
                    parts.push(ContentPart::ToolCall(ToolCall {
                        call_id: call.id.clone(),
                        fn_name: call.name.clone(),
                        fn_arguments: call.arguments.clone(),
                        thought_signatures: None,
                    }));
                }
                if parts.is_empty() {
                    continue;
                }
                chat = chat.append_message(ChatMessage::assistant(MessageContent::from_parts(parts)));
            }
            LlmMessage::ToolResults(results) => {
                let responses: Vec<ToolResponse> =
                    results.iter().map(|r| ToolResponse::new(r.call_id.clone(), r.content.clone()).with_fn_name(r.name.clone())).collect();
                chat = chat.append_message(ChatMessage::tool(MessageContent::from_tool_responses(responses)));
            }
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }
    if !request.tools.is_empty() {
        chat = chat.with_tools(
            request
                .tools
                .iter()
                .map(|t| Tool::new(t.name.clone()).with_description(t.description.clone()).with_schema(t.parameters.clone()))
                .collect::<Vec<_>>(),
        );
    }
    chat
}

fn image_part(url: &str) -> Option<ContentPart> {
    if let Some(path) = url.strip_prefix("file://") {
        return match Binary::from_file(path) {
            Ok(binary) => Some(ContentPart::Binary(binary)),
            Err(e) => {
                tracing::warn!(path, error = %e, "image dropped: cannot read file");
                None
            }
        };
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(ContentPart::Binary(Binary::from_url(image_content_type_from_url(url), url, None)));
    }
    if url.starts_with("data:") {
        return match parse_data_url(url) {
            Some((content_type, payload)) => Some(ContentPart::Binary(Binary::from_base64(content_type, payload, None))),
            None => {
                tracing::warn!(
                    url = gns_core::text::clamp_line(url, 60),
                    "image dropped: unsupported data URL (expected data:<mime>;base64,<payload>)"
                );
                None
            }
        };
    }
    tracing::warn!(url = gns_core::text::clamp_line(url, 120), "image dropped: unsupported URL scheme (use file://, http(s):// or data:)");
    None
}

/// MIME type for an image URL from its extension (query string ignored);
/// `image/jpeg` when the extension is unknown.
fn image_content_type_from_url(url: &str) -> &'static str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let ext = path.rsplit('/').next().and_then(|name| name.rsplit_once('.')).map(|(_, ext)| ext.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        _ => "image/jpeg",
    }
}

/// Split `data:<mime>;base64,<payload>` into (mime, payload).
fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (header, payload) = rest.split_once(',')?;
    let mut params = header.split(';');
    let mime = params.next().map(str::trim).filter(|m| !m.is_empty())?.to_owned();
    if !params.any(|p| p.trim().eq_ignore_ascii_case("base64")) {
        return None;
    }
    let payload: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
    if payload.is_empty() {
        return None;
    }
    Some((mime, payload))
}

fn usage_from(usage: &genai::chat::Usage) -> UsageTotals {
    UsageTotals {
        prompt_tokens: usage.prompt_tokens.unwrap_or(0).max(0) as u64,
        completion_tokens: usage.completion_tokens.unwrap_or(0).max(0) as u64,
        cached_prompt_tokens: usage.prompt_tokens_details.as_ref().and_then(|d| d.cached_tokens).unwrap_or(0).max(0) as u64,
        total_tokens: usage.total_tokens.unwrap_or(0).max(0) as u64,
        llm_calls: 1,
    }
}

fn tool_calls_from(content: &MessageContent) -> Vec<LlmToolCall> {
    content
        .tool_calls()
        .into_iter()
        .map(|c| LlmToolCall { id: c.call_id.clone(), name: c.fn_name.clone(), arguments: c.fn_arguments.clone() })
        .collect()
}

fn map_error(error: genai::Error) -> LlmError {
    // A streamed call reports an HTTP failure as `WebStream` wrapping an
    // `HttpError`, which `Error::status` does not look into.
    let status = error
        .status()
        .or_else(|| match &error {
            genai::Error::WebStream { error, .. } => error.downcast_ref::<genai::Error>().and_then(genai::Error::status),
            _ => None,
        })
        .map(|s| s.as_u16());
    // Transport failures (connection refused, DNS, timeouts, dropped streams)
    // carry no status and are worth one more attempt.
    let transport = status.is_none()
        && matches!(
            &error,
            genai::Error::WebStream { .. }
                | genai::Error::WebModelCall { webc_error: genai::webc::Error::Reqwest(_), .. }
                | genai::Error::WebAdapterCall { webc_error: genai::webc::Error::Reqwest(_), .. }
        );
    let retryable = matches!(status, Some(408 | 409 | 429 | 500..=599)) || transport;
    LlmError::Provider { status, message: error.to_string(), retryable }
}

#[async_trait]
impl LlmProvider for GenaiProvider {
    async fn complete(&self, request: LlmRequest, cancel: CancellationToken, on_delta: DeltaSink<'_>) -> Result<LlmResponse, LlmError> {
        let options = self.chat_options(&request);
        let chat = to_chat_request(&request);
        let model = self.routed_model.as_str();
        if !self.config.stream {
            let response = tokio::select! {
                _ = cancel.cancelled() => return Err(LlmError::Cancelled),
                r = self.client.exec_chat(model, chat, Some(&options)) => r.map_err(map_error)?,
            };
            let text = response.first_text().map(str::to_owned).filter(|t| !t.is_empty());
            if let Some(t) = &text {
                on_delta(LlmDelta::Text(t.clone()));
            }
            return Ok(LlmResponse {
                text,
                reasoning: response.reasoning_content.clone(),
                tool_calls: tool_calls_from(&response.content),
                usage: usage_from(&response.usage),
                stop_reason: response.stop_reason.as_ref().map(|s| s.raw().to_owned()),
            });
        }

        let stream_response = tokio::select! {
            _ = cancel.cancelled() => return Err(LlmError::Cancelled),
            r = self.client.exec_chat_stream(model, chat, Some(&options)) => r.map_err(map_error)?,
        };
        let mut stream = stream_response.stream;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut result = LlmResponse::default();
        let mut ended = false;
        loop {
            let event = tokio::select! {
                _ = cancel.cancelled() => return Err(LlmError::Cancelled),
                e = stream.next() => e,
            };
            let Some(event) = event else { break };
            match event.map_err(map_error)? {
                ChatStreamEvent::Chunk(chunk) => {
                    text.push_str(&chunk.content);
                    on_delta(LlmDelta::Text(chunk.content));
                }
                ChatStreamEvent::ReasoningChunk(chunk) => {
                    reasoning.push_str(&chunk.content);
                    on_delta(LlmDelta::Reasoning(chunk.content));
                }
                ChatStreamEvent::End(end) => {
                    ended = true;
                    if let Some(usage) = &end.captured_usage {
                        result.usage = usage_from(usage);
                    }
                    if let Some(content) = &end.captured_content {
                        result.tool_calls = tool_calls_from(content);
                        if text.is_empty()
                            && let Some(joined) = content.joined_texts()
                        {
                            text = joined;
                        }
                    }
                    result.stop_reason = end.captured_stop_reason.as_ref().map(|s| s.raw().to_owned());
                    if reasoning.is_empty()
                        && let Some(r) = &end.captured_reasoning_content
                    {
                        reasoning = r.clone();
                    }
                }
                ChatStreamEvent::Start
                | ChatStreamEvent::ToolCallChunk(_)
                | ChatStreamEvent::ThoughtSignatureChunk(_)
                | ChatStreamEvent::Heartbeat => {}
                #[allow(unreachable_patterns)]
                _ => {}
            }
        }
        if !ended {
            // Tool calls, usage and the stop reason only arrive with the final
            // event; without it the reply is incomplete and must not be
            // mistaken for a text-only turn.
            return Err(LlmError::Provider {
                status: None,
                message: "stream closed before the final event (no [DONE]); the response is incomplete".to_owned(),
                retryable: true,
            });
        }
        result.usage.llm_calls = 1;
        result.text = Some(text).filter(|t| !t.is_empty());
        result.reasoning = Some(reasoning).filter(|r| !r.is_empty());
        Ok(result)
    }

    fn model_name(&self) -> String {
        self.config.model.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_content_type_follows_extension() {
        assert_eq!(image_content_type_from_url("https://x/a.PNG"), "image/png");
        assert_eq!(image_content_type_from_url("https://x/a.jpeg?size=large"), "image/jpeg");
        assert_eq!(image_content_type_from_url("https://x/a.gif#frag"), "image/gif");
        assert_eq!(image_content_type_from_url("https://x/a.webp"), "image/webp");
        assert_eq!(image_content_type_from_url("https://x/a.bmp"), "image/bmp");
        assert_eq!(image_content_type_from_url("https://x/photo"), "image/jpeg");
        assert_eq!(image_content_type_from_url("https://x.y/dir.v2/photo"), "image/jpeg");
    }

    #[test]
    fn data_url_parsing() {
        assert_eq!(parse_data_url("data:image/png;base64,AAAA").unwrap(), ("image/png".to_owned(), "AAAA".to_owned()));
        assert_eq!(parse_data_url("data:image/png;name=a;BASE64,AA\nAA").unwrap().1, "AAAA");
        assert!(parse_data_url("data:image/png,percent-encoded").is_none(), "non-base64 payloads are unsupported");
        assert!(parse_data_url("data:;base64,AAAA").is_none(), "mime is required");
        assert!(parse_data_url("data:image/png;base64,").is_none(), "empty payload");
        assert!(image_part("data:image/png;base64,AAAA").is_some());
        assert!(image_part("ftp://x/a.png").is_none());
    }

    #[test]
    fn provider_inference() {
        assert_eq!(ModelProvider::infer("gpt-4o-mini"), ModelProvider::OpenAi);
        assert_eq!(ModelProvider::infer("o3-mini"), ModelProvider::OpenAi);
        assert_eq!(ModelProvider::infer("kimi-k2-turbo-preview"), ModelProvider::Kimi);
        assert_eq!(ModelProvider::infer("Kimi-K2-Thinking"), ModelProvider::Kimi);
        assert_eq!(ModelProvider::infer("moonshot-v1-8k"), ModelProvider::Moonshot);
        assert_eq!(ModelProvider::infer("moonshot::kimi-k2-0905-preview"), ModelProvider::Moonshot);
        assert_eq!(ModelProvider::infer("claude-sonnet-4-5"), ModelProvider::Auto);
        assert_eq!(ModelProvider::infer("qwen-plus"), ModelProvider::Auto);
    }

    #[test]
    fn genai_routability() {
        assert!(ModelProvider::is_known_to_genai("claude-sonnet-4-5"));
        assert!(ModelProvider::is_known_to_genai("gemini-2.5-flash"));
        assert!(ModelProvider::is_known_to_genai("deepseek-chat"));
        assert!(ModelProvider::is_known_to_genai("ollama::llama3.2"));
        assert!(!ModelProvider::is_known_to_genai("qwen-plus"));
        assert!(!ModelProvider::is_known_to_genai("Qwen/Qwen2.5-72B-Instruct"));
        assert!(!ModelProvider::is_known_to_genai("llama-3.3-70b-instruct"));
        assert!(!ModelProvider::is_known_to_genai("my-finetune"));
    }

    #[test]
    fn kimi_defaults_and_routing() {
        let config = GenaiConfig::kimi("kimi-k2-turbo-preview");
        assert_eq!(config.api_key_env.as_deref(), Some("KIMI_API_KEY"));
        assert_eq!(config.routed_model_name(), "kimi::kimi-k2-turbo-preview");
        let cn = GenaiConfig::moonshot("kimi-k2-0905-preview");
        assert_eq!(cn.api_key_env.as_deref(), Some("MOONSHOT_API_KEY"));
        assert_eq!(cn.routed_model_name(), "moonshot::kimi-k2-0905-preview");
        assert_eq!(GenaiConfig::new("gpt-4o-mini").routed_model_name(), "openai::gpt-4o-mini");
        assert_eq!(GenaiConfig::new("kimi::kimi-latest").routed_model_name(), "kimi::kimi-latest");
    }

    #[test]
    fn kimi_temperature_is_forced_to_one() {
        assert_eq!(ModelProvider::Kimi.clamp_temperature(1.7), 1.0);
        assert_eq!(ModelProvider::Kimi.clamp_temperature(0.2), 1.0);
        assert_eq!(ModelProvider::Moonshot.clamp_temperature(0.6), 1.0);
        assert_eq!(ModelProvider::OpenAi.clamp_temperature(1.7), 1.7);
        assert_eq!(ModelProvider::Kimi.default_temperature(), Some(1.0));
        assert_eq!(ModelProvider::OpenAi.default_temperature(), None);
    }

    #[test]
    fn provider_builds_with_explicit_key() {
        let provider = GenaiProvider::new(GenaiConfig::kimi("kimi-k2-turbo-preview").with_api_key("sk-test")).unwrap();
        assert_eq!(provider.provider(), ModelProvider::Kimi);
        assert_eq!(provider.model_name(), "kimi-k2-turbo-preview");
        let via_gateway =
            GenaiProvider::new(GenaiConfig::kimi("kimi-k2-thinking").with_api_key("k").with_base_url("https://gateway.example/v1"))
                .unwrap();
        assert_eq!(via_gateway.base_url(), Some("https://gateway.example/v1/"));
    }

    #[test]
    fn unknown_model_with_base_url_uses_openai_protocol() {
        let config = GenaiConfig::with_provider("qwen-plus", ModelProvider::Auto).with_base_url("https://gw.example/v1");
        assert_eq!(config.provider, ModelProvider::OpenAi);
        assert_eq!(config.api_key_env.as_deref(), Some(OPENAI_API_KEY_ENV));
        assert_eq!(config.routed_model_name(), "openai::qwen-plus");

        // Fields set directly (as the CLI does) are resolved when the provider is built.
        let mut direct = GenaiConfig::with_provider("Qwen/Qwen2.5-72B-Instruct", ModelProvider::Auto);
        direct.base_url = Some("http://localhost:8000/v1".into());
        direct.api_key = Some("sk-local".into());
        let provider = GenaiProvider::new(direct).unwrap();
        assert_eq!(provider.provider(), ModelProvider::OpenAi);
        assert_eq!(provider.base_url(), Some("http://localhost:8000/v1/"));
        assert_eq!(provider.routed_model, "openai::Qwen/Qwen2.5-72B-Instruct");
    }

    #[test]
    fn known_model_with_base_url_keeps_its_vendor() {
        let config = GenaiConfig::with_provider("claude-sonnet-4-5", ModelProvider::Auto).with_base_url("https://anthropic-proxy.example/");
        assert_eq!(config.provider, ModelProvider::Auto);
        assert_eq!(config.api_key_env, None);
        assert_eq!(config.routed_model_name(), "claude-sonnet-4-5");
        assert!(GenaiProvider::new(config).is_ok());

        let ollama = GenaiConfig::with_provider("ollama::llama3.2", ModelProvider::Auto).with_base_url("http://localhost:11434/");
        assert_eq!(ollama.provider, ModelProvider::Auto);
    }

    #[test]
    fn openai_compatible_endpoint() {
        let config = GenaiConfig::openai_compatible("http://localhost:8000/v1", "my-finetune").with_api_key("sk-local");
        assert_eq!(config.provider, ModelProvider::OpenAi);
        assert_eq!(config.routed_model_name(), "openai::my-finetune");
        let provider = GenaiProvider::new(config).unwrap();
        assert_eq!(provider.base_url(), Some("http://localhost:8000/v1/"));
        assert_eq!(provider.model_name(), "my-finetune");

        // A custom endpoint builds without a key (keyless local servers)…
        let keyless = GenaiConfig::openai_compatible("http://localhost:1234/v1", "local").with_api_key_env("GNS_LLM_TEST_UNSET_KEY");
        assert!(GenaiProvider::new(keyless).unwrap().is_keyless());
        assert!(!provider.is_keyless());
        // …the official endpoint does not.
        let mut official = GenaiConfig::openai("gpt-4o-mini").with_api_key_env("GNS_LLM_TEST_UNSET_KEY");
        official.base_url = None;
        assert!(matches!(GenaiProvider::new(official), Err(LlmError::Config(_))));
    }

    #[test]
    fn base_url_normalisation() {
        assert_eq!(normalize_base_url(" https://gw.example/v1 ").unwrap(), "https://gw.example/v1/");
        assert_eq!(normalize_base_url("http://localhost:8000/v1/").unwrap(), "http://localhost:8000/v1/");
        assert!(matches!(normalize_base_url("gw.example/v1"), Err(LlmError::Config(_))));
        let bad = GenaiConfig::openai_compatible("gw.example/v1", "m").with_api_key("k");
        assert!(matches!(GenaiProvider::new(bad), Err(LlmError::Config(_))));
    }

    #[test]
    fn prompt_cache_key_only_goes_to_openai_itself() {
        let request = LlmRequest {
            system: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            options: gns_core::LlmOptions { prompt_cache_key: Some("agent-1".into()), ..Default::default() },
        };
        let sent = |config: GenaiConfig| GenaiProvider::new(config.with_api_key("k")).unwrap().chat_options(&request).prompt_cache_key;
        let mut official = GenaiConfig::openai("gpt-4o-mini");
        official.base_url = None;
        assert_eq!(sent(official).as_deref(), Some("agent-1"));
        assert_eq!(sent(GenaiConfig::openai_compatible("https://api.openai.com/v1", "gpt-4o-mini")).as_deref(), Some("agent-1"));
        assert_eq!(sent(GenaiConfig::openai_compatible("https://gw.example/v1", "z-ai/glm-5.3-flash")), None);
        assert_eq!(sent(GenaiConfig::openai_compatible("https://api.openai.com.example/v1", "gpt-4o-mini")), None);
        assert_eq!(sent(GenaiConfig::kimi("kimi-k2-turbo-preview")), None);
    }

    #[test]
    fn debug_redacts_the_key() {
        let config = GenaiConfig::openai("gpt-4o-mini").with_api_key("sk-secret");
        let dbg = format!("{config:?}");
        assert!(dbg.contains("REDACTED"), "{dbg}");
        assert!(!dbg.contains("sk-secret"), "{dbg}");
    }
}
