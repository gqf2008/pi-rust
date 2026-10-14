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
use kiqi_plugin_guest::bindings::kiqi::plugin::{log, net_http, progress, types, ui};

const TOOL: &str = "conformance";
const FETCH_TOOL: &str = "fetch";

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
        Err(denied) => (format!("denied:{}", denied.reason), true),
    };
    Ok(types::ToolResult {
        blocks: vec![types::ContentBlock::Text(text)],
        is_error,
        details: "null".to_string(),
    })
}

struct Conformance;

impl Guest for Conformance {
    fn init() {}

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
        ]
    }

    fn call_tool(name: String, arguments: String) -> Result<types::ToolResult, types::PluginError> {
        if name == FETCH_TOOL {
            return fetch(&arguments);
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
            Err(denied) => denied.reason,
        };

        progress::emit(&types::ProgressBlock::Text("last".to_string()));
        Ok(types::ToolResult {
            blocks: vec![types::ContentBlock::Text(format!("{arguments}|ui={ui}"))],
            is_error: false,
            // `details` is how the host carries state that survives fork/switch.
            details: r#"{"conformance":true}"#.to_string(),
        })
    }

    fn list_commands() -> Vec<types::CommandSpec> {
        Vec::new()
    }

    fn run_command(_name: String, _arguments: String) -> Result<String, types::PluginError> {
        Err(types::PluginError::Failed("no commands".to_string()))
    }

    fn list_flags() -> Vec<types::FlagSpec> {
        Vec::new()
    }

    fn list_shortcuts() -> Vec<types::ShortcutSpec> {
        Vec::new()
    }

    fn on_event(event: types::Event) -> types::EventAction {
        // Claim one event so the host test can assert the action channel.
        match event {
            types::Event::BeforeAgentStart(_) => types::EventAction::Claimed,
            _ => types::EventAction::Continue,
        }
    }

    fn on_tick() {}

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

kiqi_plugin_guest::bindings::export!(Conformance with_types_in kiqi_plugin_guest::bindings);
