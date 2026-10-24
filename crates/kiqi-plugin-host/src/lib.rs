//! Host-side backend for **wasm plugins** (the component model tier).
//!
//! Binds `plugins/kiqi-plugin.wit` (ABI 0.8.0) with wasmtime's `bindgen!` and
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

use kiqi_plugin::grants::{HeadersMode, MessagesMode, ObserveMode};
use kiqi_plugin::{
    Block, CommandSpec, Event, EventAction, FlagSpec, HostCaps, Plugin as PluginTrait, PluginError,
    Progress, ProgressSink, ShortcutSpec, ToolOutcome, ToolSpec, Verdict,
};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::kiqi::plugin::types::{
    CapabilityError, ContentBlock, Event as WitEvent, EventAction as WitEventAction, LogLevel,
    OtherEvent, ProgressBlock, ToolCall, ToolResult, ToolResultEvent, Verdict as WitVerdict,
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

/// Collapse a capability call into a `capability-error` on any failure.
fn forward(result: Option<Result<(), PluginError>>) -> Result<(), CapabilityError> {
    match result {
        Some(Ok(())) => Ok(()),
        Some(Err(error)) => Err(capability_error(error)),
        None => Err(denied()),
    }
}

/// A granted capability whose *operation* failed is `failed`, not `denied`.
fn capability_error(error: PluginError) -> CapabilityError {
    match error {
        PluginError::Denied(reason) => CapabilityError::Denied(reason),
        other => CapabilityError::Failed(other.to_string()),
    }
}

fn denied() -> CapabilityError {
    CapabilityError::Denied(NOT_GRANTED.to_string())
}

/// What the user authorized for one plugin, shared with the CLI through the
/// contract crate (`kiqi-plugin`).
pub use kiqi_plugin::grants::{
    allows, ExecCwd, ExecGrant, FsGrant, Grants, NetHttpGrant, NetSocketsGrant, ProcGrant,
};

/// The user-grantable capabilities a component declares: the `kiqi:plugin/*`
/// interfaces that need authorization.
///
/// Imports and exports both count. An import names a channel the guest reaches
/// for (`net-http`, `fs`, `exec`, `proc`, `net-sockets`, `provider-http`,
/// `oauth-http`, `provider-request`); an export names a surface it offers the
/// host (`oauth-api`, `provider-api`, `render-api`).
/// [`kiqi_plugin::grants::grant_key_for_interface`] turns either spelling into
/// its grant key, so the scanner and the grants model can never disagree
/// (`provider-http`/`provider-stream` → `provider`, `oauth-http` → `oauth`, …).
///
/// The domain interfaces (`log`, `session-read`, …) are always available through
/// `HostCaps` and are not part of the grant set, so they are filtered out. This
/// is what `ki plugin install` shows the user and re-prompts about when it grows.
/// `resources` has no interface at all (it is a package convention,
/// `resources.json`), so it is grantable but never declared here.
pub fn declared_capabilities(wasm: &Path) -> Result<Vec<String>, String> {
    let bytes = std::fs::read(wasm).map_err(|error| format!("read {}: {error}", wasm.display()))?;
    let mut caps: Vec<String> = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(&bytes) {
        match payload.map_err(|error| error.to_string())? {
            wasmparser::Payload::ComponentImportSection(reader) => {
                for import in reader {
                    let import = import.map_err(|error| error.to_string())?;
                    note_declared(import.name.name, &mut caps);
                }
            }
            wasmparser::Payload::ComponentExportSection(reader) => {
                for export in reader {
                    let export = export.map_err(|error| error.to_string())?;
                    note_declared(export.name.name, &mut caps);
                }
            }
            _ => {}
        }
    }
    caps.sort();
    Ok(caps)
}

/// The grant keys the host can honour, in one place.
const GRANTABLE: &[&str] = &[
    "net-http",
    "fs",
    "exec",
    "proc",
    "net-sockets",
    "oauth",
    "provider",
    "render",
    "resources",
    "provider-request",
];

/// Record the grant key `name` declares, if it is grantable.
///
/// The interface→grant-key map handles the 0.8.0 names; the fallback keeps the
/// capability-as-interface convention (`kiqi:plugin/exec@0.7.1` → `exec`), which
/// is what every 0.7.1 component imports.
fn note_declared(name: &str, caps: &mut Vec<String>) {
    let key = kiqi_plugin::grants::grant_key_for_interface(name).or_else(|| {
        let bare = name.strip_prefix("kiqi:plugin/").unwrap_or(name);
        let bare = bare.split('@').next().unwrap_or(bare);
        GRANTABLE.iter().copied().find(|cap| *cap == bare)
    });
    let Some(key) = key else { return };
    if GRANTABLE.contains(&key) && !caps.iter().any(|seen| seen == key) {
        caps.push(key.to_string());
    }
}

/// The export interfaces a component offers (`oauth-api`, `provider-api`,
/// `render-api`), without package/version decoration.
///
/// [`declared_capabilities`] collapses these into grant keys; discovery needs
/// the interface itself, because it probes only what the guest actually
/// exports. A guest that merely imports `provider-http` (and does not implement
/// `provider-api`) is therefore never probed.
pub fn exported_interfaces(wasm: &Path) -> Result<Vec<String>, String> {
    let bytes = std::fs::read(wasm).map_err(|error| format!("read {}: {error}", wasm.display()))?;
    let mut out: Vec<String> = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(&bytes) {
        if let wasmparser::Payload::ComponentExportSection(reader) =
            payload.map_err(|error| error.to_string())?
        {
            for export in reader {
                let export = export.map_err(|error| error.to_string())?;
                let bare = export
                    .name
                    .name
                    .strip_prefix("kiqi:plugin/")
                    .unwrap_or(export.name.name);
                let bare = bare.split('@').next().unwrap_or(bare);
                if matches!(bare, "oauth-api" | "provider-api" | "render-api")
                    && !out.iter().any(|seen| seen == bare)
                {
                    out.push(bare.to_string());
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The capabilities this build of the host actually enforces.
///
/// [`declared_capabilities`] reports what a *component* asks for; this reports
/// what the host can honour. `ki plugin install` names any difference, so a user
/// is never asked to grant something that would only ever come back denied.
/// All ten grantable capabilities are enforced: `net-http`, `fs`, `exec`,
/// `proc`, `net-sockets`, `oauth`, `provider`, `render`, `resources` and
/// `provider-request`.
pub const ENFORCED_CAPABILITIES: &[&str] = &[
    "net-http",
    "fs",
    "exec",
    "proc",
    "net-sockets",
    "oauth",
    "provider",
    "render",
    "resources",
    "provider-request",
];

/// See [`ENFORCED_CAPABILITIES`].
pub fn capability_is_enforced(name: &str) -> bool {
    ENFORCED_CAPABILITIES.contains(&name)
}

/// Fuel granted to one install-time discovery instance. A guest that spins
/// traps instead of hanging `ki plugin install`.
pub const DISCOVERY_FUEL: u64 = 200_000_000;

/// Linear-memory cap for discovery: enough for a metadata call, far below the
/// normal guest budget.
pub const DISCOVERY_MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;

/// Wall-clock deadline for one discovery run.
pub const DISCOVERY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// How long the install thread waits for the discovery thread. It is longer
/// than [`DISCOVERY_DEADLINE`] so epoch/fuel get the first chance to fire.
pub const DISCOVERY_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// At most this many providers are accepted from one `list-providers` call.
pub const DISCOVERY_MAX_ENTRIES: usize = 16;

/// At most this many bytes of ids/display names are accepted in total.
pub const DISCOVERY_MAX_BYTES: usize = 4 * 1024;

/// At most this many bytes for one id or display name.
pub const DISCOVERY_MAX_STRING_BYTES: usize = 256;

/// One provider a component disclosed at install time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredProvider {
    /// The export interface that answered (`oauth-api` / `provider-api`).
    pub interface: String,
    /// The provider id the guest reported.
    pub id: String,
    pub display_name: String,
}

/// Result of the bounded install-time `list-providers` discovery.
///
/// `diagnostics` non-empty means discovery failed closed: nothing was learned
/// and installation continues, because the GRANT — not the guest — is the
/// catalog authority. Discovery can only narrow what a grant already allows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovery {
    pub providers: Vec<DiscoveredProvider>,
    pub diagnostics: Vec<String>,
}

/// Ask a freshly installed component which providers it can drive.
///
/// This runs before any grant exists (`Grants::default()`), on a **separate**,
/// bounded engine: fuel [`DISCOVERY_FUEL`], a [`DISCOVERY_MAX_MEMORY_BYTES`]
/// memory cap, epoch interruption with a [`DISCOVERY_DEADLINE`] deadline, and a
/// [`DISCOVERY_WAIT`] bound on the install thread. Only components exporting
/// `oauth-api`/`provider-api` are probed, and only `list-providers` is called —
/// never a request. Any trap, timeout, missing import or over-cap answer is a
/// diagnostic and installation continues (fail closed).
pub fn discover_providers(wasm: &Path) -> Discovery {
    let interfaces: Vec<String> = match exported_interfaces(wasm) {
        Ok(list) => list
            .into_iter()
            .filter(|name| name == "oauth-api" || name == "provider-api")
            .collect(),
        Err(error) => return discovery_failure(format!("scan: {error}")),
    };
    if interfaces.is_empty() {
        return Discovery::default();
    }

    let options = Options {
        fuel: Some(DISCOVERY_FUEL),
        max_memory_bytes: Some(DISCOVERY_MAX_MEMORY_BYTES),
        ..Options::default()
    };
    let mut config = wasmtime_config(&options);
    config.epoch_interruption(true);
    let engine = match Engine::new(&config) {
        Ok(engine) => engine,
        Err(error) => return discovery_failure(format!("engine: {error}")),
    };
    let component = match Component::from_file(&engine, wasm) {
        Ok(component) => component,
        Err(error) => {
            return discovery_failure(format!(
                "cannot compile for discovery (fail closed): {error}"
            ))
        }
    };

    let (tx, rx) = std::sync::mpsc::channel();
    let worker_engine = engine.clone();
    let spawned = std::thread::Builder::new()
        .name("kiqi-plugin-discovery".to_string())
        .spawn(move || {
            let _ = tx.send(probe_providers(&worker_engine, &component, &interfaces));
        });
    if let Err(error) = spawned {
        return discovery_failure(format!("discovery thread: {error}"));
    }
    match rx.recv_timeout(DISCOVERY_WAIT) {
        Ok(Ok(providers)) => Discovery {
            providers,
            diagnostics: Vec::new(),
        },
        Ok(Err(error)) => discovery_failure(error),
        Err(_) => discovery_failure(format!(
            "discovery did not finish within {}s (fail closed)",
            DISCOVERY_WAIT.as_secs()
        )),
    }
}

fn discovery_failure(message: String) -> Discovery {
    Discovery {
        providers: Vec::new(),
        diagnostics: vec![format!("provider discovery: {message}")],
    }
}

/// The bounded half of [`discover_providers`]; runs on its own thread.
fn probe_providers(
    engine: &Engine,
    component: &Component,
    interfaces: &[String],
) -> Result<Vec<DiscoveredProvider>, String> {
    // No grants: the store carries `Grants::default()` so any host import
    // linked into this probe (none is today) would be denied, not authorized.
    let mut store = Store::new(engine, Grants::default());
    store
        .set_fuel(DISCOVERY_FUEL)
        .map_err(|error| format!("set fuel: {error}"))?;
    // One epoch tick = the deadline below. The ticker only fires if the call
    // is still running when the deadline passes.
    store.set_epoch_deadline(1);
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ticker_engine = engine.clone();
    let ticker_done = done.clone();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + DISCOVERY_DEADLINE;
        while std::time::Instant::now() < deadline {
            if ticker_done.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !ticker_done.load(std::sync::atomic::Ordering::Relaxed) {
            ticker_engine.increment_epoch();
        }
    });

    let linker: Linker<Grants> = Linker::new(engine);
    let instance = linker
        .instantiate(&mut store, component)
        .map_err(|error| format!("cannot instantiate for discovery (fail closed): {error}"))?;
    let mut out = Vec::new();
    for interface in interfaces {
        out.extend(probe_interface(&mut store, &instance, interface)?);
    }
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(out)
}

/// Call one export interface's `list-providers` and apply the output caps.
fn probe_interface(
    store: &mut Store<Grants>,
    instance: &wasmtime::component::Instance,
    interface: &str,
) -> Result<Vec<DiscoveredProvider>, String> {
    let names = [
        format!("kiqi:plugin/{interface}@0.8.0"),
        format!("kiqi:plugin/{interface}"),
        interface.to_string(),
    ];
    let Some(interface_index) = names
        .iter()
        .find_map(|name| instance.get_export_index(&mut *store, None, name))
    else {
        // The export scan saw the name but the compiled component cannot
        // resolve it (a malformed or pre-link component): nothing to learn.
        return Ok(Vec::new());
    };
    let Some(function_index) =
        instance.get_export_index(&mut *store, Some(&interface_index), "list-providers")
    else {
        return Err(format!("{interface} exports no list-providers"));
    };
    let Some(function) = instance.get_func(&mut *store, &function_index) else {
        return Err(format!("{interface}.list-providers is not a function"));
    };
    let results_len = function.ty(&*store).results().len();
    let mut results: Vec<wasmtime::component::Val> = (0..results_len)
        .map(|_| wasmtime::component::Val::Bool(false))
        .collect();
    function
        .call(&mut *store, &[], &mut results)
        .map_err(|error| format!("{interface}.list-providers: {error}"))?;

    let Some(wasmtime::component::Val::List(entries)) = results.into_iter().next() else {
        return Err(format!("{interface}.list-providers did not return a list"));
    };
    if entries.len() > DISCOVERY_MAX_ENTRIES {
        return Err(format!(
            "{interface}.list-providers returned {} entries (cap {DISCOVERY_MAX_ENTRIES})",
            entries.len()
        ));
    }
    let mut out = Vec::new();
    let mut bytes = 0usize;
    for entry in entries {
        let id = record_string(&entry, "id").unwrap_or_default();
        let display_name = record_string(&entry, "display-name").unwrap_or_default();
        if id.len() > DISCOVERY_MAX_STRING_BYTES || display_name.len() > DISCOVERY_MAX_STRING_BYTES
        {
            return Err(format!(
                "{interface}.list-providers returned a string over {DISCOVERY_MAX_STRING_BYTES} bytes"
            ));
        }
        bytes += id.len() + display_name.len();
        if bytes > DISCOVERY_MAX_BYTES {
            return Err(format!(
                "{interface}.list-providers returned over {DISCOVERY_MAX_BYTES} bytes"
            ));
        }
        out.push(DiscoveredProvider {
            interface: interface.to_string(),
            id,
            display_name,
        });
    }
    Ok(out)
}

/// Read one string field from a returned record, if it has that shape.
fn record_string(value: &wasmtime::component::Val, field: &str) -> Option<String> {
    let wasmtime::component::Val::Record(fields) = value else {
        return None;
    };
    fields
        .iter()
        .find(|(name, _)| name == field)
        .and_then(|(_, value)| match value {
            wasmtime::component::Val::String(text) => Some(text.clone()),
            _ => None,
        })
}

/// State every instantiated component gets: WASI plus the resource table and
/// the grants the guest's host imports are allowed to act on.
struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    grants: Grants,
    /// The plugin's own data directory (`plugins/<id>/data/`). The `fs`
    /// capability is jailed here, and `exec`'s working directory is resolved
    /// inside it, so the jail *is* the policy and both grants stay simple.
    /// `None` when the component's path has no parent (in-memory components in
    /// tests), in which case `fs` reports `failed`.
    fs_root: Option<std::path::PathBuf>,
    /// The root an `exec` cwd scope of `project` resolves against. `None`
    /// resolves to the host process's working directory at call time (see
    /// [`Options::project_dir`]).
    project_dir: Option<std::path::PathBuf>,
    /// Raw TCP connections, keyed by the handle the guest was handed.
    sockets: std::collections::HashMap<u32, std::net::TcpStream>,
    next_socket: u32,
    /// Interactive children (`proc`), keyed by the handle the guest was
    /// handed. **Scoped to one tool call**: every guest-visible entry point
    /// reaps them when it returns (see [`HostState::reap_procs`]), so nothing
    /// survives the call that started it.
    procs: std::collections::HashMap<u32, ProcChild>,
    next_proc: u32,
    /// The guest's linear-memory cap, read by the store's limiter.
    limits: wasmtime::StoreLimits,
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
            session_id: with_session(|session| session.session_id())
                .flatten()
                .unwrap_or_default(),
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
    fn cli_flag(&mut self, name: String) -> Option<crate::kiqi::plugin::types::FlagValue> {
        use crate::kiqi::plugin::types::FlagValue as Wit;
        with_session(|session| session.cli_flag(&name))
            .flatten()
            .map(|value| match value {
                kiqi_plugin::FlagValue::Bare => Wit::Bare,
                kiqi_plugin::FlagValue::Text(text) => Wit::Text(text),
            })
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

/// Session mutations, forwarded to the call's `HostCaps::control`; a call
/// without that capability gets `denied`.
impl crate::kiqi::plugin::session_control::Host for HostState {
    fn set_session_name(&mut self, name: String) -> Result<(), CapabilityError> {
        forward(with_control(|control| control.set_session_name(&name)))
    }
    fn set_active_tools(&mut self, tools: Vec<String>) -> Result<(), CapabilityError> {
        forward(with_control(|control| control.set_active_tools(&tools)))
    }
    fn set_model(&mut self, model: String) -> Result<(), CapabilityError> {
        forward(with_control(|control| control.set_model(&model)))
    }
    fn set_thinking_level(&mut self, level: String) -> Result<(), CapabilityError> {
        forward(with_control(|control| control.set_thinking_level(&level)))
    }
    fn compact(&mut self) -> Result<(), CapabilityError> {
        forward(with_control(|control| control.compact()))
    }
    fn new_session(&mut self) -> Result<(), CapabilityError> {
        forward(with_control(|control| control.new_session()))
    }
    fn fork(&mut self) -> Result<(), CapabilityError> {
        forward(with_control(|control| control.fork()))
    }
    fn navigate_tree(&mut self, target: String) -> Result<(), CapabilityError> {
        forward(with_control(|control| control.navigate_tree(&target)))
    }
    fn switch_session(&mut self, id: String) -> Result<(), CapabilityError> {
        forward(with_control(|control| control.switch_session(&id)))
    }
    fn reload(&mut self) -> Result<(), CapabilityError> {
        forward(with_control(|control| control.reload()))
    }
}

/// Content injection, forwarded to the call's `HostCaps::inject`.
impl crate::kiqi::plugin::inject::Host for HostState {
    fn send_message(&mut self, message: String) -> Result<(), CapabilityError> {
        let message = serde_json::from_str(&message).unwrap_or(serde_json::Value::Null);
        forward(with_inject(|inject| inject.send_message(&message)))
    }
    fn send_user_message(&mut self, text: String) -> Result<(), CapabilityError> {
        forward(with_inject(|inject| inject.send_user_message(&text)))
    }
    fn append_entry(&mut self, entry: String) -> Result<(), CapabilityError> {
        let entry = serde_json::from_str(&entry).unwrap_or(serde_json::Value::Null);
        forward(with_inject(|inject| inject.append_message(&entry)))
    }
    fn append_custom_entry(
        &mut self,
        custom_type: String,
        data: String,
    ) -> Result<(), CapabilityError> {
        let data = serde_json::from_str(&data).unwrap_or(serde_json::Value::Null);
        forward(with_inject(|inject| {
            inject.append_custom_entry(&custom_type, &data)
        }))
    }
}

/// UI, forwarded to the call's `HostCaps::ui`.
impl crate::kiqi::plugin::ui::Host for HostState {
    fn dialog(&mut self, request: String) -> Result<String, CapabilityError> {
        let request = serde_json::from_str(&request).unwrap_or(serde_json::Value::Null);
        match with_ui(|ui| ui.dialog(&request)) {
            Some(Ok(value)) => Ok(value.to_string()),
            Some(Err(error)) => Err(CapabilityError::Failed(error.to_string())),
            None => Err(denied()),
        }
    }
    fn set_status(&mut self, key: String, value: String) -> Result<(), CapabilityError> {
        forward(with_ui(|ui| {
            ui.set_status(&serde_json::json!({"key": key, "value": value}))
        }))
    }
    fn set_editor_text(&mut self, text: String) -> Result<(), CapabilityError> {
        forward(with_ui(|ui| {
            ui.set_editor_text(&serde_json::json!({"text": text}))
        }))
    }
    fn component(&mut self, name: String, payload: String) -> Result<String, CapabilityError> {
        let payload = serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null);
        match with_ui(|ui| ui.component(&name, &payload)) {
            Some(Ok(value)) => Ok(value.to_string()),
            Some(Err(error)) => Err(CapabilityError::Failed(error.to_string())),
            None => Err(denied()),
        }
    }
}

/// Response bodies are capped. A granted — or compromised — host must not be
/// able to make the *host* allocate without bound; a plugin that genuinely needs
/// a larger document should stream it through `net-sockets`.
const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;

/// Host-mediated HTTP: the request's host must be in the grant's allow-list.
impl crate::kiqi::plugin::net_http::Host for HostState {
    fn fetch(&mut self, request: String) -> Result<String, CapabilityError> {
        let request: serde_json::Value = serde_json::from_str(&request).map_err(|error| {
            CapabilityError::Failed(format!("invalid net-http request: {error}"))
        })?;
        let url = request
            .get("url")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                CapabilityError::Failed("net-http request.url is required".to_string())
            })?;
        let parsed = reqwest::Url::parse(url)
            .map_err(|error| CapabilityError::Failed(format!("invalid url `{url}`: {error}")))?;
        let host = parsed.host_str().unwrap_or_default().to_string();
        if !self.grants.allows_host(&host) {
            return Err(CapabilityError::Denied(format!(
                "net-http to `{host}` is not granted"
            )));
        }

        let allowed_hosts = self
            .grants
            .net_http
            .as_ref()
            .map(|grant| grant.hosts.clone())
            .unwrap_or_default();
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            // A redirect is a second request, to a host the user may not have
            // granted. Follow it only when that host is allow-listed too;
            // otherwise stop and hand the 3xx back, so a granted host cannot
            // bounce the plugin somewhere it was never allowed to go.
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= 10 {
                    return attempt.error("too many redirects");
                }
                let host = attempt.url().host_str().unwrap_or_default();
                if allows(&allowed_hosts, host) {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()
            .map_err(|error| CapabilityError::Failed(error.to_string()))?;
        let method = request
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("GET");
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|error| CapabilityError::Failed(error.to_string()))?;
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

        let response = builder
            .send()
            .map_err(|error| CapabilityError::Failed(error.to_string()))?;
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
        let body = {
            use std::io::Read;
            // Read one byte past the cap to tell "exactly full" from "too big".
            let mut bytes = Vec::new();
            response
                .take(MAX_RESPONSE_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| CapabilityError::Failed(format!("read body: {error}")))?;
            if bytes.len() as u64 > MAX_RESPONSE_BYTES {
                return Err(CapabilityError::Failed(format!(
                    "response body exceeds {MAX_RESPONSE_BYTES} bytes"
                )));
            }
            String::from_utf8_lossy(&bytes).into_owned()
        };
        Ok(serde_json::json!({"status": status, "headers": headers, "body": body}).to_string())
    }
}

impl HostState {
    /// Resolve a guest-supplied relative path inside the fs grant's scope, or
    /// explain why it may not: `plugin-data` jails to the plugin's own data
    /// directory, `project` to the project tree (same scope `exec`/`proc`
    /// use).
    ///
    /// A path with any `..`, root or prefix component is refused outright, so a
    /// plugin cannot walk out of its jail — and `starts_with` is checked as well
    /// rather than relied on.
    fn fs_checked(
        &self,
        rel: &str,
        create_parents: bool,
    ) -> Result<std::path::PathBuf, CapabilityError> {
        let Some(grant) = self.grants.fs.as_ref() else {
            return Err(CapabilityError::Denied("fs is not granted".to_string()));
        };
        let (root, what) = match grant.scope() {
            kiqi_plugin::grants::ExecCwd::PluginData => (self.data_dir()?, "the plugin directory"),
            kiqi_plugin::grants::ExecCwd::Project => (self.project_dir()?, "the project directory"),
        };
        let resolved = jail_under(&root, rel, what)?;
        symlink_checked_under(&root, &resolved, create_parents, what)
    }

    /// Resolve a relative path inside the plugin's data directory without any
    /// capability check — the lexical half of the jail, exposed to tests so each
    /// guard can be asserted in isolation. Production paths go through
    /// [`fs_checked`](Self::fs_checked) or [`scoped_cwd`](Self::scoped_cwd), which
    /// combine both halves; without this attribute the method is dead code in every
    /// non-test build (which is what the compiler was reporting).
    #[cfg(test)]
    fn jail(&self, rel: &str) -> Result<std::path::PathBuf, CapabilityError> {
        jail_under(&self.data_dir()?, rel, "the plugin directory")
    }

    /// Resolve the working directory `exec` and `proc` share: the grant's
    /// scope decides the root (`plugin-data` = the plugin's sandbox,
    /// `project` = the user's tree), and the guest-supplied path gets the same
    /// lexical + symlink checks `fs` uses. The data directory is created on
    /// demand — it is the plugin's own sandbox; the project directory is the
    /// user's, so a misspelled cwd fails there instead of `mkdir -p`-ing into
    /// it.
    fn scoped_cwd(
        &self,
        scope: kiqi_plugin::grants::ExecCwd,
        cwd: Option<String>,
    ) -> Result<std::path::PathBuf, CapabilityError> {
        let (root, what) = match scope {
            kiqi_plugin::grants::ExecCwd::PluginData => (self.data_dir()?, "the plugin directory"),
            kiqi_plugin::grants::ExecCwd::Project => (self.project_dir()?, "the project directory"),
        };
        let sandboxed = matches!(scope, kiqi_plugin::grants::ExecCwd::PluginData);
        let resolved = match cwd {
            Some(rel) => {
                let resolved = jail_under(&root, &rel, what)?;
                // `.`/`./`/`` resolve to the root itself: the guest picked
                // nothing, so there is no final component to symlink-check
                // (the parent comparison would wrongly test the root's parent
                // against the root).
                if resolved == root {
                    root.clone()
                } else if !sandboxed && !resolved.exists() {
                    return Err(CapabilityError::Failed(format!(
                        "cwd `{rel}` does not exist inside {what}"
                    )));
                } else {
                    symlink_checked_under(&root, &resolved, sandboxed, what)?
                }
            }
            None => root,
        };
        if sandboxed {
            std::fs::create_dir_all(&resolved).map_err(|error| {
                CapabilityError::Failed(format!("create {}: {error}", resolved.display()))
            })?;
        }
        Ok(resolved)
    }

    /// Close every `proc` handle: kill each child's process group and reap it.
    ///
    /// Called at the end of every guest-visible entry point, which is what
    /// makes a handle's lifetime "one tool call" — a guest that forgets to
    /// `kill`, or traps mid-session, leaves nothing behind.
    fn reap_procs(&mut self) {
        for (_, mut child) in self.procs.drain() {
            child.shutdown();
        }
        self.next_proc = 1;
    }

    fn data_dir(&self) -> Result<std::path::PathBuf, CapabilityError> {
        self.fs_root
            .clone()
            .ok_or_else(|| CapabilityError::Failed("this plugin has no data directory".to_string()))
    }

    /// The project directory an `exec` cwd scope of `project` resolves against.
    ///
    /// `None` (the normal embedding) means the host process's working
    /// directory: `ki` never chdirs after startup and the session's recorded
    /// cwd is initialised from the same place, so the process cwd *is* the
    /// project directory. Tests and embedders that do relocate set the
    /// [`Options::project_dir`] knob instead.
    fn project_dir(&self) -> Result<std::path::PathBuf, CapabilityError> {
        match &self.project_dir {
            Some(dir) => Ok(dir.clone()),
            None => std::env::current_dir().map_err(|error| {
                CapabilityError::Failed(format!("resolve the project directory: {error}"))
            }),
        }
    }

    /// Re-check a lexically-jailed path against symlinks.
    ///
    /// The lexical check is not a jail on its own: a symlink left in the data
    /// directory (by `exec`, or by the user) would be followed straight out of
    /// it. So the parent is canonicalized and must still sit inside the
    /// canonical root, and the final component must not itself be a link.
    /// The symlink half of the jail for the plugin's data directory; see [`jail`]
    /// for why this is test-only.
    ///
    /// [`jail`]: Self::jail
    #[cfg(test)]
    fn symlink_checked(
        &self,
        resolved: &std::path::Path,
        create_parents: bool,
    ) -> Result<std::path::PathBuf, CapabilityError> {
        symlink_checked_under(
            &self.data_dir()?,
            resolved,
            create_parents,
            "the plugin directory",
        )
    }
}

/// Resolve a guest-supplied relative path inside `root`, or explain why it may
/// not.
///
/// A path with any `..`, root or prefix component is refused outright, so a
/// plugin cannot walk out of its jail — and `starts_with` is checked as well
/// rather than relied on. `what` names the jail in errors (`the plugin
/// directory` for `fs`, `the project directory` for a project-scoped `exec`).
fn jail_under(
    root: &std::path::Path,
    rel: &str,
    what: &str,
) -> Result<std::path::PathBuf, CapabilityError> {
    let candidate = std::path::Path::new(rel);
    let mut resolved = root.to_path_buf();
    for component in candidate.components() {
        match component {
            std::path::Component::Normal(part) => resolved.push(part),
            std::path::Component::CurDir => {}
            _ => {
                return Err(CapabilityError::Failed(format!(
                    "path `{rel}` must be relative and stay inside {what}"
                )))
            }
        }
    }
    if !resolved.starts_with(root) {
        return Err(CapabilityError::Failed(format!(
            "path `{rel}` escapes {what}"
        )));
    }
    Ok(resolved)
}

/// [`symlink_checked`](HostState::symlink_checked) against an explicit root.
fn symlink_checked_under(
    root: &std::path::Path,
    resolved: &std::path::Path,
    create_parents: bool,
    what: &str,
) -> Result<std::path::PathBuf, CapabilityError> {
    // Create it first: `canonicalize` needs the directory to exist, and
    // falling back to the un-canonicalized path would compare a symlinked
    // root (`/var` vs `/private/var` on macOS) against a canonicalized
    // parent and reject every path.
    std::fs::create_dir_all(root)
        .map_err(|error| CapabilityError::Failed(format!("create {}: {error}", root.display())))?;
    let root = &root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    // A path equal to the root itself has no final component inside the jail —
    // refuse it as before (callers name things inside the jail; `exec` handles
    // its own "cwd is the root" case without calling this).
    if resolved == root {
        return Err(CapabilityError::Failed(format!(
            "path has no file name inside {what}"
        )));
    }
    // Verify the deepest EXISTING component before creating anything:
    // `create_dir_all` on an unchecked parent would follow a symlinked middle
    // component out of the jail and create directories in it — the escape
    // check would still refuse the operation, but the pollution would stay.
    // `resolved` is lexically inside the root already (`jail_under`), so this
    // walk reaches the root at the latest. A broken symlink lstats fine but
    // fails to canonicalize — fail closed.
    let mut probe = resolved;
    while probe != root && std::fs::symlink_metadata(probe).is_err() {
        probe = probe.parent().unwrap_or(root);
    }
    let probe = probe.canonicalize().map_err(|error| {
        CapabilityError::Failed(format!("resolve {}: {error}", probe.display()))
    })?;
    if !probe.starts_with(root) {
        return Err(CapabilityError::Failed(format!("path escapes {what}")));
    }
    // Only now is creation safe: everything from the verified prefix down did
    // not exist a moment ago, so no symlink can already sit inside it.
    let parent = resolved.parent().unwrap_or(root);
    if create_parents {
        std::fs::create_dir_all(parent).map_err(|error| {
            CapabilityError::Failed(format!("create {}: {error}", parent.display()))
        })?;
    }
    // The final component itself must not be a link, even to somewhere inside
    // the root.
    if let Ok(metadata) = std::fs::symlink_metadata(resolved) {
        if metadata.file_type().is_symlink() {
            return Err(CapabilityError::Failed(format!(
                "refusing to follow a symlink inside {what}"
            )));
        }
    }
    Ok(resolved.to_path_buf())
}

/// Plugin filesystem, jailed to `plugins/<id>/data/`.
impl crate::kiqi::plugin::fs::Host for HostState {
    fn read(&mut self, rel_path: String) -> Result<Vec<u8>, CapabilityError> {
        let path = self.fs_checked(&rel_path, false)?;
        std::fs::read(&path)
            .map_err(|error| CapabilityError::Failed(format!("read {}: {error}", path.display())))
    }

    fn write(&mut self, rel_path: String, contents: Vec<u8>) -> Result<(), CapabilityError> {
        let path = self.fs_checked(&rel_path, true)?;
        std::fs::write(&path, contents)
            .map_err(|error| CapabilityError::Failed(format!("write {}: {error}", path.display())))
    }

    fn read_dir(&mut self, rel_dir: String) -> Result<Vec<String>, CapabilityError> {
        let path = self.fs_checked(&rel_dir, false)?;
        let entries = std::fs::read_dir(&path).map_err(|error| {
            CapabilityError::Failed(format!("read dir {}: {error}", path.display()))
        })?;
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        Ok(names)
    }
}

/// Raw TCP, allow-listed per host. The host holds the sockets and hands the
/// guest opaque handles (which is exactly the shape the WIT already declared),
/// so the grant cannot be bypassed by a second connect.
impl crate::kiqi::plugin::net_sockets::Host for HostState {
    fn connect(&mut self, address: String) -> Result<u32, CapabilityError> {
        let (host, port) = split_host_port(&address)?;
        if !self.grants.allows_socket_host(&host) {
            return Err(CapabilityError::Denied(format!(
                "net-sockets to `{host}` is not granted"
            )));
        }
        // Bound the live handles the way `proc` bounds its children: unbounded
        // they let a looping guest exhaust the host process's file
        // descriptors, which breaks every other plugin's fs/exec too.
        if self.sockets.len() >= MAX_OPEN_SOCKETS {
            return Err(CapabilityError::Failed(format!(
                "net-sockets: {MAX_OPEN_SOCKETS} sockets are already open on this plugin; close one first"
            )));
        }
        use std::net::ToSocketAddrs;
        let resolved = (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|error| CapabilityError::Failed(format!("resolve `{address}`: {error}")))?
            .next()
            .ok_or_else(|| CapabilityError::Failed(format!("`{address}` resolved to nothing")))?;
        let stream =
            std::net::TcpStream::connect_timeout(&resolved, std::time::Duration::from_secs(10))
                .map_err(|error| {
                    CapabilityError::Failed(format!("connect `{address}`: {error}"))
                })?;
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(30)));
        let handle = self.next_socket;
        self.next_socket += 1;
        self.sockets.insert(handle, stream);
        Ok(handle)
    }

    fn send(&mut self, handle: u32, bytes: Vec<u8>) -> Result<u64, CapabilityError> {
        use std::io::Write;
        let stream = self.socket(handle)?;
        stream
            .write_all(&bytes)
            .map_err(|error| CapabilityError::Failed(format!("socket {handle} write: {error}")))?;
        Ok(bytes.len() as u64)
    }

    fn recv(&mut self, handle: u32, max_bytes: u64) -> Result<Vec<u8>, CapabilityError> {
        use std::io::Read;
        let cap = max_bytes.min(1024 * 1024) as usize;
        let stream = self.socket(handle)?;
        let mut buffer = vec![0u8; cap];
        let read = stream
            .read(&mut buffer)
            .map_err(|error| CapabilityError::Failed(format!("socket {handle} read: {error}")))?;
        buffer.truncate(read);
        Ok(buffer)
    }

    fn close(&mut self, handle: u32) -> Result<(), CapabilityError> {
        self.sockets
            .remove(&handle)
            .map(|_| ())
            .ok_or_else(|| CapabilityError::Failed(format!("no open socket {handle}")))
    }
}

