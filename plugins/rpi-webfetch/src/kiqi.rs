//! The compile-time (`kiqi-plugin`) face of rpi-webfetch.
//!
//! Thin by design: the work function already exists in the crate root, so this
//! only replaces the cdylib's `export_plugin!` + `StepHandle` plumbing with the
//! trait. Same semantics, no FFI.

use crate::fetch;
use kiqi_plugin::{Block, HostCaps, Plugin, PluginError, ProgressSink, ToolOutcome, ToolSpec};
use serde_json::Value;

const TOOL: &str = "webfetch";
const DESCRIPTION: &str = "Fetch bounded readable text from a public HTTP(S) URL.";
const SCHEMA: &str = r#"{"type":"object","properties":{"url":{"type":"string"},"maxChars":{"type":"integer","minimum":256,"maximum":50000}},"required":["url"]}"#;

pub struct WebfetchPlugin;

impl WebfetchPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Plugin for WebfetchPlugin {
    fn id(&self) -> &str {
        "rpi-webfetch"
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
        let text = fetch(arguments).map_err(PluginError::InvalidArguments)?;
        Ok(ToolOutcome {
            blocks: vec![Block::Text(text)],
            is_error: false,
            details: Value::Null,
        })
    }
}

/// The factory the host registry calls.
pub fn factory() -> Box<dyn Plugin> {
    Box::new(WebfetchPlugin::new())
}
