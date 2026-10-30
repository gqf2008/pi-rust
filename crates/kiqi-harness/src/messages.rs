//! Mirrors `packages/agent/src/harness/messages.ts` — the harness's custom-role
//! `AgentMessage` constructors and the harness-level `convert_to_llm`.
//!
//! TS extends the agent's `AgentMessage` via declaration merging on
//! `CustomAgentMessages` with four roles: `bashExecution`, `custom`,
//! `branchSummary`, `compactionSummary`. Rust has no declaration merging, so
//! each role is a `kiqi_agent::message::AgentMessage::Custom(CustomMessage)`
//! whose `role` field carries the TS role string and whose `data: Value` carries
//! the role-specific structured payload (mirrors the TS role interface fields).
//!
//! `convert_to_llm` is the *harness* converter (distinct from
//! `kiqi_agent::default_convert_to_llm`, which drops custom roles): it projects
//! each custom role into a provider-facing `kiqi_ai::Message` (a `User` message
//! with role-rendered text), so the LLM sees bash output / branch + compaction
//! summaries as user text. Unregistered custom roles are dropped (mirrors the
//! TS `default → undefined` branch).
//!
//! Pulled forward from M5e because M5d compaction depends on it
//! (`createCompactionSummaryMessage`, `createBranchSummaryMessage`,
//! `convertToLlm`).

use kiqi_agent::message::{AgentMessage, CustomMessage};
use kiqi_ai::types::{
    Content, Message, ToolResultMessage, ToolResultRole, UserContent, UserMessage,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Wraps `summary` in the compaction-summary text envelope. Mirrors
/// `COMPACTION_SUMMARY_PREFIX` / `SUFFIX`.
pub const COMPACTION_SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

/// Wraps `summary` in the branch-summary text envelope. Mirrors
/// `BRANCH_SUMMARY_PREFIX` / `SUFFIX`.
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

// ---------------------------------------------------------------------------
// Role tag constants
// ---------------------------------------------------------------------------

pub const BASH_EXECUTION_ROLE: &str = "bashExecution";
pub const CUSTOM_ROLE: &str = "custom";
pub const BRANCH_SUMMARY_ROLE: &str = "branchSummary";
pub const COMPACTION_SUMMARY_ROLE: &str = "compactionSummary";

// ---------------------------------------------------------------------------
// Per-role structured payloads (stored on `CustomMessage::data`)
// ---------------------------------------------------------------------------

/// `BashExecutionMessage` payload. Mirrors the TS interface fields (minus
/// `role`/`timestamp`, which live on `CustomMessage` itself).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashExecutionData {
    pub command: String,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_from_context: Option<bool>,
}

/// `BranchSummaryMessage` payload. Mirrors `BranchSummaryMessage`
/// (`{summary, fromId}`, the role/timestamp live on `CustomMessage`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryData {
    pub summary: String,
    pub from_id: String,
}

/// `CompactionSummaryMessage` payload. Mirrors `CompactionSummaryMessage`
/// (`{summary, tokensBefore}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSummaryData {
    pub summary: String,
    pub tokens_before: i64,
}

/// `CustomMessage<T>` payload. Mirrors `CustomMessage` (`{customType, content,
/// display, details}`) for the generic `custom` role. `content` lives on
/// `CustomMessage::content`, so the data payload carries the metadata fields
/// only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomData {
    pub custom_type: String,
    #[serde(default = "default_true")]
    pub display: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Constructors — mirror createBashExecutionMessage / createBranchSummaryMessage
// / createCompactionSummaryMessage / createCustomMessage
// ---------------------------------------------------------------------------

/// Build a `bashExecution` custom message. Mirrors the TS constructor; `content`
/// is empty (rendered from `data` at convert time, matches TS where
/// `convertToLlm` calls `bashExecutionToText`).
pub fn bash_execution_message(
    command: impl Into<String>,
    output: impl Into<String>,
    exit_code: Option<i32>,
    cancelled: bool,
    truncated: bool,
    full_output_path: Option<String>,
    exclude_from_context: Option<bool>,
    timestamp: i64,
) -> AgentMessage {
    let data = serde_json::to_value(BashExecutionData {
        command: command.into(),
        output: output.into(),
        exit_code,
        cancelled,
        truncated,
        full_output_path,
        exclude_from_context,
    })
    .expect("BashExecutionData serializes");
    AgentMessage::Custom(CustomMessage {
        role: BASH_EXECUTION_ROLE.into(),
        content: Vec::new(),
        data,
        timestamp,
    })
}

