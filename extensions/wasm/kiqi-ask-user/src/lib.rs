//! `kiqi-ask-user` ported to the wasm ABI.
//!
//! Same argument normalization as the compile-time extension; the interaction is
//! the difference. A C ABI cannot block, so the native extension needs a
//! start/poll/cancel state machine driven by the host. Here `call_tool` blocks,
//! so the dialog is a straight open → poll → answer loop, with cancellation and
//! `timeout` handled inline.
//!
//! A question with no explicit `timeout` waits for the human indefinitely: the
//! person reading the dialog is the only one who can answer it, and an answer
//! that arrives after a host-imposed deadline used to be dropped on the floor
//! (the run had already continued with a fallback, so clicking an option did
//! nothing). Only the model's own `timeout` bounds the wait, and it produces the
//! structured `unanswered` result plus the conservative `keep` fallback.
//!
//! ## Why there is no `KI_DEBUG_DECISION` audit here
//!
//! The compile-time extension can append a timeout line to
//! `<temp>/kiqi-decision.log` behind an env gate. A guest cannot: `world extension`
//! (`extensions/kiqi-extension.wit`) imports no `wasi:cli/environment`, so the guest
//! has no environment to gate on, and its `fs` capability is jailed to the
//! extension's own data directory, so it cannot open a host temp path either.
//! Emitting through the always-granted `log` import instead would be an
//! unconditional log line rather than a gated audit, so the audit channel stays
//! host-side only; the guest's evidence is the `unanswered` details it returns.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kiqi_wasm_bindings::bindings::exports::kiqi::extension::extension_api::Guest;
use kiqi_wasm_bindings::bindings::kiqi::extension::{proc, types, ui};
use serde_json::{json, Value};

const TOOL: &str = "ask_user";
const DESCRIPTION: &str =
    "Ask the user an interactive question with selectable options or freeform input.";
const SCHEMA: &str = r#"{"type":"object","properties":{"question":{"type":"string"},"context":{"type":"string"},"options":{"type":"array","items":{"oneOf":[{"type":"string"},{"type":"object"}]}},"allowMultiple":{"type":"boolean"},"allowFreeform":{"type":"boolean"},"allowComment":{"type":"boolean"},"displayMode":{"type":"string","enum":["overlay","inline"]},"timeout":{"type":"integer","minimum":1},"type":{"type":"string","enum":["question","confirm"]},"questions":{"type":"array","minItems":1,"maxItems":3},"suggest":{"type":"string"},"summary":{"type":"string"}},"additionalProperties":false}"#;

/// How long a poll sleeps before re-checking the host for an answer.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Cap idle polling backoff so a long human wait does not burn wasm/host calls.
const MAX_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How long this question may stay open before the guest answers `unanswered`.
///
/// **No default (2026-10-28).** There used to be a 60 s ceiling, added so an
/// unattended run could not park on a dialog forever. In an interactive session
/// that ceiling is wrong: a human reading a list of options routinely needs
/// more than a minute, and when it fired the dialog stayed on screen while the
/// run went on with the `keep` fallback — so the user's eventual click was
/// answered into a cancelled request and silently discarded, which looks
/// exactly like "ki stopped after I answered". A run with nobody to ask needs no
/// TTL either: without an attached interactive UI the host rejects `open`
/// outright ("ask_user requires an interactive UI"), so a headless/goal run
/// fails the call immediately instead of parking on it.
fn answer_deadline(requested: Option<u64>) -> Option<Instant> {
    // A duration beyond `Instant`'s representable range is effectively an
    // unbounded wait, not a reason to trap after opening the dialog.
    requested.and_then(|secs| Instant::now().checked_add(Duration::from_secs(secs)))
}

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
    Confirm {
        summary: String,
        timeout: Option<u64>,
    },
    Questions {
        questions: Vec<AskQuestion>,
    },
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
        let timeout = params.get("timeout").and_then(Value::as_u64);
        if timeout == Some(0) {
            return Err("timeout must be a positive integer".into());
        }
        return Ok(AskPlan::Confirm { summary, timeout });
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
        AskPlan::Confirm { summary, .. } => format!("Confirmation requested: {summary}"),
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

