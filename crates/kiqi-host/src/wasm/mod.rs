//! Host-side backend for **wasm extensions** (the component model tier).
//!
//! Binds `extensions/kiqi-extension.wit` (ABI 0.8.0) with wasmtime's `bindgen!` and
//! hosts an extension component behind the same [`crate::Extension`] seam the
//! compiled-in extensions implement.
//!
//! Two deliberate choices:
//!
//! * **The seam stays sync.** WASI 0.2 semantics are synchronous (a guest blocks
//!   on `wasi:io/poll`), so the generated guest calls are sync and no async
//!   runtime is needed. When a host import eventually needs real async I/O, it
//!   blocks inside its own implementation — the async stays here, not in the
//!   extension trait.
//! * **The host provides every interface.** The component model requires each
//!   import a component declares to be satisfied at instantiation, so the host
//!   implements all of them: `log` writes, `progress` forwards to the running
//!   call's sink, `session-read` returns placeholders, and every capability that
//!   is not granted is a **deny stub** returning `denied`. A component only
//!   imports the interfaces it uses, so an extension that never touches `exec` never
//!   asks for it.
//!
//! `WasmExtension::unload` (and `Drop`, and [`crate::Extension::shutdown`])
//! releases the instance so `/reload` can swap an extension without leaking linear
//! memory.

wasmtime::component::bindgen!({
    path: "../../extensions/kiqi-extension.wit",
    world: "extension",
});

use std::cell::RefCell;
use std::path::Path;
use std::sync::Mutex;

use crate::grants::{
    resolve_provider_request_policy, HeadersMode, MessagesMode, ObserveMode, ProviderRequestPolicy,
};
use crate::{
    Block, CommandSpec, Event, EventAction, Extension as ExtensionTrait, ExtensionError, FlagSpec,
    HostCaps, OAuthAction, OAuthCredentialStatus, OAuthInteraction, OAuthProvider, OAuthResponse,
    Progress, ProgressSink, ShortcutSpec, ToolOutcome, ToolSpec, Verdict,
};
use serde_json::Value;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::wasm::kiqi::extension::types::{
    CapabilityError, ContentBlock, Event as WitEvent, EventAction as WitEventAction, InputEvent,
    InputKind, LogLevel, OauthAction as WitOauthAction, OauthCredentialStatus,
    OauthProvider as WitOauthProvider, OauthResponse as WitOauthResponse, OauthTokenOutcome,
    OtherEvent, ProgressBlock, ProviderDelta as WitDelta, ToolCall, ToolResult, ToolResultEvent,
    Verdict as WitVerdict,
};
use crate::wasm::Extension as World;

pub mod oauth;
pub use oauth::{
    default_transport, ensure_oauth_profile_matches, exchange_authorization_code,
    oauth_profile_fingerprint, poll_device_token, refresh_stored_token,
    request_device_authorization, revoke_stored_token, DeviceAuthorization,
    ExtensionCredentialSink, HttpOAuthTransport, HttpResponse, OAuthRequestBudget, OAuthTokens,
    OAuthTransport, StoredOAuthToken,
};

mod provider_patch;
pub use provider_patch::{
    validate_provider_patch, DENIED_HEADERS, MAX_ENVELOPE_BYTES, NON_PATCHABLE_KEYS,
};

pub mod provider_host;
pub mod provider_http;
pub use provider_host::{
    abandoned_worker_count, DeltaKind, DeltaPhase, ProviderDeltaEvent, ProviderHost,
    ProviderHostOptions, ProviderResponse, ProviderStreamSegment, PROVIDER_MAX_ABANDONED_WORKERS,
};
pub use provider_http::{
    fingerprint_credential, ProviderHttpError, ProviderHttpRequest, ProviderHttpStream,
    ProviderHttpTransport, ReqwestTransport, ResolvedCredential, STRIPPED_HEADERS,
};

/// The generated world struct is named after the world (`extension`); alias it so
/// it does not collide with the `crate::Extension` trait.
const NOT_GRANTED: &str = "capability not granted";

/// Which kind of entry point is running on this thread.
///
/// The kind is what makes a contribution channel call-scoped rather than
/// extension-scoped: `provider-stream.emit` is honored only for
/// [`CallKind::ProviderRequest`], as `event-action.patch` is honored only for
/// the provider gate. `Metadata` is the implicit answer outside a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallKind {
    Metadata,
    Tool,
    Event,
    Command,
    Probe,
    PromptHook,
    ProviderRequest,
    Render,
    Oauth,
}

/// What the call running on this thread may reach.
///
/// The host imports run on the thread that drives the guest call, and
/// [`WasmExtension`] serializes calls behind a mutex, so one thread-local carries
/// the running call's capabilities, progress sink and delta sink. The pointers
/// only need to live for the duration of that call.
#[derive(Clone, Copy)]
struct CallContext {
    sink: Option<*const (dyn ProgressSink + 'static)>,
    caps: *const HostCaps<'static>,
    kind: CallKind,
    /// Set only for a provider request; `provider-stream.emit` writes here.
    deltas: Option<*mut crate::wasm::provider_host::DeltaAssembler>,
}

thread_local! {
    static CURRENT: RefCell<Option<CallContext>> = const { RefCell::new(None) };
}

/// Install the current call's context.
///
/// # Safety
/// The erased lifetimes are only valid while the borrows the pointers came from
/// are alive; [`clear_context`] must run before either ends.
/// Install the current call's context, including its kind and (for a provider
/// request) the delta assembler the call owns.
///
/// # Safety
/// The erased pointers are valid only while the borrows they came from are
/// alive; [`clear_context`] must run before either ends.
fn install_context_kind(
    sink: Option<&dyn ProgressSink>,
    caps: &HostCaps<'_>,
    kind: CallKind,
    deltas: Option<&mut crate::wasm::provider_host::DeltaAssembler>,
) -> CallContext {
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
        kind,
        // SAFETY: as above.
        deltas: deltas.map(|deltas| deltas as *mut crate::wasm::provider_host::DeltaAssembler),
    };
    CURRENT.with(|slot| *slot.borrow_mut() = Some(context));
    context
}

/// The kind of call running on this thread; `Metadata` outside a call.
fn current_kind() -> CallKind {
    CURRENT.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|context| context.kind)
            .unwrap_or(CallKind::Metadata)
    })
}