/// `createBranchSummaryMessage(summary, fromId, timestamp)`. Mirrors the TS
/// constructor; `content` is empty (rendered from `data.summary` at convert
/// time).
pub fn create_branch_summary_message(
    summary: impl Into<String>,
    from_id: impl Into<String>,
    timestamp: i64,
) -> AgentMessage {
    let data = serde_json::to_value(BranchSummaryData {
        summary: summary.into(),
        from_id: from_id.into(),
    })
    .expect("BranchSummaryData serializes");
    AgentMessage::Custom(CustomMessage {
        role: BRANCH_SUMMARY_ROLE.into(),
        content: Vec::new(),
        data,
        timestamp,
    })
}

/// `createCompactionSummaryMessage(summary, tokensBefore, timestamp)`. Mirrors
/// the TS constructor; `content` is empty (rendered from `data.summary` at
/// convert time).
pub fn create_compaction_summary_message(
    summary: impl Into<String>,
    tokens_before: i64,
    timestamp: i64,
) -> AgentMessage {
    let data = serde_json::to_value(CompactionSummaryData {
        summary: summary.into(),
        tokens_before,
    })
    .expect("CompactionSummaryData serializes");
    AgentMessage::Custom(CustomMessage {
        role: COMPACTION_SUMMARY_ROLE.into(),
        content: Vec::new(),
        data,
        timestamp,
    })
}

/// `createCustomMessage(customType, content, display, details, timestamp)`.
/// Mirrors the TS constructor; `content` (string or text/image blocks) is
/// normalized to `Vec<Content>` on the stored `CustomMessage`.
pub fn create_custom_message(
    custom_type: impl Into<String>,
    content: UserContent,
    display: bool,
    details: Option<serde_json::Value>,
    timestamp: i64,
) -> AgentMessage {
    let blocks = match content {
        UserContent::Text(s) => vec![Content::text(s)],
        UserContent::Blocks(b) => b,
    };
    let data = serde_json::to_value(CustomData {
        custom_type: custom_type.into(),
        display,
        details,
    })
    .expect("CustomData serializes");
    AgentMessage::Custom(CustomMessage {
        role: CUSTOM_ROLE.into(),
        content: blocks,
        data,
        timestamp,
    })
}

// ---------------------------------------------------------------------------
// Role-data accessors (used by compaction's estimate_tokens / convert_to_llm)
// ---------------------------------------------------------------------------

/// If `msg` is a `compactionSummary` custom message, deserialize its `data`.
pub fn compaction_summary_data(msg: &AgentMessage) -> Option<CompactionSummaryData> {
    match msg {
        AgentMessage::Custom(c) if c.role == COMPACTION_SUMMARY_ROLE => {
            serde_json::from_value(c.data.clone()).ok()
        }
        _ => None,
    }
}

/// If `msg` is a `branchSummary` custom message, deserialize its `data`.
pub fn branch_summary_data(msg: &AgentMessage) -> Option<BranchSummaryData> {
    match msg {
        AgentMessage::Custom(c) if c.role == BRANCH_SUMMARY_ROLE => {
            serde_json::from_value(c.data.clone()).ok()
        }
        _ => None,
    }
}

/// If `msg` is a `bashExecution` custom message, deserialize its `data`.
pub fn bash_execution_data(msg: &AgentMessage) -> Option<BashExecutionData> {
    match msg {
        AgentMessage::Custom(c) if c.role == BASH_EXECUTION_ROLE => {
            serde_json::from_value(c.data.clone()).ok()
        }
        _ => None,
    }
}

