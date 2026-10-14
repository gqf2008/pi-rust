//! Host-side backend for **wasm plugins** (the component model tier).
//!
//! Binds `plugins/kiqi-plugin.wit` (ABI 0.2.0) with wasmtime's `bindgen!` and
//! hosts a plugin component behind the same [`kiqi_plugin::Plugin`] seam the
//! compiled-in plugins implement.
//!
//! Two deliberate choices:
//!
//! * **The seam stays sync.** WASI 0.2 semantics are synchronous (a guest blocks
//!   on `wasi:io/poll`), so the generated guest calls are sync and no async
//!   runtime is needed. When a host import eventually needs real async I/O, it
//!   blocks inside its own implementation — the async stays here, not in the
//!   plugin trait.
//! * **The host provides every interface.** The component model requires each
//!   import a component declares to be satisfied at instantiation, so the host
//!   implements all of them: `log` writes, `progress` forwards to the running
//!   call's sink, `session-read` returns placeholders, and every capability that
//!   is not granted is a **deny stub** returning `denied`. A component only
//!   imports the interfaces it uses, so a plugin that never touches `exec` never
//!   asks for it.
//!
//! `WasmPlugin::unload` (and `Drop`, and [`kiqi_plugin::Plugin::shutdown`])
//! releases the instance so `/reload` can swap a plugin without leaking linear
//! memory.

wasmtime::component::bindgen!({
    path: "../../plugins/kiqi-plugin.wit",
    world: "plugin",
});

use std::cell::RefCell;
use std::path::Path;
use std::sync::Mutex;

use kiqi_plugin::{
    Block, Event, EventAction, HostCaps, Plugin as PluginTrait, PluginError, Progress,
    ProgressSink, ToolOutcome, ToolSpec,
};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::kiqi::plugin::types::{
    ContentBlock, Denied, Event as WitEvent, EventAction as WitEventAction, LogLevel, OtherEvent,
    ProgressBlock, ToolCall, ToolResult, ToolResultEvent,
};
use crate::Plugin as World;

/// The generated world struct is named after the world (`plugin`); alias it so
/// it does not collide with the `kiqi_plugin::Plugin` trait.
const NOT_GRANTED: &str = "capability not granted";

