//! The [`kiqi_ai::Provider`] adapter for a granted wasm provider.
//!
//! A granted plugin provider is a real provider as far as the harness is
//! concerned: it has an id, a model catalog, and it streams
//! [`AssistantMessageEvent`]s. Two properties make it different from a built-in
//! provider, and both live here rather than in the harness:
//!
//! * **The grant is the only authority.** The catalog entry comes from
//!   `grants.json` (`crate::provider::granted_plugin_models`); this adapter
//!   re-reads the grant on first use and [`kiqi_plugin_host::ProviderHost`]
//!   refuses to instantiate the guest when no profile names this install id.
//! * **Nothing is built until a request arrives.** The grant and the component
//!   are read/compiled lazily on the first `stream_simple`, on a blocking
//!   thread, so resolving the catalog (including every `/model` reload) never
//!   pays wasmtime's compile cost for a provider the user did not select
//!   ([`ProviderHost`] owns its own `Engine`/`Store`).
//!
//! The provider call itself runs synchronously inside the host; the event
//! stream is produced by a tokio task that drains `provider-stream` deltas as
//! they arrive and assembles the same partial messages the built-in mappers
//! emit. Cancellation is a [`kiqi_plugin::CancelProbe`] backed by the harness's
//! `CancellationToken`, so aborting a run reaches the guest and
//! `provider-http` between chunks instead of merely dropping the consumer.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use kiqi_ai::event_stream::{
    create_assistant_message_event_stream, AssistantMessageEventStream,
    AssistantMessageEventStreamProducer,
};
use kiqi_ai::{
    Api, AssistantMessage, AssistantMessageEvent, Content, Context, DoneReason, ErrorReason, Model,
    Provider, SimpleStreamOptions, StopReason, ToolCall, Usage,
};

#[cfg(feature = "plugin-wasm")]
use std::collections::HashMap;

#[cfg(feature = "plugin-wasm")]
use kiqi_plugin::CancelProbe as _;

/// The runtime handle for one granted plugin provider.
pub struct WasmProvider {
    id: String,
    models: Vec<Model>,
    /// The per-resolve state the request path needs. `None` outside the
    /// `plugin-wasm` feature; such a build never surfaces a granted provider.
    #[cfg(feature = "plugin-wasm")]
    slot: Arc<ProviderSlot>,
}

#[cfg(feature = "plugin-wasm")]
struct ProviderSlot {
    id: String,
    /// `plugins/<id>/plugin.wasm`.
    wasm: PathBuf,
    /// The agent root the grant was read from, if the caller named one.
    agent_root: Option<PathBuf>,
    /// Built on the first request, never at resolve time. A trapped/dead host
    /// is replaced on the next request.
    host: std::sync::Mutex<Option<Arc<kiqi_plugin_host::ProviderHost>>>,
}

impl WasmProvider {
    /// Construct the adapter. Does not touch the filesystem or wasmtime.
    pub fn new(
        id: impl Into<String>,
        models: Vec<Model>,
        wasm: PathBuf,
        agent_root: Option<PathBuf>,
    ) -> Self {
        let id = id.into();
        #[cfg(feature = "plugin-wasm")]
        {
            let slot = Arc::new(ProviderSlot {
                id: id.clone(),
                wasm,
                agent_root,
                host: std::sync::Mutex::new(None),
            });
            Self { id, models, slot }
        }
        #[cfg(not(feature = "plugin-wasm"))]
        {
            let _ = (wasm, agent_root);
            Self { id, models }
        }
    }

    /// Whether this adapter has built its host. Test-only observability for
    /// the lazy-compile contract.
    #[cfg(all(test, feature = "plugin-wasm"))]
    pub(crate) fn host_loaded(&self) -> bool {
        self.slot
            .host
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }
}

/// Serializes tests that assert [`hosts_built`], because the counter is
/// process-wide and the tests run in parallel.
#[cfg(all(test, feature = "plugin-wasm"))]
pub(crate) static HOST_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(all(test, feature = "plugin-wasm"))]
static HOSTS_BUILT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Successful host constructions since [`reset_hosts_built`]. Test-only.
#[cfg(all(test, feature = "plugin-wasm"))]
pub(crate) fn hosts_built() -> usize {
    HOSTS_BUILT.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(all(test, feature = "plugin-wasm"))]
pub(crate) fn reset_hosts_built() {
    HOSTS_BUILT.store(0, std::sync::atomic::Ordering::SeqCst);
}