/// If `msg` is a generic `custom` role message, deserialize its `data`.
pub fn custom_data(msg: &AgentMessage) -> Option<CustomData> {
    match msg {
        AgentMessage::Custom(c) if c.role == CUSTOM_ROLE => {
            serde_json::from_value(c.data.clone()).ok()
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// bashExecutionToText — mirrors the TS renderer
// ---------------------------------------------------------------------------

/// Render a `bashExecution` message to its user-visible text. Mirrors
/// `bashExecutionToText`.
pub fn bash_execution_to_text(data: &BashExecutionData) -> String {
    let mut text = format!("Ran `{}`\n", data.command);
    if !data.output.is_empty() {
        text += &format!("```\n{}\n```", data.output);
    } else {
        text += "(no output)";
    }
    if data.cancelled {
        text += "\n\n(command cancelled)";
    } else if let Some(code) = data.exit_code {
        if code != 0 {
            text += &format!("\n\nCommand exited with code {code}");
        }
    }
    if data.truncated {
        if let Some(full) = &data.full_output_path {
            text += &format!("\n\n[Output truncated. Full output: {full}]");
        }
    }
    text
}

// ---------------------------------------------------------------------------
// convertToLlm — the harness-level AgentMessage[] -> Message[] converter
// ---------------------------------------------------------------------------

/// A diagnostic summary produced while repairing a recovered context.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCallRepairReport {
    pub repaired_call_ids: Vec<String>,
    /// Tool results with no preceding assistant call (a branch whose assistant
    /// message was never persisted) plus duplicate results. Kept out of the
    /// provider context: every provider rejects a `tool` message that does not
    /// answer a preceding `tool_calls`.
    pub dropped_result_ids: Vec<String>,
}

impl ToolCallRepairReport {
    pub fn is_empty(&self) -> bool {
        self.repaired_call_ids.is_empty() && self.dropped_result_ids.is_empty()
    }
}

const RECOVERY_TOOL_RESULT: &str = concat!(
    "Tool call was interrupted before its result was persisted. ",
    "The tool was not re-executed during session recovery."
);

/// Repair assistant tool calls whose result was lost during a crash or partial
/// session write. This only changes the in-memory provider context: the durable
/// session remains the user's original history and no tool is executed again.
pub fn repair_orphaned_tool_calls(
    messages: Vec<AgentMessage>,
) -> (Vec<AgentMessage>, ToolCallRepairReport) {
    use std::collections::HashMap;
    // A result counts only when some assistant *preceding* it declared the
    // call. A result before any such assistant is orphaned (the usual shape
    // after a resume from a branch whose assistant message was never written)
    // and must not reach the provider.
    let mut declared: HashSet<String> = HashSet::new();
    // Counts, not a set: a repeated `tool_call_id` needs one result per
    // occurrence, so a call whose only result was already consumed earlier in
    // the transcript still needs its own.
    let mut declared_calls: HashMap<String, usize> = HashMap::new();
    let mut existing_results: HashMap<String, usize> = HashMap::new();
    for message in &messages {
        match message {
            AgentMessage::Assistant(assistant) => {
                for content in &assistant.content {
                    if let Content::ToolCall(call) = content {
                        declared.insert(call.id.clone());
                        *declared_calls.entry(call.id.clone()).or_insert(0) += 1;
                    }
                }
            }
            AgentMessage::ToolResult(result) if declared.contains(&result.tool_call_id) => {
                *existing_results
                    .entry(result.tool_call_id.clone())
                    .or_insert(0) += 1;
            }
            _ => {}
        }
    }
    let mut kept_results: HashMap<String, usize> = HashMap::new();
    let mut repaired = Vec::new();
    let mut report = ToolCallRepairReport::default();
    let mut pending_calls: Vec<(String, String, i64)> = Vec::new();

    let flush_missing = |repaired: &mut Vec<AgentMessage>,
                         pending: &mut Vec<(String, String, i64)>,
                         report: &mut ToolCallRepairReport| {
        for (tool_call_id, tool_name, timestamp) in pending.drain(..) {
            report.repaired_call_ids.push(tool_call_id.clone());
            repaired.push(AgentMessage::ToolResult(Box::new(ToolResultMessage {
                role: ToolResultRole,
                tool_call_id,
                tool_name,
                content: vec![Content::text(RECOVERY_TOOL_RESULT)],
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                is_error: true,
                timestamp,
            })));
        }
    };

    for message in messages {
        match &message {
            AgentMessage::Assistant(assistant) => {
                let calls = assistant
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        Content::ToolCall(call)
                            if existing_results.get(&call.id).copied().unwrap_or(0) == 0 =>
                        {
                            Some((call.id.clone(), call.name.clone(), assistant.timestamp))
                        }
                        _ => None,
                    })
                    .collect();
                flush_missing(&mut repaired, &mut pending_calls, &mut report);
                repaired.push(message);
                pending_calls = calls;
            }
            AgentMessage::ToolResult(result) => {
                // No assistant before it declared this call, or a second result
                // for one call: providers reject both shapes.
                // Keep at most as many results as there are calls for that id:
                // duplicates beyond that are exactly what providers reject.
                let kept = {
                    let kept = kept_results.entry(result.tool_call_id.clone()).or_insert(0);
                    let calls = declared_calls
                        .get(&result.tool_call_id)
                        .copied()
                        .unwrap_or(0);
                    if *kept < calls {
                        *kept += 1;
                        true
                    } else {
                        false
                    }
                };
                if !kept {
                    report.dropped_result_ids.push(result.tool_call_id.clone());
                    continue;
                }
                // This result answers exactly one occurrence of the call.
                *existing_results
                    .get_mut(&result.tool_call_id)
                    .expect("counted above") -= 1;
                if let Some(index) = pending_calls
                    .iter()
                    .position(|(id, _, _)| id == &result.tool_call_id)
                {
                    pending_calls.remove(index);
                }
                repaired.push(message);
            }
            _ => {
                flush_missing(&mut repaired, &mut pending_calls, &mut report);
                repaired.push(message);
            }
        }
    }
    flush_missing(&mut repaired, &mut pending_calls, &mut report);
    (repaired, report)
}

