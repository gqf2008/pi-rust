//! `AgentTool` adapter: one native ki tool per discovered MCP tool.
//!
//! The schema is the server's `inputSchema`, passed through untouched; the name
//! is namespaced `{server}__{tool}` so an MCP tool can never silently shadow a
//! built-in or plugin tool (a residual collision is dropped by the caller with a
//! warning — see `kiqi-cli`'s session build).
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
use kiqi_agent::types::{
    AgentToolResult, TextContentOrImage, ToolExecutionMode, ToolResultPartial,
};
use kiqi_agent::{AgentError, AgentTool};
use kiqi_ai::types::{ImageContent, ImageContentType, Schema, Tool};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::manager::{
    call_tool, read_resource, CallError, DiscoveredTool, ServerHandle, MAX_RESOURCE_TEXT,
    READ_RESOURCE_SUFFIX, TOOL_NAME_SEPARATOR, WATCH_RESOURCE_SUFFIX,
};

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

/// An `AgentTool` reading one resource on one MCP server — the on-demand read
/// behind the light-notice product semantic: a `notifications/resources/updated`
/// notice NEVER auto-reads; the model (or the host) reads here when it wants
/// the content.
pub(crate) struct McpResourceToolAdapter {
    handle: Arc<ServerHandle>,
    schema: Tool,
}

impl McpResourceToolAdapter {
    /// `description` carries the server's resource + template list (refreshed
    /// on `resources/list_changed` by the boundary re-merge).
    pub(crate) fn new(handle: Arc<ServerHandle>, description: String) -> Self {
        let name = format!(
            "{}{TOOL_NAME_SEPARATOR}{READ_RESOURCE_SUFFIX}",
            handle.server_name()
        );
        Self {
            schema: Tool {
                name,
                description,
                parameters: Schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "uri": { "type": "string", "description": "The uri of the resource to read" }
                    },
                    "required": ["uri"]
                })),
                constrained_sampling: None,
            },
            handle,
        }
    }
}

#[async_trait]
impl AgentTool for McpResourceToolAdapter {
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
        let uri = match params.get("uri").and_then(Value::as_str) {
            Some(uri) if !uri.trim().is_empty() => uri.to_string(),
            _ => {
                return Err(AgentError::Tool(format!(
                    "{} expects a non-empty string `uri` argument",
                    self.schema.name
                )))
            }
        };
        let value = read_resource(&self.handle, &uri, &signal).await;
        let value = match value {
            Ok(value) => value,
            Err(CallError::Cancelled) => return Err(AgentError::Abort),
            Err(CallError::Failed(message)) => {
                return Err(AgentError::Tool(format!("{}: {message}", self.schema.name)))
            }
        };
        map_resource_result(&self.schema.name, &value)
    }
}

/// An `AgentTool` managing this server's resource subscriptions. Together with
/// `{server}__read_resource` it closes the loop: discover resources from the
/// read tool's description, `start` watching one, receive signals, then read on
/// demand. A model-started watch lives for this session only; durable watches
/// belong in the server's `subscribe` config.
pub(crate) struct McpResourceWatchToolAdapter {
    handle: Arc<ServerHandle>,
    schema: Tool,
}

impl McpResourceWatchToolAdapter {
    pub(crate) fn new(handle: Arc<ServerHandle>) -> Self {
        let name = format!(
            "{}{TOOL_NAME_SEPARATOR}{WATCH_RESOURCE_SUFFIX}",
            handle.server_name()
        );
        let read_name = format!(
            "{}{TOOL_NAME_SEPARATOR}{READ_RESOURCE_SUFFIX}",
            handle.server_name()
        );
        Self {
            schema: Tool {
                name,
                description: format!(
                    "Watch resources on this server. `start` subscribes to a uri so updates arrive as MCP signals; `stop` unsubscribes; `list` shows current subscriptions. Read content with `{read_name}`. A watch started here lasts for this session only; durable watches belong in the server's `subscribe` config."
                ),
                parameters: Schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["start", "stop", "list"],
                            "description": "start = subscribe, stop = unsubscribe, list = show current subscriptions"
                        },
                        "uri": {
                            "type": "string",
                            "description": "Resource uri (required for start and stop)"
                        }
                    },
                    "required": ["action"]
                })),
                constrained_sampling: None,
            },
            handle,
        }
    }
}

