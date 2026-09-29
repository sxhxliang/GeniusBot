//! Host configuration.

use std::path::PathBuf;
use std::time::Duration;

/// Configuration for [`crate::AgentHost`].
#[derive(Clone, Debug)]
pub struct AgentHostConfig {
    /// Maximum bytes per published file (default 100 MiB).
    pub artifact_max_bytes: u64,
    /// Maximum files/references per send or task submission.
    pub artifact_max_files: usize,
    /// Root directory holding `agents/` and `memory/`.
    pub root_dir: PathBuf,
    /// Product name used in the base system prompt.
    pub product_name: String,
    /// The user's display name (injected into prompts).
    pub user_name: Option<String>,
    /// IANA time zone (e.g. `Asia/Shanghai`) for routines and prompts.
    pub time_zone: Option<String>,
    /// Maximum LLM steps per turn.
    pub max_steps: usize,
    /// Character budget for the transcript window sent to the model.
    pub transcript_char_budget: usize,
    /// Broadcast channel capacity for events.
    pub event_capacity: usize,
    /// How often the routine scheduler checks for due routines.
    pub scheduler_tick: Duration,
    /// Retries for retryable LLM errors.
    pub llm_retries: u32,
    /// Run the hidden kickstart turn for agents created with `kickstart: true`.
    pub kickstart_new_agents: bool,
    /// Summarise conversations into episode memories every N memorable turns.
    pub episode_interval: usize,
    /// Extract durable facts from every memorable user exchange (LLM call in the background).
    pub memory_extraction: bool,
    /// Consolidate memory ("dream") once an agent has been idle this long and
    /// has enough new log facts. `None` disables it.
    pub dream_after_idle: Option<Duration>,
    /// Require SendMessage via native tool choice on a user-opened turn
    /// until the agent has delivered its initial reply.
    pub enforce_start_of_turn_ack: bool,
    /// Step budget for subagents.
    pub subagent_max_steps: usize,
    /// Context window (tokens) used for the summarization thresholds.
    pub context_window_tokens: u64,
    /// Character cap on the summarization request.
    pub summarization_max_prompt_chars: usize,
    /// `GNS_DISABLE_MEMORY_FREEZE`: re-render the memory block every turn.
    pub disable_memory_freeze: bool,
}

impl AgentHostConfig {
    /// Defaults rooted at `root_dir`.
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        Self {
            root_dir: root_dir.into(),
            artifact_max_bytes: 100 * 1024 * 1024,
            artifact_max_files: 10,
            product_name: gns_core::prompt::DEFAULT_PRODUCT_NAME.to_owned(),
            user_name: None,
            time_zone: None,
            max_steps: gns_core::AGENT_MAX_STEPS,
            transcript_char_budget: gns_core::DEFAULT_TRANSCRIPT_CHAR_BUDGET,
            event_capacity: 1024,
            scheduler_tick: Duration::from_secs(15),
            llm_retries: 2,
            kickstart_new_agents: true,
            episode_interval: gns_core::DEFAULT_EPISODE_INTERVAL,
            memory_extraction: true,
            dream_after_idle: Some(Duration::from_secs(30 * 60)),
            enforce_start_of_turn_ack: true,
            subagent_max_steps: gns_core::SUBAGENT_MAX_STEPS,
            context_window_tokens: 200_000,
            summarization_max_prompt_chars: gns_core::SUMMARIZATION_MAX_PROMPT_CHARS,
            disable_memory_freeze: std::env::var("GNS_DISABLE_MEMORY_FREEZE").is_ok_and(|v| v == "1"),
        }
    }
}
