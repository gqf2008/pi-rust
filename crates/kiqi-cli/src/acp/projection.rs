//! Pure projection from ki's process-local [`AgentEvent`] stream onto ACP
//! `session/update` payloads. Deltas only (proposal §4.5): Start/End envelope
//! events and `ToolExecutionUpdate` are not projected, and empty deltas are
//! dropped — sending more than the mapping here is new wire behavior.

use agent_client_protocol::schema::v1::{
    ContentChunk, SessionUpdate, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind,
};
use kiqi_agent::events::AgentEvent;
use kiqi_agent::message::AgentMessage;
use kiqi_agent::types::{AgentToolResult, TextContentOrImage};
use kiqi_ai::types::{AssistantMessageEvent, Content};
use kiqi_harness::session::types::Entry;

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

/// ki tool names → ACP tool kinds (the spirit of rpi-package kiqi-acp's
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

/// kiqi-acp builds per-tool detail titles ("Read file.rs"); batch 1 keeps the
/// raw tool name — stable without a per-tool table.
pub(crate) fn title_for_tool(name: &str) -> String {
    name.to_string()
}

// ---- session/load replay (persisted entries → updates) ----
//
// The expected/practiced replay set is the live projection set plus
// `UserMessageChunk` (kiqi-acp `history.rs` semantics); there is no protocol
// whitelist, but anything outside this mapping has never been sent.

/// Project one persisted entry into zero or more `session/update` payloads,
/// oldest-first branch order. Only message entries replay: model changes,
/// thinking levels, active-tool sets, branch summaries and custom entries are
/// ki-internal bookkeeping with no ACP mapping. Compaction entries in
/// particular carry no replay shape, so a loaded session shows the full
/// retained transcript — the summary-vs-tail distinction is client-invisible
/// (documented compaction loss, matching kiqi-acp).
pub(crate) fn replay_entry(entry: &Entry) -> Vec<SessionUpdate> {
    let Entry::Message(message_entry) = entry else {
        return Vec::new();
    };
    replay_message(&message_entry.message)
}

fn replay_message(message: &AgentMessage) -> Vec<SessionUpdate> {
    match message {
        AgentMessage::User(user) => match &user.content {
            kiqi_ai::types::UserContent::Text(text) => user_chunk(text).into_iter().collect(),
            kiqi_ai::types::UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|content| match content {
                    Content::Text(text) => user_chunk(&text.text),
                    // Thinking/tool-call/image blocks have no user-side replay
                    // mapping (they never originate from the user channel).
                    _ => None,
                })
                .collect(),
        },
        AgentMessage::Assistant(assistant) => assistant
            .content
            .iter()
            .filter_map(|content| match content {
                Content::Text(text) => {
                    chunk_if_nonempty(text.text.as_str()).map(SessionUpdate::AgentMessageChunk)
                }
                Content::Thinking(thinking) => chunk_if_nonempty(thinking.thinking.as_str())
                    .map(SessionUpdate::AgentThoughtChunk),
                Content::ToolCall(call) => Some(SessionUpdate::ToolCall(
                    ToolCall::new(call.id.clone(), title_for_tool(&call.name))
                        .kind(kind_for_tool(&call.name))
                        .status(ToolCallStatus::Pending)
                        .raw_input(Some(call.arguments.clone())),
                )),
                Content::Image(_) => None,
            })
            .collect(),
        AgentMessage::ToolResult(result) => {
            let text = replay_tool_result_text(&result.content);
            vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                result.tool_call_id.clone(),
                ToolCallUpdateFields::new()
                    .status(if result.is_error {
                        ToolCallStatus::Failed
                    } else {
                        ToolCallStatus::Completed
                    })
                    .content((!text.is_empty()).then(|| vec![text.into()])),
            ))]
        }
        // Custom message kinds are ki-internal (run resumes, notices); kiqi-acp
        // replays them the same way: not at all.
        AgentMessage::Custom(_) => Vec::new(),
    }
}

