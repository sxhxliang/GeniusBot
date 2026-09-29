//! Numeric limits and marker strings shared across crates. Values mirror the
//! original Grok Bot host so behaviour stays comparable.

/// Maximum LLM steps in one turn before the runner gives up. Mirrors the
/// original host's `SAND_AGENT_MAX_STEPS`; it is a safety net, not a budget.
pub const AGENT_MAX_STEPS: usize = 5_000;
/// Cross-agent message text is clamped to this many characters.
pub const AGENT_MESSAGE_MAX_TEXT_LENGTH: usize = 8_000;
/// At most this many teammates are listed in the system prompt directory.
pub const AGENT_DIRECTORY_PROMPT_LIMIT: usize = 40;

/// Cue that opens an inbound cross-agent wake prompt.
pub const AGENT_INBOUND_WAKE_CUE: &str = "[agent]";
/// Cue that opens an owner broadcast wake prompt.
pub const ADMIN_BROADCAST_WAKE_CUE: &str = "[broadcast]";
/// Cue that opens a routine (automation) wake prompt.
pub const AUTOMATION_WAKE_CUE: &str = "[routine]";
/// Cue that opens the first-run kickstart prompt.
pub const KICKSTART_WAKE_CUE: &str = "[first run]";
/// Cue that opens a background-job result wake.
pub const BACKGROUND_WAKE_CUE: &str = "[background]";
/// Default step budget for a subagent.
pub const SUBAGENT_MAX_STEPS: usize = 20;
/// In-turn "work out loud" pressure: after more than this many tool calls
/// without a SendMessage the model is reminded to post an update.
pub const SEND_MESSAGE_REMINDER_THRESHOLD: usize = 6;
/// A user-opened turn that runs more than this many tool calls before its
/// first text SendMessage is reminded to acknowledge the user.
pub const START_OF_TURN_ACK_THRESHOLD: usize = 1;
/// A group member preempted by a DM is retried this many times within the same room turn.
pub const GROUP_MEMBER_MAX_ATTEMPTS: usize = 3;
/// After a turn that owed the user a reply ends without one, the runner
/// re-runs the agent with a hidden nudge at most this many times.
pub const MAX_REPLY_NUDGES: usize = 3;
/// MCP: connect + handshake budget, per-call budget, and how long a failed
/// server is left alone before the next connection attempt.
pub const MCP_CONNECT_TIMEOUT_SECS: u64 = 20;
pub const MCP_CALL_TIMEOUT_SECS: u64 = 120;
pub const MCP_RETRY_AFTER_MS: i64 = 60_000;
/// A visible turn ends after this many consecutive LLM steps whose only tool
/// calls were SendMessage. Guards against models that keep re-sending their
/// reply after each "Delivered." instead of ending the turn.
pub const MAX_CONSECUTIVE_REPLY_ONLY_STEPS: usize = 4;
/// Sentinel the memory extractor answers when nothing is worth keeping.
pub const MEMORY_EXTRACTION_NONE_SENTINEL: &str = "NONE";
/// A dream (memory consolidation) needs at least this many new log facts.
pub const MEMORY_DREAM_MIN_NEW_FACTS: usize = 3;

/// Tool names (stable identifiers used in prompts and transcripts).
pub const SEND_MESSAGE_TOOL_NAME: &str = "SendMessage";
pub const SEND_TO_AGENT_TOOL_NAME: &str = "SendToAgent";
pub const CREATE_AGENT_TOOL_NAME: &str = "CreateAgent";
pub const UPDATE_AGENT_TOOL_NAME: &str = "UpdateAgent";
pub const LIST_AGENTS_TOOL_NAME: &str = "ListAgents";
pub const LIST_GROUPS_TOOL_NAME: &str = "ListGroups";
pub const UPDATE_STATE_TOOL_NAME: &str = "update_state";
pub const RUN_SUBAGENT_TOOL_NAME: &str = "RunSubagent";
pub const SHELL_TOOL_NAME: &str = "Shell";
pub const READ_TOOL_NAME: &str = "Read";
pub const WRITE_TOOL_NAME: &str = "Write";
pub const EDIT_TOOL_NAME: &str = "Edit";