#[async_trait]
impl AgentTool for McpResourceWatchToolAdapter {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        &self.schema.name
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        // Subscription state is a read-modify-write on the shared map; keep
        // start/stop calls from interleaving.
        ToolExecutionMode::Sequential
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
        let action = params
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        match action {
            "list" => {
                let uris = self.handle.subscription_uris();
                let text = if uris.is_empty() {
                    "No resource subscriptions.".to_string()
                } else {
                    let mut text = format!("Watching {} resource(s):\n", uris.len());
                    for uri in uris {
                        text.push_str(&format!("- {uri}\n"));
                    }
                    text
                };
                Ok(AgentToolResult::text(text))
            }
            "start" | "stop" => {
                let uri = params
                    .get("uri")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|uri| !uri.is_empty())
                    .ok_or_else(|| {
                        AgentError::Tool(format!(
                            "{}: `uri` is required for `{action}`",
                            self.schema.name
                        ))
                    })?;
                if action == "start" {
                    self.handle.subscribe_uri(uri).await.map_err(|error| {
                        AgentError::Tool(format!("{}: {error}", self.schema.name))
                    })?;
                    Ok(AgentToolResult::text(format!(
                        "Watching {uri}. Updates will arrive as MCP signals; read the content with the server's read_resource tool."
                    )))
                } else {
                    self.handle.unsubscribe_uri(uri).await.map_err(|error| {
                        AgentError::Tool(format!("{}: {error}", self.schema.name))
                    })?;
                    Ok(AgentToolResult::text(format!("Stopped watching {uri}.")))
                }
            }
            other => Err(AgentError::Validation(format!(
                "{} expects action start|stop|list (got {other:?})",
                self.schema.name
            ))),
        }
    }
}

/// Push one text block against the read result's remaining text budget,
/// truncating at it with the same char-boundary-safe marker as
/// [`bound_resource_text`](crate::manager). The budget makes the WHOLE read —
/// not each block — respect `MAX_RESOURCE_TEXT`, as upstream's flattened
/// `resource_text` did.
fn push_bounded_text(content: &mut Vec<TextContentOrImage>, mut body: String, budget: &mut usize) {
    if body.len() > *budget {
        let mut end = *budget;
        while end > 0 && !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
        body.push_str("\n[truncated]");
        *budget = 0;
    } else {
        *budget -= body.len();
    }
    content.push(TextContentOrImage::text(body));
}

