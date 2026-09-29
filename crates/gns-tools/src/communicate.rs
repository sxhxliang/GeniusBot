//! `SendMessage`, `SendToAgent` and `ReactToMessage` (`send-message-tool.ts`,
//! `sand-agent-management-tools.ts`, `sand-reaction-tool.ts`).

use crate::paths::sandbox_path;
use async_trait::async_trait;
use gns_core::prompt::send_message_result;
use gns_core::*;
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;

/// `isValidAttachmentUrl`: `file:` or `https:` scheme.
pub fn is_valid_attachment_url(value: &str) -> bool {
    value.starts_with("file://") || value.starts_with("https://")
}

/// Reject `file://` images outside the agent's directories.
fn check_images(ctx: &ToolContext, images: &[ImageRef]) -> Result<(), ToolError> {
    for image in images {
        if let Some(path) = image.url.strip_prefix("file://") {
            sandbox_path(path, &ctx.workspace_dir, &[&ctx.workspace_dir, &ctx.data_dir])?;
        } else if !is_valid_attachment_url(&image.url) {
            return Err(ToolError::input("each images url must include a file:// or https:// scheme"));
        }
    }
    Ok(())
}

fn basename_of_file_url(url: &str) -> Option<String> {
    let path = url.strip_prefix("file://")?;
    std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).filter(|n| !n.is_empty())
}

/// An image attached to a `SendMessage` text (`images[]`).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendMessageImage {
    /// file:// or https:// URL of the image.
    pub url: String,
    /// Optional short description of this image, shown on hover and as its fullscreen caption.
    #[serde(default)]
    pub alt: Option<String>,
    /// Optional pixel width of the image.
    #[serde(default)]
    pub width: Option<u32>,
    /// Optional pixel height of the image.
    #[serde(default)]
    pub height: Option<u32>,
}

/// Arguments of `SendMessage` (`send-message-schema.ts`).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendMessageArgs {
    /// Publish full local files as downloadable attachments; do not paste their contents.
    #[serde(default)]
    pub files: Vec<String>,
    /// Forward existing authorized artifacts without reading their bytes.
    #[serde(default)]
    pub artifact_ids: Vec<String>,
    /// text for chat messages, attachment for actual files or standalone media, widget for an interactive question with selectable options, secret-request to ask the user for a credential through a secure masked input (never a chat paste).
    #[serde(rename = "type")]
    pub kind: OutboundKind,
    /// Required when type is text. The message to show to the user.
    #[serde(default)]
    pub content: Option<String>,
    /// For type attachment, supply files/artifact_ids or a URL (file:// local, https:// remote).
    #[serde(default)]
    pub url: Option<String>,
    /// Optional, only for type:text. Image(s) that belong with this message; they render inside the same chat bubble, below your text — one image full width, several as a compact gallery. Use whenever you're showing something you're talking about; use type:attachment only for an image that IS the whole message.
    #[serde(default)]
    pub images: Option<Vec<SendMessageImage>>,
    /// Optional. A short description (alt text) of the image for type:attachment — what the image shows. Shown to the user on hover and in the fullscreen viewer.
    #[serde(default)]
    pub alt: Option<String>,
    /// Optional. Short address of the prior message this reply threads to (e.g. t3u for the user message in turn 3, t3s1 for your second SendMessage in turn 3). Omit when not threading.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Required when type is widget. A question with selectable options: { prompt, helpText?, options: [{ label, value?, description?, style? }], allowCustom?, dismissOnMoveOn? }. The user picks one option; its value comes back as their reply, and the chat shows the resolved card with their selection checked under your prompt — so phrase the prompt as a natural question, not a menu instruction. The user can also dismiss the question without answering; you'll be told on your next turn, so treat that as a decline and don't re-ask. Set allowCustom: true to also let the user type their own free-text answer instead of picking an option. Set dismissOnMoveOn: true only for low-stakes questions that become moot if the user moves on (it auto-dismisses once they send a newer message without answering); leave it off for real decisions you still need answered.
    #[serde(default)]
    pub widget: Option<Widget>,
    /// Required when type is secret-request. Asks the user for a credential through a masked secure input; the value goes straight to the connector's credential file and never reaches you or the chat. You only learn that it was provided.
    #[serde(default)]
    pub secret: Option<SecretRequest>,
}

