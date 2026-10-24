//! A dedicated instance for one granted provider profile.
//!
//! A provider call is different in kind from a tool call: it streams for
//! minutes, it needs its own HTTP channel, and it must never hold the mutex a
//! tool call, an event dispatch or a shortcut lookup needs. [`ProviderHost`] is
//! therefore **not** a [`crate::WasmPlugin`]: it loads the same component onto
//! its own [`Engine`]/[`Store`] with a deny-all [`Grants`] and only the one
//! `provider` profile, and it installs the credential-backed HTTP state for the
//! duration of each `provider-api.request`.
//!
//! Isolation, in order of what makes it safe:
//!   * with no `provider` grant naming this install id, [`ProviderHost::new`]
//!     returns `Ok(None)` and the component is **never instantiated**;
//!   * the store's grants are `Grants::default()`, so `fs`/`exec`/`net-http`/
//!     `oauth` are all denied regardless of what the guest imports;
//!   * `provider-http` is reachable only while a request call is on the stack
//!     (the thread-local [`crate::CallKind::ProviderRequest`] plus the
//!     grant-backed call state);
//!   * each call runs on a fresh thread with an epoch deadline, and the caller
//!     also waits with a wall-clock timeout, so a spinning guest cannot pin the
//!     caller even if it never traps.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use kiqi_plugin::grants::{Grants, ProviderModel, ProviderProfile};
use kiqi_plugin::{CancelProbe, HostCaps, Log, LogLevel, PluginError};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::WasiCtxBuilder;

use crate::kiqi::plugin::types::{
    DeltaKind as WitDeltaKind, DeltaPhase as WitDeltaPhase, ProviderDelta as WitDelta,
    ProviderSpec as WitProviderSpec,
};
use crate::provider_http::{ProviderCallState, ProviderHttpTable, ProviderHttpTransport};
use crate::{CallKind, HostState, World};

/// Which part of the response a delta belongs to (host-side mirror of the WIT
/// enum, so callers do not depend on the generated bindings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeltaKind {
    Text,
    Thinking,
    ToolCall,
}

/// Where in a block's lifecycle a delta sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaPhase {
    Start,
    Delta,
    End,
}

/// One assembled content block: all deltas for one `(kind, content-index)`,
/// concatenated in emission order.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderStreamSegment {
    pub kind: DeltaKind,
    pub content_index: u32,
    pub text: String,
}

/// The validated outcome of one `provider-api.request`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderResponse {
    /// The guest's final assistant message, with `provider`/`model`/`api`
    /// overwritten from the grant so a guest cannot impersonate another model.
    pub message: String,
    pub provider: String,
    pub model: String,
    pub api: String,
    /// Streamed deltas, in first-emission order. Empty for a single-shot
    /// provider.
    pub segments: Vec<ProviderStreamSegment>,
}

/// Load-time knobs for a provider instance.
#[derive(Clone, Default)]
pub struct ProviderHostOptions {
    /// The HTTP seam for `provider-http`. `None` uses real TLS reqwest; tests
    /// inject a fake so policy and streaming are asserted without a network.
    pub transport: Option<Arc<dyn ProviderHttpTransport>>,
    /// Where `auth.json` lives, for credential resolution. `None` derives it
    /// from the component path (`plugins/<id>/../..`).
    pub agent_root: Option<PathBuf>,
    /// Optional fuel budget for the guest.
    pub fuel: Option<u64>,
    /// Optional linear-memory cap; `None` uses the normal default.
    pub max_memory_bytes: Option<usize>,
}

/// The single logger a provider call gets. Everything else is denied.
struct ProviderLog;

impl Log for ProviderLog {
    fn log(&self, level: LogLevel, message: &str) {
        tracing::debug!(?level, %message, "provider guest log");
    }
}

static PROVIDER_LOG: ProviderLog = ProviderLog;

