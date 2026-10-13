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

    /// Dispatch a mutating action, discarding its success payload.
    fn run(
        &self,
        action: &'static str,
        args: serde_json::Value,
    ) -> Result<(), kiqi_plugin::PluginError> {
        let host = self.host.clone();
        let call = move |args: serde_json::Value| async move {
            match action {
                "set_session_name" => host.set_session_name(args).await,
                "set_active_tools" => host.set_active_tools(args).await,
                "set_model" => host.set_model(args).await,
                "set_thinking_level" => host.set_thinking_level(args).await,
                "compact" => host.compact(args).await,
                "new_session" => host.new_session(args).await,
                "fork" => host.fork(args).await,
                "navigate_tree" => host.navigate_tree(args).await,
                "switch_session" => host.switch_session(args).await,
                "reload" => host.reload(args).await,
                "send_message" => host.send_message(args).await,
                "send_user_message" => host.send_user_message(args).await,
                "append_entry" => host.append_entry(args).await,
                other => Err(format!("unknown action `{other}`")),
            }
        };
        self.drive(action, call(args)).map(|_| ())
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
        // The native trampoline can park the calling thread because it runs on
        // a *foreign* plugin thread. This adapter is called from the agent
        // loop, i.e. on a runtime worker: parking it outright would deadlock
        // when that worker is the only one available to run the spawned task.
        // `block_in_place` hands the worker's other tasks to a replacement
        // thread first, which is the same tool the provider-stream bridge uses.
        // Requires the multi-thread flavor (the binary sets it).
        let answer = if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| rx.recv())
        } else {
            rx.recv()
        };
        match answer {
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
        self.drive("get_active_tools", async move {
            host.get_active_tools(serde_json::json!({})).await
        })
        .ok()
        .and_then(|value| {
            value.get("tools")?.as_array().map(|tools| {
                tools
                    .iter()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .collect()
            })
        })
        .unwrap_or_default()
    }

    fn system_prompt(&self) -> String {
        let host = self.host.clone();
        self.drive("get_system_prompt", async move {
            host.get_system_prompt(serde_json::json!({})).await
        })
        .ok()
        .and_then(|value| value.get("prompt")?.as_str().map(str::to_string))
        .unwrap_or_default()
    }

    fn thinking_level(&self) -> String {
        let host = self.host.clone();
        self.drive("get_thinking_level", async move {
            host.get_thinking_level(serde_json::json!({})).await
        })
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
        self.drive("get_session_branch", async move {
            host.get_session_branch(serde_json::json!({})).await
        })
        .unwrap_or(serde_json::Value::Null)
    }
}

/// `SessionControl` and `InjectApi`. The mutation actions answer with a
/// success blob the plugin does not need, so they collapse to `Result<(), _>`.
///
/// `UiApi` is deliberately **not** implemented here: `SetStatus` /
/// `SetEditorText` / `UiComponent` are served by the bridge's own registries,
/// not by `RuntimeActionHost`, so that adapter needs the bridge rather than
/// this host handle.
impl kiqi_plugin::SessionControl for ActionCaps {
    fn set_session_name(&self, name: &str) -> Result<(), kiqi_plugin::PluginError> {
        self.run("set_session_name", serde_json::json!({ "name": name }))
    }

    fn set_active_tools(&self, tools: &[String]) -> Result<(), kiqi_plugin::PluginError> {
        self.run("set_active_tools", serde_json::json!({ "tools": tools }))
    }

    fn set_model(&self, model: &str) -> Result<(), kiqi_plugin::PluginError> {
        self.run("set_model", serde_json::json!({ "model": model }))
    }

    fn set_thinking_level(&self, level: &str) -> Result<(), kiqi_plugin::PluginError> {
        self.run("set_thinking_level", serde_json::json!({ "level": level }))
    }

    fn compact(&self) -> Result<(), kiqi_plugin::PluginError> {
        self.run("compact", serde_json::json!({}))
    }

