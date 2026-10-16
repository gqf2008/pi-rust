//! The compile-time (`kiqi-plugin`) face of kiqi-plan-mode.
//!
//! Plan mode lives or dies by one capability: it swaps the active tool set, so
//! without `SessionControl` it cannot plan. That is threaded through the narrow
//! `PlanHost` trait; the core (activate/deactivate/start_plan/complete_plan/
//! run_command) is shared with the cdylib face.

use crate::CapsHost;
use kiqi_plan_mode_core::{complete_plan, run_command, start_plan, PlanHost};
use kiqi_plugin::{
    Block, CommandSpec, EventAction, EventTag, HostCaps, Plugin, PluginError, ProgressSink,
    ToolOutcome, ToolSpec,
};
use serde_json::Value;

const COMPLETE_TOOL: &str = "plan_mode_complete";
const START_TOOL: &str = "plan_mode_start";
const MAX_PLAN_CHARS: usize = 50_000;

pub struct PlanModePlugin;

impl PlanModePlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Plugin for PlanModePlugin {
    fn id(&self) -> &str {
        "kiqi-plan-mode"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: COMPLETE_TOOL.to_string(),
                description: "Submit a complete implementation-ready plan and leave plan mode. Only available while /plan mode is active.".to_string(),
                parameters: serde_json::from_str(&format!(
                    r#"{{"type":"object","required":["plan"],"properties":{{"plan":{{"type":"string","minLength":1,"maxLength":{MAX_PLAN_CHARS},"description":"The complete decision-ready implementation plan in Markdown."}}}}}}"#
                ))
                .expect("plan schema is valid JSON"),
            },
            ToolSpec {
                name: START_TOOL.to_string(),
                description: "Enter Plan mode before making a complex change. Use this when the request needs repository exploration and an implementation plan.".to_string(),
                parameters: serde_json::from_str(
                    r#"{"type":"object","properties":{"request":{"type":"string","description":"Optional user request to plan."}}}"#,
                )
                .expect("start schema is valid JSON"),
            },
        ]
    }

    fn commands(&self) -> Vec<CommandSpec> {
        vec![CommandSpec {
            name: "plan".to_string(),
            description: "Enter, inspect, or exit CodeX-like plan mode".to_string(),
        }]
    }

    fn call(
        &self,
        name: &str,
        arguments: &Value,
        caps: &HostCaps<'_>,
        _sink: &dyn ProgressSink,
    ) -> Result<ToolOutcome, PluginError> {
        let host = CapsHost(caps);
        let outcome = match name {
            COMPLETE_TOOL => complete_plan(&host, arguments),
            START_TOOL => start_plan(&host, arguments),
            other => return Err(PluginError::UnknownTool(other.to_string())),
        };
        let value = outcome.map_err(PluginError::InvalidArguments)?;
        // These tools answer with a full result object (content + details); the
        // text block is what the model reads, the details ride along.
        let parsed: Value = serde_json::from_str(&value).unwrap_or(Value::String(value));
        let text = parsed
            .get("content")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| parsed.to_string());
        let details = parsed.get("details").cloned().unwrap_or(Value::Null);
        Ok(ToolOutcome {
            blocks: vec![Block::Text(text)],
            is_error: false,
            details,
        })
    }

    fn run_command(
        &self,
        name: &str,
        arguments: &str,
        caps: &HostCaps<'_>,
    ) -> Result<String, PluginError> {
        if name != "plan" {
            return Err(PluginError::UnknownTool(name.to_string()));
        }
        let host: CapsHost<'_> = CapsHost(caps);
        let output = run_command(&host, arguments);
        Ok(output
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| output.to_string()))
    }

    /// Session start / tree change re-reads the plan from the branch, which
    /// needs `SessionRead`.
    fn on_event(&self, event: &kiqi_plugin::Event, caps: &HostCaps<'_>) -> EventAction {
        if matches!(event.tag, EventTag::SessionStart | EventTag::SessionTree) {
            crate::on_session_event(&CapsHost(caps));
        }
        EventAction::Continue
    }
}

/// The factory the host registry calls.
pub fn factory() -> Box<dyn Plugin> {
    Box::new(PlanModePlugin::new())
}
