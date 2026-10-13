//! Compile-time plugin registry.
//!
//! Plugins are cargo features rather than dynamically loaded libraries, so
//! there is no ABI to version and no `unsafe` at the boundary. The `cdylib`
//! channel (`rpi-extensions`) is untouched and keeps serving `ki dev`.

use kiqi_plugin::{HostCaps, Plugin};

/// Host capabilities backed by the real runtime-action host.
///
/// This is the sync/async seam: the host's actions are `async`, the plugin
/// boundary is sync, and the native trampoline already solved that by spawning
/// on the bridge runtime's `Handle` and parking on a std channel ("works from
/// ANY thread - no ambient runtime needed"). The adapter reuses exactly that
/// mechanism instead of inventing a second one.
pub struct ActionCaps {
    host: std::sync::Arc<dyn rpi_extensions::RuntimeActionHost>,
    runtime: tokio::runtime::Handle,
}

impl ActionCaps {
    pub fn new(
        host: std::sync::Arc<dyn rpi_extensions::RuntimeActionHost>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self { host, runtime }
    }

    fn drive(
        &self,
        action: &str,
        future: impl std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'static,
    ) -> Result<serde_json::Value, kiqi_plugin::PluginError> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.runtime.spawn(async move {
            let _ = tx.send(future.await);
        });
        match rx.recv() {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(kiqi_plugin::PluginError::Failed(error)),
            Err(_) => Err(kiqi_plugin::PluginError::Failed(format!(
                "the host dropped `{action}` without answering"
            ))),
        }
    }
}

/// `SessionRead` reads return whatever the host action returned; the plugin
/// gets the documented field rather than a raw blob.
impl kiqi_plugin::SessionRead for ActionCaps {
    fn active_tools(&self) -> Vec<String> {
        let host = self.host.clone();
        self.drive("get_active_tools", async move { host.get_active_tools(serde_json::json!({})).await })
            .ok()
            .and_then(|value| {
                value.get("tools")?.as_array().map(|tools| {
                    tools.iter().filter_map(|t| t.as_str().map(str::to_string)).collect()
                })
            })
            .unwrap_or_default()
    }

    fn system_prompt(&self) -> String {
        let host = self.host.clone();
        self.drive("get_system_prompt", async move { host.get_system_prompt(serde_json::json!({})).await })
            .ok()
            .and_then(|value| value.get("prompt")?.as_str().map(str::to_string))
            .unwrap_or_default()
    }

    fn thinking_level(&self) -> String {
        let host = self.host.clone();
        self.drive("get_thinking_level", async move { host.get_thinking_level(serde_json::json!({})).await })
            .ok()
            .and_then(|value| value.get("level")?.as_str().map(str::to_string))
            .unwrap_or_default()
    }

    fn cli_flag(&self, name: &str) -> Option<String> {
        let host = self.host.clone();
        let args = serde_json::json!({ "name": name });
        self.drive("get_cli_flag", async move { host.get_cli_flag(args).await })
            .ok()
            .and_then(|value| value.get("value")?.as_str().map(str::to_string))
    }

    fn session_branch(&self) -> serde_json::Value {
        let host = self.host.clone();
        self.drive("get_session_branch", async move { host.get_session_branch(serde_json::json!({})).await })
            .unwrap_or(serde_json::Value::Null)
    }
}

/// A plugin the host registered at build time.
pub struct Registered {
    pub id: &'static str,
    pub plugin: Box<dyn Plugin>,
}

/// Every first-party plugin compiled into this binary.
pub fn load_all() -> Vec<Registered> {
    #[allow(unused_mut)]
    let mut plugins = Vec::new();
    #[cfg(feature = "plugin-rpi-todo")]
    plugins.push(Registered {
        id: "rpi-todo",
        plugin: rpi_todo::kiqi::factory(),
    });
    plugins
}

/// A plugin plus the isolation the host owes it.
///
/// A plugin is ordinary in-process code, so "isolate the failure" here means
/// one thing: a panic must not take down the agent loop. That is the same
/// guarantee the wasm path gets from a trap, and the same one the native
/// loader gets from `catch_unwind`.
pub struct PluginHost {
    id: &'static str,
    plugin: Box<dyn Plugin>,
}

impl PluginHost {
    pub fn new(id: &'static str, plugin: Box<dyn Plugin>) -> Self {
        Self { id, plugin }
    }

