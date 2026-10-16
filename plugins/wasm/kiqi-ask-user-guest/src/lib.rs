//! `kiqi-ask-user` ported to the wasm ABI.
//!
//! Same argument normalization as the compile-time plugin; the interaction is
//! the difference. A C ABI cannot block, so the native plugin needs a
//! start/poll/cancel state machine driven by the host. Here `call_tool` blocks,
//! so the dialog is a straight open → poll → answer loop, with cancellation and
//! `timeout` handled inline.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kiqi_plugin_guest::bindings::exports::kiqi::plugin::plugin_api::Guest;
use kiqi_plugin_guest::bindings::kiqi::plugin::{types, ui};
use serde_json::{json, Value};

const TOOL: &str = "ask_user";
const DESCRIPTION: &str =
    "Ask the user an interactive question with selectable options or freeform input.";
const SCHEMA: &str = r#"{"type":"object","properties":{"question":{"type":"string"},"context":{"type":"string"},"options":{"type":"array","items":{"oneOf":[{"type":"string"},{"type":"object"}]}},"allowMultiple":{"type":"boolean"},"allowFreeform":{"type":"boolean"},"allowComment":{"type":"boolean"},"displayMode":{"type":"string","enum":["overlay","inline"]},"timeout":{"type":"integer","minimum":1},"type":{"type":"string","enum":["question","confirm"]},"questions":{"type":"array","minItems":1,"maxItems":3},"suggest":{"type":"string"},"summary":{"type":"string"}},"additionalProperties":false}"#;

/// How long a poll sleeps before re-checking the host for an answer.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

// ---------------------------------------------------------------------------
// Typed normalization
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct AskOption {
    title: String,
    description: Option<String>,
}

#[derive(Clone, Debug)]
struct AskQuestion {
    id: String,
    question: String,
    header: Option<String>,
    context: Option<String>,
    options: Vec<AskOption>,
    allow_multiple: bool,
    allow_freeform: bool,
    allow_comment: bool,
    suggest: Option<String>,
    display_mode: Option<String>,
    timeout: Option<u64>,
}

enum AskPlan {
    Confirm { summary: String },
    Questions { questions: Vec<AskQuestion> },
}

impl AskQuestion {
    fn ui_kind(&self) -> &'static str {
        if self.options.is_empty() {
            "input"
        } else {
            "selector"
        }
    }

    fn to_ui(&self, fallback_context: Option<&str>) -> Value {
        json!({
            "kind": self.ui_kind(),
            "id": self.id,
            "header": self.header,
            "question": self.question,
            "context": self.context.as_deref().or(fallback_context),
            "options": self.options.iter().map(|option| {
                let mut value = json!({ "title": option.title });
                if let Some(description) = &option.description {
                    value["description"] = json!(description);
                }
                value
            }).collect::<Vec<_>>(),
            "allowMultiple": self.allow_multiple,
            "allowFreeform": self.allow_freeform,
            "allowComment": self.allow_comment,
            "suggest": self.suggest,
            "displayMode": self.display_mode,
            "timeout": self.timeout,
        })
    }
}

fn opt_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    })
}

fn opt_bool(value: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_bool))
}

fn parse_options(item: &Value) -> Result<Vec<AskOption>, String> {
    let Some(raw) = item.get("options").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    if raw.len() > 20 {
        return Err("a question may contain at most 20 options".into());
    }
    let mut options = Vec::new();
    for option in raw {
        let (title, description) = match option {
            Value::String(title) => (title.trim().to_string(), None),
            Value::Object(object) => {
                let title = ["title", "label", "text", "value", "name"]
                    .iter()
                    .find_map(|key| object.get(*key).and_then(Value::as_str))
                    .map(str::trim)
                    .filter(|title| !title.is_empty())
                    .ok_or("each option requires a non-empty `title`")?
                    .to_string();
                let description = match object.get("description") {
                    None => None,
                    Some(value) => Some(
                        value
                            .as_str()
                            .ok_or("option `description` must be a string")?
                            .trim()
                            .to_string(),
                    ),
                };
                (title, description)
            }
            other => (other.to_string(), None),
        };
        if title.is_empty() {
            return Err("each option requires a non-empty `title`".into());
        }
        options.push(AskOption { title, description });
    }
    Ok(options)
}

