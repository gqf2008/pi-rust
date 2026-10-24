//! `kiqi-codegraph` ported to the wasm ABI (0.7.0).
//!
//! The compile-time plugin spawns the `codegraph` CLI itself: a one-shot
//! `init`/`sync`, then a per-call MCP server it talks to over stdio, then it
//! kills it. A wasm guest has neither a process table nor sockets of its own,
//! so the split is exactly the one the two capabilities offer:
//!
//! * `init` / `sync` are one-shot commands → **`exec`**;
//! * the MCP session is a child the guest talks to inside one tool call →
//!   **`proc`** (the capability this port motivated: `serve --mcp` per call,
//!   spawned and killed within the call, exactly as the compiled version does).
//!
//! What stays in the guest is everything pure: the eight tool schemas, the
//! JSON-RPC request sequence, the response matching, the output/truncation and
//! the diagnostic redaction.
//!
//! Differences from the compile-time plugin, stated rather than hidden:
//!
//! 1. **`.codegraph/` is probed with the `test` command, not `stat`.** The
//!    guest's `fs` is jailed to its own data directory, so it cannot look at
//!    the project tree; `test -d <root>/.codegraph` (through `exec`) is the
//!    equivalent, and it needs `test` in the exec grant's allow-list. On a host
//!    without a `test` binary (Windows) the probe fails and the port reports
//!    it rather than guessing.
//! 2. **The command is a bare name.** The compiled version resolves the real
//!    path through PowerShell on Windows (npm/Scoop shims do not resolve); the
//!    wasm `exec`/`proc` contract is bare names only, so a Windows shim cannot
//!    be resolved here — that platform stays on the compiled plugin.
//! 3. **The project path must be reachable from the session's directory.**
//!    `projectPath` is resolved lexically against `session-read`'s
//!    `info().cwd`; the compiled version accepts any absolute path on the
//!    machine. A path outside the project directory is refused with a message
//!    naming both.
//! 4. **The session deadline is enforced per read.** `proc.read-line` takes a
//!    timeout (`0` = the child's whole budget), so the 20-second session
//!    timeout still produces the compiled version's message ("…timed out after
//!    20 seconds; try `codegraph unlock`") instead of hanging until the grant's
//!    budget runs out.
//! 5. `clientInfo.version` is this crate's version, not the compiled plugin's.

use kiqi_plugin_guest::bindings::exports::kiqi::plugin::plugin_api::Guest;
use kiqi_plugin_guest::bindings::kiqi::plugin::{exec, proc, session_read, types};
use serde_json::{json, Map, Value};

const ID: &str = "kiqi-codegraph";
/// The binary the grant must allow (both `exec` and `proc`) — and `test` for
/// the `.codegraph/` probe.
const CODEGRAPH: &str = "codegraph";
const SESSION_TIMEOUT_MS: u64 = 20_000;
const MAX_OUTPUT_CHARS: usize = 25_000;
const MAX_DIAGNOSTIC_CHARS: usize = 1_000;
const MAX_LINE_BYTES: u64 = 4 * 1024 * 1024;

struct KiCodegraph;

impl Guest for KiCodegraph {
    fn id() -> String {
        ID.to_string()
    }

    fn shutdown() {}

    fn list_tools() -> Vec<types::ToolSpec> {
        tools()
            .into_iter()
            .map(|spec| types::ToolSpec {
                name: spec.name.to_string(),
                description: spec.description.to_string(),
                parameters_schema: spec.parameters,
            })
            .collect()
    }

    fn call_tool(name: String, arguments: String) -> Result<types::ToolResult, types::PluginError> {
        if !tools().iter().any(|spec| spec.name == name) {
            return Err(types::PluginError::UnknownTool(name));
        }
        let params: Value = serde_json::from_str(&arguments).unwrap_or(Value::Null);
        let text = call_codegraph(&name, &params).map_err(types::PluginError::Failed)?;
        Ok(types::ToolResult {
            blocks: vec![types::ContentBlock::Text(text)],
            is_error: false,
            details: "null".to_string(),
        })
    }

    fn list_commands() -> Vec<types::CommandSpec> {
        Vec::new()
    }

