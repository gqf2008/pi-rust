//! Built-in `watch` tool: let the model manage `.ki/watch.json` itself.
//!
//! `cron` fires on the clock; `watch` fires when a command's output *changes*.
//! Typical use: "tell me when CI turns red" or "wake me when this file appears".
//! The first probe of a session only records a baseline, so opening a session
//! does not flood the transcript with "changed" messages for state that was
//! already there.
//!
//! Scope is the session working directory: `<cwd>/.ki/watch.json`, the same file
//! [`crate::watch`] reads. Jobs tick only while an interactive session is open.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use kiqi_agent::agent_tool::AgentTool;
use kiqi_agent::error::AgentError;
use kiqi_agent::types::{AgentToolResult, ToolExecutionMode, ToolResultPartial};
use kiqi_ai::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::schedule_file::{self, ScheduleFile};

const KEY: &str = "watches";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WatchAction {
    /// Show the watches currently in `.ki/watch.json`.
    List,
    /// Add one watch.
    Add,
    /// Remove one watch by name.
    Remove,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WatchModeInput {
    /// Probe the command every interval (default).
    Probe,
    /// Start the command once and stream its output; report when it exits.
    Resident,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WatchToolInput {
    pub action: WatchAction,
    /// Watch name; required for `add` and `remove`.
    pub name: Option<String>,
    /// Shell command probed in the session working directory. A wake-up fires
    /// when its merged stdout/stderr (or exit code) differs from the previous
    /// probe.
    pub command: Option<String>,
    /// Message injected together with the new output when the probe changes.
    pub prompt: Option<String>,
    /// `probe` (default): run the command every interval and compare output.
    /// `resident`: start it once and stream; output is batched per interval and
    /// the exit code is reported when the process ends.
    pub mode: Option<WatchModeInput>,
    /// For `probe`, seconds between probes; for `resident`, the batching window.
    /// Range 5..=3600, default 60.
    #[serde(alias = "interval_secs")]
    pub interval_secs: Option<u64>,
    /// Per-probe timeout in seconds (1..=600, default 60).
    #[serde(alias = "timeout_secs")]
    pub timeout_secs: Option<u64>,
    /// Whether the watch runs; defaults to true.
    pub enabled: Option<bool>,
}

pub struct WatchTool {
    schema: Tool,
    cwd: PathBuf,
    trust: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl WatchTool {
    fn new(cwd: PathBuf, trust: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        let params = schemars::schema_for!(WatchToolInput);
        Self {
            schema: Tool {
                name: "watch".to_string(),
                description: "Manage this session's change-driven probes in `.ki/watch.json`. \
                    `list` shows them; `add` registers one (`name` + `command` + `prompt`, \
                    optional `intervalSecs`/`timeoutSecs`); `remove` deletes it by name. \
                    By default (`mode: probe`) the command is probed every interval; when its output \
                    (or exit code) differs from the previous probe, the prompt and the new \
                    output are injected. With `mode: resident` the command is started once and \
                    its output is batched per interval; the exit code is reported when it ends. \
                    Probes and residents tick only while an interactive session is open here."
                    .to_string(),
                parameters: kiqi_ai::types::Schema::new(
                    serde_json::to_value(params).unwrap_or_default(),
                ),
                constrained_sampling: None,
            },
            cwd,
            trust,
        }
    }

    fn path(&self) -> PathBuf {
        crate::watch::config_path(&self.cwd)
    }

    fn ensure_trusted(&self) -> Result<(), AgentError> {
        if (self.trust)() {
            Ok(())
        } else {
            Err(AgentError::Tool(
                "this project is not trusted; `.ki/watch.json` is project state and would not be probed"
                    .into(),
            ))
        }
    }
}

pub fn create_watch_tool(
    cwd: impl Into<PathBuf>,
    trust: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Arc<dyn AgentTool> {
    Arc::new(WatchTool::new(cwd.into(), trust))
}

#[async_trait]
impl AgentTool for WatchTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        "watch"
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        // Read-modify-write of one config file: two parallel calls would race
        // each other's write.
        ToolExecutionMode::Sequential
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let input: WatchToolInput = serde_json::from_value(params)
            .map_err(|error| AgentError::Validation(format!("watch input invalid: {error}")))?;
        if signal.is_cancelled() {
            return Err(AgentError::Tool("watch cancelled".into()));
        }
        let path = self.path();
        match input.action {
            WatchAction::List => {
                let file = schedule_file::load(&path, KEY).map_err(AgentError::Tool)?;
                Ok(AgentToolResult::text(describe(&path, &file)))
            }
            WatchAction::Add => {
                self.ensure_trusted()?;
                let mut file = schedule_file::load(&path, KEY).map_err(AgentError::Tool)?;
                let entry = build_entry(&input)?;
                let existing = names(file.entries());
                if existing
                    .iter()
                    .any(|name| name.as_str() == entry_name(&entry))
                {
                    return Err(AgentError::Validation(format!(
                        "a watch named `{}` already exists; remove it first",
                        entry_name(&entry)
                    )));
                }
                let mut entries = file.entries().to_vec();
                entries.push(entry);
                file.set_entries(entries);
                schedule_file::save(&path, &file).map_err(AgentError::Tool)?;
                Ok(AgentToolResult::text(format!(
                    "added watch `{}` to {}\n{}",
                    input.name.as_deref().unwrap_or("").trim(),
                    path.display(),
                    describe(&path, &file)
                )))
            }
            WatchAction::Remove => {
                self.ensure_trusted()?;
                let wanted = input
                    .name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        AgentError::Validation("`name` is required for remove".into())
                    })?;
                let mut file = schedule_file::load(&path, KEY).map_err(AgentError::Tool)?;
                let mut entries = file.entries().to_vec();
                let before = entries.len();
                entries.retain(|entry| entry_name(entry) != wanted);
                if entries.len() == before {
                    let known = names(file.entries());
                    return Err(AgentError::Tool(if known.is_empty() {
                        format!("no watch named `{wanted}`; the file has no watches")
                    } else {
                        format!("no watch named `{wanted}`; known: {}", known.join(", "))
                    }));
                }
                file.set_entries(entries);
                schedule_file::save(&path, &file).map_err(AgentError::Tool)?;
                Ok(AgentToolResult::text(format!(
                    "removed watch `{wanted}` from {}\n{}",
                    path.display(),
                    describe(&path, &file)
                )))
            }
        }
    }
}

