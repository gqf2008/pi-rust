//! B5a — the `RuntimeActionHost` impl over the harness, lived in `kiqi-cli`
//! (NOT `kiqi-harness`) so `kiqi-extensions` stays a leaf in the crate DAG. The
//! trait is defined in `kiqi-extensions` (JSON + primitives only); this is the
//! host side that bridges the 16 [`RuntimeActionId`] actions to the harness.
//!
//! 10 ops delegate to `harness.lane("main")` (the `AgentLane` surface backs
//! `prompt_message`/`prompt_text`/`get_active_tools`/`set_active_tools`/
//! `set_model`/`get_thinking_level`/`set_thinking_level`/`compact`/
//! `navigate_tree`). Run-ops serialize via `acquire_run`'s `active_run` guard —
//! a concurrent extension invocation surfaces `lane_busy` as the harness error
//! string, the right behavior (an extension can't re-enter an active run).
//!
//! 6 non-lane ops:
//! - `append_entry`/`set_session_name` → `harness.session().append_message`/
//!   `set_name`.
//! - `get_system_prompt` → `AgentHarness::get_system_prompt` (B5a accessor).
//! - `new_session`/`fork`/`switch_session` → reuse `crate::session`'s
//!   create/fork/open helpers + `harness.set_session`.
//! - `reload` → handled by `ActionBridge.reload` (B5d); the host impl is the
//!   "not configured" fallback for bridges without a callback.
//!
//! ## Construction ordering (the load-bearing wrinkle)
//!
//! Extensions load **before** the harness is created (extensions provide the
//! tools the harness is built with), but the extension stores the
//! `ActionBridge`'s `user_data` pointer during `register` and that pointer
//! must remain valid for the whole session. So the host cannot hold the
//! `AgentHarness` directly — it holds an [`Arc<OnceLock<Weak<AgentHarness>>>`]
//! that is **empty at register time** and **filled once** by
//! [`HarnessActionHost::set_harness`] immediately after `AgentHarness::create`
//! succeeds. No extension can call a runtime action before the harness runs, so
//! the cell is always set before the first `get()`. The `Arc<dyn
//! RuntimeActionHost>` (and thus the `ActionBridge` pointer) is stable from
//! construction, satisfying the FFI lifetime requirement.
//!
//! The cell is **weak on purpose**: the sink the session's snapshot carries
//! reaches these capabilities (`snapshot → sink → caps → ActionCaps → host`),
//! and the harness reaches the snapshot through its `before_tool_call` hook. A
//! strong harness reference here would close
//! `harness → … → snapshot → sink → caps → host → harness`, leaking the whole
//! harness per ACP session. The session owner keeps the strong `Arc` (see
//! `session::build`), so `upgrade()` succeeds for the session's lifetime and
//! fails loudly ("not ready") only before the harness was installed or after the
//! session released it.
//!
//! The impl needs the model `catalog` for `set_model(id)` (the lane wants a
//! `Model`, not an id) and the `cwd` for session create/fork/switch. Both are
//! held by `kiqi-cli` at build time and moved into the host.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};

use kiqi_ai::types::ThinkingLevel;
use kiqi_harness::agent_harness::{AgentHarness, HarnessRunOutcome, NavigationOutcome};
use kiqi_harness::session::session::Session;
use kiqi_harness::session::types::{BranchBounds, EntryOrder, EntryQuery};
use kiqi_host::RuntimeActionHost;
use tokio::runtime::Handle;

use crate::session::{default_session_dir, open_session_by_id};

