//! The `signals` tool: the agent's durable handle on MCP resource signals.
//!
//! The host records every signal that was allowed to wake the session in the
//! attention inbox (`crates/kiqi-cli/src/signal_host.rs`). This tool lets the
//! agent choose a disposition instead of leaving the notice implicit:
//!
//! * `list` / `status` — inspect what is still open,
//! * `defer` — handle it later (kept, with an optional note),
//! * `resolve` — done; remove it,
//! * `dismiss` — judged irrelevant; remove it (optional reason),
//! * `mute` / `unmute` — stop/resume signal-triggered wakes for this session.
//!
//! The inbox is a session value, so dispositions survive resume. The tool does
//! not grant any new authority: reading a resource or acting on it still goes
//! through the normal tools and permission policy.

use std::sync::Arc;

use async_trait::async_trait;
use kiqi_agent::agent_tool::AgentTool;
use kiqi_agent::error::AgentError;
use kiqi_agent::types::{AgentToolResult, ToolExecutionMode, ToolResultPartial};
use kiqi_ai::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::signal_host::{InboxEntry, InboxStatus, Settle, SignalHost};

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SignalsAction {
    /// Show every open signal.
    List,
    /// Show the session mute state and the number of open signals.
    Status,
    /// Keep the signal for later, optionally with a note.
    Defer,
    /// Mark the signal handled and remove it.
    Resolve,
    /// Mark the signal irrelevant and remove it, optionally with a reason.
    Dismiss,
    /// Stop signal-triggered wakes for this session.
    Mute,
    /// Resume signal-triggered wakes for this session.
    Unmute,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SignalsInput {
    pub action: SignalsAction,
    /// Inbox id from `list` (required by defer/resolve/dismiss).
    #[serde(default)]
    pub id: Option<u64>,
    /// Optional note stored on defer, or reason recorded on dismiss.
    #[serde(default)]
    pub note: Option<String>,
}

pub struct SignalsTool {
    schema: Tool,
    host: Arc<SignalHost>,
}

impl SignalsTool {
    fn new(host: Arc<SignalHost>) -> Self {
        let params = schemars::schema_for!(SignalsInput);
        Self {
            host,
            schema: Tool {
                name: "signals".to_string(),
                description: "Inspect and settle MCP resource signals recorded by the host. \
Actions: list (open signals), status (mute state + count), defer (id, note), \
resolve (id), dismiss (id, note), mute, unmute. Use defer when you decide to \
handle a signal later, resolve when it is done, and dismiss when it is not \
relevant. Reading the resource itself still uses <server>__read_resource."
                    .to_string(),
                parameters: kiqi_ai::types::Schema::new(
                    serde_json::to_value(params).unwrap_or_default(),
                ),
                constrained_sampling: None,
            },
        }
    }
}

pub fn create_signals_tool(host: Arc<SignalHost>) -> Arc<dyn AgentTool> {
    Arc::new(SignalsTool::new(host))
}

#[async_trait]
impl AgentTool for SignalsTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        "signals"
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        // The inbox is a read-modify-write session value; serializing this
        // tool keeps concurrent calls from losing a disposition.
        ToolExecutionMode::Sequential
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let input: SignalsInput = serde_json::from_value(params)
            .map_err(|error| AgentError::Validation(format!("signals input invalid: {error}")))?;
        if signal.is_cancelled() {
            return Err(AgentError::Tool("signals call cancelled".into()));
        }
        match input.action {
            SignalsAction::List => {
                let entries = self.host.inbox_entries().await.map_err(AgentError::Tool)?;
                Ok(AgentToolResult::text(format_entries(&entries)))
            }
            SignalsAction::Status => {
                let entries = self.host.inbox_entries().await.map_err(AgentError::Tool)?;
                Ok(AgentToolResult::text(format!(
                    "MCP signals: muted={} open={}",
                    self.host.session_muted(),
                    entries.len()
                )))
            }
            SignalsAction::Defer => {
                let id = require_id(input.id)?;
                let message = self
                    .host
                    .settle(id, Settle::Defer(input.note))
                    .await
                    .map_err(AgentError::Tool)?;
                Ok(AgentToolResult::text(message))
            }
            SignalsAction::Resolve => {
                let id = require_id(input.id)?;
                let message = self
                    .host
                    .settle(id, Settle::Resolve)
                    .await
                    .map_err(AgentError::Tool)?;
                Ok(AgentToolResult::text(message))
            }
            SignalsAction::Dismiss => {
                let id = require_id(input.id)?;
                let message = self
                    .host
                    .settle(id, Settle::Dismiss(input.note))
                    .await
                    .map_err(AgentError::Tool)?;
                Ok(AgentToolResult::text(message))
            }
            SignalsAction::Mute => {
                self.host.set_session_muted(true);
                if let Err(error) = self.host.persist_session_muted(true).await {
                    self.host.set_session_muted(false);
                    return Err(AgentError::Tool(error));
                }
                Ok(AgentToolResult::text(
                    "MCP signal wake is muted for this session.",
                ))
            }
            SignalsAction::Unmute => {
                self.host.set_session_muted(false);
                if let Err(error) = self.host.persist_session_muted(false).await {
                    self.host.set_session_muted(true);
                    return Err(AgentError::Tool(error));
                }
                Ok(AgentToolResult::text(
                    "MCP signal wake is enabled for this session.",
                ))
            }
        }
    }
}

