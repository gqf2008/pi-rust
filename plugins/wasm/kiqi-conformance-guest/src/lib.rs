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
use kiqi_plugin_guest::bindings::kiqi::plugin::{log, progress, types, ui};

const TOOL: &str = "conformance";

struct Conformance;

impl Guest for Conformance {
    fn init() {}

    fn shutdown() {}

    fn list_tools() -> Vec<types::ToolSpec> {
        vec![types::ToolSpec {
            name: TOOL.to_string(),
            description: "Conformance probe: streams progress, logs, reports a denied capability"
                .to_string(),
            parameters_schema: r#"{"type":"object","properties":{}}"#.to_string(),
        }]
    }

    fn call_tool(name: String, arguments: String) -> Result<String, types::PluginError> {
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
        Ok(format!("{arguments}|ui={ui}"))
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

    fn on_event(_event: types::Event) {}

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
