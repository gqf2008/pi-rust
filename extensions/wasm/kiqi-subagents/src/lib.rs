//! `kiqi-subagents` ported to the wasm ABI (0.7.0).
//!
//! The extension registers one tool, `delegate_task`, which validates its arguments
//! and emits a bounded, non-recursive delegation envelope. That is *all* the
//! current code does: the compile-time extension does not spawn anything either —
//! "scheduling remains a host responsibility" (its own README). So the whole
//! behaviour is pure and stays in this guest, and no capability is imported.
//!
//! Two things are worth stating rather than hiding:
//!
//! * **No real spawn, and wasm is not where it would live.** The aspirational
//!   path — actually launching a child agent — is not available here. `exec`
//!   could run a bounded one-shot (default 30s, capped at 300s), but the
//!   delegation contract is a session-level child that outlives a tool call,
//!   which a wasm guest has no handle for. A real spawner belongs to the
//!   compile-time `kiqi-orchestrator`, not to this envelope tool.
//! * **Field order is pinned deliberately.** The compile-time extension builds the
//!   envelope with `serde_json::json!`, and its workspace turns on serde_json's
//!   `preserve_order`, so the keys serialize in insertion order (`kind, task,
//!   role, maxTurns, timeoutSeconds, recursive`). This guest workspace does not
//!   declare that feature, where `json!` would sort the same keys
//!   (`kind, maxTurns, recursive, role, task, timeoutSeconds`). Rather than bet
//!   the byte contract on an ambient feature flag, the envelope is written with
//!   an explicit field order; the two string values still go through
//!   `serde_json` for correct escaping, so the bytes match the compile-time
//!   extension exactly.

use kiqi_wasm_bindings::bindings::exports::kiqi::extension::extension_api::Guest;
use kiqi_wasm_bindings::bindings::kiqi::extension::types;
use serde_json::Value;

const ID: &str = "kiqi-subagents";
const TOOL: &str = "delegate_task";
const DESCRIPTION: &str = "Create a bounded, non-recursive subagent delegation envelope.";
const SCHEMA: &str = r#"{"type":"object","properties":{"task":{"type":"string","maxLength":20000},"role":{"type":"string"},"maxTurns":{"type":"integer","minimum":1,"maximum":32},"timeoutSeconds":{"type":"integer","minimum":10,"maximum":1800}},"required":["task"]}"#;

struct Subagents;

impl Guest for Subagents {
    /// No selector/editor/prompt flow exists in this extension yet; a reply is
    /// refused rather than silently dropped.
    fn continue_command(
        _name: String,
        _flow_id: String,
        _reply: types::CommandReply,
    ) -> Result<types::CommandResult, types::ExtensionError> {
        Err(types::ExtensionError::Failed(
            "this extension declares no command continuations".to_string(),
        ))
    }

    fn id() -> String {
        ID.to_string()
    }

    fn shutdown() {}

    fn list_tools() -> Vec<types::ToolSpec> {
        vec![types::ToolSpec {
            name: TOOL.to_string(),
            description: DESCRIPTION.to_string(),
            parameters_schema: SCHEMA.to_string(),
        }]
    }

    fn call_tool(
        name: String,
        arguments: String,
    ) -> Result<types::ToolResult, types::ExtensionError> {
        if name != TOOL {
            return Err(types::ExtensionError::UnknownTool(name));
        }
        let envelope = delegate_task(&arguments)?;
        Ok(types::ToolResult {
            blocks: vec![types::ContentBlock::Text(envelope)],
            is_error: false,
            details: "null".to_string(),
        })
    }

    fn list_commands() -> Vec<types::CommandSpec> {
        Vec::new()
    }

    fn run_command(
        name: String,
        _arguments: String,
    ) -> Result<types::CommandResult, types::ExtensionError> {
        Err(types::ExtensionError::UnknownTool(name))
    }

    fn list_flags() -> Vec<types::FlagSpec> {
        Vec::new()
    }

    fn list_shortcuts() -> Vec<types::ShortcutSpec> {
        Vec::new()
    }

    fn on_event(_event: types::Event) -> types::EventAction {
        types::EventAction::Continue
    }

    fn before_agent_start(_prompt: String) -> Option<String> {
        None
    }

    fn list_probe_points() -> Vec<String> {
        Vec::new()
    }

    fn probe(_point: String, _payload: String) -> types::Verdict {
        types::Verdict::Continue
    }
}

/// Validate the arguments and render the delegation envelope.
///
/// Same checks as the compile-time `delegate_task`, including its exact error
/// strings, so a caller sees the same behaviour on either tier.
fn delegate_task(arguments: &str) -> Result<String, types::ExtensionError> {
    // The compile-time kit parses leniently (`unwrap_or(Value::Null)`), so a
    // malformed argument object surfaces as a missing `task`, not as a parse
    // error. Kept identical here.
    let params: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let task = string_param(&params, "task")?;
    if task.trim().is_empty() || task.len() > 20_000 {
        return Err(types::ExtensionError::InvalidArguments(
            "task must contain 1-20000 characters".to_string(),
        ));
    }
    let role = optional_string(&params, "role", "worker");
    let max_turns = params
        .get("maxTurns")
        .and_then(Value::as_u64)
        .unwrap_or(8)
        .clamp(1, 32);
    let timeout = params
        .get("timeoutSeconds")
        .and_then(Value::as_u64)
        .unwrap_or(300)
        .clamp(10, 1800);
    Ok(envelope(&task, &role, max_turns, timeout))
}

/// The envelope, with its field order written out rather than inherited from a
/// map's ordering. See the module docs for why.
fn envelope(task: &str, role: &str, max_turns: u64, timeout_seconds: u64) -> String {
    format!(
        "{{\"kind\":\"kiqi-delegation\",\"task\":{},\"role\":{},\"maxTurns\":{},\"timeoutSeconds\":{},\"recursive\":false}}",
        Value::String(task.to_string()).to_string(),
        Value::String(role.to_string()).to_string(),
        max_turns,
        timeout_seconds,
    )
}

/// Mirror of the compile-time `kit::string_param`.
fn string_param(params: &Value, name: &str) -> Result<String, types::ExtensionError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            types::ExtensionError::InvalidArguments(format!("missing string parameter `{name}`"))
        })
}

/// Mirror of the compile-time `kit::optional_string`: a present-but-wrong-typed
/// value falls back to the default, exactly as there.
fn optional_string(params: &Value, name: &str, default: &str) -> String {
    params
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_string()
}

kiqi_wasm_bindings::bindings::export!(Subagents with_types_in kiqi_wasm_bindings::bindings);

// The 0.8.0 contribution exports are mandatory world exports; deny them all.
kiqi_wasm_bindings::deny_contribution_exports!(Subagents);