const SEND_MESSAGE_DESCRIPTION: &str = "Say something to the user in the chat. This is your only voice. The user only ever sees the content of SendMessage calls; your plain assistant text is invisible to them (it is just your private scratchpad), so a reply counts only once it is inside SendMessage, including short, casual, or social replies like \"Hey\" or \"Doing good, you?\". Finish a turn where someone is waiting on you without calling SendMessage and they see total silence and assume you ignored them; the lone exception is a scheduled routine (a [routine] run) whose saved instruction says to stay quiet when there's nothing to report, where ending with no SendMessage is correct rather than filler like \"(no change.)\". Keep the user posted with meaningful beats, not just at the end: post an update for a real result, decision, blocker, or change of plan, and batch or omit routine mechanics, retries, and minor snags rather than narrating each one; prefer fewer, higher-signal updates over a play-by-play. Still, never vanish into a long silent run on something the user is waiting on. This also covers results: output the user is waiting on counts as delivered only inside a SendMessage, so an opening acknowledgement does not discharge it (ack ≠ delivery), and if you ran something for them you send the actual result before you yield. Use {\"type\":\"text\",\"content\":\"...\"} for normal messages. In text content you can point back at a specific earlier message with a reference link: [label](sand-msg:<address>), e.g. \"Covered in [my earlier breakdown](sand-msg:t2s1)\" — it renders as a small chip that jumps there on click. Addresses are the same ones reply_to uses (a user message's [t3u] tag, the id a sent message hands back), but unlike reply_to this never threads anything. Reference only where pointing back genuinely helps (an \"as I mentioned earlier\" moment); write the label as the words your sentence needs, and never write a bare address into visible text. Use {\"type\":\"attachment\",\"url\":\"file:///absolute/path/to/file.png\"} for actual files or standalone media; https:// file/media URLs are also accepted. The rule for images: if image(s) belong WITH what you're saying, attach them to the text message itself — {\"type\":\"text\",\"content\":\"...\",\"images\":[{\"url\":\"file:///absolute/path/to/shot.png\",\"alt\":\"...\"}]} renders them inside the same chat bubble, below your text (one image full width, several as a compact gallery). Use {\"type\":\"attachment\"} only when the image IS the whole message, with no accompanying text; videos and non-image files always go as attachments. Never embed images as markdown ![](...) in content. Use {\"type\":\"widget\",\"widget\":{...}} to ask the user a question with selectable options instead of asking in plain text — but ask rarely: by default decide and proceed (see Autonomy), reserving a widget for a consequential or destructive go/no-go, true ambiguity you cannot resolve by looking it up, or something only the user knows. Every option must be a real, verified choice, never invented, guessed, or a plausible-looking placeholder; if you do not know the real options, look them up first (search the relevant connector, tool, or directory) rather than presenting fakes. Use {\"type\":\"secret-request\",\"secret\":{\"label\":\"...\",\"connector\":\"...\",\"field\":\"...\"}} to ask for a credential (an API token, key, or secret): the user gets a masked secure input and the value goes straight to the connector's credential file. NEVER ask the user to paste a token, key, or password into the chat; always request it this way so it stays out of the transcript and out of your context. You only learn that they provided it. Sending a secret-request ends your turn; you are resumed once they submit. The widget has a prompt, optional helpText, and 1-6 options; each option has a label, an optional value (the text sent back to you when confirmed; defaults to the label), an optional description, and an optional style (\"default\"|\"primary\"|\"danger\"). Set the optional allowCustom: true to also let the user type their own free-text answer instead of picking an option. Set the optional dismissOnMoveOn: true only for low-stakes questions that become moot if the user moves on; the widget then auto-dismisses once they send a newer message without answering. Leave it off (default) for real decisions you still need answered. The user picks an option and its value comes back to you as their reply. In the chat, the resolved card keeps your question and shows their selection checked under it, so phrase the prompt as a natural conversational question (never a menu instruction like \"Pick one of the following\") and give every option a value that reads like a reply the user would actually send. The user can also dismiss the question without answering; you'll be told on your next turn — treat that as a decline and don't re-ask. Example: {\"type\":\"widget\",\"widget\":{\"prompt\":\"Deploy to production?\",\"options\":[{\"label\":\"Deploy\",\"value\":\"Yes, deploy now\",\"style\":\"primary\"},{\"label\":\"Cancel\",\"value\":\"No, hold off\",\"style\":\"danger\"}]}}. When you do genuinely need a decision or confirmation, this widget is how you ask, not plain text. Sending a widget ends your turn; make it your last action and stop, and the user's selection arrives as the next message.";