    fn new_session(&self) -> Result<(), kiqi_plugin::PluginError> {
        self.run("new_session", serde_json::json!({}))
    }

    fn fork(&self) -> Result<(), kiqi_plugin::PluginError> {
        self.run("fork", serde_json::json!({}))
    }

    fn navigate_tree(&self, target: &str) -> Result<(), kiqi_plugin::PluginError> {
        self.run("navigate_tree", serde_json::json!({ "targetId": target }))
    }

    fn switch_session(&self, id: &str) -> Result<(), kiqi_plugin::PluginError> {
        self.run("switch_session", serde_json::json!({ "id": id }))
    }

    fn reload(&self) -> Result<(), kiqi_plugin::PluginError> {
        self.run("reload", serde_json::json!({}))
    }
}

impl kiqi_plugin::InjectApi for ActionCaps {
    fn send_message(&self, message: &serde_json::Value) -> Result<(), kiqi_plugin::PluginError> {
        self.run("send_message", serde_json::json!({ "message": message }))
    }

    fn send_user_message(&self, text: &str) -> Result<(), kiqi_plugin::PluginError> {
        self.run("send_user_message", serde_json::json!({ "text": text }))
    }

    fn append_entry(&self, entry: &serde_json::Value) -> Result<(), kiqi_plugin::PluginError> {
        self.run("append_entry", serde_json::json!({ "data": entry }))
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

/// Capabilities owned by the host, so they can be handed to a plugin per call.
///
/// [`HostCaps`] borrows its trait objects, which is right at the call site but
/// not when the capabilities have to live inside a tool adapter. The owned
/// form exists only to produce the borrowed one.
#[derive(Clone)]
pub struct OwnedCaps {
    pub session: Option<std::sync::Arc<dyn kiqi_plugin::SessionRead>>,
    pub control: Option<std::sync::Arc<dyn kiqi_plugin::SessionControl>>,
    pub inject: Option<std::sync::Arc<dyn kiqi_plugin::InjectApi>>,
    pub ui: Option<std::sync::Arc<dyn kiqi_plugin::UiApi>>,
    pub log: std::sync::Arc<dyn kiqi_plugin::Log>,
}

impl OwnedCaps {
    pub fn caps(&self) -> HostCaps<'_> {
        HostCaps {
            session: self.session.as_deref(),
            control: self.control.as_deref(),
            inject: self.inject.as_deref(),
            ui: self.ui.as_deref(),
            log: self.log.as_ref(),
        }
    }
}

/// Capabilities for a compiled plugin, backed by the real action host.
///
/// `runtime` is `None` when the caller has no tokio handle; the plugin then
/// gets logging only, rather than a capability that would fail at call time.
/// `ui` stays `None` on purpose: `SetStatus`/`SetEditorText`/`UiComponent` are
/// served by the bridge's registries, not by the action host.
pub fn owned_caps(
    host: std::sync::Arc<dyn rpi_extensions::RuntimeActionHost>,
    runtime: Option<tokio::runtime::Handle>,
) -> OwnedCaps {
    let mut caps = OwnedCaps {
        session: None,
        control: None,
        inject: None,
        ui: None,
        log: std::sync::Arc::new(StderrLog),
    };
    if let Some(runtime) = runtime {
        let shared = std::sync::Arc::new(ActionCaps::new(host, runtime));
        caps.session = Some(shared.clone() as std::sync::Arc<dyn kiqi_plugin::SessionRead>);
        caps.control = Some(shared.clone() as std::sync::Arc<dyn kiqi_plugin::SessionControl>);
        caps.inject = Some(shared as std::sync::Arc<dyn kiqi_plugin::InjectApi>);
    }
    caps
}

/// Plugin logging goes to stderr for now, prefixed so it is attributable.
pub struct StderrLog;

impl kiqi_plugin::Log for StderrLog {
    fn log(&self, level: kiqi_plugin::LogLevel, message: &str) {
        eprintln!("[plugin/{level:?}] {message}");
    }
}

/// A plugin tool exposed as a harness tool.
///
/// The counterpart of `PluginToolAdapter` for compiled-in plugins: same
/// `AgentTool` surface, but the call is a plain Rust method - no `PluginApi`,
/// no handle, no `free_string`, no keepalive.
pub struct CompiledToolAdapter {
    host: std::sync::Arc<PluginHost>,
    name: String,
    label: String,
    schema: rpi_ai::types::Tool,
    caps: OwnedCaps,
}

impl CompiledToolAdapter {
    /// `None` when the plugin's parameter schema will not parse; the caller
    /// diagnoses and skips rather than losing the whole tool list.
    pub fn new(
        host: std::sync::Arc<PluginHost>,
        spec: &kiqi_plugin::ToolSpec,
        caps: OwnedCaps,
    ) -> Option<Self> {
        let parameters =
            serde_json::from_value::<rpi_ai::types::Schema>(spec.parameters.clone()).ok()?;
        let label = format!("{}:{}", host.id(), spec.name);
        Some(Self {
            host,
            name: spec.name.clone(),
            label,
            schema: rpi_ai::types::Tool {
                name: spec.name.clone(),
                description: spec.description.clone(),
                parameters,
                constrained_sampling: None,
            },
            caps,
        })
    }
}

/// Turns a plugin's partial output into the harness's update callback.
struct UpdateSink {
    on_update: std::sync::Arc<dyn Fn(rpi_agent::types::ToolResultPartial) + Send + Sync>,
}

impl kiqi_plugin::ProgressSink for UpdateSink {
    fn progress(&self, progress: kiqi_plugin::Progress) {
        let text = match progress {
            kiqi_plugin::Progress::Text(text) => text,
            kiqi_plugin::Progress::Json(value) => value.to_string(),
        };
        (self.on_update)(rpi_agent::types::AgentToolResult {
            content: vec![rpi_agent::types::TextContentOrImage::Text(
                rpi_ai::types::TextContent {
                    kind: Default::default(),
                    text,
                    text_signature: None,
                },
            )],
            ..Default::default()
        });
    }
}

#[async_trait::async_trait]
impl rpi_agent::agent_tool::AgentTool for CompiledToolAdapter {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        &self.label
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: tokio_util::sync::CancellationToken,
        on_update: std::sync::Arc<dyn Fn(rpi_agent::types::ToolResultPartial) + Send + Sync>,
    ) -> Result<rpi_agent::types::AgentToolResult, rpi_agent::error::AgentError> {
        let sink = UpdateSink { on_update };
        let caps = self.caps.caps();
        let outcome = self
            .host
            .call_tool(&self.name, &params, &caps, &sink)
            .map_err(|error| rpi_agent::error::AgentError::tool(error.to_string()))?;

        let content = outcome
            .blocks
            .iter()
            .map(|block| match block {
                kiqi_plugin::Block::Text(text) => {
                    rpi_agent::types::TextContentOrImage::Text(rpi_ai::types::TextContent {
                        kind: Default::default(),
                        text: text.clone(),
                        text_signature: None,
                    })
                }
                kiqi_plugin::Block::Json(value) => {
                    rpi_agent::types::TextContentOrImage::Text(rpi_ai::types::TextContent {
                        kind: Default::default(),
                        text: value.to_string(),
                        text_signature: None,
                    })
                }
            })
            .collect();

        // `is_error` becomes a tool-level error so the model sees a failed call
        // instead of a crash, matching how the native adapter reports failures.
        if outcome.is_error {
            return Err(rpi_agent::error::AgentError::tool(
                outcome
                    .blocks
                    .iter()
                    .map(|block| match block {
                        kiqi_plugin::Block::Text(text) => text.clone(),
                        kiqi_plugin::Block::Json(value) => value.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ));
        }

        Ok(rpi_agent::types::AgentToolResult {
            content,
            details: outcome.details,
            ..Default::default()
        })
    }
}

/// Merge compiled-in plugin tools into the tool list.
///
/// Same contract as the extension side: an explicit `--tools`/`--exclude-tools`
/// decision applies, and a compiled plugin tool overrides a same-named built-in
/// exactly like a native one does, so the model cannot tell the two apart.
pub fn merge_compiled_plugin_tools(
    tools: &mut Vec<rpi_harness::types::HarnessTool>,
    hosts: &[std::sync::Arc<PluginHost>],
    caps: &OwnedCaps,
    allowed: &dyn Fn(&str) -> bool,
) {
    for host in hosts {
        for spec in host.plugin().tools() {
            if !allowed(&spec.name) {
                continue;
            }
            let Some(adapter) =
                CompiledToolAdapter::new(std::sync::Arc::clone(host), &spec, caps.clone())
            else {
                eprintln!(
                    "plugin `{}`: tool `{}` has an unparseable parameter schema; skipped",
                    host.id(),
                    spec.name
                );
                continue;
            };
            let name = spec.name.clone();
            let harness_tool = rpi_harness::types::HarnessTool::new(std::sync::Arc::new(adapter));
            match tools.iter_mut().find(|t| t.tool.schema().name == name) {
                Some(slot) => *slot = harness_tool,
                None => tools.push(harness_tool),
            }
        }
    }
}

/// The provider-facing schemas for every compiled-in plugin tool.
///
/// This is half of the wiring: it makes plugin tools visible to the model. The
/// other half - routing an actual call back into [`PluginHost::call_tool`] -
/// lives in the tool-execution dispatch.
pub fn compiled_tool_schemas(hosts: &[PluginHost]) -> Vec<rpi_ai::types::Tool> {
    let mut tools = Vec::new();
    for host in hosts {
        for spec in host.plugin().tools() {
            match serde_json::from_value::<rpi_ai::types::Schema>(spec.parameters.clone()) {
                Ok(parameters) => tools.push(rpi_ai::types::Tool {
                    name: spec.name,
                    description: spec.description,
                    parameters,
                    constrained_sampling: None,
                }),
                Err(error) => {
                    // Skipped rather than poisoning the whole list, but never
                    // silently: the plugin and tool name are named.
                    eprintln!(
                        "plugin `{}`: tool `{}` has an unparseable parameter schema ({error}); skipped",
                        host.id(),
                        spec.name
                    );
                }
            }
        }
    }
    tools
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
    pub fn on_event(
        &self,
        event: &kiqi_plugin::Event,
        caps: &HostCaps<'_>,
    ) -> kiqi_plugin::EventAction {
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

/// The process-wide plugin instances.
///
/// Created once: a plugin owns state (rpi-todo's task list lives in its
/// instance), so reloading it per tool-list build would silently reset that
/// state.
pub fn compiled_plugin_hosts() -> Vec<std::sync::Arc<PluginHost>> {
    static HOSTS: std::sync::OnceLock<Vec<std::sync::Arc<PluginHost>>> = std::sync::OnceLock::new();
    HOSTS
        .get_or_init(|| load_hosts().into_iter().map(std::sync::Arc::new).collect())
        .clone()
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

    /// The whole path: registry -> harness tool list -> adapter -> plugin call
    /// -> result. This is the one that proves a compiled-in plugin is usable,
    /// not merely loadable.
    #[tokio::test]
    async fn a_compiled_plugin_tool_runs_from_the_harness_tool_list() {
        use rpi_agent::agent_tool::AgentTool;

        let hosts = compiled_plugin_hosts();
        let caps = OwnedCaps {
            session: None,
            control: None,
            inject: None,
            ui: None,
            log: std::sync::Arc::new(StderrLog),
        };
        let mut tools: Vec<rpi_harness::types::HarnessTool> = Vec::new();
        merge_compiled_plugin_tools(&mut tools, &hosts, &caps, &|_| true);

        let tool = tools
            .iter()
            .find(|t| t.tool.schema().name == "todo")
            .expect("rpi-todo's tool reached the harness list");

        let result = tool
            .tool
            .execute(
                "call-1",
                serde_json::json!({"action": "add", "text": "from the harness"}),
                tokio_util::sync::CancellationToken::new(),
                std::sync::Arc::new(|_| {}),
            )
            .await
            .expect("the plugin tool executed");

        let text = result
            .content
            .iter()
            .map(|block| match block {
                rpi_agent::types::TextContentOrImage::Text(t) => t.text.clone(),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("from the harness"), "got {text}");
        // Order-independent on purpose: the plugin instance is process-wide
        // (its state is the point), so an earlier test may have added items.
        let added = result.details["todos"]
            .as_array()
            .expect("details carries the task list")
            .iter()
            .any(|todo| todo["text"] == "from the harness");
        assert!(
            added,
            "the added task is in the details: {}",
            result.details
        );
    }

    #[test]
    fn compiled_plugin_tools_reach_the_provider_facing_tool_list() {
        let hosts = load_hosts();
        let tools = compiled_tool_schemas(&hosts);
        let todo = tools
            .iter()
            .find(|tool| tool.name == "todo")
            .expect("rpi-todo's tool is in the model-visible list");
        assert!(!todo.description.is_empty());
        // The schema survived the Value -> Schema conversion intact.
        let rendered = serde_json::to_value(&todo.parameters).unwrap();
        let properties = rendered["properties"]
            .as_object()
            .expect("parameters is an object schema");
        assert!(properties.contains_key("action"));
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
    #[derive(Default)]
    struct FakeHost {
        seen: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
    }

    impl FakeHost {
        fn record(
            &self,
            action: &str,
            args: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            self.seen.lock().unwrap().push((action.to_string(), args));
            Ok(serde_json::json!({}))
        }
    }

    #[async_trait::async_trait]
    impl rpi_extensions::RuntimeActionHost for FakeHost {
        async fn send_message(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
            self.record("send_message", args)
        }
        async fn send_user_message(
            &self,
            args: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            self.record("send_user_message", args)
        }
        async fn append_entry(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
            self.record("append_entry", args)
        }
        async fn set_session_name(
            &self,
            args: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            self.record("set_session_name", args)
        }
        async fn get_active_tools(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({ "tools": ["todo", "bash"] }))
        }
        async fn set_active_tools(
            &self,
            args: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            self.record("set_active_tools", args)
        }
        async fn set_model(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
            self.record("set_model", args)
        }
        async fn get_thinking_level(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({ "level": "medium" }))
        }
        async fn set_thinking_level(
            &self,
            args: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            self.record("set_thinking_level", args)
        }
        async fn compact(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
            self.record("compact", args)
        }
        async fn get_system_prompt(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({ "prompt": "you are kiqi" }))
        }
        async fn new_session(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
            self.record("new_session", args)
        }
        async fn fork(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
            self.record("fork", args)
        }
        async fn navigate_tree(
            &self,
            args: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            self.record("navigate_tree", args)
        }
        async fn switch_session(
            &self,
            args: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            self.record("switch_session", args)
        }
        async fn reload(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
            self.record("reload", args)
        }
        async fn get_cli_flag(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
            let value = match args.get("name").and_then(|n| n.as_str()) {
                Some("verbose") => serde_json::json!("2"),
                _ => serde_json::Value::Null,
            };
            Ok(serde_json::json!({ "value": value }))
        }
        async fn get_session_branch(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
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
        let fake = std::sync::Arc::new(FakeHost::default());
        let caps_host = ActionCaps::new(fake.clone(), runtime.handle().clone());
        let silent = Silent;
        let caps = HostCaps {
            session: Some(&caps_host),
            control: None,
            inject: None,
            ui: None,
            log: &silent,
        };

        // The adapter really crosses the spawn/park seam.
        assert_eq!(
            caps_host.active_tools(),
            vec!["todo".to_string(), "bash".to_string()]
        );
        assert_eq!(caps_host.thinking_level(), "medium");
        assert_eq!(caps_host.system_prompt(), "you are kiqi");
        assert_eq!(caps_host.cli_flag("verbose").as_deref(), Some("2"));
        assert_eq!(caps_host.cli_flag("missing"), None);

        // The mutating capabilities must hand the host the documented args,
        // not a re-shaped blob. `kiqi_plugin::SessionControl` / `InjectApi` are
        // in scope for these calls.
        use kiqi_plugin::{InjectApi, SessionControl};
        caps_host.set_model("deepseek-flash").unwrap();
        caps_host.set_active_tools(&["todo".to_string()]).unwrap();
        caps_host.navigate_tree("abc").unwrap();
        caps_host.switch_session("sess-1").unwrap();
        caps_host.reload().unwrap();
        caps_host.send_user_message("hi").unwrap();
        caps_host
            .append_entry(&serde_json::json!({"kind": "note"}))
            .unwrap();
        let seen = fake.seen.lock().unwrap().clone();
        let sent = |action: &str| {
            seen.iter()
                .find(|(a, _)| a == action)
                .map(|(_, args)| args.clone())
                .unwrap_or_else(|| panic!("`{action}` was never dispatched"))
        };
        assert_eq!(
            sent("set_model"),
            serde_json::json!({"model": "deepseek-flash"})
        );
        assert_eq!(
            sent("set_active_tools"),
            serde_json::json!({"tools": ["todo"]})
        );
        assert_eq!(
            sent("navigate_tree"),
            serde_json::json!({"targetId": "abc"})
        );
        assert_eq!(sent("switch_session"), serde_json::json!({"id": "sess-1"}));
        assert_eq!(sent("reload"), serde_json::json!({}));
        assert_eq!(sent("send_user_message"), serde_json::json!({"text": "hi"}));
        assert_eq!(
            sent("append_entry"),
            serde_json::json!({"data": {"kind": "note"}})
        );

        // And a plugin restoring through caps gets the branch the host served,
        // not a stub: this is the rpi-todo restore path end to end.
        let host = PluginHost::new("rpi-todo", rpi_todo::kiqi::factory());
        host.on_event(&kiqi_plugin::Event::SessionStart, &caps);
        let listed = host
            .call_tool(
                "todo",
                &serde_json::json!({"action": "list"}),
                &caps,
                &NoProgress,
            )
            .unwrap();
        let text = match &listed.blocks[0] {
            kiqi_plugin::Block::Text(text) => text.clone(),
            other => panic!("unexpected block {other:?}"),
        };
        assert!(text.contains("from the branch"), "got {text}");
    }

    /// One worker thread on purpose: parking it without `block_in_place` would
    /// leave nobody to run the spawned action, so this test hands forever if
    /// the deadlock fix regresses.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn capabilities_do_not_deadlock_when_called_from_the_agent_loop() {
        let fake = std::sync::Arc::new(FakeHost::default());
        let caps = ActionCaps::new(fake, tokio::runtime::Handle::current());
        assert_eq!(
            caps.active_tools(),
            vec!["todo".to_string(), "bash".to_string()]
        );
        assert_eq!(caps.thinking_level(), "medium");
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
            .call_tool(
                "todo",
                &serde_json::json!({"action": "list"}),
                &caps,
                &NoProgress
            )
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
        // Order-independent: the plugin instance (and therefore its list) is
        // process-wide, so tests do not start from an empty list.
        let stored = added.details["todos"]
            .as_array()
            .expect("details carries the task list")
            .iter()
            .any(|todo| todo["text"] == "wire the registry");
        assert!(
            stored,
            "the added task is in the details: {}",
            added.details
        );

        // Unknown tools are a plugin error, not a panic or a crash.
        let err = todo
            .plugin
            .call("nope", &serde_json::json!({}), &caps, &NoProgress)
            .unwrap_err();
        assert!(matches!(err, kiqi_plugin::PluginError::UnknownTool(_)));
    }
}