/// Build the command `proc::spawn` runs: the same construction `exec` uses —
/// scrubbed env with `PATH` back, its own process group, piped stdio.
fn spawn_proc(
    cmd: &str,
    args: &[String],
    cwd: &std::path::Path,
) -> Result<std::process::Child, CapabilityError> {
    use std::process::{Command, Stdio};
    let mut command = Command::new(cmd);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Cleared, then only `PATH` back — same rule as `exec`.
        .env_clear();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .spawn()
        .map_err(|error| CapabilityError::Failed(format!("spawn `{cmd}`: {error}")))
}

/// Truncate to at most `max_bytes` on a char boundary; `0` means the host's
/// hard cap only.
fn truncate_chars(text: &str, max_bytes: u64) -> String {
    if max_bytes == 0 || text.len() <= max_bytes as usize {
        return text.to_string();
    }
    let mut end = max_bytes as usize;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Interactive children: `spawn` / `write` / `read-line` / `read-stderr` /
/// `wait` / `kill`.
///
/// Three rules shape the implementation:
///
/// * **Handles last one tool call.** The host reaps every child when the
///   calling entry point returns ([`HostState::reap_procs`]), so a guest that
///   forgets `kill` — or traps mid-session — leaks nothing.
/// * **Every wait is deadline-checked** against the grant's per-child budget,
///   so a silent child cannot wedge the call.
/// * **Nothing unbounded**: channels are bounded (a chatty child blocks on its
///   own write), lines and stderr are capped, and the number of live children
///   is the grant's `max-open`.
impl crate::kiqi::plugin::proc::Host for HostState {
    fn spawn(
        &mut self,
        cmd: String,
        args: Vec<String>,
        cwd: Option<String>,
    ) -> Result<u32, CapabilityError> {
        let grant = self
            .grants
            .proc
            .clone()
            .ok_or_else(|| CapabilityError::Denied("proc is not granted".to_string()))?;
        if !kiqi_plugin::grants::allows(&grant.commands, &cmd) {
            return Err(CapabilityError::Denied(format!(
                "proc of `{cmd}` is not granted"
            )));
        }
        if cmd.contains('/') || cmd.contains('\\') {
            return Err(CapabilityError::Failed(format!(
                "proc command must be a bare name, not a path: `{cmd}`"
            )));
        }
        let max_open = effective_proc_max_open(grant.max_open);
        if self.procs.len() >= max_open as usize {
            return Err(CapabilityError::Denied(format!(
                "proc already holds {} children and the grant allows {} at once",
                self.procs.len(),
                max_open
            )));
        }
        let cwd = self.scoped_cwd(grant.cwd, cwd)?;
        let mut child = spawn_proc(&cmd, &args, &cwd)?;

        // stdout → lines. A bounded channel: when the guest stops reading, the
        // reader blocks and the child blocks on its own write.
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CapabilityError::Failed("the child has no stdout pipe".to_string()))?;
        let (line_tx, line_rx) = std::sync::mpsc::sync_channel::<String>(PROC_CHANNEL_DEPTH);
        std::thread::spawn(move || {
            use std::io::Read;
            // Read in chunks and frame lines by hand: `BufRead::lines()` grows
            // its String until a newline arrives, so a newline-free stream
            // (say, `cat /dev/zero`) would buffer without bound before the
            // `PROC_LINE_CAP` truncation could apply. Here a line is capped as
            // it accumulates, and only the cap is ever held in memory.
            let mut reader = std::io::BufReader::new(stdout);
            let mut chunk = [0u8; 8192];
            let mut line: Vec<u8> = Vec::new();
            // A line longer than the cap: keep the first `PROC_LINE_CAP` bytes,
            // report that line truncated, drop the rest up to the newline.
            let mut discarding = false;
            let send = |line: &mut Vec<u8>, line_tx: &std::sync::mpsc::SyncSender<String>| {
                let text = String::from_utf8_lossy(line).into_owned();
                line.clear();
                line_tx
                    .send(truncate_chars(&text, PROC_LINE_CAP as u64))
                    .is_ok()
            };
            'outer: loop {
                let read = match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                let mut start = 0;
                for index in 0..read {
                    if chunk[index] != b'\n' {
                        continue;
                    }
                    if !discarding && !chunk[start..index].is_empty() {
                        line.extend_from_slice(&chunk[start..index]);
                    }
                    // Like `lines()`, strip a trailing `\r` (CRLF endings).
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    if !send(&mut line, &line_tx) {
                        break 'outer;
                    }
                    discarding = false;
                    start = index + 1;
                }
                if start < read {
                    if discarding {
                        continue;
                    }
                    let room = PROC_LINE_CAP.saturating_sub(line.len());
                    let tail = &chunk[start..read];
                    line.extend_from_slice(&tail[..tail.len().min(room)]);
                    if tail.len() > room {
                        // The line outgrew the cap: send what we have as a
                        // truncated line when its newline arrives.
                        discarding = true;
                    }
                }
            }
            // A final line without a trailing newline (an over-long final line
            // arrives truncated by the cap, like every other over-long line).
            if !line.is_empty() {
                let _ = send(&mut line, &line_tx);
            }
            // Dropping `line_tx` is the EOF signal `read-line` reports as `none`.
        });

        // stderr → a capped buffer the guest polls.
        let stderr_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        if let Some(mut stderr) = child.stderr.take() {
            let sink = std::sync::Arc::clone(&stderr_buf);
            std::thread::spawn(move || {
                use std::io::Read;
                let mut chunk = [0u8; 8192];
                loop {
                    match stderr.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            let mut sink = sink.lock().unwrap_or_else(|p| p.into_inner());
                            let room = PROC_STDERR_CAP.saturating_sub(sink.len());
                            sink.extend_from_slice(&chunk[..read.min(room)]);
                        }
                    }
                }
            });
        }

        // stdin → a writer thread, so a child that stops reading makes the
        // guest's `write` time out instead of blocking the call forever.
        let (stdin_tx, stdin_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(PROC_CHANNEL_DEPTH);
        let writer = child.stdin.take().map(|mut stdin| {
            std::thread::spawn(move || {
                use std::io::Write;
                while let Ok(bytes) = stdin_rx.recv() {
                    if stdin.write_all(&bytes).is_err() {
                        break;
                    }
                    let _ = stdin.flush();
                }
            })
        });
        let stdin = writer.as_ref().map(|_| stdin_tx);

        let deadline = std::time::Instant::now() + effective_proc_timeout(grant.timeout_seconds);
        let handle = self.next_proc;
        self.next_proc += 1;
        self.procs.insert(
            handle,
            ProcChild {
                child,
                lines: line_rx,
                stderr: stderr_buf,
                stdin,
                writer,
                deadline,
            },
        );
        Ok(handle)
    }

    fn write(&mut self, handle: u32, bytes: Vec<u8>) -> Result<u64, CapabilityError> {
        let child = self
            .procs
            .get_mut(&handle)
            .ok_or_else(|| CapabilityError::Failed(format!("no open child {handle}")))?;
        let Some(stdin) = child.stdin.as_ref() else {
            return Err(CapabilityError::Failed(
                "the child's stdin is closed".to_string(),
            ));
        };
        let remaining = child
            .deadline
            .saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(CapabilityError::Failed(
                "the child's time budget is spent".to_string(),
            ));
        }
        let len = bytes.len() as u64;
        let deadline = std::time::Instant::now() + remaining;
        // `SyncSender::send_timeout` is not stable yet, so this is the
        // equivalent loop: `try_send` hands the buffer back when the queue is
        // full, so nothing is copied per attempt.
        let mut pending = bytes;
        loop {
            match stdin.try_send(pending) {
                Ok(()) => return Ok(len),
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    return Err(CapabilityError::Failed(
                        "the child's stdin is gone (it exited?)".to_string(),
                    ))
                }
                Err(std::sync::mpsc::TrySendError::Full(returned)) => pending = returned,
            }
            if std::time::Instant::now() >= deadline {
                return Err(CapabilityError::Failed(
                    "the child is not reading its stdin (its budget ran out waiting)".to_string(),
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn read_line(
        &mut self,
        handle: u32,
        max_bytes: u64,
        timeout_ms: u64,
    ) -> Result<Option<String>, CapabilityError> {
        let child = self
            .procs
            .get(&handle)
            .ok_or_else(|| CapabilityError::Failed(format!("no open child {handle}")))?;
        let remaining = child
            .deadline
            .saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(CapabilityError::Failed(
                "the child's time budget is spent".to_string(),
            ));
        }
        // The guest's own deadline may be shorter than the child's budget:
        // `timeout_ms = 0` means "however long the budget allows", anything
        // else caps this wait so a plugin can report its own timeout.
        let wait = match timeout_ms {
            0 => remaining,
            ms => remaining.min(std::time::Duration::from_millis(ms)),
        };
        match child.lines.recv_timeout(wait) {
            Ok(line) => Ok(Some(truncate_chars(&line, max_bytes))),
            // The reader dropped its sender: stdout ended.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Ok(None),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(CapabilityError::Failed(
                "no output within the wait".to_string(),
            )),
        }
    }

    fn read_stderr(&mut self, handle: u32, max_bytes: u64) -> Result<Vec<u8>, CapabilityError> {
        let child = self
            .procs
            .get(&handle)
            .ok_or_else(|| CapabilityError::Failed(format!("no open child {handle}")))?;
        let sink = child.stderr.lock().unwrap_or_else(|p| p.into_inner());
        let text = String::from_utf8_lossy(&sink).into_owned();
        Ok(truncate_chars(&text, max_bytes).into_bytes())
    }

    fn wait(&mut self, handle: u32, timeout_ms: u64) -> Result<Option<i32>, CapabilityError> {
        let child = self
            .procs
            .get_mut(&handle)
            .ok_or_else(|| CapabilityError::Failed(format!("no open child {handle}")))?;
        let budget = child
            .deadline
            .saturating_duration_since(std::time::Instant::now());
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms).min(budget);
        loop {
            match child.child.try_wait() {
                Ok(Some(status)) => return Ok(Some(status.code().unwrap_or(-1))),
                Ok(None) => {}
                Err(error) => {
                    return Err(CapabilityError::Failed(format!("wait on child: {error}")))
                }
            }
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn cancel_requested(&mut self) -> bool {
        // The per-call `HostCaps.cancel` probe is installed around the guest
        // call; outside a tool call there is nothing to cancel.
        with_caps_context(|caps| caps.cancel.is_some_and(|probe| probe.is_cancelled()))
            .unwrap_or(false)
    }

    fn close_stdin(&mut self, handle: u32) -> Result<(), CapabilityError> {
        // Dropping the sender ends the writer thread, which closes the pipe:
        // the child observes EOF. Unknown handle = already closed (idempotent).
        if let Some(child) = self.procs.get_mut(&handle) {
            child.stdin = None;
        }
        Ok(())
    }

    fn kill(&mut self, handle: u32) -> Result<(), CapabilityError> {
        // Idempotent by contract: an unknown handle is already closed.
        if let Some(mut child) = self.procs.remove(&handle) {
            child.shutdown();
        }
        Ok(())
    }
}