/// The `RuntimeActionHost` impl over an `AgentHarness`. Built once per session
/// in [`crate::session::build`] — constructed **empty** before extension load
/// (the harness doesn't exist yet), then filled via [`set_harness`](Self::set_harness)
/// once `AgentHarness::create` succeeds. Carried inside the [`ActionBridge`] as
/// `Arc<dyn RuntimeActionHost>`.
///
/// `runtime` is captured at build time so the bridge can spawn dispatch from
/// any thread; the impl's async methods run ON that runtime (they are spawned
/// by the trampoline), so they may freely await.
pub struct HarnessActionHost {
    /// Filled by `set_harness` after the harness exists. **Weak** so the
    /// capability chain (`snapshot → sink → caps → host`) cannot keep the
    /// harness alive: the session owner holds the strong `Arc` for as long as
    /// the session lives. `upgrade()` fails only mid-build (before any extension
    /// call) or once the session has released the harness; both surface the
    /// existing "not ready" error.
    harness: Arc<OnceLock<Weak<AgentHarness>>>,
    /// The auth-filtered catalog (the same list the TUI `/model` selector
    /// shows). `set_model(id)` resolves an id against this.
    catalog: Vec<kiqi_ai::Model>,
    /// The session cwd — `new_session`/`fork`/`switch_session` need it to
    /// locate the session dir.
    cwd: PathBuf,
    /// Parsed unknown long flags retained for extension consumption.
    cli_flags: BTreeMap<String, serde_json::Value>,
    /// The session's extension-tool context (`cwd` + session id). The session
    /// runtime actions (`new_session`/`fork`/`switch_session`) swap the session
    /// the harness serves, and must republish the id here too — an extension whose
    /// tool state keyed off the old id would otherwise keep serving the session
    /// it named before the swap.
    tool_context: kiqi_host::ToolCallContext,
    /// Shared MCP-signal state. The same swap must move this host onto the new
    /// session, or `mcp.signals.muted` and the attention inbox keep landing in
    /// the previous session's JSONL; `set_active_session` owns that move.
    signal_host: Arc<crate::signal_host::SignalHost>,
    #[allow(dead_code)]
    runtime: Handle,
}

impl HarnessActionHost {
    /// Build an **empty** host (no harness yet). The `runtime` is the handle
    /// the bridge captured (kept only so the impl can name it for future
    /// direct-spawn needs; the trampoline already spawns on the bridge's
    /// runtime). Call [`set_harness`](Self::set_harness) once the harness is
    /// created. Returns `(host, harness_cell)` where `harness_cell` is the
    /// shared weak cell the caller fills.
    pub(crate) fn new_empty(
        catalog: Vec<kiqi_ai::Model>,
        cwd: PathBuf,
        runtime: Handle,
        cli_flags: BTreeMap<String, serde_json::Value>,
        tool_context: kiqi_host::ToolCallContext,
        signal_host: Arc<crate::signal_host::SignalHost>,
    ) -> (Self, Arc<OnceLock<Weak<AgentHarness>>>) {
        let harness = Arc::new(OnceLock::new());
        (
            Self {
                harness: Arc::clone(&harness),
                catalog,
                cwd,
                cli_flags,
                tool_context,
                signal_host,
                runtime,
            },
            harness,
        )
    }

    /// Fill the harness cell with a weak handle. Call exactly once, immediately
    /// after `AgentHarness::create` succeeds, with the **session-lifetime** Arc
    /// the caller keeps (not a temporary one — a temporary would drop right
    /// here and leave the cell permanently dead).
    pub fn set_harness(cell: &Arc<OnceLock<Weak<AgentHarness>>>, harness: &Arc<AgentHarness>) {
        // `set` panics if already set; that's the right failure (double-build
        // is a programming error, not a runtime condition).
        let _ = cell.set(Arc::downgrade(harness));
    }

    /// Upgrade the weak handle, or return the "not ready" error. Fails only
    /// mid-build (before `set_harness`) or after the session released the
    /// harness — never silently substitutes another session's harness.
    fn harness(&self) -> Result<Arc<AgentHarness>, String> {
        self.harness
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| "runtime action invoked before harness was built".to_string())
    }

    /// Resolve `id` (case-insensitive exact, then substring) against the
    /// catalog. Mirrors the resolver's exact-id-first fallback.
    fn resolve_model(&self, id: &str) -> Option<kiqi_ai::Model> {
        self.catalog
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(id))
            .cloned()
            .or_else(|| {
                self.catalog
                    .iter()
                    .find(|m| m.id.to_ascii_lowercase().contains(&id.to_ascii_lowercase()))
                    .cloned()
            })
    }
}

/// Helper: pull a string field `key` from `args` (object), or return `msg`.
fn arg_str(args: &serde_json::Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing string field `{key}` in action args"))
}

