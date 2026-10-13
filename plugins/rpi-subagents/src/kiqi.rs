//! The compile-time (`kiqi-plugin`) face of rpi-subagents.
//!
//! Thin by design: the work function already exists in the crate root, so this
//! only replaces the cdylib's `export_plugin!` + `StepHandle` plumbing with the
//! trait. Same semantics, no FFI.

use crate::delegate_task;
use kiqi_plugin::{Block, HostCaps, Plugin, PluginError, ProgressSink, ToolOutcome, ToolSpec};
use serde_json::Value;

const TOOL: &str = "delegate_task";
const DESCRIPTION: &str = "Create a bounded, non-recursive subagent delegation envelope.";
const SCHEMA: &str = r#"{"type":"object","properties":{"task":{"type":"string","maxLength":20000},"role":{"type":"string"},"maxTurns":{"type":"integer","minimum":1,"maximum":32},"timeoutSeconds":{"type":"integer","minimum":10,"maximum":1800}},"required":["task"]}"#;

pub struct DelegateTaskPlugin;

impl DelegateTaskPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Plugin for DelegateTaskPlugin {
    fn id(&self) -> &str {
        "rpi-subagents"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: TOOL.to_string(),
            description: DESCRIPTION.to_string(),
            parameters: serde_json::from_str(SCHEMA).expect("schema is valid JSON"),
        }]
    }

    fn call(
        &self,
        name: &str,
        arguments: &Value,
        _caps: &HostCaps<'_>,
        _sink: &dyn ProgressSink,
    ) -> Result<ToolOutcome, PluginError> {
        if name != TOOL {
            return Err(PluginError::UnknownTool(name.to_string()));
        }
        let text = delegate_task(arguments).map_err(PluginError::InvalidArguments)?;
        Ok(ToolOutcome {
            blocks: vec![Block::Text(text)],
            is_error: false,
            details: Value::Null,
        })
    }
}

/// The factory the host registry calls.
pub fn factory() -> Box<dyn Plugin> {
    Box::new(DelegateTaskPlugin::new())
}