fn user_chunk(text: &str) -> Option<SessionUpdate> {
    chunk_if_nonempty(text).map(SessionUpdate::UserMessageChunk)
}

/// The empty-delta discipline extends to replay: an empty text/thinking block
/// carries no information, so it drops instead of shipping an empty chunk.
fn chunk_if_nonempty(text: &str) -> Option<ContentChunk> {
    (!text.is_empty()).then(|| ContentChunk::new(text.into()))
}

/// The tool result's text: joined text blocks; images have no replay mapping.
fn replay_tool_result_text(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|item| match item {
            Content::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiqi_ai::types::{Api, AssistantMessage};
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

    // ---- session/load replay: entry → update mapping table ----

    use kiqi_ai::types::ToolResultMessage;
    use kiqi_harness::session::types::{BranchSummaryEntry, CustomEntry, EntryBase, MessageEntry};

    fn entry(seq: u64, message: AgentMessage) -> Entry {
        Entry::Message(MessageEntry {
            base: EntryBase {
                entry_type: "message".to_string(),
                id: format!("e{seq}"),
                seq,
                parent_id: None,
                timestamp: 0,
            },
            message,
            terminate: None,
        })
    }

    fn user_message(content: kiqi_ai::types::UserContent) -> AgentMessage {
        AgentMessage::User(kiqi_ai::types::UserMessage {
            role: kiqi_ai::types::UserRole,
            content,
            timestamp: 0,
        })
    }

    fn assistant(content: Vec<Content>) -> AgentMessage {
        let mut message = AssistantMessage::empty(Api::Faux, "faux", "faux-1", 0);
        message.content = content;
        AgentMessage::Assistant(Box::new(message))
    }

    fn tool_result(tool_call_id: &str, is_error: bool) -> AgentMessage {
        AgentMessage::ToolResult(Box::new(ToolResultMessage {
            role: kiqi_ai::types::ToolResultRole,
            tool_call_id: tool_call_id.to_string(),
            tool_name: "bash".to_string(),
            content: vec![Content::text("did the thing")],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error,
            timestamp: 0,
        }))
    }

    fn chunk_text(update: &SessionUpdate) -> Option<(&'static str, &str)> {
        match update {
            SessionUpdate::UserMessageChunk(chunk) => text_of(chunk).map(|text| ("user", text)),
            SessionUpdate::AgentMessageChunk(chunk) => text_of(chunk).map(|text| ("agent", text)),
            SessionUpdate::AgentThoughtChunk(chunk) => text_of(chunk).map(|text| ("thought", text)),
            _ => None,
        }
    }

    fn text_of(chunk: &ContentChunk) -> Option<&str> {
        match &chunk.content {
            agent_client_protocol::schema::v1::ContentBlock::Text(text) => Some(&text.text),
            _ => None,
        }
    }

    #[test]
    fn replay_mapping_table_user_assistant_thought_toolcall_result() {
        let branch = vec![
            entry(
                1,
                user_message(kiqi_ai::types::UserContent::Text("hi".into())),
            ),
            entry(
                2,
                assistant(vec![
                    Content::text("plan: "),
                    Content::thinking("pondering"),
                    Content::tool_call("t1", "bash", serde_json::json!({"cmd": "ls"})),
                ]),
            ),
            entry(3, tool_result("t1", false)),
            entry(4, assistant(vec![Content::text("done")])),
        ];

        let expected: &[&str] = &["user", "agent", "thought", "call", "update", "agent"];
        let mut kinds = Vec::new();
        let mut updates = Vec::new();
        for entry in &branch {
            for update in replay_entry(entry) {
                match &update {
                    SessionUpdate::ToolCall(call) => {
                        assert_eq!(call.tool_call_id, "t1".into());
                        assert_eq!(call.kind, ToolKind::Execute);
                        assert_eq!(call.raw_input, Some(serde_json::json!({"cmd": "ls"})));
                        kinds.push("call");
                    }
                    SessionUpdate::ToolCallUpdate(update) => {
                        assert_eq!(update.tool_call_id, "t1".into());
                        assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
                        kinds.push("update");
                    }
                    other => {
                        let (kind, _) = chunk_text(other).expect("chunk or tool update");
                        kinds.push(kind);
                    }
                }
                updates.push(update);
            }
        }
        assert_eq!(kinds, expected);
        // Chunk texts ride through in order.
        let texts: Vec<(&str, &str)> = updates.iter().filter_map(chunk_text).collect();
        assert_eq!(
            texts,
            vec![
                ("user", "hi"),
                ("agent", "plan: "),
                ("thought", "pondering"),
                ("agent", "done"),
            ]
        );
    }

    #[test]
    fn replay_user_blocks_emit_one_chunk_per_text_block() {
        let entry = entry(
            1,
            user_message(kiqi_ai::types::UserContent::Blocks(vec![
                Content::text("part one "),
                Content::text("part two"),
            ])),
        );
        let updates = replay_entry(&entry);
        let texts: Vec<&str> = updates
            .iter()
            .filter_map(chunk_text)
            .map(|(_, t)| t)
            .collect();
        assert_eq!(texts, vec!["part one ", "part two"]);
        assert!(updates
            .iter()
            .all(|update| matches!(update, SessionUpdate::UserMessageChunk(_))));
    }

    #[test]
    fn replay_failed_tool_result_marks_failed() {
        let entry = entry(1, tool_result("t9", true));
        let updates = replay_entry(&entry);
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            SessionUpdate::ToolCallUpdate(update) => {
                assert_eq!(update.tool_call_id, "t9".into());
                assert_eq!(update.fields.status, Some(ToolCallStatus::Failed));
            }
            other => panic!("expected tool call update, got {other:?}"),
        }
    }

    #[test]
    fn replay_skips_non_message_entries_and_custom_messages() {
        let base = EntryBase {
            entry_type: "compaction".to_string(),
            id: "c1".to_string(),
            seq: 1,
            parent_id: None,
            timestamp: 0,
        };
        let compaction = Entry::Compaction(kiqi_harness::session::types::CompactionEntry {
            base: base.clone(),
            summary: "earlier context".to_string(),
            retained_tail: Vec::new(),
            tokens_before: 0,
            details: None,
            usage: None,
        });
        let custom = Entry::Custom(CustomEntry {
            custom_type: "runResume".to_string(),
            data: None,
            base: base.clone(),
        });
        let branch_summary = Entry::BranchSummary(BranchSummaryEntry {
            from_id: "e0".to_string(),
            summary: "s".to_string(),
            details: None,
            usage: None,
            base,
        });
        // ModelChange too, via a different shape of the same "not a message" rule.
        let model_change = Entry::ModelChange(kiqi_harness::session::types::ModelChangeEntry {
            provider: "faux".to_string(),
            model_id: "faux-1".to_string(),
            base: EntryBase {
                entry_type: "model_change".to_string(),
                id: "m1".to_string(),
                seq: 2,
                parent_id: None,
                timestamp: 0,
            },
        });
        for skipped in [compaction, custom, branch_summary, model_change] {
            assert!(
                replay_entry(&skipped).is_empty(),
                "{:?} must not replay",
                skipped.entry_type()
            );
        }

        // A message entry carrying a custom (ki-internal) message also skips.
        let custom_message = entry(
            3,
            AgentMessage::Custom(kiqi_agent::message::CustomMessage {
                role: "runResume".to_string(),
                content: Vec::new(),
                data: serde_json::Value::Null,
                timestamp: 0,
            }),
        );
        assert!(replay_entry(&custom_message).is_empty());
    }

    #[test]
    fn replay_drops_empty_text_blocks() {
        let entry = entry(1, assistant(vec![Content::text(""), Content::thinking("")]));
        assert!(replay_entry(&entry).is_empty());
    }
}
