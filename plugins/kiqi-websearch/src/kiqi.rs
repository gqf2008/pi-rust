//! The compile-time (`kiqi-plugin`) face of kiqi-websearch.
//!
//! Thin by design: the work function already exists in the crate root, so this
//! only replaces the cdylib's `export_plugin!` + `StepHandle` plumbing with the
//! trait. Same semantics, no FFI.

use crate::search;
use kiqi_plugin::{Block, HostCaps, Plugin, PluginError, ProgressSink, ToolOutcome, ToolSpec};
use serde_json::Value;

const TOOL: &str = "websearch";
const DESCRIPTION: &str = "Search public web references with DuckDuckGo.";
const SCHEMA: &str = r#"{"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":20}},"required":["query"]}"#;

pub struct WebsearchPlugin;

impl WebsearchPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Plugin for WebsearchPlugin {
    fn id(&self) -> &str {
        "kiqi-websearch"
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
        let text = search(arguments).map_err(PluginError::InvalidArguments)?;
        Ok(ToolOutcome {
            blocks: vec![Block::Text(text)],
            is_error: false,
            details: Value::Null,
        })
    }
}

/// The factory the host registry calls.
pub fn factory() -> Box<dyn Plugin> {
    Box::new(WebsearchPlugin::new())
}
