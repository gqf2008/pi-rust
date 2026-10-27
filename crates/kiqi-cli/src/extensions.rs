//! Host-side extension registry.
//!
//! First-party extensions ship as wasm guests; the compiled-in cargo-feature
//! channel was retired on 2026-10-21 and the cdylib channel (`ki dev`) on
//! 2026-10-26, so nothing is linked into the binary here. What remains is the
//! host plumbing every tier shares: the capability bundle (`HostCaps`), the
//! process-wide event sink that fans events into `ExtensionHost`s, and the
//! registry of loaded hosts.

use std::path::{Path, PathBuf};

use kiqi_host::grants::{Grants, ResourcesGrant, RESOURCES_KIND_PROMPTS, RESOURCES_KIND_SKILLS};
use kiqi_host::{Extension, HostCaps};

/// Host capabilities backed by the real runtime-action host.
///
/// This is the sync/async seam: the host's actions are `async`, the extension
/// boundary is sync, and the native trampoline already solved that by spawning
/// on the bridge runtime's `Handle` and parking on a std channel ("works from
/// ANY thread - no ambient runtime needed"). The adapter reuses exactly that
/// mechanism instead of inventing a second one.
pub struct ActionCaps {
    host: std::sync::Arc<dyn kiqi_host::RuntimeActionHost>,
    runtime: tokio::runtime::Handle,
}

impl ActionCaps {
    pub fn new(
        host: std::sync::Arc<dyn kiqi_host::RuntimeActionHost>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self { host, runtime }
    }

    /// Dispatch a mutating action, discarding its success payload.
    fn run(
        &self,
        action: &'static str,
        args: serde_json::Value,
    ) -> Result<(), kiqi_host::ExtensionError> {
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
    ) -> Result<serde_json::Value, kiqi_host::ExtensionError> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.runtime.spawn(async move {
            let _ = tx.send(future.await);
        });
        // The native trampoline can park the calling thread because it runs on
        // a *foreign* extension thread. This adapter is called from the agent
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
            Ok(Err(error)) => Err(kiqi_host::ExtensionError::Failed(error)),
            Err(_) => Err(kiqi_host::ExtensionError::Failed(format!(
                "the host dropped `{action}` without answering"
            ))),
        }
    }
}

/// `SessionRead` reads return whatever the host action returned; the extension
/// gets the documented field rather than a raw blob.
impl kiqi_host::SessionRead for ActionCaps {
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

    fn cli_flag(&self, name: &str) -> Option<kiqi_host::FlagValue> {
        let host = self.host.clone();
        let args = serde_json::json!({ "name": name });
        self.drive("get_cli_flag", async move { host.get_cli_flag(args).await })
            .ok()
            .and_then(|value| match value.get("value")? {
                serde_json::Value::Bool(true) => Some(kiqi_host::FlagValue::Bare),
                serde_json::Value::String(text) => Some(kiqi_host::FlagValue::Text(text.clone())),
                _ => None,
            })
    }

    fn session_branch(&self) -> serde_json::Value {
        let host = self.host.clone();
        self.drive("get_session_branch", async move {
            host.get_session_branch(serde_json::json!({})).await
        })
        .unwrap_or(serde_json::Value::Null)
    }

    /// Answer from the host rather than the trait default `None`: an extension
    /// tracing a run needs to say which session it is tracing.
    fn session_id(&self) -> Option<String> {
        self.host.session_id()
    }
}

/// `SessionControl` and `InjectApi`. The mutation actions answer with a
/// success blob the extension does not need, so they collapse to `Result<(), _>`.
///
/// `UiApi` is deliberately **not** implemented here: `SetStatus` /
/// `SetEditorText` / `UiComponent` are served by the bridge's own registries,
/// not by `RuntimeActionHost`, so that adapter needs the bridge rather than
/// this host handle.
impl kiqi_host::SessionControl for ActionCaps {
    fn set_session_name(&self, name: &str) -> Result<(), kiqi_host::ExtensionError> {
        self.run("set_session_name", serde_json::json!({ "name": name }))
    }

    fn set_active_tools(&self, tools: &[String]) -> Result<(), kiqi_host::ExtensionError> {
        self.run("set_active_tools", serde_json::json!({ "tools": tools }))
    }

    fn set_model(&self, model: &str) -> Result<(), kiqi_host::ExtensionError> {
        self.run("set_model", serde_json::json!({ "model": model }))
    }

    fn set_thinking_level(&self, level: &str) -> Result<(), kiqi_host::ExtensionError> {
        self.run("set_thinking_level", serde_json::json!({ "level": level }))
    }

    fn compact(&self) -> Result<(), kiqi_host::ExtensionError> {
        self.run("compact", serde_json::json!({}))
    }

    fn new_session(&self) -> Result<(), kiqi_host::ExtensionError> {
        self.run("new_session", serde_json::json!({}))
    }

    fn fork(&self) -> Result<(), kiqi_host::ExtensionError> {
        self.run("fork", serde_json::json!({}))
    }

    fn navigate_tree(&self, target: &str) -> Result<(), kiqi_host::ExtensionError> {
        self.run("navigate_tree", serde_json::json!({ "targetId": target }))
    }

    fn switch_session(&self, id: &str) -> Result<(), kiqi_host::ExtensionError> {
        self.run("switch_session", serde_json::json!({ "id": id }))
    }

    fn reload(&self) -> Result<(), kiqi_host::ExtensionError> {
        self.run("reload", serde_json::json!({}))
    }
}

impl kiqi_host::InjectApi for ActionCaps {
    fn send_message(&self, message: &serde_json::Value) -> Result<(), kiqi_host::ExtensionError> {
        self.run("send_message", serde_json::json!({ "message": message }))
    }

    fn send_user_message(&self, text: &str) -> Result<(), kiqi_host::ExtensionError> {
        self.run("send_user_message", serde_json::json!({ "text": text }))
    }

    /// The host's `append_entry` dispatches on `message` vs `customType`
    /// (`extensions_actions.rs`), so these two send exactly those shapes. An
    /// earlier version sent `{"data": ...}`, which matches neither branch, and
    /// a test that asserted that same wrong shape still passed - which is why
    /// these expectations cite the host rather than the sender.
    fn append_message(&self, message: &serde_json::Value) -> Result<(), kiqi_host::ExtensionError> {
        self.run("append_entry", serde_json::json!({ "message": message }))
    }

    fn append_custom_entry(
        &self,
        custom_type: &str,
        data: &serde_json::Value,
    ) -> Result<(), kiqi_host::ExtensionError> {
        self.run(
            "append_entry",
            serde_json::json!({ "customType": custom_type, "data": data }),
        )
    }
}

/// An extension the host registered at build time.
pub struct Registered {
    pub id: &'static str,
    pub extension: Box<dyn Extension>,
}

/// Every first-party extension compiled into this binary.
///
/// Empty since the compiled-in channel was retired (2026-10-21): first-party
/// extensions ship as wasm guests (`extensions/wasm/kiqi-*`), and the cdylib
/// channel was retired on 2026-10-26, so no crate is linked into the binary
/// here. The historical
/// `extension-kiqi-*` cfg blocks below stay as design history; those features no
/// longer exist, so they can never be enabled.
/// 把 extension-host 的 guest 失败诊断接进 ki 自己的运行时日志
/// （`<agent_dir>/logs/ki.log`）。进程级一次：首次设置生效；之后任何
/// WasmExtension guest 的 trap/失败都会出现在 ki 的诊断日志里。
#[cfg(feature = "extension-wasm")]
pub(crate) fn install_guest_error_sink() {
    kiqi_host::wasm::set_guest_error_sink(std::sync::Arc::new(
        |extension: &str, op: &str, error: &str| {
            crate::interactive_tui::diagnostic_log(&format!("[{extension}] {op} failed: {error}"));
        },
    ));
}

/// Lean build (the `extension-wasm` feature off): there is no wasm backend, so
/// there is no guest error sink to install.
#[cfg(not(feature = "extension-wasm"))]
pub(crate) fn install_guest_error_sink() {}

pub fn load_all() -> Vec<Registered> {
    Vec::new()
}

/// Capabilities owned by the host, so they can be handed to an extension per call.
///
/// [`HostCaps`] borrows its trait objects, which is right at the call site but
/// not when the capabilities have to live inside a tool adapter. The owned
/// form exists only to produce the borrowed one.
#[derive(Clone)]
pub struct OwnedCaps {
    pub session: Option<std::sync::Arc<dyn kiqi_host::SessionRead>>,
    pub control: Option<std::sync::Arc<dyn kiqi_host::SessionControl>>,
    pub inject: Option<std::sync::Arc<dyn kiqi_host::InjectApi>>,
    pub ui: Option<std::sync::Arc<dyn kiqi_host::UiApi>>,
    pub log: std::sync::Arc<dyn kiqi_host::Log>,
}

impl OwnedCaps {
    pub fn caps(&self) -> HostCaps<'_> {
        HostCaps {
            session: self.session.as_deref(),
            control: self.control.as_deref(),
            inject: self.inject.as_deref(),
            ui: self.ui.as_deref(),
            log: self.log.as_ref(),
            cancel: None,
        }
    }
}

/// The owned bundle is the CLI's `SessionCaps` implementation: `kiqi-host`
/// stores it as a trait object on the session snapshot without learning the
/// concrete type.
impl kiqi_host::SessionCaps for OwnedCaps {
    fn host_caps(&self) -> HostCaps<'_> {
        self.caps()
    }
}

/// Capabilities for a compiled extension, backed by the real action host.
///
/// `runtime` is `None` when the caller has no tokio handle; the extension then
/// gets logging only, rather than a capability that would fail at call time.
/// `ui` stays `None` on purpose: `SetStatus`/`SetEditorText`/`UiComponent` are
/// served by the bridge's registries, not by the action host.
pub fn owned_caps(
    host: std::sync::Arc<dyn kiqi_host::RuntimeActionHost>,
    runtime: Option<tokio::runtime::Handle>,
    ui: Option<std::sync::Arc<UiCaps>>,
) -> OwnedCaps {
    let mut caps = OwnedCaps {
        session: None,
        control: None,
        inject: None,
        ui: ui.map(|ui| ui as std::sync::Arc<dyn kiqi_host::UiApi>),
        log: std::sync::Arc::new(StderrLog),
    };
    if let Some(runtime) = runtime {
        let shared = std::sync::Arc::new(ActionCaps::new(host, runtime));
        caps.session = Some(shared.clone() as std::sync::Arc<dyn kiqi_host::SessionRead>);
        caps.control = Some(shared.clone() as std::sync::Arc<dyn kiqi_host::SessionControl>);
        caps.inject = Some(shared as std::sync::Arc<dyn kiqi_host::InjectApi>);
    }
    caps
}

/// Capabilities for the compiled extensions after a `/reload`.
///
/// Same shape as [`owned_caps`], but taking the session's UI mailboxes
/// explicitly. `/reload` swaps the action bridge but **not** the mailboxes (they
/// live in the reload context so a TUI attachment and in-flight requests
/// survive the swap), and the rebuilt tool adapters must be handed the *same*
/// ones the TUI is attached to — otherwise an extension that opened a dialog before
/// the reload can never reach the user again.
pub fn reload_caps(
    host: std::sync::Arc<dyn kiqi_host::RuntimeActionHost>,
    ui_dialog: kiqi_host::UiDialogMailbox,
    ext_status: kiqi_host::ExtensionStatusMailbox,
    editor_text: kiqi_host::EditorTextMailbox,
) -> OwnedCaps {
    owned_caps(
        host,
        tokio::runtime::Handle::try_current().ok(),
        Some(std::sync::Arc::new(UiCaps::new(
            ui_dialog,
            ext_status,
            editor_text,
        ))),
    )
}

/// Install the sink that fans dispatched events into the compiled extensions.
///
/// `kiqi-extensions` owns the dispatch funnels (the `AgentEvent` emitter,
/// lifecycle events, provider hooks); this closure is how those reach
/// [`Extension::on_event`](kiqi_host::Extension::on_event). The host calls it once
/// per session build with that session's capabilities.
///
/// It also installs the closure as the **process-global fallback** and returns
/// it, so the caller can mount the same sink on the session's
/// `RegistrySnapshot` (`ExtensionSession::none_with_compiled_sink`). Session
/// dispatch then goes through that snapshot and never consults the global slot,
/// which is what keeps concurrent sessions (ACP) isolated; the global slot stays
/// the fallback for call sites without a snapshot (and for tests).
///
/// The tag discriminant maps straight through (`EventTag` is the single shared
/// enum; its numbering is pinned by `kiqi-extensions`'s ABI tests).
///
/// Test builds serialise installs through [`TEST_SINK_LOCK`]: a test that has
/// installed its own sink and is about to dispatch must not have a session
/// build elsewhere in this binary replace it first.
pub fn install_compiled_event_sink(
    caps: impl Into<std::sync::Arc<OwnedCaps>>,
) -> kiqi_host::CompiledEventSink {
    #[cfg(test)]
    let _guard = lock_test_sink();
    install_compiled_event_sink_locked(caps.into())
}

/// Remove the process-wide sink under the same test gate as
/// [`install_compiled_event_sink`].
///
/// Test builds only: the running host never tears the sink down (see the
/// `kiqi_extensions` function). Callers that already hold the gate -
/// [`with_test_sink`] - must call the `kiqi_extensions` function directly; the
/// gate is not reentrant.
#[cfg(test)]
fn clear_compiled_event_sink() {
    let _guard = lock_test_sink();
    kiqi_host::clear_compiled_event_sink();
}

/// [`install_compiled_event_sink`] without the test gate, for callers that
/// already hold it ([`with_test_sink`]). Never call this from a
/// `with_test_sink` closure: the gate is not reentrant.
fn install_compiled_event_sink_locked(
    caps: std::sync::Arc<OwnedCaps>,
) -> kiqi_host::CompiledEventSink {
    // Remember the same capabilities for the surfaces that run outside a tool
    // call (commands, probes, prompt transforms) **when they have no session**.
    // The production path mounts the owning session's caps on its
    // `RegistrySnapshot`; this slot is the compatibility fallback only.
    if let Ok(mut guard) = COMPILED_CAPS.write() {
        *guard = Some((*caps).clone());
    }
    let hosts = compiled_extension_hosts();
    let sink: kiqi_host::CompiledEventSink = std::sync::Arc::new(move |mode, event| {
        if mode == kiqi_host::DispatchMode::ProviderGate {
            return match dispatch_provider_gate_event(&hosts, &caps, event) {
                Some(patch) => kiqi_host::CompiledVerdict::Patch(patch),
                None => kiqi_host::CompiledVerdict::Continue,
            };
        }
        dispatch_event_to_compiled(&hosts, &caps, event, mode)
    });
    // Global fallback for call sites without a session snapshot; the caller
    // mounts the returned Arc on its session for the isolated path.
    kiqi_host::set_compiled_event_sink(std::sync::Arc::clone(&sink));
    sink
}

// ---------------------------------------------------------------------------
// Test-only serialisation of the process-wide sink
// ---------------------------------------------------------------------------

/// Serialises every test that writes the process-wide compiled-event sink.
///
/// `install_compiled_event_sink` writes a global and
/// `clear_compiled_event_sink` sets it back to `None`. The tests here do
/// "install -> dispatch -> assert -> clear", so a neighbour's *clear* landing
/// between our *install* and our *dispatch* leaves the fan-out with no sink:
/// the extension never sees the event, and the assertion then reports the
/// extension's empty state.
///
/// Both writes take this gate, so a clear cannot land inside another test's
/// install-to-dispatch window. Only gating the install would leave the window
/// open in the other direction: `session.rs` builds install a *real* sink and
/// dispatch through it later, without holding the gate, so an ungated clear
/// from a neighbour emptied the slot between that build and that dispatch.
/// [`clear_compiled_event_sink`] is the gated spelling; `with_test_sink` calls
/// the `kiqi_extensions` function directly because it already holds the gate.
///
/// That is not a guess. Interleaving it deterministically (install, clear,
/// dispatch) reproduces the observed failure verbatim:
///
///     probe: after a neighbour cleared the sink, todo list = "No todos"
///
/// - the same `got No todos` seen when the opt-in feature run failed. The same
/// window exists in the other direction (a neighbour's *install* replacing ours
/// between our install and dispatch), which would surface as the wrong extension's
/// data rather than nothing.
///
/// The window is not limited to the tests in this module: `session.rs` builds
/// and `/reload`s install the sink too (`session::tests::*` exercise both
/// paths), so the gate has to sit in `install_compiled_event_sink` itself
/// rather than only around the tests that install a fake sink.
///
/// Note the ordering that matters: clearing *after* a test's own dispatch does
/// nothing to it (the state is already restored), so a probe that clears late
/// will not show the race - that was the first attempt, and it "passed".
#[cfg(test)]
static TEST_SINK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take [`TEST_SINK_LOCK`], ignoring poisoning: a test panicking mid-dispatch
/// says nothing about the global, and the next test installs its own sink
/// anyway.
#[cfg(test)]
pub(crate) fn lock_test_sink() -> std::sync::MutexGuard<'static, ()> {
    TEST_SINK_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Install a test sink and hold the process-wide gate for `f`, clearing the
/// sink before the gate is released.
///
/// Invariant: nothing called from `f` may call `install_compiled_event_sink`.
/// The gate is a non-reentrant mutex, so that would deadlock. The closures here
/// only dispatch events and call extension tools, neither of which builds a
/// session.
///
/// Only the kiqi-todo-gated tests use this, so it is dead code in a test build
/// without that feature.
#[cfg(test)]
#[allow(dead_code)]
fn with_test_sink<R>(caps: impl Into<std::sync::Arc<OwnedCaps>>, f: impl FnOnce() -> R) -> R {
    let _guard = lock_test_sink();
    let _ = install_compiled_event_sink_locked(caps.into());
    let result = f();
    kiqi_host::clear_compiled_event_sink();
    result
}

/// Test-only wrapper: build an event from `(tag, payload)` and fan it out.
#[cfg(test)]
pub(crate) fn dispatch_to_compiled(
    hosts: &[std::sync::Arc<ExtensionHost>],
    caps: &OwnedCaps,
    tag: kiqi_host::EventTag,
    payload: serde_json::Value,
    mode: kiqi_host::DispatchMode,
) -> kiqi_host::CompiledVerdict {
    dispatch_event_to_compiled(hosts, caps, &kiqi_host::Event::new(tag, payload), mode)
}

/// Test-only wrapper: build an event from `(tag, payload)` and run the gate.
#[cfg(test)]
pub(crate) fn dispatch_provider_gate(
    hosts: &[std::sync::Arc<ExtensionHost>],
    caps: &OwnedCaps,
    tag: kiqi_host::EventTag,
    payload: serde_json::Value,
) -> Option<serde_json::Value> {
    dispatch_provider_gate_event(hosts, caps, &kiqi_host::Event::new(tag, payload))
}

/// Ask the extensions that should see `(tag, payload)` and fold their actions into
/// one verdict.
///
/// Split from the closure in [`install_compiled_event_sink`] so the key filter
/// and the claim fold can be exercised without the process-wide registry.
pub(crate) fn dispatch_event_to_compiled(
    hosts: &[std::sync::Arc<ExtensionHost>],
    caps: &OwnedCaps,
    event: &kiqi_host::Event,
    mode: kiqi_host::DispatchMode,
) -> kiqi_host::CompiledVerdict {
    let tag = event.tag;
    let caps = caps.caps();
    // An `Input` event is a key: only the extensions that declared it are asked,
    // matching the native `has_shortcut` filter. Every other tag goes to all.
    let key = (tag == kiqi_host::EventTag::Input)
        .then(|| event.payload.get("key").and_then(|key| key.as_str()))
        .flatten()
        .map(str::to_string);
    let asked = match &key {
        Some(key) => shortcut_hosts(hosts, key),
        None => hosts.to_vec(),
    };
    let mut verdict = kiqi_host::CompiledVerdict::Continue;
    for host in &asked {
        // Observe fan-out runs on the UI key loop and must not wait for a guest
        // that is itself waiting on the user (`ask_user` holds the wasm call
        // lock while its dialog is open). Claiming fan-out decides control flow
        // and keeps the blocking call.
        let action = match mode {
            kiqi_host::DispatchMode::Observe => {
                // AfterProviderResponse 在 0.8.0 无 wasm/compiled 面（终端消息
                // 由 MessageEnd 覆盖），宿主源也已停止派发——这里双保险跳过。
                if tag == kiqi_host::EventTag::AfterProviderResponse {
                    continue;
                }
                // The partition belongs to the provider request alone: there a
                // patch-capable host is invoked only by the gate, and a host
                // with neither switch granted is never called. Every other tag
                // (SessionStart, MessageUpdate, ToolExecution, ...) still fans
                // out to every host, so a patch-capable extension keeps its
                // lifecycle trace. An in-process host (no policy) observes
                // throughout.
                // 两个 provider 事件都是 deny-by-default：无 grant 的主机一个
                // 都不投递；BeforeProviderRequest 里可 patch 的主机只由 gate
                // 调用（避免同请求双投递），observe 主机留在本扇出。
                if matches!(
                    tag,
                    kiqi_host::EventTag::BeforeProviderRequest
                        | kiqi_host::EventTag::BeforeProviderHeaders
                ) {
                    let Some(policy) = host.provider_request_policy() else {
                        continue;
                    };
                    if tag == kiqi_host::EventTag::BeforeProviderRequest
                        && (policy.patch || !policy.observe)
                    {
                        continue;
                    }
                    if tag == kiqi_host::EventTag::BeforeProviderHeaders
                        && !policy.observe
                        && !policy.patch
                    {
                        continue;
                    }
                }
                host.try_on_event(event, &caps)
            }
            kiqi_host::DispatchMode::Claiming => host.on_event(event, &caps),
            // The gate goes through `dispatch_provider_gate`, never this fold.
            kiqi_host::DispatchMode::ProviderGate => continue,
        };
        match action {
            // A patch is consumed only by the provider gate; on this fan-out it
            // changes nothing and does not claim the event.
            kiqi_host::EventAction::Patch(_) => {}
            kiqi_host::EventAction::Continue => {}
            // `Claimed` (and the legacy `Replace`, folded to `Claimed` upstream)
            // takes the event, but the carried payload has no channel here.
            kiqi_host::EventAction::Claimed => {
                verdict = kiqi_host::CompiledVerdict::Claimed;
            }
        }
    }
    verdict
}