/// Run a child process: allow-listed bare command name, grant-scoped cwd,
/// cleared environment, capped output, wall-clock timeout. No shell is
/// involved, so shell metacharacters in `args` stay literal.
///
/// The cwd scope rides with the grant: `plugin-data` jails the child's working
/// directory to `plugins/<id>/data/`; `project` jails it to the project
/// directory (for diagnostic plugins that run `cargo`/`clippy`). Note the jail
/// bounds where the child *starts*, not what it can reach: the child inherits
/// the user's privileges and can read or write anywhere it can path to. The
/// scope exists so a plugin cannot *accidentally* operate on the wrong tree
/// and so the grant's intent stays legible — not to sandbox the child.
impl crate::kiqi::plugin::exec::Host for HostState {
    fn run(
        &mut self,
        cmd: String,
        args: Vec<String>,
        cwd: Option<String>,
    ) -> Result<String, CapabilityError> {
        if !self.grants.allows_command(&cmd) {
            return Err(CapabilityError::Denied(format!(
                "exec of `{cmd}` is not granted"
            )));
        }
        if cmd.contains('/') || cmd.contains('\\') {
            return Err(CapabilityError::Failed(format!(
                "exec command must be a bare name, not a path: `{cmd}`"
            )));
        }
        let grant = self
            .grants
            .exec
            .as_ref()
            .expect("allows_command implies an exec grant");
        let cwd = self.scoped_cwd(grant.cwd, cwd)?;
        run_with_limits(&cmd, &args, &cwd, grant.timeout_seconds)
    }
}