#[async_trait]
impl Provider for WasmProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn models(&self) -> &[Model] {
        &self.models
    }

    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let (mut producer, stream) = create_assistant_message_event_stream();
        let api = model.api.clone();
        let provider = self.id.clone();
        let model_id = model.id.clone();
        let timestamp = now_millis();
        // A provider always opens its stream, even when the very first thing
        // that happens is an error; the harness relies on the Start → terminal
        // shape.
        producer.push(AssistantMessageEvent::Start {
            partial: Arc::new(AssistantMessage::empty(
                api.clone(),
                provider.clone(),
                model_id.clone(),
                timestamp,
            )),
        });

        #[cfg(not(feature = "plugin-wasm"))]
        {
            push_error(
                &mut producer,
                &api,
                &provider,
                &model_id,
                ErrorReason::Error,
                "This build has no wasm plugin tier (`plugin-wasm` is disabled), so the granted \
                 provider could not run.",
                timestamp,
            );
            return stream;
        }

        #[cfg(feature = "plugin-wasm")]
        {
            let request = match build_request(model, ctx, opts) {
                Ok(request) => request,
                Err(error) => {
                    push_error(
                        &mut producer,
                        &api,
                        &provider,
                        &model_id,
                        ErrorReason::Error,
                        error,
                        timestamp,
                    );
                    return stream;
                }
            };
            let slot = Arc::clone(&self.slot);
            let local = tokio_util::sync::CancellationToken::new();
            let probe = Arc::new(SignalProbe {
                caller: opts.signal.clone(),
                local: local.clone(),
            });
            tokio::spawn(async move {
                run_request(
                    &mut producer,
                    slot,
                    request,
                    probe,
                    local,
                    api,
                    provider,
                    model_id,
                    timestamp,
                )
                .await;
            });
            stream
        }
    }
}

#[cfg(feature = "plugin-wasm")]
struct SignalProbe {
    caller: tokio_util::sync::CancellationToken,
    /// Set locally when the consumer drops the stream, so a guest that polls
    /// `proc.cancel-requested` winds down instead of running to its deadline.
    local: tokio_util::sync::CancellationToken,
}

#[cfg(feature = "plugin-wasm")]
impl kiqi_plugin::CancelProbe for SignalProbe {
    fn is_cancelled(&self) -> bool {
        self.caller.is_cancelled() || self.local.is_cancelled()
    }
}

/// Build the request envelope the guest receives. The model identity and the
/// endpoint come from the grant-backed catalog entry; no credential or auth
/// header is ever included (`provider-http` injects it host-side).
fn build_request(
    model: &Model,
    ctx: &Context,
    opts: &SimpleStreamOptions,
) -> Result<String, String> {
    let value = serde_json::json!({
        "model": model.id,
        "api": model.api.as_str(),
        "baseUrl": model.base_url,
        "systemPrompt": ctx.system_prompt,
        "messages": ctx.messages,
        "tools": ctx.tools,
        "maxTokens": opts.max_tokens.unwrap_or(model.max_tokens),
        "temperature": opts.temperature,
        "reasoning": opts.reasoning,
    });
    serde_json::to_string(&value).map_err(|error| format!("build provider request: {error}"))
}

