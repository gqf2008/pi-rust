#![forbid(unsafe_code)]
//! CodeX-like plan mode, exposed through three channels that share one
//! implementation ([`rpi_plan_mode_core`]):
//!
//! * the cdylib ABI (`AbiHost`, via `runtime_action`);
//! * the compile-time `Plugin` trait (`kiqi.rs`, via `HostCaps`);
//! * the wasm guest (`kiqi-plan-mode-guest`, via the WIT session imports),
//!   which lives in the guest workspace and depends on the same core crate.
//!
//! What is left here is only the two host adapters and the C-ABI plumbing.

pub mod kiqi;

use rpi_plan_mode_core::{complete_plan, on_session_event, run_command, start_plan, PlanHost};
use rpi_plugin_sdk::handles::ToolHandles;
use rpi_plugin_sdk::{
    EventHandlerFn, EventTag, FreeStringFn, PluginApi, RuntimeActionId, StablePluginEvent,
    StableToolSchema, StbString, StbStringRef, StepHandle, StepResult, ToolPartialCb,
};
use serde_json::{json, Value};
use std::ffi::c_void;
use std::sync::OnceLock;

const MAX_PLAN_CHARS: usize = rpi_plan_mode_core::MAX_PLAN_CHARS;
const PLAN_TOOL: &str = rpi_plan_mode_core::PLAN_TOOL;
const START_TOOL: &str = rpi_plan_mode_core::START_TOOL;

// Reuse the SDK's host-context lifetime and thread-safety contract.
type Runtime = PluginApi;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

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

extern "C" fn handle_event(event: StablePluginEvent, _: *mut c_void) -> i32 {
    if matches!(event.tag, EventTag::SessionStart | EventTag::SessionTree) {
        on_session_event(&AbiHost);
    }
    0
}

static HANDLES: ToolHandles<Drive> = ToolHandles::new();

struct Drive {
    tool: &'static str,
    params: Value,
    done: bool,
}

fn drive_from(tool: &'static str, params: StbString, free: Option<FreeStringFn>) -> StepHandle {
    let text = params.to_string_lossy();
    params.free_with(free);
    HANDLES.insert(Drive {
        tool,
        params: serde_json::from_str(&text).unwrap_or(Value::Null),
        done: false,
    })
}

// One `execute` per tool: the ABI hands `execute` only the params, so the tool
// identity is stamped here. Dispatching on the payload's shape instead (as
// this did) misroutes a `plan_mode_start` call that happens to carry a `plan`
// field — the start schema does not forbid extra properties.
extern "C" fn execute_start(
    _: StbStringRef,
    params: StbString,
    free: Option<FreeStringFn>,
) -> StepHandle {
    drive_from(START_TOOL, params, free)
}

extern "C" fn execute_complete(
    _: StbStringRef,
    params: StbString,
    free: Option<FreeStringFn>,
) -> StepHandle {
    drive_from(PLAN_TOOL, params, free)
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
    let result = if drive.tool == PLAN_TOOL {
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
    let rc = register(
        &*schema,
        execute_complete,
        poll,
        cancel,
        destroy,
        free_string,
    );
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
    let start_rc = register(
        &*start_schema,
        execute_start,
        poll,
        cancel,
        destroy,
        free_string,
    );
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
