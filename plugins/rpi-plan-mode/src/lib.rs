#![forbid(unsafe_code)]
pub mod kiqi;

use rpi_plugin_sdk::handles::ToolHandles;
use rpi_plugin_sdk::{
    EventHandlerFn, EventTag, FreeStringFn, PluginApi, RuntimeActionId, StablePluginEvent,
    StableToolSchema, StbString, StbStringRef, StepHandle, StepResult, ToolPartialCb,
};
use serde_json::{json, Value};
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

const MAX_PLAN_CHARS: usize = 50_000;
const PLAN_TOOL: &str = "plan_mode_complete";
const START_TOOL: &str = "plan_mode_start";
const PLAN_ENTRY: &str = "plan-mode";

// Reuse the SDK's host-context lifetime and thread-safety contract.
type Runtime = PluginApi;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static STATE: OnceLock<Mutex<PlanState>> = OnceLock::new();

struct PlanState {
    active: bool,
    plan: Option<String>,
    previous_tools: Option<Vec<String>>,
}

fn state() -> &'static Mutex<PlanState> {
    STATE.get_or_init(|| {
        Mutex::new(PlanState {
            active: false,
            plan: None,
            previous_tools: None,
        })
    })
}

fn call_runtime(id: RuntimeActionId, args: Value) -> Result<Value, String> {
    let runtime = *RUNTIME.get().ok_or("plan mode runtime is unavailable")?;
    let input = args.to_string();
    let mut output = StbString::empty();
    let rc = (runtime.runtime_action)(
        id as u32,
        StbStringRef::from_str(&input),
        &mut output,
        runtime.user_data,
    );
    let text = output.to_string_lossy();
    (runtime.free_string)(output);
    if rc != 0 {
        return Err(if text.is_empty() {
            format!("runtime action {id:?} failed")
        } else {
            text
        });
    }
    if text.trim().is_empty() {
        Ok(Value::Null)
    } else {
        serde_json::from_str(&text).map_err(|e| format!("invalid runtime response: {e}"))
    }
}

/// The host operations plan mode needs.
///
/// Two adapters, so neither plugin channel regresses: the cdylib face keeps
/// going through `call_runtime`, and the compile-time face goes through
/// `HostCaps`. Nothing synthesises a `PluginApi` vtable for Rust.
pub(crate) trait PlanHost {
    fn active_tools(&self) -> Result<Vec<String>, String>;
    fn set_active_tools(&self, tools: &[String]) -> Result<(), String>;
    fn session_branch(&self) -> Result<Value, String>;
    fn append_custom_entry(&self, custom_type: &str, data: &Value) -> Result<(), String>;
    fn send_user_message(&self, text: &str) -> Result<(), String>;
}

/// cdylib adapter: the existing C-ABI path, unchanged.
struct AbiHost;

impl PlanHost for AbiHost {
    fn active_tools(&self) -> Result<Vec<String>, String> {
        let value = call_runtime(RuntimeActionId::GetActiveTools, json!({}))?;
        value
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| "runtime did not return an active tool list".to_string())
            .and_then(|tools| {
                tools
                    .iter()
                    .map(|tool| {
                        tool.as_str().map(str::to_owned).ok_or_else(|| {
                            "active tool list contains a non-string value".to_string()
                        })
                    })
                    .collect()
            })
    }

    fn set_active_tools(&self, tools: &[String]) -> Result<(), String> {
        call_runtime(RuntimeActionId::SetActiveTools, json!({"tools": tools}))?;
        Ok(())
    }

    fn session_branch(&self) -> Result<Value, String> {
        call_runtime(RuntimeActionId::GetSessionBranch, json!({}))
    }

    fn append_custom_entry(&self, custom_type: &str, data: &Value) -> Result<(), String> {
        call_runtime(
            RuntimeActionId::AppendEntry,
            json!({"customType": custom_type, "data": data}),
        )?;
        Ok(())
    }

    fn send_user_message(&self, text: &str) -> Result<(), String> {
        call_runtime(RuntimeActionId::SendUserMessage, json!({"text": text}))?;
        Ok(())
    }
}