/// Group chat limits.
pub const GROUP_MAX_ROUNDS: usize = 3;
pub const GROUP_MAX_MEMBER_TURNS: usize = 10;
pub const GROUP_MAX_MESSAGES_PER_TURN: usize = 2;
pub const GROUP_PROMPT_HISTORY_LIMIT: usize = 24;
pub const GROUP_CHAT_TAG_PREFIX: &str = "[Group chat: ";

/// Memory limits.
pub const MEMORY_RECENT_PROMPT_LIMIT: usize = 30;
pub const MEMORY_RECENT_PROMPT_CHAR_BUDGET: usize = 4_000;
pub const MEMORY_PROFILE_PROMPT_LIMIT: usize = 100;
pub const MEMORY_MAX_CONTENT_LENGTH: usize = 500;
pub const MEMORY_EPISODE_PREFIX: &str = "[episode] ";
pub const MEMORY_NOTE_PREFIX: &str = "[note] ";
pub const DEFAULT_EPISODE_INTERVAL: usize = 6;
pub const MEMORY_USER_PROFILE_PROMPT_LIMIT: usize = 50;
pub const MEMORY_USER_RECENT_PROMPT_LIMIT: usize = 15;
pub const MEMORY_PROJECT_PROFILE_PROMPT_LIMIT: usize = 25;
pub const MEMORY_PROJECT_RECENT_PROMPT_LIMIT: usize = 10;
pub const MEMORY_PROJECT_INJECTED_CAP: usize = 3;
pub const MEMORY_DECAY_HALF_LIFE_DAYS: f64 = 30.0;
pub const MEMORY_USER_PROFILE_CHAR_BUDGET: usize = 4_000;
pub const MEMORY_USER_RECENT_CHAR_BUDGET: usize = 2_000;
pub const MEMORY_PROJECT_PROFILE_CHAR_BUDGET: usize = 2_500;
pub const MEMORY_PROJECT_RECENT_CHAR_BUDGET: usize = 1_500;
pub const DAY_MS: f64 = 86_400_000.0;

/// Routine (automation) limits.
pub const AUTOMATION_MAX_NAME_LENGTH: usize = 80;
pub const AUTOMATION_MAX_PER_AGENT: usize = 50;
pub const AUTOMATION_MAX_RUN_HISTORY: usize = 20;
pub const AUTOMATION_MAX_RUN_DETAIL_LENGTH: usize = 300;

/// Pending tool approvals expire after this many seconds.
pub const APPROVAL_TTL_SECS: u64 = 600;

/// Default character budget for the transcript window sent to the model.
pub const DEFAULT_TRANSCRIPT_CHAR_BUDGET: usize = 120_000;

/// Well-known KV keys inside an agent's `store.db`.
pub mod kv_keys {
    pub const SAND_PROFILE: &str = "sandProfile";
    pub const AWAITING_USER_RESPONSE: &str = "awaitingUserResponse";
    pub const EPISODE_PENDING: &str = "episodePending";
    pub const MEMORY_PROMPT_SNAPSHOT: &str = "memoryPromptSnapshot";
    pub const AGENT_PROFILE_PROMPT_SNAPSHOT: &str = "agentProfilePromptSnapshot";
    pub const INTRODUCTION_PENDING: &str = "introductionPending";
    pub const CONVERSATION_PARTNERS: &str = "conversationPartners";
    pub const COMPACTION_EPOCH: &str = "compactionEpoch";
    pub const USAGE_TOTALS: &str = "usageTotals";
    pub const LAST_TURN_AT: &str = "lastTurnAt";
    pub const LAST_DREAM_AT: &str = "lastDreamAt";
    pub const DREAM_LOG_COUNT: &str = "dreamLogCount";
    /// Inbound agent messages not yet delivered as wakes (survives restarts).
    pub const PENDING_INBOUND: &str = "pendingInbound";
    /// Prompt tokens of the last model request (summarization trigger).
    pub const LAST_PROMPT_TOKENS: &str = "lastPromptTokens";
    /// `origin`: "user" or "dev".
    pub const AGENT_ORIGIN: &str = "origin";
}