fn kind_tag(kind: OutboundKind) -> &'static str {
    match kind {
        OutboundKind::Text => "text",
        OutboundKind::Attachment => "attachment",
        OutboundKind::Widget => "widget",
        OutboundKind::SecretRequest => "secret-request",
        #[allow(unreachable_patterns)]
        _ => "unknown",
    }
}

fn provided_str(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(|v| !v.is_empty())
}

/// `refineSendMessage`: cross-field validation with the original wording.
pub fn refine_send_message(args: &SendMessageArgs) -> Vec<String> {
    let mut issues = Vec::new();
    let kind = args.kind;
    let fields: [(&str, bool, OutboundKind); 5] = [
        ("content", provided_str(&args.content), OutboundKind::Text),
        ("url", provided_str(&args.url), OutboundKind::Attachment),
        ("alt", provided_str(&args.alt), OutboundKind::Attachment),
        ("widget", args.widget.is_some(), OutboundKind::Widget),
        ("secret", args.secret.is_some(), OutboundKind::SecretRequest),
    ];
    for (field, provided, allowed) in fields {
        if kind != allowed && provided {
            let allowed = format!("type:{}", kind_tag(allowed));
            issues.push(format!(
                "{field} is only valid with {allowed} and cannot ride a type:{} message — it would be silently dropped. Nothing was sent. Re-send as separate SendMessage calls, one per type: this field on its own properly-typed message ({allowed}), and any text as its own type:text message.",
                kind_tag(kind)
            ));
        }
    }
    if args.images.as_ref().is_some_and(|i| !i.is_empty()) && kind != OutboundKind::Text {
        issues.push(
            "images can only be set for type:text (they attach to a text message); for a standalone attachment use type:attachment with url"
                .to_owned(),
        );
    }
    match kind {
        OutboundKind::Text => {
            if args.content.as_deref().is_none_or(|c| c.trim().is_empty()) {
                issues.push("content is required when type is text".to_owned());
            }
            for image in args.images.iter().flatten() {
                if !is_valid_attachment_url(image.url.trim()) {
                    issues.push("each images url must include a file:// or https:// scheme".to_owned());
                }
            }
        }
        OutboundKind::Attachment => match args.url.as_deref().map(str::trim) {
            None | Some("") if args.files.is_empty() && args.artifact_ids.is_empty() => {
                issues.push("url or files/artifact_ids is required when type is attachment".to_owned())
            }
            None | Some("") => {}
            Some(url) if !is_valid_attachment_url(url) => {
                issues.push("url must include a file:// or https:// scheme when type is attachment".to_owned())
            }
            _ => {}
        },
        OutboundKind::Widget => match &args.widget {
            None => issues.push("widget is required when type is widget".to_owned()),
            Some(widget) => {
                if widget.prompt.trim().is_empty() {
                    issues.push("widget.prompt must contain at least 1 character(s)".to_owned());
                }
                if widget.options.is_empty() {
                    issues.push("widget.options must contain at least 1 element(s)".to_owned());
                }
                if widget.options.len() > 6 {
                    issues.push("widget.options must contain at most 6 element(s)".to_owned());
                }
                if widget.options.iter().any(|o| o.label.trim().is_empty()) {
                    issues.push("widget.options label must contain at least 1 character(s)".to_owned());
                }
            }
        },
        OutboundKind::SecretRequest => match &args.secret {
            None => issues.push("secret is required when type is secret-request".to_owned()),
            Some(secret) if secret.label.trim().is_empty() => issues.push("secret.label must contain at least 1 character(s)".to_owned()),
            _ => {}
        },
        #[allow(unreachable_patterns)]
        _ => {}
    }
    issues
}

/// The agent's only voice.
#[derive(Debug, Default)]
pub struct SendMessageTool;

impl SendMessageTool {
    pub fn arc() -> Arc<dyn Tool> {
        Typed::arc(Self)
    }
}

