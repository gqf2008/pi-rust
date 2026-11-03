//! Built-in `cron` tool: let the model manage `.ki/cron.json` itself.
//!
//! Before this tool the only way to schedule anything was for the model to
//! hand-write the whole config file with `write`: it had to know the schema,
//! could not merge with jobs the user had already written, and got no feedback
//! (a bad field surfaced only as a reload diagnostic on a later minute). The
//! scheduler in [`crate::cron`] already reloads on mtime, so a plain file edit
//! is enough — this tool is the safe editing surface over it.
//!
//! Scope is the session working directory: the file is `<cwd>/.ki/cron.json`,
//! the same one the interactive session's scheduler reads. Headless runs have
//! no scheduler, so the tool says so rather than pretending a job will fire.

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

/// `jobs` is the object key `.ki/cron.json` uses.
const KEY: &str = "jobs";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CronAction {
    /// Show the jobs currently in `.ki/cron.json`.
    List,
    /// Add one job.
    Add,
    /// Remove one job by name.
    Remove,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CronToolInput {
    pub action: CronAction,
    /// Job name; required for `add` and `remove`.
    pub name: Option<String>,
    /// Five-field cron expression, or `@hourly` / `@daily` / `@weekly` / `@monthly`.
    pub schedule: Option<String>,
    /// Text injected as a user message when the job fires. Combine it with
    /// `command` to say what the output means for you.
    pub prompt: Option<String>,
    /// Shell command run in the session working directory when the job fires;
    /// its merged output is injected after `prompt`, when both are set.
    pub command: Option<String>,
    /// Command timeout in seconds (default 60, max 600).
    #[serde(alias = "timeout_secs")]
    pub timeout_secs: Option<u64>,
    /// Whether the job runs; defaults to true.
    pub enabled: Option<bool>,
}

pub struct CronTool {
    schema: Tool,
    cwd: PathBuf,
    trust: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl CronTool {
    fn new(cwd: PathBuf, trust: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        let params = schemars::schema_for!(CronToolInput);
        Self {
            schema: Tool {
                name: "cron".to_string(),
                description: "Manage this session's scheduled jobs in `.ki/cron.json`. \
                    `list` shows them with their next run time; `add` registers one job \
                    (`name` + `schedule` + exactly one of `prompt`/`command`); `remove` \
                    deletes it by name. Jobs tick only while an interactive session is \
                    open in this directory — missed minutes are skipped, not backfilled. \
                    Prefer this over writing the file by hand: it keeps the user's other \
                    jobs intact and reports validation errors immediately."
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
        crate::cron::config_path(&self.cwd)
    }

    fn ensure_trusted(&self) -> Result<(), AgentError> {
        if (self.trust)() {
            Ok(())
        } else {
            Err(AgentError::Tool(
                "this project is not trusted; `.ki/cron.json` is project state and would not be scheduled"
                    .into(),
            ))
        }
    }
}

pub fn create_cron_tool(
    cwd: impl Into<PathBuf>,
    trust: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Arc<dyn AgentTool> {
    Arc::new(CronTool::new(cwd.into(), trust))
}

#[async_trait]
impl AgentTool for CronTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        "cron"
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
        let input: CronToolInput = serde_json::from_value(params)
            .map_err(|error| AgentError::Validation(format!("cron input invalid: {error}")))?;
        if signal.is_cancelled() {
            return Err(AgentError::Tool("cron cancelled".into()));
        }
        let path = self.path();
        match input.action {
            CronAction::List => {
                let file = schedule_file::load(&path, KEY).map_err(AgentError::Tool)?;
                Ok(AgentToolResult::text(describe(&path, &file)))
            }
            CronAction::Add => {
                self.ensure_trusted()?;
                let mut file = schedule_file::load(&path, KEY).map_err(AgentError::Tool)?;
                let entry = build_entry(&input)?;
                let existing = names(file.entries());
                if existing
                    .iter()
                    .any(|name| name.as_str() == entry_name(&entry))
                {
                    return Err(AgentError::Validation(format!(
                        "a job named `{}` already exists; remove it first",
                        entry_name(&entry)
                    )));
                }
                let mut entries = file.entries().to_vec();
                entries.push(entry);
                file.set_entries(entries);
                schedule_file::save(&path, &file).map_err(AgentError::Tool)?;
                Ok(AgentToolResult::text(format!(
                    "added cron job `{}` to {}\n{}",
                    input.name.as_deref().unwrap_or("").trim(),
                    path.display(),
                    describe(&path, &file)
                )))
            }
            CronAction::Remove => {
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
                        format!("no job named `{wanted}`; the file has no jobs")
                    } else {
                        format!("no job named `{wanted}`; known: {}", known.join(", "))
                    }));
                }
                file.set_entries(entries);
                schedule_file::save(&path, &file).map_err(AgentError::Tool)?;
                Ok(AgentToolResult::text(format!(
                    "removed cron job `{wanted}` from {}\n{}",
                    path.display(),
                    describe(&path, &file)
                )))
            }
        }
    }
}