/// The delta assembler one provider call owns.
///
/// The host validates order here rather than trusting the guest: a duplicate
/// start, a delta before its start, a delta after its end, a reused content
/// index or an unterminated stream fails the whole request. That keeps the
/// caller's partial-message model well founded.
#[derive(Default)]
pub(crate) struct DeltaAssembler {
    segments: Vec<ProviderStreamSegment>,
    /// `(kind, index) -> (segment position, still open)`. A closed entry stays
    /// so a late delta is an error, not a silent append.
    by_key: HashMap<(DeltaKind, u32), (usize, bool)>,
    /// `content-index -> kind`, so one index cannot label two blocks.
    index_kind: HashMap<u32, DeltaKind>,
    error: Option<String>,
}

impl DeltaAssembler {
    pub(crate) fn push(&mut self, delta: WitDelta) {
        if self.error.is_some() {
            return;
        }
        let kind = match delta.kind {
            WitDeltaKind::Text => DeltaKind::Text,
            WitDeltaKind::Thinking => DeltaKind::Thinking,
            WitDeltaKind::ToolCall => DeltaKind::ToolCall,
        };
        let phase = match delta.phase {
            WitDeltaPhase::Start => DeltaPhase::Start,
            WitDeltaPhase::Delta => DeltaPhase::Delta,
            WitDeltaPhase::End => DeltaPhase::End,
        };
        let index = delta.content_index;
        match phase {
            DeltaPhase::Start => {
                if self.by_key.contains_key(&(kind, index)) {
                    self.fail(format!("duplicate start for {kind:?} block {index}"));
                    return;
                }
                if let Some(existing) = self.index_kind.get(&index) {
                    self.fail(format!(
                        "content index {index} was already used for {existing:?}"
                    ));
                    return;
                }
                self.index_kind.insert(index, kind);
                let position = self.segments.len();
                self.segments.push(ProviderStreamSegment {
                    kind,
                    content_index: index,
                    text: delta.text,
                });
                self.by_key.insert((kind, index), (position, true));
            }
            DeltaPhase::Delta => {
                let Some((position, open)) = self.by_key.get(&(kind, index)).copied() else {
                    self.fail(format!("delta before start for {kind:?} block {index}"));
                    return;
                };
                if !open {
                    self.fail(format!("delta after end for {kind:?} block {index}"));
                    return;
                }
                self.segments[position].text.push_str(&delta.text);
            }
            DeltaPhase::End => {
                let Some((position, open)) = self.by_key.get(&(kind, index)).copied() else {
                    self.fail(format!("end before start for {kind:?} block {index}"));
                    return;
                };
                if !open {
                    self.fail(format!("duplicate end for {kind:?} block {index}"));
                    return;
                }
                self.segments[position].text.push_str(&delta.text);
                self.by_key.insert((kind, index), (position, false));
            }
        }
    }

    fn fail(&mut self, message: String) {
        if self.error.is_none() {
            self.error = Some(message);
        }
    }

    pub(crate) fn finish(self) -> Result<Vec<ProviderStreamSegment>, String> {
        if let Some(error) = self.error {
            return Err(format!("provider stream: {error}"));
        }
        if let Some((kind, index)) = self
            .by_key
            .iter()
            .find_map(|((kind, index), (_, open))| open.then_some((*kind, *index)))
        {
            return Err(format!(
                "provider stream: {kind:?} block {index} never ended"
            ));
        }
        for segment in &self.segments {
            if segment.kind == DeltaKind::ToolCall {
                validate_tool_call(&segment.text)?;
            }
        }
        Ok(self.segments)
    }
}

fn validate_tool_call(text: &str) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| format!("provider stream: tool call is not JSON: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "provider stream: tool call must be a JSON object".to_string())?;
    for field in ["id", "name", "arguments"] {
        if !object.get(field).is_some_and(|value| value.is_string()) {
            return Err(format!(
                "provider stream: tool call is missing the string field `{field}`"
            ));
        }
    }
    Ok(())
}

