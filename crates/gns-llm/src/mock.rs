//! Scripted provider for tests.

use async_trait::async_trait;
use gns_core::{DeltaSink, LlmDelta, LlmError, LlmProvider, LlmRequest, LlmResponse};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

type Responder = dyn Fn(&LlmRequest) -> LlmResponse + Send + Sync;

/// A provider that replays scripted responses (FIFO) or computes them with a
/// closure, recording every request it receives.
#[derive(Clone)]
pub struct MockLlm {
    scripted: Arc<Mutex<VecDeque<LlmResponse>>>,
    responder: Option<Arc<Responder>>,
    requests: Arc<Mutex<Vec<LlmRequest>>>,
    /// Artificial latency per call (lets tests exercise cancellation).
    pub delay: Option<std::time::Duration>,
    /// Response when the script runs dry.
    pub fallback: LlmResponse,
}

impl std::fmt::Debug for MockLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockLlm").field("pending", &self.scripted.lock().map(|s| s.len()).unwrap_or(0)).finish()
    }
}

impl Default for MockLlm {
    fn default() -> Self {
        Self::new()
    }
}

impl MockLlm {
    /// Empty script; falls back to an empty text response.
    pub fn new() -> Self {
        Self {
            scripted: Arc::new(Mutex::new(VecDeque::new())),
            responder: None,
            requests: Arc::new(Mutex::new(Vec::new())),
            delay: None,
            fallback: LlmResponse::text("(mock: script exhausted)"),
        }
    }
    /// Provider that answers with `responses` in order.
    pub fn scripted(responses: Vec<LlmResponse>) -> Self {
        let mock = Self::new();
        mock.push_all(responses);
        mock
    }
    /// Provider that computes every answer with `f` (scripted answers, if any, win first).
    pub fn with_responder(mut self, f: impl Fn(&LlmRequest) -> LlmResponse + Send + Sync + 'static) -> Self {
        self.responder = Some(Arc::new(f));
        self
    }
    /// Append responses to the script.
    pub fn push_all(&self, responses: Vec<LlmResponse>) {
        if let Ok(mut s) = self.scripted.lock() {
            s.extend(responses);
        }
    }
    /// Append one response.
    pub fn push(&self, response: LlmResponse) {
        self.push_all(vec![response]);
    }
    /// Every request received so far.
    pub fn requests(&self) -> Vec<LlmRequest> {
        self.requests.lock().map(|r| r.clone()).unwrap_or_default()
    }
    /// Number of requests received.
    pub fn call_count(&self) -> usize {
        self.requests.lock().map(|r| r.len()).unwrap_or(0)
    }
    /// Remaining scripted responses.
    pub fn remaining(&self) -> usize {
        self.scripted.lock().map(|s| s.len()).unwrap_or(0)
    }
}

#[async_trait]
impl LlmProvider for MockLlm {
    async fn complete(&self, request: LlmRequest, cancel: CancellationToken, on_delta: DeltaSink<'_>) -> Result<LlmResponse, LlmError> {
        if let Ok(mut r) = self.requests.lock() {
            r.push(request.clone());
        }
        if let Some(delay) = self.delay {
            tokio::select! {
                _ = cancel.cancelled() => return Err(LlmError::Cancelled),
                _ = tokio::time::sleep(delay) => {}
            }
        }
        if cancel.is_cancelled() {
            return Err(LlmError::Cancelled);
        }
        let next = self.scripted.lock().ok().and_then(|mut s| s.pop_front());
        let mut response = match next {
            Some(r) => r,
            None => match &self.responder {
                Some(f) => f(&request),
                None => self.fallback.clone(),
            },
        };
        if let Some(text) = &response.text {
            on_delta(LlmDelta::Text(text.clone()));
        }
        response.usage.llm_calls = 1;
        Ok(response)
    }

    fn model_name(&self) -> String {
        "mock".to_owned()
    }
}