thread_local! {
    /// Sink for the tool call currently running on this thread.
    ///
    /// The host imports run on the thread that drives the guest call, and
    /// [`WasmPlugin`] serializes calls behind a mutex, so a thread-local is
    /// enough to hand the borrowed `&dyn ProgressSink` to `progress.emit`. It
    /// also keeps that non-`Send` borrow out of the store data.
    static CURRENT_SINK: RefCell<Option<*const (dyn ProgressSink + 'static)>> =
        const { RefCell::new(None) };
}

/// Erase the borrow's lifetime for a raw-pointer hand-off.
///
/// # Safety
/// The returned pointer is only valid while the borrow it came from is alive;
/// callers must clear the slot before then.
unsafe fn erase_sink_lifetime(sink: &dyn ProgressSink) -> *const (dyn ProgressSink + 'static) {
    std::mem::transmute::<*const dyn ProgressSink, *const (dyn ProgressSink + 'static)>(sink)
}

fn emit_progress(progress: Progress) {
    CURRENT_SINK.with(|slot| {
        if let Some(pointer) = *slot.borrow() {
            // SAFETY: `WasmPlugin::call` installs this pointer for exactly the
            // duration of the guest call and clears it afterwards; the borrow it
            // came from outlives that call, and calls are serialized.
            let sink = unsafe { &*pointer };
            sink.progress(progress);
        }
    });
}

fn denied() -> Denied {
    Denied {
        reason: NOT_GRANTED.to_string(),
    }
}

/// State every instantiated component gets: WASI plus the resource table.
struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

// ---------------------------------------------------------------------------
// Host import implementations
// ---------------------------------------------------------------------------

/// `types` exports no functions, but the world-level linker still requires its
/// (empty) host trait to be implemented.
impl crate::kiqi::plugin::types::Host for HostState {}

impl crate::kiqi::plugin::log::Host for HostState {
    fn write(&mut self, _level: LogLevel, message: String) {
        eprintln!("[kiqi-plugin] {message}");
    }
}

/// Read-only session state is low risk, so it is granted with placeholder
/// values for now. Wiring it to the real session is a later increment.
impl crate::kiqi::plugin::session_read::Host for HostState {
    fn info(&mut self) -> crate::kiqi::plugin::types::SessionInfo {
        crate::kiqi::plugin::types::SessionInfo {
            session_id: String::new(),
            run_id: String::new(),
            cwd: String::new(),
        }
    }
    fn active_tools(&mut self) -> Vec<String> {
        Vec::new()
    }
    fn system_prompt(&mut self) -> String {
        String::new()
    }
    fn thinking_level(&mut self) -> String {
        String::new()
    }
    fn cli_flag(&mut self, _name: String) -> Option<String> {
        None
    }
    fn session_branch(&mut self) -> String {
        String::new()
    }
}

impl crate::kiqi::plugin::progress::Host for HostState {
    fn emit(&mut self, block: ProgressBlock) {
        emit_progress(match block {
            ProgressBlock::Text(text) => Progress::Text(text),
            ProgressBlock::Json(json) => Progress::Json(
                serde_json::from_str(&json).unwrap_or(serde_json::Value::String(json)),
            ),
        });
    }
}

macro_rules! deny_capability {
    ($iface:ident { $($method:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)* }) => {
        impl crate::kiqi::plugin::$iface::Host for HostState {
            $(
                fn $method(&mut self, $($arg: $ty),*) -> $ret {
                    $(let _ = $arg;)*
                    Err(denied())
                }
            )*
        }
    };
}

deny_capability!(session_control {
    set_session_name(name: String) -> Result<(), Denied>;
    set_active_tools(tools: Vec<String>) -> Result<(), Denied>;
    set_model(model: String) -> Result<(), Denied>;
    set_thinking_level(level: String) -> Result<(), Denied>;
    compact() -> Result<(), Denied>;
    new_session() -> Result<(), Denied>;
    fork() -> Result<(), Denied>;
    navigate_tree(target: String) -> Result<(), Denied>;
    switch_session(id: String) -> Result<(), Denied>;
    reload() -> Result<(), Denied>;
});

deny_capability!(inject {
    send_message(message: String) -> Result<(), Denied>;
    send_user_message(text: String) -> Result<(), Denied>;
    append_entry(entry: String) -> Result<(), Denied>;
});

impl crate::kiqi::plugin::ui::Host for HostState {
    fn dialog(&mut self, _request: String) -> Result<String, Denied> {
        Err(denied())
    }
    fn set_status(&mut self, _key: String, _value: String) -> Result<(), Denied> {
        Err(denied())
    }
    fn set_editor_text(&mut self, _text: String) -> Result<(), Denied> {
        Err(denied())
    }
}

deny_capability!(net_http {
    fetch(request: String) -> Result<String, Denied>;
});

deny_capability!(fs {
    read(rel_path: String) -> Result<Vec<u8>, Denied>;
    write(rel_path: String, contents: Vec<u8>) -> Result<(), Denied>;
    read_dir(rel_dir: String) -> Result<Vec<String>, Denied>;
});

deny_capability!(net_sockets {
    connect(address: String) -> Result<u32, Denied>;
    send(handle: u32, bytes: Vec<u8>) -> Result<u64, Denied>;
    recv(handle: u32, max_bytes: u64) -> Result<Vec<u8>, Denied>;
    close(handle: u32) -> Result<(), Denied>;
});

deny_capability!(exec {
    run(cmd: String, args: Vec<String>, cwd: Option<String>) -> Result<String, Denied>;
});

deny_capability!(timer {
    request_tick(every_ms: u32) -> Result<(), Denied>;
});

// ---------------------------------------------------------------------------
// WasmPlugin
// ---------------------------------------------------------------------------

struct Inner {
    store: Store<HostState>,
    guest: World,
}

/// A plugin loaded from a wasm component.
pub struct WasmPlugin {
    id: String,
    /// Kept so a reload can re-instantiate without recompiling the wasm.
    #[allow(dead_code)]
    engine: Engine,
    /// Kept so `/reload` can re-instantiate from the already-compiled component.
    #[allow(dead_code)]
    component: Component,
    inner: Mutex<Option<Inner>>,
}

impl WasmPlugin {
    /// Load and instantiate a component. `init` is called before returning.
    pub fn load(id: impl Into<String>, wasm: &Path) -> Result<Self, PluginError> {
        let id = id.into();
        let engine = Engine::default();
        let component = Component::from_file(&engine, wasm)
            .map_err(|error| PluginError::Failed(format!("load {}: {error}", wasm.display())))?;

        let mut linker = Linker::new(&engine);
        World::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |state| state)
            .map_err(|error| PluginError::Failed(format!("link plugin interfaces: {error}")))?;
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|error| PluginError::Failed(format!("link wasi: {error}")))?;

        let mut store = Store::new(
            &engine,
            HostState {
                wasi: WasiCtxBuilder::new().build(),
                table: ResourceTable::new(),
            },
        );
        let guest = World::instantiate(&mut store, &component, &linker)
            .map_err(|error| PluginError::Failed(format!("instantiate {id}: {error}")))?;
        guest
            .kiqi_plugin_plugin_api()
            .call_init(&mut store)
            .map_err(|error| PluginError::Failed(format!("{id}.init: {error}")))?;

        Ok(Self {
            id,
            engine,
            component,
            inner: Mutex::new(Some(Inner { store, guest })),
        })
    }

    /// Call the guest `shutdown` and release the instance.
    ///
    /// This is the counterpart of [`WasmPlugin::load`]: after it returns, the
    /// component's linear memory and resources are gone. `/reload` relies on it,
    /// and [`Drop`] calls it so a dropped plugin cannot leak an instance.
    pub fn unload(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(mut inner) = guard.take() {
            // Best effort: a broken guest must not keep the host from releasing
            // the instance.
            let _ = inner
                .guest
                .kiqi_plugin_plugin_api()
                .call_shutdown(&mut inner.store);
            // `inner` drops here, releasing the store (and its memory).
        }
    }
}