impl HostState {
    fn socket(&mut self, handle: u32) -> Result<&mut std::net::TcpStream, CapabilityError> {
        self.sockets
            .get_mut(&handle)
            .ok_or_else(|| CapabilityError::Failed(format!("no open socket {handle}")))
    }
}

/// Split `host:port` (IPv6 in brackets, or a bare `SocketAddr`).
fn split_host_port(address: &str) -> Result<(String, u16), CapabilityError> {
    if let Ok(parsed) = address.parse::<std::net::SocketAddr>() {
        return Ok((parsed.ip().to_string(), parsed.port()));
    }
    let (host, port) = address
        .rsplit_once(':')
        .ok_or_else(|| CapabilityError::Failed(format!("`{address}` must be host:port")))?;
    let port = port
        .parse::<u16>()
        .map_err(|_| CapabilityError::Failed(format!("invalid port in `{address}`")))?;
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    if host.is_empty() {
        return Err(CapabilityError::Failed(format!("`{address}` has no host")));
    }
    Ok((host, port))
}

/// The effective per-child budget for `proc`: the grant's value clamped to its
/// own cap (600s by default, 1800s at most — deliberately independent of
/// `exec`'s 300s, which is the point of a separate capability) and floored at
/// 1s (the parse layer rejects 0, so only an in-code grant can say it).
fn effective_proc_timeout(timeout_seconds: u64) -> std::time::Duration {
    if timeout_seconds == 0 {
        eprintln!(
            "[kiqi-plugin] proc grant timeout 0s is not a usable budget; using 1s \
             (the parse layer rejects 0, so only an in-code grant can say it)"
        );
        return std::time::Duration::from_secs(1);
    }
    let clamped = timeout_seconds.min(kiqi_plugin::grants::PROC_TIMEOUT_MAX_SECONDS);
    if clamped != timeout_seconds {
        eprintln!(
            "[kiqi-plugin] proc grant timeout {timeout_seconds}s exceeds the {clamped}s cap; \
             clamped (a hand-edited grants.json is the only way in past the CLI's check)"
        );
    }
    std::time::Duration::from_secs(clamped)
}