#[cfg(feature = "plugin-wasm")]
#[allow(clippy::too_many_arguments)]
async fn run_request(
    producer: &mut AssistantMessageEventStreamProducer,
    slot: Arc<ProviderSlot>,
    request: String,
    probe: Arc<SignalProbe>,
    local: tokio_util::sync::CancellationToken,
    api: Api,
    provider: String,
    model_id: String,
    timestamp: i64,
) {
    use kiqi_plugin_host::ProviderDeltaEvent;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ProviderDeltaEvent>();
    let request_probe: Arc<dyn kiqi_plugin::CancelProbe> = probe.clone();
    let blocking = tokio::task::spawn_blocking(move || {
        let host = ensure_host(&slot)?;
        host.request_streaming(
            &request,
            Some(request_probe),
            Box::new(move |event| {
                // Unbounded: the guest thread must never block on rendering.
                let _ = tx.send(event);
            }),
        )
    });

    let mut builder =
        PartialBuilder::new(api.clone(), provider.clone(), model_id.clone(), timestamp);
    let mut assembly_error: Option<String> = None;
    while let Some(event) = rx.recv().await {
        if assembly_error.is_some() {
            continue;
        }
        match builder.apply(event, producer) {
            Ok(true) => {}
            Ok(false) => {
                // The consumer is gone; ask the guest to stop.
                local.cancel();
            }
            Err(error) => {
                assembly_error = Some(error);
                local.cancel();
            }
        }
    }

    match blocking.await {
        Ok(Ok(response)) => {
            if let Some(error) = assembly_error {
                let reason = if probe.is_cancelled() {
                    ErrorReason::Aborted
                } else {
                    ErrorReason::Error
                };
                push_error(
                    producer, &api, &provider, &model_id, reason, error, timestamp,
                );
                return;
            }
            match parse_terminal(&response.message, &api, &provider, &model_id, timestamp) {
                Ok(message) => match done_reason(message.stop_reason) {
                    Ok(reason) => {
                        producer.push(AssistantMessageEvent::Done { reason, message });
                    }
                    Err(reason) => {
                        producer.push(AssistantMessageEvent::Error {
                            reason,
                            error: message,
                        });
                    }
                },
                Err(error) => push_error(
                    producer,
                    &api,
                    &provider,
                    &model_id,
                    ErrorReason::Error,
                    error,
                    timestamp,
                ),
            }
        }
        Ok(Err(error)) => {
            let reason = if probe.is_cancelled() {
                ErrorReason::Aborted
            } else {
                ErrorReason::Error
            };
            push_error(
                producer, &api, &provider, &model_id, reason, error, timestamp,
            );
        }
        Err(join_error) => push_error(
            producer,
            &api,
            &provider,
            &model_id,
            ErrorReason::Error,
            format!("provider task failed: {join_error}"),
            timestamp,
        ),
    }
}

/// Create (or reuse) the dedicated [`kiqi_plugin_host::ProviderHost`].
///
/// Runs on the blocking thread that owns the request. `Ok(None)` from
/// `ProviderHost::load_with_options` — no provider grant (or no profile naming
/// this install id) — becomes an error: deny-by-default means the guest is
/// never instantiated.
#[cfg(feature = "plugin-wasm")]
fn ensure_host(slot: &ProviderSlot) -> Result<Arc<kiqi_plugin_host::ProviderHost>, String> {
    use kiqi_plugin_host::{ProviderHost, ProviderHostOptions};

    let mut guard = slot
        .host
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Re-read and validate the grant before *every* cache decision: a revoked
    // or narrowed profile must stop the very next request, even though the
    // adapter and its credential outlive a `/model` reload.
    let grants = match &slot.agent_root {
        Some(root) => kiqi_plugin::grants::Grants::load_for_at(root, &slot.id),
        None => kiqi_plugin::grants::Grants::load_for(&slot.id),
    };
    let current_profile = grants
        .provider
        .as_ref()
        .and_then(|grant| grant.providers.iter().find(|profile| profile.id == slot.id));
    if let Some(host) = guard.as_ref() {
        if !host.is_disabled()
            && current_profile.is_some_and(|profile| host.has_exact_profile(profile))
        {
            return Ok(Arc::clone(host));
        }
    }
    // Never leave a stale instance eligible for a later request. A failed
    // rebuild fails closed and is retried from the current grant next time.
    *guard = None;
    let host = ProviderHost::load_with_options(
        slot.id.clone(),
        &slot.wasm,
        &grants,
        ProviderHostOptions {
            agent_root: slot.agent_root.clone(),
            ..ProviderHostOptions::default()
        },
    )
    .map_err(|error| format!("{}: {error}", slot.id))?
    .ok_or_else(|| {
        format!(
            "Plugin provider `{}` has no `provider` grant, so the request was not sent. \
             Re-run `ki plugin grant {} provider …`.",
            slot.id, slot.id
        )
    })?;
    let host = Arc::new(host);
    *guard = Some(Arc::clone(&host));
    #[cfg(test)]
    HOSTS_BUILT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Ok(host)
}

/// Assemble the partial `AssistantMessage` the live events describe.
///
/// Guest `content-index` values are global to the call; the event protocol
/// wants the block's position in `message.content`, which is the order starts
/// arrive (the same rule the built-in mappers use).
#[cfg(feature = "plugin-wasm")]
struct PartialBuilder {
    message: AssistantMessage,
    /// Guest content index → position in `message.content`.
    positions: HashMap<u32, usize>,
    /// Tool-call text accumulates here until its `end` makes it a `ToolCall`.
    tool_text: HashMap<usize, String>,
}

#[cfg(feature = "plugin-wasm")]
impl PartialBuilder {
    fn new(api: Api, provider: String, model: String, timestamp: i64) -> Self {
        Self {
            message: AssistantMessage::empty(api, provider, model, timestamp),
            positions: HashMap::new(),
            tool_text: HashMap::new(),
        }
    }

