//! Pure projection from ki's process-local [`AgentEvent`] stream onto ACP
//! `session/update` payloads. Deltas only (proposal §4.5): Start/End envelope
//! events and `ToolExecutionUpdate` are not projected, and empty deltas are
//! dropped — sending more than the mapping here is new wire behavior.

use agent_client_protocol::schema::v1::{
    ContentChunk, SessionUpdate, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind,
};
use rpi_agent::events::AgentEvent;
use rpi_agent::types::{AgentToolResult, TextContentOrImage};
use rpi_ai::types::AssistantMessageEvent;

/// Project one event into zero or more `session/update` payloads, in wire order.
pub(crate) fn project(event: &AgentEvent) -> Vec<SessionUpdate> {
    match event {
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => match assistant_message_event {
            AssistantMessageEvent::TextDelta { delta, .. } => text_chunk(delta)
                .map(SessionUpdate::AgentMessageChunk)
                .into_iter()
                .collect(),
            AssistantMessageEvent::ThinkingDelta { delta, .. } => text_chunk(delta)
                .map(SessionUpdate::AgentThoughtChunk)
                .into_iter()
                .collect(),
            // Start/End and ToolCallDelta stay internal.
            _ => Vec::new(),
        },
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => vec![
            SessionUpdate::ToolCall(
                ToolCall::new(tool_call_id.clone(), title_for_tool(tool_name))
                    .kind(kind_for_tool(tool_name))
                    .status(ToolCallStatus::Pending)
                    .raw_input(Some(args.clone())),
            ),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                tool_call_id.clone(),
                ToolCallUpdateFields::new().status(ToolCallStatus::InProgress),
            )),
        ],
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            result,
            is_error,
            ..
        } => {
            let text = tool_result_text(result);
            vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                tool_call_id.clone(),
                ToolCallUpdateFields::new()
                    .status(if *is_error {
                        ToolCallStatus::Failed
                    } else {
                        ToolCallStatus::Completed
                    })
                    .content((!text.is_empty()).then(|| vec![text.into()])),
            ))]
        }
        // AgentStart/AgentEnd/TurnStart/TurnEnd/MessageStart/MessageEnd are
        // turn-envelope events; `ToolExecutionUpdate` is deliberately not
        // projected (abb's "more sends = new behavior" rule).
        _ => Vec::new(),
    }
}

fn text_chunk(delta: &str) -> Option<ContentChunk> {
    (!delta.is_empty()).then(|| ContentChunk::new(delta.into()))
}