/// compile-time adapter: the caller already holds the capabilities.
pub(crate) struct CapsHost<'a>(pub &'a kiqi_plugin::HostCaps<'a>);

impl PlanHost for CapsHost<'_> {
    fn active_tools(&self) -> Result<Vec<String>, String> {
        self.0
            .session
            .map(|session| session.active_tools())
            .ok_or_else(|| "the host did not grant session read to this plugin".to_string())
    }

    fn set_active_tools(&self, tools: &[String]) -> Result<(), String> {
        self.0
            .control
            .ok_or_else(|| "the host did not grant session control to this plugin".to_string())?
            .set_active_tools(tools)
            .map_err(|error| error.to_string())
    }

    fn session_branch(&self) -> Result<Value, String> {
        self.0
            .session
            .map(|session| session.session_branch())
            .ok_or_else(|| "the host did not grant session read to this plugin".to_string())
    }

    fn append_custom_entry(&self, custom_type: &str, data: &Value) -> Result<(), String> {
        self.0
            .inject
            .ok_or_else(|| "the host did not grant message injection to this plugin".to_string())?
            .append_custom_entry(custom_type, data)
            .map_err(|error| error.to_string())
    }

    fn send_user_message(&self, text: &str) -> Result<(), String> {
        self.0
            .inject
            .ok_or_else(|| "the host did not grant message injection to this plugin".to_string())?
            .send_user_message(text)
            .map_err(|error| error.to_string())
    }
}

fn current_tools(host: &dyn PlanHost) -> Result<Vec<String>, String> {
    host.active_tools()
}

fn set_tools(host: &dyn PlanHost, tools: &[String]) -> Result<(), String> {
    host.set_active_tools(tools)
}

fn persist_state(host: &dyn PlanHost, active: bool, plan: Option<&str>) {
    let _ = host.append_custom_entry(PLAN_ENTRY, &json!({"active": active, "plan": plan}));
}

fn restore_state(host: &dyn PlanHost) {
    let Ok(value) = host.session_branch() else {
        return;
    };
    let mut restored = None;
    if let Some(entries) = value.get("entries").and_then(Value::as_array) {
        for entry in entries {
            if entry.get("type").and_then(Value::as_str) == Some("custom")
                && entry.get("customType").and_then(Value::as_str) == Some(PLAN_ENTRY)
            {
                restored = entry.get("data").cloned();
            }
        }
    }
    if let Some(data) = restored {
        let mut guard = state().lock().unwrap_or_else(|p| p.into_inner());
        guard.active = data.get("active").and_then(Value::as_bool).unwrap_or(false);
        guard.plan = data.get("plan").and_then(Value::as_str).map(str::to_owned);
    }
}

/// Session start / tree change body, shared by both channels.
pub(crate) fn on_session_event(host: &dyn PlanHost) {
    restore_state(host);
}

extern "C" fn handle_event(event: StablePluginEvent, _: *mut c_void) -> i32 {
    if matches!(event.tag, EventTag::SessionStart | EventTag::SessionTree) {
        on_session_event(&AbiHost);
    }
    0
}