#[async_trait]
impl TypedTool for SendMessageTool {
    type Args = SendMessageArgs;
    fn name(&self) -> &str {
        SEND_MESSAGE_TOOL_NAME
    }
    fn description(&self) -> &str {
        SEND_MESSAGE_DESCRIPTION
    }
    async fn run(&self, ctx: &ToolContext, args: Self::Args) -> Result<ToolOutput, ToolError> {
        let issues = refine_send_message(&args);
        if !issues.is_empty() {
            return Err(ToolError::input(issues.join("; ")));
        }
        let reply_to = args.reply_to.as_deref().map(str::trim).filter(|r| !r.is_empty()).map(str::to_owned);
        let mut message = OutboundMessage::text("");
        message.reply_to = reply_to;
        match args.kind {
            OutboundKind::Text => {
                let images: Vec<ImageRef> = args
                    .images
                    .unwrap_or_default()
                    .into_iter()
                    .map(|i| ImageRef {
                        url: i.url.trim().to_owned(),
                        alt: i.alt.map(|a| a.trim().to_owned()).filter(|a| !a.is_empty()),
                        width: i.width,
                        height: i.height,
                    })
                    .collect();
                check_images(ctx, &images)?;
                message.kind = OutboundKind::Text;
                message.content = Some(args.content.unwrap_or_default().trim().to_owned());
                message.images = images;
            }
            OutboundKind::Attachment => {
                let url = args.url.unwrap_or_default().trim().to_owned();
                if let Some(path) = url.strip_prefix("file://") {
                    sandbox_path(path, &ctx.workspace_dir, &[&ctx.workspace_dir, &ctx.data_dir])?;
                }
                message.kind = OutboundKind::Attachment;
                message.content = None;
                message.file_name = basename_of_file_url(&url);
                message.alt = args.alt.map(|a| a.trim().to_owned()).filter(|a| !a.is_empty());
                message.url = (!url.is_empty()).then_some(url);
            }
            OutboundKind::Widget => {
                message.kind = OutboundKind::Widget;
                message.content = None;
                message.widget = args.widget;
            }
            OutboundKind::SecretRequest => {
                message.kind = OutboundKind::SecretRequest;
                message.content = None;
                message.secret = args.secret.map(Box::new);
            }
            #[allow(unreachable_patterns)]
            _ => return Err(ToolError::input("unsupported message type")),
        }
        let mut files = args.files;
        if let Some(path) = message.url.as_deref().and_then(|u| u.strip_prefix("file://")) {
            files.push(path.to_owned());
            message.url = None;
        }
        message.artifacts = ctx.services.prepare_artifacts(&ctx.agent_id, files, args.artifact_ids).await?;
        let kind = message.kind;
        let entry_id = ctx.services.deliver_message(&ctx.agent_id, &ctx.run_id, message).await?;
        let mut output = ToolOutput::text(send_message_result(Some(entry_id.as_str()))).with_effect(TurnEffect::MessageSent);
        if matches!(kind, OutboundKind::Widget | OutboundKind::SecretRequest) {
            output = output.with_effect(TurnEffect::AwaitingUserSelection);
        }
        Ok(output)
    }
}

/// An image attached to a `SendToAgent` message.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentImage {
    /// file:// or https:// URL of the image.
    pub url: String,
    /// Optional short description of this image, shown on hover and as its fullscreen caption.
    #[serde(default)]
    pub alt: Option<String>,
}

/// Arguments of `SendToAgent`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendToAgentArgs {
    /// Publish full local files. The host snapshots bytes and grants the target access.
    #[serde(default)]
    pub files: Vec<String>,
    /// Forward existing authorized artifact IDs to this teammate or group.
    #[serde(default)]
    pub artifact_ids: Vec<String>,
    /// The id of the target — either another agent or a GROUP you belong to. Use an id from your teammates list, ListAgents, or ListGroups — not a name.
    pub target_id: String,
    /// What to say. Write it as if texting a teammate: lead with the point, keep it short.
    pub message: String,
    /// Optional image(s) to send with the message — a screenshot, chart, or photo the other agent needs. Delivered with your message: a 1:1 recipient actually sees them (like an image the user sends), and they render with your text in the exchange. Not delivered to groups.
    #[serde(default)]
    pub images: Option<Vec<AgentImage>>,
    /// When true (1:1 only; ignored for groups), interrupt the recipient's current non-user work and wake them immediately — same steer as a direct user message. Use for STOP / supersede / time-critical instructions. Default false: waits out the current turn, but still runs ahead of automations and other background work.
    #[serde(default)]
    pub priority: Option<bool>,
}

/// Fire-and-forget cross-agent messaging.
#[derive(Debug, Default)]
pub struct SendToAgentTool;

impl SendToAgentTool {
    pub fn arc() -> Arc<dyn Tool> {
        Typed::arc(Self)
    }
}

