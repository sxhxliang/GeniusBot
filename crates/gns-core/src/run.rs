//! Turn scheduling vocabulary: lanes, sources, options and results.
//!
//! Mirrors the original host's `RunLane` (`user` > `agent` > `background`),
//! its per-turn options (`hidden`, `isSilenceAllowed`, system prompt
//! override) and its `TurnResult`.

use serde::{Deserialize, Serialize};

/// Priority lanes, lowest first. Each lane is a FIFO queue; the scheduler
/// always drains `User` before `Agent` before `Automation` (the original's
/// `background` lane).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lane {
    /// Background work: routines, broadcasts, revivals, ack redrives (lowest).
    Automation,
    /// Cross-agent wakes and room turns started by an agent's post.
    Agent,
    /// Direct user interaction, room turns started by a user post, kickstart (highest).
    User,
}

/// What triggered a turn (the original's `source` / `requestSource`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunSource {
    /// A message the user typed (`"turn"`).
    User,
    /// An inbound cross-agent wake (`"agent"`).
    Agent,
    /// A routine firing (`"automation"`).
    Automation,
    /// A group-member turn inside a room round (`"group-member"`).
    Group,
    /// The first-run introduction (`"kickstart"`).
    Kickstart,
    /// An owner broadcast (`"broadcast"`).
    Broadcast,
    /// A hidden wake carrying a background job / task result (`"shell-revival"` / `"subagent-revival"`).
    Background,
    /// A hidden recovery turn for an unacknowledged user message (`"ack-redrive"`).
    AckRedrive,
    /// An ephemeral subagent spawned by a parent's tool call.
    Subagent,
}

/// Options for one turn.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunOptions {
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub task_attempt: u64,
    /// Hidden prompts are persisted but not shown to the user as their message.
    pub hidden: bool,
    /// When true the agent may end the turn without any `SendMessage`.
    pub is_silence_allowed: bool,
    pub lane: Lane,
    pub source: RunSource,
    /// Replace the BASE section of the system prompt (group room turns);
    /// every other section is still assembled, as in the original.
    #[serde(default)]
    pub base_prompt_override: Option<String>,
    #[serde(default)]
    pub max_steps: Option<usize>,
    /// Group room this turn speaks into (room turns only).
    #[serde(default)]
    pub group_id: Option<crate::ids::GroupId>,
    /// Append the user reply reminder to the prompt (visible user turns).
    #[serde(default)]
    pub append_reply_reminder: bool,
    /// The routine this wake belongs to (excluded from the status reminder).
    #[serde(default)]
    pub automation_id: Option<String>,
    /// The routine wake carries untrusted event text (no trusted marker).
    #[serde(default)]
    pub contains_untrusted_event_text: bool,
}

impl RunOptions {
    fn base(lane: Lane, source: RunSource, hidden: bool, is_silence_allowed: bool) -> Self {
        Self {
            task_id: None,
            task_attempt: 0,
            hidden,
            is_silence_allowed,
            lane,
            source,
            base_prompt_override: None,
            max_steps: None,
            group_id: None,
            append_reply_reminder: false,
            automation_id: None,
            contains_untrusted_event_text: false,
        }
    }
    /// A visible turn opened by the user.
    pub fn user() -> Self {
        Self { append_reply_reminder: true, ..Self::base(Lane::User, RunSource::User, false, false) }
    }
    /// A hidden wake from another agent (silence allowed).
    pub fn agent_wake() -> Self {
        Self::base(Lane::Agent, RunSource::Agent, true, true)
    }
    /// A hidden routine wake (silence allowed).
    pub fn automation() -> Self {
        Self::base(Lane::Automation, RunSource::Automation, true, true)
    }
    /// A hidden wake with a background job / task result (silence allowed).
    pub fn background_wake() -> Self {
        Self::base(Lane::Automation, RunSource::Background, true, true)
    }
    /// The hidden first-run introduction (user lane, one reply nudge).
    pub fn kickstart() -> Self {
        Self::base(Lane::User, RunSource::Kickstart, true, false)
    }
    /// A hidden owner broadcast (background lane, one reply nudge).
    pub fn broadcast() -> Self {
        Self::base(Lane::Automation, RunSource::Broadcast, true, false)
    }
    /// A hidden ack-obligation redrive (background lane).
    pub fn ack_redrive() -> Self {
        Self::base(Lane::Automation, RunSource::AckRedrive, true, false)
    }
    /// A group room turn: the member prompt replaces the base section; the
    /// turn is neither hidden nor silence-allowed (reminder middlewares run),
    /// on the lane of whatever triggered the round.
    pub fn group_turn(group_id: crate::ids::GroupId, member_base_prompt: String, lane: Lane) -> Self {
        Self {
            base_prompt_override: Some(member_base_prompt),
            group_id: Some(group_id),
            ..Self::base(lane, RunSource::Group, false, false)
        }
    }
}

/// Token usage accumulated over a turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UsageTotals {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_prompt_tokens: u64,
    pub total_tokens: u64,
    pub llm_calls: u64,
}

impl UsageTotals {
    /// Accumulate another usage sample.
    pub fn add(&mut self, other: &UsageTotals) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.cached_prompt_tokens += other.cached_prompt_tokens;
        self.total_tokens += other.total_tokens;
        self.llm_calls += other.llm_calls;
    }
}

/// Outcome of one turn (the original's `TurnResult`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RunResult {
    /// Final plain assistant text (inner monologue; not delivered).
    pub text: String,
    pub sent_message_count: usize,
    /// A `ReactToMessage` reached the user (counts as delivery).
    pub reacted: bool,
    pub aborted: bool,
    /// The queued user turn was skipped because a newer user message
    /// supersedes it (its text is carried by the newer turn's context).
    pub superseded: bool,
    pub awaiting_user_selection: bool,
    pub usage: UsageTotals,
    /// Prompt tokens of the last model request (context size estimate).
    pub last_prompt_tokens: u64,
    pub steps: usize,
    /// Messages posted into a group room during a room turn.
    pub room_messages: Vec<crate::OutboundMessage>,
    /// Set when the turn ended with an error.
    pub error: Option<String>,
    /// The turn acknowledged the user first, then its last tool step had no
    /// delivery call and the model ended without text: results may be undelivered.
    pub ended_on_silent_tool_calls: bool,
}

impl RunResult {
    /// A user-facing turn still owes a reply (no message, no reaction).
    pub fn delivery_owed(&self) -> bool {
        self.sent_message_count == 0 && !self.reacted
    }
}