/// Routine (automation) limits and markers ported from the original host
/// (`automations/automation.ts`, `shared/automations.ts`,
/// `automations/automation-trigger.ts`, `transcript/automation-event-fires.ts`).
pub const AUTOMATION_UI_LIMIT: usize = 100;
pub const MAX_EVENTS_IN_AUTOMATION_WAKE: usize = 25;
pub const AUTOMATION_STATUS_PROMPT_MARKER: &str = "<automation_status>";
pub const AUTOMATION_PROMPT_GUIDANCE_VERSION: &str = "backend_triggers_v4";
/// Folder slugs derived from a routine name are cut to this many characters.
pub const AUTOMATION_SLUG_MAX_LENGTH: usize = 48;
/// A stored cron schedule string is cut to this many characters.
pub const AUTOMATION_SCHEDULE_MAX_LENGTH: usize = 120;
/// Event fires for one routine are coalesced for this long before waking it.
pub const EVENT_FIRE_DEBOUNCE_MS: u64 = 750;
pub const MAX_QUEUED_EVENT_FIRES_PER_AUTOMATION: usize = 500;
/// The next-run search walks at most this many minutes (366 days).
pub const MAX_CRON_SEARCH_MINUTES: i64 = 366 * 24 * 60;
/// Trigger model limits.
pub const TRIGGER_ANY_SCOPE: &str = "*";
pub const TRIGGER_MAX_GROUP_LISTENERS: usize = 8;
pub const TRIGGER_MAX_REACTION_EMOJI: usize = 8;
pub const TRIGGER_MAX_CHANNEL_LENGTH: usize = 80;
pub const TRIGGER_MAX_KEYWORD_LENGTH: usize = 120;
pub const TRIGGER_MAX_REPO_LENGTH: usize = 140;
pub const TRIGGER_MAX_BRANCH_LENGTH: usize = 200;
pub const TRIGGER_MAX_ALLOWLIST_LOGINS: usize = 50;
pub const TRIGGER_MAX_ALLOWLIST_LOGIN_LENGTH: usize = 80;
pub const TRIGGER_MAX_FILTER_IDS: usize = 50;
pub const TRIGGER_MAX_ID_LENGTH: usize = 200;
/// Routine notice raised once for github listeners created before this instant (2026-07-30 UTC).
pub const GITHUB_LISTENER_SCOPE_NOTICE: &str = "github-listener-scope";
pub const GITHUB_LISTENER_SCOPE_CREATED_BEFORE_MS: i64 = 1_785_369_600_000;

/// Memory prompt markers and extraction limits (mirror `sand-memory.ts`).
pub const MEMORY_EXTRACTION_PROMPT_MARKER: &str = "<<SAND_MEMORY_EXTRACTION>>";
pub const MEMORY_EPISODE_PROMPT_MARKER: &str = "<<SAND_MEMORY_EPISODE>>";
/// How many archived (not in prompt) facts the extractor scans for relevance.
pub const MEMORY_EXTRACTION_ARCHIVE_SCAN_LIMIT: usize = 500;
/// How many relevant archive facts join the extractor's "existing memory" list.
pub const MEMORY_EXTRACTION_RELEVANT_ARCHIVE_LIMIT: usize = 10;
/// Facts listed in the memory UI.
pub const MEMORY_UI_LIMIT: usize = 1_000;

