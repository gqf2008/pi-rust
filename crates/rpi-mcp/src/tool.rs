//! `AgentTool` adapter: one native ki tool per discovered MCP tool.
//!
//! The schema is the server's `inputSchema`, passed through untouched; the name
//! is namespaced `{server}__{tool}` so an MCP tool can never silently shadow a
//! built-in or plugin tool (a residual collision is dropped by the caller with a
//! warning — see `rpi-cli`'s session build).
//!
//! Result mapping follows the retired adapter:
//! * text blocks become text;
//! * image blocks that carry both `data` and `mimeType` become a native image
//!   content block;
//! * every other block (audio, embedded resource, resource link, …) is
//!   stringified into text rather than dropped;
//! * `structuredContent`, when present, is appended as text;
//! * `isError: true` becomes a **tool error** — never a silent success.

use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use rpi_agent::{AgentError, AgentTool};
use rpi_ai::types::{ImageContent, ImageContentType, Schema, Tool};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::manager::{call_tool, CallError, DiscoveredTool, ServerHandle};

/// An `AgentTool` backed by one tool on one MCP server.
pub(crate) struct McpToolAdapter {
    handle: Arc<ServerHandle>,
    bare: String,
    schema: Tool,
}

impl McpToolAdapter {
    pub(crate) fn new(handle: Arc<ServerHandle>, tool: &DiscoveredTool) -> Self {
        Self {
            handle,
            bare: tool.bare.clone(),
            schema: Tool {
                name: tool.qualified.clone(),
                description: tool.description.clone(),
                parameters: Schema(tool.schema.clone()),
                constrained_sampling: None,
            },
        }
    }
}

#[async_trait]
impl AgentTool for McpToolAdapter {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        &self.schema.name
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        if signal.is_cancelled() {
            return Err(AgentError::Abort);
        }
        // MCP `arguments` is an object or absent; any other shape is a malformed
        // call we can reject locally instead of putting it on the wire.
        let arguments = match params {
            Value::Null => None,
            Value::Object(map) => Some(map),
            other => {
                return Err(AgentError::Tool(format!(
                    "{} expects a JSON object argument, got {}",
                    self.schema.name,
                    type_name(&other)
                )))
            }
        };

        let result = call_tool(&self.handle, &self.bare, arguments, &signal).await;
        let value = match result {
            Ok(value) => value,
            Err(CallError::Cancelled) => return Err(AgentError::Abort),
            Err(CallError::Failed(message)) => {
                return Err(AgentError::Tool(format!("{}: {message}", self.schema.name)))
            }
        };
        map_result(&self.schema.name, &value)
    }
}

/// Translate a serialized `CallToolResult` into an `AgentToolResult` (or a tool
/// error when the server flagged `isError`).
fn map_result(qualified: &str, value: &Value) -> Result<AgentToolResult, AgentError> {
    let mut content = Vec::new();
    if let Some(items) = value.get("content").and_then(Value::as_array) {
        for item in items {
            map_content_block(&mut content, item);
        }
    }
    if let Some(structured) = value.get("structuredContent") {
        if !structured.is_null() {
            content.push(TextContentOrImage::text(format!(
                "structuredContent:\n{}",
                pretty(structured)
            )));
        }
    }

    let is_error = value
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if is_error {
        // Errors travel as text through `AgentError::Tool`; an image in an error
        // result is described rather than dropped.
        let text = flatten_text(&content);
        return Err(AgentError::Tool(if text.is_empty() {
            format!("{qualified} reported failure with no content")
        } else {
            text
        }));
    }

    if content.is_empty() {
        content.push(TextContentOrImage::text("(no content returned)"));
    }
    Ok(AgentToolResult {
        content,
        details: Value::Null,
        usage: None,
        added_tool_names: Vec::new(),
        terminate: false,
    })
}

