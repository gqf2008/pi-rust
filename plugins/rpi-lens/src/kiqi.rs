//! The compile-time (`kiqi-plugin`) face of rpi-lens.
//!
//! Thin by design: the work function already exists in the crate root, so this
//! only replaces the cdylib's `export_plugin!` + `StepHandle` plumbing with the
//! trait. Same semantics, no FFI.

use crate::code_lens;
use kiqi_plugin::{Block, HostCaps, Plugin, PluginError, ProgressSink, ToolOutcome, ToolSpec};
use serde_json::Value;

const TOOL: &str = "code_lens";
const DESCRIPTION: &str = "Run an allowlisted, timeout-bounded local code diagnostic.";
const SCHEMA: &str = r#"{"type":"object","properties":{"check":{"type":"string","enum":["diff","cargo","fmt","clippy"]},"cwd":{"type":"string"},"timeoutSeconds":{"type":"integer","minimum":1,"maximum":120}}}"#;

pub struct CodeLensPlugin;

impl CodeLensPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Plugin for CodeLensPlugin {
    fn id(&self) -> &str {
        "rpi-lens"
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
        let text = code_lens(arguments).map_err(PluginError::InvalidArguments)?;
        Ok(ToolOutcome {
            blocks: vec![Block::Text(text)],
            is_error: false,
            details: Value::Null,
        })
    }
}

/// The factory the host registry calls.
pub fn factory() -> Box<dyn Plugin> {
    Box::new(CodeLensPlugin::new())
}