/// Run the provider gate: ask only patch-capable hosts, in host order, and
/// merge their patches.
///
/// The gate is a separate pass from the observe fan-out, so each host is called
/// at most once per request. A host whose patch violates its grant (or a
/// non-JSON patch) produces one diagnostic and the WHOLE request proceeds
/// unpatched. A timeout is the same: the slow host has no say, and it is not
/// retried.
pub(crate) fn dispatch_provider_gate_event(
    hosts: &[std::sync::Arc<ExtensionHost>],
    caps: &OwnedCaps,
    event: &kiqi_host::Event,
) -> Option<serde_json::Value> {
    if event.tag != kiqi_host::EventTag::BeforeProviderRequest {
        return None;
    }
    let mut merged: Option<serde_json::Value> = None;
    for host in hosts {
        let Some(policy) = host.provider_request_policy() else {
            continue;
        };
        if !policy.patch {
            continue;
        }
        match gate_with_deadline(
            host.clone(),
            caps.clone(),
            event.clone(),
            policy.patch_timeout,
        ) {
            Ok(Some(patch)) => merged = Some(merge_patches(merged, patch)),
            Ok(None) => {}
            Err(reason) => {
                tracing::warn!(
                    extension = host.id(),
                    %reason,
                    "provider patch rejected; request proceeds unpatched"
                );
                return None;
            }
        }
    }
    if let Some(value) = &merged {
        let size = serde_json::to_vec(value)
            .map(|bytes| bytes.len())
            .unwrap_or(0);
        if size > kiqi_host::PROVIDER_PATCH_MAX_ENVELOPE_BYTES {
            tracing::warn!(
                bytes = size,
                cap = kiqi_host::PROVIDER_PATCH_MAX_ENVELOPE_BYTES,
                "merged provider patch is over the envelope cap; request proceeds unpatched"
            );
            return None;
        }
    }
    merged
}

/// Run one host's gate on a plain thread and wait at most `deadline`.
///
/// A plain `std::thread` (no tokio context) matches the `spawn_extension_call`
/// rule: a wasm guest's WASI waits panic if the calling thread carries a
/// runtime. The caller is the harness's provider path, which may itself be on a
/// tokio worker, so it waits with `recv_timeout` — the request never blocks on
/// a slow or busy guest past the grant's budget.
fn gate_with_deadline(
    host: std::sync::Arc<ExtensionHost>,
    caps: OwnedCaps,
    event: kiqi_host::Event,
    deadline: std::time::Duration,
) -> Result<Option<serde_json::Value>, String> {
    let id = host.id().to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name(format!("extension-gate {id}"))
        .spawn(move || {
            let caps = caps.caps();
            let _ = tx.send(host.provider_gate(&event, &caps));
        });
    if let Err(error) = spawned {
        return Err(format!("could not spawn the provider gate: {error}"));
    }
    match rx.recv_timeout(deadline) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            tracing::warn!(
                extension = %id,
                timeout_ms = deadline.as_millis() as u64,
                "provider gate timed out; request proceeds unpatched"
            );
            Ok(None)
        }
        // The worker panicked or was dropped: no patch.
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Ok(None),
    }
}

/// Merge a later host's patch over an earlier one. Scalar keys are replaced;
/// `headers`/`metadata` maps are merged key-by-key (a later `null` deletes).
fn merge_patches(base: Option<serde_json::Value>, patch: serde_json::Value) -> serde_json::Value {
    let Some(mut base) = base else {
        return patch;
    };
    let (Some(base_object), Some(patch_object)) = (base.as_object_mut(), patch.as_object()) else {
        return patch;
    };
    for (key, value) in patch_object {
        if matches!(key.as_str(), "headers" | "metadata") {
            if let (Some(target), Some(source)) = (
                base_object.get_mut(key).and_then(|v| v.as_object_mut()),
                value.as_object(),
            ) {
                for (name, entry) in source {
                    target.insert(name.clone(), entry.clone());
                }
                continue;
            }
        }
        base_object.insert(key.clone(), value.clone());
    }
    base
}

/// The capabilities compiled extensions currently run with.
///
/// Written by [`install_compiled_event_sink`] (one session build) so commands,
/// probes and prompt transforms reach the same live host the tools do. Before a
/// session exists (a test, or `ki extension …`) an extension gets logging only, rather
/// than a capability that would fail at call time.
static COMPILED_CAPS: std::sync::RwLock<Option<OwnedCaps>> = std::sync::RwLock::new(None);

/// See [`COMPILED_CAPS`]. Cloned so the caller's borrow does not hold the lock.
pub fn compiled_caps() -> OwnedCaps {
    COMPILED_CAPS
        .read()
        .ok()
        .and_then(|guard| guard.clone())
        .unwrap_or_else(|| OwnedCaps {
            session: None,
            control: None,
            inject: None,
            ui: None,
            log: std::sync::Arc::new(StderrLog),
        })
}

/// The capabilities the surface serving `cell` must use.
///
/// Production sessions mount their own bundle on the snapshot, so two sessions
/// in one process (the ACP multi-session server) each hand their own caps to
/// probes, extension commands and prompt transforms. Only a session that
/// carries none — a sessionless test, or `ki extension …` before any session
/// exists — falls back to [`COMPILED_CAPS`], which is exactly the old behaviour
/// for that path.
///
/// A poisoned cell is recovered with `into_inner`: the lock only guards a
/// `mem::replace`, so the inner value is always a complete session, and using
/// it is the fail-closed choice (never another session's bundle).
pub fn session_caps_from_cell(
    cell: &crate::session::ExtensionSessionCell,
) -> std::sync::Arc<dyn kiqi_host::SessionCaps> {
    let guard = match cell.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    match guard.session_caps() {
        Some(caps) => std::sync::Arc::clone(caps),
        None => {
            // Never silent: a builder that forgot to mount caps would fall back
            // to the process-global (last-built) bundle — exactly the
            // cross-session leak this indirection removes. Production builders
            // always mount; this warning catches the next wiring mistake.
            eprintln!(
                "warning: extension session has no caps mounted; falling back to the \
                 process-global bundle"
            );
            std::sync::Arc::new(compiled_caps())
        }
    }
}

/// Owners for compiled extensions start here, far above any registry id.
fn next_compiled_owner() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1 << 32);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// UI capabilities, served by the interaction mailboxes.
///
/// `UiDialog` / `SetStatus` / `SetEditorText` are handled by the bridge's own
/// registries (`actions.rs`), not by `RuntimeActionHost`, which is exactly why
/// this adapter exists separately from [`ActionCaps`].
pub struct UiCaps {
    dialog: kiqi_host::UiDialogMailbox,
    status: kiqi_host::ExtensionStatusMailbox,
    editor: kiqi_host::EditorTextMailbox,
    /// Component ownership. The native bridge keys components by the registry
    /// id it assigned at load; compiled extensions have no registry entry, so they
    /// draw from a high range that cannot collide with one.
    owner: u64,
}

impl UiCaps {
    pub fn new(
        dialog: kiqi_host::UiDialogMailbox,
        status: kiqi_host::ExtensionStatusMailbox,
        editor: kiqi_host::EditorTextMailbox,
    ) -> Self {
        Self {
            dialog,
            status,
            editor,
            owner: next_compiled_owner(),
        }
    }
}

