//! The compile-time (`kiqi-plugin`) face of kiqi-permissions.
//!
//! Thin by design: the work function already exists in the crate root, so this
//! only replaces the cdylib's `export_plugin!` + `StepHandle` plumbing with the
//! trait. Same semantics, no FFI.

use crate::permissions;
use kiqi_plugin::{Block, HostCaps, Plugin, PluginError, ProgressSink, ToolOutcome, ToolSpec};
use serde_json::Value;

const TOOL: &str = "permissions";
const DESCRIPTION: &str = "Check and manage native Pi allow/deny rules for tool calls.";
const SCHEMA: &str = r#"{"type":"object","properties":{"action":{"type":"string","enum":["check","grant","revoke","list"]},"tool":{"type":"string","description":"Pi tool name, such as Bash, Read, Write, or Edit"},"value":{"type":"string","description":"Command or path to evaluate"},"rule":{"type":"string","description":"Rule such as Bash(git push *)"},"capability":{"type":"string","description":"Legacy alias for value/rule"},"effect":{"type":"string","enum":["allow","deny"]},"cwd":{"type":"string"}},"required":["action"]}"#;

pub struct PermissionsPlugin;

impl PermissionsPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Plugin for PermissionsPlugin {
    fn id(&self) -> &str {
        "kiqi-permissions"
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
        let text = permissions(arguments).map_err(PluginError::InvalidArguments)?;
        Ok(ToolOutcome {
            blocks: vec![Block::Text(text)],
            is_error: false,
            details: Value::Null,
        })
    }
}

/// The factory the host registry calls.
pub fn factory() -> Box<dyn Plugin> {
    Box::new(PermissionsPlugin::new())
}
