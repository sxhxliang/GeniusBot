//! `gns-llm` — [`LlmProvider`] implementations.
//!
//! * [`GenaiProvider`]: OpenAI-family chat completions through the `genai`
//!   crate (streaming by default, tool calling, usage capture, cancellation).
//! * [`MockLlm`]: a scripted provider for tests and offline demos.

mod genai_provider;
mod mock;

pub use genai_provider::*;
pub use mock::*;

pub use gns_core::LlmProvider;