/// Move each assistant message's tool results directly after it, pushing any
/// message that was interleaved between `tool_calls` and its results to after
/// the results.
///
/// Strict OpenAI-compatible providers require an assistant message carrying
/// `tool_calls` to be immediately followed by the matching tool messages. A
/// message appended while a tool batch is still in flight — an extension calling
/// the `append_entry` runtime action from inside its tool, e.g. `kiqi-goal`
/// injecting the goal state — would otherwise sit between them, and the request
/// is rejected with HTTP 400 ("An assistant message with 'tool_calls' must be
/// followed by tool messages…"). This repairs it at the provider boundary, so
/// no extension has to know about tool-batch timing.
fn ensure_tool_results_adjacent(messages: Vec<AgentMessage>) -> Vec<AgentMessage> {
    use std::collections::HashMap;

    // One slot per result, not per id: a transcript can repeat a
    // `tool_call_id` (a retried turn), and each occurrence of the call needs its
    // own tool message. A set would give both occurrences the same result and
    // leave the second unanswered, which providers reject.
    let mut result_index: HashMap<&str, std::collections::VecDeque<usize>> = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        if let AgentMessage::ToolResult(result) = message {
            result_index
                .entry(result.tool_call_id.as_str())
                .or_default()
                .push_back(index);
        }
    }

    // Results already emitted next to their assistant; the main loop skips them
    // when it reaches their original position.
    let mut consumed = vec![false; messages.len()];
    let mut out = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        if consumed[index] {
            continue;
        }
        match message {
            AgentMessage::Assistant(assistant) => {
                out.push(message.clone());
                for content in &assistant.content {
                    if let Content::ToolCall(call) = content {
                        while let Some(result_at) = result_index
                            .get_mut(call.id.as_str())
                            .and_then(|queue| queue.pop_front())
                        {
                            if consumed[result_at] {
                                continue;
                            }
                            consumed[result_at] = true;
                            out.push(messages[result_at].clone());
                            break;
                        }
                    }
                }
            }
            _ => out.push(message.clone()),
        }
    }
    out
}