/// One instantiated provider, on its own store.
pub struct ProviderHost {
    id: String,
    /// Keep the engine alive: the ticker holds only a `Weak<Engine>`.
    #[allow(dead_code)]
    engine: Arc<Engine>,
    #[allow(dead_code)]
    component: Component,
    profile: ProviderProfile,
    /// `plugins/<id>/`, where `credentials.json` lives.
    plugin_dir: PathBuf,
    agent_root: Option<PathBuf>,
    transport: Arc<dyn ProviderHttpTransport>,
    /// One request's wall clock. A call that exceeds it is aborted.
    timeout: std::time::Duration,
    inner: Mutex<Option<ProviderInner>>,
    /// Set after a trap or a wall-clock deadline. A trapped store may be
    /// mid-import, so the instance is not reused.
    disabled: AtomicBool,
}

struct ProviderInner {
    store: Store<HostState>,
    guest: World,
}

impl ProviderHost {
    /// Load the component only when the grant names this install id.
    ///
    /// `Ok(None)` is the deny-by-default answer: no `provider` grant (or no
    /// profile for this id) means the guest is never instantiated.
    pub fn load(
        id: impl Into<String>,
        wasm: &Path,
        grants: &Grants,
    ) -> Result<Option<Self>, PluginError> {
        Self::load_with_options(id, wasm, grants, ProviderHostOptions::default())
    }