    /// Apply one delta. `Ok(false)` means the consumer dropped the stream.
    fn apply(
        &mut self,
        event: kiqi_plugin_host::ProviderDeltaEvent,
        producer: &mut AssistantMessageEventStreamProducer,
    ) -> Result<bool, String> {
        use kiqi_plugin_host::{DeltaKind, DeltaPhase};

        let index = event.content_index;
        match event.phase {
            DeltaPhase::Start => {
                if self.positions.contains_key(&index) {
                    return Err(format!("provider stream: duplicate block index {index}"));
                }
                let start_text = event.text.clone();
                let content = match event.kind {
                    DeltaKind::Text => Content::text(event.text),
                    DeltaKind::Thinking => Content::thinking(event.text),
                    DeltaKind::ToolCall => {
                        Content::tool_call(String::new(), String::new(), serde_json::json!({}))
                    }
                };
                self.message.content.push(content);
                let position = self.message.content.len() - 1;
                self.positions.insert(index, position);
                // A tool call's text is the complete `{id,name,arguments}`
                // object by its end; seed the buffer with anything the start
                // carried so a guest that opens with the identity still lands.
                if event.kind == DeltaKind::ToolCall && !start_text.is_empty() {
                    self.tool_text.insert(position, start_text.clone());
                    if let Some(tool_call) = partial_tool_call(&start_text) {
                        self.message.content[position] = Content::ToolCall(tool_call);
                    }
                }
                let partial = Arc::new(self.message.clone());
                let event = match event.kind {
                    DeltaKind::Text => AssistantMessageEvent::TextStart {
                        content_index: position,
                        partial,
                    },
                    DeltaKind::Thinking => AssistantMessageEvent::ThinkingStart {
                        content_index: position,
                        partial,
                    },
                    DeltaKind::ToolCall => AssistantMessageEvent::ToolCallStart {
                        content_index: position,
                        partial,
                    },
                };
                Ok(producer.push(event))
            }
            DeltaPhase::Delta => {
                let position = *self.positions.get(&index).ok_or_else(|| {
                    format!("provider stream: delta before start for block {index}")
                })?;
                self.append(position, event.kind, &event.text);
                let partial = Arc::new(self.message.clone());
                let event = match event.kind {
                    DeltaKind::Text => AssistantMessageEvent::TextDelta {
                        content_index: position,
                        delta: event.text,
                        partial,
                    },
                    DeltaKind::Thinking => AssistantMessageEvent::ThinkingDelta {
                        content_index: position,
                        delta: event.text,
                        partial,
                    },
                    DeltaKind::ToolCall => AssistantMessageEvent::ToolCallDelta {
                        content_index: position,
                        delta: event.text,
                        partial,
                    },
                };
                Ok(producer.push(event))
            }
            DeltaPhase::End => {
                let position = *self.positions.get(&index).ok_or_else(|| {
                    format!("provider stream: end before start for block {index}")
                })?;
                self.append(position, event.kind, &event.text);
                let partial = Arc::new(self.message.clone());
                let event = match event.kind {
                    DeltaKind::Text => AssistantMessageEvent::TextEnd {
                        content_index: position,
                        content: self.text_at(position),
                        partial,
                    },
                    DeltaKind::Thinking => AssistantMessageEvent::ThinkingEnd {
                        content_index: position,
                        content: self.thinking_at(position),
                        partial,
                    },
                    DeltaKind::ToolCall => {
                        let raw = self.tool_text.get(&position).cloned().unwrap_or_default();
                        let tool_call = parse_tool_call(&raw)?;
                        self.message.content[position] = Content::ToolCall(tool_call.clone());
                        AssistantMessageEvent::ToolCallEnd {
                            content_index: position,
                            tool_call,
                            partial: Arc::new(self.message.clone()),
                        }
                    }
                };
                Ok(producer.push(event))
            }
        }
    }

    fn append(&mut self, position: usize, kind: kiqi_plugin_host::DeltaKind, text: &str) {
        if text.is_empty() {
            return;
        }
        match kind {
            kiqi_plugin_host::DeltaKind::Text => {
                if let Some(Content::Text(slot)) = self.message.content.get_mut(position) {
                    slot.text.push_str(text);
                }
            }
            kiqi_plugin_host::DeltaKind::Thinking => {
                if let Some(Content::Thinking(slot)) = self.message.content.get_mut(position) {
                    slot.thinking.push_str(text);
                }
            }
            kiqi_plugin_host::DeltaKind::ToolCall => {
                let buffer = self.tool_text.entry(position).or_default();
                buffer.push_str(text);
                // Keep the partial renderable while the object is still
                // incomplete: parse whatever identity fields exist so far.
                if let Some(tool_call) = partial_tool_call(buffer) {
                    if let Some(slot) = self.message.content.get_mut(position) {
                        *slot = Content::ToolCall(tool_call);
                    }
                }
            }
        }
    }

