//! Compile-time plugin registry.
//!
//! Plugins are cargo features rather than dynamically loaded libraries, so
//! there is no ABI to version and no `unsafe` at the boundary. The `cdylib`
//! channel (`rpi-extensions`) is untouched and keeps serving `ki dev`.

use kiqi_plugin::Plugin;

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
