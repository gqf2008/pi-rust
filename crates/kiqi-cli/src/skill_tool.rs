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
use kiqi_tools::{ExecutionEnv, FileKind};
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
    /// The session's filesystem view. The catalog's `file_path` values came out
    /// of this same env, so the re-read sees the files the loader saw, honors
    /// the cancellation token, and never blocks a runtime worker on a slow
    /// filesystem (`std::fs` here would).
    env: Arc<dyn ExecutionEnv>,
}

impl SkillTool {
    fn new(skills: Arc<RwLock<Vec<Skill>>>, env: Arc<dyn ExecutionEnv>) -> Self {
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
            env,
        }
    }
}

pub fn create_skill_tool(
    skills: Arc<RwLock<Vec<Skill>>>,
    env: Arc<dyn ExecutionEnv>,
) -> Arc<dyn AgentTool> {
    Arc::new(SkillTool::new(skills, env))
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
                    // Never enumerate `disableModelInvocation` skills here: the
                    // filter in `format_skills_for_system_prompt` keeps them out
                    // of model-visible surfaces, and a typo's error message must
                    // not defeat it.
                    let available: Vec<&str> = skills
                        .iter()
                        .filter(|skill| !skill.disable_model_invocation.unwrap_or(false))
                        .map(|skill| skill.name.as_str())
                        .collect();
                    if available.is_empty() {
                        AgentError::Tool(format!(
                            "unknown skill `{wanted}`; no model-invocable skills are available"
                        ))
                    } else {
                        AgentError::Tool(format!(
                            "unknown skill `{wanted}`; available: {}",
                            available.join(", ")
                        ))
                    }
                })?
                .clone()
        };
        if skill.disable_model_invocation.unwrap_or(false) {
            return Err(AgentError::Tool(format!(
                "skill `{}` is user-invoked only (`/skill {}`)",
                skill.name, skill.name
            )));
        }

        // Bodies are not cached since discovery went lazy, so this read is the
        // load. A failure is an error — there is no stale body to fall back to.
        let current = skill;
        let body = match self
            .env
            .read_text_file(&current.file_path, Some(&signal))
            .await
        {
            Ok(raw) => kiqi_harness::skills::skill_body(&raw),
            Err(_) if signal.is_cancelled() => {
                return Err(AgentError::Tool("skill load cancelled".into()))
            }
            Err(error) => {
                return Err(AgentError::Tool(format!(
                    "could not read skill `{}` at {}: {error}",
                    current.name, current.file_path
                )))
            }
        };
        let mut body = body.trim().to_string();
        if body.chars().count() > MAX_BODY_CHARS {
            body = body.chars().take(MAX_BODY_CHARS).collect();
            body.push_str("\n… (truncated; read the file for the rest)");
        }

        // Build the block through the harness helper so the model sees the same
        // `<skill name location>` contract whether it called this tool or the
        // user typed `/skill`; only the trailing directory listing is ours.
        let mut out =
            kiqi_harness::skills::format_skill_invocation_with_body(&current, &body, None);
        let listing = self.directory_listing(&current.file_path, &signal).await;
        if !listing.is_empty() {
            out.push_str("\n\n");
            out.push_str(&listing);
        }
        Ok(AgentToolResult::text(out))
    }
}

impl SkillTool {
    /// One-level listing of the skill's directory, so referenced files are named
    /// (relative to the skill dir) instead of guessed.
    async fn directory_listing(&self, file_path: &str, signal: &CancellationToken) -> String {
        let Some(dir) = std::path::Path::new(file_path).parent() else {
            return String::new();
        };
        let Ok(entries) = self
            .env
            .list_dir(&dir.to_string_lossy(), Some(signal))
            .await
        else {
            return String::new();
        };
        let mut names: Vec<String> = entries
            .into_iter()
            .filter_map(|entry| {
                if entry.name.eq_ignore_ascii_case("SKILL.md") {
                    return None;
                }
                // Dotfiles/dot-directories are not skill references (`.git/`,
                // `.env`, `.npmrc`): naming them would invite the model to read
                // local state the skill never advertised.
                if entry.name.starts_with('.') {
                    return None;
                }
                Some(match entry.kind {
                    FileKind::Directory => format!("{}/", entry.name),
                    _ => entry.name,
                })
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiqi_tools::OsExecutionEnv;

    fn skill(name: &str, dir: &std::path::Path, disabled: bool) -> Skill {
        Skill {
            name: name.to_string(),
            description: "d".into(),
            content: "cached body".into(),
            file_path: dir.join("SKILL.md").to_string_lossy().into_owned(),
            disable_model_invocation: disabled.then_some(true),
        }
    }

    fn env_for(dir: &std::path::Path) -> Arc<dyn ExecutionEnv> {
        Arc::new(OsExecutionEnv::with_cwd(dir.to_path_buf()))
    }

    #[tokio::test]
    async fn loads_the_body_fresh_and_lists_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("references")).unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "fresh body").unwrap();
        std::fs::write(dir.path().join("references/a.md"), "x").unwrap();
        std::fs::write(dir.path().join(".env"), "SECRET=1").unwrap();
        let cell = new_skill_cell();
        *cell.write().unwrap() = vec![skill("demo", dir.path(), false)];
        let tool = create_skill_tool(cell, env_for(dir.path()));

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
        assert!(
            !text.contains(".env"),
            "dotfiles are not skill references: {text}"
        );
    }

    /// The re-read returns the same body the loader parsed: frontmatter is
    /// stripped (only the body belongs in the `<skill>` block), and a failed
    /// re-read falls back to the cached body with a visible note.
    #[tokio::test]
    async fn a_fresh_read_strips_frontmatter_and_a_missing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let skill_path = dir.path().join("SKILL.md");
        std::fs::write(
            &skill_path,
            "---\nname: demo\ndescription: d\n---\n\nfresh body\n",
        )
        .unwrap();
        let cell = new_skill_cell();
        *cell.write().unwrap() = vec![skill("demo", dir.path(), false)];
        let tool = create_skill_tool(cell, env_for(dir.path()));

        let out = tool
            .execute(
                "t",
                serde_json::json!({ "name": "demo" }),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap();
        let text = format!("{out:?}");
        assert!(text.contains("fresh body"), "{text}");
        assert!(
            !text.contains("description: d"),
            "frontmatter must not reach the model: {text}"
        );

        // Bodies are lazy since discovery, so a disappeared file is an error:
        // there is no cached body to fall back to (and serving one silently
        // would violate the freshness contract).
        std::fs::remove_file(&skill_path).unwrap();
        let missing = tool
            .execute(
                "t",
                serde_json::json!({ "name": "demo" }),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap_err();
        let text = missing.to_string();
        assert!(text.contains("could not read skill `demo`"), "{text}");
    }

    #[tokio::test]
    async fn user_only_skills_and_unknown_names_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "body").unwrap();
        let cell = new_skill_cell();
        *cell.write().unwrap() = vec![skill("manual", dir.path(), true)];
        let tool = create_skill_tool(cell, env_for(dir.path()));

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

        // The refusal must not disclose a `disableModelInvocation` state name:
        // it is hidden from `<available_skills>` and from this error's list.
        let unknown = tool
            .execute(
                "t",
                serde_json::json!({ "name": "nope" }),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap_err();
        assert!(
            unknown.to_string().contains("no model-invocable skills"),
            "{unknown}"
        );
        assert!(!unknown.to_string().contains("manual"), "{unknown}");
    }
}