    fn run_command(_name: String, _arguments: String) -> Result<types::CommandResult, types::PluginError> {
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

kiqi_plugin_guest::bindings::export!(KiCodegraph with_types_in kiqi_plugin_guest::bindings);

// ---------------------------------------------------------------------------
// Tool surface (ported verbatim: the model must see the same eight tools)
// ---------------------------------------------------------------------------

struct ToolSpec {
    name: &'static str,
    description: &'static str,
    parameters: String,
}

/// Every tool takes an optional absolute `projectPath`, exactly as the compiled
/// schemas do.
fn parameters(mut properties: Map<String, Value>, required: &[&str]) -> String {
    properties.insert(
        "projectPath".into(),
        json!({
            "type": "string",
            "description": "Absolute path to an initialized CodeGraph project. Defaults to the current directory."
        }),
    );
    let mut schema = json!({"type":"object", "properties": properties});
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    schema.to_string()
}

fn tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "codegraph_search",
            description: "Search indexed code symbols by name. Returns compact locations only.",
            parameters: parameters(json!({
                "query":{"type":"string"},
                "kind":{"type":"string","enum":["function","method","class","interface","type","variable","route","component"]},
                "limit":{"type":"integer","default":10}
            }).as_object().unwrap().clone(), &["query"]),
        },
        ToolSpec {
            name: "codegraph_callers",
            description: "Find functions or methods that call an indexed symbol.",
            parameters: parameters(json!({
                "symbol":{"type":"string"}, "file":{"type":"string"},
                "limit":{"type":"integer","default":20}
            }).as_object().unwrap().clone(), &["symbol"]),
        },
        ToolSpec {
            name: "codegraph_callees",
            description: "Find functions or methods called by an indexed symbol.",
            parameters: parameters(json!({
                "symbol":{"type":"string"}, "file":{"type":"string"},
                "limit":{"type":"integer","default":20}
            }).as_object().unwrap().clone(), &["symbol"]),
        },
        ToolSpec {
            name: "codegraph_impact",
            description: "Analyze the dependency impact radius of changing an indexed symbol.",
            parameters: parameters(json!({
                "symbol":{"type":"string"}, "file":{"type":"string"},
                "depth":{"type":"integer","default":2}
            }).as_object().unwrap().clone(), &["symbol"]),
        },
        ToolSpec {
            name: "codegraph_explore",
            description: "Explore a code area by question, symbol, file, or flow; returns bounded relevant source grouped by file.",
            parameters: parameters(json!({
                "query":{"type":"string"}, "maxFiles":{"type":"integer","default":8}
            }).as_object().unwrap().clone(), &["query"]),
        },
        ToolSpec {
            name: "codegraph_node",
            description: "Get one symbol with source and relationships, or read one indexed file with optional line bounds.",
            parameters: parameters(json!({
                "symbol":{"type":"string"}, "includeCode":{"type":"boolean","default":false},
                "file":{"type":"string"}, "offset":{"type":"integer"}, "limit":{"type":"integer"},
                "symbolsOnly":{"type":"boolean","default":false}, "line":{"type":"integer"}
            }).as_object().unwrap().clone(), &[]),
        },
        ToolSpec {
            name: "codegraph_status",
            description: "Report CodeGraph index health, counts, and pending synchronization.",
            parameters: parameters(Map::new(), &[]),
        },
        ToolSpec {
            name: "codegraph_files",
            description: "Get the indexed project file tree with optional path, glob, format, metadata, and depth filters.",
            parameters: parameters(json!({
                "path":{"type":"string"}, "pattern":{"type":"string"},
                "format":{"type":"string","enum":["tree","flat","grouped"],"default":"tree"},
                "includeMetadata":{"type":"boolean","default":true}, "maxDepth":{"type":"integer"}
            }).as_object().unwrap().clone(), &[]),
        },
    ]
}

// ---------------------------------------------------------------------------
// The flow
// ---------------------------------------------------------------------------

/// The project root the caller asked for, plus the path `exec`/`proc` should
/// start the child in (relative to the granted project scope; `None` = the
/// scope root itself).
struct Root {
    absolute: String,
    cwd: Option<String>,
}

fn project_root(params: &Value) -> Result<Root, String> {
    let session_cwd = session_read::info().cwd;
    let session_cwd = session_cwd.trim_end_matches('/');
    let requested = params
        .get("projectPath")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(session_cwd);
    let absolute = requested.trim_end_matches('/').to_string();
    if !absolute.starts_with('/') {
        return Err("CodeGraph projectPath must be an absolute path".into());
    }
    if absolute == session_cwd {
        return Ok(Root {
            absolute,
            cwd: None,
        });
    }
    match absolute.strip_prefix(session_cwd) {
        Some(rest) if rest.starts_with('/') && rest.len() > 1 => {
            // Own the relative path before `absolute` moves into the struct.
            let cwd = rest.trim_start_matches('/').to_string();
            Ok(Root {
                absolute,
                cwd: Some(cwd),
            })
        }
        _ => Err(format!(
            "CodeGraph projectPath `{absolute}` is outside the project directory `{session_cwd}`; \
             the wasm `exec`/`proc` scope can only reach inside it"
        )),
    }
}