/// Helper: pull an optional string field.
fn arg_str_opt(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Helper: pull a bool field (default `false`).
fn arg_bool(args: &serde_json::Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Helper: pull a string array field.
fn arg_str_array(args: &serde_json::Value, key: &str) -> Result<Vec<String>, String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .ok_or_else(|| format!("missing string-array field `{key}` in action args"))
}

/// Render a `HarnessRunOutcome` as JSON for the extension. Only the terminal
/// status + leaf id cross (the final message text is folded to a string; full
/// assistant content is too rich for a v1 action result).
fn run_outcome_json(outcome: HarnessRunOutcome) -> serde_json::Value {
    match outcome {
        HarnessRunOutcome::Completed {
            leaf_id,
            final_entry_id,
            final_message,
        } => {
            serde_json::json!({
                "status": "completed",
                "leafId": leaf_id,
                "finalEntryId": final_entry_id,
                "text": assistant_text(&final_message),
            })
        }
        HarnessRunOutcome::Aborted {
            leaf_id,
            final_entry_id,
            final_message,
        } => {
            serde_json::json!({
                "status": "aborted",
                "leafId": leaf_id,
                "finalEntryId": final_entry_id,
                "text": assistant_text(&final_message),
            })
        }
        HarnessRunOutcome::Failed {
            leaf_id,
            error,
            final_entry_id,
            final_message,
        } => {
            serde_json::json!({
                "status": "failed",
                "leafId": leaf_id,
                "error": format!("{error:?}"),
                "finalEntryId": final_entry_id,
                "text": final_message.map(|m| assistant_text(&m)).unwrap_or_default(),
            })
        }
        HarnessRunOutcome::Suspended {
            leaf_id,
            final_entry_id,
            ..
        } => {
            serde_json::json!({
                "status": "suspended",
                "leafId": leaf_id,
                "finalEntryId": final_entry_id,
            })
        }
    }
}

/// Extract the concatenated text from an assistant message (the `text` blocks).
fn assistant_text(msg: &kiqi_ai::types::AssistantMessage) -> String {
    msg.content
        .iter()
        .filter_map(|b| match b {
            kiqi_ai::types::Content::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[async_trait::async_trait]
impl RuntimeActionHost for HarnessActionHost {
    async fn get_cli_flag(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let name = arg_str(&args, "name")?;
        Ok(serde_json::json!({
            "value": self.cli_flags.get(&name).cloned().unwrap_or(serde_json::Value::Null)
        }))
    }

    /// The id of the session currently being served (republished on
    /// `new_session`/`fork`/`switch_session`, like `tool_context` itself).
    fn session_id(&self) -> Option<String> {
        self.tool_context.session_id()
    }

    async fn send_message(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        // `{"message": <AgentMessage json>}` — drive a full run from any message
        // kind. Falls back to `{"text": "..."}` as a user-text shorthand.
        let lane = self.harness()?.lane("main");
        if let Some(text) = arg_str_opt(&args, "text") {
            let result = lane
                .prompt_text(&text, Vec::new())
                .await
                .map_err(|e| e.to_string())?;
            return Ok(run_outcome_json(result.outcome));
        }
        let msg = args
            .get("message")
            .ok_or_else(|| "missing `message` or `text` field".to_string())?;
        let message: kiqi_agent::AgentMessage =
            serde_json::from_value(msg.clone()).map_err(|e| format!("invalid message: {e}"))?;
        let result = lane
            .prompt_message(message)
            .await
            .map_err(|e| e.to_string())?;
        Ok(run_outcome_json(result.outcome))
    }

    async fn send_user_message(
        &self,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let text = arg_str(&args, "text")?;
        let lane = self.harness()?.lane("main");
        let result = lane
            .prompt_text(&text, Vec::new())
            .await
            .map_err(|e| e.to_string())?;
        Ok(run_outcome_json(result.outcome))
    }

    async fn append_entry(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        // `{"message": <AgentMessage json>}` appends a message entry; OR
        // `{"customType": "...", "data": {...}}` appends a custom entry. No run
        // is driven — the entry lands in the transcript only.
        if let Some(custom_type) = arg_str_opt(&args, "customType") {
            let data = args.get("data").cloned();
            let id = self
                .harness()?
                .session()
                .append_custom_entry(&custom_type, data)
                .await
                .map_err(|e| e.to_string())?;
            return Ok(serde_json::json!({ "entryId": id }));
        }
        let msg = args
            .get("message")
            .ok_or_else(|| "missing `message` or `customType` field".to_string())?;
        let message: kiqi_agent::AgentMessage =
            serde_json::from_value(msg.clone()).map_err(|e| format!("invalid message: {e}"))?;
        let id = self
            .harness()?
            .session()
            .append_message(message)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "entryId": id }))
    }

    async fn get_session_branch(
        &self,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let entries = self
            .harness()?
            .find_entries_on_branch(
                &EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
            .map_err(|e| e.to_string())?;
        let entries = entries
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("serialize session branch: {e}"))?;
        Ok(serde_json::json!({"entries": entries}))
    }

    async fn set_session_name(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let name = arg_str(&args, "name")?;
        self.harness()?
            .session()
            .set_name(Some(&name))
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::Value::Null)
    }

    async fn get_active_tools(
        &self,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let lane = self.harness()?.lane("main");
        let tools = lane.get_active_tools().await.map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "tools": tools }))
    }

    async fn set_active_tools(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let tools = arg_str_array(&args, "tools")?;
        let lane = self.harness()?.lane("main");
        lane.set_active_tools(tools)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::Value::Null)
    }

    async fn set_model(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let id = arg_str(&args, "model")?;
        let model = self
            .resolve_model(&id)
            .ok_or_else(|| format!("model `{id}` not in catalog"))?;
        let lane = self.harness()?.lane("main");
        lane.set_model(model.clone())
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "model": model.id }))
    }

    async fn get_thinking_level(
        &self,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let lane = self.harness()?.lane("main");
        let level = lane.get_thinking_level().await.map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "level": level }))
    }

    async fn set_thinking_level(
        &self,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let level_val = args
            .get("level")
            .ok_or_else(|| "missing `level` field".to_string())?;
        let level: ThinkingLevel = if let Some(s) = level_val.as_str() {
            serde_json::from_value(serde_json::Value::String(s.to_string()))
                .map_err(|e| format!("invalid thinking level `{s}`: {e}"))?
        } else {
            serde_json::from_value(level_val.clone())
                .map_err(|e| format!("invalid thinking level: {e}"))?
        };
        let lane = self.harness()?.lane("main");
        lane.set_thinking_level(level)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::Value::Null)
    }

    async fn compact(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let custom = arg_str_opt(&args, "customInstructions");
        let lane = self.harness()?.lane("main");
        let result = lane
            .compact(custom.as_deref())
            .await
            .map_err(|e| e.to_string())?;
        Ok(
            serde_json::json!({ "runId": result.run_id, "outcome": format!("{:?}", result.outcome) }),
        )
    }

    async fn get_system_prompt(
        &self,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let prompt = self
            .harness()?
            .get_system_prompt()
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "prompt": prompt }))
    }

    async fn new_session(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        let cwd_str = self.cwd.to_string_lossy().to_string();
        let dir = default_session_dir(&self.cwd);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("create session dir {}: {e}", dir.display()))?;
        let session = crate::session::create_jsonl_session(&dir, &cwd_str)
            .await
            .map_err(|e| format!("create session: {e}"))?;
        let id = session.storage().metadata().id.clone();
        // Hold the upgraded handle across the await; `as_ref()` is the
        // `&AgentHarness` the session setter wants (a temporary `&Arc` borrow
        // does not outlive the call cleanly here).
        let harness = self.harness()?;
        crate::session::set_active_session(
            harness.as_ref(),
            session,
            Some(&self.tool_context),
            &self.signal_host,
        )
        .await?;
        Ok(serde_json::json!({ "sessionId": id }))
    }

    async fn fork(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        let cwd_str = self.cwd.to_string_lossy().to_string();
        let harness = self.harness()?;
        let new_session = crate::session::fork_session_storage(harness.as_ref(), &cwd_str)
            .await
            .map_err(|e| format!("fork session: {e}"))?;
        let id = new_session.storage().metadata().id.clone();
        crate::session::set_active_session(
            harness.as_ref(),
            new_session,
            Some(&self.tool_context),
            &self.signal_host,
        )
        .await?;
        Ok(serde_json::json!({ "sessionId": id }))
    }

    async fn navigate_tree(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let target_id = arg_str_opt(&args, "targetId");
        let summarize = arg_bool(&args, "summarize");
        let custom = arg_str_opt(&args, "customInstructions");
        let label = arg_str_opt(&args, "label");
        let lane = self.harness()?.lane("main");
        let result = lane
            .navigate_tree(
                target_id.as_deref(),
                summarize,
                custom.as_deref(),
                label.as_deref(),
            )
            .await
            .map_err(|e| e.to_string())?;
        let status = match &result.outcome {
            NavigationOutcome::Completed { .. } => "completed",
            NavigationOutcome::Declined { .. } => "declined",
            NavigationOutcome::Aborted { .. } => "aborted",
            NavigationOutcome::Failed { .. } => "failed",
        };
        Ok(serde_json::json!({ "runId": result.run_id, "status": status }))
    }

    async fn switch_session(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let id = arg_str(&args, "id")?;
        let cwd_str = self.cwd.to_string_lossy().to_string();
        let new_session: Session = open_session_by_id(&id, &cwd_str)
            .await
            .map_err(|e| e.to_string())?;
        let new_id = new_session.storage().metadata().id.clone();
        let harness = self.harness()?;
        crate::session::set_active_session(
            harness.as_ref(),
            new_session,
            Some(&self.tool_context),
            &self.signal_host,
        )
        .await?;
        Ok(serde_json::json!({ "sessionId": new_id }))
    }

    async fn reload(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        // Reached only when `ActionBridge.reload` is `None` (no /reload wired).
        // B5d wires the reload callback at the bridge layer; this host impl is
        // the "not configured" fallback.
        Err("reload not configured (no /reload callback on this bridge)".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiqi_ai::Api;
    use kiqi_harness::agent_harness::AgentHarness;
    use kiqi_harness::session::SessionValues;
    use kiqi_harness::types::{
        AgentHarnessOptions, AgentHarnessResources, DrivingMode, HarnessToolExecution, RetryPolicy,
    };

    fn harness_options(session: Session) -> AgentHarnessOptions {
        AgentHarnessOptions {
            model: kiqi_ai::Model::new("m", "m", Api::Faux, "faux", "http://x"),
            thinking_level: Default::default(),
            active_tool_names: Vec::new(),
            tools: Vec::new(),
            system_prompt: None,
            resources: AgentHarnessResources::default(),
            stream_options: Default::default(),
            retry: RetryPolicy::default(),
            compaction: Default::default(),
            steering_mode: Default::default(),
            follow_up_mode: Default::default(),
            tool_execution: HarnessToolExecution::default(),
            drive: DrivingMode::default(),
            session,
            allow_existing_session: true,
            models: Vec::new(),
            to_provider_messages: None,
            entry_projectors: Default::default(),
            agent_emitter: None,
            before_tool_call: None,
            after_tool_call: None,
            transform_context: None,
            entry_transforms: Vec::new(),
            provider_hooks: None,
        }
    }

    fn session_id(result: serde_json::Value) -> String {
        result["sessionId"]
            .as_str()
            .expect("the action returns a sessionId")
            .to_string()
    }

    async fn persisted_mute(cwd: &str, id: &str) -> Option<bool> {
        let session = crate::session::open_session_by_id(id, cwd)
            .await
            .unwrap_or_else(|error| panic!("could not open session {id}: {error}"));
        let values = SessionValues::load(&session).await.unwrap();
        values.get_as::<bool>(crate::signal_host::SESSION_MUTE_KEY)
    }

    /// The extension runtime actions (`new_session`/`fork`/`switch_session`) must
    /// move the signal host with the harness. It caches a `Session` handle
    /// whose clone snapshots the storage slot, so a swap that only calls
    /// `set_session` keeps persisting `mcp.signals.*` into the previous JSONL.
    #[tokio::test]
    async fn extension_session_actions_reattach_the_signal_host() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let cwd_str = cwd.to_string_lossy().to_string();
        let sessions = crate::session::default_session_dir(cwd);
        std::fs::create_dir_all(&sessions).unwrap();
        let initial = crate::session::create_jsonl_session(&sessions, &cwd_str)
            .await
            .unwrap();
        let harness = Arc::new(
            AgentHarness::create(harness_options(initial))
                .await
                .unwrap(),
        );

        let signal_host = Arc::new(crate::signal_host::SignalHost::new());
        let (host, cell) = HarnessActionHost::new_empty(
            Vec::new(),
            cwd.to_path_buf(),
            Handle::current(),
            BTreeMap::new(),
            kiqi_host::ToolCallContext::new(cwd_str.clone()),
            Arc::clone(&signal_host),
        );
        HarnessActionHost::set_harness(&cell, &harness);

        // The harness starts in the initial session; A becomes the muted one.
        let a = session_id(host.new_session(serde_json::json!({})).await.unwrap());
        signal_host.set_session_muted(true);
        signal_host.persist_session_muted(true).await.unwrap();

        // Switching to a fresh B re-attaches the host (a write would otherwise
        // fail with "no session attached") and loads B's per-session mute: B
        // has none, so the flag from A must be cleared.
        let b = session_id(host.new_session(serde_json::json!({})).await.unwrap());
        assert!(
            !signal_host.session_muted(),
            "an unmuted session must clear the mute carried over from the previous session"
        );

        // A write must land in B, leaving A untouched. (Before the fix the
        // persist failed against the still-attached initial session.)
        signal_host.set_session_muted(false);
        signal_host.persist_session_muted(false).await.unwrap();
        assert_eq!(persisted_mute(&cwd_str, &b).await, Some(false));
        assert_eq!(
            persisted_mute(&cwd_str, &a).await,
            Some(true),
            "B's write must not have reached A"
        );

        // Switching back loads A's mute and retargets subsequent writes at A.
        host.switch_session(serde_json::json!({ "id": a }))
            .await
            .unwrap();
        assert!(
            signal_host.session_muted(),
            "switching back must load A's persisted mute"
        );
        signal_host.set_session_muted(false);
        signal_host.persist_session_muted(false).await.unwrap();
        assert_eq!(persisted_mute(&cwd_str, &a).await, Some(false));

        // `fork` goes through the same funnel. `fork_typed` forks at a message
        // entry, and the mute writes above are custom entries, so give the
        // branch a message tip first.
        let harness = cell
            .get()
            .expect("harness installed")
            .upgrade()
            .expect("the session owner still holds the harness");
        harness
            .session()
            .append_message(kiqi_agent::AgentMessage::User(
                kiqi_ai::types::UserMessage::new("fork point", 0),
            ))
            .await
            .unwrap();
        let fork = session_id(host.fork(serde_json::json!({})).await.unwrap());
        signal_host.set_session_muted(true);
        signal_host.persist_session_muted(true).await.unwrap();
        assert_eq!(
            persisted_mute(&cwd_str, &fork).await,
            Some(true),
            "the fork's mute must be written into the fork's file"
        );
    }

    /// Every in-process swap funnels through `set_active_session`, which must
    /// republish the **shell-facing** channel too. Startup-only coverage would
    /// stay green while a fork or switch left `KI_AGENT_ID` naming the previous
    /// session — the exact staleness the shell-visible twin exists to avoid.
    ///
    /// `None` for the tool context is deliberate: the env channel is published
    /// even when no extension context is available.
    #[tokio::test]
    async fn switching_the_active_session_republishes_the_agent_identity() {
        struct Restore {
            session: Option<std::ffi::OsString>,
            agent: Option<std::ffi::OsString>,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                for (name, saved) in [
                    (kiqi_agent::SESSION_ID_ENV, &self.session),
                    (kiqi_agent::AGENT_ID_ENV, &self.agent),
                ] {
                    match saved {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }

        let _guard = crate::config::test_support::env_lock().lock();
        let _restore = Restore {
            session: std::env::var_os(kiqi_agent::SESSION_ID_ENV),
            agent: std::env::var_os(kiqi_agent::AGENT_ID_ENV),
        };

        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let cwd_str = cwd.to_string_lossy().to_string();
        let sessions = crate::session::default_session_dir(cwd);
        std::fs::create_dir_all(&sessions).unwrap();
        let initial = crate::session::create_jsonl_session(&sessions, &cwd_str)
            .await
            .unwrap();
        let harness = AgentHarness::create(harness_options(initial))
            .await
            .unwrap();
        let target = crate::session::create_jsonl_session(&sessions, &cwd_str)
            .await
            .unwrap();
        let target_id = target.get_metadata().await.unwrap().id;

        crate::session::set_active_session(
            &harness,
            target,
            None,
            &crate::signal_host::SignalHost::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            std::env::var(kiqi_agent::AGENT_ID_ENV).ok().as_deref(),
            Some(target_id.as_str()),
            "a session swap must republish KI_AGENT_ID, not leave the startup value"
        );
    }

    /// The capability chain must not keep the harness alive.
    ///
    /// The session's `RegistrySnapshot` carries the compiled sink, the sink
    /// holds the owned caps, the caps hold the action host, and the harness
    /// reaches the snapshot through its `before_tool_call` hook. While the host
    /// held a **strong** `Arc<AgentHarness>`, that was the only back-edge of
    /// `harness → … → snapshot → sink → caps → host → harness`: dropping the
    /// session's handle leaked the whole harness (and its session) for every
    /// ACP `session/new`. The host now stores a `Weak` and the session owner
    /// keeps the strong handle. This test wires the exact chain and drops that
    /// handle; the harness and its session storage must be released.
    #[tokio::test]
    async fn dropping_the_session_releases_the_harness_despite_the_caps_chain() {
        use kiqi_harness::session::memory::{InMemorySessionStorage, SystemClock};
        use kiqi_harness::session::session::DefaultIdGenerator;
        use kiqi_harness::session::types::{SessionMetadata, SessionStorage};

        let dir = tempfile::tempdir().unwrap();
        let cwd_str = dir.path().to_string_lossy().to_string();

        // 1. The action host is built first (extensions load before the harness).
        let (host, cell) = HarnessActionHost::new_empty(
            Vec::new(),
            dir.path().to_path_buf(),
            Handle::current(),
            BTreeMap::new(),
            kiqi_host::ToolCallContext::new(cwd_str),
            Arc::new(crate::signal_host::SignalHost::new()),
        );
        let host_arc: Arc<dyn kiqi_host::RuntimeActionHost> = Arc::new(host);

        // 2. caps → host, sink → caps, snapshot → sink/caps. The sink closure
        //    mirrors the production one: it owns the caps (and thus the host).
        let caps = Arc::new(crate::extensions::owned_caps(
            Arc::clone(&host_arc),
            Some(Handle::current()),
            None,
        ));
        let sink_caps = Arc::clone(&caps);
        let sink: kiqi_host::CompiledEventSink = Arc::new(move |_mode, _event| {
            let _ = &sink_caps;
            kiqi_host::CompiledVerdict::Continue
        });
        let extension_session = kiqi_host::ExtensionSession::none_with_compiled_sink_and_caps(
            Some(sink),
            Some(Arc::clone(&caps) as Arc<dyn kiqi_host::SessionCaps>),
        );

        // 3. The harness reaches the snapshot the same way `build` wires it.
        let session_cell = Arc::new(std::sync::Mutex::new(extension_session));
        let hook_cell = Arc::clone(&session_cell);
        let before_tool_call: kiqi_agent::BeforeToolCall = Arc::new(move |_ctx, _signal| {
            let _ = Arc::clone(&hook_cell);
            Box::pin(async { None })
        });

        let storage = Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "cycle-test".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        ));
        let storage_weak = Arc::downgrade(&storage);
        let session = Session::new(Arc::clone(&storage) as Arc<dyn SessionStorage>, None);
        // Only the harness may keep the storage alive from here on.
        drop(storage);

        let options = AgentHarnessOptions {
            before_tool_call: Some(before_tool_call),
            ..harness_options(session)
        };
        let harness = Arc::new(AgentHarness::create(options).await.unwrap());
        HarnessActionHost::set_harness(&cell, &harness);

        // Preconditions: the cell upgrades to the live handle, and the harness
        // still owns the session (the storage is not already gone).
        assert!(cell.get().expect("cell set").upgrade().is_some());
        assert!(storage_weak.upgrade().is_some());

        // 4. Drop the session owner's only strong handle.
        drop(harness);

        assert!(
            cell.get().expect("cell set").upgrade().is_none(),
            "the sink/caps/host chain must not keep the harness alive"
        );
        assert!(
            storage_weak.upgrade().is_none(),
            "the harness (and its session storage) must be released once its owner drops the Arc"
        );

        // Keep the capability chain alive to the end so the release above is
        // not merely the chain itself dropping early.
        drop(session_cell);
        drop(caps);
        drop(host_arc);
    }
}