/// The result text: joined text blocks; images have no batch-1 mapping.
fn tool_result_text(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|item| match item {
            TextContentOrImage::Text(text) => Some(text.text.as_str()),
            TextContentOrImage::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// ki tool names → ACP tool kinds (the spirit of rpi-package rpi-acp's
/// `kind_for_tool`, matched loosely so plugin tools land somewhere sensible).
pub(crate) fn kind_for_tool(name: &str) -> ToolKind {
    let lower = name.to_ascii_lowercase();
    if lower.contains("bash") || lower.contains("exec") {
        ToolKind::Execute
    } else if lower.starts_with("edit") || lower.starts_with("write") {
        ToolKind::Edit
    } else if lower.starts_with("read") || lower.starts_with("ls") || lower.starts_with("image") {
        ToolKind::Read
    } else if lower.starts_with("grep") || lower.starts_with("find") {
        ToolKind::Search
    } else if lower.starts_with("think") {
        ToolKind::Think
    } else if lower.starts_with("fetch") || lower.starts_with("web") {
        ToolKind::Fetch
    } else {
        ToolKind::Other
    }
}

/// rpi-acp builds per-tool detail titles ("Read file.rs"); batch 1 keeps the
/// raw tool name — stable without a per-tool table.
pub(crate) fn title_for_tool(name: &str) -> String {
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_ai::types::{Api, AssistantMessage};
    use std::sync::Arc;

    fn partial() -> Arc<AssistantMessage> {
        Arc::new(AssistantMessage::empty(Api::Faux, "faux", "faux-1", 0))
    }

    fn text_delta(delta: &str) -> AgentEvent {
        AgentEvent::MessageUpdate {
            message: partial(),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: delta.to_string(),
                partial: partial(),
            },
        }
    }

    fn thinking_delta(delta: &str) -> AgentEvent {
        AgentEvent::MessageUpdate {
            message: partial(),
            assistant_message_event: AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: delta.to_string(),
                partial: partial(),
            },
        }
    }

    fn tool_start(name: &str) -> AgentEvent {
        AgentEvent::ToolExecutionStart {
            tool_call_id: "t1".to_string(),
            tool_name: name.to_string(),
            args: serde_json::json!({"path": "a.rs"}),
        }
    }

    fn tool_end(is_error: bool) -> AgentEvent {
        AgentEvent::ToolExecutionEnd {
            tool_call_id: "t1".to_string(),
            tool_name: "bash".to_string(),
            result: AgentToolResult::text("did the thing"),
            is_error,
        }
    }

    #[test]
    fn text_delta_projects_agent_message_chunk() {
        let updates = project(&text_delta("hello "));
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                agent_client_protocol::schema::v1::ContentBlock::Text(text) => {
                    assert_eq!(text.text, "hello ");
                }
                other => panic!("expected text content, got {other:?}"),
            },
            other => panic!("expected agent message chunk, got {other:?}"),
        }
    }

    #[test]
    fn thinking_delta_projects_agent_thought_chunk() {
        let updates = project(&thinking_delta("hmm"));
        assert_eq!(updates.len(), 1);
        assert!(matches!(updates[0], SessionUpdate::AgentThoughtChunk(_)));
    }

    #[test]
    fn empty_delta_is_dropped() {
        assert!(project(&text_delta("")).is_empty());
        assert!(project(&thinking_delta("")).is_empty());
    }

    #[test]
    fn non_delta_message_events_are_not_projected() {
        let start = AgentEvent::MessageUpdate {
            message: partial(),
            assistant_message_event: AssistantMessageEvent::Start { partial: partial() },
        };
        assert!(project(&start).is_empty());

        let tool_delta = AgentEvent::MessageUpdate {
            message: partial(),
            assistant_message_event: AssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: "{\"x\":1}".to_string(),
                partial: partial(),
            },
        };
        assert!(project(&tool_delta).is_empty());
    }

    #[test]
    fn tool_start_projects_pending_call_then_in_progress_update() {
        let updates = project(&tool_start("bash"));
        assert_eq!(updates.len(), 2);
        match &updates[0] {
            SessionUpdate::ToolCall(call) => {
                assert_eq!(call.tool_call_id, "t1".into());
                assert_eq!(call.title, "bash");
                assert_eq!(call.kind, ToolKind::Execute);
                assert_eq!(call.status, ToolCallStatus::Pending);
                assert_eq!(call.raw_input, Some(serde_json::json!({"path": "a.rs"})));
            }
            other => panic!("expected tool_call, got {other:?}"),
        }
        match &updates[1] {
            SessionUpdate::ToolCallUpdate(update) => {
                assert_eq!(update.tool_call_id, "t1".into());
                assert_eq!(update.fields.status, Some(ToolCallStatus::InProgress));
            }
            other => panic!("expected tool_call_update, got {other:?}"),
        }
    }

    #[test]
    fn tool_end_projects_completed_update_with_result_text() {
        let updates = project(&tool_end(false));
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            SessionUpdate::ToolCallUpdate(update) => {
                assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
                let content = update.fields.content.as_ref().expect("content");
                assert_eq!(content.len(), 1);
                let agent_client_protocol::schema::v1::ToolCallContent::Content(content) =
                    &content[0]
                else {
                    panic!("expected plain content");
                };
                match &content.content {
                    agent_client_protocol::schema::v1::ContentBlock::Text(text) => {
                        assert_eq!(text.text, "did the thing");
                    }
                    other => panic!("expected text, got {other:?}"),
                }
            }
            other => panic!("expected tool_call_update, got {other:?}"),
        }
    }

    #[test]
    fn tool_end_error_projects_failed_update() {
        let updates = project(&tool_end(true));
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            SessionUpdate::ToolCallUpdate(update) => {
                assert_eq!(update.fields.status, Some(ToolCallStatus::Failed));
            }
            other => panic!("expected tool_call_update, got {other:?}"),
        }
    }

    #[test]
    fn tool_execution_update_is_not_projected() {
        let event = AgentEvent::ToolExecutionUpdate {
            tool_call_id: "t1".to_string(),
            tool_name: "bash".to_string(),
            args: serde_json::json!({}),
            partial_result: Arc::new(AgentToolResult::text("partial")),
        };
        assert!(project(&event).is_empty());
    }

    #[test]
    fn envelope_events_are_not_projected() {
        assert!(project(&AgentEvent::AgentStart).is_empty());
        assert!(project(&AgentEvent::AgentEnd { messages: vec![] }).is_empty());
        assert!(project(&AgentEvent::TurnStart).is_empty());
    }

    #[test]
    fn kind_for_tool_mapping_table() {
        let cases: &[(&str, ToolKind)] = &[
            ("bash", ToolKind::Execute),
            ("websearch_execute", ToolKind::Execute),
            ("edit", ToolKind::Edit),
            ("write", ToolKind::Edit),
            ("read", ToolKind::Read),
            ("ls", ToolKind::Read),
            ("image", ToolKind::Read),
            ("grep", ToolKind::Search),
            ("find", ToolKind::Search),
            ("think", ToolKind::Think),
            ("fetch", ToolKind::Fetch),
            ("webfetch", ToolKind::Fetch),
            ("todo_write", ToolKind::Other),
        ];
        for (name, expected) in cases {
            assert_eq!(kind_for_tool(name), *expected, "kind for {name}");
        }
    }

    #[test]
    fn title_for_tool_is_the_tool_name() {
        assert_eq!(title_for_tool("bash"), "bash");
    }
}