/// The harness converter. It moves tool results next to their assistant and
/// repairs interrupted tool calls, then mirrors TS `convertToLlm`: renders
/// custom roles as user text and passes through normal user/assistant/tool-result
/// messages.
pub fn convert_to_llm(messages: Vec<AgentMessage>) -> Vec<Message> {
    let messages = ensure_tool_results_adjacent(messages);
    let (messages, report) = repair_orphaned_tool_calls(messages);
    if !report.is_empty() {
        tracing::warn!(
            repaired = report.repaired_call_ids.len(),
            call_ids = ?report.repaired_call_ids,
            dropped = report.dropped_result_ids.len(),
            dropped_ids = ?report.dropped_result_ids,
            "repaired orphaned tool calls while building provider context"
        );
    }
    let messages: Vec<Message> = messages
        .into_iter()
        .filter_map(|m| match m {
            AgentMessage::User(u) => Some(Message::User(u)),
            AgentMessage::Assistant(a) => Some(Message::Assistant(a)),
            AgentMessage::ToolResult(t) => Some(Message::ToolResult(t)),
            AgentMessage::Custom(c) => convert_custom_to_llm(c),
        })
        .collect();
    let messages = drop_unanchored_tool_results(messages);
    ensure_every_tool_call_has_a_result(messages)
}

/// Give every `tool_calls` entry a result message.
///
/// The other half of the same gateway check: "An assistant message with
/// 'tool_calls' must be followed by tool messages responding to each
/// 'tool_call_id'". The id-keyed repairs upstream only reason about the results
/// they can see; if history lost one (an interrupted run, a compaction cut, a
/// duplicated id across turns) the request is rejected whole, and because the
/// history is durable it fails the same way on every retry and every resume.
fn ensure_every_tool_call_has_a_result(messages: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    let mut index = 0;
    while index < messages.len() {
        let message = messages[index].clone();
        out.push(message.clone());
        index += 1;
        let Message::Assistant(assistant) = &message else {
            continue;
        };
        let calls: Vec<String> = assistant
            .content
            .iter()
            .filter_map(|content| match content {
                kiqi_ai::types::Content::ToolCall(call) => Some(call.id.clone()),
                _ => None,
            })
            .collect();
        if calls.is_empty() {
            continue;
        }
        // The contiguous results behind this turn are its answers.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        while let Some(Message::ToolResult(result)) = messages.get(index) {
            seen.insert(result.tool_call_id.clone());
            out.push(messages[index].clone());
            index += 1;
        }
        for id in calls {
            if seen.contains(&id) {
                continue;
            }
            tracing::warn!(
                tool_call_id = %id,
                "synthesized a tool result for a tool_calls entry that had none"
            );
            out.push(Message::ToolResult(Box::new(ToolResultMessage {
                role: ToolResultRole,
                tool_call_id: id.clone(),
                tool_name: String::new(),
                content: vec![Content::text(
                    "Tool execution was interrupted before its result was recorded.",
                )],
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                is_error: true,
                timestamp: 0,
            })));
        }
    }
    out
}

/// Drop tool results that are not directly anchored to a preceding assistant
/// `tool_calls` message.
///
/// Strict OpenAI-compatible gateways reject the whole request with
/// `Messages with role 'tool' must be a response to a preceding message with
/// 'tool_calls'`. The id-keyed repair above keeps a result as long as *some*
/// assistant declared its id, which is not the same as the result sitting
/// directly behind that assistant: an empty/aborted assistant dropped by the
/// provider's own conversion, a compaction boundary, or a user message wedged
/// between them all produce exactly this shape. This pass is structural — it is
/// the exact property the gateway checks — so it holds regardless of how the
/// history got into that state.
fn drop_unanchored_tool_results(messages: Vec<Message>) -> Vec<Message> {
    let mut anchored: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(messages.len());
    for message in messages {
        match &message {
            Message::Assistant(assistant) => {
                anchored = assistant
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        kiqi_ai::types::Content::ToolCall(call) => Some(call.id.clone()),
                        _ => None,
                    })
                    .collect();
                out.push(message);
            }
            Message::ToolResult(result) => {
                if anchored.contains(&result.tool_call_id) {
                    out.push(message);
                } else {
                    tracing::warn!(
                        tool_call_id = %result.tool_call_id,
                        tool = %result.tool_name,
                        "dropped a tool result with no preceding tool_calls message"
                    );
                }
            }
            _ => {
                // Only an assistant turn declares tool calls; anything else
                // ends the anchor window.
                anchored.clear();
                out.push(message);
            }
        }
    }
    out
}

