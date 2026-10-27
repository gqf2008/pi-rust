//! Conformance fixture for the kiqi wasm ABI.
//!
//! Not a real extension: the smallest guest that exercises the surfaces the host
//! must get right. It is the "SDK maturity" test — it only uses the published
//! WIT, the guest SDK and the docs, and the host test asserts what the host was
//! actually asked to do (caps level), not merely that it loaded.
//!
//! `call-tool` on the single tool:
//!   * calls `log.write`         (granted, infallible)
//!   * emits two `progress` blocks (granted; the host forwards them to the
//!     tool's `ProgressSink`, proving the streaming seam)
//!   * calls `ui.set-status`     (denied by the host's stub, proving the
//!     fallible-capability path and that an extension degrades instead of failing)
//!
//! The returned string reports the deny reason so the host test can assert it.

use kiqi_wasm_bindings::bindings::exports::kiqi::extension::extension_api::Guest;
use kiqi_wasm_bindings::bindings::exports::kiqi::extension::oauth_api::Guest as OAuthGuest;
use kiqi_wasm_bindings::bindings::exports::kiqi::extension::provider_api::Guest as ProviderGuest;
use kiqi_wasm_bindings::bindings::exports::kiqi::extension::render_api::Guest as RenderGuest;
use kiqi_wasm_bindings::bindings::kiqi::extension::{
    exec, fs, log, net_http, net_sockets, proc, progress, provider_http, provider_stream,
    session_read, types, ui,
};

const TOOL: &str = "conformance";
const FETCH_TOOL: &str = "fetch";
const DIALOG_TOOL: &str = "dialog";
const SPIN_TOOL: &str = "spin";
const PANIC_TOOL: &str = "panic";
const ALLOC_TOOL: &str = "alloc";
const COMMAND: &str = "conformance";

/// Fetch a URL through the host's `net-http` capability. Whether it succeeds
/// depends entirely on the user's grant; the extension degrades either way.
fn fetch(arguments: &str) -> Result<types::ToolResult, types::ExtensionError> {
    let url = serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("url")
                .and_then(|url| url.as_str())
                .map(str::to_string)
        })
        .ok_or_else(|| types::ExtensionError::InvalidArguments("url is required".to_string()))?;
    let request = serde_json::json!({ "url": url }).to_string();
    let (text, is_error) = match net_http::fetch(&request) {
        Ok(response) => (response, false),
        Err(error) => (format!("cap:{}", capability_reason(error)), true),
    };
    Ok(types::ToolResult {
        blocks: vec![types::ContentBlock::Text(text)],
        is_error,
        details: "null".to_string(),
    })
}

