//! Built-in `skill` tool: load one skill's instructions on demand.
//!
//! The system prompt lists every model-visible skill as name + description +
//! location (`kiqi_harness::skills::format_skills_for_system_prompt`); this tool
//! is the load step of that progressive disclosure. It re-reads the body from
//! disk at call time, so an edited skill is current without a restart, and it
//! appends a one-level listing of the skill directory so the model can pull
//! referenced files (`references/…`, `scripts/…`) with the normal read tool
//! instead of guessing absolute paths.
//!
//! Skills whose frontmatter sets `disableModelInvocation` are refused here: they
//! are for explicit user invocation (`/skill <name>`) only.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use kiqi_agent::agent_tool::AgentTool;
use kiqi_agent::error::AgentError;
use kiqi_agent::types::{AgentToolResult, ToolExecutionMode, ToolResultPartial};
use kiqi_ai::types::Tool;
use kiqi_harness::types::Skill;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

/// Longest body returned inline. A skill that big is already pathological; the
/// model can read the rest from `location` when it genuinely needs it.
const MAX_BODY_CHARS: usize = 40_000;
/// Cap on the directory listing appended after the body.
const MAX_LISTING_ENTRIES: usize = 50;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SkillInput {
    /// Exact skill name as shown in `<available_skills>`.
    pub name: String,
}

pub struct SkillTool {
    schema: Tool,
    skills: Arc<RwLock<Vec<Skill>>>,
}

impl SkillTool {
    fn new(skills: Arc<RwLock<Vec<Skill>>>) -> Self {
        let params = schemars::schema_for!(SkillInput);
        Self {
            schema: Tool {
                name: "skill".to_string(),
                description: "Load a skill's full instructions by its exact name. \
                              Use it when the task matches a skill listed in <available_skills>; \
                              the reply carries the instructions plus a listing of the skill \
                              directory so referenced files can be read with the read tool. \
                              Re-reads the file, so edits are visible immediately."
                    .to_string(),
                parameters: kiqi_ai::types::Schema::new(
                    serde_json::to_value(params).unwrap_or_default(),
                ),
                constrained_sampling: None,
            },
            skills,
        }
    }
}

pub fn create_skill_tool(skills: Arc<RwLock<Vec<Skill>>>) -> Arc<dyn AgentTool> {
    Arc::new(SkillTool::new(skills))
}

/// The shared cell a session's skill list lives in; the reload path updates the
/// same allocation so the tool always sees the current set.
pub fn new_skill_cell() -> Arc<RwLock<Vec<Skill>>> {
    Arc::new(RwLock::new(Vec::new()))
}

#[async_trait]
impl AgentTool for SkillTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        "skill"
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let input: SkillInput = serde_json::from_value(params)
            .map_err(|error| AgentError::Validation(format!("skill input invalid: {error}")))?;
        if signal.is_cancelled() {
            return Err(AgentError::Tool("skill load cancelled".into()));
        }
        let wanted = input.name.trim();
        if wanted.is_empty() {
            return Err(AgentError::Validation("skill name is required".into()));
        }
        let skill = {
            let skills = self
                .skills
                .read()
                .map_err(|_| AgentError::Tool("skill list is poisoned".into()))?;
            if skills.is_empty() {
                return Err(AgentError::Tool(
                    "no skills are discovered in this session".into(),
                ));
            }
            skills
                .iter()
                .find(|skill| skill.name.eq_ignore_ascii_case(wanted))
                .ok_or_else(|| {
                    AgentError::Tool(format!(
                        "unknown skill `{wanted}`; available: {}",
                        skills
                            .iter()
                            .map(|skill| skill.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })?
                .clone()
        };
        if skill.disable_model_invocation.unwrap_or(false) {
            return Err(AgentError::Tool(format!(
                "skill `{}` is user-invoked only (`/skill {}`)",
                skill.name, skill.name
            )));
        }

        // Fresh read: the listing in the prompt may predate an edit.
        let body = std::fs::read_to_string(&skill.file_path).unwrap_or(skill.content);
        let mut body = body.trim().to_string();
        if body.chars().count() > MAX_BODY_CHARS {
            body = body.chars().take(MAX_BODY_CHARS).collect();
            body.push_str("\n… (truncated; read the file for the rest)");
        }

        Ok(AgentToolResult::text(format!(
            "<skill name=\"{}\" location=\"{}\">\n{}\n</skill>\n\n{}",
            skill.name,
            skill.file_path,
            body,
            directory_listing(&skill.file_path)
        )))
    }
}

/// One-level listing of the skill's directory, so referenced files are named
/// (relative to the skill dir) instead of guessed.
fn directory_listing(file_path: &str) -> String {
    let Some(dir) = std::path::Path::new(file_path).parent() else {
        return String::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return String::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "SKILL.md" {
                return None;
            }
            let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            Some(if is_dir { format!("{name}/") } else { name })
        })
        .collect();
    if names.is_empty() {
        return String::new();
    }
    names.sort();
    names.truncate(MAX_LISTING_ENTRIES);
    format!(
        "Files beside the skill (resolve against {}):\n{}",
        dir.display(),
        names
            .iter()
            .map(|name| format!("- {name}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, dir: &std::path::Path, disabled: bool) -> Skill {
        Skill {
            name: name.to_string(),
            description: "d".into(),
            content: "cached body".into(),
            file_path: dir.join("SKILL.md").to_string_lossy().into_owned(),
            disable_model_invocation: disabled.then_some(true),
        }
    }

    #[tokio::test]
    async fn loads_the_body_fresh_and_lists_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("references")).unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "fresh body").unwrap();
        std::fs::write(dir.path().join("references/a.md"), "x").unwrap();
        let cell = new_skill_cell();
        *cell.write().unwrap() = vec![skill("demo", dir.path(), false)];
        let tool = create_skill_tool(cell);

        let out = tool
            .execute(
                "t",
                serde_json::json!({ "name": "Demo" }),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap();
        let text = format!("{out:?}");
        assert!(text.contains("fresh body"), "{text}");
        assert!(text.contains("references/"), "{text}");
    }

    #[tokio::test]
    async fn user_only_skills_and_unknown_names_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "body").unwrap();
        let cell = new_skill_cell();
        *cell.write().unwrap() = vec![skill("manual", dir.path(), true)];
        let tool = create_skill_tool(cell);

        let disabled = tool
            .execute(
                "t",
                serde_json::json!({ "name": "manual" }),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap_err();
        assert!(disabled.to_string().contains("user-invoked only"));

        let unknown = tool
            .execute(
                "t",
                serde_json::json!({ "name": "nope" }),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap_err();
        assert!(unknown.to_string().contains("available: manual"));
    }
}