/// The effective `proc` concurrency bound: the grant's `max-open` clamped to
/// the hard cap (the machine's process table, not something the sandbox can
/// carry). Like the timeouts, only a hand-edited `grants.json` can come in
/// over it — `grant()` rejects the rest at grant time — so the clamp is the
/// last-resort guard, and it says so.
fn effective_proc_max_open(max_open: u32) -> u32 {
    let clamped = max_open.min(kiqi_plugin::grants::PROC_MAX_OPEN_MAX);
    if clamped != max_open {
        eprintln!(
            "[kiqi-plugin] proc grant max-open {max_open} exceeds the {clamped} cap; \
             clamped (a hand-edited grants.json is the only way in past the CLI's check)"
        );
    }
    clamped
}

/// The effective exec timeout: the grant's budget clamped to the cap and
/// floored at 1s. The CLI and the parse layer validate at grant time, so only
/// a hand-edited `grants.json` (over the cap) or an in-code grant (at 0) can
/// come in out of bounds — the clamp is the last-resort guard, and it says
/// so.
fn effective_exec_timeout(timeout_seconds: u64) -> std::time::Duration {
    if timeout_seconds == 0 {
        eprintln!(
            "[kiqi-plugin] exec grant timeout 0s is not a usable budget; using 1s \
             (the parse layer rejects 0, so only an in-code grant can say it)"
        );
        return std::time::Duration::from_secs(1);
    }
    let clamped = timeout_seconds.min(kiqi_plugin::grants::EXEC_TIMEOUT_MAX_SECONDS);
    if clamped != timeout_seconds {
        eprintln!(
            "[kiqi-plugin] exec grant timeout {timeout_seconds}s exceeds the {clamped}s cap; \
             clamped (a hand-edited grants.json is the only way in past the CLI's check)"
        );
    }
    std::time::Duration::from_secs(clamped)
}

/// Hard cap on one line of a child's stdout, applied by the reader thread; the
/// guest's `max-bytes` truncates further. Bounds what a single line can cost.
const PROC_LINE_CAP: usize = 1024 * 1024;

/// How much of a child's stderr is kept for `read-stderr`.
const PROC_STDERR_CAP: usize = 256 * 1024;

/// How many lines (or stdin chunks) may sit in a child's channel before the
/// reader/writer blocks. This is the backpressure that keeps a chatty child
/// from buffering the host's memory.
const PROC_CHANNEL_DEPTH: usize = 1024;

/// How many sockets one plugin may hold open at once through `net-sockets`.
/// The map would otherwise grow without bound — nothing else reaps it — and a
/// guest that loops `connect` would drain the host's file descriptors.
const MAX_OPEN_SOCKETS: usize = 256;

/// One child spawned through `proc`.
///
/// Three helper threads keep the guest from blocking the host: stdout is framed
/// into lines, stderr is collected, and stdin is written. Each channel is
/// bounded and every wait is deadline-checked, so neither a silent child nor a
/// child that stops reading can wedge the call.
struct ProcChild {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
    stderr: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    stdin: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    writer: Option<std::thread::JoinHandle<()>>,
    /// When this child's granted budget runs out.
    deadline: std::time::Instant,
}