fn build_entry(input: &CronToolInput) -> Result<Value, AgentError> {
    let name = input
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| AgentError::Validation("`name` is required for add".into()))?;
    let schedule = input
        .schedule
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AgentError::Validation("`schedule` is required for add".into()))?;
    let prompt = input
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let command = input
        .command
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    // Both are allowed: `prompt` says what to do with the output, `command`
    // produces it. At least one is required.
    if prompt.is_none() && command.is_none() {
        return Err(AgentError::Validation(
            "one of `prompt` or `command` is required (they can be combined)".into(),
        ));
    }

    let mut entry = serde_json::Map::new();
    entry.insert("name".into(), json!(name));
    entry.insert("schedule".into(), json!(schedule));
    if let Some(prompt) = prompt {
        entry.insert("prompt".into(), json!(prompt));
    }
    if let Some(command) = command {
        entry.insert("command".into(), json!(command));
        if let Some(timeout) = input.timeout_secs {
            entry.insert("timeoutSecs".into(), json!(timeout));
        }
    }
    entry.insert("enabled".into(), json!(input.enabled.unwrap_or(true)));

    // Validate through the scheduler's own parser so the tool and the runtime
    // cannot disagree about what is a legal job.
    let single = Value::Array(vec![Value::Object(entry.clone())]);
    let load = crate::cron::parse_config(&single.to_string());
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

/// Render the file as the model needs to see it: valid jobs with their next run,
/// plus any entry the scheduler rejects (so a job written by hand is not silently
/// invisible).
fn describe(path: &std::path::Path, file: &ScheduleFile) -> String {
    let raw = Value::Array(file.entries().to_vec()).to_string();
    let load = crate::cron::parse_config(&raw);
    let mut out = String::new();
    if file.entries().is_empty() {
        out.push_str(&format!("{}: no jobs", path.display()));
    } else {
        out.push_str(&format!(
            "{}: {} job(s)",
            path.display(),
            file.entries().len()
        ));
        let now = chrono::Local::now();
        for entry in file.entries() {
            let name = entry_name(entry);
            let spec = entry
                .get("schedule")
                .and_then(Value::as_str)
                .unwrap_or("(missing schedule)");
            match load.jobs.iter().find(|job| job.name == name) {
                Some(job) => {
                    let next = job
                        .schedule
                        .next_due(now)
                        .map(|when| when.format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| "never".to_string());
                    let kind = match (&job.action.prompt, &job.action.command) {
                        (Some(_), Some(_)) => "prompt+command",
                        (Some(_), None) => "prompt",
                        (None, Some(_)) => "command",
                        (None, None) => "empty",
                    };
                    out.push_str(&format!(
                        "\n- {name} [{}] {spec} ({kind}) next {next}",
                        if job.enabled { "enabled" } else { "disabled" }
                    ));
                }
                None => out.push_str(&format!("\n- {name} [invalid] {spec}")),
            }
        }
    }
    if !load.errors.is_empty() {
        out.push_str("\ninvalid entries:");
        for error in &load.errors {
            out.push_str("\n- ");
            out.push_str(&error.to_string());
        }
    }
    out.push_str("\njobs tick only while an interactive session is open in this directory");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(prompt: Option<&str>, command: Option<&str>, schedule: &str) -> CronToolInput {
        CronToolInput {
            action: CronAction::Add,
            name: Some("supervise".into()),
            schedule: Some(schedule.into()),
            prompt: prompt.map(str::to_string),
            command: command.map(str::to_string),
            timeout_secs: None,
            enabled: None,
        }
    }

    #[test]
    fn accepts_a_prompt_job() {
        let entry = build_entry(&input(Some("check the board"), None, "*/5 * * * *")).unwrap();
        assert_eq!(entry["name"], serde_json::json!("supervise"));
        assert_eq!(entry["schedule"], serde_json::json!("*/5 * * * *"));
        assert_eq!(entry["enabled"], serde_json::json!(true));
    }

    #[test]
    fn accepts_prompt_and_command_together() {
        let entry = build_entry(&input(Some("act on it"), Some("true"), "@hourly")).unwrap();
        assert_eq!(entry["prompt"], serde_json::json!("act on it"));
        assert_eq!(entry["command"], serde_json::json!("true"));
    }

    #[test]
    fn rejects_missing_action_and_bad_schedule() {
        assert!(build_entry(&input(None, None, "@hourly")).is_err());
        assert!(build_entry(&input(Some("x"), None, "not a cron expression")).is_err());
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ki-cron-tool-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn call(
        tool: &CronTool,
        params: serde_json::Value,
    ) -> Result<AgentToolResult, AgentError> {
        tool.execute("t", params, CancellationToken::new(), Arc::new(|_| {}))
            .await
    }

    #[tokio::test]
    async fn add_list_remove_round_trip() {
        let dir = scratch("roundtrip");
        let tool = CronTool::new(dir.clone(), Arc::new(|| true));
        call(
            &tool,
            serde_json::json!({"action":"add","name":"a","schedule":"@hourly","prompt":"x"}),
        )
        .await
        .unwrap();
        let path = dir.join(".ki/cron.json");
        assert!(std::fs::read_to_string(&path).unwrap().contains("\"a\""));

        let duplicate = call(
            &tool,
            serde_json::json!({"action":"add","name":"a","schedule":"@hourly","prompt":"y"}),
        )
        .await
        .unwrap_err();
        assert!(
            duplicate.to_string().contains("already exists"),
            "{duplicate}"
        );

        let listed = call(&tool, serde_json::json!({"action":"list"}))
            .await
            .unwrap();
        assert!(format!("{:?}", listed.content).contains("a"));

        call(&tool, serde_json::json!({"action":"remove","name":"a"}))
            .await
            .unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("\"a\""));
    }

    #[tokio::test]
    async fn untrusted_project_refuses_to_write() {
        let dir = scratch("untrusted");
        let tool = CronTool::new(dir.clone(), Arc::new(|| false));
        let error = call(
            &tool,
            serde_json::json!({"action":"add","name":"a","schedule":"@hourly","prompt":"x"}),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("not trusted"), "{error}");
        assert!(!dir.join(".ki/cron.json").exists());
    }
}
