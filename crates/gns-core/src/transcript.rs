//! Transcript entries persisted as JSON in `transcript_entries.entry`. Field
//! names use camelCase to stay compatible with the original store layout.

use crate::ids::{AgentId, EntryId, GroupId, RunId};
use serde::{Deserialize, Serialize};

/// Who authored a message entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
}

/// Reference to another agent attached to a message (`fromAgent` / `toAgent`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRef {
    pub id: AgentId,
    pub name: String,
}

/// An image attached to a message (`file://` or `https://`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ImageRef {
    /// file:// or https:// URL of the image.
    pub url: String,
    /// Optional short description of this image, shown on hover and as its fullscreen caption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
    /// Pixel width, when known (filled in by the host, not the model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Pixel height, when known (filled in by the host, not the model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

impl ImageRef {
    /// An image reference with no dimensions.
    pub fn new(url: impl Into<String>, alt: Option<String>) -> Self {
        Self { url: url.into(), alt, width: None, height: None }
    }
}

/// Visual weight of a widget option button.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WidgetActionStyle {
    Default,
    Primary,
    Danger,
}

/// One selectable option inside a question widget.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WidgetOption {
    /// Label shown to the user.
    pub label: String,
    /// Text sent back to you when this option is picked. Defaults to the label. Make it read like something the user would naturally say in reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Optional one-line explanation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Button style: default, primary or danger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<WidgetActionStyle>,
}

/// A question with selectable options.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Widget {
    /// The question, phrased naturally.
    pub prompt: String,
    /// Optional help text under the prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help_text: Option<String>,
    /// The options the user can pick from (1 to 6).
    pub options: Vec<WidgetOption>,
    /// When true, the user can type a custom free-text answer instead of choosing one of the options.
    #[serde(default)]
    pub allow_custom: bool,
    /// When true, this widget auto-dismisses (becomes inert, shows a muted Dismissed state) once the user sends a newer message without answering it. Omit/false to keep the question live and answerable indefinitely. Set true only for low-stakes questions that become moot if the user moves on; keep it off for real decisions you still need answered.
    #[serde(default)]
    pub dismiss_on_move_on: bool,
}

/// A credential request delivered through a masked secure input
/// (`type:secret-request`). The value never reaches the agent or the transcript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SecretRequest {
    /// What credential to ask for, shown as the card title and echoed in the field placeholder ("Paste your …"), e.g. "Slack bot token".
    pub label: String,
    /// Optional short help shown under the label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The connector/platform the secret is for. The value is written to that connector's per-agent credential file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector: Option<String>,
    /// The credential field name to store the value under, e.g. "token".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

/// Payload kinds accepted by `SendMessage`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OutboundKind {
    Text,
    Attachment,
    Widget,
    #[serde(rename = "secret-request")]
    SecretRequest,
}

/// A message delivered to the user (or to a group room) through `SendMessage`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboundMessage {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<crate::ArtifactRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<Box<crate::TaskRecord>>,
    #[serde(rename = "type")]
    pub kind: OutboundKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widget: Option<Widget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// Basename of a `file://` attachment (filled in by the host).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// Payload of a `secret-request` message (boxed to keep transcript entries small).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<Box<SecretRequest>>,
}

impl OutboundMessage {
    /// Plain text message.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            artifacts: Vec::new(),
            task: None,
            kind: OutboundKind::Text,
            content: Some(content.into()),
            url: None,
            alt: None,
            images: Vec::new(),
            widget: None,
            reply_to: None,
            file_name: None,
            secret: None,
        }
    }
    /// Human-readable body for logs and room history.
    pub fn body_text(&self) -> String {
        match self.kind {
            OutboundKind::Text => self.content.clone().unwrap_or_default(),
            OutboundKind::Attachment => format!(
                "[attachment] {}{}",
                self.url.clone().unwrap_or_default(),
                self.alt.as_ref().map(|a| format!(" — {a}")).unwrap_or_default()
            ),
            OutboundKind::Widget => {
                let Some(widget) = &self.widget else {
                    return "[widget]".to_owned();
                };
                let mut out = widget.prompt.clone();
                for (i, opt) in widget.options.iter().enumerate() {
                    out.push_str(&format!("\n  {}. {}", i + 1, opt.label));
                    if let Some(d) = &opt.description {
                        out.push_str(&format!(" — {d}"));
                    }
                }
                out
            }
            OutboundKind::SecretRequest => match self.secret.as_deref() {
                Some(secret) => format!("[secret request] {}", secret.label),
                None => "[secret request]".to_owned(),
            },
        }
    }
    pub fn display_text(&self) -> String {
        let mut body = self.body_text();
        body.push_str(&crate::artifact_prompt(&self.artifacts));
        if let Some(task) = &self.task {
            body.push_str(&format!("\nTask {}: {:?}. {}", task.id, task.status, task.verification));
        }
        body
    }
}

