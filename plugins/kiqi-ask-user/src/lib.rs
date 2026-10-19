//! `ask_user` — structured questions backed by the host's interactive UI
//! dialog bridge (runtime action 17).
//!
//! The tool accepts the native Pi shape (`question`, `context`, option objects,
//! `allowMultiple`, `allowFreeform`, `allowComment`, `displayMode`, `timeout`,
//! `suggest`) and the earlier `questions[]` alias. Both normalize into
//! [`AskQuestion`] values, then each question is presented to the host TUI one
//! at a time through [`RUNTIME_ACTION_UI_DIALOG`]. The plugin parks in `poll`
//! until the host answers or cancels, so the model never receives a fabricated
//! answer.
//!
//! A headless host (no attached TUI) rejects `ui_request/open`, and the tool
//! fails with the explicit `ask_user requires an interactive UI` error instead
//! of returning a selector payload that the model could mistake for an answer.
#![forbid(unsafe_code)]

pub mod kiqi;

use kiqi_plugin_sdk::handles::ToolHandles;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kiqi_plugin_sdk::{
    PluginApi, StableToolSchema, StbString, StbStringRef, StepHandle, StepResult, ToolPartialCb,
};
use serde_json::{json, Value};

/// Host runtime-action id for the interactive UI dialog bridge. The SDK pinned
/// by this workspace may predate `RuntimeActionId::UiDialog`, so the raw ABI id
/// is used; the host validates it (`TryFrom<u32>`).
const RUNTIME_ACTION_UI_DIALOG: u32 = 17;

/// How long a single `poll` parks before re-checking the host for an answer.
/// Keeps the driver's spin loop from burning a core while the user thinks.
pub(crate) const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);

/// Upper bound on how long `ask_user` waits for a human when the model does
/// not name a shorter timeout. Without this the wait is unbounded.
///
/// **Threshold change (2026-10-19).**
///
/// - Old value: *no upper bound at all.* `deadline` came only from the model's
///   own `timeout`, so a question without one got `deadline: None` and the
///   plugin could never time out. Measured cost: a single deletion-authorization
///   question blocked one run for about 43 minutes with no session entry
///   (`docs/design/decision-fallback.md` §1).
/// - New value: 60 s. A model-supplied `timeout` still works and still wins when
///   it is *shorter* — this is a ceiling, not a replacement for `timeout`
///   (`effective_timeout_secs` caps, it does not override).
/// - Why 60 s: with nobody at the keyboard the wait has to be finite; 60 s covers
///   "a human is in front of the dialog and answers" for the overwhelming
///   majority of questions, and it is short enough that unattended flows resume
///   instead of stalling for tens of minutes.
/// - Decided in `docs/design/decision-fallback.md` §9 "已定（2026-10-19）" item 1
///   (non-goal mode waits at most 60 s).
const DEFAULT_ANSWER_TIMEOUT_SECS: u64 = 60;

/// Host fn pointers captured once at register time. `runtime_action` lets the
/// tool drive the UI bridge; `user_data` is the host's opaque context.
// Reuse the SDK's host-context lifetime and thread-safety contract.
type HostRuntime = PluginApi;

static HOST_RUNTIME: OnceLock<HostRuntime> = OnceLock::new();

/// Monotonic suffix so concurrent questions never share a `requestId`.
static REQUEST_SEQ: AtomicU64 = AtomicU64::new(1);

