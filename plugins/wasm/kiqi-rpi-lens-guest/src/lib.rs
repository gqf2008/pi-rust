//! `rpi-lens` ported to the wasm ABI (0.6.0).
//!
//! The compile-time plugin spawns `git`/`cargo` itself with `Command`. A wasm
//! guest has no process table of its own, so the check runs through the host's
//! `exec` capability and the *user's* grant decides which commands may run and
//! in which directory scope — for this plugin that means
//! `ki plugin grant rpi-lens exec --command cargo --command git --cwd project
//! --timeout 120` (see the README).
//!
//! What stays in the guest is everything that is pure: the check-name → argv
//! mapping and the bounded output formatting, so the tool keeps its own
//! contract instead of depending on host behaviour.
//!
//! Three differences from the compile-time plugin, stated rather than hidden:
//!
//! * **No `timeoutSeconds` parameter.** The budget is the user's, not the
//!   caller's: it rides with the exec grant (`--timeout`, default 30s, cap
//!   300s). The schema drops the field so it does not promise what it cannot
//!   do.
//! * **`cwd` is scoped.** The compiled plugin accepted any absolute path on the
//!   machine. Here a cwd must be relative, or absolute *inside* the project
//!   directory (stripped against `session-read`'s `info().cwd`); the host
//!   re-jails it against the granted scope root and refuses `..`/symlink
//!   escapes.
//! * **Timeouts surface as an error.** The compiled plugin's
//!   "{check} timed out after {n}s" becomes "hit the exec time budget": the
//!   number lives in the grant, not in the call.

use kiqi_plugin_guest::bindings::exports::kiqi::plugin::plugin_api::Guest;
use kiqi_plugin_guest::bindings::kiqi::plugin::{exec, session_read, types};
use serde_json::Value;

const ID: &str = "rpi-lens";
const TOOL: &str = "code_lens";
const DESCRIPTION: &str = "Run an allowlisted, timeout-bounded local code diagnostic.";
const SCHEMA: &str = r#"{"type":"object","properties":{"check":{"type":"string","enum":["diff","cargo","fmt","clippy"]},"cwd":{"type":"string"}}}"#;
/// The compile-time plugin's own output cap, kept so the two ports agree.
/// Truncated on a char boundary (see `bounded`).
const MAX_OUTPUT_BYTES: usize = 40_000;

struct RpiLens;

impl Guest for RpiLens {
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

    fn call_tool(name: String, arguments: String) -> Result<types::ToolResult, types::PluginError> {
        if name != TOOL {
            return Err(types::PluginError::UnknownTool(name));
        }
        let text = code_lens(&arguments)?;
        Ok(types::ToolResult {
            blocks: vec![types::ContentBlock::Text(text)],
            is_error: false,
            details: "null".to_string(),
        })
    }

    fn list_commands() -> Vec<types::CommandSpec> {
        Vec::new()
    }

    fn run_command(_name: String, _arguments: String) -> Result<String, types::PluginError> {
        Err(types::PluginError::UnknownTool(_name))
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

kiqi_plugin_guest::bindings::export!(RpiLens with_types_in kiqi_plugin_guest::bindings);

/// Run one allowlisted check through the host's `exec` capability.
fn code_lens(arguments: &str) -> Result<String, types::PluginError> {
    let params: Value = serde_json::from_str(arguments)
        .map_err(|error| types::PluginError::InvalidArguments(error.to_string()))?;
    let check = params
        .get("check")
        .and_then(Value::as_str)
        .unwrap_or("diff");
    let (program, args): (&str, &[&str]) = match check {
        "diff" => ("git", &["diff", "--check"]),
        "cargo" => ("cargo", &["check"]),
        "fmt" => ("cargo", &["fmt", "--", "--check"]),
        "clippy" => ("cargo", &["clippy", "--", "-D", "warnings"]),
        _ => {
            return Err(types::PluginError::InvalidArguments(
                "check must be one of: diff, cargo, fmt, clippy".to_string(),
            ))
        }
    };
    let cwd = exec_cwd(&params)?;
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();

    let result = exec::run(program, &args, cwd.as_deref()).map_err(|error| match error {
        types::CapabilityError::Denied(reason) => types::PluginError::Failed(format!(
            // Grant replaces the whole exec policy, so the advice must name
            // every command this tool can run — not just the denied one —
            // or following it would revoke the rest.
            "{reason} — grant it with `ki plugin grant {ID} exec --command cargo \
             --command git --cwd project --timeout 120`"
        )),
        types::CapabilityError::Failed(reason) => types::PluginError::Failed(reason),
    })?;
    let result: Value = serde_json::from_str(&result)
        .map_err(|error| types::PluginError::Failed(format!("bad exec response: {error}")))?;

    if result
        .get("timedOut")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(types::PluginError::Failed(format!(
            "{check} hit the exec time budget"
        )));
    }
    let exit = result.get("status").and_then(Value::as_i64).unwrap_or(-1);
    let mut text = String::new();
    for stream in ["stdout", "stderr"] {
        if let Some(output) = result.get(stream).and_then(Value::as_str) {
            text.push_str(output);
        }
    }
    Ok(format!(
        "check={check} exit={}\n{}",
        exit,
        bounded(&mut text).trim()
    ))
}

/// Map the tool's `cwd` parameter to the relative path the host expects.
///
/// The host jails the child's cwd to the granted scope root; a guest passes a
/// path *relative* to it. An absolute path is accepted only when it is the
/// project directory itself or a subdirectory of it (the project directory is
/// `session-read`'s `info().cwd`).
fn exec_cwd(params: &Value) -> Result<Option<String>, types::PluginError> {
    let Some(cwd) = params
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    if !cwd.starts_with('/') {
        // Relative: the host's jail decides (it refuses `..` and friends).
        return Ok(Some(cwd.to_string()));
    }
    let project = session_read::info().cwd;
    let project = project.trim_end_matches('/');
    if cwd == project {
        return Ok(None);
    }
    match cwd
        .strip_prefix(project)
        .and_then(|rest| rest.strip_prefix('/'))
    {
        Some(rest) if !rest.is_empty() => Ok(Some(rest.to_string())),
        _ => Err(types::PluginError::InvalidArguments(format!(
            "cwd `{cwd}` is outside the project directory `{project}`; pass a path relative to it"
        ))),
    }
}

/// Cap the combined output at `MAX_OUTPUT_BYTES` on a char boundary.
///
/// The compile-time plugin's `text.truncate(40_000)` panics when the byte
/// offset lands mid-codepoint; the port does not inherit that bug. (No unit
/// test here: guest crates only link for `wasm32-wasip2`, so — like every
/// guest in this workspace — this is verified through the host's e2e instead.)
fn bounded(text: &mut String) -> &str {
    if text.len() > MAX_OUTPUT_BYTES {
        let mut end = MAX_OUTPUT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n[truncated]");
    }
    text
}