impl ProcChild {
    /// Kill the child's whole process group, reap it, and stop its writers.
    /// Idempotent: a child that already exited is simply reaped.
    fn shutdown(&mut self) {
        #[cfg(unix)]
        {
            // SAFETY: `kill` with a negative pid signals the process group the
            // child was started in (`process_group(0)` in `spawn_proc`); ESRCH
            // (already gone) is the common case and is ignored.
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Dropping the sender ends the writer thread (its `write_all` fails on
        // the closed pipe); joining keeps shutdown deterministic.
        self.stdin = None;
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

/// Spawn, capture output with a byte cap, and enforce a wall-clock deadline.
///
/// `timeout_seconds` is the grant's budget, clamped to
/// [`EXEC_TIMEOUT_MAX_SECONDS`](kiqi_plugin::grants::EXEC_TIMEOUT_MAX_SECONDS).
/// Read `reader` to the end, keeping at most `cap` bytes.
///
/// Stopping at the cap and closing the pipe (`.take(cap)`) hands a still-running
/// child an EPIPE — or a SIGPIPE, which kills a C program outright — so a
/// command that merely produced too much output died mid-run and `exec`
/// reported a failure instead of a truncated success. Draining the rest and
/// discarding it truncates the report without signalling the child.
fn drain_capped(mut reader: impl std::io::Read, cap: u64) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let room = cap.saturating_sub(kept.len() as u64) as usize;
                if room > 0 {
                    kept.extend_from_slice(&chunk[..read.min(room)]);
                }
            }
        }
    }
    kept
}

fn run_with_limits(
    cmd: &str,
    args: &[String],
    cwd: &std::path::Path,
    timeout_seconds: u64,
) -> Result<String, CapabilityError> {
    use std::process::{Command, Stdio};
    const OUTPUT_CAP: u64 = 256 * 1024;
    let timeout = effective_exec_timeout(timeout_seconds);

    let mut command = Command::new(cmd);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Cleared, then only `PATH` back: a plugin must not inherit the host's
        // `KI_SESSION_ID` or any other ambient secret.
        .env_clear();
    // Its own process group, so descendants can be reaped with it: a command that
    // leaves a background child holding the pipe would otherwise block the
    // readers below forever — and with them the tool call.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    let mut child = command
        .spawn()
        .map_err(|error| CapabilityError::Failed(format!("spawn `{cmd}`: {error}")))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader = std::thread::spawn(move || drain_capped(stdout, OUTPUT_CAP));
    let stderr_reader = std::thread::spawn(move || drain_capped(stderr, OUTPUT_CAP));

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child
            .try_wait()
            .map_err(|error| CapabilityError::Failed(format!("wait `{cmd}`: {error}")))?
        {
            Some(status) => break Some(status),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    };
    // Reap the whole group, not just the child: a surviving descendant would keep
    // the pipes open and the joins below would never return. `ESRCH` (the group is
    // already gone) is the common case and is ignored.
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    Ok(serde_json::json!({
        "status": status.and_then(|status| status.code()),
        "timedOut": status.is_none(),
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
    })
    .to_string())
}

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
    /// Cap the guest's linear memory. `None` uses [`DEFAULT_MAX_MEMORY_BYTES`];
    /// a guest that asks for more traps on the grow rather than taking the
    /// machine's memory.
    pub max_memory_bytes: Option<usize>,
    /// The project directory an `exec` grant with cwd scope `project` resolves
    /// against. `None` (the default) uses the host process's working directory
    /// at call time — the right answer for `ki`, which never chdirs — while
    /// tests and embedders that relocate set it explicitly.
    pub project_dir: Option<std::path::PathBuf>,
}

/// The default linear-memory cap for a guest: 256 MiB, which is far above what
/// any real plugin needs and far below the wasm32 address space.
pub const DEFAULT_MAX_MEMORY_BYTES: usize = 256 * 1024 * 1024;

/// The wasmtime configuration every engine in this process is built from.
///
/// On macOS this turns off wasmtime's Mach-port trap handler. That handler runs
/// a process-wide thread receiving with `MACH_RCV_MSG | MACH_RCV_INTERRUPT`;
/// any signal delivered to it (SIGCHLD from a child process is the common one
/// here) makes `mach_msg` return `MACH_RCV_INTERRUPTED`, and wasmtime answers
/// that by aborting the whole process (upstream wasmtime#14393, still unfixed
/// on main). POSIX signal handlers are inherited across forks and spawn no such
/// thread, so they are also the right choice for an embedder that forks and
/// spawns subprocesses, as `exec` does.
///
/// wasmtime requires every engine in the process to agree on this setting
/// (`Config::validate` panics otherwise), so build engines through this helper
/// and nowhere else.
fn wasmtime_config(options: &Options) -> wasmtime::Config {
    let mut config = wasmtime::Config::new();
    if options.fuel.is_some() {
        // Metering is off unless a bound is asked for, so ordinary plugins do
        // not pay for it.
        config.consume_fuel(true);
    }
    #[cfg(target_os = "macos")]
    config.macos_use_mach_ports(false);
    config
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
    /// 宿主侧 guest 调用诊断汇（卡：0.8.0 能力补齐，P0）。
    ///
    /// 元数据/观测类入口原先用 `Err(_) => Vec::new()` / `result.ok()` 把 trap 吞掉，
    /// 于是一个**坏掉的 guest** 与一个**什么都没声明的 guest** 在宿主侧完全同形：
    /// 安装/展示/事件扇出全都静默少一块，用户没有任何线索。这里统一走 tracing，
    /// 与 `kiqi-extensions` 既有诊断口径一致。
    fn report_guest_error(&self, op: &str, error: impl std::fmt::Display) {
        tracing::warn!(plugin = %self.id, op, %error, "guest call failed");
    }

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
        let engine = Engine::new(&wasmtime_config(&options))
            .map_err(|error| PluginError::Failed(format!("wasmtime engine: {error}")))?;
        let component = Component::from_file(&engine, wasm)
            .map_err(|error| PluginError::Failed(format!("load {}: {error}", wasm.display())))?;

        let mut linker = Linker::new(&engine);
        World::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |state| state)
            .map_err(|error| PluginError::Failed(format!("link plugin interfaces: {error}")))?;
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|error| PluginError::Failed(format!("link wasi: {error}")))?;

        let fs_root = wasm.parent().map(|dir| dir.join("data"));
        let limits = wasmtime::StoreLimitsBuilder::new()
            .memory_size(options.max_memory_bytes.unwrap_or(DEFAULT_MAX_MEMORY_BYTES))
            .build();
        let mut store = Store::new(
            &engine,
            HostState {
                wasi: WasiCtxBuilder::new().build(),
                table: ResourceTable::new(),
                grants,
                fs_root,
                project_dir: options.project_dir.clone(),
                sockets: std::collections::HashMap::new(),
                next_socket: 1,
                procs: std::collections::HashMap::new(),
                next_proc: 1,
                limits,
            },
        );
        // The cap only matters if the store consults it.
        store.limiter(|state| &mut state.limits);
        if let Some(fuel) = options.fuel {
            store.set_fuel(fuel).map_err(|error| {
                PluginError::Failed(format!("set fuel budget for {id}: {error}"))
            })?;
        }
        let guest = World::instantiate(&mut store, &component, &linker)
            .map_err(|error| PluginError::Failed(format!("instantiate {id}: {error}")))?;
        // The plugin declares its own identity. The install directory stays the
        // authorization key (grants live beside the file and were read by it), so
        // a mismatch is reported rather than silently trusted.
        if let Ok(declared) = guest.kiqi_plugin_plugin_api().call_id(&mut store) {
            if !declared.is_empty() && declared != id {
                eprintln!(
                    "[kiqi-plugin] {id}: component declares id `{declared}`; \
                     grants remain keyed by the install directory"
                );
            }
        }

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
            // `shutdown` is a guest entry point like any other: a plugin may
            // spawn through `proc` in it, and nothing else runs after this
            // point, so reap here too — a handle's lifetime is "one call".
            inner.store.data_mut().reap_procs();
            // `inner` drops here, releasing the store (and its memory).
        }
    }
}

impl WasmPlugin {
    /// Shared body of the two `on_event` faces: call the guest with the call
    /// lock already held (blocking) or skipped (observe).
    fn dispatch_event(
        &self,
        guard: &mut Option<Inner>,
        event: &Event,
        caps: &HostCaps<'_>,
    ) -> EventAction {
        let Some(inner) = guard.as_mut() else {
            return EventAction::Continue;
        };
        // Provider observation is deny-by-default and field-graded per guest:
        // the payload is cut down to what *this* plugin's provider-request
        // grant allows, and an ungranted guest is never called at all. The
        // grants live in the store (that is why this happens here and not in
        // kiqi-extensions, which cannot see them).
        let wit = if event.tag == kiqi_plugin::EventTag::BeforeProviderRequest {
            match graded_provider_request(event, &inner.store.data().grants) {
                Some(wit) => wit,
                None => return EventAction::Continue,
            }
        } else {
            to_wit_event(event)
        };
        let _context = install_context(None, caps);
        let result = inner
            .guest
            .kiqi_plugin_plugin_api()
            .call_on_event(&mut inner.store, &wit);
        clear_context();
        // A `proc` handle lives for this call and nothing beyond it.
        inner.store.data_mut().reap_procs();
        match result {
            Ok(WitEventAction::Continue) => EventAction::Continue,
            Ok(WitEventAction::Claimed) => EventAction::Claimed,
            // The 0.8.0 variant exists in the ABI, but routing a patch to the
            // provider gate is not wired yet, so honoring it is out of scope
            // here. Refusing it (continue) is the fail-closed answer.
            Ok(WitEventAction::Patch(_)) => EventAction::Continue,
            Err(trap) => {
                eprintln!("[kiqi-plugin] {}: on-event trapped: {trap}", self.id);
                EventAction::Continue
            }
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
        let result = guest.call_list_tools(&mut inner.store);
        // Spawning in a metadata call is legal; nothing survives it either.
        inner.store.data_mut().reap_procs();
        match result {
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
        let result = guest.call_list_commands(&mut inner.store);
        // Spawning in a metadata call is legal; nothing survives it either.
        inner.store.data_mut().reap_procs();
        match result {
            Ok(specs) => specs
                .into_iter()
                .map(|spec| CommandSpec {
                    name: spec.name,
                    description: spec.description,
                })
                .collect(),
            Err(e) => {
                self.report_guest_error("list-commands", &e);
                Vec::new()
            }
        }
    }

    fn run_command(
        &self,
        name: &str,
        arguments: &str,
        caps: &HostCaps<'_>,
    ) -> Result<kiqi_plugin::CommandResult, PluginError> {
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
        // A `proc` handle lives for this call and nothing beyond it.
        inner.store.data_mut().reap_procs();
        match result {
            Ok(Ok(result)) => Ok(to_command_result(result)),
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
        let result = guest.call_list_flags(&mut inner.store);
        // Spawning in a metadata call is legal; nothing survives it either.
        inner.store.data_mut().reap_procs();
        match result {
            Ok(specs) => specs
                .into_iter()
                .map(|spec| FlagSpec {
                    name: spec.name,
                    description: spec.description,
                })
                .collect(),
            Err(e) => {
                self.report_guest_error("list-flags", &e);
                Vec::new()
            }
        }
    }

    fn shortcuts(&self) -> Vec<ShortcutSpec> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return Vec::new();
        };
        let guest = inner.guest.kiqi_plugin_plugin_api();
        let result = guest.call_list_shortcuts(&mut inner.store);
        // Spawning in a metadata call is legal; nothing survives it either.
        inner.store.data_mut().reap_procs();
        match result {
            Ok(specs) => specs
                .into_iter()
                .map(|spec| ShortcutSpec {
                    key: spec.key,
                    description: spec.description,
                })
                .collect(),
            Err(e) => {
                self.report_guest_error("list-shortcuts", &e);
                Vec::new()
            }
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
        // A `proc` handle lives for this call and nothing beyond it.
        inner.store.data_mut().reap_procs();
        match result {
            Ok(value) => value,
            Err(e) => {
                self.report_guest_error("before-agent-start", &e);
                None
            }
        }
    }

    fn probe_points(&self) -> Vec<String> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return Vec::new();
        };
        let guest = inner.guest.kiqi_plugin_plugin_api();
        let points = guest.call_list_probe_points(&mut inner.store);
        // Spawning in a metadata call is legal; nothing survives it either.
        inner.store.data_mut().reap_procs();
        match points {
            Ok(points) => points,
            Err(e) => {
                self.report_guest_error("list-probe-points", &e);
                Vec::new()
            }
        }
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
        // A `proc` handle lives for this call and nothing beyond it.
        inner.store.data_mut().reap_procs();
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
        // A `proc` handle lives for this call and nothing beyond it.
        inner.store.data_mut().reap_procs();

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
        self.dispatch_event(&mut guard, event, caps)
    }

