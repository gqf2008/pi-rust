//! The compile-time (`kiqi-plugin`) face of kiqi-codegraph.
//!
//! A thin wrapper on purpose: all eight tools funnel through one function
//! (`call_codegraph`) and none of them reaches the host, so there is no
//! capability to thread. The external `codegraph` process it drives is a plain
//! subprocess - which is exactly what the in-process model allows and a wasm
//! sandbox would not.

use crate::{call_codegraph, tool_specs};
use kiqi_plugin::{Block, HostCaps, Plugin, PluginError, ProgressSink, ToolOutcome, ToolSpec};
use serde_json::Value;
use std::sync::atomic::AtomicBool;

pub struct CodegraphPlugin;

impl CodegraphPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Plugin for CodegraphPlugin {
    fn id(&self) -> &str {
        "kiqi-codegraph"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        tool_specs()
            .into_iter()
            .map(|spec| ToolSpec {
                name: spec.name.to_string(),
                description: spec.description.to_string(),
                parameters: serde_json::from_str(&spec.parameters).unwrap_or(Value::Null),
            })
            .collect()
    }

    fn call(
        &self,
        name: &str,
        arguments: &Value,
        _caps: &HostCaps<'_>,
        _sink: &dyn ProgressSink,
    ) -> Result<ToolOutcome, PluginError> {
        if !tool_specs().iter().any(|spec| spec.name == name) {
            return Err(PluginError::UnknownTool(name.to_string()));
        }
        let text = call_codegraph(name, arguments, &AtomicBool::new(false))
            .map_err(PluginError::InvalidArguments)?;
        Ok(ToolOutcome {
            blocks: vec![Block::Text(text)],
            is_error: false,
            details: Value::Null,
        })
    }
}

/// The factory the host registry calls.
pub fn factory() -> Box<dyn Plugin> {
    Box::new(CodegraphPlugin::new())
}