/// Outcome of one question.
///
/// `Unanswered` is the TTL firing: nobody replied in time. It is a result, not
/// a failure — the caller turns it into a structured `unanswered` tool result
/// so the model keeps going with the conservative fallback.
enum AskOne {
    Answered(AskAnswer),
    Unanswered,
}

/// Bytes of the question kept in the structured `unanswered` payload.
const UNANSWERED_QUESTION_BYTES: usize = 200;

/// First `limit` bytes of `text`, cut on a char boundary so slicing never panics
/// on multi-byte input (a Chinese question is 3 bytes per char).
fn truncate_on_char_boundary(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// The answer to assume when nobody replied in time. Deliberately the most
/// conservative outcome: keep everything as it is. There is no per-question
/// `fallback` field in the schema yet (adding one changes both sides and
/// `additionalProperties:false`), so this is the host's default for now.
fn conservative_fallback(_question: &AskQuestion) -> &'static str {
    "keep"
}

/// Successful result for a question nobody answered in time.
///
/// Deliberately carries no `answer` key: a timeout is not consent, and the model
/// must not be able to read one into it.
fn unanswered_result(question: &AskQuestion) -> types::ToolResult {
    let fallback = conservative_fallback(question);
    types::ToolResult {
        blocks: vec![types::ContentBlock::Text(format!(
            "ask_user is unanswered (timeout): nobody replied to:\n{}\n\
             Nothing was answered or approved. Assuming `{fallback}` (keep everything as it is); \
             this is not user consent.",
            question.question
        ))],
        is_error: false,
        details: json!({
            "status": "unanswered",
            "reason": "timeout",
            "fallback": fallback,
            "question": truncate_on_char_boundary(&question.question, UNANSWERED_QUESTION_BYTES),
        })
        .to_string(),
    }
}

/// Best-effort terminal transition followed by the poll that reclaims the
/// mailbox entry. Cancellation itself deliberately leaves the terminal state
/// for one poll; returning without that poll would retain every timed-out ID.
fn cancel_and_reap(request_id: &str) {
    let cancel = json!({ "op": "cancel", "requestId": request_id }).to_string();
    let _ = ui::dialog(&cancel);
    let poll = json!({ "op": "poll", "requestId": request_id }).to_string();
    let _ = ui::dialog(&poll);
}

/// Open one question and wait for its answer. `Err` on cancel (unchanged); a
/// timeout comes back as [`AskOne::Unanswered`], not an error.
fn ask_one(question: &AskQuestion) -> Result<AskOne, String> {
    let request_id = next_request_id();
    let open = json!({
        "op": "open",
        "requestId": request_id,
        "toolCallId": "",
        "ui": question.to_ui(None),
    })
    .to_string();
    ui::dialog(&open).map_err(capability_reason)?;

    let deadline = answer_deadline(question.timeout);
    let mut poll_interval = POLL_INTERVAL;
    loop {
        // Host abort is advisory: a guest that blocks in `call_tool` must poll
        // it explicitly or RPC/TUI cancellation cannot settle the run.
        if proc::cancel_requested() {
            cancel_and_reap(&request_id);
            return Err("ask cancelled by the host".to_string());
        }

        // Even after the deadline, poll once before cancelling. An answer
        // recorded during the last sleep but before the deadline is already in
        // the mailbox; cancelling first would erase that timely answer.
        let timed_out = deadline.is_some_and(|deadline| Instant::now() >= deadline);
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
                return Ok(AskOne::Answered(AskAnswer {
                    id: question.id.clone(),
                    question: question.question.clone(),
                    values,
                    text: answer
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    confirmed,
                }));
            }
            "cancelled" => return Err("ask cancelled".to_string()),
            _ if timed_out => {
                cancel_and_reap(&request_id);
                return Ok(AskOne::Unanswered);
            }
            _ => {
                let mut sleep_for = poll_interval;
                if let Some(deadline) = deadline {
                    sleep_for = sleep_for.min(deadline.saturating_duration_since(Instant::now()));
                }
                std::thread::sleep(sleep_for);
                poll_interval = (poll_interval * 2).min(MAX_POLL_INTERVAL);
            }
        }
    }
}

