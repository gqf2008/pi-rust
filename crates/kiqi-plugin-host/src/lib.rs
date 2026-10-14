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
    Block, CommandSpec, Event, EventAction, FlagSpec, HostCaps, Plugin as PluginTrait, PluginError,
    Progress, ProgressSink, ShortcutSpec, ToolOutcome, ToolSpec, Verdict,
};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::kiqi::plugin::types::{
    ContentBlock, Denied, Event as WitEvent, EventAction as WitEventAction, LogLevel, OtherEvent,
    ProgressBlock, ToolCall, ToolResult, ToolResultEvent, Verdict as WitVerdict,
};
use crate::Plugin as World;

/// The generated world struct is named after the world (`plugin`); alias it so
/// it does not collide with the `kiqi_plugin::Plugin` trait.
const NOT_GRANTED: &str = "capability not granted";

/// What the call running on this thread may reach.
///
/// The host imports run on the thread that drives the guest call, and
/// [`WasmPlugin`] serializes calls behind a mutex, so one thread-local carries
/// the running call's capabilities and progress sink. The pointers only need to
/// live for the duration of that call.
#[derive(Clone, Copy)]
struct CallContext {
    sink: Option<*const (dyn ProgressSink + 'static)>,
    caps: *const HostCaps<'static>,
}

thread_local! {
    static CURRENT: RefCell<Option<CallContext>> = const { RefCell::new(None) };
}

/// Install the current call's context.
///
/// # Safety
/// The erased lifetimes are only valid while the borrows the pointers came from
/// are alive; [`clear_context`] must run before either ends.
fn install_context(sink: Option<&dyn ProgressSink>, caps: &HostCaps<'_>) -> CallContext {
    let context = CallContext {
        sink: sink.map(|sink| {
            // SAFETY: erased for the guest call's duration only.
            unsafe {
                std::mem::transmute::<*const dyn ProgressSink, *const (dyn ProgressSink + 'static)>(
                    sink,
                )
            }
        }),
        // SAFETY: as above.
        caps: unsafe { std::mem::transmute::<*const HostCaps<'_>, *const HostCaps<'static>>(caps) },
    };
    CURRENT.with(|slot| *slot.borrow_mut() = Some(context));
    context
}

fn clear_context() {
    CURRENT.with(|slot| *slot.borrow_mut() = None);
}

fn emit_progress(progress: Progress) {
    CURRENT.with(|slot| {
        if let Some(pointer) = slot.borrow().as_ref().and_then(|context| context.sink) {
            // SAFETY: `install_context` set this for the guest call; it is
            // cleared before the borrow ends.
            let sink = unsafe { &*pointer };
            sink.progress(progress);
        }
    });
}

/// Run `f` with the capabilities of the current call, if there is one.
fn with_caps_context<R>(f: impl FnOnce(&HostCaps<'_>) -> R) -> Option<R> {
    CURRENT.with(|slot| {
        let borrowed = slot.borrow();
        let context = (*borrowed)?;
        // SAFETY: `install_context` set this for the guest call.
        Some(f(unsafe { &*context.caps }))
    })
}

fn with_session<R>(f: impl FnOnce(&dyn kiqi_plugin::SessionRead) -> R) -> Option<R> {
    with_caps_context(|caps| caps.session.map(f)).flatten()
}

fn with_control<R>(f: impl FnOnce(&dyn kiqi_plugin::SessionControl) -> R) -> Option<R> {
    with_caps_context(|caps| caps.control.map(f)).flatten()
}

fn with_inject<R>(f: impl FnOnce(&dyn kiqi_plugin::InjectApi) -> R) -> Option<R> {
    with_caps_context(|caps| caps.inject.map(f)).flatten()
}

fn with_ui<R>(f: impl FnOnce(&dyn kiqi_plugin::UiApi) -> R) -> Option<R> {
    with_caps_context(|caps| caps.ui.map(f)).flatten()
}

/// Collapse a capability call into the ABI's `denied` on any failure.
fn forward(result: Option<Result<(), PluginError>>) -> Result<(), Denied> {
    match result {
        Some(Ok(())) => Ok(()),
        Some(Err(error)) => Err(Denied {
            reason: error.to_string(),
        }),
        None => Err(denied()),
    }
}

fn denied() -> Denied {
    Denied {
        reason: NOT_GRANTED.to_string(),
    }
}

/// What the user authorized for one plugin.
///
/// Loaded from `$KI_AGENT_DIR/plugins/<id>/grants.json` (default
/// `~/.ki/agent/plugins/<id>/grants.json`). A missing file grants nothing: the
/// host never authorizes on a plugin's behalf.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Grants {
    /// Host-mediated HTTP, with a host allow-list.
    #[serde(default)]
    pub net_http: Option<NetHttpGrant>,
    #[serde(default)]
    pub fs: bool,
    #[serde(default)]
    pub exec: bool,
    #[serde(default)]
    pub timer: bool,
    #[serde(default)]
    pub net_sockets: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct NetHttpGrant {
    /// Hosts the plugin may reach; `*` allows any.
    pub hosts: Vec<String>,
}

impl Grants {
    /// Read the user's grants for `plugin_id`; a missing file means nothing is
    /// granted.
    pub fn load_for(plugin_id: &str) -> Self {
        let Some(root) = agent_dir() else {
            return Self::default();
        };
        let path = root.join("plugins").join(plugin_id).join("grants.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    fn allows_host(&self, host: &str) -> bool {
        self.net_http.as_ref().is_some_and(|grant| {
            grant
                .hosts
                .iter()
                .any(|allowed| allowed == "*" || allowed == host)
        })
    }
}

fn agent_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("KI_AGENT_DIR") {
        return Some(std::path::PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".ki").join("agent"))
}

/// State every instantiated component gets: WASI plus the resource table and
/// the grants the guest's host imports are allowed to act on.
struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    grants: Grants,
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

/// Read-only session state, forwarded to the call's `HostCaps::session`.
impl crate::kiqi::plugin::session_read::Host for HostState {
    fn info(&mut self) -> crate::kiqi::plugin::types::SessionInfo {
        crate::kiqi::plugin::types::SessionInfo {
            session_id: String::new(),
            run_id: String::new(),
            cwd: std::env::current_dir()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }
    fn active_tools(&mut self) -> Vec<String> {
        with_session(|session| session.active_tools()).unwrap_or_default()
    }
    fn system_prompt(&mut self) -> String {
        with_session(|session| session.system_prompt()).unwrap_or_default()
    }
    fn thinking_level(&mut self) -> String {
        with_session(|session| session.thinking_level()).unwrap_or_default()
    }
    fn cli_flag(&mut self, name: String) -> Option<String> {
        with_session(|session| session.cli_flag(&name)).flatten()
    }
    fn session_branch(&mut self) -> String {
        with_session(|session| session.session_branch().to_string())
            .unwrap_or_else(|| "null".to_string())
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

/// Session mutations, forwarded to the call's `HostCaps::control`; a call
/// without that capability gets `denied`.
impl crate::kiqi::plugin::session_control::Host for HostState {
    fn set_session_name(&mut self, name: String) -> Result<(), Denied> {
        forward(with_control(|control| control.set_session_name(&name)))
    }
    fn set_active_tools(&mut self, tools: Vec<String>) -> Result<(), Denied> {
        forward(with_control(|control| control.set_active_tools(&tools)))
    }
    fn set_model(&mut self, model: String) -> Result<(), Denied> {
        forward(with_control(|control| control.set_model(&model)))
    }
    fn set_thinking_level(&mut self, level: String) -> Result<(), Denied> {
        forward(with_control(|control| control.set_thinking_level(&level)))
    }
    fn compact(&mut self) -> Result<(), Denied> {
        forward(with_control(|control| control.compact()))
    }
    fn new_session(&mut self) -> Result<(), Denied> {
        forward(with_control(|control| control.new_session()))
    }
    fn fork(&mut self) -> Result<(), Denied> {
        forward(with_control(|control| control.fork()))
    }
    fn navigate_tree(&mut self, target: String) -> Result<(), Denied> {
        forward(with_control(|control| control.navigate_tree(&target)))
    }
    fn switch_session(&mut self, id: String) -> Result<(), Denied> {
        forward(with_control(|control| control.switch_session(&id)))
    }
    fn reload(&mut self) -> Result<(), Denied> {
        forward(with_control(|control| control.reload()))
    }
}

/// Content injection, forwarded to the call's `HostCaps::inject`.
impl crate::kiqi::plugin::inject::Host for HostState {
    fn send_message(&mut self, message: String) -> Result<(), Denied> {
        let message = serde_json::from_str(&message).unwrap_or(serde_json::Value::Null);
        forward(with_inject(|inject| inject.send_message(&message)))
    }
    fn send_user_message(&mut self, text: String) -> Result<(), Denied> {
        forward(with_inject(|inject| inject.send_user_message(&text)))
    }
    fn append_entry(&mut self, entry: String) -> Result<(), Denied> {
        let entry = serde_json::from_str(&entry).unwrap_or(serde_json::Value::Null);
        forward(with_inject(|inject| inject.append_message(&entry)))
    }
}

/// UI, forwarded to the call's `HostCaps::ui`.
impl crate::kiqi::plugin::ui::Host for HostState {
    fn dialog(&mut self, request: String) -> Result<String, Denied> {
        let request = serde_json::from_str(&request).unwrap_or(serde_json::Value::Null);
        match with_ui(|ui| ui.dialog(&request)) {
            Some(Ok(value)) => Ok(value.to_string()),
            Some(Err(error)) => Err(Denied {
                reason: error.to_string(),
            }),
            None => Err(denied()),
        }
    }
    fn set_status(&mut self, key: String, value: String) -> Result<(), Denied> {
        forward(with_ui(|ui| {
            ui.set_status(&serde_json::json!({"key": key, "value": value}))
        }))
    }
    fn set_editor_text(&mut self, text: String) -> Result<(), Denied> {
        forward(with_ui(|ui| {
            ui.set_editor_text(&serde_json::json!({"text": text}))
        }))
    }
}

/// Host-mediated HTTP: the request's host must be in the grant's allow-list.
impl crate::kiqi::plugin::net_http::Host for HostState {
    fn fetch(&mut self, request: String) -> Result<String, Denied> {
        let request: serde_json::Value =
            serde_json::from_str(&request).map_err(|error| Denied {
                reason: format!("invalid net-http request: {error}"),
            })?;
        let url = request
            .get("url")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Denied {
                reason: "net-http request.url is required".to_string(),
            })?;
        let parsed = reqwest::Url::parse(url).map_err(|error| Denied {
            reason: format!("invalid url `{url}`: {error}"),
        })?;
        let host = parsed.host_str().unwrap_or_default().to_string();
        if !self.grants.allows_host(&host) {
            return Err(Denied {
                reason: format!("net-http to `{host}` is not granted"),
            });
        }

        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|error| Denied {
                reason: error.to_string(),
            })?;
        let method = request
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("GET");
        let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|error| Denied {
            reason: error.to_string(),
        })?;
        let mut builder = client.request(method, parsed);
        if let Some(headers) = request
            .get("headers")
            .and_then(serde_json::Value::as_object)
        {
            for (name, value) in headers {
                if let Some(value) = value.as_str() {
                    builder = builder.header(name, value);
                }
            }
        }
        if let Some(body) = request.get("body").and_then(serde_json::Value::as_str) {
            builder = builder.body(body.to_string());
        }

        let response = builder.send().map_err(|error| Denied {
            reason: error.to_string(),
        })?;
        let status = response.status().as_u16();
        let headers: serde_json::Map<String, serde_json::Value> = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.to_string(),
                    serde_json::Value::String(value.to_str().unwrap_or_default().to_string()),
                )
            })
            .collect();
        let body = response.text().unwrap_or_default();
        Ok(serde_json::json!({"status": status, "headers": headers, "body": body}).to_string())
    }
}

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