impl Drop for WasmPlugin {
    fn drop(&mut self) {
        self.unload();
    }
}

impl PluginTrait for WasmPlugin {
    fn id(&self) -> &str {
        &self.id
    }

    fn tools(&self) -> Vec<ToolSpec> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return Vec::new();
        };
        let guest = inner.guest.kiqi_plugin_plugin_api();
        match guest.call_list_tools(&mut inner.store) {
            Ok(specs) => specs
                .into_iter()
                .map(|spec| ToolSpec {
                    name: spec.name,
                    description: spec.description,
                    parameters: serde_json::from_str(&spec.parameters_schema)
                        .unwrap_or(serde_json::Value::Null),
                })
                .collect(),
            Err(error) => {
                eprintln!("[kiqi-plugin] {}: list-tools trapped: {error}", self.id);
                Vec::new()
            }
        }
    }

    fn call(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        _caps: &HostCaps<'_>,
        sink: &dyn ProgressSink,
    ) -> Result<ToolOutcome, PluginError> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return Err(PluginError::Failed(format!("{} is unloaded", self.id)));
        };

        let arguments = serde_json::to_string(arguments).unwrap_or_else(|_| "null".to_string());
        // SAFETY: the slot is cleared right after the guest call returns, so the
        // pointer never outlives `sink`.
        let pointer = unsafe { erase_sink_lifetime(sink) };
        CURRENT_SINK.with(|slot| *slot.borrow_mut() = Some(pointer));
        let result =
            inner
                .guest
                .kiqi_plugin_plugin_api()
                .call_call_tool(&mut inner.store, name, &arguments);
        CURRENT_SINK.with(|slot| *slot.borrow_mut() = None);

        match result {
            Ok(Ok(tool_result)) => Ok(to_tool_outcome(tool_result)),
            Ok(Err(error)) => Err(map_plugin_error(error)),
            Err(trap) => Err(PluginError::Failed(format!(
                "{}: tool `{name}` trapped: {trap}",
                self.id
            ))),
        }
    }

    fn on_event(&self, event: &Event, _caps: &HostCaps<'_>) -> EventAction {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return EventAction::Continue;
        };
        let wit = to_wit_event(event);
        match inner
            .guest
            .kiqi_plugin_plugin_api()
            .call_on_event(&mut inner.store, &wit)
        {
            Ok(WitEventAction::Continue) => EventAction::Continue,
            Ok(WitEventAction::Claimed) => EventAction::Claimed,
            Ok(WitEventAction::Replace(json)) => {
                EventAction::Replace(serde_json::from_str(&json).unwrap_or(serde_json::Value::Null))
            }
            Err(trap) => {
                eprintln!("[kiqi-plugin] {}: on-event trapped: {trap}", self.id);
                EventAction::Continue
            }
        }
    }

    fn shutdown(&self) {
        self.unload();
    }
}

