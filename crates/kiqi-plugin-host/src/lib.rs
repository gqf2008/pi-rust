//! Host-side backend for **wasm plugins** (the component model tier).
//!
//! Binds `plugins/kiqi-plugin.wit` (ABI 0.6.0) with wasmtime's `bindgen!` and
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
pub use kiqi_plugin::grants::{allows, ExecGrant, Grants, NetHttpGrant, NetSocketsGrant};

/// The user-grantable capabilities a component declares — the `kiqi:plugin/*`
/// imports that need authorization (`net-http`, `fs`, `exec`, `net-sockets`).
///
/// The domain interfaces (`log`, `session-read`, …) are always available through
/// `HostCaps` and are not part of the grant set, so they are filtered out. This
/// is what `ki plugin install` shows the user and re-prompts about when it grows.
pub fn declared_capabilities(wasm: &Path) -> Result<Vec<String>, String> {
    const GRANTABLE: &[&str] = &["net-http", "fs", "exec", "net-sockets"];
    let bytes = std::fs::read(wasm).map_err(|error| format!("read {}: {error}", wasm.display()))?;
    let mut caps: Vec<String> = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(&bytes) {
        if let wasmparser::Payload::ComponentImportSection(reader) =
            payload.map_err(|error| error.to_string())?
        {
            for import in reader {
                let import = import.map_err(|error| error.to_string())?;
                let Some(rest) = import.name.name.strip_prefix("kiqi:plugin/") else {
                    continue;
                };
                let cap = rest.split('@').next().unwrap_or(rest);
                if GRANTABLE.contains(&cap) && !caps.iter().any(|seen| seen == cap) {
                    caps.push(cap.to_string());
                }
            }
        }
    }
    caps.sort();
    Ok(caps)
}

/// The capabilities this build of the host actually enforces.
///
/// [`declared_capabilities`] reports what a *component* asks for; this reports
/// what the host can honour. `ki plugin install` names any difference, so a user
/// is never asked to grant something that would only ever come back denied.
/// `net-sockets` and `exec` are declared in the ABI but still deny stubs.
pub const ENFORCED_CAPABILITIES: &[&str] = &["net-http", "fs", "exec", "net-sockets"];

/// See [`ENFORCED_CAPABILITIES`].
pub fn capability_is_enforced(name: &str) -> bool {
    ENFORCED_CAPABILITIES.contains(&name)
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
    /// Raw TCP connections, keyed by the handle the guest was handed.
    sockets: std::collections::HashMap<u32, std::net::TcpStream>,
    next_socket: u32,
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
    /// Resolve a guest-supplied relative path inside the plugin's data
    /// directory, or explain why it may not.
    ///
    /// A path with any `..`, root or prefix component is refused outright, so a
    /// plugin cannot walk out of its jail — and `starts_with` is checked as well
    /// rather than relied on.
    fn fs_path(&self, rel: &str) -> Result<std::path::PathBuf, CapabilityError> {
        if !self.grants.fs {
            return Err(CapabilityError::Denied("fs is not granted".to_string()));
        }
        self.jail(rel)
    }

    /// Resolve a relative path inside the plugin's data directory without any
    /// capability check, for callers whose own grant already gates them
    /// (`exec`'s cwd).
    fn jail(&self, rel: &str) -> Result<std::path::PathBuf, CapabilityError> {
        let root = self.data_dir()?;
        let candidate = std::path::Path::new(rel);
        let mut resolved = root.clone();
        for component in candidate.components() {
            match component {
                std::path::Component::Normal(part) => resolved.push(part),
                std::path::Component::CurDir => {}
                _ => {
                    return Err(CapabilityError::Failed(format!(
                        "path `{rel}` must be relative and stay inside the plugin directory"
                    )))
                }
            }
        }
        if !resolved.starts_with(&root) {
            return Err(CapabilityError::Failed(format!(
                "path `{rel}` escapes the plugin directory"
            )));
        }
        Ok(resolved)
    }

    fn data_dir(&self) -> Result<std::path::PathBuf, CapabilityError> {
        self.fs_root
            .clone()
            .ok_or_else(|| CapabilityError::Failed("this plugin has no data directory".to_string()))
    }

    /// Re-check a lexically-jailed path against symlinks.
    ///
    /// The lexical check is not a jail on its own: a symlink left in the data
    /// directory (by `exec`, or by the user) would be followed straight out of
    /// it. So the parent is canonicalized and must still sit inside the
    /// canonical root, and the final component must not itself be a link.
    fn symlink_checked(
        &self,
        resolved: &std::path::Path,
        create_parents: bool,
    ) -> Result<std::path::PathBuf, CapabilityError> {
        let root = self.data_dir()?;
        // Create it first: `canonicalize` needs the directory to exist, and
        // falling back to the un-canonicalized path would compare a symlinked
        // root (`/var` vs `/private/var` on macOS) against a canonicalized
        // parent and reject every path.
        std::fs::create_dir_all(&root).map_err(|error| {
            CapabilityError::Failed(format!("create {}: {error}", root.display()))
        })?;
        let root = root.canonicalize().unwrap_or(root);
        let parent = resolved.parent().unwrap_or(&root);
        if create_parents {
            std::fs::create_dir_all(parent).map_err(|error| {
                CapabilityError::Failed(format!("create {}: {error}", parent.display()))
            })?;
        }
        let parent = parent.canonicalize().map_err(|error| {
            CapabilityError::Failed(format!("resolve {}: {error}", parent.display()))
        })?;
        if !parent.starts_with(&root) {
            return Err(CapabilityError::Failed(
                "path escapes the plugin directory".to_string(),
            ));
        }
        let name = resolved
            .file_name()
            .ok_or_else(|| CapabilityError::Failed("path has no file name".to_string()))?;
        let target = parent.join(name);
        if let Ok(metadata) = std::fs::symlink_metadata(&target) {
            if metadata.file_type().is_symlink() {
                return Err(CapabilityError::Failed(
                    "refusing to follow a symlink inside the plugin directory".to_string(),
                ));
            }
        }
        Ok(target)
    }
}