fn normalize_question(
    item: &Value,
    defaults: &Value,
    index: usize,
    top_context: Option<&str>,
) -> Result<AskQuestion, String> {
    let question = opt_string(item, &["question", "prompt"])
        .or_else(|| opt_string(defaults, &["question", "prompt"]))
        .unwrap_or_default();
    let question = question.trim();
    if question.is_empty() || question.chars().count() > 500 {
        return Err("each question must contain 1-500 characters".into());
    }
    let display_mode =
        opt_string(item, &["displayMode"]).or_else(|| opt_string(defaults, &["displayMode"]));
    if let Some(mode) = display_mode.as_deref() {
        if mode != "overlay" && mode != "inline" {
            return Err("displayMode must be overlay or inline".into());
        }
    }
    let timeout = item
        .get("timeout")
        .and_then(Value::as_u64)
        .or_else(|| defaults.get("timeout").and_then(Value::as_u64));
    if timeout == Some(0) {
        return Err("timeout must be a positive integer".into());
    }
    let options = parse_options(item)?;
    let allow_freeform = opt_bool(item, &["allowFreeform", "allow_freeform", "allow_custom"])
        .or_else(|| {
            opt_bool(
                defaults,
                &["allowFreeform", "allow_freeform", "allow_custom"],
            )
        })
        .unwrap_or(options.is_empty());
    Ok(AskQuestion {
        id: opt_string(item, &["id", "questionId"]).unwrap_or_else(|| format!("q{}", index + 1)),
        question: question.to_string(),
        header: opt_string(item, &["header"]).or_else(|| opt_string(defaults, &["header"])),
        context: opt_string(item, &["context"]).or_else(|| top_context.map(str::to_string)),
        options,
        allow_multiple: opt_bool(item, &["allowMultiple", "allow_multiple", "multiple"])
            .or_else(|| opt_bool(defaults, &["allowMultiple", "allow_multiple", "multiple"]))
            .unwrap_or(false),
        allow_freeform,
        allow_comment: opt_bool(item, &["allowComment", "allow_comment"])
            .or_else(|| opt_bool(defaults, &["allowComment", "allow_comment"]))
            .unwrap_or(false),
        suggest: opt_string(item, &["suggest", "placeholder"])
            .or_else(|| opt_string(defaults, &["suggest", "placeholder"])),
        display_mode,
        timeout,
    })
}

fn ask(params: &Value) -> Result<AskPlan, String> {
    let kind = params
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("question");
    if kind != "question" && kind != "confirm" {
        return Err("type must be question or confirm".into());
    }
    if kind == "confirm" {
        let summary = opt_string(params, &["summary", "question"])
            .ok_or("summary is required for confirm")?;
        return Ok(AskPlan::Confirm { summary });
    }

    let top_context = opt_string(params, &["context"]);
    let mut questions = Vec::new();
    if let Some(items) = params.get("questions").and_then(Value::as_array) {
        if items.is_empty() || items.len() > 3 {
            return Err("questions must contain 1-3 items".into());
        }
        for (index, item) in items.iter().enumerate() {
            questions.push(normalize_question(
                item,
                params,
                index,
                top_context.as_deref(),
            )?);
        }
    } else {
        questions.push(normalize_question(
            params,
            params,
            0,
            top_context.as_deref(),
        )?);
    }
    Ok(AskPlan::Questions { questions })
}

fn plan_display_text(plan: &AskPlan) -> String {
    match plan {
        AskPlan::Confirm { summary } => format!("Confirmation requested: {summary}"),
        AskPlan::Questions { questions } => {
            let mut lines = Vec::new();
            for question in questions {
                let mut line = question.question.clone();
                if let Some(header) = &question.header {
                    line = format!("{header}: {line}");
                }
                if !question.options.is_empty() {
                    let labels = question
                        .options
                        .iter()
                        .map(|option| option.title.clone())
                        .collect::<Vec<_>>()
                        .join(", ");
                    line.push_str(&format!("\nChoices: {labels}"));
                }
                lines.push(line);
            }
            lines.join("\n")
        }
    }
}

// ---------------------------------------------------------------------------
// Dialog drive
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct AskAnswer {
    id: String,
    question: String,
    values: Vec<String>,
    text: Option<String>,
    confirmed: Option<bool>,
}

fn next_request_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    format!("ask-{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

fn answer_values(answer: &Value) -> Vec<String> {
    if let Some(values) = answer.get("values").and_then(Value::as_array) {
        return values
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect();
    }
    if let Some(value) = answer.get("value").and_then(Value::as_str) {
        return vec![value.to_string()];
    }
    Vec::new()
}

/// Open one question and wait for its answer. `Err` on cancel or timeout, which
/// the caller turns into a model-visible failure.
fn ask_one(question: &AskQuestion) -> Result<AskAnswer, String> {
    let request_id = next_request_id();
    let open = json!({
        "op": "open",
        "requestId": request_id,
        "toolCallId": "",
        "ui": question.to_ui(None),
    })
    .to_string();
    ui::dialog(&open).map_err(capability_reason)?;

    let deadline = question
        .timeout
        .map(|seconds| Instant::now() + Duration::from_secs(seconds));
    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            let cancel = json!({ "op": "cancel", "requestId": request_id }).to_string();
            let _ = ui::dialog(&cancel);
            return Err("ask timed out".to_string());
        }
        let poll = json!({ "op": "poll", "requestId": request_id }).to_string();
        let response = ui::dialog(&poll).map_err(capability_reason)?;
        let response: Value = serde_json::from_str(&response).unwrap_or(Value::Null);
        match response
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending")
        {
            "answered" => {
                let answer = response.get("answer").cloned().unwrap_or(Value::Null);
                let values = answer_values(&answer);
                let confirmed = if question.ui_kind() == "input" {
                    None
                } else {
                    Some(
                        values
                            .first()
                            .map(|value| value.eq_ignore_ascii_case("yes"))
                            .unwrap_or(false),
                    )
                };
                return Ok(AskAnswer {
                    id: question.id.clone(),
                    question: question.question.clone(),
                    values,
                    text: answer
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    confirmed,
                });
            }
            "cancelled" => return Err("ask cancelled".to_string()),
            _ => std::thread::sleep(POLL_INTERVAL),
        }
    }
}

