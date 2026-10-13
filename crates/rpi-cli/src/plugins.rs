//! Compile-time plugin registry.
//!
//! Plugins are cargo features rather than dynamically loaded libraries, so
//! there is no ABI to version and no `unsafe` at the boundary. The `cdylib`
//! channel (`rpi-extensions`) is untouched and keeps serving `ki dev`.

use kiqi_plugin::{HostCaps, Plugin};

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
    use kiqi_plugin::{HostCaps, Log, LogLevel, NoProgress};

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
