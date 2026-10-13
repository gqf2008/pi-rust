//! The compile-time (`kiqi-plugin`) face of rpi-ask-user.
//!
//! One host operation (`UiDialog`) threaded through the narrow `AskHost` trait,
//! so neither channel regresses. The native face has to hand the host a
//! `StepHandle` and be re-polled because a C ABI cannot block; here `call` is
//! synchronous, so the same drive collapses into one function that waits. That
//! removes the step machinery without removing any behaviour.

use crate::{
    ask, build_done, poll_open_question, start_drive, AskHost, CapsHost, Drive, POLL_INTERVAL,
};
use kiqi_plugin::{
    Block, CommandSpec, HostCaps, Plugin, PluginError, ProgressSink, ToolOutcome, ToolSpec,
};
use serde_json::Value;

const TOOL: &str = "ask_user";
const DESCRIPTION: &str =
    "Ask the user an interactive question with selectable options or freeform input.";
const COMMAND_DESCRIPTION: &str = "Ask a question with the interactive Pi selector";
const SCHEMA: &str = r#"{"type":"object","properties":{"question":{"type":"string"},"context":{"type":"string"},"options":{"type":"array","items":{"oneOf":[{"type":"string"},{"type":"object"}]}},"allowMultiple":{"type":"boolean"},"allowFreeform":{"type":"boolean"},"allowComment":{"type":"boolean"},"displayMode":{"type":"string","enum":["overlay","inline"]},"timeout":{"type":"integer","minimum":1},"type":{"type":"string","enum":["question","confirm"]},"questions":{"type":"array","minItems":1,"maxItems":3},"suggest":{"type":"string"},"summary":{"type":"string"}},"additionalProperties":false}"#;

pub struct AskUserPlugin;

impl AskUserPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Plugin for AskUserPlugin {
    fn id(&self) -> &str {
        "rpi-ask-user"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: TOOL.to_string(),
            description: DESCRIPTION.to_string(),
            parameters: serde_json::from_str(SCHEMA).expect("ask_user schema is valid JSON"),
        }]
    }

    fn commands(&self) -> Vec<CommandSpec> {
        vec![CommandSpec {
            name: TOOL.to_string(),
            description: COMMAND_DESCRIPTION.to_string(),
        }]
    }

    fn call(
        &self,
        name: &str,
        arguments: &Value,
        caps: &HostCaps<'_>,
        _sink: &dyn ProgressSink,
    ) -> Result<ToolOutcome, PluginError> {
        if name != TOOL {
            return Err(PluginError::UnknownTool(name.to_string()));
        }
        // Reject the plan before asking anything, so a malformed call never
        // opens a dialog.
        let _ = ask(arguments).map_err(PluginError::InvalidArguments)?;

        let host = CapsHost(caps);
        let mut drive = Drive::new(arguments.clone());
        let started = start_drive(&mut drive, &host).map_err(PluginError::InvalidArguments)?;
        if !started.is_null() {
            return Ok(outcome(started));
        }
        loop {
            match poll_open_question(&mut drive, &host) {
                Ok(Some(done)) => return Ok(outcome(done)),
                Ok(None) => std::thread::sleep(POLL_INTERVAL),
                Err(error) => return Err(PluginError::InvalidArguments(error)),
            }
        }
    }

    fn run_command(
        &self,
        name: &str,
        arguments: &str,
        _caps: &HostCaps<'_>,
    ) -> Result<String, PluginError> {
        if name != TOOL {
            return Err(PluginError::UnknownTool(name.to_string()));
        }
        Ok(crate::run_command(arguments).to_string())
    }
}

/// The tool answers with the host-shaped result object; the text block is what
/// the model reads and `details` rides along.
fn outcome(value: Value) -> ToolOutcome {
    let text = value
        .get("content")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("text"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string());
    let details = value.get("details").cloned().unwrap_or(Value::Null);
    ToolOutcome {
        blocks: vec![Block::Text(text)],
        is_error: false,
        details,
    }
}

/// The factory the host registry calls.
pub fn factory() -> Box<dyn Plugin> {
    Box::new(AskUserPlugin::new())
}