    fn text_at(&self, position: usize) -> String {
        match self.message.content.get(position) {
            Some(Content::Text(slot)) => slot.text.clone(),
            _ => String::new(),
        }
    }

    fn thinking_at(&self, position: usize) -> String {
        match self.message.content.get(position) {
            Some(Content::Thinking(slot)) => slot.thinking.clone(),
            _ => String::new(),
        }
    }
}

/// Parse a tool call from the guest's complete `{id,name,arguments}` object.
/// `arguments` is normally a JSON string (an SSE fragment); it is lifted into a
/// JSON value when it parses, so the harness sees the same shape as a native
/// provider.
#[cfg(feature = "plugin-wasm")]
fn parse_tool_call(raw: &str) -> Result<ToolCall, String> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|error| format!("provider stream: tool call is not JSON: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "provider stream: tool call must be a JSON object".to_string())?;
    let id = object
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "provider stream: tool call is missing `id`".to_string())?;
    let name = object
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "provider stream: tool call is missing `name`".to_string())?;
    let arguments = match object.get("arguments") {
        Some(serde_json::Value::String(raw)) => {
            serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.clone()))
        }
        Some(value) => value.clone(),
        None => serde_json::json!({}),
    };
    Ok(ToolCall {
        kind: kiqi_ai::ToolCallType,
        id: id.to_string(),
        name: name.to_string(),
        arguments,
        thought_signature: None,
        namespace: None,
    })
}

/// Best-effort identity extraction from a tool-call object that may still be
/// arriving. Returns `None` until `id` and `name` are both readable.
#[cfg(feature = "plugin-wasm")]
fn partial_tool_call(raw: &str) -> Option<ToolCall> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let object = value.as_object()?;
    let id = object.get("id").and_then(serde_json::Value::as_str)?;
    let name = object.get("name").and_then(serde_json::Value::as_str)?;
    let arguments = match object.get("arguments") {
        Some(serde_json::Value::String(raw)) => {
            serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.clone()))
        }
        Some(value) => value.clone(),
        None => serde_json::json!({}),
    };
    Some(ToolCall {
        kind: kiqi_ai::ToolCallType,
        id: id.to_string(),
        name: name.to_string(),
        arguments,
        thought_signature: None,
        namespace: None,
    })
}

/// Validate the guest's final assistant message and fill the fields a
/// provider-side fixture (or a terse guest) may omit. The host has already
/// overwritten `provider`/`model`/`api` from the grant.
#[cfg(feature = "plugin-wasm")]
fn parse_terminal(
    raw: &str,
    api: &Api,
    provider: &str,
    model: &str,
    timestamp: i64,
) -> Result<AssistantMessage, String> {
    let mut value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|error| format!("provider reply is not JSON: {error}"))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| "provider reply must be a JSON object".to_string())?;
    object
        .entry("role")
        .or_insert_with(|| serde_json::Value::String("assistant".to_string()));
    object
        .entry("content")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    object
        .entry("api")
        .or_insert_with(|| serde_json::Value::String(api.as_str().to_string()));
    object.insert(
        "provider".to_string(),
        serde_json::Value::String(provider.to_string()),
    );
    object.insert(
        "model".to_string(),
        serde_json::Value::String(model.to_string()),
    );
    object
        .entry("usage")
        .or_insert_with(|| serde_json::to_value(Usage::zero()).unwrap_or(serde_json::json!({})));
    object
        .entry("stopReason")
        .or_insert_with(|| serde_json::Value::String("stop".to_string()));
    object
        .entry("timestamp")
        .or_insert_with(|| serde_json::Value::from(timestamp));
    serde_json::from_value(value).map_err(|error| format!("provider reply is invalid: {error}"))
}

fn done_reason(stop: StopReason) -> Result<DoneReason, ErrorReason> {
    match stop {
        StopReason::Pending | StopReason::Stop => Ok(DoneReason::Stop),
        StopReason::Length => Ok(DoneReason::Length),
        StopReason::ToolUse => Ok(DoneReason::ToolUse),
        StopReason::Deferred => Ok(DoneReason::Deferred),
        StopReason::Error => Err(ErrorReason::Error),
        StopReason::Aborted => Err(ErrorReason::Aborted),
    }
}