    pub fn id(&self) -> &'static str {
        self.id
    }

    pub fn plugin(&self) -> &dyn Plugin {
        self.plugin.as_ref()
    }

    pub fn call_tool(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        caps: &HostCaps<'_>,
        sink: &dyn kiqi_plugin::ProgressSink,
    ) -> Result<kiqi_plugin::ToolOutcome, kiqi_plugin::PluginError> {
        self.guard_result("call", || self.plugin.call(name, arguments, caps, sink))
    }

    /// A panicking plugin observes nothing rather than aborting the loop.
    pub fn on_event(&self, event: &kiqi_plugin::Event, caps: &HostCaps<'_>) -> kiqi_plugin::EventAction {
        self.guard("on-event", || self.plugin.on_event(event, caps))
            .unwrap_or(kiqi_plugin::EventAction::Continue)
    }

    pub fn run_command(
        &self,
        name: &str,
        arguments: &str,
        caps: &HostCaps<'_>,
    ) -> Result<String, kiqi_plugin::PluginError> {
        self.guard_result("command", || self.plugin.run_command(name, arguments, caps))
    }

    /// `UnwindSafe` is not implemented for trait objects; a plugin owns no
    /// state the host observes through this call, so asserting it here is the
    /// intended use of the escape hatch rather than a shortcut.
    fn guard<T>(&self, _what: &str, body: impl FnOnce() -> T) -> Option<T> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).ok()
    }

    fn guard_result<T>(
        &self,
        what: &str,
        body: impl FnOnce() -> Result<T, kiqi_plugin::PluginError>,
    ) -> Result<T, kiqi_plugin::PluginError> {
        match self.guard(what, body) {
            Some(result) => result,
            None => Err(kiqi_plugin::PluginError::Failed(format!(
                "plugin `{}` panicked during {what}",
                self.id
            ))),
        }
    }
}

