//! Child progress: what a `ki -p` node is doing right now.
//!
//! `ki -p` used to print only its final answer, so a node box could show
//! nothing but "(starting)" for the whole run. With `--mode json` the child
//! streams its agent events as JSONL; this module projects the interesting ones
//! into rail lines and a final text, so the panel shows the tool it is running,
//! its token usage and, at the end, its answer.
//!
//! Unknown lines are ignored: the child may emit event kinds this guest does
//! not model yet, and the wave must never fail because of one.

use serde_json::Value;

/// One parsed child event.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum NodeEvent {
    /// A tool call started (`▶ name`).
    ToolStart(String),
    /// A tool call finished (`✔ name` / `✖ name`).
    ToolEnd { name: String, is_error: bool },
    /// A completed assistant message: its text and cumulative usage.
    Message {
        text: Option<String>,
        input: u64,
        output: u64,
    },
    /// The terminal `result` line: outcome and final text.
    Final {
        outcome: String,
        text: Option<String>,
    },
    /// Nothing the rail shows.
    Ignore,
}

/// Project one JSONL line from a `--mode json` child.
pub(crate) fn parse(line: &str) -> NodeEvent {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return NodeEvent::Ignore;
    };
    match value.get("type").and_then(Value::as_str).unwrap_or("") {
        "tool_execution_start" => NodeEvent::ToolStart(
            value
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string(),
        ),
        "tool_execution_end" => NodeEvent::ToolEnd {
            name: value
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string(),
            is_error: value
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "message_end" => {
            let message = value.get("message").cloned().unwrap_or(Value::Null);
            NodeEvent::Message {
                text: assistant_text(&message),
                input: usage(&message, &["input", "inputTokens"]),
                output: usage(&message, &["output", "outputTokens"]),
            }
        }
        "result" => NodeEvent::Final {
            outcome: value
                .get("outcome")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            text: value
                .get("finalText")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|text| !text.trim().is_empty()),
        },
        _ => NodeEvent::Ignore,
    }
}

fn usage(message: &Value, keys: &[&str]) -> u64 {
    let usage = match message.get("usage") {
        Some(usage) => usage,
        None => return 0,
    };
    keys.iter()
        .find_map(|key| usage.get(*key).and_then(Value::as_u64))
        .unwrap_or(0)
}

fn assistant_text(message: &Value) -> Option<String> {
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let text: String = message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, NodeEvent};

    #[test]
    fn tool_lifecycle_projects_to_rail_lines() {
        assert_eq!(
            parse(r#"{"type":"tool_execution_start","toolName":"bash"}"#),
            NodeEvent::ToolStart("bash".into())
        );
        assert_eq!(
            parse(r#"{"type":"tool_execution_end","toolName":"bash","isError":true}"#),
            NodeEvent::ToolEnd {
                name: "bash".into(),
                is_error: true
            }
        );
    }

    #[test]
    fn assistant_message_carries_text_and_usage() {
        let line = r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"two files touched"}],"usage":{"input":1200,"output":345}}}"#;
        assert_eq!(
            parse(line),
            NodeEvent::Message {
                text: Some("two files touched".into()),
                input: 1200,
                output: 345
            }
        );
    }

    #[test]
    fn result_line_is_the_final_answer() {
        let line = r#"{"type":"result","outcome":"completed","finalText":"done: 2/2"}"#;
        assert_eq!(
            parse(line),
            NodeEvent::Final {
                outcome: "completed".into(),
                text: Some("done: 2/2".into())
            }
        );
    }

    #[test]
    fn unknown_lines_are_ignored() {
        assert_eq!(parse("not json"), NodeEvent::Ignore);
        assert_eq!(parse(r#"{"type":"message_update","message":{}}"#), NodeEvent::Ignore);
        assert_eq!(parse(r#"{"type":"something_new"}"#), NodeEvent::Ignore);
    }
}