impl kiqi_host::UiApi for UiCaps {
    fn dialog(
        &self,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, kiqi_host::ExtensionError> {
        self.dialog
            .handle(request.clone())
            .map_err(kiqi_host::ExtensionError::Failed)
    }

    fn set_status(&self, update: &serde_json::Value) -> Result<(), kiqi_host::ExtensionError> {
        self.status
            .handle(update.clone())
            .map(|_| ())
            .map_err(kiqi_host::ExtensionError::Failed)
    }

    fn set_editor_text(&self, edit: &serde_json::Value) -> Result<(), kiqi_host::ExtensionError> {
        self.editor
            .handle(edit.clone())
            .map(|_| ())
            .map_err(kiqi_host::ExtensionError::Failed)
    }

    /// Served by the status mailbox's component registry, exactly as the
    /// bridge dispatches `UiComponent` (`ext_status.component(owner, args)`).
    /// An earlier version of this returned `Denied` with the message from the
    /// *non*-bridge dispatch - which is the branch that never runs when a
    /// bridge is present, as it always is here.
    fn component(
        &self,
        _name: &str,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value, kiqi_host::ExtensionError> {
        self.status
            .component(self.owner, payload.clone())
            .map_err(kiqi_host::ExtensionError::Failed)
    }
}

/// Extension logging goes to stderr for now, prefixed so it is attributable.
pub struct StderrLog;

impl kiqi_host::Log for StderrLog {
    fn log(&self, level: kiqi_host::LogLevel, message: &str) {
        eprintln!("[extension/{level:?}] {message}");
    }
}

/// An extension tool exposed as a harness tool.
///
/// A compiled-in extension tool exposed as a harness tool: the `AgentTool`
/// surface over a plain Rust method — no FFI handle, no keepalive.
/// Drive one extension call on a plain thread — with **no** tokio context — and
/// hand the result back through a oneshot.
///
/// The wasm guest's WASI waits (a dialog poll's `sleep`, for one) run through
/// wasmtime's sync-mode `Handle::block_on`, which panics if the calling thread
/// carries a tokio context. `spawn_blocking` threads do carry one; a plain
/// `std::thread` does not. The async half only awaits the result.
fn spawn_extension_call<T, F>(
    label: &str,
    work: F,
) -> Result<tokio::sync::oneshot::Receiver<T>, kiqi_agent::error::AgentError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name(format!("extension-call {label}"))
        .spawn(move || {
            let _ = tx.send(work());
        })
        .map_err(|error| {
            kiqi_agent::error::AgentError::tool(format!("spawn extension call thread: {error}"))
        })?;
    Ok(rx)
}

pub struct CompiledToolAdapter {
    host: std::sync::Arc<ExtensionHost>,
    name: String,
    label: String,
    schema: kiqi_ai::types::Tool,
    caps: OwnedCaps,
}

impl CompiledToolAdapter {
    /// `None` when the extension's parameter schema will not parse; the caller
    /// diagnoses and skips rather than losing the whole tool list.
    pub fn new(
        host: std::sync::Arc<ExtensionHost>,
        spec: &kiqi_host::ToolSpec,
        caps: OwnedCaps,
    ) -> Option<Self> {
        let parameters =
            serde_json::from_value::<kiqi_ai::types::Schema>(spec.parameters.clone()).ok()?;
        let label = format!("{}:{}", host.id(), spec.name);
        Some(Self {
            host,
            name: spec.name.clone(),
            label,
            schema: kiqi_ai::types::Tool {
                name: spec.name.clone(),
                description: spec.description.clone(),
                parameters,
                constrained_sampling: None,
            },
            caps,
        })
    }
}

/// Turns an extension's partial output into the harness's update callback.
struct UpdateSink {
    on_update: std::sync::Arc<dyn Fn(kiqi_agent::types::ToolResultPartial) + Send + Sync>,
}

impl kiqi_host::ProgressSink for UpdateSink {
    fn progress(&self, progress: kiqi_host::Progress) {
        let text = match progress {
            kiqi_host::Progress::Text(text) => text,
            kiqi_host::Progress::Json(value) => value.to_string(),
        };
        (self.on_update)(kiqi_agent::types::AgentToolResult {
            content: vec![kiqi_agent::types::TextContentOrImage::Text(
                kiqi_ai::types::TextContent {
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
impl kiqi_agent::agent_tool::AgentTool for CompiledToolAdapter {
    fn schema(&self) -> &kiqi_ai::types::Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        &self.label
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: tokio_util::sync::CancellationToken,
        on_update: std::sync::Arc<dyn Fn(kiqi_agent::types::ToolResultPartial) + Send + Sync>,
    ) -> Result<kiqi_agent::types::AgentToolResult, kiqi_agent::error::AgentError> {
        // Run the guest call on a plain thread: wasm guests reach WASI (the
        // ask-user dialog wait sleeps through `std::thread::sleep`), and
        // wasmtime's sync mode implements waits with `Handle::block_on`, which
        // panics ("Cannot start a runtime from within a runtime") on any thread
        // that carries a tokio context — `spawn_blocking` threads included. A
        // plain thread has none, so wasmtime can spin its own runtime.
        let host = std::sync::Arc::clone(&self.host);
        let name = self.name.clone();
        let label = name.clone();
        let owned_caps = self.caps.clone();
        let rx = spawn_extension_call(&label, move || {
            let sink = UpdateSink { on_update };
            let probe = HarnessCancel(signal);
            let mut caps = owned_caps.caps();
            caps.cancel = Some(&probe);
            host.call_tool(&name, &params, &caps, &sink)
        })?;
        let outcome = rx
            .await
            .map_err(|_| {
                kiqi_agent::error::AgentError::tool("extension call thread ended without a result")
            })?
            .map_err(|error| kiqi_agent::error::AgentError::tool(error.to_string()))?;

        let content = outcome
            .blocks
            .iter()
            .map(|block| match block {
                kiqi_host::Block::Text(text) => {
                    kiqi_agent::types::TextContentOrImage::Text(kiqi_ai::types::TextContent {
                        kind: Default::default(),
                        text: text.clone(),
                        text_signature: None,
                    })
                }
                kiqi_host::Block::Json(value) => {
                    kiqi_agent::types::TextContentOrImage::Text(kiqi_ai::types::TextContent {
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
            return Err(kiqi_agent::error::AgentError::tool(
                outcome
                    .blocks
                    .iter()
                    .map(|block| match block {
                        kiqi_host::Block::Text(text) => text.clone(),
                        kiqi_host::Block::Json(value) => value.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ));
        }

        Ok(kiqi_agent::types::AgentToolResult {
            content,
            details: outcome.details,
            ..Default::default()
        })
    }
}

/// The harness's cancellation token as a [`kiqi_host::CancelProbe`], so a
/// long-running extension tool (a subagent wave) can wind down when the user
/// aborts the call instead of leaving its children running.
struct HarnessCancel(tokio_util::sync::CancellationToken);

impl kiqi_host::CancelProbe for HarnessCancel {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

/// Merge compiled-in extension tools into the tool list.
///
/// Same contract as the extension side: an explicit `--tools`/`--exclude-tools`
/// decision applies, and a compiled extension tool overrides a same-named built-in
/// exactly like a native one does, so the model cannot tell the two apart.
pub fn merge_compiled_extension_tools(
    tools: &mut Vec<kiqi_harness::types::HarnessTool>,
    hosts: &[std::sync::Arc<ExtensionHost>],
    caps: &OwnedCaps,
    allowed: &dyn Fn(&str) -> bool,
) {
    for host in hosts {
        for spec in host.extension().tools() {
            if !allowed(&spec.name) {
                continue;
            }
            let Some(adapter) =
                CompiledToolAdapter::new(std::sync::Arc::clone(host), &spec, caps.clone())
            else {
                eprintln!(
                    "extension `{}`: tool `{}` has an unparseable parameter schema; skipped",
                    host.id(),
                    spec.name
                );
                continue;
            };
            let name = spec.name.clone();
            let harness_tool = kiqi_harness::types::HarnessTool::new(std::sync::Arc::new(adapter));
            match tools.iter_mut().find(|t| t.tool.schema().name == name) {
                Some(slot) => {
                    // A sandboxed third-party extension must not impersonate a tool
                    // the user already has: tools are called by name, so a
                    // collision would silently redirect e.g. `bash` into the
                    // extension, bypassing the grant the user did not give.
                    if host.is_untrusted() {
                        eprintln!(
                            "extension `{}`: refusing tool `{}` — the name already belongs to the host or a first-party extension",
                            host.id(),
                            name
                        );
                        continue;
                    }
                    *slot = harness_tool;
                }
                None => tools.push(harness_tool),
            }
        }
    }
}

/// The provider-facing schemas for every compiled-in extension tool.
///
/// This is half of the wiring: it makes extension tools visible to the model. The
/// other half - routing an actual call back into [`ExtensionHost::call_tool`] -
/// lives in the tool-execution dispatch.
pub fn compiled_tool_schemas(hosts: &[ExtensionHost]) -> Vec<kiqi_ai::types::Tool> {
    let mut tools = Vec::new();
    for host in hosts {
        for spec in host.extension().tools() {
            match serde_json::from_value::<kiqi_ai::types::Schema>(spec.parameters.clone()) {
                Ok(parameters) => tools.push(kiqi_ai::types::Tool {
                    name: spec.name,
                    description: spec.description,
                    parameters,
                    constrained_sampling: None,
                }),
                Err(error) => {
                    // Skipped rather than poisoning the whole list, but never
                    // silently: the extension and tool name are named.
                    eprintln!(
                        "extension `{}`: tool `{}` has an unparseable parameter schema ({error}); skipped",
                        host.id(),
                        spec.name
                    );
                }
            }
        }
    }
    tools
}

/// An extension plus the isolation the host owes it.
///
/// An extension is ordinary in-process code, so "isolate the failure" here means
/// one thing: a panic must not take down the agent loop. That is the same
/// guarantee the wasm path gets from a trap, and the same one the native
/// loader gets from `catch_unwind`.
pub struct ExtensionHost {
    id: String,
    extension: Box<dyn Extension>,
    /// Third-party and sandboxed (currently: wasm). A trusted host may take a
    /// name over — that is what "extension overrides same-named built-in" means
    /// for the tiers the user opted into — but an untrusted one must not, or an
    /// installed component could impersonate `bash` and intercept every command
    /// the model runs.
    untrusted: bool,
}

impl ExtensionHost {
    pub fn new(id: impl Into<String>, extension: Box<dyn Extension>) -> Self {
        Self {
            id: id.into(),
            extension,
            untrusted: false,
        }
    }

    /// A sandboxed third-party extension: its tools may not collide with a name the
    /// host or a trusted extension already provides.
    pub fn untrusted(id: impl Into<String>, extension: Box<dyn Extension>) -> Self {
        Self {
            id: id.into(),
            extension,
            untrusted: true,
        }
    }

    pub fn is_untrusted(&self) -> bool {
        self.untrusted
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn extension(&self) -> &dyn Extension {
        self.extension.as_ref()
    }

    pub fn call_tool(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        caps: &HostCaps<'_>,
        sink: &dyn kiqi_host::ProgressSink,
    ) -> Result<kiqi_host::ToolOutcome, kiqi_host::ExtensionError> {
        self.guard_result("call", || self.extension.call(name, arguments, caps, sink))
    }

    /// A panicking extension observes nothing rather than aborting the loop.
    pub fn on_event(
        &self,
        event: &kiqi_host::Event,
        caps: &HostCaps<'_>,
    ) -> kiqi_host::EventAction {
        self.guard("on-event", || self.extension.on_event(event, caps))
            .unwrap_or(kiqi_host::EventAction::Continue)
    }

    /// Observe-only fan-out: never waits for an extension that is mid-call.
    pub fn try_on_event(
        &self,
        event: &kiqi_host::Event,
        caps: &HostCaps<'_>,
    ) -> kiqi_host::EventAction {
        self.guard("on-event", || self.extension.try_on_event(event, caps))
            .unwrap_or(kiqi_host::EventAction::Continue)
    }

    /// The `provider-request` policy this extension enforces, if it has one.
    pub fn provider_request_policy(&self) -> Option<kiqi_host::grants::ProviderRequestPolicy> {
        self.extension.provider_request_policy()
    }

    /// Run the provider gate. `Err` means the produced patch was rejected whole
    /// (the caller then proceeds unpatched); `Ok(None)` means no patch.
    pub fn provider_gate(
        &self,
        event: &kiqi_host::Event,
        caps: &HostCaps<'_>,
    ) -> Result<Option<serde_json::Value>, String> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.extension.provider_gate(event, caps)
        })) {
            Ok(result) => result,
            Err(_) => {
                eprintln!("[extension] {}: provider gate panicked — ignored", self.id);
                Ok(None)
            }
        }
    }

    pub fn run_command(
        &self,
        name: &str,
        arguments: &str,
        caps: &HostCaps<'_>,
    ) -> Result<kiqi_host::CommandResult, kiqi_host::ExtensionError> {
        self.guard_result("command", || {
            self.extension.run_command(name, arguments, caps)
        })
    }

    /// Answer a selector/editor/prompt flow the extension started.
    ///
    /// The caller owns the flow-id validation (the pending slot lives in the
    /// TUI); the panic guard is the same one every in-process extension call gets.
    pub fn continue_command(
        &self,
        name: &str,
        flow_id: &str,
        reply: kiqi_host::CommandReply,
        caps: &HostCaps<'_>,
    ) -> Result<kiqi_host::CommandResult, kiqi_host::ExtensionError> {
        self.guard_result("command", || {
            self.extension.continue_command(name, flow_id, reply, caps)
        })
    }

    /// A gate point's verdict. A panicking extension has no opinion rather than
    /// blocking the action it could not judge.
    pub fn probe(
        &self,
        point: &str,
        payload: &serde_json::Value,
        caps: &HostCaps<'_>,
    ) -> kiqi_host::Verdict {
        self.guard("probe", || self.extension.probe(point, payload, caps))
            .unwrap_or(kiqi_host::Verdict::Continue)
    }

    /// `Some(replacement)` when this extension claims to rewrite the system prompt.
    pub fn before_agent_start(
        &self,
        prompt: &serde_json::Value,
        caps: &HostCaps<'_>,
    ) -> Option<String> {
        self.guard("before-agent-start", || {
            self.extension.before_agent_start(prompt, caps)
        })
        .flatten()
    }

    /// `UnwindSafe` is not implemented for trait objects; an extension owns no
    /// state the host observes through this call, so asserting it here is the
    /// intended use of the escape hatch rather than a shortcut.
    fn guard<T>(&self, _what: &str, body: impl FnOnce() -> T) -> Option<T> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).ok()
    }

    fn guard_result<T>(
        &self,
        what: &str,
        body: impl FnOnce() -> Result<T, kiqi_host::ExtensionError>,
    ) -> Result<T, kiqi_host::ExtensionError> {
        match self.guard(what, body) {
            Some(result) => result,
            None => Err(kiqi_host::ExtensionError::Failed(format!(
                "extension `{}` panicked during {what}",
                self.id
            ))),
        }
    }
}

/// Run the compiled extensions' `before-tool-call` gate with the
/// process-global fallback caps.
///
/// A sessionful caller must use [`probe_tool_call_in_session`] (the production
/// path); this variant exists for sessionless call sites and tests.
/// `Some(Block)` when an extension vetoes, `Some(Replace)` when the last extension to
/// opine rewrote the arguments, `None` when everyone declined. Local and
/// synchronous — no extension is asked unless it declared the point — so it runs
/// *before* the native event, whose dialog would otherwise open first.
pub fn probe_tool_call(
    tool_call_id: &str,
    tool_name: &str,
    args: &serde_json::Value,
) -> Option<kiqi_host::Verdict> {
    probe_tool_call_with(
        &compiled_extension_hosts(),
        &compiled_caps(),
        tool_call_id,
        tool_name,
        args,
    )
}

/// Run the gate with the **current session's** capabilities.
///
/// This is the production entry point. It resolves the bundle from the
/// session cell's snapshot at call time (so it follows a `/reload` swap) and
/// only falls back to the process-wide slot for a sessionless caller. A
/// poisoned cell is recovered — the swap itself never leaves the cell in a
/// half-written state — rather than silently reaching another session's caps.
pub fn probe_tool_call_in_session(
    cell: &crate::session::ExtensionSessionCell,
    tool_call_id: &str,
    tool_name: &str,
    args: &serde_json::Value,
) -> Option<kiqi_host::Verdict> {
    probe_tool_call_in_session_with(
        &compiled_extension_hosts(),
        cell,
        tool_call_id,
        tool_name,
        args,
    )
}

/// [`probe_tool_call_in_session`] over an explicit host list, so a test can
/// prove two sessions resolve to different caps without the process registry.
pub(crate) fn probe_tool_call_in_session_with(
    hosts: &[std::sync::Arc<ExtensionHost>],
    cell: &crate::session::ExtensionSessionCell,
    tool_call_id: &str,
    tool_name: &str,
    args: &serde_json::Value,
) -> Option<kiqi_host::Verdict> {
    let caps = session_caps_from_cell(cell);
    probe_tool_call_with(hosts, caps.as_ref(), tool_call_id, tool_name, args)
}

/// The gate itself, over an explicit host list so it can be tested without the
/// process-wide registry.
pub fn probe_tool_call_with(
    hosts: &[std::sync::Arc<ExtensionHost>],
    caps: &dyn kiqi_host::SessionCaps,
    tool_call_id: &str,
    tool_name: &str,
    args: &serde_json::Value,
) -> Option<kiqi_host::Verdict> {
    let mut current = args.clone();
    let mut replaced = false;
    for host in hosts {
        if !host
            .extension()
            .probe_points()
            .iter()
            .any(|point| point == "before-tool-call")
        {
            continue;
        }
        let payload = serde_json::json!({
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "args": current.clone(),
        });
        let borrowed = caps.host_caps();
        match host.probe("before-tool-call", &payload, &borrowed) {
            kiqi_host::Verdict::Block { reason } => {
                return Some(kiqi_host::Verdict::Block { reason })
            }
            kiqi_host::Verdict::Replace(next) => {
                current = next;
                replaced = true;
            }
            kiqi_host::Verdict::Continue => {}
        }
    }
    replaced.then(|| kiqi_host::Verdict::Replace(current))
}

/// Compiled extensions that declared `key` as a shortcut.
pub fn shortcut_hosts(
    hosts: &[std::sync::Arc<ExtensionHost>],
    key: &str,
) -> Vec<std::sync::Arc<ExtensionHost>> {
    hosts
        .iter()
        .filter(|host| {
            host.extension()
                .shortcuts()
                .iter()
                .any(|shortcut| shortcut.key == key)
        })
        .cloned()
        .collect()
}

/// Chain the compiled extensions' prompt transforms onto the native extension
/// provider hooks.
///
/// The native hook runs first — it owns the `BeforeAgentStart` event and its own
/// once-per-prompt cache — and each compiled extension then gets a chance to
/// replace the prompt it produced. Keyed on the latest user prompt, so a tool
/// loop's several provider calls compute the transform once per prompt, exactly
/// like the native slot.
///
/// `caps` is the **owning session's** capability bundle, captured at build (and
/// rebuilt on `/reload`): a prompt transform belongs to one harness, so it must
/// not read whichever session was constructed last.
pub fn compiled_prompt_hooks(
    inner: Option<std::sync::Arc<dyn kiqi_ai::ProviderHooks>>,
    caps: std::sync::Arc<dyn kiqi_host::SessionCaps>,
) -> std::sync::Arc<dyn kiqi_ai::ProviderHooks> {
    compiled_prompt_hooks_with(inner, compiled_extension_hosts(), caps)
}

/// See [`compiled_prompt_hooks`]; explicit hosts so it can be tested without the
/// process-wide registry.
pub fn compiled_prompt_hooks_with(
    inner: Option<std::sync::Arc<dyn kiqi_ai::ProviderHooks>>,
    hosts: Vec<std::sync::Arc<ExtensionHost>>,
    caps: std::sync::Arc<dyn kiqi_host::SessionCaps>,
) -> std::sync::Arc<dyn kiqi_ai::ProviderHooks> {
    std::sync::Arc::new(CompiledPromptHooks {
        inner,
        hosts,
        caps,
        cache: std::sync::Mutex::new(None),
    })
}

struct CompiledPromptHooks {
    inner: Option<std::sync::Arc<dyn kiqi_ai::ProviderHooks>>,
    hosts: Vec<std::sync::Arc<ExtensionHost>>,
    /// The hosting session's capabilities (never the process-wide fallback).
    caps: std::sync::Arc<dyn kiqi_host::SessionCaps>,
    /// `Some((timestamp, Some(prompt)))` = this prompt was transformed;
    /// `Some((timestamp, None))` = computed and everyone declined;
    /// `None` = not computed yet. The distinction keeps handlers from re-running
    /// just because they said no.
    cache: std::sync::Mutex<Option<(i64, Option<String>)>>,
}

impl CompiledPromptHooks {
    /// Compute the compiled extensions' replacement prompt, once per prompt.
    ///
    /// Keyed on the prompt timestamp so a tool loop's several provider calls
    /// reuse the first answer instead of re-running side-effecting handlers —
    /// the same guarantee the native slot makes.
    fn compiled_transform(&self, timestamp: i64, event: &serde_json::Value) -> Option<String> {
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        if cache.as_ref().map(|(ts, _)| *ts) != Some(timestamp) {
            let caps = self.caps.host_caps();
            let mut forced = None;
            for host in &self.hosts {
                if let Some(next) = host.before_agent_start(event, &caps) {
                    forced = Some(next);
                }
            }
            *cache = Some((timestamp, forced));
        }
        cache.as_ref().and_then(|(_, forced)| forced.clone())
    }
}

impl kiqi_ai::ProviderHooks for CompiledPromptHooks {
    fn transform_context(
        &self,
        model: &kiqi_ai::Model,
        ctx: &kiqi_ai::Context,
    ) -> Option<kiqi_ai::ContextPatch> {
        let native = self
            .inner
            .as_ref()
            .and_then(|hooks| hooks.transform_context(model, ctx));
        let Some((timestamp, mut event)) = kiqi_host::before_agent_start_input(ctx) else {
            return native;
        };
        // The prompt a compiled transform starts from: whatever the native slot
        // installed (or the context's own), so the two compose instead of one
        // silently discarding the other.
        let base = native
            .as_ref()
            .and_then(|patch| patch.system_prompt.clone())
            .or_else(|| ctx.system_prompt.clone());
        event["systemPrompt"] = serde_json::json!(base);

        let compiled = self.compiled_transform(timestamp, &event);

        match (native, compiled) {
            (Some(mut patch), Some(prompt)) => {
                patch.system_prompt = Some(prompt);
                Some(patch)
            }
            (None, Some(prompt)) => Some(kiqi_ai::ContextPatch {
                system_prompt: Some(prompt),
            }),
            (native, None) => native,
        }
    }

    fn before_request(
        &self,
        model: &kiqi_ai::Model,
        ctx: &kiqi_ai::Context,
        opts: &kiqi_ai::SimpleStreamOptions,
    ) -> Option<kiqi_ai::SimpleStreamOptionsPatch> {
        self.inner
            .as_ref()
            .and_then(|hooks| hooks.before_request(model, ctx, opts))
    }

    fn after_response(&self, model: &kiqi_ai::Model, message: &kiqi_ai::types::AssistantMessage) {
        if let Some(hooks) = &self.inner {
            hooks.after_response(model, message);
        }
    }
}

/// The process-wide extension instances.
///
/// Created once: an extension owns state (kiqi-todo's task list lives in its
/// instance), so reloading it per tool-list build would silently reset that
/// state.
pub fn compiled_extension_hosts() -> Vec<std::sync::Arc<ExtensionHost>> {
    static HOSTS: std::sync::OnceLock<Vec<std::sync::Arc<ExtensionHost>>> =
        std::sync::OnceLock::new();
    HOSTS
        .get_or_init(|| load_hosts().into_iter().map(std::sync::Arc::new).collect())
        .clone()
}

/// Wrap every registered extension with its isolation boundary.
pub fn load_hosts() -> Vec<ExtensionHost> {
    let mut hosts: Vec<ExtensionHost> = load_all()
        .into_iter()
        .map(|registered| ExtensionHost::new(registered.id, registered.extension))
        .collect();
    // Wasm extensions live beside the compiled ones: `extensions/<id>/extension.wasm`
    // with `extensions/<id>/grants.json` (the user's authorization, read by
    // `WasmExtension::load`).
    #[cfg(feature = "extension-wasm")]
    if let Some(extensions_dir) = kiqi_host::grants::agent_dir().map(|root| root.join("extensions"))
    {
        hosts.extend(load_wasm_hosts(&extensions_dir));
    }
    hosts
}

/// Check a component against its digest sidecar before it is loaded.
///
/// `Ok(None)` means verified, `Ok(Some(_))` means the sidecar is absent (an
/// install that predates digests still works), and `Err(diagnostic)` means the
/// sidecar disagrees or cannot be read — the caller skips the extension rather
/// than running bytes the recorded expectation does not match.
#[cfg(feature = "extension-wasm")]
pub(crate) fn digest_gate(wasm: &std::path::Path) -> Result<Option<&'static str>, String> {
    let bytes = std::fs::read(wasm).map_err(|error| format!("read {}: {error}", wasm.display()))?;
    digest_gate_bytes(wasm, &bytes)
}

/// [`digest_gate`] for bytes the caller already read, so the gate judges
/// exactly the bytes that are about to be compiled — the loader hands over
/// the same buffer, closing the read-check-compile gap.
pub(crate) fn digest_gate_bytes(
    wasm: &std::path::Path,
    bytes: &[u8],
) -> Result<Option<&'static str>, String> {
    use crate::extension_cli::DigestStatus;
    match crate::extension_cli::digest_status_bytes(wasm, bytes) {
        Ok(DigestStatus::Ok { .. }) => Ok(None),
        Ok(DigestStatus::Missing) => Ok(Some("missing")),
        Ok(DigestStatus::Mismatch { recorded, actual }) => Err(format!(
            "digest mismatch for {}: sidecar says {recorded}, file is {actual}; skipped \
             (reinstall to replace it)",
            wasm.display()
        )),
        Err(error) => Err(format!(
            "cannot verify {}: {error}; skipped",
            wasm.display()
        )),
    }
}

/// The `auth.json` half of host-attributed OAuth.
///
/// The host crate cannot depend on `kiqi-cli`, so this adapter is installed when
/// the CLI constructs a wasm extension. It is the only way a guest exchange reaches
/// disk, and it always goes through the namespaced writer — an extension token lands
/// on `extension:<install-id>:<provider-id>`, never on a bare provider id.
#[cfg(feature = "extension-wasm")]
#[derive(Debug, Default)]
pub(crate) struct ConfigCredentialSink;

#[cfg(feature = "extension-wasm")]
impl kiqi_host::wasm::ExtensionCredentialSink for ConfigCredentialSink {
    fn store_extension_oauth(
        &self,
        install_id: &str,
        provider_id: &str,
        access: &str,
        refresh: &str,
        expires: i64,
        profile_fingerprint: &str,
    ) -> Result<(), String> {
        crate::config::upsert_extension_oauth_token(
            install_id,
            provider_id,
            access.to_string(),
            refresh.to_string(),
            expires,
            profile_fingerprint.to_string(),
        )
        .map_err(|error| error.to_string())
    }

    fn read_extension_oauth(
        &self,
        install_id: &str,
        provider_id: &str,
    ) -> Result<Option<kiqi_host::wasm::StoredOAuthToken>, String> {
        match crate::config::read_extension_oauth_token(install_id, provider_id)
            .map_err(|error| error.to_string())?
        {
            Some(crate::config::Credential::Oauth {
                access,
                refresh,
                expires,
                profile_fingerprint,
            }) => Ok(Some(kiqi_host::wasm::StoredOAuthToken {
                access,
                refresh,
                expires,
                profile_fingerprint,
            })),
            _ => Ok(None),
        }
    }

    fn delete_extension_oauth(&self, install_id: &str, provider_id: &str) -> Result<bool, String> {
        crate::config::delete_extension_oauth_token(install_id, provider_id)
            .map_err(|error| error.to_string())
    }
}

/// Discover `extensions/<id>/extension.wasm` under `extensions_dir` and load each behind
/// the same [`Extension`] seam as the compiled-in ones.
///
/// An extension that fails to load (bad component, refused grants, missing WASI
/// import) is reported and skipped rather than taking the session down. A
/// component whose digest sidecar disagrees is skipped the same way: the
/// recorded digest is the only expectation there is, so unverified bytes never
/// run.
#[cfg(feature = "extension-wasm")]
pub fn load_wasm_hosts(extensions_dir: &std::path::Path) -> Vec<ExtensionHost> {
    let Ok(entries) = std::fs::read_dir(extensions_dir) else {
        return Vec::new();
    };
    let mut hosts = Vec::new();
    let mut unverified: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            continue;
        }
        let Some(id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        // `ki extension uninstall` moves the directory aside instead of deleting
        // it; the moved copy still holds `extension.wasm` and `grants.json`, so
        // loading it would keep running (and granting) an extension the user
        // removed.
        if kiqi_host::grants::is_uninstalled_dir_name(&id) {
            continue;
        }
        let wasm = entry.path().join("extension.wasm");
        if !wasm.is_file() {
            continue;
        }
        let missing_sidecar = match digest_gate(&wasm) {
            Ok(None) => false,
            Ok(Some(_)) => true,
            Err(error) => {
                eprintln!("extension `{id}`: {error}");
                continue;
            }
        };
        let options = kiqi_host::wasm::Options {
            credential_sink: Some(std::sync::Arc::new(ConfigCredentialSink)),
            ..Default::default()
        };
        match kiqi_host::wasm::WasmExtension::load_with_options(
            &id,
            &wasm,
            kiqi_host::grants::Grants::load_for(&id),
            options,
        ) {
            Ok(extension) => {
                // Only an extension that actually made it into `hosts` may be
                // called "loaded unverified": a failed load is reported by the
                // line above and must not be counted as loaded here.
                if missing_sidecar {
                    unverified.push(id.clone());
                }
                hosts.push(ExtensionHost::untrusted(id, Box::new(extension)));
            }
            Err(error) => eprintln!("extension `{id}`: {error}"),
        }
    }
    // One warning for the whole discovery pass, not one per extension: old
    // installs keep working, but never silently.
    if !unverified.is_empty() {
        eprintln!("{}", missing_sidecar_warning(&unverified));
    }
    hosts
}

/// The aggregate missing-sidecar warning, kept separate so the wording can be
/// asserted without capturing stderr.
#[cfg(feature = "extension-wasm")]
fn missing_sidecar_warning(unverified: &[String]) -> String {
    format!(
        "warning: {} wasm extension(s) have no digest sidecar and loaded unverified: {}. \
         Reinstall them with `ki insmod` to record one.",
        unverified.len(),
        unverified.join(", ")
    )
}

// ---------------------------------------------------------------------------
// Granted markdown renderers (`render-api`)
// ---------------------------------------------------------------------------

/// The per-extension cache bound: final message texts are kept so a rebuild that
/// changes nothing (a thinking toggle, a `/reload` of the cdylib tier) does not
/// call the guest again. It is a convenience cache, not state, so clearing it
/// on overflow is fine and keeps a long session from growing without bound.
#[cfg(feature = "extension-wasm")]
const RENDER_CACHE_MAX_ENTRIES: usize = 128;

/// Finalize-only markdown transformation backed by granted wasm extensions.
///
/// Each extension is a [`kiqi_host::wasm::RenderHost`]: its own deny-all store,
/// its own engine, called only when the message is finalized. This type adds
/// the scheduling and the `(extension id, text hash)` cache on top. An extension whose
/// grant is absent is never loaded, and a transform failure keeps the previous
/// text (the render path must never break assistant output).
#[cfg(feature = "extension-wasm")]
pub struct WasmRenderTransformer {
    hosts: Vec<(String, std::sync::Arc<kiqi_host::wasm::RenderHost>)>,
    cache: std::sync::Mutex<std::collections::HashMap<(String, u64), String>>,
}

#[cfg(feature = "extension-wasm")]
impl WasmRenderTransformer {
    /// Discover every installed component holding a `render` grant.
    ///
    /// Mirrors [`load_wasm_hosts`]: uninstalled directories are skipped, the
    /// digest sidecar gates the bytes, and a failed load is reported rather
    /// than taking the session down. A grant that does not include `markdown`
    /// is skipped by `RenderHost::load_with_grants` (`Ok(None)`).
    pub fn load() -> Self {
        let mut hosts = Vec::new();
        if let Some(root) = kiqi_host::grants::agent_dir() {
            let extensions_dir = root.join("extensions");
            if let Ok(entries) = std::fs::read_dir(&extensions_dir) {
                for entry in entries.flatten() {
                    if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                        continue;
                    }
                    let Some(id) = entry.file_name().to_str().map(str::to_string) else {
                        continue;
                    };
                    if kiqi_host::grants::is_uninstalled_dir_name(&id) {
                        continue;
                    }
                    let wasm = entry.path().join("extension.wasm");
                    if !wasm.is_file() {
                        continue;
                    }
                    if let Err(error) = digest_gate(&wasm) {
                        eprintln!("extension `{id}`: {error}");
                        continue;
                    }
                    let grants = Grants::load_for_at(&root, &id);
                    match kiqi_host::wasm::RenderHost::load_with_grants(&id, &wasm, &grants) {
                        Ok(Some(host)) => hosts.push((id, std::sync::Arc::new(host))),
                        Ok(None) => {}
                        Err(error) => eprintln!("extension `{id}`: {error}"),
                    }
                }
            }
        }
        // Stable order (the install directory name) so a chain is reproducible.
        hosts.sort_by(|left, right| left.0.cmp(&right.0));
        Self {
            hosts,
            cache: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }

    /// Run the granted markdown chains over `raw`, in extension-id order.
    pub fn transform(&self, raw: &str) -> String {
        if self.hosts.is_empty() {
            return raw.to_string();
        }
        let mut current = raw.to_string();
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if cache.len() >= RENDER_CACHE_MAX_ENTRIES {
            cache.clear();
        }
        for (id, host) in &self.hosts {
            let key = (id.clone(), hash_render_text(&current));
            if let Some(hit) = cache.get(&key) {
                current = hit.clone();
                continue;
            }
            match host.transform_markdown(&current) {
                Ok(transformed) => {
                    cache.insert(key, transformed.clone());
                    current = transformed;
                }
                Err(diagnostic) => {
                    // One diagnostic; the guest's answer is discarded and the
                    // previous text continues down the chain.
                    eprintln!("extension `{id}`: render: {diagnostic}");
                }
            }
        }
        current
    }
}

/// Cache key for one `(extension, text)` pair. A hash collision only costs a
/// repeated guest call, never a wrong answer, because the text is not stored
/// under an adversarial key.
#[cfg(feature = "extension-wasm")]
fn hash_render_text(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

// ---------------------------------------------------------------------------
// Package resources (`resources.json` + `resources/`)
// ---------------------------------------------------------------------------

/// A wasm package's resource convention: `<package>/resources.json` declares
/// what the package contributes, `<package>/resources/` holds it, and
/// `<package>/resources.sha256` records one digest per installed file.
///
/// This replaces the cdylib `resources_discover` channel, which handed bare
/// host paths to loaders that accept absolute paths and follow symlinks. A
/// wasm guest cannot see the host filesystem, so the host owns the bytes,
/// verifies them, and only then hands directories to the skill/prompt loaders.
///
/// **Ordering is load-bearing:** [`crate::resource_dirs`] dedupes
/// first-wins-by-name, and both discovery points (session startup and
/// `/reload`) append these directories **last**. A built-in or project
/// resource therefore always wins a name collision; a future reorder must not
/// let an extension shadow it. The collision still surfaces as a
/// `ResourceDiagnostic`.
///
/// **`/reload` note:** the resolver re-reads `grants.json` and the package
/// manifest from disk, but it does not re-instantiate wasm hosts
/// (`compiled_extension_hosts` is a `OnceLock`), so a newly installed extension's
/// tools still need a restart. Its resource directories are re-read.
pub const PACKAGE_RESOURCES_MANIFEST: &str = "resources.json";
/// The per-file digest manifest written by `ki insmod`.
pub const PACKAGE_RESOURCES_DIGESTS: &str = "resources.sha256";
/// The directory holding a package's resource tree.
pub const PACKAGE_RESOURCES_DIR: &str = "resources";

/// A package's resource tree, read and checked but not yet written.
///
/// Bytes are held in memory on purpose: the manifest caps them (at most the
/// grant maximum), and keeping them means install has nothing half-copied to
/// clean up if validation fails.
#[derive(Debug)]
pub(crate) struct PreparedPackageResources {
    policy: ResourcesGrant,
    manifest: Vec<u8>,
    /// `(path relative to resources/, bytes)`, sorted by path.
    files: Vec<(String, Vec<u8>)>,
}

impl PreparedPackageResources {
    /// The policy the manifest declares. This is the package's *declaration*,
    /// not an authorization: install never writes it as a grant (the user's
    /// explicit `ki extension grant <id> resources` does that), and `--yes` cannot
    /// auto-grant it because `resources` is in `AUTO_GRANT_EXEMPT`.
    #[allow(dead_code)] // read by the install tests; kept beside the data it describes
    pub(crate) fn policy(&self) -> &ResourcesGrant {
        &self.policy
    }
}

/// The verified resource directories one package contributes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedPackageResources {
    pub skills: Vec<PathBuf>,
    pub prompts: Vec<PathBuf>,
}

/// Every granted package's resource directories, plus the diagnostics for
/// packages that were skipped.
///
/// Directories are returned in extension-id order; callers append them **after**
/// the static/explicit dirs (first-wins-by-name).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtensionResourceDirs {
    pub skills: Vec<PathBuf>,
    pub prompts: Vec<PathBuf>,
    pub diagnostics: Vec<String>,
}

impl ExtensionResourceDirs {
    /// True when `path` is one of the extension-contributed resource paths. Used
    /// to make an extension-versus-built-in collision visible without `--verbose`.
    pub fn contains_path(&self, path: &str) -> bool {
        let path = Path::new(path);
        self.skills
            .iter()
            .chain(self.prompts.iter())
            .any(|dir| path.starts_with(dir))
    }
}

fn resources_root(package_dir: &Path) -> PathBuf {
    package_dir.join(PACKAGE_RESOURCES_DIR)
}

/// Whether a package ships a resource manifest (a symlink does not count; it
/// is rejected when the package is read).
pub(crate) fn has_package_resources(package_dir: &Path) -> bool {
    let path = package_dir.join(PACKAGE_RESOURCES_MANIFEST);
    std::fs::symlink_metadata(&path)
        .map(|meta| meta.is_file() && !meta.file_type().is_symlink())
        .unwrap_or(false)
}

/// Metadata for `path`, rejecting symlinks outright.
///
/// The cdylib channel's weakness was following a link out of the package; a
/// resource package may not contain one at any depth.
fn checked_metadata(path: &Path) -> Result<std::fs::Metadata, String> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!("{} is a symlink (refused)", path.display()));
    }
    Ok(meta)
}

/// Read and validate a package's `resources.json`.
///
/// `Ok(None)` means the package does not use the resource convention. The
/// manifest is the *package's declaration*; the grant stays the user's
/// authorization, and [`resolve_package_resources`] honours the intersection.
pub(crate) fn load_package_resources_manifest(
    package_dir: &Path,
) -> Result<Option<ResourcesGrant>, String> {
    let path = package_dir.join(PACKAGE_RESOURCES_MANIFEST);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    if meta.file_type().is_symlink() {
        return Err(format!("{} is a symlink (refused)", path.display()));
    }
    if !meta.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let policy: ResourcesGrant =
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    // Unknown kinds and out-of-range caps are install errors, never a silently
    // narrowed grant.
    policy.validate()?;
    Ok(Some(policy))
}

/// The item/byte ceiling one resource walk must stay within.
///
/// `scope` names the side that set the ceiling (`declared` at install time,
/// `granted` at resolve time) so the error says which budget was exceeded.
#[derive(Clone, Copy)]
struct ResourceBudget {
    max_items: u64,
    max_bytes: u64,
    scope: &'static str,
}

impl ResourceBudget {
    /// Reject an entry from its metadata, before it is read. `seen_items`
    /// counts this file; `seen_bytes` includes its size.
    fn check(&self, seen_items: u64, seen_bytes: u64) -> Result<(), String> {
        if seen_items > self.max_items {
            return Err(format!(
                "resources contribute at least {seen_items} file(s), over the {} max-items {}",
                self.scope, self.max_items
            ));
        }
        if seen_bytes > self.max_bytes {
            return Err(format!(
                "resources contribute at least {seen_bytes} bytes, over the {} max-bytes {}",
                self.scope, self.max_bytes
            ));
        }
        Ok(())
    }
}

/// Read every file under `root`, rejecting symlinks and non-regular entries,
/// keeping the whole tree within `budget` *before* each file is read.
///
/// Returned paths are relative to `root` with `/` separators; the caller maps
/// them to package-relative `resources/<path>`.
fn read_resource_files(
    root: &Path,
    budget: ResourceBudget,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    let meta = checked_metadata(root)?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    let mut out = Vec::new();
    let mut bytes = 0u64;
    read_resource_dir(root, "", &mut out, &mut bytes, budget)?;
    Ok(out)
}

fn read_resource_dir(
    dir: &Path,
    prefix: &str,
    out: &mut Vec<(String, Vec<u8>)>,
    bytes: &mut u64,
    budget: ResourceBudget,
) -> Result<(), String> {
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(dir)
        .map_err(|error| format!("read {}: {error}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read {}: {error}", dir.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let meta = checked_metadata(&path)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| format!("{}: non-UTF-8 file name", path.display()))?;
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if meta.is_dir() {
            read_resource_dir(&path, &relative, out, bytes, budget)?;
        } else if meta.is_file() {
            // Check the budget from the directory entry's metadata BEFORE
            // reading: a multi-GiB resource must fail as a cap violation at
            // install (and on every session start), not be loaded into memory
            // first and OOM the host.
            let seen_items = out.len() as u64 + 1;
            let seen_bytes = (*bytes).checked_add(meta.len()).unwrap_or(u64::MAX);
            budget.check(seen_items, seen_bytes)?;
            let content = std::fs::read(&path)
                .map_err(|error| format!("read {}: {error}", path.display()))?;
            // A file that grew between the metadata read and the read itself
            // still has to fit, so account for the bytes actually read.
            *bytes = (*bytes)
                .checked_add(content.len() as u64)
                .unwrap_or(u64::MAX);
            budget.check(seen_items, *bytes)?;
            out.push((relative, content));
        } else {
            return Err(format!("{} is not a regular file", path.display()));
        }
    }
    Ok(())
}

/// Read and validate a package's `resources.json` and `resources/` tree.
///
/// Nothing is written; `ki insmod` calls [`write_package_resources`] only after
/// the component itself has been copied, so a rejected manifest/kind/symlink
/// installs nothing.
pub(crate) fn prepare_package_resources(
    source_dir: &Path,
) -> Result<Option<PreparedPackageResources>, String> {
    let Some(policy) = load_package_resources_manifest(source_dir)? else {
        return Ok(None);
    };
    let root = resources_root(source_dir);
    let files = read_resource_files(
        &root,
        ResourceBudget {
            max_items: u64::from(policy.max_items),
            max_bytes: policy.max_bytes,
            scope: "declared",
        },
    )
    .map_err(|error| {
        format!(
            "{} declares resources but {}/ is unusable: {error}",
            PACKAGE_RESOURCES_MANIFEST, PACKAGE_RESOURCES_DIR
        )
    })?;
    let manifest = std::fs::read(source_dir.join(PACKAGE_RESOURCES_MANIFEST))
        .map_err(|error| format!("read {}: {error}", PACKAGE_RESOURCES_MANIFEST))?;
    Ok(Some(PreparedPackageResources {
        policy,
        manifest,
        files,
    }))
}

/// Write a prepared package beside an installed `extension.wasm`, replacing any
/// previous tree, and record one digest per file in `resources.sha256`.
pub(crate) fn write_package_resources(
    dest_package: &Path,
    prepared: &PreparedPackageResources,
) -> Result<(), String> {
    let root = resources_root(dest_package);
    remove_any(&root)?;
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("create {}: {error}", root.display()))?;
    for (relative, bytes) in &prepared.files {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        std::fs::write(&path, bytes)
            .map_err(|error| format!("write {}: {error}", path.display()))?;
    }
    let manifest_path = dest_package.join(PACKAGE_RESOURCES_MANIFEST);
    std::fs::write(&manifest_path, &prepared.manifest)
        .map_err(|error| format!("write {}: {error}", manifest_path.display()))?;