fn planning_tools() -> Vec<String> {
    vec![
        "read".to_string(),
        "bash".to_string(),
        "docs".to_string(),
        PLAN_TOOL.to_string(),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlanItem {
    number: usize,
    text: String,
    completed: bool,
}

fn parse_plan_items(plan: &str) -> Vec<PlanItem> {
    let mut items: Vec<PlanItem> = Vec::new();
    let mut done = std::collections::HashSet::new();
    for line in plan.lines() {
        let line = line.trim();
        if let Some(index) = line
            .strip_prefix("[DONE:")
            .and_then(|value| value.strip_suffix(']'))
        {
            if let Ok(index) = index.trim().parse::<usize>() {
                done.insert(index);
            }
        }
    }
    for line in plan.lines() {
        let line = line.trim();
        if line.starts_with("[DONE:") {
            continue;
        }

        let mut candidate = line;
        let checked = if let Some(value) = candidate.strip_prefix("- [x] ") {
            candidate = value;
            true
        } else if let Some(value) = candidate.strip_prefix("- [X] ") {
            candidate = value;
            true
        } else if let Some(value) = candidate.strip_prefix("- [ ] ") {
            candidate = value;
            false
        } else {
            false
        };
        let Some((number, text)) = candidate.split_once('.') else {
            continue;
        };
        let Ok(number) = number.trim().parse::<usize>() else {
            continue;
        };
        let text = text.trim();
        if !text.is_empty() {
            let completed = checked || done.contains(&number);
            if let Some(existing) = items.iter_mut().find(|item| item.number == number) {
                existing.completed |= completed;
            } else {
                items.push(PlanItem {
                    number,
                    text: text.to_string(),
                    completed,
                });
            }
        }
    }
    items
}

fn plan_outline(plan: &str) -> String {
    let items = parse_plan_items(plan);
    if items.is_empty() {
        return "(Plan saved; use `/plan show` to view the full plan.)".to_string();
    }
    let total = items.len();
    let completed = items.iter().filter(|item| item.completed).count();
    let mut lines = vec![format!("Progress: {completed}/{total} complete")];
    for item in items.iter().take(12) {
        let marker = if item.completed { '✓' } else { '○' };
        lines.push(format!("- {marker} {}. {}", item.number, item.text));
    }
    if total > 12 {
        lines.push("... (use `/plan show` to view the remaining steps)".to_string());
    }
    lines.join(
        "
",
    )
}

fn markdown_plan(plan: Option<&str>, active: bool) -> String {
    let (status, hint) = if active {
        (
            "Planning",
            "Explore first, then call `plan_mode_complete` when the plan is ready.",
        )
    } else if plan.is_some_and(|plan| !plan.trim().is_empty()) {
        (
            "Ready",
            "Use `/plan start` or `plan_mode_start` to create a new plan.",
        )
    } else {
        ("Idle", "Use `/plan start` to begin planning.")
    };
    let body = match plan {
        Some(plan) if !plan.trim().is_empty() => plan.trim().to_string(),
        _ => "_No completed plan yet._".to_string(),
    };
    format!("**Plan · {status}**\n\n{body}\n\n{hint}")
}

fn activate(host: &dyn PlanHost) -> Result<bool, String> {
    let mut guard = state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.active {
        return Ok(false);
    }
    let previous = current_tools(host)?;
    set_tools(host, &planning_tools())?;
    guard.previous_tools = Some(previous);
    guard.active = true;
    guard.plan = None;
    drop(guard);
    persist_state(host, true, None);
    Ok(true)
}

fn deactivate(host: &dyn PlanHost) -> Result<bool, String> {
    let mut guard = state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !guard.active {
        return Ok(false);
    }
    if let Some(previous) = guard.previous_tools.take() {
        set_tools(host, &previous)?;
    }
    guard.active = false;
    let plan = guard.plan.clone();
    drop(guard);
    persist_state(host, false, plan.as_deref());
    Ok(true)
}

fn plan_status() -> String {
    let guard = state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    markdown_plan(guard.plan.as_deref(), guard.active)
}

pub(crate) fn start_plan(host: &dyn PlanHost, raw: &Value) -> Result<String, String> {
    let request = raw
        .get("request")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|request| !request.is_empty());
    activate(host)?;
    let message = request
        .map(|value| format!("Plan mode started for: {value}"))
        .unwrap_or_else(|| {
            "Plan mode started. Inspect the repository, then call `plan_mode_complete`.".to_string()
        });
    Ok(json!({
        "content": [{"type": "text", "text": message}],
        "details": {"kind": "plan", "active": true}
    })
    .to_string())
}

fn complete_plan(host: &dyn PlanHost, raw: &Value) -> Result<String, String> {
    let plan = raw
        .get("plan")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|plan| !plan.is_empty())
        .ok_or("plan must be a non-empty string")?;
    if plan.chars().count() > MAX_PLAN_CHARS {
        return Err(format!("plan must not exceed {MAX_PLAN_CHARS} characters"));
    }
    {
        let guard = state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !guard.active {
            return Err("plan_mode_complete is only available while plan mode is active".into());
        }
    }
    {
        let mut guard = state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.plan = Some(plan.to_string());
    }
    deactivate(host)?;
    Ok(json!({
        "content": [{"type": "text", "text": markdown_plan(Some(plan), false)}],
        "details": {"kind": "plan", "plan": plan, "outline": plan_outline(plan), "active": false, "markdown": true}
    })
    .to_string())
}