#[async_trait]
impl TypedTool for SendToAgentTool {
    type Args = SendToAgentArgs;
    fn name(&self) -> &str {
        SEND_TO_AGENT_TOOL_NAME
    }
    fn description(&self) -> &str {
        "Send a message to ANOTHER of your user's agents, OR post into a GROUP chat you belong to, by its id (not the user — SendMessage is how you reach the user). This is FIRE-AND-FORGET and asynchronous, like texting: it delivers your message, wakes that agent (or the group's members), and returns immediately with a delivery acknowledgement. Peer messages run ahead of automations and other background work; pass priority=true on a 1:1 send to interrupt the recipient's current non-user turn (STOP / supersede), like a direct user message (ignored for groups). It does NOT return their reply, and you must not wait or poll for one in this turn — send it and move on. Any reply arrives later as its own message that wakes you on a fresh turn. Get agent ids from your teammates list or ListAgents, and group ids from ListGroups. To include image(s) — a screenshot, chart, or photo the other agent needs — pass images: [{\"url\":\"file:///absolute/path/to/shot.png\",\"alt\":\"...\"}] (file:// or https://). A 1:1 recipient actually sees them, like an image the user sends; never paste an image as a markdown ![](...) in the message text. Group posts are text-only today, so send images to an agent directly. Use it deliberately and sparingly — waking another agent or a whole group is a real side effect, so treat it like messaging on the user's behalf. Message someone or post to a group only when it truly serves the user's goal, not because one was mentioned or complained about, and don't spam a group. Never relay the user's private or unfiltered words (especially a complaint or criticism) verbatim; if relaying is warranted, paraphrase the actionable point diplomatically, not their tone. If you're unsure the user wants this sent, handle it yourself or ask first. Keep the message purposeful, professional, and minimal. One clearly relevant recipient can be normal work; messaging SEVERAL agents about the same effort (or posting it to a group) is a fan-out that wakes every recipient, and their replies land back in the user's chats and rooms — so fan out only when the user explicitly asked you to contact those agents. Otherwise propose it first with a question widget and wait for a yes, and never fan out \"meanwhile\" while you're waiting on the user for data or a decision."
    }
    async fn run(&self, ctx: &ToolContext, args: Self::Args) -> Result<ToolOutput, ToolError> {
        let target = args.target_id.trim();
        if target.is_empty() {
            return Err(ToolError::input("target_id is required"));
        }
        if args.message.trim().is_empty() {
            return Err(ToolError::input("message is required"));
        }
        if target == ctx.agent_id.as_str() {
            return Ok(ToolOutput::text(
                "You can't message yourself with SendToAgent. Use SendMessage to talk to the user, or pick a different target id.",
            ));
        }
        let images: Vec<ImageRef> = args
            .images
            .unwrap_or_default()
            .into_iter()
            .map(|i| ImageRef::new(i.url.trim().to_owned(), i.alt.map(|a| a.trim().to_owned()).filter(|a| !a.is_empty())))
            .collect();
        check_images(ctx, &images)?;
        let artifacts = ctx.services.prepare_artifacts(&ctx.agent_id, args.files, args.artifact_ids).await?;
        let ack = ctx
            .services
            .send_to_agent_with_artifacts(
                &ctx.agent_id,
                target,
                args.message.trim().to_owned(),
                images,
                artifacts,
                args.priority.unwrap_or(false),
            )
            .await?;
        Ok(ToolOutput::text(ack).with_summary(format!("target: {target}")))
    }
}

/// Arguments of `ReactToMessage`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReactToMessageArgs {
    /// The address of the USER message to react to — the [t3u]-style tag shown on their message. Only the user's own messages, never your own sends.
    pub message_address: String,
    /// A single common emoji to react with, e.g. 👍, ❤️, 😂, 🎉.
    pub emoji: String,
}

/// `isMessageAddress`: `t<n>u`, `t<n>ua<k>`, `t<n|b>s<k>`, `t<n|b>a<k>`.
pub fn is_message_address(value: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^t(?:\d+u(?:a\d+)?|(?:\d+|b)[as]\d+)$").expect("static regex")).is_match(value)
}

/// Emoji tapback on a user message.
#[derive(Debug, Default)]
pub struct ReactToMessageTool;

impl ReactToMessageTool {
    pub fn arc() -> Arc<dyn Tool> {
        Typed::arc(Self)
    }
}