    // Include the manifest itself: a hand-edited kinds/caps declaration after
    // install must fail the same digest check as a hand-edited skill.
    let mut entries: Vec<(String, String)> = Vec::with_capacity(prepared.files.len() + 1);
    entries.push((
        PACKAGE_RESOURCES_MANIFEST.to_string(),
        crate::extension_cli::sha256_hex(&prepared.manifest),
    ));
    for (relative, bytes) in &prepared.files {
        entries.push((
            format!("{PACKAGE_RESOURCES_DIR}/{relative}"),
            crate::extension_cli::sha256_hex(bytes),
        ));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut text = String::new();
    for (path, hex) in entries {
        text.push_str(&format!("{hex}  {path}\n"));
    }
    let digest_path = dest_package.join(PACKAGE_RESOURCES_DIGESTS);
    std::fs::write(&digest_path, text)
        .map_err(|error| format!("write {}: {error}", digest_path.display()))
}

fn remove_any(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::remove_file(path)
            .map_err(|error| format!("remove {}: {error}", path.display())),
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path)
            .map_err(|error| format!("remove {}: {error}", path.display())),
        Ok(_) => std::fs::remove_file(path)
            .map_err(|error| format!("remove {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

/// Normalise a digest-manifest path, rejecting absolute paths and `..`.
fn safe_relative_path(relative: &str) -> Result<String, String> {
    let path = Path::new(relative);
    if path.is_absolute() {
        return Err(format!("`{relative}` is absolute"));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or_else(|| format!("`{relative}` is not UTF-8"))?;
                parts.push(part.to_string());
            }
            std::path::Component::CurDir => {}
            _ => {
                return Err(format!(
                    "`{relative}` escapes the package (`..` is refused)"
                ))
            }
        }
    }
    if parts.is_empty() {
        return Err(format!("`{relative}` is empty"));
    }
    Ok(parts.join("/"))
}

/// Read `resources.sha256` into `path -> hex digest`.
fn read_digest_manifest(
    package_dir: &Path,
) -> Result<std::collections::HashMap<String, String>, String> {
    let path = package_dir.join(PACKAGE_RESOURCES_DIGESTS);
    let meta = checked_metadata(&path).map_err(|error| {
        format!("{error} (reinstall with `ki insmod` to record resource digests)")
    })?;
    if !meta.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let mut digests = std::collections::HashMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let hex = parts.next().unwrap_or_default().trim();
        let relative = parts.next().unwrap_or_default().trim_start();
        if hex.len() != 64 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Err(format!("{}:{}: invalid digest", path.display(), index + 1));
        }
        if relative.is_empty() {
            return Err(format!("{}:{}: missing path", path.display(), index + 1));
        }
        let relative = safe_relative_path(relative)
            .map_err(|error| format!("{}:{}: {error}", path.display(), index + 1))?;
        if digests
            .insert(relative.clone(), hex.to_ascii_lowercase())
            .is_some()
        {
            return Err(format!(
                "{}:{}: duplicate path `{relative}`",
                path.display(),
                index + 1
            ));
        }
    }
    Ok(digests)
}

fn expect_digest(
    digests: &std::collections::HashMap<String, String>,
    relative: &str,
    bytes: &[u8],
) -> Result<(), String> {
    let Some(recorded) = digests.get(relative) else {
        return Err(format!(
            "{PACKAGE_RESOURCES_DIGESTS} has no digest for `{relative}`"
        ));
    };
    let actual = crate::extension_cli::sha256_hex(bytes);
    if recorded != &actual {
        return Err(format!(
            "digest mismatch for `{relative}`: recorded {recorded}, file is {actual}"
        ));
    }
    Ok(())
}

/// Resolve one package's granted kinds to verified, package-relative
/// directories.
///
/// Checks, in order: the grant and manifest are valid, the tree has no
/// symlinks, per-file digests match `resources.sha256`, the item/byte caps hold
/// (the tighter of grant and manifest), and every granted-and-declared kind has
/// a directory. Any failure drops the whole package — fail closed, never a
/// partially trusted tree.
pub fn resolve_package_resources(
    package_dir: &Path,
    grant: &ResourcesGrant,
) -> Result<ResolvedPackageResources, String> {
    grant.validate()?;
    let Some(manifest) = load_package_resources_manifest(package_dir)? else {
        return Err(format!(
            "{} has no {PACKAGE_RESOURCES_MANIFEST}",
            package_dir.display()
        ));
    };
    let root = resources_root(package_dir);
    let files = read_resource_files(
        &root,
        ResourceBudget {
            max_items: u64::from(grant.max_items.min(manifest.max_items)),
            max_bytes: grant.max_bytes.min(manifest.max_bytes),
            scope: "granted",
        },
    )?;

    let digests = read_digest_manifest(package_dir)?;
    let manifest_path = package_dir.join(PACKAGE_RESOURCES_MANIFEST);
    let manifest_bytes = std::fs::read(&manifest_path)
        .map_err(|error| format!("read {}: {error}", manifest_path.display()))?;
    let mut expected: std::collections::HashSet<String> = std::collections::HashSet::new();
    expected.insert(PACKAGE_RESOURCES_MANIFEST.to_string());
    expect_digest(&digests, PACKAGE_RESOURCES_MANIFEST, &manifest_bytes)?;
    for (relative, content) in &files {
        let key = format!("{PACKAGE_RESOURCES_DIR}/{relative}");
        expect_digest(&digests, &key, content)?;
        expected.insert(key);
    }
    if let Some(extra) = digests.keys().find(|key| !expected.contains(*key)) {
        return Err(format!(
            "{PACKAGE_RESOURCES_DIGESTS} lists `{extra}`, which the package does not contain"
        ));
    }

    let mut resolved = ResolvedPackageResources::default();
    for kind in &grant.kinds {
        // The grant may enable a kind the package does not ship; the package's
        // declaration decides what exists, the grant decides what is loaded.
        if !manifest.kinds.iter().any(|declared| declared == kind) {
            continue;
        }
        let dir = root.join(kind);
        let meta = checked_metadata(&dir).map_err(|error| {
            format!("manifest declares `{kind}` but resources/{kind} is unusable: {error}")
        })?;
        if !meta.is_dir() {
            return Err(format!(
                "manifest declares `{kind}` but {} is not a directory",
                dir.display()
            ));
        }
        match kind.as_str() {
            RESOURCES_KIND_SKILLS => resolved.skills.push(dir),
            RESOURCES_KIND_PROMPTS => resolved.prompts.push(dir),
            // `ResourcesGrant::validate` already rejected anything else.
            _ => {}
        }
    }
    Ok(resolved)
}

