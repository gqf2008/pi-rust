//! Conformance fixture for the kiqi wasm ABI.
//!
//! Not a real plugin: the smallest guest that exercises the surfaces the host
//! must get right. It is the "SDK maturity" test — it only uses the published
//! WIT, the guest SDK and the docs, and the host test asserts what the host was
//! actually asked to do (caps level), not merely that it loaded.
//!
//! `call-tool` on the single tool:
//!   * calls `log.write`         (granted, infallible)
//!   * emits two `progress` blocks (granted; the host forwards them to the
//!     tool's `ProgressSink`, proving the streaming seam)
//!   * calls `ui.set-status`     (denied by the host's stub, proving the
//!     fallible-capability path and that a plugin degrades instead of failing)
//!
//! The returned string reports the deny reason so the host test can assert it.

use kiqi_plugin_guest::bindings::exports::kiqi::plugin::plugin_api::Guest;
use kiqi_plugin_guest::bindings::kiqi::plugin::{
    exec, fs, log, net_http, net_sockets, progress, types, ui,
};

const TOOL: &str = "conformance";
const FETCH_TOOL: &str = "fetch";
const DIALOG_TOOL: &str = "dialog";
const SPIN_TOOL: &str = "spin";
const PANIC_TOOL: &str = "panic";
const ALLOC_TOOL: &str = "alloc";
const COMMAND: &str = "conformance";

/// Fetch a URL through the host's `net-http` capability. Whether it succeeds
/// depends entirely on the user's grant; the plugin degrades either way.
fn fetch(arguments: &str) -> Result<types::ToolResult, types::PluginError> {
    let url = serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("url")
                .and_then(|url| url.as_str())
                .map(str::to_string)
        })
        .ok_or_else(|| types::PluginError::InvalidArguments("url is required".to_string()))?;
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
/// An explicit `{cmd, args}` from the tool arguments, so a caller can drive a
/// command that is not the default `echo` (e.g. one that leaves a background
/// child holding the pipe).
fn exec_request(arguments: &str) -> Option<(String, Vec<String>)> {
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
    Some((cmd, args))
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
fn dialog(arguments: &str) -> Result<types::ToolResult, types::PluginError> {
    let request = serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("request")
                .and_then(|request| request.as_str())
                .map(str::to_string)
        })
        .ok_or_else(|| types::PluginError::InvalidArguments("request is required".to_string()))?;
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

impl Guest for Conformance {
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

    fn call_tool(name: String, arguments: String) -> Result<types::ToolResult, types::PluginError> {
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
            return Err(types::PluginError::UnknownTool(name));
        }

        log::write(types::LogLevel::Info, "conformance: call-tool");
        progress::emit(&types::ProgressBlock::Text("first".to_string()));
        progress::emit(&types::ProgressBlock::Json("{\"step\":2}".to_string()));

        // `ui` is not granted: the host mounts a deny stub, so this must return
        // `Err(denied)` and leave the call itself successful.
        let ui = match ui::set_status("conformance", "probe") {
            Ok(()) => "granted".to_string(),
            Err(error) => capability_reason(error),
        };

        // `fs` is exercised the same way, but through a real implementation:
        // granted, it round-trips inside the plugin's own data directory;
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
        let (exec_cmd, exec_args) = exec_request(&arguments)
            .unwrap_or_else(|| ("echo".to_string(), vec!["conformance".to_string()]));
        let exec_outcome = match exec::run(&exec_cmd, &exec_args, None) {
            Ok(json) => serde_json::from_str::<serde_json::Value>(&json)
                .ok()
                .and_then(|value| {
                    value
                        .get("stdout")
                        .and_then(|stdout| stdout.as_str())
                        .map(|stdout| stdout.trim().to_string())
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
                "{arguments}|ui={ui}|fs={fs_outcome}|exec={exec_outcome}|socket={socket_outcome}"
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

    fn run_command(name: String, _arguments: String) -> Result<String, types::PluginError> {
        if name != COMMAND {
            return Err(types::PluginError::UnknownTool(name));
        }
        Ok("conformance command ran".to_string())
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
        // Claim one event so the host test can assert the action channel.
        match event {
            types::Event::BeforeAgentStart(_) => types::EventAction::Claimed,
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

kiqi_plugin_guest::bindings::export!(Conformance with_types_in kiqi_plugin_guest::bindings);