/// An emoji reaction on a message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    pub emoji: String,
    /// `"me"` for the user, otherwise the reacting agent's id.
    pub by: String,
}

/// Status of a recorded tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolCallStatus {
    Started,
    Finished,
    Failed,
}

/// A persisted transcript entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum TranscriptEntry {
    /// A user, inbound-agent, outbound-agent or hidden system prompt message.
    Message {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        artifacts: Vec<crate::ArtifactRef>,
        id: EntryId,
        role: Role,
        content: String,
        timestamp_ms: i64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        hidden: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<RunId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_agent: Option<AgentRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to_agent: Option<AgentRef>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group_id: Option<GroupId>,
        /// Inbound agent message flagged as priority.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        priority: bool,
        /// Display name of the user who posted (group rooms).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_name: Option<String>,
        /// Entry this message replies to (threads).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<EntryId>,
    },
    /// Something the agent said to the user (or a room) via `SendMessage`.
    SendMessage {
        id: EntryId,
        message: OutboundMessage,
        timestamp_ms: i64,
        run_id: RunId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group_id: Option<GroupId>,
        /// Room posts: the member who spoke.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        author: Option<AgentRef>,
        /// Entry this message replies to (`message.reply_to`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<EntryId>,
        /// Emoji reactions (`by` is `"me"` for the user or an agent id).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reactions: Vec<Reaction>,
        /// The value the user chose on a widget.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        responded_value: Option<String>,
        /// The user moved on without answering this widget.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        widget_skipped: bool,
        /// The user dismissed this widget.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        widget_dismissed: bool,
    },
    /// The model's plain text (inner monologue), kept for history reconstruction.
    AssistantText { id: EntryId, content: String, timestamp_ms: i64, run_id: RunId },
    /// A tool invocation with its result.
    ToolCall {
        id: EntryId,
        call_id: String,
        name: String,
        args: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
        timestamp_ms: i64,
        run_id: RunId,
    },
    /// Profile edits announced to the model.
    ProfileUpdate { id: EntryId, summary: String, timestamp_ms: i64 },
    /// Context compaction marker; history before it is summarised.
    Divider {
        id: EntryId,
        summary: String,
        timestamp_ms: i64,
        compaction_epoch: u64,
        /// Last entry covered by the summary; entries after it up to the
        /// divider are the verbatim tail kept in context.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summarized_through: Option<EntryId>,
    },
}

impl TranscriptEntry {
    /// The entry id.
    pub fn id(&self) -> &EntryId {
        match self {
            TranscriptEntry::Message { id, .. }
            | TranscriptEntry::SendMessage { id, .. }
            | TranscriptEntry::AssistantText { id, .. }
            | TranscriptEntry::ToolCall { id, .. }
            | TranscriptEntry::ProfileUpdate { id, .. }
            | TranscriptEntry::Divider { id, .. } => id,
        }
    }

    /// The entry timestamp.
    pub fn timestamp_ms(&self) -> i64 {
        match self {
            TranscriptEntry::Message { timestamp_ms, .. }
            | TranscriptEntry::SendMessage { timestamp_ms, .. }
            | TranscriptEntry::AssistantText { timestamp_ms, .. }
            | TranscriptEntry::ToolCall { timestamp_ms, .. }
            | TranscriptEntry::ProfileUpdate { timestamp_ms, .. }
            | TranscriptEntry::Divider { timestamp_ms, .. } => *timestamp_ms,
        }
    }

    /// Rough character size used for windowing.
    pub fn char_len(&self) -> usize {
        match self {
            TranscriptEntry::Message { content, .. } => content.chars().count(),
            TranscriptEntry::SendMessage { message, .. } => message.display_text().chars().count(),
            TranscriptEntry::AssistantText { content, .. } => content.chars().count(),
            TranscriptEntry::ToolCall { args, result, .. } => args.to_string().len() + result.as_ref().map(|r| r.len()).unwrap_or(0),
            TranscriptEntry::ProfileUpdate { summary, .. } | TranscriptEntry::Divider { summary, .. } => summary.chars().count(),
        }
    }
}