/// Does `<root>/.codegraph/` exist? The guest cannot look at the project tree,
/// so it asks the `test` command through the exec grant.
fn index_exists(root: &Root) -> Result<bool, String> {
    // The child starts in the root either way (`cwd = None` is the granted
    // scope root, which is the session directory = the root when no
    // `projectPath` was given), so the probe is relative to that, not to the
    // scope root.
    let probe = ".codegraph".to_string();
    let output = exec::run("test", &["-d".to_string(), probe.clone()], root.cwd.as_deref())
        .map_err(|error| format!("test -d {probe}: {}", capability_problem("test", error)))?;
    let output = parse_exec(&output)?;
    // `test` answers with its exit status; a denied or failed call above is a
    // different story and already returned.
    Ok(output.status == Some(0))
}

/// `codegraph init <root>` when there is no index, exactly as the compiled
/// version does — but the "is there one" question is the `test` probe above.
fn ensure_initialized(root: &Root) -> Result<(), String> {
    if index_exists(root)? {
        return Ok(());
    }
    run_codegraph(root, "init")
}

/// `codegraph sync <root>`: pick up changes since the last index.
fn sync_index(root: &Root) -> Result<(), String> {
    run_codegraph(root, "sync")
}

fn run_codegraph(root: &Root, subcommand: &str) -> Result<(), String> {
    let output = exec::run(
        CODEGRAPH,
        &[subcommand.to_string(), root.absolute.clone()],
        root.cwd.as_deref(),
    )
    .map_err(|error| format!("codegraph {subcommand}: {}", capability_problem(CODEGRAPH, error)))?;
    let output = parse_exec(&output)?;
    if output.status != Some(0) {
        let stderr = sanitize_diagnostic(&output.stderr);
        return Err(format!("codegraph {subcommand} failed: {stderr}"));
    }
    Ok(())
}

/// One MCP session: spawn `codegraph serve --mcp`, initialize, call the tool,
/// read the answer, kill the child. Everything happens inside this call, which
/// is exactly the lifetime `proc` gives a handle.
fn call_codegraph(tool: &str, params: &Value) -> Result<String, String> {
    let root = project_root(params)?;
    ensure_initialized(&root)?;
    sync_index(&root)?;

    let handle = proc::spawn(
        CODEGRAPH,
        &[
            "serve".to_string(),
            "--mcp".to_string(),
            "--path".to_string(),
            root.absolute.clone(),
        ],
        root.cwd.as_deref(),
    )
    .map_err(|error| capability_problem(CODEGRAPH, error))?;

    let operation = (|| {
        let root_uri = format!("file://{}", root.absolute);
        write_rpc(
            handle,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "rootUri": root_uri,
                    "workspaceFolders": [{"uri": root_uri, "name": "project"}],
                    "capabilities": {},
                    "clientInfo": {"name": "kiqi-codegraph", "version": env!("CARGO_PKG_VERSION")}
                }
            }),
        )?;
        wait_for_response(handle, 1, session_budget_ms())?;
        write_rpc(handle, &json!({"jsonrpc":"2.0","method":"initialized","params":{}}))?;
        write_rpc(
            handle,
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":tool,"arguments":params}}),
        )?;
        let result = wait_for_response(handle, 2, session_budget_ms())?;
        response_text(&result)
    })();

    // The host would reap this anyway when the call ends; being explicit keeps
    // the child's lifetime tied to the session rather than to the tool call.
    let _ = proc::kill(handle);

    operation.map_err(|message| {
        let stderr = proc::read_stderr(handle, 16 * 1024)
            .map(|bytes| sanitize_diagnostic(&String::from_utf8_lossy(&bytes)))
            .unwrap_or_default();
        if stderr.is_empty() || message.contains(&stderr) {
            message
        } else {
            format!("{message}: {stderr}")
        }
    })
}

fn session_budget_ms() -> u64 {
    SESSION_TIMEOUT_MS.max(1)
}

/// The per-wait budget: the compiled version tracks one `Instant` deadline for
/// the whole session, while a guest cannot carry a clock reading across
/// capability calls portably. Each wait therefore gets the session budget, and
/// the total stays bounded by the child's own budget in the host.
fn write_rpc(handle: u32, payload: &Value) -> Result<(), String> {
    let mut line = serde_json::to_string(payload)
        .map_err(|error| format!("failed to encode CodeGraph request: {error}"))?
        .into_bytes();
    line.push(b'\n');
    proc::write(handle, &line)
        .map(|_| ())
        .map_err(|error| match error {
            types::CapabilityError::Denied(reason) => reason,
            types::CapabilityError::Failed(reason) => {
                format!("failed to write to CodeGraph MCP server: {reason}")
            }
        })
}