// ---------------------------------------------------------------------------
// Mapping between the host's types and the ABI's
// ---------------------------------------------------------------------------

fn map_plugin_error(error: crate::kiqi::plugin::types::PluginError) -> PluginError {
    use crate::kiqi::plugin::types::PluginError as Wit;
    match error {
        Wit::UnknownTool(name) => PluginError::UnknownTool(name),
        Wit::InvalidArguments(detail) => PluginError::InvalidArguments(detail),
        Wit::Denied(detail) => PluginError::Denied(detail),
        Wit::Failed(detail) => PluginError::Failed(detail),
    }
}

fn to_tool_outcome(tool_result: ToolResult) -> ToolOutcome {
    ToolOutcome {
        blocks: tool_result
            .blocks
            .into_iter()
            .map(|block| match block {
                ContentBlock::Text(text) => Block::Text(text),
                ContentBlock::Json(json) => Block::Json(
                    serde_json::from_str(&json).unwrap_or(serde_json::Value::String(json)),
                ),
            })
            .collect(),
        is_error: tool_result.is_error,
        details: serde_json::from_str(&tool_result.details).unwrap_or(serde_json::Value::Null),
    }
}

fn payload_string(event: &Event) -> String {
    event.payload.to_string()
}

fn to_wit_tool_call(event: &Event) -> ToolCall {
    let get = |keys: &[&str]| keys.iter().find_map(|key| event.payload.get(*key));
    ToolCall {
        tool: get(&["toolName", "tool_name"])
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        arguments: get(&["params", "args"])
            .map(|value| value.to_string())
            .unwrap_or_else(|| "null".to_string()),
    }
}

fn to_wit_event(event: &Event) -> WitEvent {
    use kiqi_plugin::EventTag as T;
    match event.tag {
        T::SessionStart => WitEvent::SessionStart,
        T::SessionShutdown => WitEvent::SessionShutdown,
        T::SessionTree => WitEvent::SessionTree,
        T::AgentSettled => WitEvent::AgentSettled,
        T::BeforeAgentStart => WitEvent::BeforeAgentStart(payload_string(event)),
        T::BeforeProviderRequest => WitEvent::BeforeProviderRequest(payload_string(event)),
        T::MessageUpdate => WitEvent::MessageUpdate(payload_string(event)),
        T::MessageEnd => WitEvent::MessageEnd(payload_string(event)),
        T::ToolExecutionStart => WitEvent::ToolExecutionStart(to_wit_tool_call(event)),
        T::ToolExecutionEnd => WitEvent::ToolExecutionEnd(ToolResultEvent {
            call: to_wit_tool_call(event),
            output: event
                .payload
                .get("result")
                .map(|value| value.to_string())
                .unwrap_or_default(),
            is_error: event
                .payload
                .get("isError")
                .or_else(|| event.payload.get("is_error"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        }),
        other => WitEvent::Other(OtherEvent {
            kind: format!("{other:?}"),
            payload: payload_string(event),
        }),
    }
}