/// Plugin filesystem, jailed to `plugins/<id>/data/`.
impl crate::kiqi::plugin::fs::Host for HostState {
    fn read(&mut self, rel_path: String) -> Result<Vec<u8>, CapabilityError> {
        let path = self.symlink_checked(&self.fs_path(&rel_path)?, false)?;
        std::fs::read(&path)
            .map_err(|error| CapabilityError::Failed(format!("read {}: {error}", path.display())))
    }

    fn write(&mut self, rel_path: String, contents: Vec<u8>) -> Result<(), CapabilityError> {
        let path = self.symlink_checked(&self.fs_path(&rel_path)?, true)?;
        std::fs::write(&path, contents)
            .map_err(|error| CapabilityError::Failed(format!("write {}: {error}", path.display())))
    }

    fn read_dir(&mut self, rel_dir: String) -> Result<Vec<String>, CapabilityError> {
        let path = self.symlink_checked(&self.fs_path(&rel_dir)?, false)?;
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

/// Run a child process: allow-listed bare command name, jailed cwd, cleared
/// environment, capped output, wall-clock timeout. No shell is involved, so
/// shell metacharacters in `args` stay literal.
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
        let cwd = match cwd {
            Some(rel) => self.jail(&rel)?,
            None => self.data_dir()?,
        };
        std::fs::create_dir_all(&cwd).map_err(|error| {
            CapabilityError::Failed(format!("create {}: {error}", cwd.display()))
        })?;
        run_with_limits(&cmd, &args, &cwd)
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

/// Spawn, capture output with a byte cap, and enforce a wall-clock deadline.
fn run_with_limits(
    cmd: &str,
    args: &[String],
    cwd: &std::path::Path,
) -> Result<String, CapabilityError> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    const OUTPUT_CAP: u64 = 256 * 1024;
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
    let stdout_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.by_ref().take(OUTPUT_CAP).read_to_end(&mut buffer);
        buffer
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stderr.by_ref().take(OUTPUT_CAP).read_to_end(&mut buffer);
        buffer
    });

    let deadline = std::time::Instant::now() + TIMEOUT;
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
}

/// The default linear-memory cap for a guest: 256 MiB, which is far above what
/// any real plugin needs and far below the wasm32 address space.
pub const DEFAULT_MAX_MEMORY_BYTES: usize = 256 * 1024 * 1024;

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
                sockets: std::collections::HashMap::new(),
                next_socket: 1,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn state(root: &std::path::Path) -> HostState {
        HostState {
            wasi: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            grants: Grants {
                fs: true,
                ..Grants::default()
            },
            fs_root: Some(root.to_path_buf()),
            sockets: std::collections::HashMap::new(),
            next_socket: 1,
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
        state.grants.fs = false;
        let error = state.fs_path("x").expect_err("denied");
        assert!(matches!(error, CapabilityError::Denied(_)), "got {error:?}");
    }
}