fn require_id(id: Option<u64>) -> Result<u64, AgentError> {
    id.ok_or_else(|| AgentError::Validation("`id` is required for this action".to_string()))
}

fn format_entries(entries: &[InboxEntry]) -> String {
    if entries.is_empty() {
        return "MCP signal inbox is empty.".to_string();
    }
    let mut text = format!("MCP signal inbox: {} open\n", entries.len());
    for entry in entries {
        let status = match entry.status {
            InboxStatus::Pending => "pending",
            InboxStatus::Deferred => "deferred",
        };
        text.push_str(&format!(
            "- id {} [{}] {}: {}",
            entry.id, status, entry.server, entry.uri
        ));
        if let Some(note) = &entry.note {
            text.push_str(&format!(" — {note}"));
        }
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiqi_harness::session::memory::{InMemorySessionStorage, SystemClock};
    use kiqi_harness::session::session::{DefaultIdGenerator, Session};
    use kiqi_harness::session::types::SessionMetadata;
    use kiqi_mcp::{McpNotice, McpNoticeBatch};

    fn session() -> Session {
        let metadata = SessionMetadata {
            id: "s-signals-tool".to_string(),
            created_at: 0,
            parent_session_id: None,
        };
        let storage = InMemorySessionStorage::new(
            metadata,
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        );
        Session::new(Arc::new(storage), None)
    }

    fn tool() -> (Arc<dyn AgentTool>, Arc<SignalHost>) {
        let host = Arc::new(SignalHost::new());
        host.attach_session(session());
        (create_signals_tool(host.clone()), host)
    }

    async fn call(tool: &Arc<dyn AgentTool>, args: serde_json::Value) -> String {
        let result = tool
            .execute("call", args, CancellationToken::new(), Arc::new(|_| {}))
            .await
            .expect("tool call");
        result
            .content
            .iter()
            .filter_map(|block| match block {
                kiqi_agent::types::TextContentOrImage::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    #[tokio::test]
    async fn list_defer_resolve_and_status() {
        let (tool, host) = tool();
        host.add_to_inbox(&McpNoticeBatch {
            notices: vec![
                McpNotice {
                    server: "docs".into(),
                    uri: "file:///a.md".into(),
                    content: None,
                },
                McpNotice {
                    server: "code".into(),
                    uri: "mem://x".into(),
                    content: None,
                },
            ],
            suppressed: 0,
        })
        .await
        .unwrap();
        let listed = call(&tool, serde_json::json!({ "action": "list" })).await;
        assert!(listed.contains("id 1"), "got {listed:?}");
        assert!(listed.contains("docs: file:///a.md"), "got {listed:?}");

        let deferred = call(
            &tool,
            serde_json::json!({ "action": "defer", "id": 1, "note": "later" }),
        )
        .await;
        assert!(deferred.contains("Deferred signal 1"), "got {deferred:?}");
        let listed = call(&tool, serde_json::json!({ "action": "list" })).await;
        assert!(listed.contains("[deferred]"), "got {listed:?}");
        assert!(listed.contains("— later"), "got {listed:?}");

        let resolved = call(&tool, serde_json::json!({ "action": "resolve", "id": 2 })).await;
        assert!(resolved.contains("Resolved signal 2"), "got {resolved:?}");
        let status = call(&tool, serde_json::json!({ "action": "status" })).await;
        assert!(status.contains("muted=false open=1"), "got {status:?}");
    }

    #[tokio::test]
    async fn mute_is_persisted_and_toggles_the_wake_state() {
        let (tool, host) = tool();
        let message = call(&tool, serde_json::json!({ "action": "mute" })).await;
        assert!(message.contains("muted"), "got {message:?}");
        assert!(host.session_muted());
        let message = call(&tool, serde_json::json!({ "action": "unmute" })).await;
        assert!(message.contains("enabled"), "got {message:?}");
        assert!(!host.session_muted());
    }

    #[tokio::test]
    async fn missing_id_is_a_validation_error() {
        let (tool, _) = tool();
        let error = tool
            .execute(
                "call",
                serde_json::json!({ "action": "resolve" }),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("`id` is required"),
            "got {error}"
        );
    }
}
