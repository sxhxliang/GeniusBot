//! Adapter: one MCP tool as a [`Tool`].

use crate::client::McpClient;
use async_trait::async_trait;
use gns_core::text::{clamp_block, clamp_line};
use gns_core::*;
use serde_json::Value;
use std::sync::Arc;

/// Largest tool result handed back to the model.
const MAX_RESULT_CHARS: usize = 60_000;

/// A tool served by an MCP server.
pub struct McpTool {
    client: Arc<McpClient>,
    server: String,
    remote_name: String,
    name: String,
    description: String,
    schema: Value,
}

impl std::fmt::Debug for McpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTool").field("name", &self.name).field("server", &self.server).finish_non_exhaustive()
    }
}

impl McpTool {
    /// Wrap one advertised tool.
    pub fn new(client: Arc<McpClient>, server: &str, def: &McpToolDef) -> Self {
        let mut schema = if def.input_schema.is_object() { def.input_schema.clone() } else { serde_json::json!({"type": "object"}) };
        normalize_tool_schema(&mut schema);
        let description = clamp_line(&format!("[MCP {server}] {}", def.description.trim()), 1_000);
        Self {
            client,
            server: server.to_owned(),
            remote_name: def.name.clone(),
            name: mcp_tool_name(server, &def.name),
            description,
            schema,
        }
    }
    /// The server this tool belongs to.
    pub fn server(&self) -> &str {
        &self.server
    }
    /// The tool's name on the server.
    pub fn remote_name(&self) -> &str {
        &self.remote_name
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }
    fn availability(&self) -> ToolAvailability {
        ToolAvailability::Always
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let result = self.client.call_tool(&self.remote_name, args, ctx.cancel.clone()).await?;
        let (text, is_error) = flatten_call_result(&result);
        let text = clamp_block(&text, MAX_RESULT_CHARS);
        if is_error {
            return Err(ToolError::failed(if text.is_empty() { "the MCP tool reported an error".to_owned() } else { text }));
        }
        Ok(ToolOutput::text(if text.is_empty() { "(no output)".to_owned() } else { text })
            .with_summary(format!("{}: {}", self.server, self.remote_name)))
    }
}

/// Build the tool set an attached server contributes, honouring `allowed_tools`.
pub fn tools_for_server(client: &Arc<McpClient>, config: &McpServerConfig) -> Vec<Arc<dyn Tool>> {
    client
        .tools()
        .iter()
        .filter(|def| config.allowed_tools.is_empty() || config.allowed_tools.iter().any(|a| a == &def.name))
        .map(|def| Arc::new(McpTool::new(client.clone(), &config.name, def)) as Arc<dyn Tool>)
        .collect()
}

/// Turn a `tools/call` result into text for the model. Returns `(text, is_error)`.
pub fn flatten_call_result(result: &Value) -> (String, bool) {
    let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
    let mut parts: Vec<String> = Vec::new();
    if let Some(items) = result.get("content").and_then(Value::as_array) {
        for item in items {
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
            match kind {
                "text" => parts.push(item.get("text").and_then(Value::as_str).unwrap_or("").to_owned()),
                "image" | "audio" => {
                    let mime = item.get("mimeType").and_then(Value::as_str).unwrap_or("application/octet-stream");
                    let size = item.get("data").and_then(Value::as_str).map(str::len).unwrap_or(0);
                    parts.push(format!("[{kind} {mime}, {size} base64 bytes]"));
                }
                "resource" => {
                    let resource = item.get("resource").cloned().unwrap_or(Value::Null);
                    let uri = resource.get("uri").and_then(Value::as_str).unwrap_or("");
                    match resource.get("text").and_then(Value::as_str) {
                        Some(text) => parts.push(format!("[resource {uri}]\n{text}")),
                        None => parts.push(format!("[resource {uri} (binary)]")),
                    }
                }
                "resource_link" => {
                    parts.push(format!("[link {}]", item.get("uri").and_then(Value::as_str).unwrap_or("")));
                }
                _ => parts.push(item.to_string()),
            }
        }
    }
    if parts.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        parts.push(serde_json::to_string_pretty(structured).unwrap_or_default());
    }
    (parts.join("\n").trim().to_owned(), is_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_mixed_content() {
        let result = serde_json::json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "image", "data": "abcd", "mimeType": "image/png"},
                {"type": "resource", "resource": {"uri": "file:///x.txt", "text": "body"}}
            ]
        });
        let (text, err) = flatten_call_result(&result);
        assert!(!err);
        assert_eq!(text, "hello\n[image image/png, 4 base64 bytes]\n[resource file:///x.txt]\nbody");
        let (text, err) = flatten_call_result(&serde_json::json!({"isError": true, "content": [{"type":"text","text":"boom"}]}));
        assert!(err && text == "boom");
        let (text, _) = flatten_call_result(&serde_json::json!({"structuredContent": {"a": 1}}));
        assert!(text.contains("\"a\": 1"));
    }
}