/// Resolve every installed, granted package under `root/extensions/`.
pub fn granted_package_resource_dirs(root: &Path) -> ExtensionResourceDirs {
    let mut dirs = ExtensionResourceDirs::default();
    for id in kiqi_host::grants::installed_extensions_at(root) {
        let package = kiqi_host::grants::extension_dir_at(root, &id);
        // A resources-only directory is not an extension; only a package with a
        // component participates.
        if !package.join("extension.wasm").is_file() {
            continue;
        }
        let grants = Grants::load_for_at(root, &id);
        let Some(grant) = grants.resources.as_ref() else {
            continue;
        };
        match resolve_package_resources(&package, grant) {
            Ok(resolved) => {
                dirs.skills.extend(resolved.skills);
                dirs.prompts.extend(resolved.prompts);
            }
            Err(error) => dirs.diagnostics.push(format!("extension `{id}`: {error}")),
        }
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiqi_host::{HostCaps, Log, LogLevel, NoProgress, SessionRead};

    struct Silent;
    impl Log for Silent {
        fn log(&self, _level: LogLevel, _message: &str) {}
    }

    /// A `SessionRead` whose only answer is the session id: enough to tell which
    /// session's capability bundle a call was handed.
    struct IdOnlySession(&'static str);
    impl SessionRead for IdOnlySession {
        fn active_tools(&self) -> Vec<String> {
            Vec::new()
        }
        fn system_prompt(&self) -> String {
            String::new()
        }
        fn thinking_level(&self) -> String {
            String::new()
        }
        fn cli_flag(&self, _name: &str) -> Option<kiqi_host::FlagValue> {
            None
        }
        fn session_branch(&self) -> serde_json::Value {
            serde_json::Value::Null
        }
        fn session_id(&self) -> Option<String> {
            Some(self.0.to_string())
        }
    }

    fn caps_with_id(id: &'static str) -> OwnedCaps {
        OwnedCaps {
            session: Some(std::sync::Arc::new(IdOnlySession(id))),
            control: None,
            inject: None,
            ui: None,
            log: std::sync::Arc::new(Silent),
        }
    }

    fn session_cell_with_id(id: &'static str) -> crate::session::ExtensionSessionCell {
        let caps: std::sync::Arc<dyn kiqi_host::SessionCaps> =
            std::sync::Arc::new(caps_with_id(id));
        std::sync::Arc::new(std::sync::Mutex::new(
            kiqi_host::ExtensionSession::none_with_compiled_sink_and_caps(None, Some(caps)),
        ))
    }

    /// Records the session id of every capability bundle it is called with.
    struct CapsRecorder {
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl CapsRecorder {
        fn note(&self, caps: &HostCaps<'_>) -> String {
            let id = caps
                .session
                .and_then(|session| session.session_id())
                .unwrap_or_else(|| "<no-session>".to_string());
            self.seen.lock().unwrap().push(id.clone());
            id
        }
    }

    impl kiqi_host::Extension for CapsRecorder {
        fn id(&self) -> &str {
            "caps-recorder"
        }

        fn probe_points(&self) -> Vec<String> {
            vec!["before-tool-call".to_string()]
        }

        fn probe(
            &self,
            _point: &str,
            _payload: &serde_json::Value,
            caps: &HostCaps<'_>,
        ) -> kiqi_host::Verdict {
            self.note(caps);
            kiqi_host::Verdict::Continue
        }

        fn run_command(
            &self,
            name: &str,
            _arguments: &str,
            caps: &HostCaps<'_>,
        ) -> Result<kiqi_host::CommandResult, kiqi_host::ExtensionError> {
            let id = self.note(caps);
            Ok(kiqi_host::CommandResult::message(format!(
                "{name} via {id}"
            )))
        }

        fn before_agent_start(
            &self,
            _prompt: &serde_json::Value,
            caps: &HostCaps<'_>,
        ) -> Option<String> {
            Some(format!("prompt-by-{}", self.note(caps)))
        }
    }

    fn prompt_context() -> kiqi_ai::Context {
        let user = kiqi_ai::UserMessage {
            role: kiqi_ai::types::UserRole,
            content: kiqi_ai::UserContent::Text("hello".to_string()),
            timestamp: 1,
        };
        kiqi_ai::Context::new(vec![kiqi_ai::Message::User(user)])
    }

    /// Two sessions in one process, each with its own capability bundle. Every
    /// out-of-tool-call surface must use the caps of the session it serves:
    /// before this, all three read the process-global last-writer-wins slot, so
    /// session A's guest could cancel/inject/status into session B.
    #[test]
    fn two_sessions_use_their_own_caps_for_probe_command_and_prompt() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let host = std::sync::Arc::new(ExtensionHost::new(
            "caps-recorder",
            Box::new(CapsRecorder {
                seen: std::sync::Arc::clone(&seen),
            }),
        ));
        let hosts = vec![std::sync::Arc::clone(&host)];
        let cell_a = session_cell_with_id("session-a");
        let cell_b = session_cell_with_id("session-b");

        // Probe (before-tool-call gate).
        let args = serde_json::json!({});
        assert!(
            probe_tool_call_in_session_with(&hosts, &cell_a, "call-a", "bash", &args).is_none()
        );
        assert!(
            probe_tool_call_in_session_with(&hosts, &cell_b, "call-b", "bash", &args).is_none()
        );
        assert_eq!(
            &seen.lock().unwrap()[..],
            ["session-a".to_string(), "session-b".to_string()]
        );

        // Extension command (the path the TUI worker runs).
        seen.lock().unwrap().clear();
        for (cell, expected) in [(&cell_a, "session-a"), (&cell_b, "session-b")] {
            let caps = session_caps_from_cell(cell);
            host.run_command("record", "", &caps.host_caps())
                .expect("the recorder command runs");
            assert_eq!(
                seen.lock().unwrap().last().map(String::as_str),
                Some(expected),
                "the command used its own session's caps"
            );
        }

        // Prompt transform (one hooks object per harness, as build/reload makes).
        seen.lock().unwrap().clear();
        let model = kiqi_ai::Model::new("m", "m", kiqi_ai::Api::Faux, "test", "");
        let ctx = prompt_context();
        for (cell, expected) in [(&cell_a, "session-a"), (&cell_b, "session-b")] {
            let caps = session_caps_from_cell(cell);
            let hooks = compiled_prompt_hooks_with(None, hosts.clone(), caps);
            let patch = hooks
                .transform_context(&model, &ctx)
                .expect("the recorder transforms the prompt");
            assert_eq!(
                patch.system_prompt,
                Some(format!("prompt-by-{expected}")),
                "the transform used its own session's caps"
            );
        }
        assert_eq!(
            &seen.lock().unwrap()[..],
            ["session-a".to_string(), "session-b".to_string()]
        );
    }

    /// Build a guest fixture through the same path `ki extension insmod --build`
    /// uses: the artifact is located from cargo's JSON output, so a
    /// machine-level `build.target-dir` cannot make the test look in the wrong
    /// place (a hard-coded `extensions/wasm/target/...` did exactly that).
    #[cfg(feature = "extension-wasm")]
    fn build_guest(crate_dir: &str) -> std::path::PathBuf {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        crate::extension_cli::build_guest(&root.join("extensions/wasm").join(crate_dir))
            .expect("build the guest fixture")
    }

    /// The whole path: registry -> harness tool list -> adapter -> extension call
    /// -> result. This is the one that proves a compiled-in extension is usable,
    /// not merely loadable.
    /// Uses the compiled kiqi-todo to prove extension tools reach the harness
    /// list; runs where that extension is built.
    #[cfg(feature = "extension-kiqi-todo")]
    #[tokio::test]
    async fn a_compiled_extension_tool_runs_from_the_harness_tool_list() {
        use kiqi_agent::agent_tool::AgentTool;

        let hosts = compiled_extension_hosts();
        let caps = OwnedCaps {
            session: None,
            control: None,
            inject: None,
            ui: None,
            log: std::sync::Arc::new(StderrLog),
        };
        let mut tools: Vec<kiqi_harness::types::HarnessTool> = Vec::new();
        merge_compiled_extension_tools(&mut tools, &hosts, &caps, &|_| true);

        let tool = tools
            .iter()
            .find(|t| t.tool.schema().name == "todo")
            .expect("kiqi-todo's tool reached the harness list");

        let result = tool
            .tool
            .execute(
                "call-1",
                serde_json::json!({"action": "add", "text": "from the harness"}),
                tokio_util::sync::CancellationToken::new(),
                std::sync::Arc::new(|_| {}),
            )
            .await
            .expect("the extension tool executed");

        let text = result
            .content
            .iter()
            .map(|block| match block {
                kiqi_agent::types::TextContentOrImage::Text(t) => t.text.clone(),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("from the harness"), "got {text}");
        // Order-independent on purpose: the extension instance is process-wide
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
    fn every_compiled_extension_exposes_its_tools_and_schemas() {
        let hosts = load_hosts();
        let ids: Vec<&str> = hosts.iter().map(|h| h.id()).collect();
        // No compiled extension is in the default build any more: permission
        // policy moved into the host, and the extension crate is opt-in for one
        // release. Each push names the feature that brings an extension back.
        #[allow(unused_mut)]
        let mut expected_ids: Vec<&str> = Vec::new();
        #[cfg(feature = "extension-kiqi-permissions")]
        expected_ids.push("kiqi-permissions");
        #[cfg(feature = "extension-kiqi-todo")]
        expected_ids.push("kiqi-todo");
        #[cfg(feature = "extension-kiqi-lens")]
        expected_ids.push("kiqi-lens");
        #[cfg(feature = "extension-kiqi-webfetch")]
        expected_ids.push("kiqi-webfetch");
        #[cfg(feature = "extension-kiqi-websearch")]
        expected_ids.push("kiqi-websearch");
        #[cfg(feature = "extension-kiqi-subagents")]
        expected_ids.push("kiqi-subagents");
        #[cfg(feature = "extension-kiqi-goal")]
        expected_ids.push("kiqi-goal");
        #[cfg(feature = "extension-kiqi-plan-mode")]
        expected_ids.push("kiqi-plan-mode");
        #[cfg(feature = "extension-kiqi-ask-user")]
        expected_ids.push("kiqi-ask-user");
        #[cfg(feature = "extension-kiqi-langfuse")]
        expected_ids.push("kiqi-langfuse");
        #[cfg(feature = "extension-kiqi-codegraph")]
        expected_ids.push("kiqi-codegraph");
        #[cfg(feature = "extension-kiqi-orchestrator")]
        expected_ids.push("kiqi-orchestrator");
        for expected in expected_ids {
            assert!(
                ids.contains(&expected),
                "{expected} is compiled in; got {ids:?}"
            );
        }
        // Every declared tool must survive the Value -> Schema conversion;
        // an extension that cannot be exposed is a bug, not a warning.
        for host in &hosts {
            for spec in host.extension().tools() {
                let schema =
                    serde_json::from_value::<kiqi_ai::types::Schema>(spec.parameters.clone());
                assert!(
                    schema.is_ok(),
                    "{}::{} has a bad schema",
                    host.id(),
                    spec.name
                );
            }
        }
        let tools: Vec<String> = compiled_tool_schemas(&hosts)
            .into_iter()
            .map(|t| t.name)
            .collect();
        #[allow(unused_mut)]
        let mut expected_tools: Vec<&str> = Vec::new();
        #[cfg(feature = "extension-kiqi-permissions")]
        expected_tools.push("permissions");
        #[cfg(feature = "extension-kiqi-orchestrator")]
        {
            expected_tools.push("subagent");
            expected_tools.push("plan");
            expected_tools.push("dispatch");
        }
        #[cfg(feature = "extension-kiqi-todo")]
        expected_tools.push("todo");
        #[cfg(feature = "extension-kiqi-lens")]
        expected_tools.push("code_lens");
        #[cfg(feature = "extension-kiqi-webfetch")]
        expected_tools.push("webfetch");
        #[cfg(feature = "extension-kiqi-websearch")]
        expected_tools.push("websearch");
        #[cfg(feature = "extension-kiqi-subagents")]
        expected_tools.push("delegate_task");
        #[cfg(feature = "extension-kiqi-goal")]
        expected_tools.push("goal");
        #[cfg(feature = "extension-kiqi-plan-mode")]
        {
            expected_tools.push("plan_mode_complete");
            expected_tools.push("plan_mode_start");
        }
        #[cfg(feature = "extension-kiqi-ask-user")]
        expected_tools.push("ask_user");
        #[cfg(feature = "extension-kiqi-codegraph")]
        expected_tools.push("codegraph_search");
        for expected in expected_tools {
            assert!(
                tools.iter().any(|t| t == expected),
                "{expected} missing from {tools:?}"
            );
        }
    }

    /// Uses the compiled kiqi-todo as the sample extension, so it runs where that
    /// feature is built (the default build has no compiled todo).
    #[cfg(feature = "extension-kiqi-todo")]
    #[test]
    fn compiled_extension_tools_reach_the_provider_facing_tool_list() {
        let hosts = load_hosts();
        let tools = compiled_tool_schemas(&hosts);
        let todo = tools
            .iter()
            .find(|tool| tool.name == "todo")
            .expect("kiqi-todo's tool is in the model-visible list");
        assert!(!todo.description.is_empty());
        // The schema survived the Value -> Schema conversion intact.
        let rendered = serde_json::to_value(&todo.parameters).unwrap();
        let properties = rendered["properties"]
            .as_object()
            .expect("parameters is an object schema");
        assert!(properties.contains_key("action"));
    }

    /// Every mirrored tag must carry the native ABI's discriminant. Without
    /// this the mirror could drift silently - the extension and host are one
    /// binary, so nothing else would notice until a handler matched the wrong
    /// event.
    #[test]
    fn ui_capabilities_reach_the_mailboxes() {
        use kiqi_host::UiApi;

        let ui = UiCaps::new(
            kiqi_host::UiDialogMailbox::new(),
            kiqi_host::ExtensionStatusMailbox::new(),
            kiqi_host::EditorTextMailbox::new(),
        );

        // The host-shaped payloads are accepted as-is.
        ui.set_status(&serde_json::json!({"key": "kiqi.test", "value": "ok"}))
            .expect("status reaches the mailbox");
        ui.set_editor_text(&serde_json::json!({
            "text": "draft", "mode": "append", "autoSendMs": 2000
        }))
        .expect("editor text reaches the mailbox");

        // UiComponent is served by the status mailbox's component registry
        // (register/update/show/hide), exactly as the bridge dispatches it, so
        // it must be accepted rather than refused.
        ui.component(
            "footer",
            &serde_json::json!({
                "op": "register", "id": "kiqi.test", "type": "text",
                "slot": "footer.after"
            }),
        )
        .expect("component registration reaches the status mailbox");
    }

    #[test]
    fn registry_exposes_the_first_party_extensions() {
        let extensions = load_all();
        // No extension is in the default build any more: permission policy moved
        // into the host (`crate::permissions`). Assert the registry is still
        // coherent (no duplicate ids) so `extensions` is used in every config.
        let mut ids: Vec<&str> = extensions.iter().map(|entry| entry.id).collect();
        let registered = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), registered, "duplicate extension id in registry");

        #[cfg(feature = "extension-kiqi-permissions")]
        {
            let permissions = extensions
                .iter()
                .find(|entry| entry.id == "kiqi-permissions")
                .expect("the feature is on, so kiqi-permissions is registered");
            let tools = permissions.extension.tools();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].name, "permissions");
        }

        #[cfg(feature = "extension-kiqi-todo")]
        let todo = extensions
            .iter()
            .find(|entry| entry.id == "kiqi-todo")
            .expect("the feature is on, so kiqi-todo is registered");

        #[cfg(feature = "extension-kiqi-todo")]
        {
            let tools = todo.extension.tools();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].name, "todo");
            assert!(tools[0].parameters["properties"]["action"].is_object());

            assert_eq!(todo.extension.commands()[0].name, "todos");
            assert!(todo.extension.probe_points().is_empty());
        }

        // kiqi-langfuse is the last package ported off the native ABI. It is not
        // in the default build any more, so its three tools are asserted only
        // when the feature is on (CI checks that configuration).
        #[cfg(feature = "extension-kiqi-langfuse")]
        {
            let langfuse = extensions
                .iter()
                .find(|entry| entry.id == "kiqi-langfuse")
                .expect("the feature is on, so kiqi-langfuse is registered");
            let langfuse_tools = langfuse.extension.tools();
            let names: Vec<&str> = langfuse_tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect();
            assert_eq!(
                names,
                vec!["langfuse_score", "langfuse_prompt", "langfuse_trace"]
            );
            assert!(langfuse.extension.commands().is_empty());
            assert!(langfuse.extension.probe_points().is_empty());
        }
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
    impl kiqi_host::RuntimeActionHost for FakeHost {
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
        fn session_id(&self) -> Option<String> {
            Some("sess-test".into())
        }
        async fn get_session_branch(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            // The shape kiqi-todo walks: entries[].message.{role,toolName,details}
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
    fn capabilities_reach_the_real_host_and_a_extension_sees_real_data() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let fake = std::sync::Arc::new(FakeHost::default());
        let caps_host = ActionCaps::new(fake.clone(), runtime.handle().clone());
        let silent = Silent;
        // `inject` is granted deliberately: kiqi-goal cannot advance without it,
        // which is the capability gate doing its job.
        //
        // The goal start/state assertions live in
        // `goal_tool_writes_state_into_the_project_cwd` below: they have to run
        // in a child process whose cwd is a tempdir, otherwise the extension writes
        // `.ki/goals.json` into this checkout (`goals_path()` uses the process
        // cwd).
        let caps = HostCaps {
            session: Some(&caps_host),
            control: Some(&caps_host),
            inject: Some(&caps_host),
            ui: None,
            log: &silent,
            cancel: None,
        };

        // The adapter really crosses the spawn/park seam.
        assert_eq!(
            caps_host.active_tools(),
            vec!["todo".to_string(), "bash".to_string()]
        );
        assert_eq!(caps_host.thinking_level(), "medium");
        assert_eq!(caps_host.system_prompt(), "you are kiqi");
        assert_eq!(
            caps_host.cli_flag("verbose"),
            Some(kiqi_host::FlagValue::Text("2".to_string()))
        );
        assert_eq!(caps_host.cli_flag("missing"), None);

        // The mutating capabilities must hand the host the documented args,
        // not a re-shaped blob. `kiqi_host::SessionControl` / `InjectApi` are
        // in scope for these calls.
        use kiqi_host::{InjectApi, SessionControl};
        caps_host.set_model("deepseek-flash").unwrap();
        caps_host.set_active_tools(&["todo".to_string()]).unwrap();
        caps_host.navigate_tree("abc").unwrap();
        caps_host.switch_session("sess-1").unwrap();
        caps_host.reload().unwrap();
        caps_host.send_user_message("hi").unwrap();
        caps_host
            .append_message(&serde_json::json!({"role": "assistant", "content": []}))
            .unwrap();
        caps_host
            .append_custom_entry("kiqi.note", &serde_json::json!({"kind": "note"}))
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
        // Exactly the two shapes the host dispatches on
        // (`extensions_actions.rs`: `message` vs `customType` + `data`).
        // Nothing else is accepted, so a wrong shape fails only at runtime -
        // which is how the first version of this adapter shipped `{"data":..}`.
        let appends: Vec<_> = seen
            .iter()
            .filter(|(a, _)| a == "append_entry")
            .map(|(_, args)| args.clone())
            .collect();
        assert_eq!(appends.len(), 2, "both append shapes were dispatched");
        assert!(appends.contains(&serde_json::json!({
            "message": {"role": "assistant", "content": []}
        })));
        assert!(appends.contains(&serde_json::json!({
            "customType": "kiqi.note",
            "data": {"kind": "note"}
        })));

        // kiqi-goal makes its host call on the TOOL path (starting a goal injects
        // the notice) and it persists that goal through `current_dir()`, so it
        // cannot run here: the process cwd is this crate's source tree. See
        // `goal_tool_writes_state_into_the_project_cwd`, which exercises the very
        // same path with the cwd pinned to a tempdir.

        #[cfg(feature = "extension-kiqi-ask-user")]
        {
            // kiqi-ask-user needs UiApi: without a granted UI it must fail rather
            // than fabricate an answer for the model.
            let ask_host = ExtensionHost::new("kiqi-ask-user", kiqi_ask_user::kiqi::factory());
            let refused = ask_host
                .call_tool(
                    "ask_user",
                    &serde_json::json!({"question": "proceed?"}),
                    &caps,
                    &NoProgress,
                )
                .unwrap_err();
            assert!(
                refused.to_string().contains("UI"),
                "without UiApi ask_user refuses instead of answering for the user: {refused}"
            );
        }

        #[cfg(feature = "extension-kiqi-plan-mode")]
        {
            // kiqi-plan-mode needs SessionControl: entering plan mode swaps the tool
            // set. Asserted on the host side - what it was asked to do.
            let plan_host = ExtensionHost::new("kiqi-plan-mode", kiqi_plan_mode::kiqi::factory());
            // The package keeps its state process-wide (as kiqi-todo does), so a
            // previous test may have left plan mode active; entering it again
            // would short-circuit and swap nothing.
            let _ = plan_host.run_command("plan", "exit", &caps);
            plan_host.run_command("plan", "start", &caps).unwrap();
            let swapped = fake.seen.lock().unwrap().iter().any(|(action, args)| {
                action == "set_active_tools"
                    && args
                        .get("tools")
                        .and_then(|t| t.as_array())
                        .map(|tools| !tools.is_empty())
                        .unwrap_or(false)
            });
            assert!(
                swapped,
                "plan mode swapped the active tool set through SessionControl; host saw {:?}",
                fake.seen
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(a, _)| a.clone())
                    .collect::<Vec<_>>()
            );

            // Without that capability it must refuse rather than half-enter. Exit
            // first with the capabilities granted, otherwise it short-circuits on
            // "already active" and never reaches the capability check at all.
            plan_host.run_command("plan", "exit", &caps).unwrap();
            let stripped = HostCaps {
                session: None,
                control: None,
                inject: None,
                ui: None,
                log: &silent,
                cancel: None,
            };
            let refused = plan_host.run_command("plan", "start", &stripped).unwrap();
            assert!(
                matches!(
                    &refused.body,
                    kiqi_host::CommandResultBody::Message(text) if text.contains("Unable")
                ),
                "without SessionControl plan mode cannot start: {refused:?}"
            );
        }

        #[cfg(feature = "extension-kiqi-todo")]
        with_test_sink(
            // The sink's capabilities do not matter here: the direct `on_event`
            // below is what writes kiqi-todo's process-global list. Holding the
            // gate is the point, so no concurrent session build can swap in a
            // sink that restores that list from another (empty) branch while
            // this test is reading it.
            empty_caps(),
            || {
                // And an extension restoring through caps gets the branch the host
                // served, not a stub: this is the kiqi-todo restore path end to
                // end.
                let host = ExtensionHost::new("kiqi-todo", kiqi_todo::kiqi::factory());
                host.on_event(
                    &kiqi_host::Event::new(
                        kiqi_host::EventTag::SessionStart,
                        serde_json::Value::Null,
                    ),
                    &caps,
                );
                let listed = host
                    .call_tool(
                        "todo",
                        &serde_json::json!({"action": "list"}),
                        &caps,
                        &NoProgress,
                    )
                    .unwrap();
                let text = match &listed.blocks[0] {
                    kiqi_host::Block::Text(text) => text.clone(),
                    other => panic!("unexpected block {other:?}"),
                };
                assert!(text.contains("from the branch"), "got {text}");
            },
        );
    }

    /// kiqi-goal's host access sits on the TOOL path (starting a goal injects the
    /// notice), so calling it proves an extension really reaches the host through
    /// InjectApi rather than only being loadable. The assertion is on the host
    /// side - what it was handed.
    ///
    /// It also persists the goal through `current_dir()` (`.ki/goals.json`), and
    /// under `cargo test` the cwd is `crates/kiqi-cli` - the source tree, whose
    /// `.ki/goals.json` used to be tracked and got dirtied by this very test. So the
    /// goal path runs in a child process pinned to a tempdir: the parent only
    /// spawns it, and the child asserts both halves (state written to its cwd,
    /// source tree clean).
    ///
    /// Historical note: this block is `#[cfg(feature = "extension-kiqi-goal")]`, and
    /// that feature no longer exists - the compiled-in channel was retired on
    /// 2026-10-21. The block is kept as a record of what the compiled extension
    /// asserted; the live goal implementation is the wasm guest
    /// `extensions/wasm/kiqi-goal` (its `write_goals` writes the same
    /// `.ki/goals.json`), and the isolation it guards is enforced there too.
    #[cfg(feature = "extension-kiqi-goal")]
    #[test]
    fn goal_tool_writes_state_into_the_project_cwd() {
        const CHILD_ENV: &str = "KI_TEST_GOAL_CWD_CHILD";
        const TEST_NAME: &str = "extensions::tests::goal_tool_writes_state_into_the_project_cwd";

        if std::env::var_os(CHILD_ENV).is_some() {
            let project = std::env::current_dir().unwrap();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let fake = std::sync::Arc::new(FakeHost::default());
            let caps_host = ActionCaps::new(fake.clone(), runtime.handle().clone());
            let silent = Silent;
            // `inject` is granted deliberately: kiqi-goal cannot advance without
            // it, so this is the capability gate doing its job.
            let caps = HostCaps {
                session: Some(&caps_host),
                control: Some(&caps_host),
                inject: Some(&caps_host),
                ui: None,
                log: &silent,
                cancel: None,
            };

            let goal_host = ExtensionHost::new("kiqi-goal", kiqi_goal::kiqi::factory());
            let started = goal_host
                .call_tool(
                    "goal",
                    &serde_json::json!({"action": "start", "title": "wire goal"}),
                    &caps,
                    &NoProgress,
                )
                .expect("goal start runs");
            assert!(!started.is_error, "goal start succeeded: {started:?}");
            let injected = fake
                .seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(action, _)| action == "append_entry")
                .filter_map(|(_, args)| args.get("message").cloned())
                .any(|message| {
                    message
                        .get("data")
                        .and_then(|d| d.get("customType"))
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains("goal"))
                        .unwrap_or(false)
                });
            assert!(
                injected,
                "starting a goal injected its notice through InjectApi; saw {:?}",
                fake.seen.lock().unwrap()
            );

            // The state path follows `current_dir()`, so it belongs to the child.
            let written = project.join(".ki").join("goals.json");
            assert!(
                written.is_file(),
                "the goal tool wrote its state to {} (project dir {} holds {:?})",
                written.display(),
                project.display(),
                std::fs::read_dir(&project)
                    .map(|entries| entries
                        .filter_map(|e| e.ok())
                        .map(|e| e.file_name())
                        .collect::<Vec<_>>())
                    .unwrap_or_default()
            );
            // Regression lock: the tracked source-tree copy must not come back.
            let source_tree = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(".ki")
                .join("goals.json");
            assert!(
                !source_tree.exists(),
                "the goal test wrote into the source tree at {} - it must stay \
                 outside the checkout",
                source_tree.display()
            );
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(TEST_NAME)
            .arg("--nocapture")
            .env(CHILD_ENV, "1")
            .env(crate::config::CONFIG_DIR_ENV, &agent_dir)
            .current_dir(&project)
            .status()
            .unwrap();

        assert!(status.success(), "child test process failed: {status}");
        assert!(
            project.join(".ki").join("goals.json").is_file(),
            "the child wrote the goal state into its own project dir"
        );
    }

    /// The host-side wiring: an event dispatched through `kiqi-extensions`'
    /// funnel reaches a compiled extension's `on_event` with the session's
    /// capabilities. This is what makes a `ki` with no cdylibs still deliver
    /// events to `kiqi-todo`, `kiqi-langfuse`, and the rest.
    /// Uses the compiled kiqi-todo as the extension under test, so it runs in the
    /// configuration CI builds (`--features extension-kiqi-todo`), not the default.
    /// wasmtime's sync WASI waits call `Handle::block_on`, which panics on a
    /// thread that carries a tokio context (`spawn_blocking` threads do). The
    /// driver must therefore be a plain thread — the ask-user dialog's poll
    /// sleep is the call that surfaced this.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn extension_calls_run_outside_the_tokio_context() {
        let rx = spawn_extension_call("probe", || tokio::runtime::Handle::try_current().is_err())
            .expect("thread spawns");
        assert!(
            rx.await.expect("the call returns"),
            "extension calls must run outside every tokio context"
        );
    }

    #[cfg(feature = "extension-kiqi-todo")]
    #[test]
    fn kiqi_extensions_dispatch_reaches_compiled_extensions() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let fake = std::sync::Arc::new(FakeHost::default());
        let owned = OwnedCaps {
            session: Some(
                std::sync::Arc::new(ActionCaps::new(fake, runtime.handle().clone()))
                    as std::sync::Arc<dyn kiqi_host::SessionRead>,
            ),
            control: None,
            inject: None,
            ui: None,
            log: std::sync::Arc::new(Silent),
        };
        with_test_sink(owned, || {
            // A registry with no cdylib handlers still reaches the compiled extension.
            let native_handled = kiqi_host::dispatch_empty_event(kiqi_host::EventTag::SessionStart);
            assert!(!native_handled, "no cdylib subscribed");

            // kiqi-todo restored the task list the host served on the branch —
            // through the sink, not a direct `host.on_event` call.
            let host = ExtensionHost::new("kiqi-todo", kiqi_todo::kiqi::factory());
            let silent = Silent;
            let listed = host
                .call_tool(
                    "todo",
                    &serde_json::json!({"action": "list"}),
                    &HostCaps::observer(&silent),
                    &NoProgress,
                )
                .unwrap();
            let text = match &listed.blocks[0] {
                kiqi_host::Block::Text(text) => text.clone(),
                other => panic!("unexpected block {other:?}"),
            };
            assert!(text.contains("from the branch"), "got {text}");
        });
    }

    /// `SessionTree` (host emission on `/tree` navigate, `/session` switch,
    /// `/import`, `/fork`, `/clone`) rides the same funnel as `SessionStart`:
    /// one dispatch reaches the compiled trait face and the wasm guests (they
    /// share the host list), so the wasm todo/plan-mode guests re-restore from
    /// the swapped-in branch. Pins the funnel half of that contract with the
    /// compiled kiqi-todo as the extension under test (runs where
    /// `--features extension-kiqi-todo` builds); the emission sites are TUI
    /// wiring, pinned by `session_tree_notification_dispatches_and_veto_is_advisory`
    /// in `session.rs`.
    #[cfg(feature = "extension-kiqi-todo")]
    #[test]
    fn session_tree_event_reaches_the_compiled_extension() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let fake = std::sync::Arc::new(FakeHost::default());
        let owned = OwnedCaps {
            session: Some(
                std::sync::Arc::new(ActionCaps::new(fake, runtime.handle().clone()))
                    as std::sync::Arc<dyn kiqi_host::SessionRead>,
            ),
            control: None,
            inject: None,
            ui: None,
            log: std::sync::Arc::new(Silent),
        };
        with_test_sink(owned, || {
            // A registry with no cdylib handlers still fans SessionTree out to the
            // compiled+wasm tier.
            let native_handled = kiqi_host::dispatch_empty_event(kiqi_host::EventTag::SessionTree);
            assert!(!native_handled, "no cdylib subscribed");

            // kiqi-todo re-restored the task list the host served on the branch,
            // triggered by SessionTree — through the sink, not a direct
            // `host.on_event` call.
            let host = ExtensionHost::new("kiqi-todo", kiqi_todo::kiqi::factory());
            let silent = Silent;
            let listed = host
                .call_tool(
                    "todo",
                    &serde_json::json!({"action": "list"}),
                    &HostCaps::observer(&silent),
                    &NoProgress,
                )
                .unwrap();
            let text = match &listed.blocks[0] {
                kiqi_host::Block::Text(text) => text.clone(),
                other => panic!("unexpected block {other:?}"),
            };
            assert!(text.contains("from the branch"), "got {text}");
        });
    }

    /// Installing a sink must wait for a test that is mid-dispatch. Without the
    /// gate, `install_compiled_event_sink` can replace the test's sink between
    /// its install and its dispatch - a real-session rebuild in another test is
    /// exactly such an install - so the dispatch restores extension state from the
    /// wrong branch (`got No todos` was the observable form).
    ///
    /// The 100 ms bound is one-sided: in the fixed build the intruder is
    /// blocked on the mutex held below and cannot signal; in the unguarded
    /// build the install takes microseconds.
    #[test]
    fn a_concurrent_sink_install_cannot_swap_the_sink_mid_dispatch() {
        // Warm the process-global host pool first: `install_compiled_event_sink`
        // builds it on first use, which loads every wasm guest and can take
        // seconds. Doing it here keeps that cost out of the intruder's
        // post-release window (the 100 ms bound below would otherwise conflate
        // "blocked on the gate" with "still loading extensions").
        let _ = compiled_extension_hosts();
        let gate = lock_test_sink();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let intruder = std::thread::spawn(move || {
            started_tx.send(()).expect("started signal");
            // Takes the same test gate, so this returns only once it is
            // released. A real session build calls the same function.
            install_compiled_event_sink(empty_caps());
            // Leave the process-global slot as the test found it, under the gate:
            // an ungated clear here could empty the slot in the window between
            // another test's install and its dispatch.
            clear_compiled_event_sink();
            done_tx.send(()).expect("done signal");
        });

        started_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the intruder starts");
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "install_compiled_event_sink returned while a test held the gate"
        );

        drop(gate);
        // A safety net, not a timing assertion: under the full parallel suite
        // the released thread can wait behind other gated installs.
        done_rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the intruder finishes once the gate is released");
        intruder.join().expect("the intruder thread does not panic");
    }

    /// A clear must not land inside another test's install-to-dispatch window.
    ///
    /// Both writes to the process-global sink take [`TEST_SINK_LOCK`]. Gating
    /// only the install left the window open: `session.rs` builds install a real
    /// sink and dispatch through it later without holding the gate, so a
    /// neighbour's ungated clear emptied the slot in between - the same
    /// "got No todos" failure the gate exists to prevent, from the other side.
    ///
    /// Deterministic, not timing-based: the gate is held here, so the clearing
    /// thread can only proceed once this test releases it. Without the gate in
    /// `clear_compiled_event_sink` the clearing happens immediately and the
    /// "still present" assertion fails.
    #[test]
    fn a_sink_clear_cannot_land_inside_another_tests_window() {
        let gate = lock_test_sink();
        kiqi_host::set_compiled_event_sink(std::sync::Arc::new(|_, _| {
            kiqi_host::CompiledVerdict::Continue
        }));

        let (cleared_tx, cleared_rx) = std::sync::mpsc::channel();
        let clearer = std::thread::spawn(move || {
            clear_compiled_event_sink();
            cleared_tx.send(()).expect("cleared signal");
        });

        assert!(
            cleared_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "clear_compiled_event_sink returned while a test held the gate"
        );

        drop(gate);
        // A safety net rather than a timing assertion: under the full parallel
        // suite the released thread can wait behind other gated writers.
        cleared_rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the clearer finishes once the gate is released");
        clearer.join().expect("the clearer thread does not panic");
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
    impl Extension for Panicky {
        fn id(&self) -> &str {
            "panicky"
        }
        fn call(
            &self,
            _name: &str,
            _arguments: &serde_json::Value,
            _caps: &HostCaps<'_>,
            _sink: &dyn kiqi_host::ProgressSink,
        ) -> Result<kiqi_host::ToolOutcome, kiqi_host::ExtensionError> {
            panic!("this extension is broken")
        }
        fn on_event(
            &self,
            _event: &kiqi_host::Event,
            _caps: &HostCaps<'_>,
        ) -> kiqi_host::EventAction {
            panic!("this extension is broken on events too")
        }
    }

    #[test]
    fn a_panicking_extension_is_contained_and_does_not_take_the_loop_down() {
        let silent = Silent;
        let caps = HostCaps::observer(&silent);
        let host = ExtensionHost::new("panicky", Box::new(Panicky));

        let error = host
            .call_tool("anything", &serde_json::json!({}), &caps, &NoProgress)
            .unwrap_err();
        assert!(
            error.to_string().contains("panicked during call"),
            "got {error}"
        );
        assert_eq!(
            host.on_event(
                &kiqi_host::Event::new(kiqi_host::EventTag::SessionStart, serde_json::Value::Null),
                &caps
            ),
            kiqi_host::EventAction::Continue
        );

        // The process is still alive and every other extension still works.
        #[cfg(feature = "extension-kiqi-todo")]
        {
            let other = load_hosts();
            let todo = other.iter().find(|h| h.id() == "kiqi-todo").unwrap();
            assert!(todo
                .call_tool(
                    "todo",
                    &serde_json::json!({"action": "list"}),
                    &caps,
                    &NoProgress
                )
                .is_ok());
        }
    }

    /// Drives the compiled kiqi-todo through the trait; runs where that extension
    /// is built (the default build has no compiled extensions with tools).
    #[cfg(feature = "extension-kiqi-todo")]
    #[test]
    fn a_registered_extension_runs_through_the_trait_without_any_ffi() {
        let extensions = load_all();
        let todo = extensions.iter().find(|e| e.id == "kiqi-todo").unwrap();
        let silent = Silent;
        let caps = HostCaps::observer(&silent);

        let added = todo
            .extension
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
        // Order-independent: the extension instance (and therefore its list) is
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

        // Unknown tools are an extension error, not a panic or a crash.
        let err = todo
            .extension
            .call("nope", &serde_json::json!({}), &caps, &NoProgress)
            .unwrap_err();
        assert!(matches!(err, kiqi_host::ExtensionError::UnknownTool(_)));
    }

    /// The wasm orchestrator polls `proc.cancel-requested`; a cancelled tool
    /// call must return before it spawns anything.
    #[cfg(feature = "extension-wasm")]
    #[test]
    fn a_cancelled_wasm_dispatch_returns_immediately() {
        struct Cancelled;
        impl kiqi_host::CancelProbe for Cancelled {
            fn is_cancelled(&self) -> bool {
                true
            }
        }

        let wasm = build_guest("kiqi-orchestrator");
        let extension = kiqi_host::wasm::WasmExtension::load_with_grants(
            "kiqi-orchestrator",
            &wasm,
            kiqi_host::grants::Grants::default(),
        )
        .expect("load the orchestrator guest");

        let silent = Silent;
        let mut caps = HostCaps::observer(&silent);
        caps.cancel = Some(&Cancelled);
        let error = extension
            .call("dispatch", &serde_json::json!({}), &caps, &NoProgress)
            .expect_err("a cancelled dispatch must not run");
        assert!(
            error.to_string().contains("cancelled by the host"),
            "got {error}"
        );
    }

    /// The wasm tier is wired: a `.wasm` under `extensions/<id>/` is discovered and
    /// registered behind the same [`Extension`] seam as the compiled-in extensions.
    #[cfg(feature = "extension-wasm")]
    #[test]
    fn wasm_extensions_are_discovered_and_registered() {
        let wasm = build_guest("kiqi-conformance");

        // Lay it out the way the loader expects: extensions/<id>/extension.wasm.
        let extensions_dir = tempfile::tempdir().unwrap();
        let package = extensions_dir.path().join("conformance");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::copy(wasm, package.join("extension.wasm")).unwrap();

        let hosts = load_wasm_hosts(extensions_dir.path());
        let host = hosts
            .iter()
            .find(|host| host.id() == "conformance")
            .expect("the wasm extension is discovered");
        let tools = host.extension().tools();
        assert!(
            tools.iter().any(|tool| tool.name == "conformance"),
            "tools: {tools:?}"
        );

        // And it runs through the trait exactly like a compiled-in extension.
        let silent = Silent;
        let caps = HostCaps::observer(&silent);
        let outcome = host
            .call_tool(
                "conformance",
                &serde_json::json!({ "x": 1 }),
                &caps,
                &NoProgress,
            )
            .unwrap();
        let text = match &outcome.blocks[0] {
            kiqi_host::Block::Text(text) => text.clone(),
            other => panic!("unexpected block {other:?}"),
        };
        assert!(text.contains("ui="), "got {text}");
    }

    /// A sidecar that disagrees with the component makes the loader skip it:
    /// the recorded digest is the only expectation there is, so the tampered
    /// bytes never run, and the diagnostic names the path and both digests.
    #[cfg(feature = "extension-wasm")]
    #[test]
    fn wasm_digest_mismatch_skips_the_extension() {
        let wasm = build_guest("kiqi-conformance");
        let extensions_dir = tempfile::tempdir().unwrap();
        let package = extensions_dir.path().join("tampered");
        std::fs::create_dir_all(&package).unwrap();
        let component = package.join("extension.wasm");
        std::fs::copy(&wasm, &component).unwrap();
        let recorded = crate::extension_cli::write_digest_sidecar(&component).unwrap();

        let mut bytes = std::fs::read(&component).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&component, &bytes).unwrap();

        let diagnostic = digest_gate(&component).unwrap_err();
        assert!(
            diagnostic.contains(&component.display().to_string()),
            "names the path: {diagnostic}"
        );
        assert!(
            diagnostic.contains(&recorded),
            "names the recorded digest: {diagnostic}"
        );
        assert!(
            diagnostic.contains(&crate::extension_cli::sha256_hex(&bytes)),
            "names the actual digest: {diagnostic}"
        );
        assert!(
            load_wasm_hosts(extensions_dir.path())
                .iter()
                .all(|host| host.id() != "tampered"),
            "a mismatched component is never loaded"
        );
    }

    /// An install that predates sidecars still loads (old installs keep
    /// working); the gate reports it so the loader can warn once, loudly.
    #[cfg(feature = "extension-wasm")]
    #[test]
    fn wasm_missing_digest_sidecar_still_loads() {
        let wasm = build_guest("kiqi-conformance");
        let extensions_dir = tempfile::tempdir().unwrap();
        let package = extensions_dir.path().join("legacy");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::copy(wasm, package.join("extension.wasm")).unwrap();

        assert_eq!(
            digest_gate(&package.join("extension.wasm")).unwrap(),
            Some("missing")
        );
        assert!(
            load_wasm_hosts(extensions_dir.path())
                .iter()
                .any(|host| host.id() == "legacy"),
            "an install without a sidecar still loads"
        );
        // The load path must warn, not merely load: assert the exact summary
        // the caller prints, so deleting the warning cannot leave the suite
        // green.
        let warning = missing_sidecar_warning(&["legacy".to_string()]);
        assert!(
            warning.contains("1 wasm extension(s)")
                && warning.contains("loaded unverified: legacy")
                && warning.contains("ki insmod"),
            "the missing-sidecar warning names the extension and the remedy: {warning}"
        );
    }

    /// An extension that fails to load is not counted as "loaded unverified".
    /// `missing_sidecar_warning` only ever receives ids that are in `hosts`,
    /// and a corrupt component (here: not a component at all) must stay out.
    #[cfg(feature = "extension-wasm")]
    #[test]
    fn wasm_failed_load_is_not_counted_as_unverified() {
        let extensions_dir = tempfile::tempdir().unwrap();
        let package = extensions_dir.path().join("broken");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("extension.wasm"), b"not a component").unwrap();

        assert_eq!(
            digest_gate(&package.join("extension.wasm")).unwrap(),
            Some("missing")
        );
        assert!(
            load_wasm_hosts(extensions_dir.path()).is_empty(),
            "a corrupt component without a sidecar is skipped, not loaded"
        );
    }

    /// Gate §6.3.8: quantify the wasm sandbox tax and keep it from exploding.
    ///
    /// The bound is a **ratio against the compiled path measured in the same
    /// process** (never an absolute latency), so a slow or busy CI runner moves
    /// both sides together. This is a regression guard, not a benchmark.
    #[cfg(feature = "extension-wasm")]
    /// Compares the wasm guest against the compiled kiqi-todo; the comparison
    /// needs both, so it runs where the compiled extension is built.
    #[cfg(feature = "extension-kiqi-todo")]
    #[test]
    fn wasm_call_overhead_stays_within_the_compiled_paths_ballpark() {
        fn per_call_ns(iterations: u64, mut call: impl FnMut()) -> u128 {
            let start = std::time::Instant::now();
            for _ in 0..iterations {
                call();
            }
            start.elapsed().as_nanos() / u128::from(iterations)
        }

        let wasm_path = build_guest("kiqi-todo");
        let wasm_extension = kiqi_host::wasm::WasmExtension::load_with_grants(
            "kiqi-todo",
            &wasm_path,
            kiqi_host::wasm::Grants::default(),
        )
        .expect("load the wasm todo");

        let compiled = ExtensionHost::new("kiqi-todo", kiqi_todo::kiqi::factory());
        let wasm = ExtensionHost::new("kiqi-todo-wasm", Box::new(wasm_extension));
        let silent = Silent;
        let caps = HostCaps::observer(&silent);
        let params = serde_json::json!({ "action": "list" });

        // Warm both paths so neither pays first-call costs.
        let _ = compiled.call_tool("todo", &params, &caps, &NoProgress);
        let _ = wasm.call_tool("todo", &params, &caps, &NoProgress);

        let iterations = 2_000;
        let compiled_ns = per_call_ns(iterations, || {
            let _ = compiled.call_tool("todo", &params, &caps, &NoProgress);
        });
        let wasm_ns = per_call_ns(iterations, || {
            let _ = wasm.call_tool("todo", &params, &caps, &NoProgress);
        });
        let ratio = wasm_ns as f64 / compiled_ns as f64;
        eprintln!(
            "todo list: compiled {compiled_ns} ns/call, wasm {wasm_ns} ns/call ({ratio:.1}x)"
        );
        // Generous: the point is to catch an order-of-magnitude regression, not
        // to pin a number that varies by runner.
        assert!(
            ratio < 200.0,
            "wasm tax exploded: {ratio:.1}x ({wasm_ns} vs {compiled_ns} ns/call)"
        );
    }

    /// A synthetic extension exposing every non-tool surface, so the session wiring
    /// can be exercised without depending on which real extension declares what.
    struct Surface {
        prompt_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Extension for Surface {
        fn id(&self) -> &str {
            "surface"
        }

        fn commands(&self) -> Vec<kiqi_host::CommandSpec> {
            vec![kiqi_host::CommandSpec {
                name: "surface".into(),
                description: "surface command".into(),
            }]
        }

        fn run_command(
            &self,
            _name: &str,
            args: &str,
            _caps: &HostCaps<'_>,
        ) -> Result<kiqi_host::CommandResult, kiqi_host::ExtensionError> {
            Ok(kiqi_host::CommandResult::message(format!(
                "surface ran {args}"
            )))
        }

        fn shortcuts(&self) -> Vec<kiqi_host::ShortcutSpec> {
            vec![kiqi_host::ShortcutSpec {
                key: "f8".into(),
                description: "surface shortcut".into(),
            }]
        }

        fn probe_points(&self) -> Vec<String> {
            vec!["before-tool-call".into()]
        }

        fn probe(
            &self,
            point: &str,
            payload: &serde_json::Value,
            _caps: &HostCaps<'_>,
        ) -> kiqi_host::Verdict {
            assert_eq!(point, "before-tool-call");
            let args = &payload["args"];
            if args["block"] == serde_json::json!(true) {
                return kiqi_host::Verdict::Block {
                    reason: format!("{} is blocked", payload["toolName"].as_str().unwrap_or("?")),
                };
            }
            if let Some(rewrite) = args.get("rewrite") {
                return kiqi_host::Verdict::Replace(rewrite.clone());
            }
            kiqi_host::Verdict::Continue
        }

        fn before_agent_start(
            &self,
            prompt: &serde_json::Value,
            _caps: &HostCaps<'_>,
        ) -> Option<String> {
            self.prompt_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let text = prompt["prompt"].as_str().unwrap_or("");
            Some(format!("{text}\n[surface]"))
        }

        fn on_event(
            &self,
            event: &kiqi_host::Event,
            _caps: &HostCaps<'_>,
        ) -> kiqi_host::EventAction {
            // It only ever sees `Input` for a key it declared (`f8`), which is
            // what the dispatch-filter test relies on.
            if event.tag == kiqi_host::EventTag::Input {
                kiqi_host::EventAction::Claimed
            } else {
                kiqi_host::EventAction::Continue
            }
        }
    }

    fn surface_host() -> (
        std::sync::Arc<ExtensionHost>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let prompt_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let extension = Box::new(Surface {
            prompt_calls: prompt_calls.clone(),
        });
        (
            std::sync::Arc::new(ExtensionHost::new("surface", extension)),
            prompt_calls,
        )
    }

    /// The `before-tool-call` gate: block wins, a rewrite reaches the runtime,
    /// and an extension that did not declare the point is never asked.
    #[test]
    fn probe_tool_call_blocks_and_rewrites() {
        let (host, _surface) = surface_host();
        let caps = OwnedCaps {
            session: None,
            control: None,
            inject: None,
            ui: None,
            log: std::sync::Arc::new(StderrLog),
        };

        let blocked = probe_tool_call_with(
            &[host.clone()],
            &caps,
            "call-1",
            "bash",
            &serde_json::json!({ "block": true }),
        );
        assert_eq!(
            blocked,
            Some(kiqi_host::Verdict::Block {
                reason: "bash is blocked".into()
            })
        );

        let rewritten = probe_tool_call_with(
            &[host.clone()],
            &caps,
            "call-2",
            "bash",
            &serde_json::json!({ "rewrite": { "command": "ls" } }),
        );
        assert_eq!(
            rewritten,
            Some(kiqi_host::Verdict::Replace(
                serde_json::json!({ "command": "ls" })
            ))
        );

        let untouched = probe_tool_call_with(
            &[host],
            &caps,
            "call-3",
            "bash",
            &serde_json::json!({ "command": "ls" }),
        );
        assert_eq!(untouched, None);
    }

    /// The compiled prompt transform runs once per prompt, no matter how many
    /// provider calls the run makes.
    #[test]
    fn compiled_prompt_transform_runs_once_per_prompt() {
        let (host, surface) = surface_host();
        let hooks = CompiledPromptHooks {
            inner: None,
            hosts: vec![host],
            caps: std::sync::Arc::new(caps_with_id("prompt-test"))
                as std::sync::Arc<dyn kiqi_host::SessionCaps>,
            cache: std::sync::Mutex::new(None),
        };
        let event = serde_json::json!({ "prompt": "hello" });
        let calls = || surface.load(std::sync::atomic::Ordering::SeqCst);

        let first = hooks.compiled_transform(1, &event);
        assert_eq!(first.as_deref(), Some("hello\n[surface]"));
        assert_eq!(calls(), 1);

        // Same prompt (a later provider call in the same run): cached.
        assert_eq!(hooks.compiled_transform(1, &event), first);
        assert_eq!(calls(), 1, "a run must not recompute");

        // New prompt: computed again, exactly once.
        assert_eq!(hooks.compiled_transform(2, &event), first);
        assert_eq!(calls(), 2);
    }

    /// Shortcut routing only hands the key to extensions that declared it.
    #[test]
    fn shortcut_hosts_match_the_declared_key() {
        let (host, _surface) = surface_host();
        let hosts = vec![host];
        assert_eq!(shortcut_hosts(&hosts, "f8").len(), 1);
        assert!(shortcut_hosts(&hosts, "f9").is_empty());
    }

    /// The compiled event fan-out: a key only reaches the extensions that declared
    /// it, and a non-`Continue` action becomes a claim the routing dispatcher can
    /// act on.
    #[test]
    fn compiled_event_dispatch_filters_keys_and_folds_claims() {
        let (host, _surface) = surface_host();
        let caps = empty_caps();
        assert_eq!(
            dispatch_to_compiled(
                &[host.clone()],
                &caps,
                kiqi_host::EventTag::Input,
                serde_json::json!({ "key": "f8" }),
                kiqi_host::DispatchMode::Claiming,
            ),
            kiqi_host::CompiledVerdict::Claimed,
            "the extension declared f8, so it is asked and its claim counts"
        );
        assert_eq!(
            dispatch_to_compiled(
                &[host.clone()],
                &caps,
                kiqi_host::EventTag::Input,
                serde_json::json!({ "key": "f9" }),
                kiqi_host::DispatchMode::Claiming,
            ),
            kiqi_host::CompiledVerdict::Continue,
            "an undeclared key must not reach the extension at all"
        );
        assert_eq!(
            dispatch_to_compiled(
                &[host],
                &caps,
                kiqi_host::EventTag::SessionStart,
                serde_json::Value::Null,
                kiqi_host::DispatchMode::Observe,
            ),
            kiqi_host::CompiledVerdict::Continue,
            "a lifecycle event is observed but not claimed"
        );
    }

    /// Observe fan-out must go through `try_on_event` (a busy guest is skipped,
    /// not waited for); claiming fan-out must keep the blocking call.
    #[test]
    fn observe_dispatch_does_not_wait_for_a_busy_extension() {
        struct Split;
        impl kiqi_host::Extension for Split {
            fn id(&self) -> &str {
                "split"
            }
            fn on_event(&self, _: &kiqi_host::Event, _: &HostCaps<'_>) -> kiqi_host::EventAction {
                kiqi_host::EventAction::Continue
            }
            fn try_on_event(
                &self,
                _: &kiqi_host::Event,
                _: &HostCaps<'_>,
            ) -> kiqi_host::EventAction {
                kiqi_host::EventAction::Claimed
            }
        }
        let host = std::sync::Arc::new(ExtensionHost::new("split", Box::new(Split)));
        let caps = empty_caps();
        assert_eq!(
            dispatch_to_compiled(
                &[host.clone()],
                &caps,
                kiqi_host::EventTag::SessionStart,
                serde_json::Value::Null,
                kiqi_host::DispatchMode::Observe,
            ),
            kiqi_host::CompiledVerdict::Claimed,
            "observe must use try_on_event"
        );
        assert_eq!(
            dispatch_to_compiled(
                &[host],
                &caps,
                kiqi_host::EventTag::SessionStart,
                serde_json::Value::Null,
                kiqi_host::DispatchMode::Claiming,
            ),
            kiqi_host::CompiledVerdict::Continue,
            "claiming must use the blocking on_event"
        );
    }

    // ---- provider-request partition and gate --------------------------------

    #[derive(Default)]
    struct GateCounts {
        observe: std::sync::atomic::AtomicUsize,
        gate: std::sync::atomic::AtomicUsize,
    }

    struct GateProbe {
        id: &'static str,
        policy: Option<kiqi_host::grants::ProviderRequestPolicy>,
        counts: std::sync::Arc<GateCounts>,
        patch: serde_json::Value,
        reject: Option<String>,
        observe_action: kiqi_host::EventAction,
    }

    impl Extension for GateProbe {
        fn id(&self) -> &str {
            self.id
        }

        fn provider_request_policy(&self) -> Option<kiqi_host::grants::ProviderRequestPolicy> {
            self.policy
        }

        fn on_event(&self, _: &kiqi_host::Event, _: &HostCaps<'_>) -> kiqi_host::EventAction {
            self.counts
                .observe
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.observe_action.clone()
        }

        fn try_on_event(&self, _: &kiqi_host::Event, _: &HostCaps<'_>) -> kiqi_host::EventAction {
            self.counts
                .observe
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.observe_action.clone()
        }

        fn provider_gate(
            &self,
            _: &kiqi_host::Event,
            _: &HostCaps<'_>,
        ) -> Result<Option<serde_json::Value>, String> {
            self.counts
                .gate
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(reason) = &self.reject {
                return Err(reason.clone());
            }
            Ok((self.patch != serde_json::Value::Null).then(|| self.patch.clone()))
        }
    }

    fn gate_policy(
        observe: bool,
        patch: bool,
        timeout_ms: u64,
    ) -> kiqi_host::grants::ProviderRequestPolicy {
        kiqi_host::grants::ProviderRequestPolicy {
            observe,
            patch,
            patch_timeout: std::time::Duration::from_millis(timeout_ms),
        }
    }

    fn gate_probe(
        id: &'static str,
        policy: Option<kiqi_host::grants::ProviderRequestPolicy>,
        patch: serde_json::Value,
        reject: Option<&str>,
        observe_action: kiqi_host::EventAction,
    ) -> (std::sync::Arc<ExtensionHost>, std::sync::Arc<GateCounts>) {
        let counts = std::sync::Arc::new(GateCounts::default());
        let extension = Box::new(GateProbe {
            id,
            policy,
            counts: counts.clone(),
            patch,
            reject: reject.map(str::to_string),
            observe_action,
        });
        (
            std::sync::Arc::new(ExtensionHost::new(id, extension)),
            counts,
        )
    }

    /// Per-host partition: patch-capable hosts are invoked only by the gate,
    /// observe-only hosts only by the fan-out, ungranted hosts by neither — and
    /// each host is called exactly once per request.
    #[test]
    fn provider_request_partition_invokes_each_host_exactly_once() {
        let caps = empty_caps();
        let (patch_host, patch_counts) = gate_probe(
            "patch",
            Some(gate_policy(false, true, 500)),
            serde_json::json!({ "maxTokens": 128 }),
            None,
            kiqi_host::EventAction::Continue,
        );
        let (observe_host, observe_counts) = gate_probe(
            "observe",
            Some(gate_policy(true, false, 500)),
            serde_json::Value::Null,
            None,
            kiqi_host::EventAction::Continue,
        );
        let (none_host, none_counts) = gate_probe(
            "none",
            Some(gate_policy(false, false, 500)),
            serde_json::Value::Null,
            None,
            kiqi_host::EventAction::Continue,
        );
        let hosts = vec![patch_host, observe_host, none_host];

        let verdict = dispatch_to_compiled(
            &hosts,
            &caps,
            kiqi_host::EventTag::BeforeProviderRequest,
            serde_json::json!({ "model": "m" }),
            kiqi_host::DispatchMode::Observe,
        );
        assert_eq!(verdict, kiqi_host::CompiledVerdict::Continue);
        assert_eq!(
            patch_counts
                .observe
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a patch-capable host must not be observed"
        );
        assert_eq!(
            observe_counts
                .observe
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "an observe-only host is observed once"
        );
        assert_eq!(
            none_counts
                .observe
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a host with neither switch is never called"
        );

        let patch = dispatch_provider_gate(
            &hosts,
            &caps,
            kiqi_host::EventTag::BeforeProviderRequest,
            serde_json::json!({ "model": "m" }),
        );
        assert_eq!(patch, Some(serde_json::json!({ "maxTokens": 128 })));
        assert_eq!(
            patch_counts.gate.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the patch-capable host is gated once"
        );
        assert_eq!(
            observe_counts
                .gate
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an observe-only host is never gated"
        );

        // Exactly one invocation per request, per host.
        assert_eq!(
            patch_counts
                .observe
                .load(std::sync::atomic::Ordering::SeqCst)
                + patch_counts.gate.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            observe_counts
                .observe
                .load(std::sync::atomic::Ordering::SeqCst)
                + observe_counts
                    .gate
                    .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            none_counts
                .observe
                .load(std::sync::atomic::Ordering::SeqCst)
                + none_counts.gate.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    /// Header observation uses the same per-host grant partition as the
    /// request event. AfterProviderResponse is native-only: neither compiled
    /// nor wasm hosts have a delivery path for it.
    #[test]
    fn provider_header_hooks_are_partitioned_and_after_response_is_dropped() {
        let caps = empty_caps();
        let (observe_host, observe_counts) = gate_probe(
            "observe",
            Some(gate_policy(true, false, 500)),
            serde_json::Value::Null,
            None,
            kiqi_host::EventAction::Continue,
        );
        let (patch_host, patch_counts) = gate_probe(
            "patch",
            Some(gate_policy(false, true, 500)),
            serde_json::Value::Null,
            None,
            kiqi_host::EventAction::Continue,
        );
        let (none_host, none_counts) = gate_probe(
            "none",
            Some(gate_policy(false, false, 500)),
            serde_json::Value::Null,
            None,
            kiqi_host::EventAction::Continue,
        );
        // AfterProviderResponse 在 0.8.0 起不再派发（无消费者，见 extension-abi
        // §6.41）。BeforeProviderHeaders 属于 provider 事件：有 grant 的主机
        // 都收（patch 主机也不会重复——它只经 gate 收 BeforeProviderRequest），
        // 无 grant 的主机不收。计数器跨条目累加，所以每条都用全新主机。
        for (tag, observe_expected, patch_expected) in [
            (kiqi_host::EventTag::BeforeProviderHeaders, 1usize, 1usize),
            (kiqi_host::EventTag::AfterProviderResponse, 0usize, 0usize),
        ] {
            let (observe_host, observe_counts) = gate_probe(
                "observe",
                Some(gate_policy(true, false, 500)),
                serde_json::Value::Null,
                None,
                kiqi_host::EventAction::Continue,
            );
            let (patch_host, patch_counts) = gate_probe(
                "patch",
                Some(gate_policy(false, true, 500)),
                serde_json::Value::Null,
                None,
                kiqi_host::EventAction::Continue,
            );
            let (none_host, none_counts) = gate_probe(
                "none",
                Some(gate_policy(false, false, 500)),
                serde_json::Value::Null,
                None,
                kiqi_host::EventAction::Continue,
            );
            let hosts = vec![observe_host, patch_host, none_host];
            dispatch_to_compiled(
                &hosts,
                &caps,
                tag,
                serde_json::json!({ "secret": "must-not-cross" }),
                kiqi_host::DispatchMode::Observe,
            );
            assert_eq!(
                observe_counts
                    .observe
                    .load(std::sync::atomic::Ordering::SeqCst),
                observe_expected,
                "observe host count after {tag:?}"
            );
            assert_eq!(
                patch_counts
                    .observe
                    .load(std::sync::atomic::Ordering::SeqCst),
                patch_expected,
                "patch host count after {tag:?}"
            );
            assert_eq!(
                none_counts
                    .observe
                    .load(std::sync::atomic::Ordering::SeqCst),
                0,
                "ungranted host must not observe {tag:?}"
            );
        }
    }

    /// The provider partition is scoped to `BeforeProviderRequest`: a
    /// patch-capable host is skipped only on that tag, so it still receives
    /// lifecycle events (SessionStart, MessageUpdate) that its trace needs.
    #[test]
    fn a_patch_capable_host_still_observes_lifecycle_events() {
        let caps = empty_caps();
        let (patch_host, counts) = gate_probe(
            "patch",
            Some(gate_policy(false, true, 500)),
            serde_json::json!({ "maxTokens": 8 }),
            None,
            kiqi_host::EventAction::Continue,
        );
        for tag in [
            kiqi_host::EventTag::SessionStart,
            kiqi_host::EventTag::MessageUpdate,
        ] {
            assert_eq!(
                dispatch_to_compiled(
                    &[patch_host.clone()],
                    &caps,
                    tag,
                    serde_json::json!({}),
                    kiqi_host::DispatchMode::Observe,
                ),
                kiqi_host::CompiledVerdict::Continue,
                "{tag:?} is observed, not claimed"
            );
        }
        assert_eq!(
            counts.observe.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "a patch-capable host must still see the lifecycle fan-out"
        );

        // The provider event keeps the partition: the gate, not the fan-out,
        // invokes this host.
        dispatch_to_compiled(
            &[patch_host],
            &caps,
            kiqi_host::EventTag::BeforeProviderRequest,
            serde_json::json!({ "model": "m" }),
            kiqi_host::DispatchMode::Observe,
        );
        assert_eq!(
            counts.observe.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the patch-capable host is not also observed for the request"
        );
    }

    /// One host's rejected patch drops the whole request patch, including a
    /// patch an earlier host contributed.
    #[test]
    fn one_rejected_patch_leaves_the_request_unpatched() {
        let caps = empty_caps();
        let (valid, _) = gate_probe(
            "valid",
            Some(gate_policy(false, true, 500)),
            serde_json::json!({ "temperature": 0.5 }),
            None,
            kiqi_host::EventAction::Continue,
        );
        let (invalid, _) = gate_probe(
            "invalid",
            Some(gate_policy(false, true, 500)),
            serde_json::json!({ "maxTokens": 8 }),
            Some("protected header"),
            kiqi_host::EventAction::Continue,
        );
        assert_eq!(
            dispatch_provider_gate(
                &[valid, invalid],
                &caps,
                kiqi_host::EventTag::BeforeProviderRequest,
                serde_json::json!({ "model": "m" }),
            ),
            None
        );
    }

    /// A `Patch` action on the observe fan-out is not a claim and changes
    /// nothing; the gate ignores any tag other than `before-provider-request`.
    #[test]
    fn a_patch_outside_the_gate_changes_nothing() {
        let caps = empty_caps();
        let (patch_host, counts) = gate_probe(
            "patch",
            Some(gate_policy(true, true, 500)),
            serde_json::json!({ "maxTokens": 8 }),
            None,
            kiqi_host::EventAction::Patch(serde_json::json!({ "maxTokens": 8 })),
        );
        assert_eq!(
            dispatch_to_compiled(
                &[patch_host.clone()],
                &caps,
                kiqi_host::EventTag::BeforeProviderRequest,
                serde_json::json!({ "model": "m" }),
                kiqi_host::DispatchMode::Observe,
            ),
            kiqi_host::CompiledVerdict::Continue,
            "an observe patch is ignored"
        );
        assert_eq!(
            dispatch_provider_gate(
                &[patch_host],
                &caps,
                kiqi_host::EventTag::Context,
                serde_json::json!({ "tagPatch": { "maxTokens": 8 } }),
            ),
            None,
            "the gate only handles before-provider-request"
        );
        assert_eq!(
            counts.gate.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a non-provider tag must not reach the host at all"
        );
    }

    /// The gate deadline is wall-clock: a slow guest has no say, and the host is
    /// not retried.
    #[cfg(feature = "extension-wasm")]
    #[test]
    fn provider_gate_times_out_without_patching_or_retrying() {
        let wasm = build_guest("kiqi-conformance");
        let grants = kiqi_host::grants::Grants {
            provider_request: Some(kiqi_host::grants::ProviderRequestGrant {
                observe: kiqi_host::grants::ObserveMode::Requests,
                patch: true,
                patch_timeout_ms: 100,
                ..Default::default()
            }),
            ..Default::default()
        };
        let extension =
            kiqi_host::wasm::WasmExtension::load_with_grants("conformance", &wasm, grants)
                .expect("load the conformance guest");
        let host = std::sync::Arc::new(ExtensionHost::new("conformance", Box::new(extension)));
        let caps = OwnedCaps {
            session: None,
            control: None,
            inject: None,
            ui: None,
            log: std::sync::Arc::new(Silent),
        };

        let started = std::time::Instant::now();
        let patch = dispatch_provider_gate(
            &[host],
            &caps,
            kiqi_host::EventTag::BeforeProviderRequest,
            serde_json::json!({
                "model": "m",
                "gatePatch": { "maxTokens": 8 },
                "gateSpin": true,
            }),
        );
        assert!(patch.is_none(), "a timed-out gate must not patch");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(350),
            "the deadline must fire well before the guest's 400 ms sleep: {:?}",
            started.elapsed()
        );
    }

    /// A gate guest that spins (not sleeps) is interrupted at the grant's
    /// deadline, so the worker releases the instance mutex: a later call on the
    /// same extension must answer promptly instead of queueing behind it forever.
    #[cfg(feature = "extension-wasm")]
    #[test]
    fn provider_gate_timeout_releases_the_instance_for_later_calls() {
        let wasm = build_guest("kiqi-conformance");
        let grants = kiqi_host::grants::Grants {
            provider_request: Some(kiqi_host::grants::ProviderRequestGrant {
                observe: kiqi_host::grants::ObserveMode::Requests,
                patch: true,
                patch_timeout_ms: 100,
                ..Default::default()
            }),
            ..Default::default()
        };
        let extension =
            kiqi_host::wasm::WasmExtension::load_with_grants("conformance", &wasm, grants)
                .expect("load the conformance guest");
        let host = std::sync::Arc::new(ExtensionHost::new("conformance", Box::new(extension)));
        let caps = OwnedCaps {
            session: None,
            control: None,
            inject: None,
            ui: None,
            log: std::sync::Arc::new(Silent),
        };

        let started = std::time::Instant::now();
        let patch = dispatch_provider_gate(
            &[host.clone()],
            &caps,
            kiqi_host::EventTag::BeforeProviderRequest,
            serde_json::json!({
                "model": "m",
                "gatePatch": { "maxTokens": 8 },
                "gateSpinForever": true,
            }),
        );
        assert!(patch.is_none(), "an interrupted gate must not patch");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "the epoch deadline must bound a spinning guest: {:?}",
            started.elapsed()
        );

        // Do the follow-up call off the test thread so the old behaviour (the
        // abandoned worker keeps the mutex) fails the assertion, not the run.
        let probe_host = host.clone();
        let probe_caps = caps.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let verdict = probe_tool_call_with(
                &[probe_host],
                &probe_caps,
                "c1",
                "bash",
                &serde_json::json!({ "block": true }),
            );
            let _ = tx.send(verdict);
        });
        let verdict = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the timed-out gate must not leave the instance mutex held");
        assert_eq!(
            verdict,
            Some(kiqi_host::Verdict::Block {
                reason: "conformance blocks this".into()
            }),
            "the extension must still answer after a timed-out gate"
        );
    }

    /// A guest parked in a synchronous host import is invisible to epoch
    /// interruption. Its abandoned gate worker must not retain the shared
    /// instance: probes, tools, and a fresh gate all stay bounded meanwhile.
    #[cfg(feature = "extension-wasm")]
    #[test]
    fn provider_gate_parked_in_host_import_does_not_block_shared_instance() {
        #[derive(Default)]
        struct ParkingUi {
            state: std::sync::Mutex<(bool, bool)>,
            wake: std::sync::Condvar,
        }

        impl ParkingUi {
            fn wait_entered(&self) -> bool {
                let state = self.state.lock().unwrap();
                let (state, _) = self
                    .wake
                    .wait_timeout_while(state, std::time::Duration::from_secs(2), |state| !state.0)
                    .unwrap();
                state.0
            }

            fn release(&self) {
                let mut state = self.state.lock().unwrap();
                state.1 = true;
                self.wake.notify_all();
            }
        }

        impl kiqi_host::UiApi for ParkingUi {
            fn dialog(
                &self,
                _: &serde_json::Value,
            ) -> Result<serde_json::Value, kiqi_host::ExtensionError> {
                let mut state = self.state.lock().unwrap();
                state.0 = true;
                self.wake.notify_all();
                while !state.1 {
                    state = self.wake.wait(state).unwrap();
                }
                Ok(serde_json::Value::Null)
            }

            fn set_status(&self, _: &serde_json::Value) -> Result<(), kiqi_host::ExtensionError> {
                Ok(())
            }

            fn set_editor_text(
                &self,
                _: &serde_json::Value,
            ) -> Result<(), kiqi_host::ExtensionError> {
                Ok(())
            }

            fn component(
                &self,
                _: &str,
                _: &serde_json::Value,
            ) -> Result<serde_json::Value, kiqi_host::ExtensionError> {
                Err(kiqi_host::ExtensionError::Failed(
                    "no components here".into(),
                ))
            }
        }

        let wasm = build_guest("kiqi-conformance");
        let grants = kiqi_host::grants::Grants {
            provider_request: Some(kiqi_host::grants::ProviderRequestGrant {
                observe: kiqi_host::grants::ObserveMode::Requests,
                patch: true,
                patch_timeout_ms: 100,
                ..Default::default()
            }),
            ..Default::default()
        };
        let extension =
            kiqi_host::wasm::WasmExtension::load_with_grants("conformance", &wasm, grants)
                .expect("load the conformance guest");
        let host = std::sync::Arc::new(ExtensionHost::new("conformance", Box::new(extension)));
        let ui = std::sync::Arc::new(ParkingUi::default());
        let caps = OwnedCaps {
            session: None,
            control: None,
            inject: None,
            ui: Some(ui.clone()),
            log: std::sync::Arc::new(Silent),
        };

        let patch = dispatch_provider_gate(
            &[host.clone()],
            &caps,
            kiqi_host::EventTag::BeforeProviderRequest,
            serde_json::json!({ "model": "m", "gatePark": true }),
        );
        assert!(patch.is_none(), "the parked gate must time out unpatched");
        assert!(ui.wait_entered(), "the guest must be parked in ui.dialog");

        let probe_host = host.clone();
        let probe_caps = caps.clone();
        let (probe_tx, probe_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let verdict = probe_tool_call_with(
                &[probe_host],
                &probe_caps,
                "c1",
                "bash",
                &serde_json::json!({ "block": true }),
            );
            let _ = probe_tx.send(verdict);
        });
        let probe = probe_rx.recv_timeout(std::time::Duration::from_secs(2));

        let tool_host = host.clone();
        let tool_caps = caps.clone();
        let (tool_tx, tool_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome = tool_host.call_tool(
                "conformance",
                &serde_json::json!({}),
                &tool_caps.caps(),
                &NoProgress,
            );
            let _ = tool_tx.send(outcome);
        });
        let tool = tool_rx.recv_timeout(std::time::Duration::from_secs(2));

        // The next gate must instantiate independently rather than queue
        // behind the abandoned guest.
        let next_patch = dispatch_provider_gate(
            &[host],
            &caps,
            kiqi_host::EventTag::BeforeProviderRequest,
            serde_json::json!({
                "model": "m",
                "gatePatch": { "maxTokens": 16 },
            }),
        );

        // Always release the parked import before asserting, so a regression
        // fails at the bounded receives instead of leaking a wedged test thread.
        ui.release();
        assert_eq!(
            probe.expect("a parked gate must not hold the probe instance lock"),
            Some(kiqi_host::Verdict::Block {
                reason: "conformance blocks this".into()
            })
        );
        tool.expect("a parked gate must not hold the tool instance lock")
            .expect("the shared tool instance must still answer");
        assert_eq!(
            next_patch,
            Some(serde_json::json!({ "maxTokens": 16 })),
            "the next gate must use a fresh instance"
        );
    }

    /// The same wiring, driven through a real wasm component: the conformance
    /// guest declares every non-tool surface, so this proves the session-level
    /// paths reach a sandboxed extension, not just a compiled one.
    #[cfg(feature = "extension-wasm")]
    #[test]
    fn wasm_non_tool_surfaces_reach_the_session_wiring() {
        let wasm = build_guest("kiqi-conformance");
        let extension = kiqi_host::wasm::WasmExtension::load_with_grants(
            "conformance",
            &wasm,
            kiqi_host::grants::Grants::default(),
        )
        .expect("load the conformance guest");
        let host = std::sync::Arc::new(ExtensionHost::new("conformance", Box::new(extension)));
        let caps = OwnedCaps {
            session: None,
            control: None,
            inject: None,
            ui: None,
            log: std::sync::Arc::new(Silent),
        };

        // `/command` reaches the extension and its text comes back.
        let result = host
            .run_command("conformance", "", &caps.caps())
            .expect("the conformance command runs");
        assert_eq!(
            result.body,
            kiqi_host::CommandResultBody::Message("conformance command ran".to_string())
        );

        // `f8` is routed only because the component declared it.
        assert_eq!(shortcut_hosts(&[host.clone()], "f8").len(), 1);

        // `before-tool-call` veto, decided inside the sandbox.
        let blocked = probe_tool_call_with(
            &[host.clone()],
            &caps,
            "c1",
            "bash",
            &serde_json::json!({ "block": true }),
        );
        assert_eq!(
            blocked,
            Some(kiqi_host::Verdict::Block {
                reason: "conformance blocks this".into()
            })
        );

        // Prompt transform, computed once per prompt.
        let hooks = CompiledPromptHooks {
            inner: None,
            hosts: vec![host],
            caps: std::sync::Arc::new(caps_with_id("prompt-test"))
                as std::sync::Arc<dyn kiqi_host::SessionCaps>,
            cache: std::sync::Mutex::new(None),
        };
        let event = serde_json::json!({ "prompt": "hi" });
        let first = hooks.compiled_transform(7, &event);
        assert!(
            first
                .as_deref()
                .is_some_and(|prompt| prompt.ends_with("[conformance]")),
            "got {first:?}"
        );
        assert_eq!(hooks.compiled_transform(7, &event), first);
    }

    /// An extension that contributes exactly one tool, to exercise name collisions
    /// between tiers.
    struct FixedTool {
        id: &'static str,
        tool: &'static str,
    }

    impl Extension for FixedTool {
        fn id(&self) -> &str {
            self.id
        }

        fn tools(&self) -> Vec<kiqi_host::ToolSpec> {
            vec![kiqi_host::ToolSpec {
                name: self.tool.into(),
                description: self.id.into(),
                parameters: serde_json::json!({ "type": "object", "properties": {} }),
            }]
        }

        fn call(
            &self,
            _name: &str,
            _arguments: &serde_json::Value,
            _caps: &HostCaps<'_>,
            _sink: &dyn kiqi_host::ProgressSink,
        ) -> Result<kiqi_host::ToolOutcome, kiqi_host::ExtensionError> {
            Ok(kiqi_host::ToolOutcome::text(self.id))
        }
    }

    fn empty_caps() -> OwnedCaps {
        OwnedCaps {
            session: None,
            control: None,
            inject: None,
            ui: None,
            log: std::sync::Arc::new(StderrLog),
        }
    }

    async fn tool_text(tool: &kiqi_harness::types::HarnessTool) -> String {
        use kiqi_agent::agent_tool::AgentTool;
        let result = tool
            .tool
            .execute(
                "c",
                serde_json::json!({}),
                tokio_util::sync::CancellationToken::new(),
                std::sync::Arc::new(|_| {}),
            )
            .await
            .expect("the tool runs");
        result
            .content
            .iter()
            .map(|block| match block {
                kiqi_agent::types::TextContentOrImage::Text(text) => text.text.clone(),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// A tool-name collision is refused for an untrusted (wasm) extension, so an
    /// installed component cannot impersonate `bash` or a first-party tool.
    #[tokio::test]
    async fn an_untrusted_extension_cannot_take_over_a_tool_name() {
        let hosts = vec![
            std::sync::Arc::new(ExtensionHost::new(
                "trusted",
                Box::new(FixedTool {
                    id: "trusted",
                    tool: "shadow",
                }),
            )),
            std::sync::Arc::new(ExtensionHost::untrusted(
                "third-party",
                Box::new(FixedTool {
                    id: "third-party",
                    tool: "shadow",
                }),
            )),
        ];
        let mut tools = Vec::new();
        merge_compiled_extension_tools(&mut tools, &hosts, &empty_caps(), &|_| true);
        assert_eq!(tools.len(), 1, "the untrusted collision must be dropped");
        assert_eq!(
            tool_text(&tools[0]).await,
            "trusted",
            "the trusted tool must survive, not the third-party one"
        );
    }

    /// Refusing collisions must not turn into refusing untrusted extensions: a
    /// genuinely new name is still contributed.
    #[tokio::test]
    async fn an_untrusted_extension_may_still_add_a_new_tool() {
        let hosts = vec![std::sync::Arc::new(ExtensionHost::untrusted(
            "third-party",
            Box::new(FixedTool {
                id: "third-party",
                tool: "extra",
            }),
        ))];
        let mut tools = Vec::new();
        merge_compiled_extension_tools(&mut tools, &hosts, &empty_caps(), &|_| true);
        assert_eq!(tools.len(), 1);
        assert_eq!(tool_text(&tools[0]).await, "third-party");
    }

    /// The `session-read` capability answers the session id the host serves,
    /// rather than falling back to the trait's `None`.
    #[test]
    fn session_read_reports_the_session_id() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let caps = ActionCaps::new(
            std::sync::Arc::new(FakeHost::default()),
            runtime.handle().clone(),
        );
        assert_eq!(caps.session_id().as_deref(), Some("sess-test"));
    }

    /// `/reload` must not cost a compiled extension its UI capabilities: the caps
    /// the rebuilt adapters get carry the very mailbox the TUI is attached to.
    #[tokio::test]
    async fn reload_caps_carry_the_session_ui_mailboxes() {
        use kiqi_host::UiApi;
        let ui_dialog = kiqi_host::UiDialogMailbox::new();
        ui_dialog.attach();
        let caps = reload_caps(
            std::sync::Arc::new(FakeHost::default()),
            ui_dialog.clone(),
            kiqi_host::ExtensionStatusMailbox::new(),
            kiqi_host::EditorTextMailbox::new(),
        );
        let borrowed = caps.caps();
        let ui = borrowed.ui.expect("the ui capability survives a reload");
        // The mailbox refuses to enqueue while nothing is attached, so this also
        // pins that the same (attached) mailbox was threaded through.
        let opened = ui.dialog(&serde_json::json!({
            "op": "open",
            "requestId": "r1",
            "title": "reload",
        }));
        assert!(opened.is_ok(), "the dialog opened: {opened:?}");
        assert!(
            ui_dialog.take_pending().is_some(),
            "the extension's dialog reached the session mailbox"
        );
    }
    /// Build a tiny package fixture on disk: `extension.wasm` (the resolver does
    /// not parse it), `resources.json`, and the given resource files.
    fn write_package_fixture(package: &std::path::Path, manifest: &str, files: &[(&str, &str)]) {
        std::fs::create_dir_all(package.join(PACKAGE_RESOURCES_DIR)).unwrap();
        std::fs::write(package.join("extension.wasm"), b"\\0asm").unwrap();
        std::fs::write(package.join(PACKAGE_RESOURCES_MANIFEST), manifest).unwrap();
        for (relative, body) in files {
            let path = package.join(PACKAGE_RESOURCES_DIR).join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
    }

    /// The resource package round trip: the install writer records digests, the
    /// resolver verifies them, and the loaders see extension dirs LAST so a
    /// built-in resource wins a name collision.
    #[tokio::test]
    async fn package_resources_round_trip_install_resolve_discover() {
        let agent = tempfile::tempdir().unwrap();
        let package = agent.path().join("extensions/demo");

        let source = tempfile::tempdir().unwrap();
        write_package_fixture(
            source.path(),
            r#"{"kinds":["skills","prompts"],"max-items":8,"max-bytes":4096}"#,
            &[
                (
                    "skills/demo/SKILL.md",
                    "---\nname: demo-skill\ndescription: extension copy\n---\nextension body\n",
                ),
                ("prompts/demo.md", "extension prompt\n"),
            ],
        );

        // The install half: the same two calls `ki insmod` makes.
        let prepared = prepare_package_resources(source.path())
            .unwrap()
            .expect("the fixture ships resources.json");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("extension.wasm"), b"\\0asm").unwrap();
        write_package_resources(&package, &prepared).unwrap();
        assert!(package.join(PACKAGE_RESOURCES_DIGESTS).is_file());
        assert_eq!(
            std::fs::read_to_string(package.join(PACKAGE_RESOURCES_DIGESTS))
                .unwrap()
                .lines()
                .count(),
            3,
            "resources.json plus two resource files"
        );

        // The grant half: install records the manifest's policy.
        let mut grants = Grants::default();
        grants.grant_resources(prepared.policy().clone()).unwrap();
        Grants::save_for_at(agent.path(), "demo", &grants).unwrap();

        // The resolver half.
        let dirs = granted_package_resource_dirs(agent.path());
        assert!(dirs.diagnostics.is_empty(), "{:?}", dirs.diagnostics);
        assert_eq!(dirs.skills, vec![package.join("resources/skills")]);
        assert_eq!(dirs.prompts, vec![package.join("resources/prompts")]);

        // Discovery order: built-in first, extension last → first-wins keeps the
        // built-in, and the collision names the extension copy as the loser.
        let builtin = source.path().join("builtin-skills");
        std::fs::create_dir_all(builtin.join("demo-skill")).unwrap();
        std::fs::write(
            builtin.join("demo-skill/SKILL.md"),
            "---\nname: demo-skill\ndescription: built-in\n---\nbuilt-in body\n",
        )
        .unwrap();
        let env: std::sync::Arc<dyn kiqi_tools::ExecutionEnv> = std::sync::Arc::new(
            kiqi_tools::OsExecutionEnv::with_cwd(source.path().to_path_buf()),
        );
        let result = crate::resource_dirs::load_skills_with_precedence(
            &env,
            &[builtin.clone(), dirs.skills[0].clone()],
        )
        .await;
        assert_eq!(result.skills.len(), 1, "{:?}", result.diagnostics);
        assert!(
            result.skills[0].file_path.contains("builtin-skills"),
            "the built-in must win: {}",
            result.skills[0].file_path
        );
        assert!(
            result
                .resource_diagnostics
                .iter()
                .any(|diagnostic| matches!(
                    diagnostic,
                    kiqi_harness::diagnostics::ResourceDiagnostic::Collision(collision)
                        if collision.loser_path.contains("extensions/demo")
                )),
            "a collision must name the extension copy as the loser: {:?}",
            result.resource_diagnostics
        );
    }

    /// A bare `{}` manifest means both kinds with the default caps, exactly as
    /// the grant's bare `true` does.
    #[test]
    fn package_resources_manifest_defaults_to_both_kinds() {
        let package = tempfile::tempdir().unwrap();
        write_package_fixture(package.path(), "{}", &[("skills/a/SKILL.md", "x")]);
        let policy = load_package_resources_manifest(package.path())
            .unwrap()
            .expect("manifest");
        assert_eq!(
            policy.kinds,
            vec![
                RESOURCES_KIND_SKILLS.to_string(),
                RESOURCES_KIND_PROMPTS.to_string()
            ]
        );
        assert_eq!(
            policy.max_items,
            kiqi_host::grants::RESOURCES_MAX_ITEMS_DEFAULT
        );
        assert_eq!(
            policy.max_bytes,
            kiqi_host::grants::RESOURCES_MAX_BYTES_DEFAULT
        );
    }

    /// Unknown kinds are an install-time error, never a silently narrowed set.
    #[test]
    fn package_resources_reject_unknown_kinds() {
        let package = tempfile::tempdir().unwrap();
        write_package_fixture(package.path(), r#"{"kinds":["themes"]}"#, &[]);
        let error = prepare_package_resources(package.path()).unwrap_err();
        assert!(error.contains("themes"), "{error}");
    }

    /// A symlink anywhere in the tree is refused, both at install and at
    /// resolution.
    #[cfg(unix)]
    #[test]
    fn package_resources_reject_symlinks() {
        let package = tempfile::tempdir().unwrap();
        write_package_fixture(package.path(), r#"{"kinds":["skills"]}"#, &[]);
        std::fs::create_dir_all(package.path().join("resources/skills")).unwrap();
        let outside = package.path().join("outside.md");
        std::fs::write(&outside, "outside\n").unwrap();
        std::os::unix::fs::symlink(&outside, package.path().join("resources/skills/link.md"))
            .unwrap();

        let install_error = prepare_package_resources(package.path()).unwrap_err();
        assert!(install_error.contains("symlink"), "{install_error}");

        let resolve_error =
            resolve_package_resources(package.path(), &ResourcesGrant::default()).unwrap_err();
        assert!(resolve_error.contains("symlink"), "{resolve_error}");
    }

    /// The item/byte cap is enforced while walking, from metadata, before the
    /// file is read. The fixture file is over budget *and* unreadable: a walk
    /// that read first would fail with EACCES, so the cap error proves the
    /// budget was checked first. This is what keeps a multi-GiB resource from
    /// OOMing the host at install and on every session start.
    #[cfg(unix)]
    #[test]
    fn package_resources_over_budget_is_rejected_before_reading() {
        use std::os::unix::fs::PermissionsExt;

        let package = tempfile::tempdir().unwrap();
        write_package_fixture(
            package.path(),
            r#"{"kinds":["skills"],"max-items":8,"max-bytes":8}"#,
            &[("skills/a.md", "0123456789")],
        );
        let file = package
            .path()
            .join(PACKAGE_RESOURCES_DIR)
            .join("skills/a.md");
        let mut perms = std::fs::metadata(&file).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&file, perms).unwrap();

        let install_error = prepare_package_resources(package.path()).unwrap_err();
        assert!(
            install_error.contains("max-bytes"),
            "the install walk must report the cap, not fail reading: {install_error}"
        );

        let resolve_error =
            resolve_package_resources(package.path(), &ResourcesGrant::default()).unwrap_err();
        assert!(
            resolve_error.contains("max-bytes"),
            "the resolve walk must report the cap, not fail reading: {resolve_error}"
        );
    }

    /// A `..` entry in the digest manifest cannot reach outside the package.
    #[test]
    fn package_resources_reject_parent_dir_entries() {
        let package = tempfile::tempdir().unwrap();
        write_package_fixture(
            package.path(),
            r#"{"kinds":["skills"]}"#,
            &[("skills/a.md", "a")],
        );
        let manifest =
            std::fs::read_to_string(package.path().join(PACKAGE_RESOURCES_MANIFEST)).unwrap();
        let skill = "a";
        let digest = crate::extension_cli::sha256_hex(skill.as_bytes());
        let manifest_digest = crate::extension_cli::sha256_hex(manifest.as_bytes());
        let mut digests = String::new();
        digests.push_str(&format!(
            "{manifest_digest}  {PACKAGE_RESOURCES_MANIFEST}\n"
        ));
        digests.push_str(&format!("{digest}  resources/skills/a.md\n"));
        digests.push_str(&format!("{digest}  ../escape.md\n"));
        std::fs::write(package.path().join(PACKAGE_RESOURCES_DIGESTS), digests).unwrap();

        let error =
            resolve_package_resources(package.path(), &ResourcesGrant::default()).unwrap_err();
        assert!(error.contains("escapes"), "{error}");
    }

    /// A tampered resource fails the recorded digest rather than loading.
    #[test]
    fn package_resources_reject_digest_mismatch() {
        let package = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        write_package_fixture(
            source.path(),
            r#"{"kinds":["skills"]}"#,
            &[(
                "skills/a/SKILL.md",
                "---\nname: a\ndescription: a\n---\nbody\n",
            )],
        );
        let prepared = prepare_package_resources(source.path()).unwrap().unwrap();
        std::fs::create_dir_all(package.path()).unwrap();
        std::fs::write(package.path().join("extension.wasm"), b"\\0asm").unwrap();
        write_package_resources(package.path(), &prepared).unwrap();
        std::fs::write(
            package.path().join("resources/skills/a/SKILL.md"),
            "tampered",
        )
        .unwrap();

        let error =
            resolve_package_resources(package.path(), &ResourcesGrant::default()).unwrap_err();
        assert!(error.contains("digest mismatch"), "{error}");
    }
}