/// Push one delta to the running provider call's assembler. A no-op for every
/// other call kind, which is what keeps `provider-stream` from being a general
/// side channel out of a tool or event handler.
fn emit_provider_delta(delta: WitDelta) {
    if current_kind() != CallKind::ProviderRequest {
        return;
    }
    CURRENT.with(|slot| {
        if let Some(pointer) = slot.borrow().as_ref().and_then(|context| context.deltas) {
            // SAFETY: `install_context_kind` set this for the call; it is
            // cleared before the borrow ends.
            let assembler = unsafe { &mut *pointer };
            assembler.push(delta);
        }
    });
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

fn with_session<R>(f: impl FnOnce(&dyn crate::SessionRead) -> R) -> Option<R> {
    with_caps_context(|caps| caps.session.map(f)).flatten()
}

fn with_control<R>(f: impl FnOnce(&dyn crate::SessionControl) -> R) -> Option<R> {
    with_caps_context(|caps| caps.control.map(f)).flatten()
}

fn with_inject<R>(f: impl FnOnce(&dyn crate::InjectApi) -> R) -> Option<R> {
    with_caps_context(|caps| caps.inject.map(f)).flatten()
}

fn with_ui<R>(f: impl FnOnce(&dyn crate::UiApi) -> R) -> Option<R> {
    with_caps_context(|caps| caps.ui.map(f)).flatten()
}

/// Collapse a capability call into a `capability-error` on any failure.
fn forward(result: Option<Result<(), ExtensionError>>) -> Result<(), CapabilityError> {
    match result {
        Some(Ok(())) => Ok(()),
        Some(Err(error)) => Err(capability_error(error)),
        None => Err(denied()),
    }
}

/// A granted capability whose *operation* failed is `failed`, not `denied`.
fn capability_error(error: ExtensionError) -> CapabilityError {
    match error {
        ExtensionError::Denied(reason) => CapabilityError::Denied(reason),
        other => CapabilityError::Failed(other.to_string()),
    }
}

fn denied() -> CapabilityError {
    CapabilityError::Denied(NOT_GRANTED.to_string())
}

/// What the user authorized for one extension, shared with the CLI through the
/// contract crate (`kiqi-extension`).
pub use crate::grants::{
    allows, ExecCwd, ExecGrant, FsGrant, Grants, NetHttpGrant, NetSocketsGrant, ProcGrant,
    RenderGrant, RENDER_KIND_MARKDOWN, RENDER_MAX_OUTPUT_BYTES_MAX, RENDER_TIMEOUT_MS_MAX,
};

/// The user-grantable capabilities a component declares: the `kiqi:extension/*`
/// interfaces that need authorization.
///
/// Imports and exports both count. An import names a channel the guest reaches
/// for (`net-http`, `fs`, `exec`, `proc`, `net-sockets`, `provider-http`,
/// `oauth-http`, `provider-request`); an export names a surface it offers the
/// host (`oauth-api`, `provider-api`, `render-api`).
/// [`crate::grants::grant_key_for_interface`] turns either spelling into
/// its grant key, so the scanner and the grants model can never disagree
/// (`provider-http`/`provider-stream` → `provider`, `oauth-http` → `oauth`, …).
///
/// The domain interfaces (`log`, `session-read`, …) are always available through
/// `HostCaps` and are not part of the grant set, so they are filtered out. This
/// is what `ki extension install` shows the user and re-prompts about when it grows.
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
/// capability-as-interface convention (`kiqi:extension/exec@0.7.1` → `exec`), which
/// is what every 0.7.1 component imports.
fn note_declared(name: &str, caps: &mut Vec<String>) {
    let key = crate::grants::grant_key_for_interface(name).or_else(|| {
        let bare = name.strip_prefix("kiqi:extension/").unwrap_or(name);
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
                    .strip_prefix("kiqi:extension/")
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
/// what the host can honour. `ki extension install` names any difference, so a user
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
/// traps instead of hanging `ki extension install`.
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
        .name("kiqi-extension-discovery".to_string())
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
        format!("kiqi:extension/{interface}@0.8.0"),
        format!("kiqi:extension/{interface}"),
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

/// The bounded load-time `oauth-api.list-providers` cross-check.
///
/// The call runs on its own thread so a guest that spins cannot hang
/// `WasmExtension::load`: the caller waits at most `timeout`. A trap or an
/// over-cap answer is reported and treated as "no narrowing" (`None`); only a
/// call that never returns loses the instance, which aborts the load. The
/// grant stays the sole catalog authority — this can only take ids away.
fn bounded_oauth_provider_ids(
    mut store: Store<HostState>,
    guest: World,
    timeout: std::time::Duration,
) -> Result<
    (
        Option<std::collections::BTreeSet<String>>,
        Store<HostState>,
        World,
    ),
    String,
> {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("kiqi-extension-oauth-probe".to_string())
        .spawn(move || {
            let result = guest
                .kiqi_extension_oauth_api()
                .call_list_providers(&mut store);
            // The receiver may be gone on timeout; the store still drops here.
            let _ = tx.send((result, store, guest));
        });
    if let Err(error) = spawned {
        return Err(format!("cannot start the oauth probe thread: {error}"));
    }
    let (result, mut store, guest) = rx.recv_timeout(timeout).map_err(|_| {
        format!(
            "oauth list-providers did not finish within {}s",
            timeout.as_secs()
        )
    })?;
    store.data_mut().reap_call_scoped();
    let ids = match result {
        Ok(providers) => match bounded_oauth_ids(providers) {
            Ok(ids) => Some(ids),
            Err(error) => {
                tracing::warn!(%error, "oauth list-providers over cap; the grant is not narrowed");
                None
            }
        },
        Err(error) => {
            tracing::warn!(%error, "oauth list-providers trapped; the grant is not narrowed");
            None
        }
    };
    Ok((ids, store, guest))
}

/// Apply the install-time discovery caps to a `list-providers` answer.
fn bounded_oauth_ids(
    providers: Vec<WitOauthProvider>,
) -> Result<std::collections::BTreeSet<String>, String> {
    if providers.len() > DISCOVERY_MAX_ENTRIES {
        return Err(format!(
            "returned {} providers (cap {DISCOVERY_MAX_ENTRIES})",
            providers.len()
        ));
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut bytes = 0usize;
    for provider in providers {
        if provider.id.len() > DISCOVERY_MAX_STRING_BYTES
            || provider.display_name.len() > DISCOVERY_MAX_STRING_BYTES
        {
            return Err(format!(
                "a provider id/display name is over {DISCOVERY_MAX_STRING_BYTES} bytes"
            ));
        }
        bytes += provider.id.len() + provider.display_name.len();
        if bytes > DISCOVERY_MAX_BYTES {
            return Err(format!("provider list is over {DISCOVERY_MAX_BYTES} bytes"));
        }
        ids.insert(provider.id);
    }
    Ok(ids)
}

/// The extension-side `oauth-action` → the WIT variant.
fn to_wit_oauth_action(action: OAuthAction) -> WitOauthAction {
    match action {
        OAuthAction::Begin(challenge) => WitOauthAction::Begin(challenge),
        OAuthAction::Poll => WitOauthAction::Poll,
        OAuthAction::Exchange(code) => WitOauthAction::Exchange(code),
        OAuthAction::Cancel => WitOauthAction::Cancel,
    }
}

/// The WIT `oauth-response` → the extension-side type (no token material exists on
/// either side; only the expiry crosses).
fn to_extension_oauth_response(response: WitOauthResponse) -> OAuthResponse {
    match response {
        WitOauthResponse::Interaction(interaction) => {
            OAuthResponse::Interaction(OAuthInteraction {
                title: interaction.title,
                url: interaction.url,
                instructions: interaction.instructions,
                user_code: interaction.user_code,
            })
        }
        WitOauthResponse::Stored(status) => OAuthResponse::Stored(OAuthCredentialStatus {
            expires_at: status.expires_at,
        }),
        WitOauthResponse::AuthorizationPending => OAuthResponse::AuthorizationPending,
        WitOauthResponse::SlowDown => OAuthResponse::SlowDown,
        WitOauthResponse::Denied(message) => OAuthResponse::Denied(message),
        WitOauthResponse::Error(message) => OAuthResponse::Error(message),
    }
}

/// State every instantiated component gets: WASI plus the resource table and
/// the grants the guest's host imports are allowed to act on.
struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    grants: Grants,
    /// The install id this instance was loaded under. Kept so host-side
    /// features that must name the extension (diagnostics, declaration scope,
    /// credential namespacing) can do so without re-reading the file path.
    /// P4 wires the credential sink; until then nothing reads it here.
    #[allow(dead_code)]
    extension_id: Option<String>,
    /// The flag names THIS extension declared in `list-flags`, captured once at
    /// instantiation. `session-read.cli-flag` is declaration-scoped: a name
    /// that is not in this set returns `None` even when another extension (or the
    /// CLI) knows the flag. That stops one installed component from reading
    /// another's configuration through a shared flag namespace.
    declared_flags: std::sync::Arc<std::collections::BTreeSet<String>>,
    /// The extension's own data directory (`extensions/<id>/data/`). The `fs`
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
    /// Host-attributed OAuth: the flow budget plus the injected transport and
    /// credential sink. `grants.oauth` still decides *which* provider may be
    /// driven; this only carries the machinery and the allow-list's counters.
    oauth: OAuthState,
    /// Open `provider-http` streams, scoped to one provider call.
    provider_http: provider_http::ProviderHttpTable,
    /// The grant-backed provider call, installed only for the duration of a
    /// `provider-api.request`. `None` everywhere else (the tool, render and
    /// OAuth instances), which is why those can never open a handle.
    provider_call: Option<provider_http::ProviderCallState>,
}

/// The OAuth machinery one instance carries.
///
/// The budget lives per instance so a flow cannot outlive the extension that
/// started it, and `sink` is the only route from a guest exchange to disk.
struct OAuthState {
    budget: OAuthRequestBudget,
    transport: Option<std::sync::Arc<dyn OAuthTransport>>,
    sink: Option<std::sync::Arc<dyn ExtensionCredentialSink>>,
}

impl Default for OAuthState {
    fn default() -> Self {
        Self {
            budget: OAuthRequestBudget::default(),
            transport: None,
            sink: None,
        }
    }
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
impl crate::wasm::kiqi::extension::types::Host for HostState {}

impl crate::wasm::kiqi::extension::log::Host for HostState {
    fn write(&mut self, _level: LogLevel, message: String) {
        eprintln!("[kiqi-extension] {message}");
    }
}

/// Read-only session state, forwarded to the call's `HostCaps::session`.
impl crate::wasm::kiqi::extension::session_read::Host for HostState {
    fn info(&mut self) -> crate::wasm::kiqi::extension::types::SessionInfo {
        crate::wasm::kiqi::extension::types::SessionInfo {
            session_id: with_session(|session| session.session_id())
                .flatten()
                .unwrap_or_default(),
            run_id: String::new(),
            // Only a call with a host context may learn the working directory.
            // A render-only instance never installs one (it is deny-all), and
            // std::env::current_dir() here would otherwise hand the sandbox the
            // host's absolute path.
            cwd: with_caps_context(|_| {
                std::env::current_dir()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
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
    fn cli_flag(&mut self, name: String) -> Option<crate::wasm::kiqi::extension::types::FlagValue> {
        use crate::wasm::kiqi::extension::types::FlagValue as Wit;
        // Declaration-scoped: the calling extension may only read a flag it
        // declared itself. This is the default (no grant), so an install-tree
        // extension cannot probe the flags of its neighbours.
        if !self.declared_flags.contains(&name) {
            return None;
        }
        with_session(|session| session.cli_flag(&name))
            .flatten()
            .map(|value| match value {
                crate::FlagValue::Bare => Wit::Bare,
                crate::FlagValue::Text(text) => Wit::Text(text),
            })
    }
    fn session_branch(&mut self) -> String {
        with_session(|session| session.session_branch().to_string())
            .unwrap_or_else(|| "null".to_string())
    }
}

impl crate::wasm::kiqi::extension::progress::Host for HostState {
    fn emit(&mut self, block: ProgressBlock) {
        emit_progress(match block {
            ProgressBlock::Text(text) => Progress::Text(text),
            ProgressBlock::Json(json) => Progress::Json(
                serde_json::from_str(&json).unwrap_or(serde_json::Value::String(json)),
            ),
        });
    }
}

/// Streamed provider deltas. Honored only while a `provider-api.request` call
/// is on the stack; outside one this is a no-op by contract, so a tool or an
/// event handler cannot smuggle deltas into another call's stream.
impl crate::wasm::kiqi::extension::provider_stream::Host for HostState {
    fn emit(&mut self, delta: WitDelta) {
        emit_provider_delta(delta);
    }
    fn emit_batch(&mut self, deltas: Vec<WitDelta>) {
        for delta in deltas {
            emit_provider_delta(delta);
        }
    }
}

/// Host-attributed OAuth endpoints. Deny-by-default: a provider id is reachable
/// only when the `oauth` grant names exactly that profile.
impl crate::wasm::kiqi::extension::oauth_http::Host for HostState {
    fn token_request(
        &mut self,
        provider_id: String,
        form: String,
    ) -> Result<OauthTokenOutcome, CapabilityError> {
        self.oauth_token_request(&provider_id, &form)
    }
    fn revoke(&mut self, provider_id: String) -> Result<(), CapabilityError> {
        self.oauth_revoke(&provider_id)
    }
}

/// Host-attributed streaming HTTP for provider requests.
///
/// Deny-by-default twice over: the call must be a `provider-api.request` (the
/// thread-local call kind), and that call must have installed a grant-backed
/// [`HostState::provider_call`]. The tool and OAuth instances satisfy neither,
/// so they can never hold a handle.
impl crate::wasm::kiqi::extension::provider_http::Host for HostState {
    fn open(&mut self, request: String) -> Result<u32, CapabilityError> {
        self.provider_http_open(&request)
            .map_err(provider_http::ProviderHttpError::into_capability)
    }
    fn read(&mut self, handle: u32, max_bytes: u64) -> Result<Option<Vec<u8>>, CapabilityError> {
        self.provider_http_read(handle, max_bytes)
            .map_err(provider_http::ProviderHttpError::into_capability)
    }
    fn close(&mut self, handle: u32) -> Result<(), CapabilityError> {
        self.provider_http_close(handle)
            .map_err(provider_http::ProviderHttpError::into_capability)
    }
}

impl HostState {
    /// `provider-http.open`: authorize the request against the grant, then let
    /// the transport connect. A policy failure never makes a connection.
    fn provider_http_open(&mut self, raw: &str) -> Result<u32, provider_http::ProviderHttpError> {
        use provider_http::ProviderHttpError;
        if current_kind() != CallKind::ProviderRequest {
            return Err(ProviderHttpError::Denied(
                "provider-http is only available during a provider request".to_string(),
            ));
        }
        let Some(call) = self.provider_call.as_ref() else {
            return Err(ProviderHttpError::Denied(
                "provider-http is not granted".to_string(),
            ));
        };
        let request = provider_http::checked_request(&call.profile, call.credential.as_ref(), raw)?;
        let max_handles = call.profile.max_handles;
        let stream = call
            .transport
            .open(request)
            .map_err(ProviderHttpError::Failed)?;
        let status = stream.status();
        if (300..400).contains(&status) {
            return Err(ProviderHttpError::Denied(format!(
                "provider-http refused a {status} redirect"
            )));
        }
        self.provider_http.open(stream, max_handles)
    }

    /// `provider-http.read`: bounded by the per-call table, the response cap
    /// and the grant's cancellation probe.
    fn provider_http_read(
        &mut self,
        handle: u32,
        max_bytes: u64,
    ) -> Result<Option<Vec<u8>>, provider_http::ProviderHttpError> {
        use provider_http::ProviderHttpError;
        if current_kind() != CallKind::ProviderRequest {
            return Err(ProviderHttpError::Denied(
                "provider-http is only available during a provider request".to_string(),
            ));
        }
        let Some(call) = self.provider_call.as_ref() else {
            return Err(ProviderHttpError::Denied(
                "provider-http is not granted".to_string(),
            ));
        };
        let max_response_bytes = call.profile.max_response_bytes;
        let cancel = call.cancel.clone();
        self.provider_http
            .read(handle, max_bytes, max_response_bytes, cancel.as_deref())
    }

    fn provider_http_close(&mut self, handle: u32) -> Result<(), provider_http::ProviderHttpError> {
        use provider_http::ProviderHttpError;
        if current_kind() != CallKind::ProviderRequest {
            return Err(ProviderHttpError::Denied(
                "provider-http is only available during a provider request".to_string(),
            ));
        }
        self.provider_http.close(handle)
    }
}

/// Session mutations, forwarded to the call's `HostCaps::control`; a call
/// without that capability gets `denied`.
impl crate::wasm::kiqi::extension::session_control::Host for HostState {
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
impl crate::wasm::kiqi::extension::inject::Host for HostState {
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
impl crate::wasm::kiqi::extension::ui::Host for HostState {
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
/// able to make the *host* allocate without bound; an extension that genuinely needs
/// a larger document should stream it through `net-sockets`.
const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;

/// Host-mediated HTTP: the request's host must be in the grant's allow-list.
impl crate::wasm::kiqi::extension::net_http::Host for HostState {
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
            // bounce the extension somewhere it was never allowed to go.
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
    /// The exact granted profile, or a policy refusal.
    ///
    /// Provider ids are matched exactly: there is no wildcard, no prefix and no
    /// case folding, because the grant is what stops one extension from driving
    /// another extension's IdP profile.
    fn oauth_profile(
        &self,
        provider_id: &str,
    ) -> Result<crate::grants::OAuthProviderProfile, CapabilityError> {
        let grant = self
            .grants
            .oauth
            .as_ref()
            .ok_or_else(|| CapabilityError::Denied("oauth is not granted".to_string()))?;
        grant
            .providers
            .iter()
            .find(|profile| profile.id == provider_id)
            .cloned()
            .ok_or_else(|| {
                CapabilityError::Denied(format!("oauth provider `{provider_id}` is not granted"))
            })
    }

    /// The install id inside the namespaced credential key.
    fn oauth_install_id(&self) -> Result<String, CapabilityError> {
        self.extension_id.clone().ok_or_else(|| {
            CapabilityError::Denied("this extension instance has no install id".to_string())
        })
    }

    fn oauth_transport(&self) -> std::sync::Arc<dyn OAuthTransport> {
        self.oauth
            .transport
            .clone()
            .unwrap_or_else(oauth::default_transport)
    }

    /// `oauth-http.token-request`: exchange an authorization code, or poll a
    /// device code the host itself issued.
    ///
    /// Grant/form violations are `denied`. A transport, HTTP or parse failure is
    /// `failed(...)` with a message that never contains the response body. A
    /// success returns only the expiry the guest is allowed to see.
    fn oauth_token_request(
        &mut self,
        provider_id: &str,
        form: &str,
    ) -> Result<OauthTokenOutcome, CapabilityError> {
        let profile = self.oauth_profile(provider_id)?;
        let install_id = self.oauth_install_id()?;
        let profile_fingerprint =
            oauth::oauth_profile_fingerprint(&profile).map_err(CapabilityError::Failed)?;
        let form = oauth::parse_token_form(form).map_err(CapabilityError::Denied)?;
        let mut form =
            oauth::enforce_token_form_grant(&profile, form).map_err(CapabilityError::Denied)?;
        let endpoint =
            oauth::granted_endpoint(&profile.token_endpoint).map_err(CapabilityError::Failed)?;
        // The client id is the grant's, never the guest's.
        form.push(("client_id".to_string(), profile.client_id.clone()));
        let transport = self.oauth_transport();
        let timeout = {
            self.oauth
                .budget
                .charge()
                .map_err(CapabilityError::Failed)?;
            self.oauth.budget.request_timeout()
        };
        let response = transport
            .post_form(endpoint.as_str(), &form, timeout)
            .map_err(CapabilityError::Failed)?;
        if !(200..300).contains(&response.status) {
            // A 3xx is a request to a host the grant never named. Never follow
            // it, and say which host was refused when it parses.
            if (300..400).contains(&response.status) {
                let host = response
                    .location
                    .as_deref()
                    .and_then(|location| reqwest::Url::parse(location).ok())
                    .and_then(|url| url.host_str().map(str::to_string));
                return Ok(OauthTokenOutcome::Failed(match host {
                    Some(host) => format!(
                        "token endpoint redirected to `{host}`; redirects to another host are refused"
                    ),
                    None => "token endpoint returned a redirect; redirects are refused".to_string(),
                }));
            }
            return Ok(OauthTokenOutcome::Failed(format!(
                "token endpoint returned HTTP {}",
                response.status
            )));
        }
        let tokens = match oauth::parse_token_response(&response.body, unix_now()) {
            Ok(tokens) => tokens,
            Err(error) => return Ok(OauthTokenOutcome::Failed(error)),
        };
        let Some(sink) = self.oauth.sink.clone() else {
            // No sink means no disk path: say so instead of pretending success.
            return Ok(OauthTokenOutcome::Failed(
                "OAuth credential store is not configured".to_string(),
            ));
        };
        if let Err(error) = sink.store_extension_oauth(
            &install_id,
            provider_id,
            &tokens.access,
            &tokens.refresh,
            tokens.expires,
            &profile_fingerprint,
        ) {
            return Ok(OauthTokenOutcome::Failed(format!(
                "credential store: {error}"
            )));
        }
        // The flow is complete; a later re-login starts a fresh budget.
        self.oauth.budget.reset();
        Ok(OauthTokenOutcome::Stored(OauthCredentialStatus {
            expires_at: tokens.expires,
        }))
    }

    /// `oauth-http.revoke`: revoke the token the host stored, then drop it.
    ///
    /// Host-only in effect: the guest can ask for the revocation, but never
    /// sees or supplies the token.
    fn oauth_revoke(&mut self, provider_id: &str) -> Result<(), CapabilityError> {
        let profile = self.oauth_profile(provider_id)?;
        let install_id = self.oauth_install_id()?;
        let endpoint_text = profile.revocation_endpoint.as_deref().ok_or_else(|| {
            CapabilityError::Failed(format!(
                "oauth provider `{provider_id}` has no revocation-endpoint"
            ))
        })?;
        let endpoint = oauth::granted_endpoint(endpoint_text).map_err(CapabilityError::Failed)?;
        let sink = self.oauth.sink.clone().ok_or_else(|| {
            CapabilityError::Failed("OAuth credential store is not configured".to_string())
        })?;
        let stored = sink
            .read_extension_oauth(&install_id, provider_id)
            .map_err(CapabilityError::Failed)?
            .ok_or_else(|| {
                CapabilityError::Failed("no stored OAuth credential for this provider".to_string())
            })?;
        oauth::ensure_oauth_profile_matches(&profile, &stored).map_err(CapabilityError::Denied)?;
        let (hint, value) = if stored.refresh.is_empty() {
            ("access_token", stored.access)
        } else {
            ("refresh_token", stored.refresh)
        };
        let transport = self.oauth_transport();
        let timeout = {
            self.oauth
                .budget
                .charge()
                .map_err(CapabilityError::Failed)?;
            self.oauth.budget.request_timeout()
        };
        let form = vec![
            ("token".to_string(), value),
            ("token_type_hint".to_string(), hint.to_string()),
            ("client_id".to_string(), profile.client_id.clone()),
        ];
        let response = transport
            .post_form(endpoint.as_str(), &form, timeout)
            .map_err(CapabilityError::Failed)?;
        if !(200..300).contains(&response.status) {
            return Err(CapabilityError::Failed(format!(
                "revocation endpoint returned HTTP {}",
                response.status
            )));
        }
        sink.delete_extension_oauth(&install_id, provider_id)
            .map_err(CapabilityError::Failed)?;
        self.oauth.budget.reset();
        Ok(())
    }

    /// Resolve a guest-supplied relative path inside the fs grant's scope, or
    /// explain why it may not: `extension-data` jails to the extension's own data
    /// directory, `project` to the project tree (same scope `exec`/`proc`
    /// use).
    ///
    /// A path with any `..`, root or prefix component is refused outright, so a
    /// extension cannot walk out of its jail — and `starts_with` is checked as well
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
            crate::grants::ExecCwd::ExtensionData => (self.data_dir()?, "the extension directory"),
            crate::grants::ExecCwd::Project => (self.project_dir()?, "the project directory"),
        };
        let resolved = jail_under(&root, rel, what)?;
        symlink_checked_under(&root, &resolved, create_parents, what)
    }

    /// Resolve a relative path inside the extension's data directory without any
    /// capability check — the lexical half of the jail, exposed to tests so each
    /// guard can be asserted in isolation. Production paths go through
    /// [`fs_checked`](Self::fs_checked) or [`scoped_cwd`](Self::scoped_cwd), which
    /// combine both halves; without this attribute the method is dead code in every
    /// non-test build (which is what the compiler was reporting).
    #[cfg(test)]
    fn jail(&self, rel: &str) -> Result<std::path::PathBuf, CapabilityError> {
        jail_under(&self.data_dir()?, rel, "the extension directory")
    }

    /// Resolve the working directory `exec` and `proc` share: the grant's
    /// scope decides the root (`extension-data` = the extension's sandbox,
    /// `project` = the user's tree), and the guest-supplied path gets the same
    /// lexical + symlink checks `fs` uses. The data directory is created on
    /// demand — it is the extension's own sandbox; the project directory is the
    /// user's, so a misspelled cwd fails there instead of `mkdir -p`-ing into
    /// it.
    fn scoped_cwd(
        &self,
        scope: crate::grants::ExecCwd,
        cwd: Option<String>,
    ) -> Result<std::path::PathBuf, CapabilityError> {
        let (root, what) = match scope {
            crate::grants::ExecCwd::ExtensionData => (self.data_dir()?, "the extension directory"),
            crate::grants::ExecCwd::Project => (self.project_dir()?, "the project directory"),
        };
        let sandboxed = matches!(scope, crate::grants::ExecCwd::ExtensionData);
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

    /// Every handle scoped to the current call, not just `proc`.
    ///
    /// Entry points that call [`Self::reap_procs`] call this instead, so the
    /// provider-http table cannot outlive the call that opened a stream.
    fn reap_call_scoped(&mut self) {
        self.reap_procs();
        self.reap_provider_http();
    }

    /// Drop every open `provider-http` handle. Called at entry and exit of
    /// every guest-visible entry point and on unload.
    fn reap_provider_http(&mut self) {
        self.provider_http.reap();
    }

    fn data_dir(&self) -> Result<std::path::PathBuf, CapabilityError> {
        self.fs_root.clone().ok_or_else(|| {
            CapabilityError::Failed("this extension has no data directory".to_string())
        })
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
    /// The symlink half of the jail for the extension's data directory; see [`jail`]
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
            "the extension directory",
        )
    }
}

/// Resolve a guest-supplied relative path inside `root`, or explain why it may
/// not.
///
/// A path with any `..`, root or prefix component is refused outright, so a
/// extension cannot walk out of its jail — and `starts_with` is checked as well
/// rather than relied on. `what` names the jail in errors (`the extension
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

/// Extension filesystem, jailed to `extensions/<id>/data/`.
impl crate::wasm::kiqi::extension::fs::Host for HostState {
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
impl crate::wasm::kiqi::extension::net_sockets::Host for HostState {
    fn connect(&mut self, address: String) -> Result<u32, CapabilityError> {
        let (host, port) = split_host_port(&address)?;
        if !self.grants.allows_socket_host(&host) {
            return Err(CapabilityError::Denied(format!(
                "net-sockets to `{host}` is not granted"
            )));
        }
        // Bound the live handles the way `proc` bounds its children: unbounded
        // they let a looping guest exhaust the host process's file
        // descriptors, which breaks every other extension's fs/exec too.
        if self.sockets.len() >= MAX_OPEN_SOCKETS {
            return Err(CapabilityError::Failed(format!(
                "net-sockets: {MAX_OPEN_SOCKETS} sockets are already open on this extension; close one first"
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
impl crate::wasm::kiqi::extension::proc::Host for HostState {
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
        if !crate::grants::allows(&grant.commands, &cmd) {
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
        // else caps this wait so an extension can report its own timeout.
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
/// The cwd scope rides with the grant: `extension-data` jails the child's working
/// directory to `extensions/<id>/data/`; `project` jails it to the project
/// directory (for diagnostic extensions that run `cargo`/`clippy`). Note the jail
/// bounds where the child *starts*, not what it can reach: the child inherits
/// the user's privileges and can read or write anywhere it can path to. The
/// scope exists so an extension cannot *accidentally* operate on the wrong tree
/// and so the grant's intent stays legible — not to sandbox the child.
impl crate::wasm::kiqi::extension::exec::Host for HostState {
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
            "[kiqi-extension] proc grant timeout 0s is not a usable budget; using 1s \
             (the parse layer rejects 0, so only an in-code grant can say it)"
        );
        return std::time::Duration::from_secs(1);
    }
    let clamped = timeout_seconds.min(crate::grants::PROC_TIMEOUT_MAX_SECONDS);
    if clamped != timeout_seconds {
        eprintln!(
            "[kiqi-extension] proc grant timeout {timeout_seconds}s exceeds the {clamped}s cap; \
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
    let clamped = max_open.min(crate::grants::PROC_MAX_OPEN_MAX);
    if clamped != max_open {
        eprintln!(
            "[kiqi-extension] proc grant max-open {max_open} exceeds the {clamped} cap; \
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
            "[kiqi-extension] exec grant timeout 0s is not a usable budget; using 1s \
             (the parse layer rejects 0, so only an in-code grant can say it)"
        );
        return std::time::Duration::from_secs(1);
    }
    let clamped = timeout_seconds.min(crate::grants::EXEC_TIMEOUT_MAX_SECONDS);
    if clamped != timeout_seconds {
        eprintln!(
            "[kiqi-extension] exec grant timeout {timeout_seconds}s exceeds the {clamped}s cap; \
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

/// How many sockets one extension may hold open at once through `net-sockets`.
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
/// [`EXEC_TIMEOUT_MAX_SECONDS`](crate::grants::EXEC_TIMEOUT_MAX_SECONDS).
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
        // Cleared, then only `PATH` back: an extension must not inherit the host's
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
// WasmExtension
// ---------------------------------------------------------------------------

struct Inner {
    store: Store<HostState>,
    guest: World,
}

/// Everything needed to build a fresh instance from the retained engine and
/// component.
///
/// A call with a wall-clock budget (`oauth`) that overruns it hands its store
/// to the abandoned worker, which still owns it. The extension re-instantiates
/// from this recipe instead of staying "unloaded" for the rest of the process.
struct InstanceRecipe {
    grants: Grants,
    fs_root: Option<std::path::PathBuf>,
    project_dir: Option<std::path::PathBuf>,
    fuel: Option<u64>,
    max_memory_bytes: usize,
    oauth_transport: Option<std::sync::Arc<dyn OAuthTransport>>,
    credential_sink: Option<std::sync::Arc<dyn ExtensionCredentialSink>>,
    /// The engine is epoch-instrumented (a `patch` grant exists), so a fresh
    /// store must start with a disarmed epoch deadline.
    gate_epoch: bool,
    declared_flags: std::sync::Arc<std::collections::BTreeSet<String>>,
}

/// The declarative surfaces an extension publishes, captured once per instance.
///
/// `shortcut_hosts` runs on every key event and `probe_tool_call` on every
/// tool call, so asking the guest each time would put a mutex and a wasm call
/// on those hot paths. The cache makes the *first* call populate it; every
/// later lookup is a clone out of memory. A trapping guest caches an empty
/// list (fail closed) rather than retrying on every keystroke.
#[derive(Debug, Default, Clone)]
struct DeclaredSurfaces {
    commands: Option<Vec<crate::CommandSpec>>,
    flags: Option<Vec<crate::FlagSpec>>,
    shortcuts: Option<Vec<crate::ShortcutSpec>>,
    probe_points: Option<Vec<String>>,
}

/// Load-time knobs.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Bound the guest's execution. With a budget, an extension that spins traps
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
    /// Where a completed OAuth exchange writes its token. The host crate cannot
    /// depend on `kiqi-cli`, so the CLI installs this when it constructs the
    /// instance. `None` means a granted exchange returns `failed` rather than
    /// writing anywhere — the safe default for embedders and tests.
    pub credential_sink: Option<std::sync::Arc<dyn ExtensionCredentialSink>>,
    /// The HTTP seam for `oauth-http`. `None` uses the real transport; tests
    /// inject a fake so policy and bounds are asserted without a network.
    pub oauth_transport: Option<std::sync::Arc<dyn OAuthTransport>>,
}

/// The default linear-memory cap for a guest: 256 MiB, which is far above what
/// any real extension needs and far below the wasm32 address space.
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
/// The `kiqi:extension` ABI version a component was built against, read from
/// its imports (`kiqi:extension/<interface>@<version>`).
///
/// `None` when the component imports nothing from the extension world (an empty
/// or foreign component) or fails to parse; the caller then lets instantiation
/// produce its own error. This check only exists to turn a *mismatch* into an
/// actionable message.
fn component_abi_version(bytes: &[u8]) -> Option<String> {
    use wasmparser::{Parser, Payload};
    for payload in Parser::new(0).parse_all(bytes) {
        let Ok(Payload::ComponentImportSection(reader)) = payload else {
            continue;
        };
        for import in reader.into_iter().flatten() {
            let name = import.name.name;
            if !name.starts_with("kiqi:extension") {
                continue;
            }
            // The reader splits `kiqi:extension/<iface>@<version>` into
            // `name` + `version_suffix`; older encoders keep the suffix inline —
            // accept both so the diagnostic does not depend on the encoder.
            if let Some(version) = import.name.version_suffix {
                return Some(version.to_string());
            }
            if let Some((_, version)) = name.rsplit_once('@') {
                if !version.is_empty() {
                    return Some(version.to_string());
                }
            }
        }
    }
    None
}

fn wasmtime_config(options: &Options) -> wasmtime::Config {
    let mut config = wasmtime::Config::new();
    if options.fuel.is_some() {
        // Metering is off unless a bound is asked for, so ordinary extensions do
        // not pay for it.
        config.consume_fuel(true);
    }
    #[cfg(target_os = "macos")]
    config.macos_use_mach_ports(false);
    config
}

/// An extension loaded from a wasm component.
pub struct WasmExtension {
    id: String,
    /// Kept so a reload can re-instantiate without recompiling the wasm, and
    /// so an instance abandoned by a timed-out call can be replaced.
    engine: std::sync::Arc<Engine>,
    /// Kept so `/reload` can re-instantiate from the already-compiled component.
    component: Component,
    /// Declared surfaces, captured lazily on first use (see [`DeclaredSurfaces`]).
    declared: Mutex<DeclaredSurfaces>,
    /// The resolved `provider-request` policy (observe / patch / deadline).
    ///
    /// Kept beside the instance rather than behind [`Self::inner`]: the event
    /// fan-out partitions hosts on every provider request, and reading this
    /// must never wait for a guest that is mid-call (`ask_user` holds that
    /// mutex while its dialog is open).
    provider_request: Option<ProviderRequestPolicy>,
    /// The `oauth` grant's profiles, hoisted out of the store so the CLI can
    /// ask which providers this host offers without waiting for a guest that is
    /// mid-call. `None` means no `oauth` grant: the guest is never called.
    oauth_grant: Option<crate::grants::OAuthGrant>,
    /// Provider ids the guest declared through `oauth-api.list-providers` in
    /// the bounded load-time cross-check. `None` means the cross-check could
    /// not run (fail open to the grant, the sole catalog authority);
    /// `Some(set)` narrows the grant's list to what the guest actually drives.
    oauth_crosscheck: Option<std::collections::BTreeSet<String>>,
    /// How to build a fresh store when the current one is abandoned by a
    /// timed-out call (the worker owns it and will not give it back).
    recipe: InstanceRecipe,
    inner: Mutex<Option<Inner>>,
}

/// 宿主侧 guest 失败诊断汇。kiqi-extension-host 是库，`tracing::warn!` 只有在
/// 宿主进程装了 subscriber 时才可见——ki（kiqi-cli）至今没有装，P0 加的
/// 诊断因此是死代码。嵌入方（kiqi-cli）在进程启动时调用
/// [`set_guest_error_sink`] 把诊断接进自己的日志文件；未设置时只剩 tracing。
static GUEST_ERROR_SINK: std::sync::OnceLock<
    std::sync::Arc<dyn Fn(&str, &str, &str) + Send + Sync>,
> = std::sync::OnceLock::new();

/// 把一条 guest 失败诊断同时发给 tracing 与已安装的 sink（若有的话）。
fn emit_guest_error(extension: &str, op: &str, error: &str) {
    tracing::warn!(extension = %extension, op, %error, "guest call failed");
    if let Some(sink) = GUEST_ERROR_SINK.get() {
        sink(extension, op, error);
    }
}

/// 安装 guest 失败诊断汇（进程级，幂等：后一次设置不覆盖前一次）。
pub fn set_guest_error_sink(sink: std::sync::Arc<dyn Fn(&str, &str, &str) + Send + Sync>) {
    let _ = GUEST_ERROR_SINK.set(sink);
}

impl WasmExtension {
    /// 宿主侧 guest 调用诊断汇（卡：0.8.0 能力补齐，P0）。
    ///
    /// 元数据/观测类入口原先用 `Err(_) => Vec::new()` / `result.ok()` 把 trap 吞掉，
    /// 于是一个**坏掉的 guest** 与一个**什么都没声明的 guest** 在宿主侧完全同形：
    /// 安装/展示/事件扇出全都静默少一块，用户没有任何线索。这里统一走 tracing，
    /// 与 `kiqi-extensions` 既有诊断口径一致。
    fn report_guest_error(&self, op: &str, error: impl std::fmt::Display) {
        emit_guest_error(&self.id, op, &error.to_string());
    }

    /// Load and instantiate a component with the grants the user recorded for
    /// this extension id (a missing `grants.json` grants nothing). `init` is called
    /// before returning.
    pub fn load(id: impl Into<String>, wasm: &Path) -> Result<Self, ExtensionError> {
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
    ) -> Result<Self, ExtensionError> {
        Self::load_with_options(id, wasm, grants, Options::default())
    }

    /// Load with explicit grants and load-time knobs.
    pub fn load_with_options(
        id: impl Into<String>,
        wasm: &Path,
        grants: Grants,
        options: Options,
    ) -> Result<Self, ExtensionError> {
        let id = id.into();
        // Resolve the provider-request policy before the grants move into the
        // store: the fan-out needs it without locking the guest. A `patch`
        // grant also decides whether the engine is epoch-instrumented: only
        // the gate has a deadline, so only a patch-capable extension pays for
        // interruption (and needs the ticker that makes a deadline fire).
        let provider_request = resolve_provider_request_policy(&grants);
        let gate_epoch = provider_request.as_ref().is_some_and(|policy| policy.patch);
        let mut config = wasmtime_config(&options);
        if gate_epoch {
            config.epoch_interruption(true);
        }
        let engine = std::sync::Arc::new(
            Engine::new(&config)
                .map_err(|error| ExtensionError::Failed(format!("wasmtime engine: {error}")))?,
        );
        if gate_epoch {
            let ticker = render_ticker().ok_or_else(|| {
                ExtensionError::Failed(format!(
                    "{id}: epoch ticker is unavailable; a provider gate could never time out"
                ))
            })?;
            ticker.register(&engine);
        }
        // Read once so the ABI version can be checked before wasmtime gets a
        // chance to fail with an opaque linker error.
        let bytes = std::fs::read(wasm)
            .map_err(|error| ExtensionError::Failed(format!("load {}: {error}", wasm.display())))?;
        if let Some(version) = component_abi_version(&bytes) {
            if version != crate::ABI_VERSION {
                return Err(ExtensionError::Failed(format!(
                    "{id}: this component was built for kiqi:extension@{version}, but the host \
                     speaks kiqi:extension@{}; rebuild it with `ki insmod --build extensions/wasm/<crate>`",
                    crate::ABI_VERSION
                )));
            }
        }
        let component = Component::new(&engine, &bytes)
            .map_err(|error| ExtensionError::Failed(format!("load {}: {error}", wasm.display())))?;

        // Hoist the OAuth grant before it moves into the store: `oauth_providers`
        // is read by `ki auth` and must not lock the guest. `None` is the
        // deny-by-default state and means the guest is never called.
        let oauth_grant = grants.oauth.clone();
        let mut recipe = InstanceRecipe {
            grants,
            fs_root: wasm.parent().map(|dir| dir.join("data")),
            project_dir: options.project_dir.clone(),
            fuel: options.fuel,
            max_memory_bytes: options.max_memory_bytes.unwrap_or(DEFAULT_MAX_MEMORY_BYTES),
            oauth_transport: options.oauth_transport.clone(),
            credential_sink: options.credential_sink.clone(),
            gate_epoch,
            declared_flags: std::sync::Arc::new(std::collections::BTreeSet::new()),
        };
        let Inner { mut store, guest } = Self::instantiate(&id, &engine, &component, &recipe)?;
        // The extension declares its own identity. The install directory stays the
        // authorization key (grants live beside the file and were read by it), so
        // a mismatch is reported rather than silently trusted.
        if let Ok(declared) = guest.kiqi_extension_extension_api().call_id(&mut store) {
            if !declared.is_empty() && declared != id {
                eprintln!(
                    "[kiqi-extension] {id}: component declares id `{declared}`; \
                     grants remain keyed by the install directory"
                );
            }
        }
        // Capture the declared flags ONCE, right after instantiation, so
        // `session-read.cli-flag` can be declaration-scoped without touching
        // the guest on every read. A trap here is a broken guest: report it and
        // scope to the empty set (fail closed — no flag is readable).
        let (declared_flag_names, declared_flag_specs) = match guest
            .kiqi_extension_extension_api()
            .call_list_flags(&mut store)
        {
            Ok(specs) => {
                // One call serves both consumers: the declaration scope on
                // `HostState` and the cached `flags()` surface.
                let names = specs.iter().map(|spec| spec.name.clone()).collect();
                let flags = specs
                    .into_iter()
                    .map(|spec| crate::FlagSpec {
                        name: spec.name,
                        description: spec.description,
                    })
                    .collect();
                (names, flags)
            }
            Err(error) => {
                tracing::warn!(extension = %id, op = "list-flags", %error, "guest call failed");
                (std::collections::BTreeSet::new(), Vec::new())
            }
        };
        store.data_mut().reap_call_scoped();
        let declared_flag_names = std::sync::Arc::new(declared_flag_names);
        store.data_mut().declared_flags = std::sync::Arc::clone(&declared_flag_names);
        recipe.declared_flags = declared_flag_names;

        // A granted OAuth guest declares the providers it drives. The check is
        // bounded (the install-time discovery budget) and advisory: the GRANT
        // is the catalog authority, so a trap or an over-cap answer narrows
        // nothing (`None`). Only a call that never returns aborts the load,
        // because the instance is stuck in that thread.
        let (oauth_crosscheck, store, guest) = if oauth_grant.is_some() {
            match bounded_oauth_provider_ids(store, guest, DISCOVERY_WAIT) {
                Ok((ids, store, guest)) => (ids, store, guest),
                Err(error) => {
                    return Err(ExtensionError::Failed(format!("{id}: {error}")));
                }
            }
        } else {
            (None, store, guest)
        };

        Ok(Self {
            id,
            engine,
            component,
            declared: Mutex::new(DeclaredSurfaces {
                flags: Some(declared_flag_specs),
                ..DeclaredSurfaces::default()
            }),
            provider_request,
            oauth_grant,
            oauth_crosscheck,
            recipe,
            inner: Mutex::new(Some(Inner { store, guest })),
        })
    }

    /// Build one store + guest from the retained engine and component.
    ///
    /// Called at load, and again after an `oauth` call times out: the worker
    /// that overran its deadline still owns the old store, so re-instantiating
    /// is the only way to put the extension back in service without waiting for
    /// it (and, in the worst case, without ever waiting for it at all).
    fn instantiate(
        id: &str,
        engine: &Engine,
        component: &Component,
        recipe: &InstanceRecipe,
    ) -> Result<Inner, ExtensionError> {
        let mut linker = Linker::new(engine);
        World::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |state| state).map_err(
            |error| ExtensionError::Failed(format!("link extension interfaces: {error}")),
        )?;
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|error| ExtensionError::Failed(format!("link wasi: {error}")))?;
        let oauth = OAuthState {
            budget: recipe
                .grants
                .oauth
                .as_ref()
                .map(OAuthRequestBudget::for_grant)
                .unwrap_or_default(),
            transport: recipe.oauth_transport.clone(),
            sink: recipe.credential_sink.clone(),
        };
        let limits = wasmtime::StoreLimitsBuilder::new()
            .memory_size(recipe.max_memory_bytes)
            .build();
        let mut store = Store::new(
            engine,
            HostState {
                wasi: WasiCtxBuilder::new().build(),
                table: ResourceTable::new(),
                grants: recipe.grants.clone(),
                fs_root: recipe.fs_root.clone(),
                project_dir: recipe.project_dir.clone(),
                extension_id: Some(id.to_string()),
                declared_flags: std::sync::Arc::clone(&recipe.declared_flags),
                sockets: std::collections::HashMap::new(),
                next_socket: 1,
                procs: std::collections::HashMap::new(),
                next_proc: 1,
                limits,
                oauth,
                provider_http: provider_http::ProviderHttpTable::default(),
                provider_call: None,
            },
        );
        // The cap only matters if the store consults it.
        store.limiter(|state| &mut state.limits);
        // An epoch-instrumented engine traps on the default deadline of 0.
        // Instantiation and every non-gate call run unbounded; `provider_gate`
        // installs the real deadline around its guest call.
        if recipe.gate_epoch {
            store.set_epoch_deadline(EPOCH_DEADLINE_UNBOUNDED);
        }
        if let Some(fuel) = recipe.fuel {
            store.set_fuel(fuel).map_err(|error| {
                ExtensionError::Failed(format!("set fuel budget for {id}: {error}"))
            })?;
        }
        let guest = World::instantiate(&mut store, component, &linker)
            .map_err(|error| ExtensionError::Failed(format!("instantiate {id}: {error}")))?;
        Ok(Inner { store, guest })
    }

    /// Call the guest `shutdown` and release the instance.
    ///
    /// This is the counterpart of [`WasmExtension::load`]: after it returns, the
    /// component's linear memory and resources are gone. `/reload` relies on it,
    /// and [`Drop`] calls it so a dropped extension cannot leak an instance.
    pub fn unload(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(mut inner) = guard.take() {
            // Best effort: a broken guest must not keep the host from releasing
            // the instance.
            let _ = inner
                .guest
                .kiqi_extension_extension_api()
                .call_shutdown(&mut inner.store);
            // `shutdown` is a guest entry point like any other: an extension may
            // spawn through `proc` in it, and nothing else runs after this
            // point, so reap here too — a handle's lifetime is "one call".
            inner.store.data_mut().reap_call_scoped();
            // `inner` drops here, releasing the store (and its memory).
        }
    }
}

impl WasmExtension {
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
        // the payload is cut down to what *this* extension's provider-request
        // grant allows, and an ungranted guest is never called at all. The
        // grants live in the store (that is why this happens here and not in
        // kiqi-extensions, which cannot see them).
        // Provider 观测三件套统一按 grant 投递：无 `provider-request` grant 的
        // guest 一个都收不到；`AfterProviderResponse` 在 0.8.0 里没有 wasm 面
        // （终端消息由 MessageEnd 覆盖），永远不投递。
        let wit = match event.tag {
            crate::EventTag::BeforeProviderRequest => {
                match graded_provider_request(event, &inner.store.data().grants, false) {
                    Some(wit) => wit,
                    None => return EventAction::Continue,
                }
            }
            crate::EventTag::BeforeProviderHeaders => {
                // 与 CLI 分流同口径：observe 与 patch 都没开就不投递。
                match inner.store.data().grants.provider_request.as_ref() {
                    Some(policy) if policy.observe == ObserveMode::Requests || policy.patch => {}
                    _ => return EventAction::Continue,
                }
                to_wit_event(event)
            }
            crate::EventTag::AfterProviderResponse => return EventAction::Continue,
            _ => to_wit_event(event),
        };
        let _context = install_context_kind(None, caps, CallKind::Event, None);
        let result = inner
            .guest
            .kiqi_extension_extension_api()
            .call_on_event(&mut inner.store, &wit);
        clear_context();
        // A `proc` handle lives for this call and nothing beyond it.
        inner.store.data_mut().reap_call_scoped();
        match result {
            Ok(WitEventAction::Continue) => EventAction::Continue,
            Ok(WitEventAction::Claimed) => EventAction::Claimed,
            // The patch channel belongs to the provider gate, which calls
            // [`WasmExtension::provider_gate`] directly. An observe or claiming
            // dispatch ignores it: a patch returned for any tag (including
            // `before-provider-request` on the observe path) changes nothing.
            Ok(WitEventAction::Patch(_)) => EventAction::Continue,
            Err(trap) => {
                eprintln!("[kiqi-extension] {}: on-event trapped: {trap}", self.id);
                EventAction::Continue
            }
        }
    }
}

impl WasmExtension {
    /// Run the provider gate for one `before-provider-request` event.
    ///
    /// Deny-by-default: a host whose policy does not grant `patch` is never
    /// called. The guest sees the same graded (redacted) payload an observer
    /// would; a returned patch is validated against the clamp table, and any
    /// violation rejects the WHOLE patch (`Err`) so the request proceeds
    /// unpatched. A patch for any other tag is ignored.
    pub fn provider_gate(
        &self,
        event: &Event,
        caps: &HostCaps<'_>,
    ) -> Result<Option<Value>, String> {
        if event.tag != crate::EventTag::BeforeProviderRequest {
            return Ok(None);
        }
        let Some(policy) = self.provider_request else {
            return Ok(None);
        };
        if !policy.patch {
            return Ok(None);
        }
        // A gate call never borrows the shared tool/event instance. Epoch
        // interruption cannot stop a guest parked in a blocking host import;
        // using `self.inner` here would let the abandoned gate worker keep
        // that mutex forever and wedge every later probe and tool call. A
        // fresh store also gives the next gate a clean instance after the
        // caller's wall-clock deadline abandons this one. Component
        // instantiation is cheap next to a provider request; compilation is
        // still reused through `self.component`.
        let mut inner =
            match Self::instantiate(&self.id, &self.engine, &self.component, &self.recipe) {
                Ok(inner) => inner,
                Err(error) => {
                    self.report_guest_error("provider-gate-instantiate", &error);
                    return Ok(None);
                }
            };
        // Gate delivery ignores the observe switch: a patch-only grant must
        // still see the request it is patching.
        let Some(wit) = graded_provider_request(event, &inner.store.data().grants, true) else {
            return Ok(None);
        };
        // Keep epoch interruption as the second line of defence for a spinning
        // guest. The caller's wall-clock wait handles host imports, which do
        // not consume epoch ticks; neither path can retain the shared mutex.
        inner
            .store
            .set_epoch_deadline(epoch_ticks(policy.patch_timeout));
        let _context = install_context_kind(None, caps, CallKind::Event, None);
        let result = inner
            .guest
            .kiqi_extension_extension_api()
            .call_on_event(&mut inner.store, &wit);
        clear_context();
        // A `proc` handle lives for this call and nothing beyond it.
        inner.store.data_mut().reap_call_scoped();
        // Disarm: only the gate runs against a deadline.
        inner.store.set_epoch_deadline(EPOCH_DEADLINE_UNBOUNDED);
        let (grant, provider_budget_ms) = {
            let grants = &inner.store.data().grants;
            let budget = grants
                .provider
                .as_ref()
                .map(|grant| grant.effective_timeout_seconds().saturating_mul(1000));
            (grants.provider_request.clone(), budget)
        };
        // The dedicated instance is dropped on every return. In particular,
        // an epoch-trapped instance is poisoned and must never be entered
        // again; a guest abandoned in a host import remains owned by its
        // worker and is dropped there if it ever returns.
        match result {
            Ok(WitEventAction::Patch(patch)) => {
                let Some(grant) = grant else {
                    return Ok(None);
                };
                if !grant.patch {
                    return Ok(None);
                }
                let raw: Value = serde_json::from_str(&patch)
                    .map_err(|error| format!("{}: provider patch is not JSON: {error}", self.id))?;
                provider_patch::validate_provider_patch(&raw, provider_budget_ms)
                    .map(Some)
                    .map_err(|error| format!("{}: provider patch rejected: {error}", self.id))
            }
            // `Continue`/`Claimed` mean "no patch"; `Claimed` does not stop the
            // request, because only a patch can change it.
            Ok(_) => Ok(None),
            Err(trap) => {
                self.report_guest_error("provider-gate", &trap);
                Ok(None)
            }
        }
    }

    /// `list-commands`, captured on first use.
    fn cached_commands(&self) -> Vec<crate::CommandSpec> {
        if let Some(cached) = self
            .declared
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .commands
            .clone()
        {
            return cached;
        }
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return Vec::new();
        };
        let result = inner
            .guest
            .kiqi_extension_extension_api()
            .call_list_commands(&mut inner.store);
        inner.store.data_mut().reap_call_scoped();
        let commands = match result {
            Ok(specs) => specs
                .into_iter()
                .map(|spec| crate::CommandSpec {
                    name: spec.name,
                    description: spec.description,
                })
                .collect(),
            Err(error) => {
                self.report_guest_error("list-commands", &error);
                Vec::new()
            }
        };
        self.declared
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .commands = Some(commands.clone());
        commands
    }

    /// `list-flags`, captured on first use.
    fn cached_flags(&self) -> Vec<crate::FlagSpec> {
        if let Some(cached) = self
            .declared
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .flags
            .clone()
        {
            return cached;
        }
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return Vec::new();
        };
        let result = inner
            .guest
            .kiqi_extension_extension_api()
            .call_list_flags(&mut inner.store);
        inner.store.data_mut().reap_call_scoped();
        let flags: Vec<crate::FlagSpec> = match result {
            Ok(specs) => specs
                .into_iter()
                .map(|spec| crate::FlagSpec {
                    name: spec.name,
                    description: spec.description,
                })
                .collect(),
            Err(error) => {
                self.report_guest_error("list-flags", &error);
                Vec::new()
            }
        };
        self.declared
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .flags = Some(flags.clone());
        flags
    }

    /// `list-shortcuts`, captured on first use. This is the hot path the cache
    /// exists for: shortcut lookup runs on every key event.
    fn cached_shortcuts(&self) -> Vec<crate::ShortcutSpec> {
        if let Some(cached) = self
            .declared
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .shortcuts
            .clone()
        {
            return cached;
        }
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return Vec::new();
        };
        let result = inner
            .guest
            .kiqi_extension_extension_api()
            .call_list_shortcuts(&mut inner.store);
        inner.store.data_mut().reap_call_scoped();
        let shortcuts: Vec<crate::ShortcutSpec> = match result {
            Ok(specs) => specs
                .into_iter()
                .map(|spec| crate::ShortcutSpec {
                    key: spec.key,
                    description: spec.description,
                })
                .collect(),
            Err(error) => {
                self.report_guest_error("list-shortcuts", &error);
                Vec::new()
            }
        };
        self.declared
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .shortcuts = Some(shortcuts.clone());
        shortcuts
    }

    /// `list-probe-points`, captured on first use.
    fn cached_probe_points(&self) -> Vec<String> {
        if let Some(cached) = self
            .declared
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .probe_points
            .clone()
        {
            return cached;
        }
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return Vec::new();
        };
        let result = inner
            .guest
            .kiqi_extension_extension_api()
            .call_list_probe_points(&mut inner.store);
        inner.store.data_mut().reap_call_scoped();
        let points = match result {
            Ok(points) => points,
            Err(error) => {
                self.report_guest_error("list-probe-points", &error);
                Vec::new()
            }
        };
        self.declared
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .probe_points = Some(points.clone());
        points
    }
}

impl Drop for WasmExtension {
    fn drop(&mut self) {
        self.unload();
    }
}

impl ExtensionTrait for WasmExtension {
    fn id(&self) -> &str {
        &self.id
    }

    fn tools(&self) -> Vec<ToolSpec> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return Vec::new();
        };
        let guest = inner.guest.kiqi_extension_extension_api();
        let result = guest.call_list_tools(&mut inner.store);
        // Spawning in a metadata call is legal; nothing survives it either.
        inner.store.data_mut().reap_call_scoped();
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
                eprintln!("[kiqi-extension] {}: list-tools trapped: {error}", self.id);
                Vec::new()
            }
        }
    }

    fn commands(&self) -> Vec<CommandSpec> {
        self.cached_commands()
    }

    fn run_command(
        &self,
        name: &str,
        arguments: &str,
        caps: &HostCaps<'_>,
    ) -> Result<crate::CommandResult, ExtensionError> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return Err(ExtensionError::Failed(format!("{} is unloaded", self.id)));
        };
        let _context = install_context_kind(None, caps, CallKind::Command, None);
        let result = inner.guest.kiqi_extension_extension_api().call_run_command(
            &mut inner.store,
            name,
            arguments,
        );
        clear_context();
        // A `proc` handle lives for this call and nothing beyond it.
        inner.store.data_mut().reap_call_scoped();
        match result {
            Ok(Ok(result)) => match sanitize_command_result(to_command_result(result)) {
                Ok(result) => Ok(result),
                Err(diagnostic) => {
                    self.report_guest_error("run-command", &diagnostic);
                    Err(ExtensionError::Failed(diagnostic))
                }
            },
            Ok(Err(error)) => Err(map_extension_error(error)),
            Err(trap) => {
                self.report_guest_error("run-command", &trap);
                Err(ExtensionError::Failed(format!(
                    "{}: command `{name}` trapped: {trap}",
                    self.id
                )))
            }
        }
    }

    /// Answer a selector/editor/prompt flow started by [`Extension::run_command`].
    ///
    /// The CLI owns the pending slot and validates the id before this is
    /// called; the host still sanitizes and types the returned body so a guest
    /// cannot smuggle a second unsanitized flow through the continuation.
    fn continue_command(
        &self,
        name: &str,
        flow_id: &str,
        reply: crate::CommandReply,
        caps: &HostCaps<'_>,
    ) -> Result<crate::CommandResult, ExtensionError> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return Err(ExtensionError::Failed(format!("{} is unloaded", self.id)));
        };
        let _context = install_context_kind(None, caps, CallKind::Command, None);
        let result = inner
            .guest
            .kiqi_extension_extension_api()
            .call_continue_command(&mut inner.store, name, flow_id, &to_wit_reply(reply));
        clear_context();
        inner.store.data_mut().reap_call_scoped();
        match result {
            Ok(Ok(result)) => match sanitize_command_result(to_command_result(result)) {
                Ok(result) => Ok(result),
                Err(diagnostic) => {
                    self.report_guest_error("continue-command", &diagnostic);
                    Err(ExtensionError::Failed(diagnostic))
                }
            },
            Ok(Err(error)) => Err(map_extension_error(error)),
            Err(trap) => {
                self.report_guest_error("continue-command", &trap);
                Err(ExtensionError::Failed(format!(
                    "{}: command `{name}` continuation trapped: {trap}",
                    self.id
                )))
            }
        }
    }

    fn flags(&self) -> Vec<FlagSpec> {
        self.cached_flags()
    }

    fn shortcuts(&self) -> Vec<ShortcutSpec> {
        self.cached_shortcuts()
    }

    fn before_agent_start(
        &self,
        prompt: &serde_json::Value,
        caps: &HostCaps<'_>,
    ) -> Option<String> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let inner = guard.as_mut()?;
        let _context = install_context_kind(None, caps, CallKind::PromptHook, None);
        let result = inner
            .guest
            .kiqi_extension_extension_api()
            .call_before_agent_start(&mut inner.store, &prompt.to_string());
        clear_context();
        // A `proc` handle lives for this call and nothing beyond it.
        inner.store.data_mut().reap_call_scoped();
        match result {
            Ok(value) => value,
            Err(e) => {
                self.report_guest_error("before-agent-start", &e);
                None
            }
        }
    }

    fn probe_points(&self) -> Vec<String> {
        self.cached_probe_points()
    }

    fn probe(&self, point: &str, payload: &serde_json::Value, caps: &HostCaps<'_>) -> Verdict {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return Verdict::Continue;
        };
        let _context = install_context_kind(None, caps, CallKind::Probe, None);
        let result = inner.guest.kiqi_extension_extension_api().call_probe(
            &mut inner.store,
            point,
            &payload.to_string(),
        );
        clear_context();
        // A `proc` handle lives for this call and nothing beyond it.
        inner.store.data_mut().reap_call_scoped();
        match result {
            Ok(WitVerdict::Continue) => Verdict::Continue,
            Ok(WitVerdict::Replace(json)) => {
                Verdict::Replace(serde_json::from_str(&json).unwrap_or(serde_json::Value::Null))
            }
            Ok(WitVerdict::Block(reason)) => Verdict::Block { reason },
            Err(trap) => {
                eprintln!("[kiqi-extension] {}: probe trapped: {trap}", self.id);
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
    ) -> Result<ToolOutcome, ExtensionError> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.as_mut() else {
            return Err(ExtensionError::Failed(format!("{} is unloaded", self.id)));
        };

        let arguments = serde_json::to_string(arguments).unwrap_or_else(|_| "null".to_string());
        let _context = install_context_kind(Some(sink), caps, CallKind::Tool, None);
        let result = inner.guest.kiqi_extension_extension_api().call_call_tool(
            &mut inner.store,
            name,
            &arguments,
        );
        clear_context();
        // A `proc` handle lives for this call and nothing beyond it.
        inner.store.data_mut().reap_call_scoped();

        match result {
            Ok(Ok(tool_result)) => Ok(to_tool_outcome(tool_result)),
            Ok(Err(error)) => Err(map_extension_error(error)),
            Err(trap) => Err(ExtensionError::Failed(format!(
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

    fn oauth_providers(&self) -> Vec<OAuthProvider> {
        let Some(grant) = self.oauth_grant.as_ref() else {
            // Deny-by-default: no grant, no provider. The guest is not asked.
            return Vec::new();
        };
        grant
            .providers
            .iter()
            .filter(|profile| {
                // The load-time cross-check can only narrow the grant. `None`
                // means it did not run, so the grant's list stands.
                self.oauth_crosscheck
                    .as_ref()
                    .map_or(true, |ids| ids.contains(&profile.id))
            })
            .map(|profile| OAuthProvider {
                id: profile.id.clone(),
                display_name: profile.display_name.clone(),
                scopes: profile.scopes.clone(),
                flows: profile
                    .flows
                    .iter()
                    .map(|flow| match flow {
                        crate::grants::OAuthFlow::AuthorizationCode => {
                            crate::OAuthFlow::AuthorizationCode
                        }
                        crate::grants::OAuthFlow::DeviceCode => crate::OAuthFlow::DeviceCode,
                    })
                    .collect(),
                supports_pkce: profile.supports_pkce,
            })
            .collect()
    }

    fn oauth_action(
        &self,
        provider_id: &str,
        action: OAuthAction,
    ) -> Result<OAuthResponse, ExtensionError> {
        // Deny-by-default: the grant decides whether the guest is reached at
        // all, and it must name exactly this provider.
        let Some(grant) = self.oauth_grant.as_ref() else {
            return Err(ExtensionError::Denied(format!(
                "oauth is not granted for provider `{provider_id}`"
            )));
        };
        if !grant
            .providers
            .iter()
            .any(|profile| profile.id == provider_id)
        {
            return Err(ExtensionError::Denied(format!(
                "oauth provider `{provider_id}` is not granted"
            )));
        }
        // The load-time cross-check can only narrow: an id the guest did not
        // declare in `list-providers` is never passed through. `None` means the
        // check could not run, so the grant stands alone.
        if let Some(ids) = self.oauth_crosscheck.as_ref() {
            if !ids.contains(provider_id) {
                return Err(ExtensionError::Denied(format!(
                    "oauth provider `{provider_id}` is not declared by the guest"
                )));
            }
        }
        // One step runs on a plain thread with a wall-clock deadline derived
        // from the grant. The extra second lets the host-side HTTP timeout
        // inside the guest fire before we give up on the call.
        let timeout = std::time::Duration::from_secs(grant.timeout_seconds.max(1))
            + std::time::Duration::from_secs(1);
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(inner) = guard.take() else {
            return Err(ExtensionError::Failed(format!("{} is unloaded", self.id)));
        };
        let pid = provider_id.to_string();
        let wit_action = to_wit_oauth_action(action);
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("kiqi-extension-oauth".to_string())
            .spawn(move || {
                let mut inner = inner;
                let result = inner.guest.kiqi_extension_oauth_api().call_action(
                    &mut inner.store,
                    &pid,
                    &wit_action,
                );
                // A `proc` handle lives for this call and nothing beyond it.
                inner.store.data_mut().reap_call_scoped();
                // The receiver may be gone on timeout; the store still drops.
                let _ = tx.send((result, inner));
            });
        if let Err(error) = spawned {
            return Err(ExtensionError::Failed(format!(
                "{}: cannot start the oauth thread: {error}",
                self.id
            )));
        }
        match rx.recv_timeout(timeout) {
            Ok((result, inner)) => {
                *guard = Some(inner);
                match result {
                    Ok(Ok(response)) => Ok(to_extension_oauth_response(response)),
                    Ok(Err(error)) => Err(map_extension_error(error)),
                    Err(trap) => {
                        self.report_guest_error("oauth-action", &trap);
                        Err(ExtensionError::Failed(format!(
                            "{}: oauth action trapped: {trap}",
                            self.id
                        )))
                    }
                }
            }
            Err(_) => {
                // The worker that overran the deadline still owns the store,
                // so the extension must not stay "unloaded" for good: build a
                // fresh instance from the retained engine and component. The
                // abandoned store drops on its own thread when (if) the guest
                // finally returns.
                match Self::instantiate(&self.id, &self.engine, &self.component, &self.recipe) {
                    Ok(fresh) => *guard = Some(fresh),
                    Err(error) => tracing::warn!(
                        extension = %self.id,
                        %error,
                        "cannot re-instantiate after an oauth timeout; extension stays unloaded"
                    ),
                }
                Err(ExtensionError::Failed(format!(
                    "{}: oauth action timed out after {}s",
                    self.id,
                    timeout.as_secs()
                )))
            }
        }
    }

    fn provider_request_policy(&self) -> Option<ProviderRequestPolicy> {
        self.provider_request
    }

    fn provider_gate(&self, event: &Event, caps: &HostCaps<'_>) -> Result<Option<Value>, String> {
        // The inherent method is the real one; this is the trait surface the
        // fan-out dispatcher calls.
        WasmExtension::provider_gate(self, event, caps)
    }

    fn shutdown(&self) {
        self.unload();
    }
}

// ---------------------------------------------------------------------------
// Mapping between the host's types and the ABI's
// ---------------------------------------------------------------------------

fn map_extension_error(
    error: crate::wasm::kiqi::extension::types::ExtensionError,
) -> ExtensionError {
    use crate::wasm::kiqi::extension::types::ExtensionError as Wit;
    match error {
        Wit::UnknownTool(name) => ExtensionError::UnknownTool(name),
        Wit::InvalidArguments(detail) => ExtensionError::InvalidArguments(detail),
        Wit::Denied(detail) => ExtensionError::Denied(detail),
        Wit::Failed(detail) => ExtensionError::Failed(detail),
    }
}

fn to_command_result(
    result: crate::wasm::kiqi::extension::types::CommandResult,
) -> crate::CommandResult {
    use crate::wasm::kiqi::extension::types::CommandResultBody as Wit;
    let body = match result.body {
        Wit::Message(text) => crate::CommandResultBody::Message(text),
        Wit::Selector(spec) => crate::CommandResultBody::Selector(crate::SelectorSpec {
            title: spec.title,
            items: spec.items,
        }),
        Wit::Editor(spec) => crate::CommandResultBody::Editor(crate::EditorSpec {
            initial_text: spec.initial_text,
        }),
        Wit::Prompt(spec) => crate::CommandResultBody::Prompt(crate::PromptSpec {
            title: spec.title,
            placeholder: spec.placeholder,
        }),
    };
    crate::CommandResult {
        flow_id: result.flow_id,
        body,
    }
}

/// Convert a reply crossing back into the guest.
fn to_wit_reply(reply: crate::CommandReply) -> crate::wasm::kiqi::extension::types::CommandReply {
    use crate::wasm::kiqi::extension::types::CommandReply as Wit;
    match reply {
        crate::CommandReply::Selected(value) => Wit::Selected(value),
        crate::CommandReply::Submitted(value) => Wit::Submitted(value),
        crate::CommandReply::Cancelled => Wit::Cancelled,
    }
}

/// Maximum selector items a command may return.
pub const COMMAND_FLOW_MAX_ITEMS: usize = 64;
/// Maximum bytes in one selector item (or one title/placeholder/flow id).
pub const COMMAND_FLOW_MAX_ITEM_BYTES: usize = 1024;
/// Maximum bytes across all selector items.
pub const COMMAND_FLOW_MAX_TOTAL_BYTES: usize = 64 * 1024;
/// Maximum bytes an editor may pre-fill.
pub const COMMAND_FLOW_MAX_EDITOR_BYTES: usize = 256 * 1024;

/// Sanitize a guest-returned command result before the TUI renders it or
/// writes it into the editor buffer.
///
/// Every string is ANSI-stripped and then rejected if a C0/C1 control
/// character (other than newline/tab) survives. Selector item count, per-item
/// bytes and total bytes are capped; editor pre-fill has its own larger cap.
/// A selector/editor/prompt body must carry a non-empty `flow_id`, otherwise
/// the continuation could never be addressed and the slot would leak.
pub fn sanitize_command_result(
    result: crate::CommandResult,
) -> Result<crate::CommandResult, String> {
    use crate::{CommandResult, CommandResultBody};
    let flow_id = sanitize_command_text(&result.flow_id, "flow id", COMMAND_FLOW_MAX_ITEM_BYTES)?;
    let body = match result.body {
        CommandResultBody::Message(text) => CommandResultBody::Message(sanitize_command_text(
            &text,
            "command message",
            COMMAND_FLOW_MAX_EDITOR_BYTES,
        )?),
        CommandResultBody::Selector(spec) => {
            require_flow_id(&flow_id, "selector")?;
            let title =
                sanitize_command_text(&spec.title, "selector title", COMMAND_FLOW_MAX_ITEM_BYTES)?;
            let mut items = Vec::new();
            let mut total = 0usize;
            for item in spec.items.into_iter().take(COMMAND_FLOW_MAX_ITEMS) {
                let item =
                    sanitize_command_text(&item, "selector item", COMMAND_FLOW_MAX_ITEM_BYTES)?;
                total += item.len();
                if total > COMMAND_FLOW_MAX_TOTAL_BYTES {
                    break;
                }
                items.push(item);
            }
            CommandResultBody::Selector(crate::SelectorSpec { title, items })
        }
        CommandResultBody::Editor(spec) => {
            require_flow_id(&flow_id, "editor")?;
            let initial_text = sanitize_command_text(
                &spec.initial_text,
                "editor initial text",
                COMMAND_FLOW_MAX_EDITOR_BYTES,
            )?;
            CommandResultBody::Editor(crate::EditorSpec { initial_text })
        }
        CommandResultBody::Prompt(spec) => {
            require_flow_id(&flow_id, "prompt")?;
            let title =
                sanitize_command_text(&spec.title, "prompt title", COMMAND_FLOW_MAX_ITEM_BYTES)?;
            let placeholder = sanitize_command_text(
                &spec.placeholder,
                "prompt placeholder",
                COMMAND_FLOW_MAX_ITEM_BYTES,
            )?;
            CommandResultBody::Prompt(crate::PromptSpec { title, placeholder })
        }
    };
    Ok(CommandResult { flow_id, body })
}

fn require_flow_id(flow_id: &str, kind: &str) -> Result<(), String> {
    if flow_id.is_empty() {
        return Err(format!(
            "{kind} command-result has an empty flow id; a continuation could not be addressed"
        ));
    }
    Ok(())
}

fn sanitize_command_text(raw: &str, what: &str, max_bytes: usize) -> Result<String, String> {
    let stripped = strip_ansi_codes(raw);
    if let Some((offset, character)) = stripped
        .char_indices()
        .find(|(_, character)| is_forbidden_control(*character))
    {
        return Err(format!(
            "{what} contains control character U+{:04X} at byte {offset}",
            character as u32
        ));
    }
    Ok(truncate_utf8(&stripped, max_bytes))
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
///   observation, so a merely-loaded extension sees nothing;
/// * the observe fan-out, when `observe` is not `requests`; the provider gate
///   (`for_gate`) also requires `patch`, so a grant with neither delivers
///   nothing on either path.
///
/// What crosses the boundary is graded by the same grant:
///
/// * `messages` only with `messages = full`. Without it the payload still
///   carries `messageCount`, which tells an observer the size of the turn
///   without handing over the conversation;
/// * header values only with `headers = values`, and then only for the
///   explicit non-credential names in
///   [`provider_patch::OBSERVABLE_VALUE_HEADERS`]; otherwise any value-bearing
///   key is removed and `headerNames` remains. A deny-list is not a sufficient
///   credential boundary because providers also use nonstandard secret names.
///   The
///   `kiqi-extensions` bridge emits `headerValues` in its rich payload (the
///   redacted payload for cdylib handlers still carries names only), and this
///   strip is what keeps a default grant from receiving them.
///
/// `BeforeProviderHeaders` and `AfterProviderResponse` keep their current
/// semantics in this phase (deferred: they are not grant-gated yet).
fn graded_provider_request(event: &Event, grants: &Grants, for_gate: bool) -> Option<WitEvent> {
    let grant = grants.provider_request.as_ref()?;
    // The gate needs the event even when the grant is patch-only; the observe
    // fan-out requires the observe switch. A grant with neither never delivers.
    let deliver = grant.observe == ObserveMode::Requests || (for_gate && grant.patch);
    if !deliver {
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
        } else {
            // `headers = values` is not a credential grant. Only explicitly
            // non-credential tracing/correlation values cross; unknown names
            // stay host-side because any of them may be provider credentials.
            for key in ["headers", "headerValues"] {
                if let Some(map) = object.get_mut(key).and_then(Value::as_object_mut) {
                    map.retain(|name, _| provider_patch::is_observable_value_header(name));
                }
            }
        }
    }
    Some(WitEvent::BeforeProviderRequest(payload.to_string()))
}

/// The typed form of a hotkey, read from the `Input` event's payload.
///
/// The payload is the one the TUI builds (`type`, `key`, `kind`, `ctrl`,
/// `alt`, `shift`); a missing field degrades to the neutral value rather than
/// dropping the key, because a shortcut should still see its own event.
fn to_wit_input(event: &Event) -> InputEvent {
    let payload = &event.payload;
    let flag = |name: &str| {
        payload
            .get(name)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    };
    InputEvent {
        key: payload
            .get("key")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        kind: match payload.get("kind").and_then(serde_json::Value::as_str) {
            Some("repeat") => InputKind::Repeat,
            Some("release") => InputKind::Release,
            _ => InputKind::Press,
        },
        ctrl: flag("ctrl"),
        alt: flag("alt"),
        shift: flag("shift"),
    }
}

fn to_wit_event(event: &Event) -> WitEvent {
    use crate::EventTag as T;
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
        T::Input => WitEvent::Input(to_wit_input(event)),
        other => WitEvent::Other(OtherEvent {
            kind: format!("{other:?}"),
            payload: payload_string(event),
        }),
    }
}

// ---------------------------------------------------------------------------
// Markdown render (`render-api`)
// ---------------------------------------------------------------------------

/// How often the process-wide render ticker advances every render engine's
/// epoch. One tick is the deadline granularity: a grant's `timeout-ms` is
/// rounded up to whole ticks.
pub const RENDER_TICK_MS: u64 = 10;

/// A far-future epoch deadline (about 115 days at [`RENDER_TICK_MS`]).
///
/// An epoch-instrumented store traps on the default deadline of 0, so every
/// instance built on a ticked engine starts disarmed here; only a call with a
/// budget installs a real deadline.
pub(crate) const EPOCH_DEADLINE_UNBOUNDED: u64 = 1_000_000_000;

/// Round a wall-clock budget up to whole epoch ticks, the deadline granularity.
pub(crate) fn epoch_ticks(deadline: std::time::Duration) -> u64 {
    deadline.as_millis().div_ceil(RENDER_TICK_MS as u128).max(1) as u64
}

/// Instantiate a full extension world without letting component start code run
/// unbounded. Compilation happens before this worker starts; fuel, the epoch
/// deadline and the outer wall clock cover only untrusted instantiation.
fn instantiate_bounded(
    id: &str,
    kind: &str,
    mut store: Store<HostState>,
    component: Component,
    linker: Linker<HostState>,
) -> Result<(Store<HostState>, World), ExtensionError> {
    store.set_fuel(DISCOVERY_FUEL).map_err(|error| {
        ExtensionError::Failed(format!("instantiate {id} for {kind}: set fuel: {error}"))
    })?;
    store.set_epoch_deadline(epoch_ticks(DISCOVERY_DEADLINE));
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::Builder::new()
        .name("kiqi-extension-instantiate".to_string())
        .spawn(move || {
            let outcome =
                World::instantiate(&mut store, &component, &linker).map(|guest| (store, guest));
            let _ = tx.send(outcome);
        });
    match worker {
        Ok(handle) => drop(handle),
        Err(error) => {
            return Err(ExtensionError::Failed(format!(
                "instantiate {id} for {kind}: cannot start worker: {error}"
            )))
        }
    }
    match rx.recv_timeout(DISCOVERY_WAIT) {
        Ok(Ok(pair)) => Ok(pair),
        Ok(Err(error)) => Err(ExtensionError::Failed(format!(
            "instantiate {id} for {kind}: {error}"
        ))),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(ExtensionError::Failed(format!(
            "instantiate {id} for {kind}: exceeded the {}s instantiation deadline",
            DISCOVERY_WAIT.as_secs()
        ))),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(ExtensionError::Failed(
            format!("instantiate {id} for {kind}: worker ended without a result"),
        )),
    }
}

/// Advance every live render engine's epoch once per [`RENDER_TICK_MS`].
///
/// Epoch interruption is an **Engine** setting, so one shared ticker thread
/// ticks every render engine instead of one thread per instance. A config with
/// `epoch_interruption(true)` and no ticker would never fire; the ticker is
/// therefore part of the render gate, not an optimization.
struct RenderTicker {
    engines: Mutex<Vec<std::sync::Weak<Engine>>>,
}

impl RenderTicker {
    fn register(&self, engine: &std::sync::Arc<Engine>) {
        let mut engines = self
            .engines
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        engines.retain(|weak| weak.strong_count() > 0);
        engines.push(std::sync::Arc::downgrade(engine));
    }

    fn tick(&self) {
        let mut engines = self
            .engines
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        engines.retain(|weak| match weak.upgrade() {
            Some(engine) => {
                engine.increment_epoch();
                true
            }
            None => false,
        });
    }
}

static RENDER_TICKER: std::sync::OnceLock<Option<std::sync::Arc<RenderTicker>>> =
    std::sync::OnceLock::new();

/// The process-wide render ticker, started lazily.
///
/// `None` means the thread could not be spawned; [`RenderHost::new`] refuses to
/// create an instance in that case, because a deadline that can never fire is
/// worse than no renderer at all.
fn render_ticker() -> Option<&'static std::sync::Arc<RenderTicker>> {
    RENDER_TICKER
        .get_or_init(|| {
            let ticker = std::sync::Arc::new(RenderTicker {
                engines: Mutex::new(Vec::new()),
            });
            let worker = std::sync::Arc::clone(&ticker);
            let spawned = std::thread::Builder::new()
                .name("kiqi-render-epoch".to_string())
                .spawn(move || loop {
                    std::thread::sleep(std::time::Duration::from_millis(RENDER_TICK_MS));
                    worker.tick();
                });
            match spawned {
                Ok(_) => Some(ticker),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "cannot start the render epoch ticker; render extensions stay disabled"
                    );
                    None
                }
            }
        })
        .as_ref()
}

/// One instantiated markdown renderer.
///
/// Deliberately **not** a [`WasmExtension`]: the render instance has its own
/// [`Engine`] (epoch interruption enabled), its own [`Store`] whose
/// [`Grants`] are `Grants::default()` (deny everything), and it never calls
/// [`install_context`]. The guest call runs on a freshly spawned thread, whose
/// thread-local [`CURRENT`] starts empty — so `session-read`, `ui` and `exec`
/// are unreachable even though those imports are not grant-gated. The host
/// owns sanitization, the output cap and the deadline; the guest is a pure
/// text-to-text function by contract.
pub struct RenderHost {
    id: String,
    /// Kept alive for the instance's lifetime: the ticker holds only a
    /// `Weak<Engine>`, so dropping our `Arc` would stop every epoch tick and a
    /// guest loop would run forever.
    #[allow(dead_code)]
    engine: std::sync::Arc<Engine>,
    #[allow(dead_code)]
    component: Component,
    timeout: std::time::Duration,
    max_output_bytes: usize,
    inner: Mutex<Option<RenderInner>>,
    /// Set after a trap/deadline: the instance is not reused for the rest of
    /// the process (a trapped store may be mid-import, and the spec's answer to
    /// a trap is "fall back to the raw markdown for the session").
    disabled: std::sync::atomic::AtomicBool,
}

struct RenderInner {
    store: Store<HostState>,
    guest: World,
}

impl RenderHost {
    /// The markdown half of a `render` grant, if this grant can drive one.
    pub fn granted(grants: &Grants) -> Option<&RenderGrant> {
        grants
            .render
            .as_ref()
            .filter(|grant| grant.kinds.iter().any(|kind| kind == RENDER_KIND_MARKDOWN))
    }

    /// Load a renderer only when the grant actually authorizes `markdown`.
    /// `Ok(None)` is the deny-by-default answer: an absent grant means the
    /// guest is never instantiated, let alone called.
    pub fn load_with_grants(
        id: impl Into<String>,
        wasm: &Path,
        grants: &Grants,
    ) -> Result<Option<Self>, ExtensionError> {
        match Self::granted(grants) {
            Some(grant) => Self::new(id, wasm, grant).map(Some),
            None => Ok(None),
        }
    }

    /// Instantiate the component on a deny-all store.
    pub fn new(
        id: impl Into<String>,
        wasm: &Path,
        grant: &RenderGrant,
    ) -> Result<Self, ExtensionError> {
        let id = id.into();
        if !grant.kinds.iter().any(|kind| kind == RENDER_KIND_MARKDOWN) {
            return Err(ExtensionError::Failed(format!(
                "{id}: render grant does not include `{RENDER_KIND_MARKDOWN}`"
            )));
        }
        // Last-resort clamps: the CLI validates at grant time, but a hand-edited
        // `grants.json` is the only way in past it, and a renderer must still
        // stay bounded.
        let timeout_ms = grant.timeout_ms.clamp(1, RENDER_TIMEOUT_MS_MAX);
        let max_output_bytes = grant.max_output_bytes.clamp(1, RENDER_MAX_OUTPUT_BYTES_MAX);
        if timeout_ms != grant.timeout_ms || max_output_bytes != grant.max_output_bytes {
            tracing::warn!(
                extension = %id,
                "render grant budget is outside the host range; clamped to {timeout_ms}ms/\
                 {max_output_bytes} bytes"
            );
        }

        let ticker = render_ticker().ok_or_else(|| {
            ExtensionError::Failed(format!("{id}: render epoch ticker is unavailable"))
        })?;

        let mut config = wasmtime_config(&Options {
            fuel: Some(DISCOVERY_FUEL),
            ..Options::default()
        });
        config.epoch_interruption(true);
        let engine =
            std::sync::Arc::new(Engine::new(&config).map_err(|error| {
                ExtensionError::Failed(format!("{id}: render engine: {error}"))
            })?);
        ticker.register(&engine);
        let component = Component::from_file(&engine, wasm)
            .map_err(|error| ExtensionError::Failed(format!("load {}: {error}", wasm.display())))?;

        let mut linker = Linker::new(&engine);
        World::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |state| state).map_err(
            |error| ExtensionError::Failed(format!("link extension interfaces: {error}")),
        )?;
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|error| ExtensionError::Failed(format!("link wasi: {error}")))?;

        let limits = wasmtime::StoreLimitsBuilder::new()
            .memory_size(DEFAULT_MAX_MEMORY_BYTES)
            .build();
        let state = HostState {
            wasi: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            // Deny-all: a render instance holds no capability at all. It also
            // never gets a `CallContext`, so the non-granted channels stay
            // unreachable too.
            grants: Grants::default(),
            fs_root: None,
            project_dir: None,
            extension_id: None,
            declared_flags: std::sync::Arc::new(std::collections::BTreeSet::new()),
            sockets: std::collections::HashMap::new(),
            next_socket: 1,
            procs: std::collections::HashMap::new(),
            next_proc: 1,
            limits,
            oauth: OAuthState::default(),
            provider_http: provider_http::ProviderHttpTable::default(),
            provider_call: None,
        };
        let mut store = Store::new(&engine, state);
        store.limiter(|state| &mut state.limits);
        let (store, guest) = instantiate_bounded(&id, "render", store, component.clone(), linker)?;

        Ok(Self {
            id,
            engine,
            component,
            timeout: std::time::Duration::from_millis(timeout_ms),
            max_output_bytes: max_output_bytes as usize,
            inner: Mutex::new(Some(RenderInner { store, guest })),
            disabled: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// The id this renderer was loaded under (the install directory name).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Transform one final markdown string.
    ///
    /// `Err` carries exactly one diagnostic and means "keep the original
    /// text": a guest error, a trap, a deadline or a control character in the
    /// answer. The caller never sees a partially sanitized value.
    pub fn transform_markdown(&self, text: &str) -> Result<String, String> {
        use std::sync::atomic::Ordering;
        if self.disabled.load(Ordering::Acquire) {
            return Err(format!(
                "{}: render instance disabled after an earlier trap",
                self.id
            ));
        }
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let Some(mut inner) = guard.take() else {
            return Err(format!("{}: render instance is gone", self.id));
        };
        if let Err(error) = inner.store.set_fuel(u64::MAX) {
            *guard = Some(inner);
            return Err(format!("{}: reset render fuel: {error}", self.id));
        }

        let ticks = epoch_ticks(self.timeout);
        let deadline = self.timeout;
        let max_output_bytes = self.max_output_bytes;
        let input = text.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("kiqi-extension-render".to_string())
            .spawn(move || {
                let mut inner = inner;
                // This thread was just spawned, so `CURRENT` is empty and no
                // `CallContext` was ever installed: session/UI/exec return
                // their deny/empty answers by construction.
                inner.store.set_epoch_deadline(ticks);
                debug_assert!(
                    CURRENT.with(|slot| slot.borrow().is_none()),
                    "a render call must run without a host call context"
                );
                let outcome = inner
                    .guest
                    .kiqi_extension_render_api()
                    .call_transform_markdown(&mut inner.store, &input);
                // The receiver is gone on a deadline; the failed send drops the
                // abandoned store on this thread, which is exactly what we want.
                let _ = tx.send((outcome, inner));
            });
        match worker {
            // Detach the worker: a call that hits the deadline below must not
            // wait for the guest (that is the whole point of the wall clock).
            Ok(handle) => drop(handle),
            Err(error) => {
                // The instance was moved into the (failed) spawn; it is gone.
                self.disabled
                    .store(true, std::sync::atomic::Ordering::Release);
                return Err(format!(
                    "{}: cannot start the render thread: {error}",
                    self.id
                ));
            }
        }
        // A hard wall-clock deadline, not just the epoch: wasmtime checks the
        // epoch only at guest instruction boundaries, so a guest parked inside
        // a WASI host call (`clock`/`io` sleep) would otherwise pin this thread
        // — and the TUI event loop that awaits it — forever. On expiry the
        // instance stays with the abandoned worker, is marked disabled and is
        // never reused; the caller keeps the original markdown.
        let (outcome, inner) = match rx.recv_timeout(deadline) {
            Ok(pair) => pair,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                self.disabled.store(true, Ordering::Release);
                return Err(format!(
                    "{}: transform-markdown exceeded {}ms; renderer disabled",
                    self.id,
                    deadline.as_millis()
                ));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                self.disabled.store(true, Ordering::Release);
                return Err(format!("{}: render thread panicked", self.id));
            }
        };
        match outcome {
            Ok(Ok(transformed)) => {
                // A guest-level error keeps the instance: the guest returned
                // normally and may answer the next message.
                *guard = Some(inner);
                sanitize_render_output(&transformed, max_output_bytes)
                    .map_err(|diagnostic| format!("{}: {diagnostic}", self.id))
            }
            Ok(Err(error)) => {
                *guard = Some(inner);
                Err(format!("{}: transform-markdown failed: {error:?}", self.id))
            }
            Err(trap) => {
                self.disabled.store(true, Ordering::Release);
                // Drop the store: after a trap it must not be reused.
                drop(inner);
                Err(format!("{}: transform-markdown trapped: {trap}", self.id))
            }
        }
    }

    /// [`Self::transform_markdown`] with the host's fallback rule applied: any
    /// failure returns the original text, after one diagnostic.
    pub fn transform_markdown_or_original(&self, text: &str) -> String {
        render_or_original(text, self.transform_markdown(text))
    }
}

/// Apply a render result, falling back to the original on any failure.
///
/// This is the contract the CLI transformer relies on: an extension can never make
/// assistant output disappear or come back half-sanitized.
pub fn render_or_original(original: &str, result: Result<String, String>) -> String {
    match result {
        Ok(transformed) => transformed,
        Err(diagnostic) => {
            tracing::warn!(%diagnostic, "markdown render skipped; using the original text");
            original.to_string()
        }
    }
}

/// Sanitize a guest-returned markdown string and enforce the byte cap.
///
/// ANSI escape sequences are stripped (the TUI styles markdown itself; a guest
/// must not smuggle its own colors), then any remaining C0/C1 control character
/// other than newline/tab is a hard error. The cap truncates at a UTF-8
/// boundary rather than splitting a character.
pub fn sanitize_render_output(raw: &str, max_output_bytes: usize) -> Result<String, String> {
    let stripped = strip_ansi_codes(raw);
    if let Some((offset, character)) = stripped
        .char_indices()
        .find(|(_, character)| is_forbidden_control(*character))
    {
        return Err(format!(
            "transformed markdown contains control character U+{:04X} at byte {offset}",
            character as u32
        ));
    }
    Ok(truncate_utf8(&stripped, max_output_bytes))
}

/// Strip CSI/OSC/two-byte ANSI escape sequences.
fn strip_ansi_codes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            out.push(character);
            continue;
        }
        match chars.peek().copied() {
            // CSI: parameter/intermediate bytes, then a final byte 0x40..=0x7E.
            Some('[') => {
                chars.next();
                while let Some(next) = chars.next() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            // OSC: terminated by BEL or ST (ESC \).
            Some(']') => {
                chars.next();
                let mut previous = '\0';
                while let Some(next) = chars.next() {
                    if next == '\u{07}' || (previous == '\u{1b}' && next == '\\') {
                        break;
                    }
                    previous = next;
                }
            }
            // Any other escape: a two-byte sequence (e.g. ESC ( B is three; the
            // third byte is consumed on the next iteration as ordinary text,
            // which the control check then decides on).
            Some(_) => {
                chars.next();
            }
            // A dangling escape is not a sequence we can account for; keep it
            // so the control-character check rejects it instead of silently
            // dropping bytes.
            None => out.push(character),
        }
    }
    out
}

/// C0/C1 controls (and DEL) other than newline/tab are never layout-safe.
fn is_forbidden_control(character: char) -> bool {
    let code = character as u32;
    if character == '\n' || character == '\t' {
        return false;
    }
    code <= 0x1f || (0x7f..=0x9f).contains(&code)
}

/// Truncate to at most `max_bytes`, on a char boundary.
fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Unix epoch seconds, saturating at 0 for a clock before 1970.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(root: &std::path::Path) -> HostState {
        HostState {
            wasi: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            grants: Grants {
                fs: Some(crate::grants::FsGrant {
                    cwd: crate::grants::ExecCwd::ExtensionData,
                }),
                ..Grants::default()
            },
            fs_root: Some(root.to_path_buf()),
            project_dir: None,
            extension_id: Some("test".to_string()),
            declared_flags: std::sync::Arc::new(std::collections::BTreeSet::new()),
            sockets: std::collections::HashMap::new(),
            next_socket: 1,
            procs: std::collections::HashMap::new(),
            next_proc: 1,
            limits: wasmtime::StoreLimitsBuilder::new().build(),
            oauth: OAuthState::default(),
            provider_http: provider_http::ProviderHttpTable::default(),
            provider_call: None,
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
        let max = crate::grants::EXEC_TIMEOUT_MAX_SECONDS;
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
        let max = crate::grants::PROC_TIMEOUT_MAX_SECONDS;
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
        let max = crate::grants::PROC_MAX_OPEN_MAX;
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
        let wasm = dir.path().join("extension.wasm");
        std::fs::write(
            &wasm,
            synthetic_component(
                &["kiqi:extension/provider-http@0.8.0"],
                &["kiqi:extension/oauth-api@0.8.0"],
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
        let wasm = dir.path().join("extension.wasm");
        std::fs::write(
            &wasm,
            synthetic_component(
                &["kiqi:extension/exec@0.7.1", "kiqi:extension/fs@0.7.1"],
                &[
                    "kiqi:extension/render-api@0.8.0",
                    "kiqi:extension/log@0.8.0",
                ],
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
        let wasm = dir.path().join("extension.wasm");
        std::fs::write(
            &wasm,
            synthetic_component(
                &["kiqi:extension/provider-http@0.8.0"],
                &["kiqi:extension/provider-api@0.8.0"],
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
        let wasm = dir.path().join("extension.wasm");
        std::fs::write(
            &wasm,
            synthetic_component(&["kiqi:extension/net-http@0.7.1"], &[]),
        )
        .unwrap();

        assert_eq!(discover_providers(&wasm), Discovery::default());
    }

    /// The provider-request grant is the delivery switch: absent, or recorded
    /// without `observe = requests`, means the guest is not called at all.
    #[test]
    fn provider_observation_is_denied_without_an_observe_grant() {
        let event = Event::new(
            crate::EventTag::BeforeProviderRequest,
            serde_json::json!({ "model": "m", "messages": [{ "role": "user" }] }),
        );

        let mut grants = Grants::default();
        assert!(graded_provider_request(&event, &grants, false).is_none());

        grants.provider_request = Some(crate::grants::ProviderRequestGrant::default());
        assert!(
            graded_provider_request(&event, &grants, false).is_none(),
            "a grant with observe = none delivers nothing"
        );
    }

    /// `messages` is opt-in; the default replaces it with `messageCount`, and
    /// `headers = names` keeps values out even if a future bridge sends them.
    #[test]
    fn provider_observation_strips_messages_and_header_values() {
        let event = Event::new(
            crate::EventTag::BeforeProviderRequest,
            serde_json::json!({
                "model": "m",
                "apiKeyPresent": true,
                "headerNames": ["authorization", "x-api-key", "x-trace"],
                "headerValues": {
                    "authorization": "secret",
                    "X-API-Key": "api-secret",
                    "x-trace": "abc",
                },
                "messages": [{ "role": "user" }, { "role": "assistant" }],
            }),
        );
        let mut grants = Grants::default();
        grants.provider_request = Some(crate::grants::ProviderRequestGrant {
            observe: ObserveMode::Requests,
            ..crate::grants::ProviderRequestGrant::default()
        });

        let Some(WitEvent::BeforeProviderRequest(payload)) =
            graded_provider_request(&event, &grants, false)
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
        grants.provider_request = Some(crate::grants::ProviderRequestGrant {
            observe: ObserveMode::Requests,
            messages: MessagesMode::Full,
            headers: HeadersMode::Values,
            ..crate::grants::ProviderRequestGrant::default()
        });
        let Some(WitEvent::BeforeProviderRequest(payload)) =
            graded_provider_request(&event, &grants, false)
        else {
            panic!("the observe grant must deliver");
        };
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["messages"].as_array().unwrap().len(), 2);
        // `values` does not lift the credential boundary: denied names and
        // every name outside the non-credential allowlist are stripped.
        assert!(
            payload["headerValues"].get("authorization").is_none(),
            "authorization must never cross the ABI: {payload}"
        );
        assert!(
            payload["headerValues"].get("X-API-Key").is_none(),
            "the deny-list is case-insensitive: {payload}"
        );
        assert_eq!(payload["headerValues"]["x-trace"], "abc");
        assert_eq!(payload["messageCount"], 2);
    }

    /// A payload without messages still reports a zero count rather than
    /// omitting the field.
    #[test]
    fn provider_observation_always_reports_a_message_count() {
        let event = Event::new(
            crate::EventTag::BeforeProviderRequest,
            serde_json::json!({ "model": "m" }),
        );
        let mut grants = Grants::default();
        grants.provider_request = Some(crate::grants::ProviderRequestGrant {
            observe: ObserveMode::Requests,
            ..crate::grants::ProviderRequestGrant::default()
        });
        let Some(WitEvent::BeforeProviderRequest(payload)) =
            graded_provider_request(&event, &grants, false)
        else {
            panic!("the observe grant must deliver");
        };
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["messageCount"], 0);
    }

    /// `headers = values` is an allowlist, not permission to disclose every
    /// nonstandard provider credential.
    #[test]
    fn provider_observation_header_values_use_a_non_credential_allowlist() {
        let event = Event::new(
            crate::EventTag::BeforeProviderRequest,
            serde_json::json!({
                "headerNames": ["x-portkey-key", "private-token", "x-custom", "traceparent"],
                "headerValues": {
                    "x-portkey-key": "portkey-secret",
                    "private-token": "private-secret",
                    "x-custom": "unlisted-value",
                    "traceparent": "00-abc-def-01",
                },
            }),
        );
        let mut grants = Grants::default();
        grants.provider_request = Some(crate::grants::ProviderRequestGrant {
            observe: ObserveMode::Requests,
            headers: HeadersMode::Values,
            ..crate::grants::ProviderRequestGrant::default()
        });

        let Some(WitEvent::BeforeProviderRequest(payload)) =
            graded_provider_request(&event, &grants, false)
        else {
            panic!("the observe grant must deliver");
        };
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let values = payload["headerValues"].as_object().unwrap();
        assert_eq!(values.get("traceparent").unwrap(), "00-abc-def-01");
        assert_eq!(values.len(), 1, "only allowlisted values may cross");
        let serialized = payload.to_string();
        for secret in ["portkey-secret", "private-secret", "unlisted-value"] {
            assert!(!serialized.contains(secret), "leaked {secret}: {payload}");
        }
    }

    // -----------------------------------------------------------------------
    // oauth-http
    // -----------------------------------------------------------------------

    use crate::grants::{OAuthFlow, OAuthGrant, OAuthProviderProfile};
    use crate::wasm::kiqi::extension::oauth_http::Host as _;

    fn oauth_profile() -> OAuthProviderProfile {
        OAuthProviderProfile {
            id: "acme".to_string(),
            display_name: "Acme".to_string(),
            scopes: vec!["openid".to_string()],
            flows: vec![OAuthFlow::AuthorizationCode, OAuthFlow::DeviceCode],
            authorization_endpoint: "https://id.acme.test/authorize".to_string(),
            token_endpoint: "https://id.acme.test/token".to_string(),
            device_authorization_endpoint: Some("https://id.acme.test/device".to_string()),
            revocation_endpoint: Some("https://id.acme.test/revoke".to_string()),
            client_id: "client-123".to_string(),
            supports_pkce: true,
        }
    }

    fn oauth_grants(profile: OAuthProviderProfile) -> Grants {
        Grants {
            oauth: Some(OAuthGrant {
                providers: vec![profile],
                timeout_seconds: 60,
                max_requests: 5,
            }),
            ..Grants::default()
        }
    }

    #[derive(Debug, Default)]
    struct RecordingSink {
        stored: Mutex<Vec<(String, String, String, String, i64, String)>>,
        token: Mutex<Option<StoredOAuthToken>>,
        deleted: Mutex<Vec<(String, String)>>,
    }

    impl ExtensionCredentialSink for RecordingSink {
        fn store_extension_oauth(
            &self,
            install_id: &str,
            provider_id: &str,
            access: &str,
            refresh: &str,
            expires: i64,
            profile_fingerprint: &str,
        ) -> Result<(), String> {
            self.stored.lock().unwrap().push((
                install_id.to_string(),
                provider_id.to_string(),
                access.to_string(),
                refresh.to_string(),
                expires,
                profile_fingerprint.to_string(),
            ));
            *self.token.lock().unwrap() = Some(StoredOAuthToken {
                access: access.to_string(),
                refresh: refresh.to_string(),
                expires,
                profile_fingerprint: Some(profile_fingerprint.to_string()),
            });
            Ok(())
        }

        fn read_extension_oauth(
            &self,
            _install_id: &str,
            _provider_id: &str,
        ) -> Result<Option<StoredOAuthToken>, String> {
            Ok(self.token.lock().unwrap().clone())
        }

        fn delete_extension_oauth(
            &self,
            install_id: &str,
            provider_id: &str,
        ) -> Result<bool, String> {
            self.deleted
                .lock()
                .unwrap()
                .push((install_id.to_string(), provider_id.to_string()));
            Ok(true)
        }
    }

    #[derive(Debug, Default)]
    struct ScriptedTransport {
        responses: Mutex<Vec<HttpResponse>>,
        requests: Mutex<Vec<(String, Vec<(String, String)>)>>,
    }

    impl ScriptedTransport {
        fn with(responses: Vec<HttpResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<(String, Vec<(String, String)>)> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl OAuthTransport for ScriptedTransport {
        fn post_form(
            &self,
            url: &str,
            form: &[(String, String)],
            _timeout: std::time::Duration,
        ) -> Result<HttpResponse, String> {
            self.requests
                .lock()
                .unwrap()
                .push((url.to_string(), form.to_vec()));
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Err("no scripted response".to_string());
            }
            Ok(responses.remove(0))
        }
    }

    fn oauth_state(
        grants: Grants,
        transport: std::sync::Arc<dyn OAuthTransport>,
        sink: std::sync::Arc<dyn ExtensionCredentialSink>,
    ) -> HostState {
        HostState {
            wasi: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            grants,
            fs_root: None,
            project_dir: None,
            extension_id: Some("install-1".to_string()),
            declared_flags: std::sync::Arc::new(std::collections::BTreeSet::new()),
            sockets: std::collections::HashMap::new(),
            next_socket: 1,
            procs: std::collections::HashMap::new(),
            next_proc: 1,
            limits: wasmtime::StoreLimitsBuilder::new().build(),
            oauth: OAuthState {
                budget: OAuthRequestBudget::new(60, 5),
                transport: Some(transport),
                sink: Some(sink),
            },
            provider_http: provider_http::ProviderHttpTable::default(),
            provider_call: None,
        }
    }

    #[test]
    fn oauth_http_denies_every_provider_without_a_grant_or_with_the_wrong_id() {
        let transport = std::sync::Arc::new(ScriptedTransport::default());
        let sink = std::sync::Arc::new(RecordingSink::default());
        let mut state = oauth_state(Grants::default(), transport.clone(), sink);

        let error = state
            .token_request("acme".to_string(), "{}".to_string())
            .unwrap_err();
        assert!(
            matches!(&error, CapabilityError::Denied(message) if message.contains("not granted")),
            "{error:?}"
        );

        let transport = std::sync::Arc::new(ScriptedTransport::default());
        let sink = std::sync::Arc::new(RecordingSink::default());
        let mut state = oauth_state(oauth_grants(oauth_profile()), transport.clone(), sink);
        let error = state
            .token_request("other".to_string(), "{}".to_string())
            .unwrap_err();
        assert!(
            matches!(&error, CapabilityError::Denied(message) if message.contains("not granted")),
            "{error:?}"
        );
        assert!(transport.requests().is_empty());
    }

    #[test]
    fn oauth_http_form_allow_list_refuses_the_secret_and_unknown_keys() {
        let transport = std::sync::Arc::new(ScriptedTransport::default());
        let sink = std::sync::Arc::new(RecordingSink::default());
        let mut state = oauth_state(oauth_grants(oauth_profile()), transport.clone(), sink);

        let error = state
            .token_request(
                "acme".to_string(),
                r#"{"grant_type":"authorization_code","client_secret":"shh"}"#.to_string(),
            )
            .unwrap_err();
        assert!(
            matches!(&error, CapabilityError::Denied(message) if message.contains("client_secret")),
            "{error:?}"
        );

        let error = state
            .token_request("acme".to_string(), r#"{"token":"stolen"}"#.to_string())
            .unwrap_err();
        assert!(
            matches!(&error, CapabilityError::Denied(message) if message.contains("not allowed")),
            "{error:?}"
        );

        // Policy refusals never reach the network.
        assert!(transport.requests().is_empty());
    }

    #[test]
    fn oauth_http_refuses_a_flow_outside_the_profile_before_http() {
        let mut profile = oauth_profile();
        profile.flows = vec![OAuthFlow::AuthorizationCode];
        let transport = std::sync::Arc::new(ScriptedTransport::default());
        let sink = std::sync::Arc::new(RecordingSink::default());
        let mut state = oauth_state(oauth_grants(profile), transport.clone(), sink);

        let error = state
            .token_request(
                "acme".to_string(),
                r#"{"grant_type":"urn:ietf:params:oauth:grant-type:device_code","device_code":"device-secret"}"#
                    .to_string(),
            )
            .unwrap_err();
        assert!(
            matches!(&error, CapabilityError::Denied(message) if message.contains("device-code") && message.contains("flow")),
            "{error:?}"
        );
        assert!(!format!("{error:?}").contains("device-secret"));
        assert!(transport.requests().is_empty());
    }

    #[test]
    fn oauth_http_refuses_a_scope_outside_the_profile_before_http() {
        let transport = std::sync::Arc::new(ScriptedTransport::default());
        let sink = std::sync::Arc::new(RecordingSink::default());
        let mut state = oauth_state(oauth_grants(oauth_profile()), transport.clone(), sink);

        let error = state
            .token_request(
                "acme".to_string(),
                r#"{"code":"abc","scope":"openid admin"}"#.to_string(),
            )
            .unwrap_err();
        assert!(
            matches!(&error, CapabilityError::Denied(message) if message.contains("scope")),
            "{error:?}"
        );
        assert!(!format!("{error:?}").contains("admin"));
        assert!(transport.requests().is_empty());
    }

    #[test]
    fn oauth_http_stores_under_the_install_and_provider_ids_and_hides_the_token() {
        let transport = std::sync::Arc::new(ScriptedTransport::with(vec![HttpResponse {
            status: 200,
            location: None,
            body: r#"{"access_token":"at-secret","refresh_token":"rt-secret","expires_in":120}"#
                .to_string(),
        }]));
        let sink = std::sync::Arc::new(RecordingSink::default());
        let mut state = oauth_state(
            oauth_grants(oauth_profile()),
            transport.clone(),
            sink.clone(),
        );

        let outcome = state
            .token_request(
                "acme".to_string(),
                r#"{"grant_type":"authorization_code","code":"abc","code_verifier":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","redirect_uri":"https://app.test/cb"}"#
                    .to_string(),
            )
            .unwrap();
        match outcome {
            OauthTokenOutcome::Stored(status) => assert!(status.expires_at > 0),
            other => panic!("expected stored, got {other:?}"),
        }
        // The outcome the guest sees carries no token material.
        assert!(!format!("{outcome:?}").contains("secret"));

        let stored = sink.stored.lock().unwrap().clone();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].0, "install-1");
        assert_eq!(stored[0].1, "acme");
        assert_eq!(stored[0].2, "at-secret");
        assert_eq!(stored[0].3, "rt-secret");
        assert_eq!(
            stored[0].5,
            oauth_profile_fingerprint(&oauth_profile()).unwrap()
        );

        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, "https://id.acme.test/token");
        let form = &requests[0].1;
        assert!(form
            .iter()
            .any(|(key, value)| key == "client_id" && value == "client-123"));
        assert!(!form.iter().any(|(key, _)| key == "client_secret"));
    }

    #[test]
    fn oauth_http_refuses_a_redirect_to_another_host() {
        let transport = std::sync::Arc::new(ScriptedTransport::with(vec![HttpResponse {
            status: 302,
            location: Some("https://evil.example/token".to_string()),
            body: String::new(),
        }]));
        let sink = std::sync::Arc::new(RecordingSink::default());
        let mut state = oauth_state(
            oauth_grants(oauth_profile()),
            transport.clone(),
            sink.clone(),
        );

        let outcome = state
            .token_request("acme".to_string(), r#"{"code":"abc"}"#.to_string())
            .unwrap();
        match outcome {
            OauthTokenOutcome::Failed(message) => {
                assert!(message.contains("evil.example"), "{message}");
                assert!(message.contains("refused"), "{message}");
            }
            other => panic!("expected failed, got {other:?}"),
        }
        assert!(sink.stored.lock().unwrap().is_empty());
    }

    #[test]
    fn oauth_http_revoke_uses_the_stored_token_and_deletes_it() {
        let sink = std::sync::Arc::new(RecordingSink::default());
        *sink.token.lock().unwrap() = Some(StoredOAuthToken {
            access: "at-secret".to_string(),
            refresh: "rt-secret".to_string(),
            expires: 0,
            profile_fingerprint: Some(oauth_profile_fingerprint(&oauth_profile()).unwrap()),
        });
        let transport = std::sync::Arc::new(ScriptedTransport::with(vec![HttpResponse {
            status: 200,
            location: None,
            body: String::new(),
        }]));
        let mut state = oauth_state(
            oauth_grants(oauth_profile()),
            transport.clone(),
            sink.clone(),
        );

        state.revoke("acme".to_string()).unwrap();
        let requests = transport.requests();
        assert_eq!(requests[0].0, "https://id.acme.test/revoke");
        let form = &requests[0].1;
        assert!(form
            .iter()
            .any(|(key, value)| key == "token" && value == "rt-secret"));
        assert!(form
            .iter()
            .any(|(key, value)| key == "token_type_hint" && value == "refresh_token"));
        assert_eq!(
            sink.deleted.lock().unwrap().clone(),
            vec![("install-1".to_string(), "acme".to_string())]
        );
    }

    #[test]
    fn oauth_http_revoke_refuses_a_changed_profile_without_a_request_or_delete() {
        let original = oauth_profile();
        let mut changed = original.clone();
        changed.token_endpoint = "https://id.other.test/token".to_string();
        changed.revocation_endpoint = Some("https://id.other.test/revoke".to_string());
        let sink = std::sync::Arc::new(RecordingSink::default());
        *sink.token.lock().unwrap() = Some(StoredOAuthToken {
            access: "at-secret".to_string(),
            refresh: "rt-secret".to_string(),
            expires: 0,
            profile_fingerprint: Some(oauth_profile_fingerprint(&original).unwrap()),
        });
        let transport = std::sync::Arc::new(ScriptedTransport::default());
        let mut state = oauth_state(oauth_grants(changed), transport.clone(), sink.clone());

        let error = state.revoke("acme".to_string()).unwrap_err();
        assert!(matches!(error, CapabilityError::Denied(_)), "{error:?}");
        assert!(format!("{error:?}").contains("different issuing profile"));
        assert!(!format!("{error:?}").contains("at-secret"));
        assert!(!format!("{error:?}").contains("rt-secret"));
        assert!(transport.requests().is_empty());
        assert!(sink.deleted.lock().unwrap().is_empty());
        assert!(sink.token.lock().unwrap().is_some());
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    #[test]
    fn sanitize_strips_ansi_and_keeps_newline_and_tab() {
        let raw = "a\u{1b}[31mred\u{1b}[0m\tb\nc\u{1b}]0;title\u{07}d";
        assert_eq!(sanitize_render_output(raw, 1024).unwrap(), "ared\tb\ncd");
    }

    #[test]
    fn sanitize_rejects_c0_c1_and_del() {
        for (name, raw) in [
            ("nul", "bad\u{0}"),
            ("bell", "bad\u{7}"),
            ("escape", "bad\u{1b}"),
            ("del", "bad\u{7f}"),
            ("c1", "bad\u{85}"),
            ("carriage-return", "bad\r"),
        ] {
            assert!(
                sanitize_render_output(raw, 1024).is_err(),
                "{name} must be rejected"
            );
        }
    }

    #[test]
    fn sanitize_truncates_at_a_char_boundary() {
        // "é" is two bytes, so a 3-byte cap must not split it.
        let out = sanitize_render_output("aéé", 3).unwrap();
        assert_eq!(out, "aé");
        assert!(out.len() <= 3);
        assert_eq!(sanitize_render_output("short", 1024).unwrap(), "short");
    }

    #[test]
    fn failure_passthrough_keeps_the_original_text() {
        assert_eq!(
            render_or_original("original", Err("guest trapped".to_string())),
            "original"
        );
        assert_eq!(
            render_or_original("original", Ok("transformed".to_string())),
            "transformed"
        );
    }

    #[test]
    fn grant_gating_needs_a_markdown_render_grant() {
        assert!(RenderHost::granted(&Grants::default()).is_none());
        let mut grants = Grants::default();
        grants.render = Some(RenderGrant {
            kinds: vec!["something-else".to_string()],
            ..RenderGrant::default()
        });
        assert!(RenderHost::granted(&grants).is_none());
        grants.render = Some(RenderGrant::default());
        assert!(RenderHost::granted(&grants).is_some());
    }
}

#[cfg(test)]
mod guest_error_sink_tests {
    use super::{emit_guest_error, set_guest_error_sink};

    /// P0 诊断汇不再依赖 tracing subscriber：嵌入方装了 sink 之后，guest
    /// 失败必须出现在 sink 里（最终复审 round-4 的 blocking 1）。
    #[test]
    fn guest_error_sink_receives_emitted_diagnostics() {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let for_sink = std::sync::Arc::clone(&captured);
        set_guest_error_sink(std::sync::Arc::new(
            move |extension: &str, op: &str, error: &str| {
                for_sink
                    .lock()
                    .unwrap()
                    .push(format!("{extension}/{op}/{error}"));
            },
        ));
        emit_guest_error("test-extension", "list-commands", "boom");
        let got = captured.lock().unwrap();
        assert!(
            got.iter()
                .any(|line| line == "test-extension/list-commands/boom"),
            "sink must capture the diagnostic verbatim: {got:?}"
        );
    }
}