/// Episode buffer limits (`agent-db.ts`): each side of a pending turn is
/// clamped to this many characters, and at most this many turns are kept.
pub const EPISODE_TURN_MAX_CHARS: usize = 2_000;
pub const EPISODE_PENDING_MAX_TURNS: usize = 64;
/// Group chats hold at most this many members (`GROUP_MAX_MEMBERS`).
pub const GROUP_MAX_MEMBERS: usize = 6;
/// Default name of a freshly minted agent (`SAND_DEFAULT_AGENT_NAME`).
pub const SAND_DEFAULT_AGENT_NAME: &str = "New Bot";
pub const LEGACY_SAND_DEFAULT_AGENT_NAME: &str = "New Agent";
/// Ack obligations: redrive an unacknowledged user message this long after
/// the agent goes idle, at most this many times.
pub const ACK_REDRIVE_IDLE_DELAY_MS: u64 = 5_000;
pub const MAX_ACK_REDRIVES: usize = 3;
/// Summarisation thresholds (`turn-agent-composition.ts`): start a
/// background summary when the unused context is at or below either bound.
pub const SUMMARIZATION_TRIGGER_TOKENS: u64 = 10_000;
pub const SUMMARIZATION_TRIGGER_FRACTION: f64 = 0.10;
/// Block on a summary when usage exceeds the window by more than
/// `min(25% of the window, 50_000)` tokens.
pub const SUMMARIZATION_BLOCK_OVER_FRACTION: f64 = 0.25;
pub const SUMMARIZATION_BLOCK_OVER_TOKENS: u64 = 50_000;
/// `SAND_SUMMARIZATION_MAX_PROMPT_CHARS`.
pub const SUMMARIZATION_MAX_PROMPT_CHARS: usize = 2_800_000;

/// Tool names added for parity with the original tool set.
pub const REACT_TO_MESSAGE_TOOL_NAME: &str = "ReactToMessage";
pub const AWAIT_SHELL_TOOL_NAME: &str = "AwaitShell";

/// Shell: inline output kept in the tool result (front-and-back truncation).
pub const SHELL_CHAR_HARD_LIMIT: usize = 20_000;
/// Shell / AwaitShell: default `block_until_ms`.
pub const SHELL_DEFAULT_BLOCK_UNTIL_MS: u64 = 30_000;
/// AwaitShell: polling slice while blocking on a terminal file.
pub const SHELL_CHECK_SLICE_MS: u64 = 250;
/// Shell: how often a backgrounded command's `running_for_ms` header is refreshed.
pub const SHELL_HEADER_REFRESH_MS: u64 = 5_000;
/// Directory (under the agent workspace) holding `<shellId>.txt` terminal files.
pub const TERMINALS_DIRNAME: &str = ".gns/terminals";
/// Read: a returned slice may not exceed this many characters.
pub const READ_CHAR_HARD_LIMIT: usize = 100_000;

/// Per-tool-call execution timeout tiers (`tool-execution-timeout.ts`).
pub const EXTRA_SHORT_TOOL_TIMEOUT_MS: u64 = 5 * 60 * 1_000;
pub const SHORT_TOOL_TIMEOUT_MS: u64 = 15 * 60 * 1_000;
pub const MEDIUM_TOOL_TIMEOUT_MS: u64 = 30 * 60 * 1_000;
pub const LONG_TOOL_TIMEOUT_MS: u64 = 60 * 60 * 1_000;
pub const EXTRA_LONG_TOOL_TIMEOUT_MS: u64 = 2 * 60 * 60 * 1_000;
/// Tool names treated as long-running (subagent-style) for timeout purposes.
pub const LONG_RUNNING_TOOL_NAMES: &[&str] = &["task", "mcp_task", "subagent", "runsubagent"];

/// `update_state` memory write on the tool path prefixes `note` facts with this.
pub const MEMORY_TOOL_NOTE_PREFIX: &str = "Note: ";
/// Memory file headers written when a shard file is created.
pub const MEMORY_PROFILE_HEADER: &str = "# About the user\n\n<!-- Enduring facts, one per line as \"- (YYYY-MM-DD) <fact>\". -->\n\n";
pub const MEMORY_LOG_HEADER: &str = "# Memory log\n\n<!-- Dated facts, one per line as \"- (YYYY-MM-DD) <fact>\". -->\n\n";
/// Per-agent project membership file (`<agentDir>/projects.json`).
pub const PROJECT_MEMBERSHIP_FILENAME: &str = "projects.json";
/// Avatar limits and canonical filename.
pub const AVATAR_MAX_BYTES: usize = 5 * 1024 * 1024;
pub const CANONICAL_AVATAR_FILENAME: &str = "avatar.png";
pub const CONVENTIONAL_AVATAR_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "svg"];
/// KV key holding emoji reactions the agent left on user messages (`address → [emoji]`).
pub const REACTIONS_KV_KEY: &str = "messageReactions";