    /// [`Self::load`] with load-time knobs (the HTTP seam, mostly for tests).
    pub fn load_with_options(
        id: impl Into<String>,
        wasm: &Path,
        grants: &Grants,
        options: ProviderHostOptions,
    ) -> Result<Option<Self>, PluginError> {
        let id = id.into();
        let Some(profile) = grants
            .provider
            .as_ref()
            .and_then(|grant| grant.providers.iter().find(|profile| profile.id == id))
            .cloned()
        else {
            return Ok(None);
        };
        if profile.models.is_empty() {
            return Err(PluginError::Failed(format!(
                "{id}: provider profile has no models"
            )));
        }
        let ticker = crate::render_ticker().ok_or_else(|| {
            PluginError::Failed(format!("{id}: provider epoch ticker is unavailable"))
        })?;

        let mut config = crate::wasmtime_config(&crate::Options {
            fuel: options.fuel,
            max_memory_bytes: options.max_memory_bytes,
            ..crate::Options::default()
        });
        config.epoch_interruption(true);
        let engine = Arc::new(
            Engine::new(&config)
                .map_err(|error| PluginError::Failed(format!("{id}: provider engine: {error}")))?,
        );
        ticker.register(&engine);
        let component = Component::from_file(&engine, wasm)
            .map_err(|error| PluginError::Failed(format!("load {}: {error}", wasm.display())))?;

        let mut linker = Linker::new(&engine);
        World::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |state| state)
            .map_err(|error| PluginError::Failed(format!("link plugin interfaces: {error}")))?;
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|error| PluginError::Failed(format!("link wasi: {error}")))?;

        let limits = wasmtime::StoreLimitsBuilder::new()
            .memory_size(
                options
                    .max_memory_bytes
                    .unwrap_or(crate::DEFAULT_MAX_MEMORY_BYTES),
            )
            .build();
        let state = HostState {
            wasi: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            // Deny-all outside the provider profile: a provider guest needs
            // `provider-http` (carried by the call state below), never `fs`,
            // `exec`, `net-http` or `oauth`.
            grants: Grants::default(),
            fs_root: wasm.parent().map(|dir| dir.join("data")),
            project_dir: None,
            plugin_id: Some(id.clone()),
            declared_flags: Arc::new(std::collections::BTreeSet::new()),
            sockets: HashMap::new(),
            next_socket: 1,
            procs: HashMap::new(),
            next_proc: 1,
            limits,
            oauth: crate::OAuthState::default(),
            provider_http: ProviderHttpTable::default(),
            provider_call: None,
        };
        let mut store = Store::new(&engine, state);
        store.limiter(|state| &mut state.limits);
        // A registered epoch engine starts at deadline 0; each call installs
        // its real deadline, so instantiation itself must not be interrupted.
        store.set_epoch_deadline(1_000_000_000);
        let guest = World::instantiate(&mut store, &component, &linker).map_err(|error| {
            PluginError::Failed(format!("instantiate {id} for provider: {error}"))
        })?;

        Ok(Some(Self {
            id,
            engine,
            component,
            timeout: std::time::Duration::from_secs(
                profile
                    .timeout_seconds
                    .clamp(1, kiqi_plugin::grants::PROVIDER_TIMEOUT_MAX_SECONDS),
            ),
            profile,
            plugin_dir: wasm
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
            agent_root: options.agent_root,
            transport: options
                .transport
                .unwrap_or_else(|| Arc::new(crate::provider_http::ReqwestTransport)),
            inner: Mutex::new(Some(ProviderInner { store, guest })),
            disabled: AtomicBool::new(false),
        }))
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// The granted catalog for this profile.
    pub fn models(&self) -> &[ProviderModel] {
        &self.profile.models
    }

    /// The grant-narrowed catalog: the guest may only remove models, never add
    /// one. Runs on its own thread with the discovery wall clock.
    pub fn list_specs(&self) -> Result<Vec<ProviderModel>, String> {
        if self.disabled.load(Ordering::Acquire) {
            return Err(format!("{}: provider instance is disabled", self.id));
        }
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(mut inner) = guard.take() else {
            return Err(format!("{}: provider instance is gone", self.id));
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let probe = std::thread::Builder::new()
            .name("kiqi-provider-metadata".to_string())
            .spawn(move || {
                inner.store.data_mut().provider_http.reap();
                // Bound the metadata call too: `list-providers` is guest code.
                let ticks = (crate::DISCOVERY_WAIT.as_millis() / crate::RENDER_TICK_MS as u128)
                    .max(1) as u64;
                inner.store.set_epoch_deadline(ticks);
                let caps = HostCaps {
                    session: None,
                    control: None,
                    inject: None,
                    ui: None,
                    cancel: None,
                    log: &PROVIDER_LOG,
                };
                let _context = crate::install_context_kind(None, &caps, CallKind::Metadata, None);
                let outcome = inner
                    .guest
                    .kiqi_plugin_provider_api()
                    .call_list_providers(&mut inner.store);
                crate::clear_context();
                inner.store.data_mut().reap_call_scoped();
                let _ = tx.send((outcome, inner));
            });
        if let Err(error) = probe {
            self.disabled.store(true, Ordering::Release);
            return Err(format!(
                "{}: cannot start the metadata thread: {error}",
                self.id
            ));
        }
        match rx.recv_timeout(crate::DISCOVERY_WAIT) {
            Ok((outcome, inner)) => {
                *guard = Some(inner);
                let specs = outcome
                    .map_err(|trap| format!("{}: list-providers trapped: {trap}", self.id))?;
                self.narrow(specs)
            }
            Err(_) => {
                self.disabled.store(true, Ordering::Release);
                Err(format!(
                    "{}: list-providers did not finish within {}s",
                    self.id,
                    crate::DISCOVERY_WAIT.as_secs()
                ))
            }
        }
    }

    fn narrow(&self, specs: Vec<WitProviderSpec>) -> Result<Vec<ProviderModel>, String> {
        if specs.len() > crate::DISCOVERY_MAX_ENTRIES {
            return Err(format!(
                "{}: list-providers returned {} entries (cap {})",
                self.id,
                specs.len(),
                crate::DISCOVERY_MAX_ENTRIES
            ));
        }
        let Some(spec) = specs.into_iter().find(|spec| spec.provider_id == self.id) else {
            return Err(format!(
                "{}: the guest does not declare this provider",
                self.id
            ));
        };
        let declared: std::collections::BTreeSet<&str> =
            spec.models.iter().map(|model| model.id.as_str()).collect();
        Ok(self
            .profile
            .models
            .iter()
            .filter(|model| declared.contains(model.id.as_str()))
            .cloned()
            .collect())
    }

    /// One `provider-api.request` with the default (no) cancellation probe.
    pub fn request(&self, request: &str) -> Result<ProviderResponse, String> {
        self.request_with_cancel(request, None)
    }

    /// One `provider-api.request`.
    ///
    /// The credential is resolved and fingerprinted before the guest runs, so
    /// a changed or missing credential refuses the call without opening a
    /// connection. The guest call runs on a fresh thread with an epoch deadline;
    /// the caller also waits with a wall-clock timeout, so a guest stuck in a
    /// host import cannot pin it.
    pub fn request_with_cancel(
        &self,
        request: &str,
        cancel: Option<Arc<dyn CancelProbe>>,
    ) -> Result<ProviderResponse, String> {
        if self.disabled.load(Ordering::Acquire) {
            return Err(format!(
                "{}: provider instance disabled after an earlier failure",
                self.id
            ));
        }
        let credential = crate::provider_http::resolve_credential(
            &self.profile,
            &self.plugin_dir,
            self.agent_root.as_deref(),
        )
        .map_err(|error| match error {
            crate::provider_http::ProviderHttpError::Denied(message)
            | crate::provider_http::ProviderHttpError::Failed(message) => {
                format!("{}: {message}", self.id)
            }
        })?;
        // Refuse an ungranted model before the guest runs. A missing `model`
        // falls back to the first granted one.
        let selected = self.selected_model(request)?;

        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(mut inner) = guard.take() else {
            return Err(format!("{}: provider instance is gone", self.id));
        };

        let call = ProviderCallState {
            profile: self.profile.clone(),
            credential,
            transport: Arc::clone(&self.transport),
            cancel: cancel.clone(),
        };
        let ticks = self
            .timeout
            .as_millis()
            .div_ceil(crate::RENDER_TICK_MS as u128)
            .max(1) as u64;
        let request = request.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("kiqi-provider-request".to_string())
            .spawn(move || {
                inner.store.data_mut().provider_http.begin_call();
                inner.store.data_mut().provider_call = Some(call);
                inner.store.set_epoch_deadline(ticks);
                let mut assembler = DeltaAssembler::default();
                let caps = HostCaps {
                    session: None,
                    control: None,
                    inject: None,
                    ui: None,
                    cancel: cancel.as_deref(),
                    log: &PROVIDER_LOG,
                };
                let _context = crate::install_context_kind(
                    None,
                    &caps,
                    CallKind::ProviderRequest,
                    Some(&mut assembler),
                );
                let outcome = inner
                    .guest
                    .kiqi_plugin_provider_api()
                    .call_request(&mut inner.store, &request);
                crate::clear_context();
                inner.store.data_mut().provider_call = None;
                inner.store.data_mut().reap_call_scoped();
                let assembled = assembler.finish();
                let _ = tx.send((outcome, assembled, inner));
            });
        if let Err(error) = worker {
            self.disabled.store(true, Ordering::Release);
            return Err(format!(
                "{}: cannot start the provider thread: {error}",
                self.id
            ));
        }

        // The epoch deadline fires at `timeout`; this is the backstop for a
        // guest parked inside a host import, which epoch cannot interrupt.
        let wait = self.timeout + std::time::Duration::from_secs(2);
        match rx.recv_timeout(wait) {
            Ok((outcome, assembled, inner)) => match outcome {
                Ok(Ok(raw)) => {
                    *guard = Some(inner);
                    let segments = assembled?;
                    let mut response = finalize_message(raw, &self.profile, &selected)?;
                    response.segments = segments;
                    Ok(response)
                }
                Ok(Err(error)) => {
                    // A guest-level error is a normal refusal/answer: keep the
                    // instance for the next request.
                    *guard = Some(inner);
                    Err(format!("{}: provider request failed: {error:?}", self.id))
                }
                Err(trap) => {
                    self.disabled.store(true, Ordering::Release);
                    drop(inner);
                    Err(format!("{}: provider request trapped: {trap}", self.id))
                }
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                self.disabled.store(true, Ordering::Release);
                Err(format!(
                    "{}: provider request exceeded its {}s deadline",
                    self.id,
                    self.timeout.as_secs()
                ))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                self.disabled.store(true, Ordering::Release);
                Err(format!(
                    "{}: provider request thread ended without a result",
                    self.id
                ))
            }
        }
    }

    fn selected_model(&self, request: &str) -> Result<ProviderModel, String> {
        let requested = serde_json::from_str::<serde_json::Value>(request)
            .ok()
            .and_then(|value| {
                value
                    .get("model")
                    .and_then(|model| model.as_str())
                    .map(str::to_string)
            });
        match requested {
            Some(id) => self
                .profile
                .models
                .iter()
                .find(|model| model.id == id)
                .cloned()
                .ok_or_else(|| format!("{}: model `{id}` is not in the grant", self.id)),
            None => self
                .profile
                .models
                .first()
                .cloned()
                .ok_or_else(|| format!("{}: the grant has no models", self.id)),
        }
    }

    /// Number of open `provider-http` handles. Used by tests to prove the
    /// per-call table is reaped at exit.
    pub fn open_handle_count(&self) -> usize {
        let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match guard.as_ref() {
            Some(inner) => inner.store.data().provider_http.len(),
            None => 0,
        }
    }

    /// Call `shutdown` and release the instance. Idempotent.
    pub fn unload(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(mut inner) = guard.take() {
            let _ = inner
                .guest
                .kiqi_plugin_plugin_api()
                .call_shutdown(&mut inner.store);
            inner.store.data_mut().reap_call_scoped();
        }
    }
}

