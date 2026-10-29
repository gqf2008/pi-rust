//! The `subagent` tool: start a child agent in the background.
//!
//! The orchestrator's wasm guest used to own this tool. It had to run the child
//! inside the call — a wasm extension cannot leave a process running past its
//! return — so several `subagent` calls in one batch ran strictly in sequence
//! and the session sat inside a single tool call until the slowest child
//! finished. This tool returns as soon as the child is up; the result arrives
//! later as a follow-up message (see `background_agents`).

use std::sync::Arc;

use async_trait::async_trait;
use kiqi_agent::agent_tool::AgentTool;
use kiqi_agent::error::AgentError;
use kiqi_agent::types::{AgentToolResult, ToolExecutionMode, ToolResultPartial};
use kiqi_ai::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::background_agents::{self, StartSpec, DEFAULT_TIMEOUT_SECS, MAX_TIMEOUT_SECS};

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SubagentInput {
    /// The task for the child agent. It starts fresh and cannot see this
    /// conversation, so the task must contain everything it needs.
    pub task: String,
    /// Kill the child after this many seconds (default 900, max 3600).
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    /// Provider override for the child (defaults to the child's own config).
    #[serde(default)]
    pub provider: Option<String>,
    /// Model override for the child.
    #[serde(default)]
    pub model: Option<String>,
}

pub struct SubagentTool {
    schema: Tool,
}

impl SubagentTool {
    fn new() -> Self {
        let params = schemars::schema_for!(SubagentInput);
        Self {
            schema: Tool {
                name: "subagent".to_string(),
                description: "Start a child agent on a bounded task in the background and \
return immediately. Independent calls run in parallel. The child starts fresh and \
cannot see this conversation, so the task must contain everything it needs. When the \
child finishes, its answer arrives as a follow-up user message — call this, then keep \
working or end the turn; do not poll. Results are not available until that message \
arrives."
                    .to_string(),
                parameters: kiqi_ai::types::Schema::new(
                    serde_json::to_value(params).unwrap_or_default(),
                ),
                constrained_sampling: None,
            },
        }
    }
}

pub(crate) fn create_subagent_tool() -> Arc<dyn AgentTool> {
    Arc::new(SubagentTool::new())
}

#[async_trait]
impl AgentTool for SubagentTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        "subagent"
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        // Starting a child is cheap and independent; nothing here serializes.
        ToolExecutionMode::Parallel
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let input: SubagentInput = serde_json::from_value(params)
            .map_err(|error| AgentError::Validation(format!("subagent input invalid: {error}")))?;
        if signal.is_cancelled() {
            return Err(AgentError::Tool("subagent call cancelled".into()));
        }
        let timeout_secs = input
            .timeout_seconds
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(10, MAX_TIMEOUT_SECS);
        let spec = StartSpec {
            task: input.task,
            timeout_secs,
            provider: input.provider,
            model: input.model,
            program: None,
        };
        let id = background_agents::global()
            .start(spec)
            .await
            .map_err(AgentError::Tool)?;
        Ok(AgentToolResult::text(format!(
            "Started background {id} (timeout {timeout_secs}s). It runs on its own; \
its result will arrive as a follow-up message. Continue with other work or end the \
turn — do not wait for it."
        )))
    }
}