/// Map a serialized `ReadResourceResult` into content blocks — the same
/// text/image/other policy as [`map_result`] applied to resource contents —
/// bounding the whole read's text at [`MAX_RESOURCE_TEXT`](crate::manager) with
/// the truncation marker (the adapter's read bound; a blob must not reach the
/// model whole).
///
/// When a read returns several contents, each is preceded by a `{uri}:` line so
/// the model can tell them apart; a single-content read is just the body.
fn map_resource_result(_qualified: &str, value: &Value) -> Result<AgentToolResult, AgentError> {
    let empty = Vec::new();
    let items = value
        .get("contents")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    if items.is_empty() {
        return Ok(AgentToolResult {
            content: vec![TextContentOrImage::text("(no content returned)")],
            details: Value::Null,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
        });
    }
    let multiple = items.len() > 1;
    let mut content = Vec::new();
    // The whole flattened read is bounded once at MAX_RESOURCE_TEXT (upstream's
    // `resource_text` -> `bound_resource_text` shape), so a multi-content read
    // can never hand the model more than the cap regardless of the content
    // count. Image blobs carry no text budget (the native-image mapping below
    // inlines pixels, not text).
    let mut budget = MAX_RESOURCE_TEXT;
    for item in items {
        if budget == 0 {
            break;
        }
        let uri = item.get("uri").and_then(Value::as_str).unwrap_or("");
        let mime = item.get("mimeType").and_then(Value::as_str);
        if multiple {
            content.push(TextContentOrImage::text(format!("{uri}:")));
        }
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            push_bounded_text(&mut content, text.to_string(), &mut budget);
        } else if let Some(blob) = item.get("blob").and_then(Value::as_str) {
            if mime.is_some_and(|mime| mime.starts_with("image/")) {
                // Same native-image mapping a tool result gets.
                content.push(TextContentOrImage::Image(ImageContent {
                    kind: ImageContentType,
                    data: blob.to_string(),
                    mime_type: mime.expect("checked starts_with above").to_string(),
                }));
            } else {
                push_bounded_text(
                    &mut content,
                    format!(
                        "[blob, {} base64 chars{}]",
                        blob.len(),
                        mime.map(|mime| format!(", {mime}")).unwrap_or_default()
                    ),
                    &mut budget,
                );
            }
        } else {
            push_bounded_text(
                &mut content,
                serde_json::to_string(item).unwrap_or_else(|_| "<unserializable>".into()),
                &mut budget,
            );
        }
    }
    Ok(AgentToolResult {
        content,
        details: Value::Null,
        usage: None,
        added_tool_names: Vec::new(),
        terminate: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::MAX_RESOURCE_TEXT;
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

    #[test]
    fn resource_text_maps_to_text_and_oversize_is_bounded() {
        let result = map_resource_result(
            "srv__read_resource",
            &json!({ "contents": [{ "uri": "file:///a.md", "mimeType": "text/markdown", "text": "hello doc" }] }),
        )
        .unwrap();
        assert_eq!(result.content.len(), 1);
        assert_eq!(flatten_text(&result.content), "hello doc");

        let huge = "x".repeat(MAX_RESOURCE_TEXT + 100);
        let result = map_resource_result(
            "srv__read_resource",
            &json!({ "contents": [{ "uri": "file:///big.txt", "text": huge }] }),
        )
        .unwrap();
        let text = flatten_text(&result.content);
        assert!(text.ends_with("\n[truncated]"));
        assert!(text.len() < MAX_RESOURCE_TEXT + 100);
    }

    #[test]
    fn image_blob_maps_to_a_native_image_other_blobs_are_described() {
        let result = map_resource_result(
            "srv__read_resource",
            &json!({ "contents": [
                { "uri": "file:///pic.png", "mimeType": "image/png", "blob": "AAAA" },
                { "uri": "file:///data.bin", "mimeType": "application/octet-stream", "blob": "QUJD" }
            ] }),
        )
        .unwrap();
        let blocks = &result.content;
        // Provenance line + image for the first, provenance + descriptor for the second.
        assert_eq!(blocks.len(), 4, "got {blocks:?}");
        assert!(
            matches!(&blocks[1], TextContentOrImage::Image(image) if image.data == "AAAA" && image.mime_type == "image/png")
        );
        let text = flatten_text(blocks);
        assert!(
            text.contains("[blob, 4 base64 chars, application/octet-stream]"),
            "got {text:?}"
        );
    }

    #[test]
    fn single_content_read_has_no_uri_echo_and_empty_read_says_so() {
        let single = map_resource_result(
            "srv__read_resource",
            &json!({ "contents": [{ "uri": "file:///a.md", "text": "body" }] }),
        )
        .unwrap();
        assert_eq!(flatten_text(&single.content), "body");

        let empty = map_resource_result("srv__read_resource", &json!({ "contents": [] })).unwrap();
        assert_eq!(flatten_text(&empty.content), "(no content returned)");
    }

    #[test]
    fn multi_content_read_is_bounded_whole_not_per_block() {
        // Upstream bounds the FLATTENED read once at MAX_RESOURCE_TEXT; two
        // ~15k blocks must therefore deliver ~20k total, not ~30k.
        let chunk = "x".repeat(15_000);
        let result = map_resource_result(
            "srv__read_resource",
            &json!({ "contents": [
                { "uri": "file:///a", "text": chunk },
                { "uri": "file:///b", "text": chunk }
            ] }),
        )
        .unwrap();
        let text = flatten_text(&result.content);
        assert!(
            text.len() < 2 * 15_000,
            "the whole read must be bounded, not each block: {} chars",
            text.len()
        );
        assert!(text.starts_with("file:///a:"));
        assert!(text.ends_with("\n[truncated]"));
    }
}