/// Render a capability error for the model: a refusal and a failure read
/// differently on purpose.
/// An explicit `{cmd, args, cwd?}` from the tool arguments, so a caller can
/// drive a command that is not the default `echo` (e.g. one that leaves a
/// background child holding the pipe), optionally with a relative cwd so the
/// grant's cwd scope can be exercised.
fn exec_request(arguments: &str) -> Option<(String, Vec<String>, Option<String>)> {
    let spec = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    let spec = spec.get("exec")?;
    let cmd = spec.get("cmd")?.as_str()?.to_string();
    let args = spec
        .get("args")
        .and_then(|args| args.as_array())
        .map(|args| {
            args.iter()
                .filter_map(|arg| arg.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let cwd = spec
        .get("cwd")
        .and_then(|cwd| cwd.as_str())
        .map(str::to_string);
    Some((cmd, args, cwd))
}

/// A `proc` exercise from the tool arguments, when the caller supplied one.
struct ProcRequest {
    cmd: String,
    args: Vec<String>,
    cwd: Option<String>,
    line: Option<String>,
    count: usize,
    keep: bool,
}

fn proc_request(arguments: &str) -> Option<ProcRequest> {
    let spec = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    let spec = spec.get("proc")?;
    Some(ProcRequest {
        cmd: spec.get("cmd")?.as_str()?.to_string(),
        args: spec
            .get("args")
            .and_then(|args| args.as_array())
            .map(|args| {
                args.iter()
                    .filter_map(|arg| arg.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        cwd: spec
            .get("cwd")
            .and_then(|cwd| cwd.as_str())
            .map(str::to_string),
        line: spec
            .get("line")
            .and_then(|line| line.as_str())
            .map(str::to_string),
        count: spec
            .get("count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize,
        keep: spec
            .get("keep")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

/// The `address` field of the tool arguments, when the caller supplied one.
fn address_from(arguments: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()?
        .get("address")?
        .as_str()
        .map(str::to_string)
}

fn capability_reason(error: types::CapabilityError) -> String {
    match error {
        types::CapabilityError::Denied(reason) => reason,
        types::CapabilityError::Failed(reason) => format!("failed:{reason}"),
    }
}

struct Conformance;

/// Ask the host UI for an answer; whether it is answered depends on the grant.
fn dialog(arguments: &str) -> Result<types::ToolResult, types::ExtensionError> {
    let request = serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("request")
                .and_then(|request| request.as_str())
                .map(str::to_string)
        })
        .ok_or_else(|| {
            types::ExtensionError::InvalidArguments("request is required".to_string())
        })?;
    let (text, is_error) = match ui::dialog(&request) {
        Ok(answer) => (answer, false),
        Err(error) => (format!("cap:{}", capability_reason(error)), true),
    };
    Ok(types::ToolResult {
        blocks: vec![types::ContentBlock::Text(text)],
        is_error,
        details: "null".to_string(),
    })
}

/// Render `cli_flag`'s typed answer for the host tests.
///
/// This is the only way a guest can observe the declaration scope: the host
/// returns `None` for a flag the *calling* extension did not declare.
fn flag_value(name: &str) -> String {
    match session_read::cli_flag(name) {
        None => "none".to_string(),
        Some(types::FlagValue::Bare) => "bare".to_string(),
        Some(types::FlagValue::Text(text)) => format!("text:{text}"),
    }
}

impl Guest for Conformance {
    /// Answer the selector/editor/prompt flows `run_command` starts with the
    /// `flow:`/`editor:`/`prompt:` argument prefixes. An unknown id is refused,
    /// so the host test can prove a continuation really reaches the guest with
    /// the id it returned.
    fn continue_command(
        name: String,
        flow_id: String,
        reply: types::CommandReply,
    ) -> Result<types::CommandResult, types::ExtensionError> {
        if name != COMMAND {
            return Err(types::ExtensionError::UnknownTool(name));
        }
        let body = match (flow_id.as_str(), reply) {
            ("flow-1", types::CommandReply::Selected(value)) => format!("selected:{value}"),
            ("flow-ed", types::CommandReply::Submitted(value)) => format!("submitted:{value}"),
            ("flow-pr", types::CommandReply::Submitted(value)) => format!("prompted:{value}"),
            (id, types::CommandReply::Cancelled) => format!("cancelled:{id}"),
            (id, _) => {
                return Err(types::ExtensionError::Failed(format!(
                    "unknown flow id `{id}`"
                )))
            }
        };
        Ok(types::CommandResult {
            flow_id: String::new(),
            body: types::CommandResultBody::Message(body),
        })
    }

    fn id() -> String {
        "conformance".to_string()
    }

    fn shutdown() {}

    fn list_tools() -> Vec<types::ToolSpec> {
        vec![
            types::ToolSpec {
                name: TOOL.to_string(),
                description:
                    "Conformance probe: streams progress, logs, reports a denied capability"
                        .to_string(),
                parameters_schema: r#"{"type":"object","properties":{}}"#.to_string(),
            },
            types::ToolSpec {
                name: FETCH_TOOL.to_string(),
                description: "Fetch a URL through the host (net-http capability)".to_string(),
                parameters_schema:
                    r#"{"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}"#
                        .to_string(),
            },
            types::ToolSpec {
                name: DIALOG_TOOL.to_string(),
                description: "Ask the host UI a dialog (ui capability)".to_string(),
                parameters_schema:
                    r#"{"type":"object","properties":{"request":{"type":"string"}},"required":["request"]}"#
                        .to_string(),
            },
            types::ToolSpec {
                name: SPIN_TOOL.to_string(),
                description: "Loop forever (isolation probe)".to_string(),
                parameters_schema: r#"{"type":"object","properties":{}}"#.to_string(),
            },
            types::ToolSpec {
                name: PANIC_TOOL.to_string(),
                description: "Panic (isolation probe)".to_string(),
                parameters_schema: r#"{"type":"object","properties":{}}"#.to_string(),
            },
            types::ToolSpec {
                name: ALLOC_TOOL.to_string(),
                description: "Allocate N bytes (memory-cap probe)".to_string(),
                parameters_schema:
                    r#"{"type":"object","properties":{"bytes":{"type":"integer"}},"required":["bytes"]}"#
                        .to_string(),
            },
        ]
    }

    fn call_tool(
        name: String,
        arguments: String,
    ) -> Result<types::ToolResult, types::ExtensionError> {
        if name == SPIN_TOOL {
            let mut counter: u64 = 0;
            loop {
                counter = std::hint::black_box(counter.wrapping_add(1));
            }
        }
        if name == PANIC_TOOL {
            panic!("conformance panics on purpose");
        }
        if name == ALLOC_TOOL {
            let bytes = serde_json::from_str::<serde_json::Value>(&arguments)
                .ok()
                .and_then(|value| value.get("bytes").and_then(|bytes| bytes.as_u64()))
                .unwrap_or(0) as usize;
            // Touch every byte, so the allocation is a real `memory.grow` rather
            // than something the allocator could defer.
            let mut buffer = vec![7u8; bytes];
            std::hint::black_box(&mut buffer);
            return Ok(types::ToolResult {
                blocks: vec![types::ContentBlock::Text(format!("allocated {bytes}"))],
                is_error: false,
                details: "null".to_string(),
            });
        }
        if name == FETCH_TOOL {
            return fetch(&arguments);
        }
        if name == DIALOG_TOOL {
            return dialog(&arguments);
        }
        if name != TOOL {
            return Err(types::ExtensionError::UnknownTool(name));
        }

        log::write(types::LogLevel::Info, "conformance: call-tool");
        // `proc`: an interactive child, driven only when the caller asks for
        // one. `{"proc": {"cmd", "args", "cwd", "line", "count", "keep"}}` —
        // spawn `count` children, write `line` to the first and read its reply,
        // and (unless `keep`) close them all. `keep` exists so a test can prove
        // the host reaps at call end rather than trusting the guest to.
        let proc_outcome = match proc_request(&arguments) {
            None => "skipped".to_string(),
            Some(request) => match proc::spawn(&request.cmd, &request.args, request.cwd.as_deref())
            {
                Ok(first) => {
                    let mut spawned = vec![first];
                    for _ in 1..request.count {
                        match proc::spawn(&request.cmd, &request.args, request.cwd.as_deref()) {
                            Ok(handle) => spawned.push(handle),
                            // Report the refusal (the max-open path) and stop.
                            Err(error) => {
                                return Ok(types::ToolResult {
                                    blocks: vec![types::ContentBlock::Text(format!(
                                        "proc=denied:{}",
                                        capability_reason(error)
                                    ))],
                                    is_error: false,
                                    details: "null".to_string(),
                                })
                            }
                        }
                    }
                    let reply = match &request.line {
                        None => "no-line".to_string(),
                        Some(line) => {
                            let mut bytes = line.clone().into_bytes();
                            bytes.push(b'\n');
                            match proc::write(first, &bytes) {
                                Ok(_) => match proc::read_line(first, 64 * 1024, 0) {
                                    Ok(Some(reply)) => reply,
                                    Ok(None) => "eof".to_string(),
                                    Err(error) => format!("failed:{}", capability_reason(error)),
                                },
                                Err(error) => format!("failed:{}", capability_reason(error)),
                            }
                        }
                    };
                    // Every `proc` function gets touched: `wait` and
                    // `read-stderr` here, `kill` below (and by the host's own
                    // end-of-call reap when `keep` is set).
                    let (waited, stderr_len) = if request.keep {
                        ("skipped".to_string(), 0)
                    } else {
                        let code = match proc::wait(first, 2_000) {
                            Ok(Some(code)) => code.to_string(),
                            Ok(None) => "running".to_string(),
                            Err(error) => format!("failed:{}", capability_reason(error)),
                        };
                        let bytes = proc::read_stderr(first, 4096).unwrap_or_default();
                        (code, bytes.len())
                    };
                    if !request.keep {
                        for handle in &spawned {
                            let _ = proc::kill(*handle);
                        }
                    }
                    format!(
                        "spawned={} reply={reply} wait={waited} stderr={stderr_len}",
                        spawned.len()
                    )
                }
                Err(error) => capability_reason(error),
            },
        };

        progress::emit(&types::ProgressBlock::Text("first".to_string()));
        progress::emit(&types::ProgressBlock::Json("{\"step\":2}".to_string()));

        // `ui` is not granted: the host mounts a deny stub, so this must return
        // `Err(denied)` and leave the call itself successful.
        let ui = match ui::set_status("conformance", "probe") {
            Ok(()) => "granted".to_string(),
            Err(error) => capability_reason(error),
        };

        // `fs` is exercised the same way, but through a real implementation:
        // granted, it round-trips inside the extension's own data directory;
        // otherwise it reports the reason. Both outcomes keep the tool call
        // successful, which is the fallible-capability contract.
        let fs_outcome = match fs::write("conformance/probe.txt", b"conformance".as_slice()) {
            Ok(()) => match fs::read("conformance/probe.txt") {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(error) => capability_reason(error),
            },
            Err(error) => capability_reason(error),
        };

        // `exec`: an allow-listed bare command, no shell. Granted it returns the
        // child's stdout; otherwise the reason.
        let (exec_cmd, exec_args, exec_cwd) = exec_request(&arguments)
            .unwrap_or_else(|| ("echo".to_string(), vec!["conformance".to_string()], None));
        let exec_outcome = match exec::run(&exec_cmd, &exec_args, exec_cwd.as_deref()) {
            Ok(json) => serde_json::from_str::<serde_json::Value>(&json)
                .ok()
                .map(|value| {
                    let stdout = value
                        .get("stdout")
                        .and_then(|stdout| stdout.as_str())
                        .map(|stdout| stdout.trim().to_string())
                        .unwrap_or_default();
                    // A timed-out call is not a clean run: say so, so a caller
                    // can tell "ran and printed nothing" apart from "killed".
                    if value
                        .get("timedOut")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false)
                    {
                        format!("{stdout} timedout")
                    } else {
                        stdout
                    }
                })
                .unwrap_or_else(|| "unparseable".to_string()),
            Err(error) => capability_reason(error),
        };

        // `net-sockets`: only exercised when the caller names an address, since
        // it needs a peer. The host holds the socket; the guest gets a handle.
        let socket_outcome = match address_from(&arguments) {
            None => "skipped".to_string(),
            Some(address) => match net_sockets::connect(&address) {
                Ok(handle) => {
                    let sent = net_sockets::send(handle, b"ping".as_slice()).unwrap_or(0);
                    let received = net_sockets::recv(handle, 64)
                        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                        .unwrap_or_else(|error| capability_reason(error));
                    let _ = net_sockets::close(handle);
                    format!("sent={sent} recv={received}")
                }
                Err(error) => capability_reason(error),
            },
        };

        progress::emit(&types::ProgressBlock::Text("last".to_string()));
        Ok(types::ToolResult {
            blocks: vec![types::ContentBlock::Text(format!(
                "{arguments}|ui={ui}|fs={fs_outcome}|exec={exec_outcome}|proc={proc_outcome}|socket={socket_outcome}"
            ))],
            is_error: false,
            // `details` is how the host carries state that survives fork/switch.
            details: r#"{"conformance":true}"#.to_string(),
        })
    }

    fn list_commands() -> Vec<types::CommandSpec> {
        vec![types::CommandSpec {
            name: COMMAND.to_string(),
            description: "Conformance command".to_string(),
        }]
    }

    fn run_command(
        name: String,
        arguments: String,
    ) -> Result<types::CommandResult, types::ExtensionError> {
        if name != COMMAND {
            return Err(types::ExtensionError::UnknownTool(name));
        }
        // `flag:<name>` probes the declaration-scoped `cli-flag` read.
        if let Some(flag) = arguments.strip_prefix("flag:") {
            return Ok(types::CommandResult {
                flow_id: String::new(),
                body: types::CommandResultBody::Message(format!("flag={}", flag_value(flag))),
            });
        }
        // `flow:` starts a selector; `editor:` / `prompt:` the other kinds.
        if arguments.starts_with("flow:") {
            return Ok(types::CommandResult {
                flow_id: "flow-1".to_string(),
                body: types::CommandResultBody::Selector(types::SelectorSpec {
                    title: "Pick one".to_string(),
                    items: vec!["alpha".to_string(), "beta".to_string()],
                }),
            });
        }
        if arguments.starts_with("editor:") {
            return Ok(types::CommandResult {
                flow_id: "flow-ed".to_string(),
                body: types::CommandResultBody::Editor(types::EditorSpec {
                    initial_text: "hello".to_string(),
                }),
            });
        }
        if arguments.starts_with("prompt:") {
            return Ok(types::CommandResult {
                flow_id: "flow-pr".to_string(),
                body: types::CommandResultBody::Prompt(types::PromptSpec {
                    title: "Ask".to_string(),
                    placeholder: "type".to_string(),
                }),
            });
        }
        // Sanitization fixtures: the host must strip ANSI and reject controls.
        if arguments.starts_with("ansi:") {
            return Ok(types::CommandResult {
                flow_id: "flow-ansi".to_string(),
                body: types::CommandResultBody::Selector(types::SelectorSpec {
                    title: "\u{1b}[31mRed\u{1b}[0m".to_string(),
                    items: vec!["plain".to_string()],
                }),
            });
        }
        if arguments.starts_with("ctl:") {
            return Ok(types::CommandResult {
                flow_id: "flow-ctl".to_string(),
                body: types::CommandResultBody::Selector(types::SelectorSpec {
                    title: "bad\u{1}title".to_string(),
                    items: vec!["plain".to_string()],
                }),
            });
        }
        Ok(types::CommandResult {
            flow_id: String::new(),
            body: types::CommandResultBody::Message("conformance command ran".to_string()),
        })
    }

    fn list_flags() -> Vec<types::FlagSpec> {
        vec![types::FlagSpec {
            name: "conformance-flag".to_string(),
            description: "Conformance flag".to_string(),
        }]
    }

    fn list_shortcuts() -> Vec<types::ShortcutSpec> {
        vec![types::ShortcutSpec {
            key: "f8".to_string(),
            description: "Conformance shortcut".to_string(),
        }]
    }

    fn on_event(event: types::Event) -> types::EventAction {
        // Claim one lifecycle event so the host test can assert the action
        // channel, and claim the declared `f8` shortcut so the typed `Input`
        // event path (key/kind/modifiers) is exercised end to end.
        //
        // The provider gate fixture is driven entirely by the request payload:
        // `gatePatch` is the patch to return, `gateSpin` makes the guest slow
        // (the host's deadline must let the request proceed unpatched). A
        // `tagPatch` on any other tag proves the host ignores a patch outside
        // `before-provider-request`.
        match event {
            types::Event::BeforeAgentStart(_) => types::EventAction::Claimed,
            types::Event::Input(input) if input.key == "f8" => types::EventAction::Claimed,
            types::Event::BeforeProviderRequest(payload) => {
                log::write(
                    types::LogLevel::Info,
                    &format!("provider-event:BeforeProviderRequest:{payload}"),
                );
                // 0.8.0 投递探针：带 marker 的载荷直接 Claimed，让宿主侧能从
                // 返回值断言「这个 guest 被调用过」（provider_hooks 测试用）。
                if payload.contains("\"marker\"") {
                    return types::EventAction::Claimed;
                }
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&payload) else {
                    return types::EventAction::Continue;
                };
                if value
                    .get("gateSpinForever")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    // A busy loop, unlike the WASI `gateSpin` sleep below:
                    // epoch interruption must trap this at the host's deadline
                    // so the abandoned worker releases the instance.
                    loop {
                        std::hint::black_box(0u64);
                    }
                }
                if value
                    .get("gatePark")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    // Park inside a synchronous host import. Epoch ticks only
                    // interrupt guest execution, so the host's wall-clock
                    // deadline must abandon this call without locking the
                    // shared tool/probe instance behind it.
                    let _ = ui::dialog(r#"{"kind":"gate-park"}"#);
                }
                if value
                    .get("gateSpin")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    // Long enough that the host's deadline fires first; the
                    // late result must be discarded, not applied. A WASI
                    // sleep is invisible to epoch interruption, so this also
                    // pins the host's wall-clock fallback.
                    std::thread::sleep(std::time::Duration::from_millis(400));
                }
                match value.get("gatePatch") {
                    Some(patch) => types::EventAction::Patch(patch.to_string()),
                    None => types::EventAction::Continue,
                }
            }
            types::Event::Other(other)
                if matches!(
                    other.kind.as_str(),
                    "BeforeProviderHeaders" | "AfterProviderResponse"
                ) =>
            {
                log::write(
                    types::LogLevel::Info,
                    &format!("provider-event:{}:{}", other.kind, other.payload),
                );
                // 0.8.0 投递探针：只要被调用就 Claimed，宿主据此区分
                // 「投递了」与「从未投递」（AfterProviderResponse 应永远
                // 不会有这条路径）。
                types::EventAction::Claimed
            }
            types::Event::Other(other) if other.kind == "Context" => {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&other.payload) else {
                    return types::EventAction::Continue;
                };
                match value.get("tagPatch") {
                    Some(patch) => types::EventAction::Patch(patch.to_string()),
                    None => types::EventAction::Continue,
                }
            }
            // §6.50: `EditorChange` is a wasm-visible tag. Claim only the
            // documented payload shape, so the host e2e that dispatches it
            // through the observation funnel proves both the tag and its
            // fields crossed the WIT boundary (`Other` carries a JSON string).
            types::Event::Other(other) if other.kind == "EditorChange" => {
                match serde_json::from_str::<serde_json::Value>(&other.payload) {
                    Ok(value)
                        if value.get("chars").and_then(serde_json::Value::as_u64).is_some()
                            && value
                                .get("empty")
                                .and_then(serde_json::Value::as_bool)
                                .is_some() =>
                    {
                        types::EventAction::Claimed
                    }
                    _ => types::EventAction::Continue,
                }
            }
            _ => types::EventAction::Continue,
        }
    }

    fn before_agent_start(prompt: String) -> Option<String> {
        Some(format!("{prompt}\n[conformance]"))
    }

    fn list_probe_points() -> Vec<String> {
        vec!["before-tool-call".to_string()]
    }

    fn probe(point: String, payload: String) -> types::Verdict {
        if point == "before-tool-call" && payload.contains("block") {
            types::Verdict::Block("conformance blocks this".to_string())
        } else {
            types::Verdict::Continue
        }
    }
}

kiqi_wasm_bindings::bindings::export!(Conformance with_types_in kiqi_wasm_bindings::bindings);

// OAuth and provider are mandatory world exports. The OAuth face is a fixture
// the host e2e drives; the provider face denies, as an ungranted guest must.
impl OAuthGuest for Conformance {
    /// One static provider so the host can test its load-time cross-check. The
    /// id is fixed; the host connects it to a grant by naming the same id.
    fn list_providers() -> Vec<types::OauthProvider> {
        vec![
            types::OauthProvider {
                id: "conformance-oauth".to_string(),
                display_name: "Conformance OAuth".to_string(),
                scopes: vec!["openid".to_string()],
                flows: vec![types::OauthFlow::DeviceCode],
                supports_pkce: true,
            },
            // Declared so the deadline test can name a granted id; `action`
            // spins for it instead of returning an interaction.
            types::OauthProvider {
                id: "conformance-spin".to_string(),
                display_name: "Conformance OAuth (slow)".to_string(),
                scopes: Vec::new(),
                flows: vec![types::OauthFlow::AuthorizationCode],
                supports_pkce: false,
            },
        ]
    }

    /// Answer each flow step with a distinctive marker so the host test can
    /// tell "the guest was called" from "the host denied before reaching it".
    ///
    /// `url` is guest-supplied on purpose: the host must ignore it and build
    /// the authorization URL itself. `conformance-spin` exercises the host's
    /// wall-clock deadline.
    fn action(
        provider_id: String,
        action: types::OauthAction,
    ) -> Result<types::OauthResponse, types::ExtensionError> {
        if provider_id == "conformance-spin" {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while std::time::Instant::now() < deadline {
                std::hint::black_box(0u64);
            }
        }
        if provider_id != "conformance-oauth" && provider_id != "conformance-spin" {
            return Ok(types::OauthResponse::Denied(format!(
                "unknown provider `{provider_id}`"
            )));
        }
        match action {
            types::OauthAction::Begin(_) => {
                Ok(types::OauthResponse::Interaction(types::OauthInteraction {
                    title: "conformance oauth begin".to_string(),
                    url: Some("https://guest.example/never-opened".to_string()),
                    instructions: vec!["the host builds the URL".to_string()],
                    user_code: None,
                }))
            }
            types::OauthAction::Poll => Ok(types::OauthResponse::AuthorizationPending),
            types::OauthAction::Exchange(code) => Ok(types::OauthResponse::Error(format!(
                "conformance exchange `{code}`"
            ))),
            types::OauthAction::Cancel => Ok(types::OauthResponse::Denied(
                "conformance cancelled".to_string(),
            )),
        }
    }
}

/// The one provider the fixture declares. The `provider` grant is still the
/// catalog authority; this list can only narrow it.
const PROVIDER_ID: &str = "conformance";
const PROVIDER_MODEL: &str = "conformance-model";
/// The host side of the adapter test drives these by granted model id, so the
/// production `WasmProvider` envelope (which carries no fixture-only `mode`
/// field) can exercise streaming and cancellation end to end.
const PROVIDER_STREAM_MODEL: &str = "conformance-stream";
const PROVIDER_SLOW_MODEL: &str = "conformance-slow";
const PROVIDER_PARK_MODEL: &str = "conformance-park";

/// Wrap one answer in the assistant-message shape the host validates, with
/// deliberately wrong identity fields: the host must overwrite them.
fn provider_reply(text: String) -> Result<String, types::ExtensionError> {
    Ok(serde_json::json!({
        "role": "assistant",
        "content": [{"type": "text", "text": text}],
        "provider": "impostor",
        "model": "impostor",
        "api": "faux",
    })
    .to_string())
}

fn emit_delta(kind: types::DeltaKind, phase: types::DeltaPhase, content_index: u32, text: &str) {
    provider_stream::emit(&types::ProviderDelta {
        kind,
        phase,
        content_index,
        text: text.to_string(),
    });
}

fn provider_model_spec(id: &str) -> types::ModelSpec {
    types::ModelSpec {
        id: id.to_string(),
        name: "Conformance Model".to_string(),
        api: "openai-completions".to_string(),
        base_url: "https://api.example.test/v1".to_string(),
        reasoning: true,
        input: vec!["text".to_string()],
        context_window: 8_000,
        max_tokens: 1_024,
    }
}

/// The terminal message for the model-driven streaming fixture. It matches the
/// deltas emitted above, so a host-side adapter can assert the terminal message
/// and the live partials describe the same answer.
fn provider_stream_reply() -> Result<String, types::ExtensionError> {
    Ok(serde_json::json!({
        "role": "assistant",
        "content": [
            {"type": "text", "text": "Hello world"},
            {"type": "thinking", "thinking": "reasoned"},
            {"type": "toolCall", "id": "call-1", "name": "conformance", "arguments": {}},
        ],
        "provider": "impostor",
        "model": "impostor",
        "api": "faux",
        "stopReason": "tooluse",
    })
    .to_string())
}

impl ProviderGuest for Conformance {
    fn list_providers() -> Vec<types::ProviderSpec> {
        vec![types::ProviderSpec {
            provider_id: PROVIDER_ID.to_string(),
            display_name: "Conformance Provider".to_string(),
            models: vec![
                provider_model_spec(PROVIDER_MODEL),
                provider_model_spec(PROVIDER_STREAM_MODEL),
                provider_model_spec(PROVIDER_SLOW_MODEL),
                provider_model_spec(PROVIDER_PARK_MODEL),
            ],
        }]
    }

    fn request(request: String) -> Result<String, types::ExtensionError> {
        let value: serde_json::Value = serde_json::from_str(&request).unwrap_or_default();
        let model = value
            .get("model")
            .and_then(|model| model.as_str())
            .unwrap_or("");
        if model == PROVIDER_STREAM_MODEL {
            emit_delta(types::DeltaKind::Text, types::DeltaPhase::Start, 0, "");
            emit_delta(types::DeltaKind::Text, types::DeltaPhase::Delta, 0, "Hello");
            emit_delta(
                types::DeltaKind::Text,
                types::DeltaPhase::Delta,
                0,
                " world",
            );
            emit_delta(types::DeltaKind::Text, types::DeltaPhase::End, 0, "");
            provider_stream::emit_batch(&[
                types::ProviderDelta {
                    kind: types::DeltaKind::Thinking,
                    phase: types::DeltaPhase::Start,
                    content_index: 1,
                    text: String::new(),
                },
                types::ProviderDelta {
                    kind: types::DeltaKind::Thinking,
                    phase: types::DeltaPhase::End,
                    content_index: 1,
                    text: "reasoned".to_string(),
                },
            ]);
            emit_delta(types::DeltaKind::ToolCall, types::DeltaPhase::Start, 2, "");
            emit_delta(
                types::DeltaKind::ToolCall,
                types::DeltaPhase::End,
                2,
                r#"{"id":"call-1","name":"conformance","arguments":"{}"}"#,
            );
            return provider_stream_reply();
        }
        if model == PROVIDER_SLOW_MODEL {
            // Poll the host's cancel probe the way a real streaming guest
            // should, so aborting a run winds this down mid-generation.
            for step in 0..400u32 {
                if proc::cancel_requested() {
                    return Err(types::ExtensionError::Failed(
                        "cancelled by host".to_string(),
                    ));
                }
                if step == 0 {
                    emit_delta(types::DeltaKind::Text, types::DeltaPhase::Start, 0, "");
                }
                emit_delta(types::DeltaKind::Text, types::DeltaPhase::Delta, 0, "tick ");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            emit_delta(types::DeltaKind::Text, types::DeltaPhase::End, 0, "");
            return provider_reply("slow done".to_string());
        }
        if model == PROVIDER_PARK_MODEL {
            // Stay inside synchronous WASI longer than any test grant's wall
            // clock. Epoch interruption cannot wake this host import.
            std::thread::sleep(std::time::Duration::from_secs(60));
            return provider_reply("parked done".to_string());
        }
        let mode = value
            .get("mode")
            .and_then(|mode| mode.as_str())
            .unwrap_or("plain");
        if mode == "spin" {
            // The host's epoch deadline must interrupt this.
            loop {
                std::hint::black_box(0u64);
            }
        }
        if mode == "stream" {
            emit_delta(types::DeltaKind::Text, types::DeltaPhase::Start, 0, "");
            emit_delta(types::DeltaKind::Text, types::DeltaPhase::Delta, 0, "Hello");
            emit_delta(
                types::DeltaKind::Text,
                types::DeltaPhase::Delta,
                0,
                " world",
            );
            emit_delta(types::DeltaKind::Text, types::DeltaPhase::End, 0, "");
            provider_stream::emit_batch(&[
                types::ProviderDelta {
                    kind: types::DeltaKind::Thinking,
                    phase: types::DeltaPhase::Start,
                    content_index: 1,
                    text: String::new(),
                },
                types::ProviderDelta {
                    kind: types::DeltaKind::Thinking,
                    phase: types::DeltaPhase::End,
                    content_index: 1,
                    text: "reasoned".to_string(),
                },
            ]);
            emit_delta(types::DeltaKind::ToolCall, types::DeltaPhase::Start, 2, "");
            emit_delta(
                types::DeltaKind::ToolCall,
                types::DeltaPhase::End,
                2,
                r#"{"id":"call-1","name":"conformance","arguments":"{}"}"#,
            );
            return provider_reply("streamed".to_string());
        }
        if mode == "stream-bad" {
            // Delta before start: the host must reject the whole stream.
            emit_delta(types::DeltaKind::Text, types::DeltaPhase::Delta, 0, "x");
            return provider_reply("bad".to_string());
        }
        if mode == "stream-many-blocks" {
            // One more than the host's default 1,024 distinct-block budget.
            for index in 0..1_025u32 {
                emit_delta(types::DeltaKind::Text, types::DeltaPhase::Start, index, "");
                emit_delta(types::DeltaKind::Text, types::DeltaPhase::End, index, "");
            }
            return provider_reply("too many blocks".to_string());
        }
        if mode == "stale" {
            // No open in this call: handle 1 must not resolve.
            return match provider_http::read(1, 64) {
                Ok(_) => provider_reply("stale-read-succeeded".to_string()),
                Err(error) => provider_reply(format!("stale:{}", capability_reason(error))),
            };
        }

        let Some(url) = value.get("url").and_then(|url| url.as_str()) else {
            return provider_reply("no-url".to_string());
        };
        let headers = value
            .get("headers")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let method = value
            .get("method")
            .and_then(|method| method.as_str())
            .unwrap_or("POST");
        let body = value
            .get("body")
            .and_then(|body| body.as_str())
            .unwrap_or("");
        let open_request = serde_json::json!({
            "url": url,
            "method": method,
            "headers": headers,
            "body": body,
        })
        .to_string();
        let handle = match provider_http::open(&open_request) {
            Ok(handle) => handle,
            Err(error) => {
                return provider_reply(format!("open-denied:{}", capability_reason(error)))
            }
        };
        if mode == "leak" {
            // Deliberately never closed: the host must reap it at call exit.
            return provider_reply(format!("opened:{handle}"));
        }
        let mut body = Vec::new();
        loop {
            match provider_http::read(handle, 64 * 1024) {
                Ok(Some(chunk)) => body.extend_from_slice(&chunk),
                Ok(None) => break,
                Err(error) => {
                    let _ = provider_http::close(handle);
                    return provider_reply(format!("read-failed:{}", capability_reason(error)));
                }
            }
        }
        let _ = provider_http::close(handle);
        provider_reply(String::from_utf8_lossy(&body).into_owned())
    }
}

/// Deterministic markdown renderer used by the host's render tests.
///
/// The prefixes let the host exercise every branch of its contract without a
/// second fixture: ANSI stripping (`strip:`), control-character rejection
/// (`control:`), the output cap (`cap:`), guest failure (`fail:`) and the
/// deny-all instance (`ui:` / `session:` / `exec:`).
impl RenderGuest for Conformance {
    fn transform_markdown(markdown: String) -> Result<String, types::ExtensionError> {
        if let Some(rest) = markdown.strip_prefix("strip:") {
            return Ok(format!("\x1b[31m{rest}\x1b[0m"));
        }
        if let Some(rest) = markdown.strip_prefix("control:") {
            return Ok(format!("{rest}\x01"));
        }
        if markdown.starts_with("cap:") {
            return Ok("x".repeat(64));
        }
        if markdown.starts_with("fail:") {
            return Err(types::ExtensionError::Failed(
                "conformance render failed".to_string(),
            ));
        }
        if let Some(rest) = markdown.strip_prefix("sleep:") {
            // Parked inside a WASI host call, which epoch interruption cannot
            // see: the host's wall clock must give up on it.
            std::thread::sleep(std::time::Duration::from_millis(1_500));
            return Ok(format!("slept:{rest}"));
        }
        if markdown.starts_with("spin:") {
            // The host's epoch deadline must interrupt this; a renderer that
            // spins would otherwise hang the finalize path.
            loop {
                std::hint::black_box(0u64);
            }
        }
        if markdown.starts_with("ui:") {
            return Ok(format!(
                "ui-denied={}",
                ui::set_status("render", "x").is_err()
            ));
        }
        if markdown.starts_with("session:") {
            let info = session_read::info();
            return Ok(format!("session-id={} cwd={}", info.session_id, info.cwd));
        }
        if markdown.starts_with("exec:") {
            return Ok(format!(
                "exec-denied={}",
                exec::run("echo", &["x".to_string()], None).is_err()
            ));
        }
        Ok(format!("[render] {markdown}"))
    }
}