/// Load-time knobs.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Bound the guest's execution. With a budget, a plugin that spins traps
    /// instead of hanging the host; `None` leaves it unbounded.
    pub fuel: Option<u64>,
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
    /// Load and instantiate a component with the grants the user recorded for
    /// this plugin id (a missing `grants.json` grants nothing). `init` is called
    /// before returning.
    pub fn load(id: impl Into<String>, wasm: &Path) -> Result<Self, PluginError> {
        let id = id.into();
        let grants = Grants::load_for(&id);
        Self::load_with_grants(id, wasm, grants)
    }

    /// Load and instantiate with explicit grants (used by callers that manage
    /// authorization themselves, and by tests).
    pub fn load_with_grants(
        id: impl Into<String>,
        wasm: &Path,
        grants: Grants,
    ) -> Result<Self, PluginError> {
        Self::load_with_options(id, wasm, grants, Options::default())
    }

    /// Load with explicit grants and load-time knobs.
    pub fn load_with_options(
        id: impl Into<String>,
        wasm: &Path,
        grants: Grants,
        options: Options,
    ) -> Result<Self, PluginError> {
        let id = id.into();
        let mut config = wasmtime::Config::new();
        if options.fuel.is_some() {
            // Metering is off unless a bound is asked for, so ordinary plugins
            // do not pay for it.
            config.consume_fuel(true);
        }
        let engine = Engine::new(&config)
            .map_err(|error| PluginError::Failed(format!("wasmtime engine: {error}")))?;
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
                grants,
            },
        );
        if let Some(fuel) = options.fuel {
            store.set_fuel(fuel).map_err(|error| {
                PluginError::Failed(format!("set fuel budget for {id}: {error}"))
            })?;
        }
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

    fn commands(&self) -> Vec<CommandSpec> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return Vec::new();
        };
        let guest = inner.guest.kiqi_plugin_plugin_api();
        match guest.call_list_commands(&mut inner.store) {
            Ok(specs) => specs
                .into_iter()
                .map(|spec| CommandSpec {
                    name: spec.name,
                    description: spec.description,
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn run_command(
        &self,
        name: &str,
        arguments: &str,
        caps: &HostCaps<'_>,
    ) -> Result<String, PluginError> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return Err(PluginError::Failed(format!("{} is unloaded", self.id)));
        };
        let _context = install_context(None, caps);
        let result = inner.guest.kiqi_plugin_plugin_api().call_run_command(
            &mut inner.store,
            name,
            arguments,
        );
        clear_context();
        match result {
            Ok(Ok(text)) => Ok(text),
            Ok(Err(error)) => Err(map_plugin_error(error)),
            Err(trap) => Err(PluginError::Failed(format!(
                "{}: command `{name}` trapped: {trap}",
                self.id
            ))),
        }
    }

    fn flags(&self) -> Vec<FlagSpec> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return Vec::new();
        };
        let guest = inner.guest.kiqi_plugin_plugin_api();
        match guest.call_list_flags(&mut inner.store) {
            Ok(specs) => specs
                .into_iter()
                .map(|spec| FlagSpec {
                    name: spec.name,
                    description: spec.description,
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn shortcuts(&self) -> Vec<ShortcutSpec> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return Vec::new();
        };
        let guest = inner.guest.kiqi_plugin_plugin_api();
        match guest.call_list_shortcuts(&mut inner.store) {
            Ok(specs) => specs
                .into_iter()
                .map(|spec| ShortcutSpec {
                    key: spec.key,
                    description: spec.description,
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn before_agent_start(
        &self,
        prompt: &serde_json::Value,
        caps: &HostCaps<'_>,
    ) -> Option<String> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let inner = guard.as_mut()?;
        let _context = install_context(None, caps);
        let result = inner
            .guest
            .kiqi_plugin_plugin_api()
            .call_before_agent_start(&mut inner.store, &prompt.to_string());
        clear_context();
        result.ok().flatten()
    }

    fn probe_points(&self) -> Vec<String> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return Vec::new();
        };
        let guest = inner.guest.kiqi_plugin_plugin_api();
        guest
            .call_list_probe_points(&mut inner.store)
            .unwrap_or_default()
    }

    fn probe(&self, point: &str, payload: &serde_json::Value, caps: &HostCaps<'_>) -> Verdict {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return Verdict::Continue;
        };
        let _context = install_context(None, caps);
        let result = inner.guest.kiqi_plugin_plugin_api().call_probe(
            &mut inner.store,
            point,
            &payload.to_string(),
        );
        clear_context();
        match result {
            Ok(WitVerdict::Continue) => Verdict::Continue,
            Ok(WitVerdict::Replace(json)) => {
                Verdict::Replace(serde_json::from_str(&json).unwrap_or(serde_json::Value::Null))
            }
            Ok(WitVerdict::Block(reason)) => Verdict::Block { reason },
            Err(trap) => {
                eprintln!("[kiqi-plugin] {}: probe trapped: {trap}", self.id);
                Verdict::Continue
            }
        }
    }

    fn call(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        caps: &HostCaps<'_>,
        sink: &dyn ProgressSink,
    ) -> Result<ToolOutcome, PluginError> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return Err(PluginError::Failed(format!("{} is unloaded", self.id)));
        };

        let arguments = serde_json::to_string(arguments).unwrap_or_else(|_| "null".to_string());
        let _context = install_context(Some(sink), caps);
        let result =
            inner
                .guest
                .kiqi_plugin_plugin_api()
                .call_call_tool(&mut inner.store, name, &arguments);
        clear_context();

        match result {
            Ok(Ok(tool_result)) => Ok(to_tool_outcome(tool_result)),
            Ok(Err(error)) => Err(map_plugin_error(error)),
            Err(trap) => Err(PluginError::Failed(format!(
                "{}: tool `{name}` trapped: {trap}",
                self.id
            ))),
        }
    }

    fn on_event(&self, event: &Event, caps: &HostCaps<'_>) -> EventAction {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return EventAction::Continue;
        };
        let wit = to_wit_event(event);
        let _context = install_context(None, caps);
        let result = inner
            .guest
            .kiqi_plugin_plugin_api()
            .call_on_event(&mut inner.store, &wit);
        clear_context();
        match result {
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