/// Wrap every registered plugin with its isolation boundary.
pub fn load_hosts() -> Vec<PluginHost> {
    load_all()
        .into_iter()
        .map(|registered| PluginHost::new(registered.id, registered.plugin))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiqi_plugin::{HostCaps, Log, LogLevel, NoProgress, SessionRead};

    struct Silent;
    impl Log for Silent {
        fn log(&self, _level: LogLevel, _message: &str) {}
    }

    #[test]
    fn registry_exposes_the_first_party_plugins() {
        let plugins = load_all();
        let todo = plugins
            .iter()
            .find(|entry| entry.id == "rpi-todo")
            .expect("rpi-todo is compiled in by the default feature");

        let tools = todo.plugin.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "todo");
        assert!(tools[0].parameters["properties"]["action"].is_object());

        assert_eq!(todo.plugin.commands()[0].name, "todos");
        assert!(todo.plugin.probe_points().is_empty());
    }

    /// A host that answers the two actions this test needs and refuses the
    /// rest, so the adapter is exercised over the real spawn/park seam.
    struct FakeHost;

    #[async_trait::async_trait]
    impl rpi_extensions::RuntimeActionHost for FakeHost {
        async fn send_message(&self, _: serde_json::Value) -> Result<serde_json::Value, String> { refused() }
        async fn send_user_message(&self, _: serde_json::Value) -> Result<serde_json::Value, String> { refused() }
        async fn append_entry(&self, _: serde_json::Value) -> Result<serde_json::Value, String> { refused() }
        async fn set_session_name(&self, _: serde_json::Value) -> Result<serde_json::Value, String> { refused() }
        async fn get_active_tools(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({ "tools": ["todo", "bash"] }))
        }
        async fn set_active_tools(&self, _: serde_json::Value) -> Result<serde_json::Value, String> { refused() }
        async fn set_model(&self, _: serde_json::Value) -> Result<serde_json::Value, String> { refused() }
        async fn get_thinking_level(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({ "level": "medium" }))
        }
        async fn set_thinking_level(&self, _: serde_json::Value) -> Result<serde_json::Value, String> { refused() }
        async fn compact(&self, _: serde_json::Value) -> Result<serde_json::Value, String> { refused() }
        async fn get_system_prompt(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({ "prompt": "you are kiqi" }))
        }
        async fn new_session(&self, _: serde_json::Value) -> Result<serde_json::Value, String> { refused() }
        async fn fork(&self, _: serde_json::Value) -> Result<serde_json::Value, String> { refused() }
        async fn navigate_tree(&self, _: serde_json::Value) -> Result<serde_json::Value, String> { refused() }
        async fn switch_session(&self, _: serde_json::Value) -> Result<serde_json::Value, String> { refused() }
        async fn reload(&self, _: serde_json::Value) -> Result<serde_json::Value, String> { refused() }
        async fn get_cli_flag(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
            let value = match args.get("name").and_then(|n| n.as_str()) {
                Some("verbose") => serde_json::json!("2"),
                _ => serde_json::Value::Null,
            };
            Ok(serde_json::json!({ "value": value }))
        }
        async fn get_session_branch(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            // The shape rpi-todo walks: entries[].message.{role,toolName,details}
            Ok(serde_json::json!({ "entries": [ { "message": {
                "role": "toolResult", "toolName": "todo",
                "details": { "todos": [{"id":1,"text":"from the branch","done":false}], "nextId": 2 }
            } } ] }))
        }
    }

    fn refused() -> Result<serde_json::Value, String> {
        Err("not implemented in this test host".to_string())
    }

    #[test]
    fn capabilities_reach_the_real_host_and_a_plugin_sees_real_data() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let caps_host = ActionCaps::new(std::sync::Arc::new(FakeHost), runtime.handle().clone());
        let silent = Silent;
        let caps = HostCaps {
            session: Some(&caps_host),
            control: None,
            inject: None,
            ui: None,
            log: &silent,
        };

        // The adapter really crosses the spawn/park seam.
        assert_eq!(caps_host.active_tools(), vec!["todo".to_string(), "bash".to_string()]);
        assert_eq!(caps_host.thinking_level(), "medium");
        assert_eq!(caps_host.system_prompt(), "you are kiqi");
        assert_eq!(caps_host.cli_flag("verbose").as_deref(), Some("2"));
        assert_eq!(caps_host.cli_flag("missing"), None);

        // And a plugin restoring through caps gets the branch the host served,
        // not a stub: this is the rpi-todo restore path end to end.
        let host = PluginHost::new("rpi-todo", rpi_todo::kiqi::factory());
        host.on_event(&kiqi_plugin::Event::SessionStart, &caps);
        let listed = host
            .call_tool("todo", &serde_json::json!({"action": "list"}), &caps, &NoProgress)
            .unwrap();
        let text = match &listed.blocks[0] {
            kiqi_plugin::Block::Text(text) => text.clone(),
            other => panic!("unexpected block {other:?}"),
        };
        assert!(text.contains("from the branch"), "got {text}");
    }

    struct Panicky;
    impl Plugin for Panicky {
        fn id(&self) -> &str {
            "panicky"
        }
        fn call(
            &self,
            _name: &str,
            _arguments: &serde_json::Value,
            _caps: &HostCaps<'_>,
            _sink: &dyn kiqi_plugin::ProgressSink,
        ) -> Result<kiqi_plugin::ToolOutcome, kiqi_plugin::PluginError> {
            panic!("this plugin is broken")
        }
        fn on_event(
            &self,
            _event: &kiqi_plugin::Event,
            _caps: &HostCaps<'_>,
        ) -> kiqi_plugin::EventAction {
            panic!("this plugin is broken on events too")
        }
    }

    #[test]
    fn a_panicking_plugin_is_contained_and_does_not_take_the_loop_down() {
        let silent = Silent;
        let caps = HostCaps::observer(&silent);
        let host = PluginHost::new("panicky", Box::new(Panicky));

        let error = host
            .call_tool("anything", &serde_json::json!({}), &caps, &NoProgress)
            .unwrap_err();
        assert!(
            error.to_string().contains("panicked during call"),
            "got {error}"
        );
        assert_eq!(
            host.on_event(&kiqi_plugin::Event::SessionStart, &caps),
            kiqi_plugin::EventAction::Continue
        );

        // The process is still alive and every other plugin still works.
        let other = load_hosts();
        let todo = other.iter().find(|h| h.id() == "rpi-todo").unwrap();
        assert!(todo
            .call_tool("todo", &serde_json::json!({"action": "list"}), &caps, &NoProgress)
            .is_ok());
    }

    #[test]
    fn a_registered_plugin_runs_through_the_trait_without_any_ffi() {
        let plugins = load_all();
        let todo = plugins.iter().find(|e| e.id == "rpi-todo").unwrap();
        let silent = Silent;
        let caps = HostCaps::observer(&silent);

        let added = todo
            .plugin
            .call(
                "todo",
                &serde_json::json!({"action": "add", "text": "wire the registry"}),
                &caps,
                &NoProgress,
            )
            .unwrap();
        assert!(!added.is_error);
        // The state rides the details channel, which is how it survives
        // fork/switch once the host persists it with the tool result.
        assert_eq!(added.details["action"], "add");
        assert_eq!(added.details["todos"][0]["text"], "wire the registry");

        // Unknown tools are a plugin error, not a panic or a crash.
        let err = todo
            .plugin
            .call("nope", &serde_json::json!({}), &caps, &NoProgress)
            .unwrap_err();
        assert!(matches!(err, kiqi_plugin::PluginError::UnknownTool(_)));
    }
}