fn build_entry(input: &WatchToolInput) -> Result<Value, AgentError> {
    let name = input
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| AgentError::Validation("`name` is required for add".into()))?;
    let command = input
        .command
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AgentError::Validation("`command` is required for add".into()))?;
    let prompt = input
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AgentError::Validation("`prompt` is required for add".into()))?;

    let mut entry = serde_json::Map::new();
    entry.insert("name".into(), json!(name));
    entry.insert("command".into(), json!(command));
    entry.insert("prompt".into(), json!(prompt));
    if let Some(mode) = &input.mode {
        entry.insert(
            "mode".into(),
            json!(match mode {
                WatchModeInput::Probe => "probe",
                WatchModeInput::Resident => "resident",
            }),
        );
    }
    if let Some(interval) = input.interval_secs {
        entry.insert("intervalSecs".into(), json!(interval));
    }
    if let Some(timeout) = input.timeout_secs {
        entry.insert("timeoutSecs".into(), json!(timeout));
    }
    entry.insert("enabled".into(), json!(input.enabled.unwrap_or(true)));

    let single = Value::Array(vec![Value::Object(entry.clone())]);
    let load = crate::watch::parse_config(&single.to_string());
    if let Some(error) = load.errors.first() {
        return Err(AgentError::Validation(error.to_string()));
    }
    Ok(Value::Object(entry))
}

fn entry_name(entry: &Value) -> String {
    entry
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn names(entries: &[Value]) -> Vec<String> {
    entries.iter().map(entry_name).collect()
}

fn describe(path: &std::path::Path, file: &ScheduleFile) -> String {
    let raw = Value::Array(file.entries().to_vec()).to_string();
    let load = crate::watch::parse_config(&raw);
    let mut out = String::new();
    if file.entries().is_empty() {
        out.push_str(&format!("{}: no watches", path.display()));
    } else {
        out.push_str(&format!(
            "{}: {} watch(es)",
            path.display(),
            file.entries().len()
        ));
        for watch in &load.watches {
            out.push_str(&format!(
                "\n- {} [{}] every {}s: `{}`",
                watch.name,
                if watch.enabled { "enabled" } else { "disabled" },
                watch.interval_secs,
                watch.command
            ));
        }
    }
    if !load.errors.is_empty() {
        out.push_str("\ninvalid entries:");
        for error in &load.errors {
            out.push_str("\n- ");
            out.push_str(&error.to_string());
        }
    }
    out.push_str("\nwatches tick only while an interactive session is open in this directory");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(prompt: Option<&str>, interval: Option<u64>) -> WatchToolInput {
        WatchToolInput {
            action: WatchAction::Add,
            name: Some("ci".into()),
            command: Some("gh run list".into()),
            prompt: prompt.map(str::to_string),
            mode: None,
            interval_secs: interval,
            timeout_secs: None,
            enabled: None,
        }
    }

    #[test]
    fn accepts_a_valid_watch() {
        let entry = build_entry(&input(Some("look"), Some(30))).unwrap();
        assert_eq!(entry["name"], serde_json::json!("ci"));
        assert_eq!(entry["intervalSecs"], serde_json::json!(30));
    }

    #[test]
    fn rejects_missing_prompt_and_out_of_range_interval() {
        assert!(build_entry(&input(None, None)).is_err());
        assert!(build_entry(&input(Some("x"), Some(1))).is_err());
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ki-watch-tool-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn add_list_remove_round_trip() {
        let dir = scratch("roundtrip");
        let tool = WatchTool::new(dir.clone(), Arc::new(|| true));
        let call = |params: serde_json::Value| {
            tool.execute("t", params, CancellationToken::new(), Arc::new(|_| {}))
        };
        call(serde_json::json!({
            "action":"add","name":"ci","command":"gh run list","prompt":"look"
        }))
        .await
        .unwrap();
        let path = dir.join(".ki/watch.json");
        assert!(std::fs::read_to_string(&path).unwrap().contains("\"ci\""));

        let listed = call(serde_json::json!({"action":"list"})).await.unwrap();
        assert!(format!("{:?}", listed.content).contains("ci"));

        call(serde_json::json!({"action":"remove","name":"ci"}))
            .await
            .unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("\"ci\""));
    }
}