static HANDLES: ToolHandles<Drive> = ToolHandles::new();

struct Drive {
    params: Value,
    done: bool,
}

extern "C" fn execute(
    _: StbStringRef,
    params: StbString,
    free: Option<FreeStringFn>,
) -> StepHandle {
    let text = params.to_string_lossy();
    params.free_with(free);
    HANDLES.insert(Drive {
        params: serde_json::from_str(&text).unwrap_or(Value::Null),
        done: false,
    })
}

extern "C" fn poll(handle: StepHandle, _: Option<ToolPartialCb>, _: *mut c_void) -> StepResult {
    if handle.is_null() {
        return StepResult::err(StbString::from_string("null plan handle".into()));
    }
    let Some(entry) = HANDLES.get(handle) else {
        return StepResult::err(StbString::from_string(
            "invalid or destroyed tool handle".into(),
        ));
    };
    let mut drive = entry.lock();
    if entry.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
        drive.done = true;
        return StepResult::err(StbString::from_string("plan cancelled".into()));
    }
    if drive.done {
        return StepResult::err(StbString::from_string(
            "plan polled after completion".into(),
        ));
    }
    drive.done = true;
    let result = if drive.params.get("plan").is_some() {
        complete_plan(&AbiHost, &drive.params)
    } else {
        start_plan(&AbiHost, &drive.params)
    };
    match result {
        Ok(value) => StepResult::done(StbString::from_string(value)),
        Err(error) => StepResult::err(StbString::from_string(error)),
    }
}

extern "C" fn cancel(handle: StepHandle) {
    HANDLES.cancel(handle);
}

extern "C" fn destroy(handle: StepHandle) {
    drop(HANDLES.remove(handle));
}

use rpi_plugin_sdk::callbacks::free_string;

fn command_output(out: &mut rpi_plugin_sdk::callbacks::Output<'_>, value: Value) -> i32 {
    if !out.is_available() {
        return 1;
    }
    out.write(value.to_string());
    0
}

rpi_plugin_sdk::string_handler! {
fn plan_command(args_json, out, _user_data) {
    let raw = args_json.to_owned();
    let envelope: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let args = envelope.get("args").and_then(Value::as_str).unwrap_or("");
    command_output(out, run_command(&AbiHost, args))
}
}