fn convert_custom_to_llm(c: CustomMessage) -> Option<Message> {
    let timestamp = c.timestamp;
    match c.role.as_str() {
        BASH_EXECUTION_ROLE => {
            let data: BashExecutionData = serde_json::from_value(c.data).ok()?;
            if data.exclude_from_context.unwrap_or(false) {
                return None;
            }
            let text = bash_execution_to_text(&data);
            Some(Message::User(UserMessage::new(text, timestamp)))
        }
        CUSTOM_ROLE => {
            // Generic custom: content is the stored text/image blocks.
            Some(Message::User(UserMessage::new(
                UserContent::Blocks(c.content),
                timestamp,
            )))
        }
        BRANCH_SUMMARY_ROLE => {
            let data: BranchSummaryData = serde_json::from_value(c.data).ok()?;
            let text = format!(
                "{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}",
                data.summary
            );
            Some(Message::User(UserMessage::new(text, timestamp)))
        }
        COMPACTION_SUMMARY_ROLE => {
            let data: CompactionSummaryData = serde_json::from_value(c.data).ok()?;
            let text = format!(
                "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                data.summary
            );
            Some(Message::User(UserMessage::new(text, timestamp)))
        }
        // Unregistered custom role → drop (mirrors TS `default → undefined`).
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiqi_ai::types::{Api, AssistantMessage, ToolCall, ToolCallType};

    fn assistant_with_calls(ids: &[(&str, &str)]) -> AgentMessage {
        let mut assistant = AssistantMessage::empty(Api::OpenaiCompletions, "test", "model", 42);
        assistant.content = ids
            .iter()
            .map(|(id, name)| {
                Content::ToolCall(ToolCall {
                    kind: ToolCallType,
                    id: (*id).to_string(),
                    name: (*name).to_string(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                    namespace: None,
                })
            })
            .collect();
        AgentMessage::Assistant(Box::new(assistant))
    }

    fn tool_result(id: &str, name: &str) -> AgentMessage {
        AgentMessage::ToolResult(Box::new(ToolResultMessage {
            role: ToolResultRole,
            tool_call_id: id.to_string(),
            tool_name: name.to_string(),
            content: vec![Content::text("ok")],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: 43,
        }))
    }

    /// The other half of the gateway's check: a turn that declares two calls
    /// but only has one result must still produce a valid request.
    #[test]
    fn a_tool_call_without_a_result_gets_a_synthesized_one() {
        let history = vec![
            assistant_with_calls(&[("call-1", "bash"), ("call-2", "read")]),
            tool_result("call-1", "bash"),
        ];
        let converted = convert_to_llm(history);
        let ids: Vec<&str> = converted
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(result) => Some(result.tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["call-1", "call-2"], "{ids:?}");
        // The synthesized one is an error result, so the model knows the output
        // is missing instead of assuming the call succeeded silently.
        assert!(
            converted.iter().any(|message| matches!(
                message,
                Message::ToolResult(result) if result.is_error
            )),
            "{converted:?}"
        );
    }

    /// A tool result that is not directly behind its `tool_calls` message is
    /// what strict gateways reject with "Messages with role 'tool' must be a
    /// response to a preceding message with 'tool_calls'".
    #[test]
    fn a_tool_result_without_its_tool_calls_in_front_is_dropped() {
        // A user message wedged between the assistant turn and its result — the
        // shape steering/background-agent delivery can produce.
        let interleaved = vec![
            assistant_with_calls(&[("call-1", "bash")]),
            AgentMessage::User(kiqi_ai::types::UserMessage::new("继续", 44)),
            tool_result("call-1", "bash"),
        ];
        let converted = convert_to_llm(interleaved);
        let shape: Vec<&str> = converted
            .iter()
            .map(|message| match message {
                Message::Assistant(_) => "assistant",
                Message::ToolResult(_) => "tool",
                Message::User(_) => "user",
            })
            .collect();
        // The repair moves the result back behind its turn; the anchor pass
        // then has nothing left to drop.
        assert_eq!(shape, vec!["assistant", "tool", "user"], "{shape:?}");

        // Same for a result whose declaring assistant never made it into the
        // context at all.
        let orphan = vec![tool_result("call-9", "bash")];
        assert!(convert_to_llm(orphan).is_empty());

        // The healthy shape survives untouched.
        let healthy = vec![
            assistant_with_calls(&[("call-1", "bash")]),
            tool_result("call-1", "bash"),
        ];
        let converted = convert_to_llm(healthy);
        assert_eq!(
            converted
                .iter()
                .filter(|message| matches!(message, Message::ToolResult(_)))
                .count(),
            1
        );
    }

    /// A retried turn can reuse a `tool_call_id`. Each occurrence needs its own
    /// tool message immediately after it, or strict providers reject the whole
    /// request ("insufficient tool messages following tool_calls message").
    #[test]
    fn a_repeated_tool_call_id_gets_its_own_result() {
        let original = vec![
            assistant_with_calls(&[("call-1", "bash")]),
            tool_result("call-1", "bash"),
            assistant_with_calls(&[("call-1", "bash")]),
        ];
        let converted = convert_to_llm(original);
        let shape: Vec<&str> = converted
            .iter()
            .map(|message| match message {
                Message::Assistant(_) => "assistant",
                Message::ToolResult(_) => "tool",
                Message::User(_) => "user",
            })
            .collect();
        assert_eq!(
            shape,
            vec!["assistant", "tool", "assistant", "tool"],
            "{shape:?}"
        );
    }

    #[test]
    fn repair_inserts_error_result_for_missing_tool_output() {
        let original = assistant_with_calls(&[("call-1", "read")]);
        let (repaired, report) = repair_orphaned_tool_calls(vec![original.clone()]);

        assert_eq!(report.repaired_call_ids, vec!["call-1"]);
        assert_eq!(repaired.len(), 2);
        assert_eq!(repaired[0], original);
        let AgentMessage::ToolResult(result) = &repaired[1] else {
            panic!("expected synthetic tool result");
        };
        assert_eq!(result.tool_call_id, "call-1");
        assert_eq!(result.tool_name, "read");
        assert!(result.is_error);
        assert!(
            matches!(&result.content[0], Content::Text(text) if text.text.contains("not re-executed"))
        );
    }

    #[test]
    fn repair_handles_parallel_calls_and_preserves_existing_results() {
        let assistant = assistant_with_calls(&[("call-1", "read"), ("call-2", "bash")]);
        let (repaired, report) =
            repair_orphaned_tool_calls(vec![assistant, tool_result("call-1", "read")]);

        assert_eq!(report.repaired_call_ids, vec!["call-2"]);
        assert_eq!(repaired.len(), 3);
        assert!(
            matches!(&repaired[1], AgentMessage::ToolResult(result) if result.tool_call_id == "call-1" && !result.is_error)
        );
        assert!(
            matches!(&repaired[2], AgentMessage::ToolResult(result) if result.tool_call_id == "call-2" && result.is_error)
        );
    }

    #[test]
    fn normal_history_is_not_changed_by_repair() {
        let messages = vec![
            assistant_with_calls(&[("call-1", "read")]),
            tool_result("call-1", "read"),
        ];
        let (repaired, report) = repair_orphaned_tool_calls(messages.clone());
        assert!(report.is_empty());
        assert_eq!(repaired, messages);
    }

    #[test]
    fn tool_results_without_a_preceding_call_are_dropped() {
        // The resumed-branch shape: the assistant message that declared the
        // calls was never persisted, so the branch starts with its results.
        let messages = vec![
            tool_result("call-orphan-1", "ask_user"),
            tool_result("call-orphan-2", "ask_user"),
            assistant_with_calls(&[("call-1", "read")]),
            tool_result("call-1", "read"),
        ];
        let (repaired, report) = repair_orphaned_tool_calls(messages);
        assert_eq!(
            report.dropped_result_ids,
            vec!["call-orphan-1", "call-orphan-2"]
        );
        assert_eq!(repaired.len(), 2, "the orphan results are gone");
        assert!(matches!(&repaired[0], AgentMessage::Assistant(_)));
        assert!(matches!(&repaired[1], AgentMessage::ToolResult(r) if r.tool_call_id == "call-1"));
    }

    #[test]
    fn a_duplicate_tool_result_is_dropped() {
        let messages = vec![
            assistant_with_calls(&[("call-1", "read")]),
            tool_result("call-1", "read"),
            tool_result("call-1", "read"),
        ];
        let (repaired, report) = repair_orphaned_tool_calls(messages);
        assert_eq!(report.dropped_result_ids, vec!["call-1"]);
        assert_eq!(repaired.len(), 2);
    }

    #[test]
    fn compaction_summary_round_trips_data() {
        let m = create_compaction_summary_message("sum", 42, 100);
        let d = compaction_summary_data(&m).unwrap();
        assert_eq!(d.summary, "sum");
        assert_eq!(d.tokens_before, 42);
    }

    #[test]
    fn branch_summary_round_trips_data() {
        let m = create_branch_summary_message("s", "from-1", 7);
        let d = branch_summary_data(&m).unwrap();
        assert_eq!(d.summary, "s");
        assert_eq!(d.from_id, "from-1");
    }

    #[test]
    fn convert_to_llm_renders_compaction_summary_as_user_text() {
        let m = create_compaction_summary_message("hello", 10, 1);
        let out = convert_to_llm(vec![m]);
        assert_eq!(out.len(), 1);
        match &out[0] {
            Message::User(u) => match &u.content {
                UserContent::Text(s) => {
                    assert!(s.contains("hello"));
                    assert!(s.contains("<summary>"));
                }
                _ => panic!("expected text content"),
            },
            _ => panic!("expected user message"),
        }
    }

    #[test]
    fn convert_to_llm_drops_bash_when_excluded() {
        let m = bash_execution_message("ls", "out", None, false, false, None, Some(true), 1);
        assert!(convert_to_llm(vec![m]).is_empty());
    }

    #[test]
    fn convert_to_llm_renders_bash_execution_to_text() {
        let m = bash_execution_message("echo hi", "hi", Some(0), false, false, None, None, 1);
        let out = convert_to_llm(vec![m]);
        assert_eq!(out.len(), 1);
        match &out[0] {
            Message::User(u) => match &u.content {
                UserContent::Text(s) => {
                    assert!(s.contains("Ran `echo hi`"));
                    assert!(s.contains("hi"));
                }
                _ => panic!("expected text"),
            },
            _ => panic!("expected user"),
        }
    }

    #[test]
    fn convert_to_llm_passes_through_base_roles() {
        let u = AgentMessage::User(UserMessage::new("hi", 1));
        let out = convert_to_llm(vec![u]);
        assert!(matches!(out[0], Message::User(_)));
    }

    #[test]
    fn convert_to_llm_drops_unregistered_custom_role() {
        let c = CustomMessage::new("weird".to_string(), Vec::new(), serde_json::Value::Null, 1);
        let out = convert_to_llm(vec![AgentMessage::Custom(c)]);
        assert!(out.is_empty());
    }

    #[test]
    fn convert_to_llm_moves_an_interleaved_message_after_the_tool_results() {
        // A context injection appended mid tool-batch lands between the
        // assistant's tool_calls and its result. It must be moved after the
        // result, or strict providers reject the request with HTTP 400.
        let custom = CustomMessage::new(
            CUSTOM_ROLE.to_string(),
            vec![Content::text("injected")],
            serde_json::Value::Null,
            1,
        );
        let messages = vec![
            assistant_with_calls(&[("call_a", "goal")]),
            AgentMessage::Custom(custom),
            tool_result("call_a", "goal"),
        ];
        let out = convert_to_llm(messages);
        assert_eq!(out.len(), 3);
        match (&out[0], &out[1], &out[2]) {
            (Message::Assistant(a), Message::ToolResult(t), Message::User(_)) => {
                assert!(a
                    .content
                    .iter()
                    .any(|c| matches!(c, Content::ToolCall(tc) if tc.id == "call_a")));
                assert_eq!(t.tool_call_id, "call_a");
            }
            _ => panic!("assistant tool_calls must be immediately followed by its tool result"),
        }
    }
}