fn push_error(
    producer: &mut AssistantMessageEventStreamProducer,
    api: &Api,
    provider: &str,
    model: &str,
    reason: ErrorReason,
    message: impl Into<String>,
    timestamp: i64,
) {
    producer.push(AssistantMessageEvent::Error {
        reason,
        error: AssistantMessage::terminal(
            api.clone(),
            provider.to_string(),
            model.to_string(),
            reason.into(),
            message,
            timestamp,
        ),
    });
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(all(test, feature = "plugin-wasm"))]
mod tests {
    use super::*;
    use kiqi_ai::SimpleStreamOptions;
    use kiqi_plugin::grants::{
        Grants, ProviderEndpoint, ProviderGrant, ProviderModel, ProviderProfile,
        PROVIDER_MAX_HANDLES_DEFAULT, PROVIDER_MAX_RESPONSE_BYTES_DEFAULT,
    };
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// Build the conformance guest and install it under `root/plugins/conformance`
    /// with a provider grant covering `models`.
    fn install_conformance_provider(root: &Path, models: &[&str]) -> PathBuf {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let built =
            crate::plugin_cli::build_guest(&workspace.join("plugins/wasm/kiqi-conformance-guest"))
                .expect("build the conformance guest");
        let dir = root.join("plugins/conformance");
        std::fs::create_dir_all(&dir).unwrap();
        let wasm = dir.join("plugin.wasm");
        std::fs::copy(&built, &wasm).expect("install plugin.wasm");

        let grant = ProviderGrant {
            providers: vec![ProviderProfile {
                id: "conformance".to_string(),
                display_name: "Conformance Provider".to_string(),
                models: models
                    .iter()
                    .map(|id| ProviderModel {
                        id: (*id).to_string(),
                        api: "openai-completions".to_string(),
                        base_url: "https://api.example.test/v1".to_string(),
                        reasoning: true,
                        input: vec!["text".to_string()],
                    })
                    .collect(),
                endpoint: ProviderEndpoint {
                    scheme: "https".to_string(),
                    host: "api.example.test".to_string(),
                    path_prefix: "/v1".to_string(),
                    methods: vec!["POST".to_string()],
                },
                credential: None,
                auth: None,
                timeout_seconds: 30,
                max_response_bytes: PROVIDER_MAX_RESPONSE_BYTES_DEFAULT,
                max_handles: PROVIDER_MAX_HANDLES_DEFAULT,
            }],
        };
        let mut grants = Grants::default();
        grants.grant_provider(grant).expect("valid provider grant");
        Grants::save_for_at(root, "conformance", &grants).unwrap();
        wasm
    }

    fn conformance_model(id: &str) -> Model {
        Model::new(
            id,
            "Conformance Model",
            Api::OpenaiCompletions,
            "conformance",
            "https://api.example.test/v1",
        )
    }