/// The `/plan` command body, shared by both channels.
pub(crate) fn run_command(host: &dyn PlanHost, args: &str) -> Value {
    let args = args.trim();
    let first = args.split_whitespace().next().unwrap_or("").to_lowercase();
    let result = match first.as_str() {
        "start" => match activate(host) {
            Ok(true) => {
                "Plan mode enabled. Inspect the codebase and call `plan_mode_complete` when ready."
                    .to_string()
            }
            Ok(false) => "Plan mode is already active.".to_string(),
            Err(error) => format!("Unable to enter plan mode: {error}"),
        },
        "show" | "status" | "" => plan_status(),
        "finalize" => {
            if state()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .plan
                .is_some()
            {
                "Plan is stored in the current session. Use `/plan show` to review it.".to_string()
            } else {
                "No plan yet. Complete plan mode with `plan_mode_complete` first.".to_string()
            }
        }
        "exit" | "off" => match deactivate(host) {
            Ok(true) => "Plan mode disabled. Full tool access restored.".to_string(),
            Ok(false) => "Plan mode is not active.".to_string(),
            Err(error) => format!("Unable to leave plan mode: {error}"),
        },
        _ => match activate(host) {
            Ok(_) => {
                let prompt = format!(
                    "You are now in Plan mode. Do not edit files or implement changes.\n\nUser request: {args}\n\nInspect the repository, resolve important decisions, and call `plan_mode_complete` with a complete implementation-ready Markdown plan when finished."
                );
                if let Err(error) = host.send_user_message(&prompt) {
                    let _ = deactivate(host);
                    format!("Unable to start planning request: {error}")
                } else {
                    "Plan mode started and the planning request was submitted.".to_string()
                }
            }
            Err(error) => format!("Unable to enter plan mode: {error}"),
        },
    };
    let mut output = json!({"kind": "message", "text": result});
    if !result.starts_with("Unable to") {
        let guard = state().lock().unwrap_or_else(|p| p.into_inner());
        output["details"] = json!({"kind":"plan","plan":guard.plan,"active":guard.active,"expanded":first == "show"});
    }
    output
}
rpi_plugin_sdk::export_plugin!(|api| {
    if let Some(register_event) = api.register_event_handler {
        for tag in [EventTag::SessionStart, EventTag::SessionTree] {
            if register_event(tag, handle_event as EventHandlerFn, std::ptr::null_mut()) != 0 {
                return 1;
            }
        }
    }
    let Some(register) = api.register_tool else {
        return 1;
    };
    let _ = RUNTIME.set(*api);
    let schema = Box::new(StableToolSchema {
        name: StbString::from_string(PLAN_TOOL.into()),
        description: StbString::from_string(
            "Submit a complete implementation-ready plan and leave plan mode. Only available while /plan mode is active.".into(),
        ),
        parameters: StbString::from_string(format!(
            r#"{{"type":"object","required":["plan"],"properties":{{"plan":{{"type":"string","minLength":1,"maxLength":{MAX_PLAN_CHARS},"description":"The complete decision-ready implementation plan in Markdown."}}}}}}"#
        )),
    });
    let rc = register(&*schema, execute, poll, cancel, destroy, free_string);
    drop(schema);
    if rc != 0 {
        return rc;
    }
    let start_schema = Box::new(StableToolSchema {
        name: StbString::from_string(START_TOOL.into()),
        description: StbString::from_string(
            "Enter Plan mode before making a complex change. Use this when the request needs repository exploration and an implementation plan.".into(),
        ),
        parameters: StbString::from_string(
            r#"{"type":"object","properties":{"request":{"type":"string","description":"Optional user request to plan."}}}"#.into(),
        ),
    });
    let start_rc = register(&*start_schema, execute, poll, cancel, destroy, free_string);
    drop(start_schema);
    if start_rc != 0 {
        return start_rc;
    }
    if let Some(register_command) = api.register_command {
        let name = StbStringRef::from_str("plan");
        let description = StbStringRef::from_str("Enter, inspect, or exit CodeX-like plan mode");
        let _ = register_command(name, description, plan_command);
    }
    0
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_empty_status() {
        assert!(markdown_plan(None, false).contains("No completed plan"));
        assert!(markdown_plan(None, false).contains("Idle"));
        assert!(!markdown_plan(None, false).contains("Ready"));
    }

    #[test]
    fn rejects_empty_plan() {
        assert!(complete_plan(&AbiHost, &json!({"plan": ""})).is_err());
    }

    #[test]
    fn extracts_numbered_outline_without_markdown() {
        let outline = plan_outline("## Plan\n\n1. Add the parser\n2. Update the UI\n\nDetails");
        assert_eq!(
            outline,
            "Progress: 0/2 complete
- ○ 1. Add the parser
- ○ 2. Update the UI"
        );
    }

    #[test]
    fn marks_done_steps_from_tags_and_checkboxes() {
        let items = parse_plan_items(
            "1. First
2. Second
[DONE:1]
- [x] 2. Second",
        );
        assert!(items[0].completed);
        assert!(items[1].completed);
        assert!(plan_outline(
            "1. First
2. Second
[DONE:1]"
        )
        .contains("Progress: 1/2 complete"));
    }

    #[test]
    fn limits_long_outline() {
        let plan = (1..=13)
            .map(|index| format!("{index}. Step {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outline = plan_outline(&plan);
        assert!(outline.contains("... (use `/plan show` to view the remaining steps)"));
        assert_eq!(outline.lines().count(), 14);
    }

    #[test]
    fn complete_display_preserves_sections_and_non_numbered_plans() {
        let plan =
            "## Approach\n\n- Inspect source\n- Update UI\n\n## Tests\n\nRun the regression suite";
        let rendered = markdown_plan(Some(plan), false);
        assert!(rendered.contains(plan));
        assert_eq!(rendered.matches("Ready").count(), 1);
        assert!(!rendered.contains("Next:"));
    }
}