#[async_trait]
impl TypedTool for ReactToMessageTool {
    type Args = ReactToMessageArgs;
    fn name(&self) -> &str {
        REACT_TO_MESSAGE_TOOL_NAME
    }
    fn description(&self) -> &str {
        "React to one of the USER's messages with a single emoji tapback (like an iMessage reaction), attributed to you and shown as a small pill on their message. Use this VERY sparingly, only when a reaction is the genuinely natural, human response and a reply would be overkill: they said something funny, shared good news, or a quick 👍 fits better than a sentence. It is NOT a substitute for a real reply when they asked you for something, and you never react just to seem friendly. Only react to the user's own messages (their [t3u]-style address), never your own sends. It toggles: reacting the same emoji to the same message again removes your reaction, which is how you take one back. Fire-and-forget: it doesn't end your turn and returns nothing to act on. Mirror the user — if they don't use emoji, basically never do this."
    }
    async fn run(&self, ctx: &ToolContext, args: Self::Args) -> Result<ToolOutput, ToolError> {
        let address = args.message_address.trim();
        if !is_message_address(address) {
            return Ok(ToolOutput::text(format!(
                "\"{address}\" isn't a valid message address. React with the [t3u]-style tag shown on the user's message."
            )));
        }
        let emoji = args.emoji.trim();
        if emoji.is_empty() || emoji.chars().count() > 16 {
            return Err(ToolError::input("emoji must be a single emoji (1-16 characters)"));
        }
        let ack = ctx.services.react_to_message(&ctx.agent_id, &ctx.run_id, address, emoji).await?;
        Ok(ToolOutput::text(ack).with_effect(TurnEffect::Reacted).with_summary(format!("target: {address}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(kind: OutboundKind) -> SendMessageArgs {
        SendMessageArgs {
            files: Vec::new(),
            artifact_ids: Vec::new(),
            kind,
            content: None,
            url: None,
            images: None,
            alt: None,
            reply_to: None,
            widget: None,
            secret: None,
        }
    }

    #[test]
    fn refine_reports_the_original_issues() {
        let mut a = args(OutboundKind::Text);
        assert_eq!(refine_send_message(&a), vec!["content is required when type is text"]);
        a.content = Some("hi".into());
        a.url = Some("https://x".into());
        let issues = refine_send_message(&a);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0]
                .starts_with("url is only valid with type:attachment and cannot ride a type:text message — it would be silently dropped.")
        );
        let mut w = args(OutboundKind::Widget);
        w.images = Some(vec![SendMessageImage { url: "https://x".into(), alt: None, width: None, height: None }]);
        let issues = refine_send_message(&w);
        assert!(issues.iter().any(|i| i.starts_with("images can only be set for type:text")));
        assert!(issues.iter().any(|i| i == "widget is required when type is widget"));
        let mut att = args(OutboundKind::Attachment);
        att.url = Some("ftp://x".into());
        assert_eq!(refine_send_message(&att), vec!["url must include a file:// or https:// scheme when type is attachment"]);
        let mut s = args(OutboundKind::SecretRequest);
        assert_eq!(refine_send_message(&s), vec!["secret is required when type is secret-request"]);
        s.secret = Some(SecretRequest {
            label: "Slack bot token".into(),
            description: None,
            connector: Some("slack".into()),
            field: Some("token".into()),
        });
        assert!(refine_send_message(&s).is_empty());
    }

    #[test]
    fn message_addresses() {
        for ok in ["t3u", "t0u", "t3ua1", "t3s1", "tba2", "t12a0"] {
            assert!(is_message_address(ok), "{ok}");
        }
        for bad in ["t3", "u3", "t3x1", "3u", " t3u", "tb"] {
            assert!(!is_message_address(bad), "{bad}");
        }
    }

    #[test]
    fn schegns_are_flat() {
        for schema in [schema_for_args::<SendMessageArgs>(), schema_for_args::<SendToAgentArgs>(), schema_for_args::<ReactToMessageArgs>()]
        {
            let text = schema.to_string();
            for forbidden in ["$ref", "$defs", "anyOf", "oneOf", "\"null\""] {
                assert!(!text.contains(forbidden), "{text}");
            }
        }
        let schema = schema_for_args::<SendMessageArgs>();
        let kinds: Vec<&str> = schema["properties"]["type"]["enum"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(kinds, ["text", "attachment", "widget", "secret-request"]);
        let required = schema["required"].as_array().expect("schema required fields");
        assert!(required.iter().any(|field| field.as_str() == Some("type")), "{required:?}");
        let style = &schema["properties"]["widget"]["properties"]["options"]["items"]["properties"]["style"];
        assert_eq!(style["type"], "string");
        assert_eq!(style["enum"].as_array().unwrap().len(), 3);
        assert!(schema["properties"]["widget"]["properties"].get("dismissOnMoveOn").is_some());
    }
}