    /// Observe fan-out. A guest call such as `ask_user` holds `inner` while it
    /// waits for the user, and the same user must click the dialog that opens
    /// from this very dispatch — blocking here would deadlock the TUI. Observe
    /// events are advisory, so a busy guest simply misses this one.
    fn try_on_event(&self, event: &Event, caps: &HostCaps<'_>) -> EventAction {
        let mut guard = match self.inner.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => return EventAction::Continue,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        };
        self.dispatch_event(&mut guard, event, caps)
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

fn to_command_result(
    result: crate::kiqi::plugin::types::CommandResult,
) -> kiqi_plugin::CommandResult {
    use crate::kiqi::plugin::types::CommandResultBody as Wit;
    let body = match result.body {
        Wit::Message(text) => kiqi_plugin::CommandResultBody::Message(text),
        Wit::Selector(spec) => {
            kiqi_plugin::CommandResultBody::Selector(kiqi_plugin::SelectorSpec {
                title: spec.title,
                items: spec.items,
            })
        }
        Wit::Editor(spec) => kiqi_plugin::CommandResultBody::Editor(kiqi_plugin::EditorSpec {
            initial_text: spec.initial_text,
        }),
        Wit::Prompt(spec) => kiqi_plugin::CommandResultBody::Prompt(kiqi_plugin::PromptSpec {
            title: spec.title,
            placeholder: spec.placeholder,
        }),
    };
    kiqi_plugin::CommandResult {
        flow_id: result.flow_id,
        body,
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

/// The `BeforeProviderRequest` payload as one guest is allowed to see it.
///
/// `None` means the event is not delivered to this guest:
///
/// * no `provider-request` grant — the grant is the only authorization for
///   observation, so a merely-loaded plugin sees nothing;
/// * `observe` is not `requests` — a grant that was recorded without the
///   observe switch still delivers nothing.
///
/// What crosses the boundary is graded by the same grant:
///
/// * `messages` only with `messages = full`. Without it the payload still
///   carries `messageCount`, which tells an observer the size of the turn
///   without handing over the conversation;
/// * header values only with `headers = values`; otherwise any value-bearing
///   key is removed and `headerNames` remains. The `kiqi-extensions` bridge
///   currently emits names only (credentials are redacted at the source), so
///   `values` has nothing extra to add until that bridge carries values; the
///   strip here is what keeps a future bridge from leaking them by default.
///
/// `BeforeProviderHeaders` and `AfterProviderResponse` keep their current
/// semantics in this phase (deferred: they are not grant-gated yet).
fn graded_provider_request(event: &Event, grants: &Grants) -> Option<WitEvent> {
    let grant = grants.provider_request.as_ref()?;
    if grant.observe != ObserveMode::Requests {
        return None;
    }
    let mut payload = event.payload.clone();
    // Count before removing: `messageCount` must survive `messages`, and a
    // payload without messages is a zero-message request.
    let message_count = payload
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    if let Some(object) = payload.as_object_mut() {
        object.insert(
            "messageCount".to_string(),
            serde_json::Value::from(message_count),
        );
        if grant.messages != MessagesMode::Full {
            object.remove("messages");
        }
        if grant.headers != HeadersMode::Values {
            object.remove("headers");
            object.remove("headerValues");
        }
    }
    Some(WitEvent::BeforeProviderRequest(payload.to_string()))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn state(root: &std::path::Path) -> HostState {
        HostState {
            wasi: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            grants: Grants {
                fs: Some(kiqi_plugin::grants::FsGrant {
                    cwd: kiqi_plugin::grants::ExecCwd::PluginData,
                }),
                ..Grants::default()
            },
            fs_root: Some(root.to_path_buf()),
            project_dir: None,
            sockets: std::collections::HashMap::new(),
            next_socket: 1,
            procs: std::collections::HashMap::new(),
            next_proc: 1,
            limits: wasmtime::StoreLimitsBuilder::new().build(),
        }
    }

    /// The jail is lexical *and* symlink-aware: `..` never resolves, and a link
    /// that points outside is refused rather than followed.
    #[test]
    fn the_data_jail_refuses_traversal_and_symlinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("data");
        std::fs::create_dir_all(root.join("sub")).expect("data dir");
        let outside = dir.path().join("secret.txt");
        std::fs::write(&outside, b"secret").expect("secret");
        let state = state(&root);

        assert!(state.jail("sub/ok.txt").is_ok());
        assert!(
            state.jail("../secret.txt").is_err(),
            "`..` must not resolve"
        );
        assert!(state.jail("/etc/passwd").is_err(), "absolute must not pass");

        #[cfg(unix)]
        {
            // A link as the final component, and a link as a directory in the
            // middle — the second is the one a lexical-only check would miss.
            std::os::unix::fs::symlink(&outside, root.join("link")).expect("symlink");
            std::os::unix::fs::symlink(dir.path(), root.join("up")).expect("symlink");
            std::fs::create_dir_all(root.join("sub")).expect("sub");

            let resolved = state.jail("link").expect("lexically inside");
            assert!(
                state.symlink_checked(&resolved, false).is_err(),
                "a final-component symlink must be refused"
            );
            let resolved = state.jail("up/secret.txt").expect("lexically inside");
            assert!(
                state.symlink_checked(&resolved, false).is_err(),
                "a symlinked parent must be refused"
            );
        }
    }

    /// A missing grant is `denied` (authorization), not `failed` (the op broke).
    #[test]
    fn fs_without_a_grant_is_denied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut state = state(dir.path());
        state.grants.fs = None;
        let error = state.fs_checked("x", false).expect_err("denied");
        assert!(matches!(error, CapabilityError::Denied(_)), "got {error:?}");
    }

    /// The timeout clamp: a hand-edited grants.json over the cap is clamped
    /// (the CLI rejects it at grant time; this is the last-resort guard), and
    /// an in-range budget passes through untouched.
    #[test]
    fn the_exec_timeout_is_capped() {
        let max = kiqi_plugin::grants::EXEC_TIMEOUT_MAX_SECONDS;
        assert_eq!(
            effective_exec_timeout(max + 1),
            std::time::Duration::from_secs(max),
            "a hand-edited oversized budget clamps to the cap"
        );
        assert_eq!(
            effective_exec_timeout(120),
            std::time::Duration::from_secs(120),
            "an in-range budget is untouched"
        );
        assert_eq!(
            effective_exec_timeout(0),
            std::time::Duration::from_secs(1),
            "a zero budget floors to 1s (the parse layer rejects 0; this is \
             the last-resort guard)"
        );
    }

    /// `proc`'s budget gets the same guard: its own cap, and the 0 floor the
    /// parse layer's rejection implies.
    #[test]
    fn the_proc_timeout_is_capped() {
        let max = kiqi_plugin::grants::PROC_TIMEOUT_MAX_SECONDS;
        assert_eq!(
            effective_proc_timeout(max + 1),
            std::time::Duration::from_secs(max),
            "a hand-edited oversized budget clamps to the cap"
        );
        assert_eq!(
            effective_proc_timeout(900),
            std::time::Duration::from_secs(900),
            "an in-range budget is untouched"
        );
        assert_eq!(
            effective_proc_timeout(0),
            std::time::Duration::from_secs(1),
            "a zero budget floors to 1s (the parse layer rejects 0; this is \
             the last-resort guard)"
        );
    }

    /// `proc`'s concurrency bound gets the same last-resort guard as its
    /// budget: a hand-edited `max-open` over the hard cap clamps to it, an
    /// in-range one passes through untouched.
    #[test]
    fn the_proc_max_open_is_capped() {
        let max = kiqi_plugin::grants::PROC_MAX_OPEN_MAX;
        assert_eq!(
            effective_proc_max_open(max + 1),
            max,
            "a hand-edited oversized max-open clamps to the cap"
        );
        assert_eq!(
            effective_proc_max_open(8),
            8,
            "an in-range max-open is untouched"
        );
    }

    /// `Options::project_dir` unset — the production embedding for `ki` —
    /// falls back to the process's working directory.
    #[test]
    fn the_project_dir_defaults_to_the_process_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state(dir.path());
        assert!(state.project_dir.is_none());
        assert_eq!(
            state.project_dir().expect("cwd"),
            std::env::current_dir().expect("process cwd"),
        );
    }

    /// Regression: the jail check must verify the existing prefix BEFORE
    /// creating anything. `create_dir_all` on an unchecked parent follows a
    /// symlinked middle component out of the jail and pollutes it — the write
    /// is then refused, but the directory it made outside stays.
    #[cfg(unix)]
    #[test]
    fn the_jail_check_creates_nothing_outside_the_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("data");
        std::fs::create_dir_all(&root).expect("data dir");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        std::os::unix::fs::symlink(&outside, root.join("link")).expect("symlink");

