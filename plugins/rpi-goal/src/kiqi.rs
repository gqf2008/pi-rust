//! The compile-time (`kiqi-plugin`) face of rpi-goal.
//!
//! The core (`goal`, `run_command`, `turn_end`) is shared with the cdylib face.
//! Only the single host operation it needs is replaced: the C-ABI global
//! `call_runtime(AppendEntry)` becomes `HostCaps::inject`, threaded in through
//! the narrow `GoalHost` trait so neither channel regresses.

use crate::{goal, run_command, turn_end, CapsHost};
use kiqi_plugin::{
    Block, CommandSpec, EventAction, EventTag, HostCaps, Plugin, PluginError, ProgressSink,
    ToolOutcome, ToolSpec,
};
use serde_json::Value;

const DESCRIPTION: &str = "Codex-style goal mode: record one persistent, context-injected objective per project and let the agent advance it across turns. Actions: start, update, pause, resume, complete, limit, show, list.";
const SCHEMA: &str = r#"{"type":"object","properties":{"action":{"type":"string","enum":["start","update","pause","resume","complete","limit","show","list"]},"title":{"type":"string","description":"Goal title (required for start)"},"notes":{"type":"string","description":"Goal notes or progress update (start/update)"},"status":{"type":"string","enum":["active","paused"],"description":"Explicit status change (update)"},"maxTurns":{"type":"integer","minimum":1,"description":"Optional turn budget (start/limit)"},"maxMinutes":{"type":"integer","minimum":1,"description":"Optional wall-clock budget in minutes (start/limit)"}},"required":["action"]}"#;

pub struct GoalPlugin;

impl GoalPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Plugin for GoalPlugin {
    fn id(&self) -> &str {
        "rpi-goal"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "goal".to_string(),
            description: DESCRIPTION.to_string(),
            parameters: serde_json::from_str(SCHEMA).expect("goal schema is valid JSON"),
        }]
    }

    fn commands(&self) -> Vec<CommandSpec> {
        vec![CommandSpec {
            name: "goal".to_string(),
            description: "Show, start, pause, stop, or budget the active goal".to_string(),
        }]
    }

    fn call(
        &self,
        name: &str,
        arguments: &Value,
        caps: &HostCaps<'_>,
        _sink: &dyn ProgressSink,
    ) -> Result<ToolOutcome, PluginError> {
        if name != "goal" {
            return Err(PluginError::UnknownTool(name.to_string()));
        }
        let text = goal(&CapsHost(caps), arguments).map_err(PluginError::InvalidArguments)?;
        Ok(ToolOutcome {
            blocks: vec![Block::Text(text)],
            is_error: false,
            details: Value::Null,
        })
    }

    fn run_command(
        &self,
        name: &str,
        arguments: &str,
        caps: &HostCaps<'_>,
    ) -> Result<String, PluginError> {
        if name != "goal" {
            return Err(PluginError::UnknownTool(name.to_string()));
        }
        Ok(run_command(&CapsHost(caps), arguments))
    }

    /// TurnEnd is where the goal notice is injected - and it is the whole
    /// reason this plugin needs a capability: the injection goes through the
    /// host, so a plugin without `InjectApi` simply cannot advance a goal.
    fn on_event(&self, event: &kiqi_plugin::Event, caps: &HostCaps<'_>) -> EventAction {
        if event.tag == EventTag::TurnEnd {
            turn_end(&CapsHost(caps));
        }
        EventAction::Continue
    }
}

/// The factory the host registry calls.
pub fn factory() -> Box<dyn Plugin> {
    Box::new(GoalPlugin::new())
}