/// Read lines until the reply with `id` arrives, skipping anything else (the
/// server's notifications and its own log lines), exactly like the compiled
/// version's `wait_for_response`.
fn wait_for_response(handle: u32, id: u64, timeout_ms: u64) -> Result<Value, String> {
    loop {
        match proc::read_line(handle, MAX_LINE_BYTES, timeout_ms) {
            Ok(Some(line)) => {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if message.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if let Some(error) = message.get("error") {
                    let text = error
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| error.to_string());
                    return Err(text);
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            // stdout ended: the server left without answering.
            Ok(None) => return Err("CodeGraph MCP process closed before responding".into()),
            Err(types::CapabilityError::Denied(reason)) => return Err(reason),
            Err(types::CapabilityError::Failed(_)) => {
                return Err(format!(
                    "CodeGraph MCP session timed out after {} seconds; try `codegraph unlock`",
                    SESSION_TIMEOUT_MS / 1000
                ))
            }
        }
    }
}

/// The shape `exec`/`proc`-driven commands answer with, from the fixture's
/// convention: `{"status": <code|null>, "timedOut": bool, "stdout", "stderr"}`.
struct ExecOutput {
    status: Option<i64>,
    /// Only diagnostics are read: the compiled version ignores `init`/`sync`
    /// stdout too (it checks the exit status and reports stderr on failure).
    stderr: String,
}

fn parse_exec(raw: &str) -> Result<ExecOutput, String> {
    let parsed: Value =
        serde_json::from_str(raw).map_err(|error| format!("bad capability response: {error}"))?;
    Ok(ExecOutput {
        status: parsed.get("status").and_then(Value::as_i64),
        stderr: parsed
            .get("stderr")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// A capability refusal reads differently from a failure, and both say which
/// command needs which grant.
fn capability_problem(command: &str, error: types::CapabilityError) -> String {
    match error {
        types::CapabilityError::Denied(reason) => format!(
            "{reason} — grant it with `ki plugin grant {ID} exec --command {command} \
             --command test --cwd project` and `… proc --command {command} --cwd project`"
        ),
        types::CapabilityError::Failed(reason) => reason,
    }
}

// ---------------------------------------------------------------------------
// Pure helpers, ported verbatim from the compile-time plugin
// ---------------------------------------------------------------------------

fn response_text(result: &Value) -> Result<String, String> {
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(if text.is_empty() {
            "CodeGraph tool failed".into()
        } else {
            text
        });
    }
    let output = if text.is_empty() {
        result.to_string()
    } else {
        text
    };
    Ok(truncate_chars(&output, MAX_OUTPUT_CHARS))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let byte_end = text
        .char_indices()
        .nth(max_chars)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    let prefix = &text[..byte_end];
    let cut = prefix
        .rfind('\n')
        .filter(|index| *index >= byte_end.saturating_mul(4) / 5)
        .unwrap_or(byte_end);
    format!(
        "{}\n\n... (output truncated to {max_chars} characters)",
        &text[..cut]
    )
}

fn sanitize_diagnostic(text: &str) -> String {
    let mut cleaned = String::with_capacity(text.len().min(MAX_DIAGNOSTIC_CHARS));
    for raw in text.lines() {
        let mut line = raw.replace('\u{1b}', "");
        for marker in ["TOKEN=", "SECRET=", "PASSWORD=", "API_KEY=", "APIKEY="] {
            if let Some(start) = line.to_ascii_uppercase().find(marker) {
                let value_start = start + marker.len();
                let value_end = line[value_start..]
                    .find(char::is_whitespace)
                    .map(|offset| value_start + offset)
                    .unwrap_or(line.len());
                line.replace_range(value_start..value_end, "[redacted]");
            }
        }
        if let Some(start) = line.to_ascii_lowercase().find("bearer ") {
            let value_start = start + "bearer ".len();
            let value_end = line[value_start..]
                .find(char::is_whitespace)
                .map(|offset| value_start + offset)
                .unwrap_or(line.len());
            line.replace_range(value_start..value_end, "[redacted]");
        }
        if !cleaned.is_empty() {
            cleaned.push('\n');
        }
        cleaned.push_str(&line);
        if cleaned.chars().count() >= MAX_DIAGNOSTIC_CHARS {
            return truncate_chars(&cleaned, MAX_DIAGNOSTIC_CHARS);
        }
    }
    cleaned
}