impl Drop for ProviderHost {
    fn drop(&mut self) {
        self.unload();
    }
}

/// Validate the guest's final message and overwrite the identity fields from
/// the grant. A guest cannot claim another provider, model or wire API.
fn finalize_message(
    raw: String,
    profile: &ProviderProfile,
    model: &ProviderModel,
) -> Result<ProviderResponse, String> {
    let mut value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|error| format!("{}: provider reply is not JSON: {error}", profile.id))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| format!("{}: provider reply must be a JSON object", profile.id))?;
    object.insert(
        "provider".to_string(),
        serde_json::Value::String(profile.id.clone()),
    );
    object.insert(
        "model".to_string(),
        serde_json::Value::String(model.id.clone()),
    );
    object.insert(
        "api".to_string(),
        serde_json::Value::String(model.api.clone()),
    );
    Ok(ProviderResponse {
        message: value.to_string(),
        provider: profile.id.clone(),
        model: model.id.clone(),
        api: model.api.clone(),
        // Filled by the caller after `assembler.finish()`.
        segments: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiqi_plugin::grants::{
        ProviderEndpoint, PROVIDER_MAX_HANDLES_DEFAULT, PROVIDER_MAX_RESPONSE_BYTES_DEFAULT,
    };

    fn delta(kind: WitDeltaKind, phase: WitDeltaPhase, index: u32, text: &str) -> WitDelta {
        WitDelta {
            kind,
            phase,
            content_index: index,
            text: text.to_string(),
        }
    }

    fn caps() -> HostCaps<'static> {
        HostCaps {
            session: None,
            control: None,
            inject: None,
            ui: None,
            cancel: None,
            log: &PROVIDER_LOG,
        }
    }

    #[test]
    fn emit_is_a_noop_without_a_call_or_outside_a_provider_call() {
        let mut assembler = DeltaAssembler::default();
        // No context at all.
        crate::emit_provider_delta(delta(WitDeltaKind::Text, WitDeltaPhase::Start, 0, "x"));
        {
            // A tool call: the delta must be dropped, not assembled.
            let caps = caps();
            let _context =
                crate::install_context_kind(None, &caps, CallKind::Tool, Some(&mut assembler));
            crate::emit_provider_delta(delta(WitDeltaKind::Text, WitDeltaPhase::Start, 0, "x"));
            crate::clear_context();
        }
        assert!(assembler.finish().unwrap().is_empty());
    }

    #[test]
    fn a_provider_call_assembles_and_validates_order() {
        let mut assembler = DeltaAssembler::default();
        {
            let caps = caps();
            let _context = crate::install_context_kind(
                None,
                &caps,
                CallKind::ProviderRequest,
                Some(&mut assembler),
            );
            crate::emit_provider_delta(delta(WitDeltaKind::Text, WitDeltaPhase::Start, 0, ""));
            crate::emit_provider_delta(delta(WitDeltaKind::Text, WitDeltaPhase::Delta, 0, "Hello"));
            crate::emit_provider_delta(delta(WitDeltaKind::Text, WitDeltaPhase::End, 0, " world"));
            crate::clear_context();
        }
        let segments = assembler.finish().unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].kind, DeltaKind::Text);
        assert_eq!(segments[0].text, "Hello world");
    }

    #[test]
    fn order_violations_fail_the_whole_stream() {
        let mut before_start = DeltaAssembler::default();
        before_start.push(delta(WitDeltaKind::Text, WitDeltaPhase::Delta, 0, "x"));
        assert!(before_start.finish().unwrap_err().contains("before start"));

        let mut duplicate_end = DeltaAssembler::default();
        duplicate_end.push(delta(WitDeltaKind::Text, WitDeltaPhase::Start, 0, ""));
        duplicate_end.push(delta(WitDeltaKind::Text, WitDeltaPhase::End, 0, ""));
        duplicate_end.push(delta(WitDeltaKind::Text, WitDeltaPhase::End, 0, ""));
        assert!(duplicate_end
            .finish()
            .unwrap_err()
            .contains("duplicate end"));

        let mut unterminated = DeltaAssembler::default();
        unterminated.push(delta(WitDeltaKind::Thinking, WitDeltaPhase::Start, 1, ""));
        assert!(unterminated.finish().unwrap_err().contains("never ended"));

        let mut reused = DeltaAssembler::default();
        reused.push(delta(WitDeltaKind::Text, WitDeltaPhase::Start, 0, ""));
        reused.push(delta(WitDeltaKind::Text, WitDeltaPhase::End, 0, ""));
        reused.push(delta(WitDeltaKind::Thinking, WitDeltaPhase::Start, 0, ""));
        assert!(reused.finish().unwrap_err().contains("already used"));
    }

    #[test]
    fn a_tool_call_end_must_carry_id_name_and_arguments() {
        let mut bad = DeltaAssembler::default();
        bad.push(delta(WitDeltaKind::ToolCall, WitDeltaPhase::Start, 2, ""));
        bad.push(delta(
            WitDeltaKind::ToolCall,
            WitDeltaPhase::End,
            2,
            r#"{"id":"c1"}"#,
        ));
        assert!(bad.finish().unwrap_err().contains("missing"));
    }

    fn profile() -> ProviderProfile {
        ProviderProfile {
            id: "acme".to_string(),
            display_name: "Acme".to_string(),
            models: vec![ProviderModel {
                id: "acme-1".to_string(),
                api: "openai-completions".to_string(),
                base_url: "https://api.acme.test/v1".to_string(),
                reasoning: false,
                input: vec!["text".to_string()],
            }],
            endpoint: ProviderEndpoint {
                scheme: "https".to_string(),
                host: "api.acme.test".to_string(),
                path_prefix: "/v1".to_string(),
                methods: vec!["POST".to_string()],
            },
            credential: None,
            auth: None,
            timeout_seconds: 30,
            max_response_bytes: PROVIDER_MAX_RESPONSE_BYTES_DEFAULT,
            max_handles: PROVIDER_MAX_HANDLES_DEFAULT,
        }
    }

    #[test]
    fn the_final_message_identity_comes_from_the_grant() {
        let profile = profile();
        let model = profile.models[0].clone();
        let response = finalize_message(
            r#"{"provider":"impostor","model":"impostor","api":"faux","content":[]}"#.to_string(),
            &profile,
            &model,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&response.message).unwrap();
        assert_eq!(value["provider"], "acme");
        assert_eq!(value["model"], "acme-1");
        assert_eq!(value["api"], "openai-completions");
    }
}