// ---------------------------------------------------------------------------
// Typed normalization
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
struct AskOption {
    title: String,
    description: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
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

#[derive(Clone, Debug, PartialEq)]
enum AskPlan {
    Confirm { summary: String },
    Questions { questions: Vec<AskQuestion> },
}

impl AskQuestion {
    /// UI kind the host should render for this question.
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
                let mut value = json!({"title": option.title});
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
    if question.is_empty() {
        return Err("each question must contain 1-500 characters".into());
    }
    if question.chars().count() > 500 {
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
    if let Some(timeout) = timeout {
        if timeout == 0 {
            return Err("timeout must be a positive integer".into());
        }
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

/// Normalize the tool arguments into an [`AskPlan`].
pub(crate) fn ask(params: &Value) -> Result<AskPlan, String> {
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

/// Human-readable transcript text for a plan (used for the `/ask_user` command
/// and as a fallback). Never serialized JSON.
#[cfg_attr(not(test), allow(dead_code))]
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
// Host UI bridge
// ---------------------------------------------------------------------------

fn next_request_id() -> String {
    let seq = REQUEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("ask-{}-{seq}-{nanos}", std::process::id())
}

/// The one host operation ask_user needs.
///
/// Two adapters, so neither channel regresses: the cdylib face keeps the raw
/// `UiDialog` call, the compile-time face goes through `HostCaps::ui`.
pub(crate) trait AskHost {
    fn dialog(&self, args: &Value) -> Result<Value, String>;
}

/// cdylib adapter: the existing `RUNTIME_ACTION_UI_DIALOG` call, unchanged.
struct AbiHost;

impl AskHost for AbiHost {
    fn dialog(&self, args: &Value) -> Result<Value, String> {
        let Some(runtime) = HOST_RUNTIME.get().copied() else {
            return Err("ask_user requires an interactive UI".to_string());
        };
        invoke_ui_dialog_raw(&runtime, args)
    }
}

/// compile-time adapter: the caller already holds the capabilities.
pub(crate) struct CapsHost<'a>(pub &'a kiqi_plugin::HostCaps<'a>);

impl AskHost for CapsHost<'_> {
    fn dialog(&self, args: &Value) -> Result<Value, String> {
        self.0
            .ui
            .ok_or_else(|| "the host did not grant a UI to this plugin".to_string())?
            .dialog(args)
            .map_err(|error| error.to_string())
    }
}

fn invoke_ui_dialog(host: &dyn AskHost, args: &Value) -> Result<Value, String> {
    host.dialog(args)
}

fn invoke_ui_dialog_raw(runtime: &HostRuntime, args: &Value) -> Result<Value, String> {
    let args_json = serde_json::to_string(args).map_err(|error| error.to_string())?;
    let mut output = StbString::empty();
    let rc = (runtime.runtime_action)(
        RUNTIME_ACTION_UI_DIALOG,
        StbStringRef::from_str(&args_json),
        &mut output,
        runtime.user_data,
    );
    let text = output.to_string_lossy();
    (runtime.free_string)(output);
    if rc != 0 {
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            if let Some(error) = value.get("error").and_then(Value::as_str) {
                return Err(error.to_owned());
            }
        }
        return Err(if text.is_empty() {
            format!("ask_user UI dialog failed with status {rc}")
        } else {
            text
        });
    }
    serde_json::from_str(&text)
        .map_err(|error| format!("ask_user UI dialog returned invalid JSON: {error}"))
}

fn cancel_ui_dialog(host: &dyn AskHost, request_id: &str) {
    let _ = invoke_ui_dialog(host, &json!({"op": "cancel", "requestId": request_id}));
}

// ---------------------------------------------------------------------------
// Tool drive
// ---------------------------------------------------------------------------

/// How long this question may stay open: the model's `timeout`, capped at
/// [`DEFAULT_ANSWER_TIMEOUT_SECS`]. A question without an explicit `timeout`
/// used to park here forever, which is what this function exists to prevent.
fn effective_timeout_secs(requested: Option<u64>) -> u64 {
    requested
        .unwrap_or(DEFAULT_ANSWER_TIMEOUT_SECS)
        .min(DEFAULT_ANSWER_TIMEOUT_SECS)
}

struct PendingQuestion {
    request_id: String,
    question: AskQuestion,
    deadline: Instant,
}

struct AskAnswer {
    id: String,
    question: String,
    values: Vec<String>,
    text: Option<String>,
    confirmed: Option<bool>,
}

static HANDLES: ToolHandles<Drive> = ToolHandles::new();

pub(crate) struct Drive {
    params: Value,
    tool_call_id: String,
    started: bool,
    finished: bool,
    pending: Option<PendingQuestion>,
    remaining: std::collections::VecDeque<AskQuestion>,
    answers: Vec<AskAnswer>,
    confirm: Option<String>,
}

impl Drive {
    pub(crate) fn new(params: Value) -> Self {
        Self {
            params,
            tool_call_id: String::new(),
            started: false,
            finished: false,
            pending: None,
            remaining: std::collections::VecDeque::new(),
            answers: Vec::new(),
            confirm: None,
        }
    }
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
        AskPlan::Questions { .. } => {
            if answers.len() == 1 {
                let answer = &answers[0];
                let value = answer
                    .text
                    .clone()
                    .filter(|text| !text.is_empty())
                    .or_else(|| (!answer.values.is_empty()).then(|| answer.values.join(", ")))
                    .unwrap_or_else(|| "(no answer)".to_string());
                format!("{}: {value}", answer.question)
            } else {
                answers
                    .iter()
                    .map(|answer| {
                        let value = answer
                            .text
                            .clone()
                            .filter(|text| !text.is_empty())
                            .or_else(|| {
                                (!answer.values.is_empty()).then(|| answer.values.join(", "))
                            })
                            .unwrap_or_else(|| "(no answer)".to_string());
                        format!("{}: {value}", answer.question)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
    }
}

fn answer_result(answers: &[AskAnswer]) -> Value {
    json!(answers
        .iter()
        .map(|answer| {
            json!({
                "id": answer.id,
                "question": answer.question,
                "values": answer.values,
                "text": answer.text,
                "confirmed": answer.confirmed,
            })
        })
        .collect::<Vec<_>>())
}

fn pending_progress(question: &AskQuestion) -> Value {
    json!({
        "content": [{"type": "text", "text": format!("Waiting for your answer…\n{}", question.question)}],
        "details": {"ui": {"kind": question.ui_kind(), "question": question.question}},
    })
}

/// Open the next queued question, or return `None` when none remain.
fn open_next(drive: &mut Drive, host: &dyn AskHost) -> Result<Option<Value>, String> {
    let Some(question) = drive.remaining.pop_front() else {
        return Ok(None);
    };
    let request_id = next_request_id();
    let request = json!({
        "op": "open",
        "requestId": request_id,
        "toolCallId": drive.tool_call_id,
        "ui": question.to_ui(None),
    });
    let response = invoke_ui_dialog(host, &request)?;
    let deadline = Instant::now() + Duration::from_secs(effective_timeout_secs(question.timeout));
    drive.pending = Some(PendingQuestion {
        request_id,
        question,
        deadline,
    });
    Ok(Some(response))
}

/// Poll the currently open question. `Ok(Some(done))` when the whole plan is
/// answered; `Ok(None)` while still waiting.
pub(crate) fn poll_open_question(
    drive: &mut Drive,
    host: &dyn AskHost,
) -> Result<Option<Value>, String> {
    let Some(pending) = drive.pending.as_ref() else {
        return Ok(None);
    };
    if Instant::now() >= pending.deadline {
        let request_id = pending.request_id.clone();
        cancel_ui_dialog(host, &request_id);
        drive.pending = None;
        return Err("ask timed out".into());
    }
    let request_id = pending.request_id.clone();
    let response = invoke_ui_dialog(host, &json!({"op": "poll", "requestId": request_id}))?;
    match response
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("pending")
    {
        "answered" => {
            let answer = response.get("answer").cloned().unwrap_or(Value::Null);
            let pending = drive.pending.take().expect("checked above");
            let text = answer
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_string);
            let values = answer_values(&answer);
            let confirmed = if pending.question.ui_kind() == "input" {
                None
            } else {
                Some(
                    values
                        .first()
                        .map(|value| value.eq_ignore_ascii_case("yes"))
                        .unwrap_or(false),
                )
            };
            drive.answers.push(AskAnswer {
                id: pending.question.id.clone(),
                question: pending.question.question.clone(),
                values,
                text,
                confirmed,
            });
            // Open the next question, if any.
            if open_next(drive, host)?.is_none() {
                return Ok(Some(build_done(drive)));
            }
            Ok(None)
        }
        "cancelled" => Err("ask cancelled".into()),
        _ => Ok(None),
    }
}

pub(crate) fn build_done(drive: &Drive) -> Value {
    let plan = match ask(&drive.params) {
        Ok(plan) => plan,
        Err(_) => AskPlan::Confirm {
            summary: String::new(),
        },
    };
    let summary = answer_summary(&plan, &drive.answers);
    json!({
        "content": [{"type": "text", "text": summary}],
        "details": {
            "answers": answer_result(&drive.answers),
            "ui": {"kind": "ask_user_result"},
        },
    })
}

pub(crate) fn start_drive(drive: &mut Drive, host: &dyn AskHost) -> Result<Value, String> {
    let plan = ask(&drive.params)?;
    match plan {
        AskPlan::Confirm { summary } => {
            let question = AskQuestion {
                id: "confirm".to_string(),
                question: summary.clone(),
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
            };
            drive.confirm = Some(summary);
            drive.remaining.push_back(question);
        }
        AskPlan::Questions { questions } => {
            for question in questions {
                drive.remaining.push_back(question);
            }
        }
    }
    match open_next(drive, host)? {
        Some(response) if response.get("status").and_then(Value::as_str) == Some("answered") => {
            // Rare immediate answer (also covers replay of a duplicate open).
            match poll_open_question(drive, host)? {
                Some(done) => Ok(done),
                None => Ok(json!(null)),
            }
        }
        Some(_) | None => Ok(json!(null)),
    }
}

// ---------------------------------------------------------------------------
// FFI lifecycle
// ---------------------------------------------------------------------------

kiqi_plugin_sdk::execute_handler! {
fn execute(tool_call_id, params, free) {
    let tool_call_id = tool_call_id.to_owned();
    let text = params.to_string_lossy();
    params.free_with(free);
    HANDLES.insert(Drive {
        params: serde_json::from_str(&text).unwrap_or(Value::Null),
        tool_call_id,
        started: false,
        finished: false,
        pending: None,
        remaining: std::collections::VecDeque::new(),
        answers: Vec::new(),
        confirm: None,
    })
}
}

extern "C" fn poll(handle: StepHandle, _: Option<ToolPartialCb>, _: *mut c_void) -> StepResult {
    if handle.is_null() {
        return StepResult::err(StbString::from_string("null ask handle".into()));
    }
    let Some(entry) = HANDLES.get(handle) else {
        return StepResult::err(StbString::from_string(
            "invalid or destroyed tool handle".into(),
        ));
    };
    let mut drive = entry.lock();
    let Some(runtime) = HOST_RUNTIME.get().copied() else {
        return StepResult::err(StbString::from_string(
            "ask_user requires an interactive UI".into(),
        ));
    };
    if entry.cancelled.load(Ordering::SeqCst) {
        if let Some(pending) = drive.pending.as_ref() {
            cancel_ui_dialog(&AbiHost, &pending.request_id);
        }
        return StepResult::err(StbString::from_string("ask cancelled".into()));
    }
    if drive.finished {
        return StepResult::err(StbString::from_string(
            "ask_user polled after completion".into(),
        ));
    }

    if !drive.started {
        drive.started = true;
        return match start_drive(&mut drive, &AbiHost) {
            Ok(done) if !done.is_null() => {
                drive.finished = true;
                StepResult::done(StbString::from_string(done.to_string()))
            }
            Ok(_) => {
                let question = drive
                    .pending
                    .as_ref()
                    .map(|pending| pending.question.clone());
                let progress = question
                    .map(|question| pending_progress(&question))
                    .unwrap_or_else(
                        || json!({"content":[{"type":"text","text":"Waiting for your answer…"}]}),
                    );
                StepResult::pending(StbString::from_string(progress.to_string()))
            }
            Err(error) => StepResult::err(StbString::from_string(error)),
        };
    }

    match poll_open_question(&mut drive, &AbiHost) {
        Ok(Some(done)) => {
            drive.finished = true;
            StepResult::done(StbString::from_string(done.to_string()))
        }
        Ok(None) => {
            // Park briefly so the driver's spin loop does not burn a core.
            std::thread::sleep(POLL_INTERVAL);
            if entry.cancelled.load(Ordering::SeqCst) {
                if let Some(pending) = drive.pending.as_ref() {
                    cancel_ui_dialog(&AbiHost, &pending.request_id);
                }
                return StepResult::err(StbString::from_string("ask cancelled".into()));
            }
            StepResult::pending(StbString::from_string(String::new()))
        }
        Err(error) => StepResult::err(StbString::from_string(error)),
    }
}

extern "C" fn cancel(handle: StepHandle) {
    if let Some(entry) = HANDLES.get(handle) {
        entry.cancel();
        // Serialize access to pending UI state with poll, after signalling it.
        let drive = entry.lock();
        if let Some(runtime) = HOST_RUNTIME.get() {
            if let Some(pending) = drive.pending.as_ref() {
                cancel_ui_dialog(&AbiHost, &pending.request_id);
            }
        }
    }
}

extern "C" fn destroy(handle: StepHandle) {
    drop(HANDLES.remove(handle));
}

use kiqi_plugin_sdk::callbacks::free_string;

fn command_output(out: &mut kiqi_plugin_sdk::callbacks::Output<'_>, value: Value) -> i32 {
    if !out.is_available() {
        return 1;
    }
    out.write(value.to_string());
    0
}
kiqi_plugin_sdk::string_handler! {
fn ask_command(args_json, out, _user_data) {
    let request_text = args_json.to_owned();
    let request: Value = serde_json::from_str(&request_text).unwrap_or(Value::Null);
    let raw = request.get("args").and_then(Value::as_str).unwrap_or("");
    command_output(out, run_command(raw))
}
}

/// `/ask_user` command body, shared by both channels.
///
/// It never touches the host: it answers with selector/editor JSON for the host
/// to render, which is why this path needs no capability at all.
pub(crate) fn run_command(args: &str) -> Value {
    let params: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({"question": args}));
    if params.get("action").and_then(Value::as_str) == Some("select")
        || params.get("action").and_then(Value::as_str) == Some("input")
    {
        let value = params.get("value").and_then(Value::as_str).unwrap_or("");
        return json!({"kind":"message","text":format!("Selected: {value}")});
    }
    let plan = match ask(&params) {
        Ok(plan) => plan,
        Err(error) => {
            return json!({"kind":"message","text":format!("ask_user failed: {error}")});
        }
    };
    match plan {
        AskPlan::Confirm { summary } => json!({"kind":"selector","items":[
            {"value":"Yes","label":"Yes","description":summary},
            {"value":"No","label":"No"}
        ]}),
        AskPlan::Questions { questions } => {
            let first = &questions[0];
            if first.options.is_empty() {
                return json!({"kind":"editor","initialText": first.suggest.clone().unwrap_or_default()});
            }
            let items = first
                .options
                .iter()
                .map(|option| {
                    let mut item = json!({"value": option.title, "label": option.title});
                    if let Some(description) = &option.description {
                        item["description"] = json!(description);
                    }
                    item
                })
                .collect::<Vec<_>>();
            json!({"kind":"selector","items":items})
        }
    }
}

kiqi_plugin_sdk::export_plugin!(|api| {
    let Some(register) = api.register_tool else {
        return 1;
    };
    let runtime = *api;
    let _ = HOST_RUNTIME.set(runtime);
    let schema = Box::new(StableToolSchema { name: StbString::from_string("ask_user".into()), description: StbString::from_string("Ask the user an interactive question with selectable options or freeform input.".into()), parameters: StbString::from_string(r#"{"type":"object","properties":{"question":{"type":"string"},"context":{"type":"string"},"options":{"type":"array","items":{"oneOf":[{"type":"string"},{"type":"object"}]}},"allowMultiple":{"type":"boolean"},"allowFreeform":{"type":"boolean"},"allowComment":{"type":"boolean"},"displayMode":{"type":"string","enum":["overlay","inline"]},"timeout":{"type":"integer","minimum":1},"type":{"type":"string","enum":["question","confirm"]},"questions":{"type":"array","minItems":1,"maxItems":3},"suggest":{"type":"string"},"summary":{"type":"string"}},"additionalProperties":false}"#.into()) });
    let rc = register(&*schema, execute, poll, cancel, destroy, free_string);
    drop(schema);
    if let Some(register_command) = api.register_command {
        let name = StbStringRef::from_str("ask_user");
        let description = StbStringRef::from_str("Ask a question with the interactive Pi selector");
        let _ = register_command(name, description, ask_command);
    }
    rc
});

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(params: Value) -> AskPlan {
        ask(&params).unwrap()
    }

    #[test]
    fn effective_timeout_defaults_to_sixty_seconds() {
        assert_eq!(effective_timeout_secs(None), 60);
    }

    #[test]
    fn effective_timeout_honours_a_shorter_request() {
        assert_eq!(effective_timeout_secs(Some(30)), 30);
    }

    #[test]
    fn effective_timeout_clamps_a_longer_request() {
        assert_eq!(effective_timeout_secs(Some(300)), 60);
    }

    #[test]
    fn effective_timeout_keeps_the_boundary_value() {
        assert_eq!(effective_timeout_secs(Some(60)), 60);
    }

    #[test]
    fn normalizes_native_single_question() {
        let AskPlan::Questions { questions } = plan(json!({
            "question": "Which target?",
            "options": [{"title": "Linux", "description": "glibc"}, "Windows"],
            "allowMultiple": true,
            "allowFreeform": false,
            "context": "release",
            "header": "Build",
            "suggest": "linux/amd64",
            "displayMode": "inline",
            "timeout": 30
        })) else {
            panic!("expected questions");
        };
        assert_eq!(questions.len(), 1);
        let question = &questions[0];
        assert_eq!(question.question, "Which target?");
        assert_eq!(question.header.as_deref(), Some("Build"));
        assert_eq!(question.context.as_deref(), Some("release"));
        assert_eq!(question.options[0].title, "Linux");
        assert_eq!(question.options[0].description.as_deref(), Some("glibc"));
        assert_eq!(question.options[1].title, "Windows");
        assert!(question.allow_multiple);
        assert!(!question.allow_freeform);
        assert_eq!(question.suggest.as_deref(), Some("linux/amd64"));
        assert_eq!(question.display_mode.as_deref(), Some("inline"));
        assert_eq!(question.timeout, Some(30));
    }

    #[test]
    fn normalizes_questions_alias_with_ids_and_per_question_defaults() {
        let AskPlan::Questions { questions } = plan(json!({
            "allowFreeform": true,
            "header": "Questions",
            "questions": [
                {"id": "proxy_port", "question": "端口?", "options": [], "suggest": "7890"},
                {"id": "level", "question": "Level?", "options": ["a", "b"], "allowMultiple": true}
            ]
        })) else {
            panic!("expected questions");
        };
        assert_eq!(questions.len(), 2);
        assert_eq!(questions[0].id, "proxy_port");
        assert!(questions[0].allow_freeform);
        assert_eq!(questions[0].suggest.as_deref(), Some("7890"));
        assert_eq!(questions[0].header.as_deref(), Some("Questions"));
        assert_eq!(questions[1].id, "level");
        assert!(questions[1].allow_multiple);
    }

    #[test]
    fn confirm_plan() {
        let plan = plan(json!({"type": "confirm", "summary": "Deploy now?"}));
        assert_eq!(
            plan,
            AskPlan::Confirm {
                summary: "Deploy now?".into()
            }
        );
    }

    #[test]
    fn rejects_invalid_input() {
        assert!(ask(&json!({"questions": []})).is_err());
        assert!(ask(&json!({"questions": [{"question": "ok"}, {"question": "ok"}, {"question": "ok"}, {"question": "ok"}]})).is_err());
        assert!(ask(&json!({"question": "x", "displayMode": "huge"})).is_err());
        assert!(ask(&json!({"question": "x", "timeout": 0})).is_err());
        assert!(ask(&json!({"question": "x", "options": [{"description": "no title"}]})).is_err());
        assert!(
            ask(&json!({"question": "x", "options": [{"title": "x", "description": 42}]})).is_err()
        );
    }

    #[test]
    fn display_text_is_human_readable() {
        let plan = plan(json!({
            "question": "Which target?",
            "options": ["Linux", {"title": "Windows"}]
        }));
        let text = plan_display_text(&plan);
        assert_eq!(text, "Which target?\nChoices: Linux, Windows");
        assert!(!text.trim_start().starts_with('{'));
    }

    #[test]
    fn answer_summary_uses_values_and_text() {
        let ask_plan = plan(json!({"question": "Port?", "suggest": "7890"}));
        let answers = vec![AskAnswer {
            id: "q1".into(),
            question: "Port?".into(),
            values: vec![],
            text: Some("7897".into()),
            confirmed: None,
        }];
        assert_eq!(answer_summary(&ask_plan, &answers), "Port?: 7897");

        let confirm = plan(json!({"type": "confirm", "summary": "Go?"}));
        let answers = vec![AskAnswer {
            id: "confirm".into(),
            question: "Go?".into(),
            values: vec!["No".into()],
            text: None,
            confirmed: Some(false),
        }];
        assert_eq!(answer_summary(&confirm, &answers), "Declined: Go?");
    }
}
