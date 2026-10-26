#![forbid(unsafe_code)]
//! `kiqi-plan-mode` ported to the wasm ABI (0.7.0).
//!
//! Plan mode is a *session* plugin: it swaps the active tool set, persists its
//! state as a custom entry, restores it from the session branch, and can submit a
//! planning request. The behaviour lives in [`plan`] (pure logic + native unit
//! tests — `cargo test -p kiqi-plan-mode-guest` from `plugins/wasm`, which runs
//! on the host); this module is only the [`plan::PlanHost`] adapter over the WIT
//! imports.
//!
//! The port found a real ABI gap: the WIT had `inject.append-entry`, which is
//! `append_message`, but nothing for `append_custom_entry` — which is exactly
//! what plan mode persists through. `append-custom-entry` was added for it.

use crate::plan::{complete_plan, on_session_event, run_command, start_plan, PlanHost};
use kiqi_plugin_guest::bindings::exports::kiqi::plugin::plugin_api::Guest;
use kiqi_plugin_guest::bindings::kiqi::plugin::{inject, session_control, session_read, types};
use serde_json::Value;

mod plan;

const ID: &str = "kiqi-plan-mode";
const COMPLETE_TOOL: &str = plan::PLAN_TOOL;
const START_TOOL: &str = plan::START_TOOL;
const COMMAND: &str = "plan";

/// The session operations, over the wasm imports instead of `HostCaps`.
struct WasmHost;

impl PlanHost for WasmHost {
    fn active_tools(&self) -> Result<Vec<String>, String> {
        Ok(session_read::active_tools())
    }

    fn set_active_tools(&self, tools: &[String]) -> Result<(), String> {
        session_control::set_active_tools(tools).map_err(reason)
    }

    fn session_branch(&self) -> Result<Value, String> {
        let branch = session_read::session_branch();
        serde_json::from_str(&branch).map_err(|error| format!("bad session branch: {error}"))
    }

    fn append_custom_entry(&self, custom_type: &str, data: &Value) -> Result<(), String> {
        inject::append_custom_entry(custom_type, &data.to_string()).map_err(reason)
    }

    fn send_user_message(&self, text: &str) -> Result<(), String> {
        inject::send_user_message(text).map_err(reason)
    }
}

fn reason(error: types::CapabilityError) -> String {
    match error {
        types::CapabilityError::Denied(reason) => format!("denied: {reason}"),
        types::CapabilityError::Failed(reason) => reason,
    }
}

struct PlanMode;

impl Guest for PlanMode {
    /// No selector/editor/prompt flow exists in this plugin yet; a reply is
    /// refused rather than silently dropped.
    fn continue_command(
        _name: String,
        _flow_id: String,
        _reply: types::CommandReply,
    ) -> Result<types::CommandResult, types::PluginError> {
        Err(types::PluginError::Failed(
            "this plugin declares no command continuations".to_string(),
        ))
    }

    fn id() -> String {
        ID.to_string()
    }

    fn shutdown() {}

    fn list_tools() -> Vec<types::ToolSpec> {
        vec![
            types::ToolSpec {
                name: COMPLETE_TOOL.to_string(),
                description: "Submit a complete implementation-ready plan and leave plan mode. Only available while /plan mode is active.".to_string(),
                parameters_schema: format!(
                    r#"{{"type":"object","required":["plan"],"properties":{{"plan":{{"type":"string","minLength":1,"maxLength":{},"description":"The complete decision-ready implementation plan in Markdown."}}}}}}"#,
                    plan::MAX_PLAN_CHARS
                ),
            },
            types::ToolSpec {
                name: START_TOOL.to_string(),
                description: "Enter Plan mode before making a complex change. Use this when the request needs repository exploration and an implementation plan.".to_string(),
                parameters_schema:
                    r#"{"type":"object","properties":{"request":{"type":"string","description":"Optional user request to plan."}}}"#
                        .to_string(),
            },
        ]
    }

    fn call_tool(name: String, arguments: String) -> Result<types::ToolResult, types::PluginError> {
        let params: Value = serde_json::from_str(&arguments)
            .map_err(|error| types::PluginError::InvalidArguments(error.to_string()))?;
        let outcome = match name.as_str() {
            COMPLETE_TOOL => complete_plan(&WasmHost, &params),
            START_TOOL => start_plan(&WasmHost, &params),
            other => return Err(types::PluginError::UnknownTool(other.to_string())),
        };
        let value = outcome.map_err(types::PluginError::InvalidArguments)?;
        let parsed: Value = serde_json::from_str(&value).unwrap_or(Value::String(value));
        // Same flattening as the compile-time face: the text block is what the
        // model reads, `details` rides along.
        let text = parsed
            .get("content")
            .and_then(|content| content.get(0))
            .and_then(|block| block.get("text"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| parsed.to_string());
        let details = parsed.get("details").cloned().unwrap_or(Value::Null);
        Ok(types::ToolResult {
            blocks: vec![types::ContentBlock::Text(text)],
            is_error: false,
            details: details.to_string(),
        })
    }

    fn list_commands() -> Vec<types::CommandSpec> {
        vec![types::CommandSpec {
            name: COMMAND.to_string(),
            description: "Enter, inspect, or exit CodeX-like plan mode".to_string(),
        }]
    }

    fn run_command(
        name: String,
        arguments: String,
    ) -> Result<types::CommandResult, types::PluginError> {
        if name != COMMAND {
            return Err(types::PluginError::UnknownTool(name));
        }
        let output = run_command(&WasmHost, &arguments);
        let text = output
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| output.to_string());
        Ok(types::CommandResult {
            flow_id: String::new(),
            body: types::CommandResultBody::Message(text),
        })
    }

    fn list_flags() -> Vec<types::FlagSpec> {
        Vec::new()
    }

    fn list_shortcuts() -> Vec<types::ShortcutSpec> {
        Vec::new()
    }

    /// Session start / tree change re-reads the plan from the branch.
    fn on_event(event: types::Event) -> types::EventAction {
        if matches!(
            event,
            types::Event::SessionStart | types::Event::SessionTree
        ) {
            on_session_event(&WasmHost);
        }
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

kiqi_plugin_guest::bindings::export!(PlanMode with_types_in kiqi_plugin_guest::bindings);

// The 0.8.0 contribution exports are mandatory world exports; deny them all.
kiqi_plugin_guest::deny_contribution_exports!(PlanMode);