        let state = state(&root);
        let resolved = state
            .jail("link/newdir/file.txt")
            .expect("lexically inside");
        assert!(
            state.symlink_checked(&resolved, true).is_err(),
            "a symlinked middle component must be refused"
        );
        assert!(
            !outside.join("newdir").exists(),
            "the refusal must not have created directories outside the jail"
        );
    }

    /// macOS only: a signal delivered to every thread must not abort the
    /// process. wasmtime's default Mach-port trap handler runs a process-wide
    /// thread that receives with `MACH_RCV_INTERRUPT`; any signal (SIGCHLD from
    /// the children this workspace's suite spawns is the usual one) makes
    /// `mach_msg` return `MACH_RCV_INTERRUPTED` and wasmtime calls
    /// `libc::abort()` (wasmtime#14393). `macos_use_mach_ports(false)` installs
    /// POSIX signal handlers instead and spawns no such thread.
    ///
    /// The child does the delivery so a regression cannot take the whole test
    /// binary down: the parent only reads the exit status. Without the config
    /// line the child dies with SIGABRT (signal 6); with it, every thread just
    /// runs the no-op handler.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_signal_delivered_to_every_thread_does_not_abort_the_process() {
        const CHILD_ENV: &str = "KI_TEST_WASMTIME_SIGNAL_CHILD";
        const TEST_NAME: &str =
            "tests::a_signal_delivered_to_every_thread_does_not_abort_the_process";

        if std::env::var_os(CHILD_ENV).is_some() {
            extern "C" fn ignore(_: libc::c_int) {}
            unsafe {
                libc::signal(libc::SIGUSR1, ignore as *const () as libc::sighandler_t);
            }
            // Creating the engine is what installs wasmtime's trap handler
            // (and, on the default config, spawns the Mach receive thread).
            let _engine =
                Engine::new(&wasmtime_config(&Options::default())).expect("engine builds");

            // Deliver SIGUSR1 to every thread, including the wasmtime handler
            // thread when one exists. A signal the thread does not expect is
            // exactly what an interrupted `mach_msg` sees.
            let mut threads: libc::thread_act_array_t = std::ptr::null_mut();
            let mut count: libc::mach_msg_type_number_t = 0;
            let kret =
                unsafe { libc::task_threads(libc::mach_task_self_, &mut threads, &mut count) };
            assert_eq!(kret, libc::KERN_SUCCESS, "task_threads");
            for index in 0..count as usize {
                let thread = unsafe { *threads.add(index) };
                let pthread = unsafe { libc::pthread_from_mach_thread_np(thread) };
                if pthread != 0 {
                    unsafe {
                        libc::pthread_kill(pthread, libc::SIGUSR1);
                    }
                }
            }
            unsafe {
                libc::vm_deallocate(
                    libc::mach_task_self_,
                    threads as libc::vm_address_t,
                    count as libc::vm_size_t
                        * std::mem::size_of::<libc::thread_t>() as libc::vm_size_t,
                );
            }
            return;
        }

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(TEST_NAME)
            .arg("--nocapture")
            .env(CHILD_ENV, "1")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "a signal to every thread killed the child ({status}); wasmtime's \
             Mach-port trap handler aborts on MACH_RCV_INTERRUPTED"
        );
    }
    // -----------------------------------------------------------------
    // Declaration scanning and bounded discovery
    // -----------------------------------------------------------------

    /// Encode one component section (id + LEB128 length + contents).
    fn push_section(out: &mut Vec<u8>, id: u8, contents: &[u8]) {
        out.push(id);
        push_leb(out, contents.len() as u32);
        out.extend_from_slice(contents);
    }

    fn push_leb(out: &mut Vec<u8>, mut value: u32) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
    }

    /// A component external name: discriminator 0x00 + UTF-8 string.
    fn push_name(out: &mut Vec<u8>, name: &str) {
        out.push(0x00);
        push_leb(out, name.len() as u32);
        out.extend_from_slice(name.as_bytes());
    }

    /// A minimal component with the given import/export name list.
    ///
    /// It is intentionally not instantiable (the instance type index is
    /// dangling): the scanners read names, and discovery must fail closed on a
    /// component it cannot link. That is exactly the shape the install test
    /// needs before the 0.8.0 WIT lands.
    fn synthetic_component(imports: &[&str], exports: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"\0asm");
        out.extend_from_slice(&[0x0d, 0x00, 0x01, 0x00]);
        if !imports.is_empty() {
            let mut body = Vec::new();
            push_leb(&mut body, imports.len() as u32);
            for name in imports {
                push_name(&mut body, name);
                body.push(0x05); // instance
                push_leb(&mut body, 0); // type index
            }
            push_section(&mut out, 10, &body);
        }
        if !exports.is_empty() {
            let mut body = Vec::new();
            push_leb(&mut body, exports.len() as u32);
            for name in exports {
                push_name(&mut body, name);
                body.push(0x05); // instance
                push_leb(&mut body, 0); // type index
                body.push(0x00); // no optional type
            }
            push_section(&mut out, 11, &body);
        }
        out
    }

    /// A component exporting `oauth-api` and importing `provider-http` declares
    /// both `oauth` and `provider` — the import scan and the new export scan
    /// agree through the shared interface→grant-key map.
    #[test]
    fn declared_capabilities_scan_imports_and_exports_through_the_grant_map() {
        let dir = tempfile::tempdir().unwrap();
        let wasm = dir.path().join("plugin.wasm");
        std::fs::write(
            &wasm,
            synthetic_component(
                &["kiqi:plugin/provider-http@0.8.0"],
                &["kiqi:plugin/oauth-api@0.8.0"],
            ),
        )
        .unwrap();

        assert_eq!(
            declared_capabilities(&wasm).unwrap(),
            vec!["oauth".to_string(), "provider".to_string()]
        );
        assert_eq!(
            exported_interfaces(&wasm).unwrap(),
            vec!["oauth-api".to_string()]
        );
    }

    /// 0.7.1 imports keep declaring their own name as the grant key, and an
    /// export with no grant key is ignored.
    #[test]
    fn legacy_imports_and_render_exports_declare_their_keys() {
        let dir = tempfile::tempdir().unwrap();
        let wasm = dir.path().join("plugin.wasm");
        std::fs::write(
            &wasm,
            synthetic_component(
                &["kiqi:plugin/exec@0.7.1", "kiqi:plugin/fs@0.7.1"],
                &["kiqi:plugin/render-api@0.8.0", "kiqi:plugin/log@0.8.0"],
            ),
        )
        .unwrap();

        assert_eq!(
            declared_capabilities(&wasm).unwrap(),
            vec!["exec".to_string(), "fs".to_string(), "render".to_string()]
        );
        assert_eq!(
            exported_interfaces(&wasm).unwrap(),
            vec!["render-api".to_string()]
        );
    }

    /// Discovery is advisory and fail-closed: a component that cannot be
    /// compiled/linked produces a diagnostic and never an install failure.
    #[test]
    fn discovery_fails_closed_without_an_instantiable_component() {
        let dir = tempfile::tempdir().unwrap();
        let wasm = dir.path().join("plugin.wasm");
        std::fs::write(
            &wasm,
            synthetic_component(
                &["kiqi:plugin/provider-http@0.8.0"],
                &["kiqi:plugin/provider-api@0.8.0"],
            ),
        )
        .unwrap();

        let discovery = discover_providers(&wasm);
        assert!(discovery.providers.is_empty());
        assert!(
            !discovery.diagnostics.is_empty(),
            "a failed discovery must say why"
        );
    }

    /// No export means no probe at all: discovery stays off the install path
    /// for every component that is not oauth/provider-shaped.
    #[test]
    fn discovery_is_skipped_without_provider_exports() {
        let dir = tempfile::tempdir().unwrap();
        let wasm = dir.path().join("plugin.wasm");
        std::fs::write(
            &wasm,
            synthetic_component(&["kiqi:plugin/net-http@0.7.1"], &[]),
        )
        .unwrap();

        assert_eq!(discover_providers(&wasm), Discovery::default());
    }

    /// The provider-request grant is the delivery switch: absent, or recorded
    /// without `observe = requests`, means the guest is not called at all.
    #[test]
    fn provider_observation_is_denied_without_an_observe_grant() {
        let event = Event::new(
            kiqi_plugin::EventTag::BeforeProviderRequest,
            serde_json::json!({ "model": "m", "messages": [{ "role": "user" }] }),
        );

        let mut grants = Grants::default();
        assert!(graded_provider_request(&event, &grants).is_none());

        grants.provider_request = Some(kiqi_plugin::grants::ProviderRequestGrant::default());
        assert!(
            graded_provider_request(&event, &grants).is_none(),
            "a grant with observe = none delivers nothing"
        );
    }

    /// `messages` is opt-in; the default replaces it with `messageCount`, and
    /// `headers = names` keeps values out even if a future bridge sends them.
    #[test]
    fn provider_observation_strips_messages_and_header_values() {
        let event = Event::new(
            kiqi_plugin::EventTag::BeforeProviderRequest,
            serde_json::json!({
                "model": "m",
                "apiKeyPresent": true,
                "headerNames": ["authorization"],
                "headerValues": { "authorization": "secret" },
                "messages": [{ "role": "user" }, { "role": "assistant" }],
            }),
        );
        let mut grants = Grants::default();
        grants.provider_request = Some(kiqi_plugin::grants::ProviderRequestGrant {
            observe: ObserveMode::Requests,
            ..kiqi_plugin::grants::ProviderRequestGrant::default()
        });

        let Some(WitEvent::BeforeProviderRequest(payload)) =
            graded_provider_request(&event, &grants)
        else {
            panic!("the observe grant must deliver");
        };
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["messageCount"], 2);
        assert!(payload.get("messages").is_none(), "messages are opt-in");
        assert!(payload.get("headerValues").is_none(), "values are opt-in");
        assert_eq!(payload["headerNames"][0], "authorization");
        assert_eq!(payload["model"], "m");
        assert_eq!(payload["apiKeyPresent"], true);

        // Both explicit opt-ins deliver the full view.
        grants.provider_request = Some(kiqi_plugin::grants::ProviderRequestGrant {
            observe: ObserveMode::Requests,
            messages: MessagesMode::Full,
            headers: HeadersMode::Values,
            ..kiqi_plugin::grants::ProviderRequestGrant::default()
        });
        let Some(WitEvent::BeforeProviderRequest(payload)) =
            graded_provider_request(&event, &grants)
        else {
            panic!("the observe grant must deliver");
        };
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["messages"].as_array().unwrap().len(), 2);
        assert_eq!(payload["headerValues"]["authorization"], "secret");
        assert_eq!(payload["messageCount"], 2);
    }

    /// A payload without messages still reports a zero count rather than
    /// omitting the field.
    #[test]
    fn provider_observation_always_reports_a_message_count() {
        let event = Event::new(
            kiqi_plugin::EventTag::BeforeProviderRequest,
            serde_json::json!({ "model": "m" }),
        );
        let mut grants = Grants::default();
        grants.provider_request = Some(kiqi_plugin::grants::ProviderRequestGrant {
            observe: ObserveMode::Requests,
            ..kiqi_plugin::grants::ProviderRequestGrant::default()
        });
        let Some(WitEvent::BeforeProviderRequest(payload)) =
            graded_provider_request(&event, &grants)
        else {
            panic!("the observe grant must deliver");
        };
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["messageCount"], 0);
    }
}
