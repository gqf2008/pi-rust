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
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

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

/// One delta as it arrives, before assembly. This is the live counterpart of
/// [`ProviderStreamSegment`]: a caller that wants to render while the guest is
/// still running subscribes to these, and the assembled segments stay the
/// authoritative end-of-call summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDeltaEvent {
    pub kind: DeltaKind,
    pub phase: DeltaPhase,
    pub content_index: u32,
    pub text: String,
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

/// Maximum deltas accepted from one provider call, including starts and ends.
/// Exceeding this budget fails the call and disables the instance.
pub const PROVIDER_STREAM_MAX_DELTAS: usize = 16_384;
/// Maximum distinct `(kind, content-index)` blocks retained for one call.
/// Exceeding this budget fails the call and disables the instance.
pub const PROVIDER_STREAM_MAX_BLOCKS: usize = 1_024;
/// Maximum aggregate UTF-8 bytes carried by all deltas in one call.
/// Exceeding this budget fails the call and disables the instance.
pub const PROVIDER_STREAM_MAX_BYTES: usize = 16 * 1024 * 1024;

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
    delta_count: usize,
    byte_count: usize,
    error: Option<String>,
    /// Budget and live-delivery failures poison the instance, unlike ordinary
    /// guest ordering errors, because its retained/queued state is untrusted.
    overflowed: bool,
    /// Live mirror of accepted deltas. Only ever called on the guest's thread,
    /// and only for a delta the assembler accepted, so a subscriber never sees
    /// a stream the host is about to reject. A bounded subscriber reports
    /// overflow here instead of letting the guest buffer without limit.
    sink: Option<Box<dyn FnMut(ProviderDeltaEvent) -> Result<(), String> + Send>>,
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
        let text = delta.text;
        if self.delta_count >= PROVIDER_STREAM_MAX_DELTAS {
            self.fail_overflow(format!(
                "delta count exceeds cap {PROVIDER_STREAM_MAX_DELTAS}"
            ));
            return;
        }
        let Some(byte_count) = self.byte_count.checked_add(text.len()) else {
            self.fail_overflow("aggregate delta byte count overflowed".to_string());
            return;
        };
        if byte_count > PROVIDER_STREAM_MAX_BYTES {
            self.fail_overflow(format!(
                "aggregate delta bytes exceed cap {PROVIDER_STREAM_MAX_BYTES}"
            ));
            return;
        }
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
                if self.segments.len() >= PROVIDER_STREAM_MAX_BLOCKS {
                    self.fail_overflow(format!(
                        "distinct blocks exceed cap {PROVIDER_STREAM_MAX_BLOCKS}"
                    ));
                    return;
                }
                self.index_kind.insert(index, kind);
                let position = self.segments.len();
                self.segments.push(ProviderStreamSegment {
                    kind,
                    content_index: index,
                    text: text.clone(),
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
                self.segments[position].text.push_str(&text);
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
                self.segments[position].text.push_str(&text);
                self.by_key.insert((kind, index), (position, false));
            }
        }
        self.delta_count += 1;
        self.byte_count = byte_count;
        if let Some(sink) = &mut self.sink {
            if let Err(error) = sink(ProviderDeltaEvent {
                kind,
                phase,
                content_index: index,
                text,
            }) {
                self.fail_overflow(error);
            }
        }
    }

    fn fail(&mut self, message: String) {
        if self.error.is_none() {
            self.error = Some(message);
        }
    }

    fn fail_overflow(&mut self, message: String) {
        self.overflowed = true;
        self.fail(message);
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

/// How many workers one provider may leave abandoned — a wall-clock deadline
/// returned while the worker was still parked inside a host import — before
/// another instance build is refused. Every abandoned worker keeps owning a
/// thread and a [`Store`] until the parked import returns, so replacing an
/// instance without bound would accumulate both.
pub const PROVIDER_MAX_ABANDONED_WORKERS: usize = 2;

/// Abandoned provider workers per `(provider id, plugin dir)`, kept until
/// their threads finish. The plugin dir is part of the key because two agent
/// roots can grant the same install id; their workers are independent.
static ABANDONED_WORKERS: OnceLock<Mutex<HashMap<String, Vec<JoinHandle<()>>>>> = OnceLock::new();

fn abandoned_workers() -> &'static Mutex<HashMap<String, Vec<JoinHandle<()>>>> {
    ABANDONED_WORKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn abandoned_key(id: &str, plugin_dir: &Path) -> String {
    format!("{id}\u{0}{}", plugin_dir.display())
}

/// Drop entries whose worker threads have finished.
fn prune_abandoned(workers: &mut HashMap<String, Vec<JoinHandle<()>>>) {
    for entry in workers.values_mut() {
        entry.retain(|worker| !worker.is_finished());
    }
    workers.retain(|_, entry| !entry.is_empty());
}

/// How many abandoned workers of one provider are still running.
pub fn abandoned_worker_count(id: &str, plugin_dir: &Path) -> usize {
    let mut workers = abandoned_workers()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    prune_abandoned(&mut workers);
    workers
        .get(&abandoned_key(id, plugin_dir))
        .map_or(0, Vec::len)
}

/// Record a worker a deadline abandoned, so instance replacement can stay
/// bounded. A worker that already finished occupies no budget: the next prune
/// drops it.
fn record_abandoned_worker(id: &str, plugin_dir: &Path, worker: JoinHandle<()>) {
    let mut workers = abandoned_workers()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    prune_abandoned(&mut workers);
    workers
        .entry(abandoned_key(id, plugin_dir))
        .or_default()
        .push(worker);
}

/// Refuse to build another instance while a provider's abandoned workers sit
/// at the cap: each one pins a thread and a store, and a guest parked in a
/// host import can keep them alive indefinitely. The caller's request fails
/// closed and succeeds again once the parked workers return.
fn refuse_when_abandoned_workers_overflow(id: &str, plugin_dir: &Path) -> Result<(), PluginError> {
    let count = abandoned_worker_count(id, plugin_dir);
    if count >= PROVIDER_MAX_ABANDONED_WORKERS {
        return Err(PluginError::Failed(format!(
            "{id}: {count} abandoned provider workers are still being reaped (cap \
             {PROVIDER_MAX_ABANDONED_WORKERS}); retry once the parked calls return"
        )));
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
    /// Fuel installed before each guest call. Instantiation always uses the
    /// discovery budget; this preserves the optional per-call test budget.
    fuel: u64,
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
        let bytes = std::fs::read(wasm)
            .map_err(|error| PluginError::Failed(format!("read {}: {error}", wasm.display())))?;
        Self::load_bytes_with_options(id, wasm, &bytes, grants, options)
    }

    /// [`Self::load_with_options`], compiling exactly `bytes`.
    ///
    /// Callers that verify the component against its digest sidecar hash the
    /// bytes first; this lets them hand over those same bytes, so a file
    /// replaced between verification and compilation is not what runs.
    pub fn load_bytes_with_options(
        id: impl Into<String>,
        wasm: &Path,
        bytes: &[u8],
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
        let plugin_dir = wasm.parent().unwrap_or_else(|| Path::new("."));
        refuse_when_abandoned_workers_overflow(&id, plugin_dir)?;
        let ticker = crate::render_ticker().ok_or_else(|| {
            PluginError::Failed(format!("{id}: provider epoch ticker is unavailable"))
        })?;

        let mut config = crate::wasmtime_config(&crate::Options {
            fuel: Some(options.fuel.unwrap_or(crate::DISCOVERY_FUEL)),
            max_memory_bytes: options.max_memory_bytes,
            ..crate::Options::default()
        });
        config.epoch_interruption(true);
        let engine = Arc::new(
            Engine::new(&config)
                .map_err(|error| PluginError::Failed(format!("{id}: provider engine: {error}")))?,
        );
        ticker.register(&engine);
        let component = Component::new(&engine, bytes)
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
        let (store, guest) =
            crate::instantiate_bounded(&id, "provider", store, component.clone(), linker)?;

        Ok(Some(Self {
            id,
            engine,
            component,
            timeout: std::time::Duration::from_secs(
                profile
                    .timeout_seconds
                    .clamp(1, kiqi_plugin::grants::PROVIDER_TIMEOUT_MAX_SECONDS),
            ),
            fuel: options.fuel.unwrap_or(u64::MAX),
            profile,
            plugin_dir: plugin_dir.to_path_buf(),
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

    /// Whether the on-disk profile still exactly matches this cached host.
    pub fn has_exact_profile(&self, profile: &ProviderProfile) -> bool {
        &self.profile == profile
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
        if let Err(error) = inner.store.set_fuel(self.fuel) {
            *guard = Some(inner);
            return Err(format!("{}: reset provider fuel: {error}", self.id));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = match std::thread::Builder::new()
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
            }) {
            Ok(worker) => worker,
            Err(error) => {
                self.disabled.store(true, Ordering::Release);
                return Err(format!(
                    "{}: cannot start the metadata thread: {error}",
                    self.id
                ));
            }
        };
        match rx.recv_timeout(crate::DISCOVERY_WAIT) {
            Ok((outcome, inner)) => {
                *guard = Some(inner);
                let specs = outcome
                    .map_err(|trap| format!("{}: list-providers trapped: {trap}", self.id))?;
                self.narrow(specs)
            }
            Err(_) => {
                self.disabled.store(true, Ordering::Release);
                // The metadata worker still owns the store; keep it on the
                // abandonment ledger so replacement stays bounded.
                record_abandoned_worker(&self.id, &self.plugin_dir, worker);
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
        self.request_with_cancel_and_sink(request, cancel, None)
    }

    /// [`Self::request_with_cancel`] plus a live delta sink.
    ///
    /// The sink runs on the provider worker thread and sees only deltas the
    /// assembler accepted, in emission order. Returning an error from the sink
    /// fails the call and disables the instance, which lets bounded callers
    /// turn queue overflow into cancellation rather than unlimited buffering.
    /// The assembled segments on the returned [`ProviderResponse`] remain the
    /// end-of-call record; a caller that renders live must still treat the
    /// final message as terminal.
    pub fn request_streaming(
        &self,
        request: &str,
        cancel: Option<Arc<dyn CancelProbe>>,
        sink: Box<dyn FnMut(ProviderDeltaEvent) -> Result<(), String> + Send>,
    ) -> Result<ProviderResponse, String> {
        self.request_with_cancel_and_sink(request, cancel, Some(sink))
    }

    fn request_with_cancel_and_sink(
        &self,
        request: &str,
        cancel: Option<Arc<dyn CancelProbe>>,
        sink: Option<Box<dyn FnMut(ProviderDeltaEvent) -> Result<(), String> + Send>>,
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
        if let Err(error) = inner.store.set_fuel(self.fuel) {
            *guard = Some(inner);
            return Err(format!("{}: reset provider fuel: {error}", self.id));
        }

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
        let worker = match std::thread::Builder::new()
            .name("kiqi-provider-request".to_string())
            .spawn(move || {
                inner.store.data_mut().provider_http.begin_call();
                inner.store.data_mut().provider_call = Some(call);
                inner.store.set_epoch_deadline(ticks);
                let mut assembler = DeltaAssembler {
                    sink,
                    ..DeltaAssembler::default()
                };
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
                let overflowed = assembler.overflowed;
                let assembled = assembler.finish();
                let _ = tx.send((outcome, assembled, inner, overflowed));
            }) {
            Ok(worker) => worker,
            Err(error) => {
                self.disabled.store(true, Ordering::Release);
                return Err(format!(
                    "{}: cannot start the provider thread: {error}",
                    self.id
                ));
            }
        };

        // The epoch deadline fires at `timeout`; this is the backstop for a
        // guest parked inside a host import, which epoch cannot interrupt.
        let wait = self.timeout + std::time::Duration::from_secs(2);
        match rx.recv_timeout(wait) {
            Ok((outcome, assembled, inner, overflowed)) => {
                if overflowed {
                    self.disabled.store(true, Ordering::Release);
                    drop(inner);
                    return Err(assembled
                        .err()
                        .unwrap_or_else(|| "provider stream: stream budget exceeded".to_string()));
                }
                match outcome {
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
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                self.disabled.store(true, Ordering::Release);
                // The worker still owns the store and may sit parked in a
                // host import for a long time; the abandonment ledger is
                // what bounds how many such instances may pile up.
                record_abandoned_worker(&self.id, &self.plugin_dir, worker);
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

    /// Whether an earlier trap or deadline marked this instance unusable. A
    /// caller that keeps the adapter across calls should build a fresh instance
    /// instead of reusing this one.
    pub fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Acquire)
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
    fn delta_count_and_aggregate_bytes_have_hard_caps() {
        let mut too_many = DeltaAssembler::default();
        too_many.push(delta(WitDeltaKind::Text, WitDeltaPhase::Start, 0, ""));
        for _ in 0..PROVIDER_STREAM_MAX_DELTAS {
            too_many.push(delta(WitDeltaKind::Text, WitDeltaPhase::Delta, 0, ""));
        }
        let error = too_many.finish().unwrap_err();
        assert!(error.contains("delta count"), "{error}");

        let mut too_large = DeltaAssembler::default();
        too_large.push(delta(
            WitDeltaKind::Text,
            WitDeltaPhase::Start,
            0,
            &"x".repeat(PROVIDER_STREAM_MAX_BYTES),
        ));
        too_large.push(delta(WitDeltaKind::Text, WitDeltaPhase::End, 0, "y"));
        assert!(too_large.overflowed);
        let error = too_large.finish().unwrap_err();
        assert!(error.contains("aggregate delta bytes"), "{error}");
    }

    #[test]
    fn live_sink_overflow_fails_the_stream() {
        let mut assembler = DeltaAssembler {
            sink: Some(Box::new(|_| Err("live delta channel full".to_string()))),
            ..DeltaAssembler::default()
        };
        assembler.push(delta(WitDeltaKind::Text, WitDeltaPhase::Start, 0, ""));
        assert!(assembler.overflowed);
        let error = assembler.finish().unwrap_err();
        assert!(error.contains("live delta channel full"), "{error}");
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

    /// A per-test key for the process-wide abandonment ledger, so parallel
    /// tests (and the other suites in this crate) never share a budget.
    fn abandoned_scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kiqi-abandoned-{tag}-{}", std::process::id()))
    }

    #[test]
    fn a_finished_worker_is_pruned_and_holds_no_budget() {
        let dir = abandoned_scratch("prune");
        let finished = std::thread::Builder::new()
            .spawn(|| {})
            .expect("spawn a finished worker");
        // `join` would consume the handle; poll instead. Once true it stays.
        while !finished.is_finished() {
            std::thread::yield_now();
        }
        record_abandoned_worker("abandoned-prune", &dir, finished);
        assert_eq!(
            abandoned_worker_count("abandoned-prune", &dir),
            0,
            "a finished worker must not occupy the budget"
        );
        refuse_when_abandoned_workers_overflow("abandoned-prune", &dir)
            .expect("reaped workers keep the provider buildable");
    }

    #[test]
    fn live_abandoned_workers_fill_the_budget_until_they_return() {
        let dir = abandoned_scratch("cap");
        let release = Arc::new(AtomicBool::new(false));
        for _ in 0..PROVIDER_MAX_ABANDONED_WORKERS {
            let parked = Arc::clone(&release);
            let worker = std::thread::Builder::new()
                .spawn(move || {
                    while !parked.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                })
                .expect("spawn a parked worker");
            record_abandoned_worker("abandoned-cap", &dir, worker);
        }
        assert_eq!(
            abandoned_worker_count("abandoned-cap", &dir),
            PROVIDER_MAX_ABANDONED_WORKERS
        );
        let error = refuse_when_abandoned_workers_overflow("abandoned-cap", &dir)
            .expect_err("a full budget must refuse another instance");
        let message = error.to_string();
        assert!(message.contains("abandoned"), "{message}");
        assert!(message.contains("retry"), "{message}");

        // Releasing the parked workers must recover the budget.
        release.store(true, Ordering::Release);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while abandoned_worker_count("abandoned-cap", &dir) > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "released workers must be reaped"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        refuse_when_abandoned_workers_overflow("abandoned-cap", &dir)
            .expect("a reaped budget must be buildable again");
    }
}