fn confirm_question(summary: &str) -> AskQuestion {
    AskQuestion {
        id: "confirm".to_string(),
        question: summary.to_string(),
        header: None,
        context: None,
        options: vec![
            AskOption {
                title: "Yes".to_string(),
                description: None,
            },
            AskOption {
                title: "No".to_string(),
                description: None,
            },
        ],
        allow_multiple: false,
        allow_freeform: false,
        allow_comment: false,
        suggest: None,
        display_mode: None,
        timeout: None,
    }
}

fn answer_summary(plan: &AskPlan, answers: &[AskAnswer]) -> String {
    match plan {
        AskPlan::Confirm { summary } => {
            let confirmed = answers
                .first()
                .and_then(|answer| answer.confirmed)
                .unwrap_or(false);
            format!(
                "{}: {summary}",
                if confirmed { "Confirmed" } else { "Declined" }
            )
        }
        AskPlan::Questions { .. } => answers
            .iter()
            .map(|answer| {
                let value = answer
                    .text
                    .clone()
                    .filter(|text| !text.is_empty())
                    .or_else(|| (!answer.values.is_empty()).then(|| answer.values.join(", ")))
                    .unwrap_or_else(|| "(no answer)".to_string());
                format!("{}: {value}", answer.question)
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn answer_result(answers: &[AskAnswer]) -> Value {
    json!(answers
        .iter()
        .map(|answer| json!({
            "id": answer.id,
            "question": answer.question,
            "values": answer.values,
            "text": answer.text,
            "confirmed": answer.confirmed,
        }))
        .collect::<Vec<_>>())
}

fn capability_reason(error: types::CapabilityError) -> String {
    match error {
        types::CapabilityError::Denied(reason) => reason,
        types::CapabilityError::Failed(reason) => format!("failed:{reason}"),
    }
}

// ---------------------------------------------------------------------------
// Plugin surface
// ---------------------------------------------------------------------------

struct AskUserPlugin;

impl Guest for AskUserPlugin {
    fn id() -> String {
        "kiqi-ask-user".to_string()
    }

    fn shutdown() {}

    fn list_tools() -> Vec<types::ToolSpec> {
        vec![types::ToolSpec {
            name: TOOL.to_string(),
            description: DESCRIPTION.to_string(),
            parameters_schema: SCHEMA.to_string(),
        }]
    }

    fn call_tool(name: String, arguments: String) -> Result<types::ToolResult, types::PluginError> {
        if name != TOOL {
            return Err(types::PluginError::UnknownTool(name));
        }
        let params: Value = serde_json::from_str(&arguments).unwrap_or(Value::Null);
        // Reject a malformed plan before opening anything.
        let plan = ask(&params).map_err(types::PluginError::InvalidArguments)?;

        let questions = match &plan {
            AskPlan::Confirm { summary } => vec![confirm_question(summary)],
            AskPlan::Questions { questions } => questions.clone(),
        };
        let mut answers = Vec::new();
        for question in &questions {
            match ask_one(question) {
                Ok(answer) => answers.push(answer),
                Err(reason) => {
                    return Ok(types::ToolResult {
                        blocks: vec![types::ContentBlock::Text(reason)],
                        is_error: true,
                        details: "null".to_string(),
                    })
                }
            }
        }

        Ok(types::ToolResult {
            blocks: vec![types::ContentBlock::Text(answer_summary(&plan, &answers))],
            is_error: false,
            details: json!({
                "answers": answer_result(&answers),
                "ui": { "kind": "ask_user_result" },
            })
            .to_string(),
        })
    }

    fn list_commands() -> Vec<types::CommandSpec> {
        vec![types::CommandSpec {
            name: TOOL.to_string(),
            description: "Ask a question with the interactive Pi selector".to_string(),
        }]
    }

    fn run_command(name: String, arguments: String) -> Result<String, types::PluginError> {
        if name != TOOL {
            return Err(types::PluginError::UnknownTool(name));
        }
        let params: Value =
            serde_json::from_str(&arguments).unwrap_or(json!({ "question": arguments }));
        let plan = ask(&params).map_err(types::PluginError::InvalidArguments)?;
        Ok(plan_display_text(&plan))
    }

    fn list_flags() -> Vec<types::FlagSpec> {
        Vec::new()
    }

    fn list_shortcuts() -> Vec<types::ShortcutSpec> {
        Vec::new()
    }

    fn on_event(_event: types::Event) -> types::EventAction {
        types::EventAction::Continue
    }

    fn before_agent_start(_prompt: String) -> Option<String> {
        None
    }

    fn list_probe_points() -> Vec<String> {
        Vec::new()
    }

    fn probe(_point: String, _payload: String) -> types::Verdict {
        types::Verdict::Continue
    }
}

kiqi_plugin_guest::bindings::export!(AskUserPlugin with_types_in kiqi_plugin_guest::bindings);