    async fn drain(stream: &mut AssistantMessageEventStream) -> Vec<AssistantMessageEvent> {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn provider_stream_deltas_assemble_a_correct_message() {
        let _lock = HOST_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempfile::tempdir().unwrap();
        let wasm = install_conformance_provider(root.path(), &["conformance-stream"]);
        let provider = WasmProvider::new(
            "conformance",
            vec![conformance_model("conformance-stream")],
            wasm,
            Some(root.path().to_path_buf()),
        );
        assert!(
            !provider.host_loaded(),
            "nothing is compiled before a request"
        );

        let model = conformance_model("conformance-stream");
        let mut stream = provider
            .stream_simple(&model, &Context::default(), &SimpleStreamOptions::default())
            .await;
        let events = drain(&mut stream).await;
        assert!(provider.host_loaded());

        let tags: Vec<&str> = events.iter().map(|event| event.type_tag()).collect();
        assert_eq!(
            tags,
            vec![
                "start",
                "text_start",
                "text_delta",
                "text_delta",
                "text_end",
                "thinking_start",
                "thinking_end",
                "toolcall_start",
                "toolcall_end",
                "done",
            ]
        );

        match &events[1] {
            AssistantMessageEvent::TextStart {
                content_index,
                partial,
            } => {
                assert_eq!(*content_index, 0);
                assert!(matches!(&partial.content[0], Content::Text(text) if text.text.is_empty()));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        match &events[2] {
            AssistantMessageEvent::TextDelta {
                content_index,
                delta,
                partial,
            } => {
                assert_eq!(*content_index, 0);
                assert_eq!(delta, "Hello");
                assert!(matches!(&partial.content[0], Content::Text(text) if text.text == "Hello"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        // The partial carries the accumulated text, not just the delta.
        match &events[3] {
            AssistantMessageEvent::TextDelta { delta, partial, .. } => {
                assert_eq!(delta, " world");
                assert!(
                    matches!(&partial.content[0], Content::Text(text) if text.text == "Hello world")
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
        match &events[4] {
            AssistantMessageEvent::TextEnd {
                content_index,
                content,
                ..
            } => {
                assert_eq!(*content_index, 0);
                assert_eq!(content, "Hello world");
            }
            other => panic!("unexpected event: {other:?}"),
        }
        match &events[5] {
            AssistantMessageEvent::ThinkingStart { content_index, .. } => {
                assert_eq!(*content_index, 1)
            }
            other => panic!("unexpected event: {other:?}"),
        }
        match &events[6] {
            AssistantMessageEvent::ThinkingEnd {
                content_index,
                content,
                ..
            } => {
                assert_eq!(*content_index, 1);
                assert_eq!(content, "reasoned");
            }
            other => panic!("unexpected event: {other:?}"),
        }
        match &events[7] {
            AssistantMessageEvent::ToolCallStart { content_index, .. } => {
                assert_eq!(*content_index, 2)
            }
            other => panic!("unexpected event: {other:?}"),
        }
        match &events[8] {
            AssistantMessageEvent::ToolCallEnd {
                content_index,
                tool_call,
                partial,
            } => {
                assert_eq!(*content_index, 2);
                assert_eq!(tool_call.id, "call-1");
                assert_eq!(tool_call.name, "conformance");
                assert_eq!(tool_call.arguments, serde_json::json!({}));
                assert!(
                    matches!(&partial.content[2], Content::ToolCall(call) if call.id == "call-1")
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
        match &events[9] {
            AssistantMessageEvent::Done { reason, message } => {
                assert_eq!(*reason, DoneReason::ToolUse);
                assert_eq!(message.provider, "conformance");
                assert_eq!(message.model, "conformance-stream");
                assert_eq!(message.content.len(), 3);
                assert!(
                    matches!(&message.content[0], Content::Text(text) if text.text == "Hello world")
                );
                assert!(
                    matches!(&message.content[1], Content::Thinking(thinking) if thinking.thinking == "reasoned")
                );
                assert!(
                    matches!(&message.content[2], Content::ToolCall(call) if call.name == "conformance")
                );
            }
            other => panic!("unexpected terminal: {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancelling_the_signal_stops_the_provider_call() {
        let _lock = HOST_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempfile::tempdir().unwrap();
        let wasm = install_conformance_provider(root.path(), &["conformance-slow"]);
        let provider = WasmProvider::new(
            "conformance",
            vec![conformance_model("conformance-slow")],
            wasm,
            Some(root.path().to_path_buf()),
        );
        let model = conformance_model("conformance-slow");
        let opts = SimpleStreamOptions::default();
        let signal = opts.signal.clone();
        let started = Instant::now();
        let mut stream = provider
            .stream_simple(&model, &Context::default(), &opts)
            .await;

        let mut cancelled = false;
        let mut terminal = None;
        while let Some(event) = stream.next().await {
            match &event {
                AssistantMessageEvent::TextDelta { .. } if !cancelled => {
                    signal.cancel();
                    cancelled = true;
                }
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {
                    terminal = Some(event);
                    break;
                }
                _ => {}
            }
        }
        assert!(cancelled, "the fixture must have started streaming");
        match terminal.expect("a terminal event") {
            AssistantMessageEvent::Error { reason, error } => {
                assert_eq!(reason, ErrorReason::Aborted);
                assert!(
                    error
                        .error_message
                        .as_deref()
                        .unwrap_or_default()
                        .contains("cancelled by host"),
                    "{error:?}"
                );
            }
            other => panic!("cancellation must end in an Aborted error: {other:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "cancellation must stop the call, not wait for the grant deadline: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn no_grant_means_the_guest_is_never_instantiated() {
        let _lock = HOST_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempfile::tempdir().unwrap();
        // plugin.wasm is present but there is no grants.json at all.
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let built =
            crate::plugin_cli::build_guest(&workspace.join("plugins/wasm/kiqi-conformance-guest"))
                .expect("build the conformance guest");
        let dir = root.path().join("plugins/conformance");
        std::fs::create_dir_all(&dir).unwrap();
        let wasm = dir.join("plugin.wasm");
        std::fs::copy(&built, &wasm).unwrap();

        let provider = WasmProvider::new(
            "conformance",
            vec![conformance_model("conformance-stream")],
            wasm,
            Some(root.path().to_path_buf()),
        );
        let model = conformance_model("conformance-stream");
        let mut stream = provider
            .stream_simple(&model, &Context::default(), &SimpleStreamOptions::default())
            .await;
        let events = drain(&mut stream).await;
        match events.last() {
            Some(AssistantMessageEvent::Error { reason, error }) => {
                assert_eq!(*reason, ErrorReason::Error);
                assert!(
                    error
                        .error_message
                        .as_deref()
                        .unwrap_or_default()
                        .contains("no `provider` grant"),
                    "{error:?}"
                );
            }
            other => panic!("expected the fail-closed error, got {other:?}"),
        }
        assert!(!provider.host_loaded());
    }

    #[tokio::test]
    async fn revoking_a_cached_provider_grant_stops_the_next_request() {
        let _lock = HOST_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempfile::tempdir().unwrap();
        let wasm = install_conformance_provider(root.path(), &["conformance-stream"]);

        // Leave a valid, resolvable credential in place: revocation, rather
        // than credential loss, must be what stops the second request.
        std::fs::write(
            root.path().join("auth.json"),
            r#"{"anthropic":{"type":"api_key","key":"sk-revocation-secret"}}"#,
        )
        .unwrap();
        std::fs::write(
            root.path().join("plugins/conformance/credentials.json"),
            serde_json::json!({
                "pc-revocation": {
                    "source": "auth-json:anthropic",
                    "provider": "anthropic",
                    "display": "Anthropic",
                    "fingerprint": kiqi_plugin_host::fingerprint_credential(
                        "sk-revocation-secret"
                    ),
                }
            })
            .to_string(),
        )
        .unwrap();
        let mut grants = Grants::load_for_at(root.path(), "conformance");
        grants.provider.as_mut().unwrap().providers[0].credential =
            Some("pc-revocation".to_string());
        Grants::save_for_at(root.path(), "conformance", &grants).unwrap();

        let provider = WasmProvider::new(
            "conformance",
            vec![conformance_model("conformance-stream")],
            wasm,
            Some(root.path().to_path_buf()),
        );
        let model = conformance_model("conformance-stream");
        let mut first = provider
            .stream_simple(&model, &Context::default(), &SimpleStreamOptions::default())
            .await;
        let first = drain(&mut first).await;
        assert!(matches!(
            first.last(),
            Some(AssistantMessageEvent::Done { .. })
        ));
        assert!(provider.host_loaded());

        grants.provider = None;
        Grants::save_for_at(root.path(), "conformance", &grants).unwrap();
        let mut second = provider
            .stream_simple(&model, &Context::default(), &SimpleStreamOptions::default())
            .await;
        let second = drain(&mut second).await;
        assert!(
            !second
                .iter()
                .any(|event| matches!(event, AssistantMessageEvent::Done { .. })),
            "the revoked guest must not execute: {second:?}"
        );
        match second.last() {
            Some(AssistantMessageEvent::Error { error, .. }) => assert!(
                error
                    .error_message
                    .as_deref()
                    .unwrap_or_default()
                    .contains("no `provider` grant"),
                "{error:?}"
            ),
            other => panic!("revocation must fail closed: {other:?}"),
        }
        assert!(root.path().join("auth.json").is_file());
    }

    #[test]
    fn a_narrowed_cached_provider_profile_is_rebuilt() {
        let _lock = HOST_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempfile::tempdir().unwrap();
        let wasm = install_conformance_provider(root.path(), &["conformance-stream"]);
        let slot = ProviderSlot {
            id: "conformance".to_string(),
            wasm,
            agent_root: Some(root.path().to_path_buf()),
            host: std::sync::Mutex::new(None),
        };
        reset_hosts_built();
        let original = ensure_host(&slot).expect("initial host");
        let cached = ensure_host(&slot).expect("unchanged grant reuses the host");
        assert!(Arc::ptr_eq(&original, &cached));
        assert_eq!(hosts_built(), 1);

        let mut grants = Grants::load_for_at(root.path(), "conformance");
        grants.provider.as_mut().unwrap().providers[0].max_handles = 1;
        Grants::save_for_at(root.path(), "conformance", &grants).unwrap();
        let narrowed = ensure_host(&slot).expect("narrowed grant builds a fresh host");
        assert!(!Arc::ptr_eq(&original, &narrowed));
        assert_eq!(hosts_built(), 2);
    }
}