fn confirm_question(summary: &str, timeout: Option<u64>) -> AskQuestion {
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
        timeout,
    }
}

fn answer_summary(plan: &AskPlan, answers: &[AskAnswer]) -> String {
    match plan {
        AskPlan::Confirm { summary, .. } => {
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
// Extension surface
// ---------------------------------------------------------------------------

struct AskUserExtension;

impl Guest for AskUserExtension {
    /// No selector/editor/prompt flow exists in this extension yet; a reply is
    /// refused rather than silently dropped.
    fn continue_command(
        _name: String,
        _flow_id: String,
        _reply: types::CommandReply,
    ) -> Result<types::CommandResult, types::ExtensionError> {
        Err(types::ExtensionError::Failed(
            "this extension declares no command continuations".to_string(),
        ))
    }

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

    fn call_tool(
        name: String,
        arguments: String,
    ) -> Result<types::ToolResult, types::ExtensionError> {
        if name != TOOL {
            return Err(types::ExtensionError::UnknownTool(name));
        }
        let params: Value = serde_json::from_str(&arguments).unwrap_or(Value::Null);
        // Reject a malformed plan before opening anything.
        let plan = ask(&params).map_err(types::ExtensionError::InvalidArguments)?;

        let questions = match &plan {
            AskPlan::Confirm { summary, timeout } => vec![confirm_question(summary, *timeout)],
            AskPlan::Questions { questions } => questions.clone(),
        };
        let mut answers = Vec::new();
        for question in &questions {
            match ask_one(question) {
                Ok(AskOne::Answered(answer)) => answers.push(answer),
                // Nobody replied in time: finish the whole call as `unanswered`
                // rather than asking the remaining questions into the void.
                Ok(AskOne::Unanswered) => return Ok(unanswered_result(question)),
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

    fn run_command(
        name: String,
        arguments: String,
    ) -> Result<types::CommandResult, types::ExtensionError> {
        if name != TOOL {
            return Err(types::ExtensionError::UnknownTool(name));
        }
        let params: Value =
            serde_json::from_str(&arguments).unwrap_or(json!({ "question": arguments }));
        let plan = ask(&params).map_err(types::ExtensionError::InvalidArguments)?;
        Ok(types::CommandResult {
            flow_id: String::new(),
            body: types::CommandResultBody::Message(plan_display_text(&plan)),
        })
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

kiqi_wasm_bindings::bindings::export!(AskUserExtension with_types_in kiqi_wasm_bindings::bindings);

// The 0.8.0 contribution exports are mandatory world exports; deny them all.
kiqi_wasm_bindings::deny_contribution_exports!(AskUserExtension);

#[cfg(test)]
mod tests {
    use super::{answer_deadline, ask, confirm_question, AskPlan};
    use serde_json::json;

    #[test]
    fn a_question_without_a_timeout_waits_for_the_human() {
        assert!(answer_deadline(None).is_none());
        assert!(answer_deadline(Some(5)).is_some());
        assert!(
            answer_deadline(Some(u64::MAX)).is_none(),
            "an unrepresentably distant deadline is effectively unbounded, not a panic"
        );
    }

    #[test]
    fn a_confirm_preserves_its_explicit_timeout() {
        let plan = ask(&json!({"type": "confirm", "summary": "Deploy?", "timeout": 5}))
            .expect("valid confirmation");
        let AskPlan::Confirm { summary, timeout } = plan else {
            panic!("expected a confirmation");
        };
        assert_eq!(summary, "Deploy?");
        assert_eq!(timeout, Some(5));
        assert_eq!(confirm_question(&summary, timeout).timeout, Some(5));

        assert!(ask(&json!({"type": "confirm", "summary": "Deploy?", "timeout": 0})).is_err());
    }
}