fn map_content_block(out: &mut Vec<TextContentOrImage>, item: &Value) {
    match item.get("type").and_then(Value::as_str) {
        Some("text") => match item.get("text").and_then(Value::as_str) {
            Some(text) => out.push(TextContentOrImage::text(text)),
            None => out.push(stringified(item)),
        },
        Some("image") => {
            let data = item.get("data").and_then(Value::as_str);
            let mime = item.get("mimeType").and_then(Value::as_str);
            match (data, mime) {
                (Some(data), Some(mime_type)) => {
                    out.push(TextContentOrImage::Image(ImageContent {
                        kind: ImageContentType,
                        data: data.to_string(),
                        mime_type: mime_type.to_string(),
                    }))
                }
                // Missing data/mimeType: keep the block visible as text rather
                // than fabricating an image the provider cannot attach.
                _ => out.push(stringified(item)),
            }
        }
        // audio / resource / resource_link / future block types.
        _ => out.push(stringified(item)),
    }
}

fn stringified(item: &Value) -> TextContentOrImage {
    TextContentOrImage::text(
        serde_json::to_string(item).unwrap_or_else(|_| "<unserializable>".into()),
    )
}

/// Collapse content to text for an error message. Images become an honest
/// one-line description (base64 length + mime), never an "attached" placeholder.
fn flatten_text(content: &[TextContentOrImage]) -> String {
    let mut parts = Vec::new();
    for block in content {
        match block {
            TextContentOrImage::Text(text) => parts.push(text.text.clone()),
            TextContentOrImage::Image(image) => parts.push(format!(
                "[image {}, {} base64 bytes]",
                image.mime_type,
                image.data.len()
            )),
        }
    }
    parts.join("\n")
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_blocks_map_to_text() {
        let result = map_result(
            "srv__echo",
            &json!({ "content": [{ "type": "text", "text": "hello" }] }),
        )
        .unwrap();
        assert_eq!(flatten_text(&result.content), "hello");
    }

    #[test]
    fn image_blocks_with_data_and_mime_map_to_native_images() {
        let result = map_result(
            "srv__img",
            &json!({ "content": [{ "type": "image", "data": "AAAA", "mimeType": "image/png" }] }),
        )
        .unwrap();
        match &result.content[0] {
            TextContentOrImage::Image(image) => {
                assert_eq!(image.data, "AAAA");
                assert_eq!(image.mime_type, "image/png");
            }
            other => panic!("expected image, got {other:?}"),
        }
    }

    #[test]
    fn image_without_data_is_stringified_not_an_attached_placeholder() {
        let result = map_result(
            "srv__img",
            &json!({ "content": [{ "type": "image", "mimeType": "image/png" }] }),
        )
        .unwrap();
        match &result.content[0] {
            TextContentOrImage::Text(text) => assert!(text.text.contains("image")),
            other => panic!("expected text fallback, got {other:?}"),
        }
    }

    #[test]
    fn other_blocks_are_stringified_not_dropped() {
        let result = map_result(
            "srv__res",
            &json!({ "content": [{ "type": "resource", "resource": { "uri": "file:///x" } }] }),
        )
        .unwrap();
        match &result.content[0] {
            TextContentOrImage::Text(text) => assert!(text.text.contains("file:///x")),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn structured_content_is_appended_as_text() {
        let result = map_result(
            "srv__s",
            &json!({
                "content": [{ "type": "text", "text": "ok" }],
                "structuredContent": { "answer": 42 }
            }),
        )
        .unwrap();
        let text = flatten_text(&result.content);
        assert!(text.contains("ok"));
        assert!(text.contains("structuredContent"));
        assert!(text.contains("42"));
    }

    #[test]
    fn is_error_becomes_a_tool_error() {
        let error = map_result(
            "srv__boom",
            &json!({ "content": [{ "type": "text", "text": "boom" }], "isError": true }),
        )
        .unwrap_err();
        assert!(matches!(error, AgentError::Tool(_)));
        assert!(error.to_string().contains("boom"));
    }

    #[test]
    fn empty_successful_result_is_not_an_empty_message() {
        let result = map_result("srv__empty", &json!({ "content": [] })).unwrap();
        assert_eq!(result.content.len(), 1);
    }
}
