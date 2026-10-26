//! B4 — extension provider hooks. Bridges an extension's `on(BeforeProviderRequest)`
//! / `on(BeforeProviderHeaders)` / `on(AfterProviderResponse)` handlers into
//! kiqi-ai's [`ProviderHooks`] slot, so every provider call fans the live
//! request/response context out to the subscribed extensions.
//!
//! **Observer semantics (v1):** the SDK `EventHandlerFn` returns `i32` (no
//! result payload), so a handler observes the request/response — it can record
//! state, rotate an external token store, emit diagnostics — but cannot PATCH
//! the live `SimpleStreamOptions`. The `before_request` hook therefore returns
//! `None` (no patch); a future ABI extension (an `out` pointer on the handler)
//! would unlock the patch path. The harness treats a `None` return as
//! "leave opts untouched", so the observer path is safe.

use std::sync::{Arc, Mutex};

use crate::EventTag;
use kiqi_ai::types::{AssistantMessage, Content, Message, UserContent};
use kiqi_ai::Context;
use kiqi_ai::{ContextPatch, Model, ProviderHooks, SimpleStreamOptions, SimpleStreamOptionsPatch};

use crate::loader::ExtensionSession;
use crate::prompt_transform::emit_before_agent_start;
use crate::registry::RegistrySnapshot;
use crate::translate::{dispatch_before_provider_request, dispatch_data_event};

/// A [`ProviderHooks`] that dispatches to the registered extension handlers.
pub struct ExtensionProviderHooks {
    snapshot: Arc<RegistrySnapshot>,
    /// Timestamp of the last user message we emitted `BeforeAgentStart` for.
    /// The hook fires once per provider call, but `before_agent_start` must
    /// fire once per prompt, so the prompt timestamp is the dedupe key.
    last_before_agent_start: Mutex<Option<i64>>,
    /// The prompt transform result for the prompt identified by the timestamp,
    /// cached so it is computed **once per prompt** rather than once per
    /// provider call. pi computes it once before the run and reuses it
    /// (`_runSystemPromptOptions`, reset in the run's `finally`); a tool loop
    /// makes several provider calls for one prompt, and re-running every
    /// registered handler on each of them would both repeat side effects and
    /// let an unstable handler change the prompt mid-run.
    ///
    /// `Some((timestamp, prompt))` where `prompt: None` records "this prompt was
    /// transformed, result was no-change" — distinct from "not yet computed",
    /// so handlers are not re-run just because they declined to change anything.
    last_forced_prompt: Mutex<Option<(i64, Option<String>)>>,
}

impl ExtensionProviderHooks {
    /// Build hooks over a loaded extension session's registry snapshot. Returns
    /// `None` when nothing needs this bridge (so a session without provider or
    /// prompt hooks runs the plain no-op path).
    ///
    /// Two independent reasons to install it, and both must be checked:
    ///
    /// - **Provider-hook tags** — an extension observing request/response. Includes
    ///   `BeforeAgentStart`, which is emitted from this bridge (see
    ///   [`Self::before_request`]) because the harness never emits a prompt
    ///   message event.
    /// - **`before_agent_start` transformers** — an extension that *replaces* the
    ///   system prompt. This is a separate slot, not an event subscription, so a
    ///   extension that only transforms the prompt registers no handlers at all;
    ///   keying this check on the tags alone would skip the bridge and silently
    ///   ignore its transform.
    pub fn from_session(session: &ExtensionSession) -> Option<Self> {
        let snapshot = session.snapshot_arc()?;
        let observes_events = [
            EventTag::BeforeProviderRequest,
            EventTag::BeforeProviderHeaders,
            EventTag::AfterProviderResponse,
            EventTag::BeforeAgentStart,
        ]
        .iter()
        .any(|t| !snapshot.handlers_for(*t).is_empty());
        let transforms_prompt = !snapshot.before_agent_start().is_empty();
        // A wasm-only install has no cdylib handler in the snapshot, but the
        // compiled/wasm fan-out is still installed and is the observer. Without
        // this, a sandboxed extension's provider-request grant would be dead in a
        // real session (it was only ever exercised by direct host tests).
        let observes_compiled = crate::compiled::sink_installed();
        if !observes_events && !transforms_prompt && !observes_compiled {
            return None;
        }
        Some(Self {
            snapshot,
            last_before_agent_start: Mutex::new(None),
            last_forced_prompt: Mutex::new(None),
        })
    }
}

impl ProviderHooks for ExtensionProviderHooks {
    /// pi's `transform_context` / `before_agent_start`-returns-`systemPrompt`:
    /// let an extension replace the prompt the request carries.
    ///
    /// Computed once per prompt and cached, so every provider call of a run
    /// (the tool loop) sees the same prompt — see
    /// [`Self::last_forced_prompt`]. Returns `None` when no extension registered a
    /// transformer, when they all declined, or when the registry is stale, all
    /// of which mean "leave the prompt alone".
    fn transform_context(&self, _model: &Model, ctx: &Context) -> Option<ContextPatch> {
        if self.snapshot.before_agent_start().is_empty() {
            return None;
        }
        // Keyed on the user prompt, like the observer event below: the prompt is
        // what identifies a turn, and it is stable across the run's provider
        // calls. With no user message (a continuation) there is nothing to key
        // on, so the transform is skipped rather than applied to an unknown turn.
        let (timestamp, prompt, image_count) = latest_user_prompt(&ctx.messages)?;

        let mut cache = self.last_forced_prompt.lock().unwrap();
        if cache.as_ref().map(|(ts, _)| *ts) != Some(timestamp) {
            let event = before_agent_start_event(&prompt, image_count, ctx);
            let forced = emit_before_agent_start(
                ctx.system_prompt.as_deref().unwrap_or_default(),
                &event.to_string(),
                &self.snapshot,
            );
            *cache = Some((timestamp, forced));
        }
        cache
            .as_ref()
            .and_then(|(_, forced)| forced.clone())
            .map(|system_prompt| ContextPatch {
                system_prompt: Some(system_prompt),
            })
    }

    /// Fan the planned request out to `BeforeProviderRequest` +
    /// `BeforeProviderHeaders` subscribers. Returns `None` for the stream options
    /// — observer-only (the event handler ABI has no result channel); an extension's
    /// influence on the *prompt* travels through [`Self::transform_context`]
    /// instead.
    fn before_request(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> Option<SimpleStreamOptionsPatch> {
        // (A) pi's `before_agent_start`: observe each NEW user prompt exactly
        // once. The harness persists prompts before the run and calls
        // `run_agent_loop` with an empty prompts vec, so no
        // MessageStart/MessageEnd reaches extensions for them. This bridge is the
        // first place the prompt is visible, so it emits the pi-equivalent
        // event. Dedupe by prompt timestamp: one run can make several provider
        // calls, but `before_agent_start` fires once per prompt.
        if let Some((timestamp, prompt, image_count)) = latest_user_prompt(&ctx.messages) {
            let mut last = self.last_before_agent_start.lock().unwrap();
            if *last != Some(timestamp) {
                *last = Some(timestamp);
                drop(last);
                let event = before_agent_start_event(&prompt, image_count, ctx);
                // Synchronous, like the request dispatch below: a subscriber's
                // `BeforeAgentStart` handler runs before any
                // `BeforeProviderRequest` of this turn reaches it, so the turn is
                // fully identified (session included) by the time its first
                // generation is created.
                dispatch_data_event(
                    &self.snapshot,
                    EventTag::BeforeAgentStart,
                    &event.to_string(),
                );
            }
        }

        // 凭据脱敏（卡：0.8.0 能力补齐，P0）：观测者只拿到「有没有 key」与
        // header **名字**，不再拿到 apiKey 与 header **值**（`authorization` /
        // `x-api-key` 就在其中）；`baseUrl` 去掉 userinfo 与查询串——网关地址里
        // 常内嵌 `user:pass@host?api_key=…`，原样下发等于把凭据递进沙箱。
        // `messages` 默认仍进载荷（由宿主按 grant 分级决定是否下发）——真实生成
        // 输入是 langfuse 这类观测者的唯一用途。
        let header_names: Vec<String> = opts
            .headers
            .as_ref()
            .map(|h| h.keys().cloned().collect())
            .unwrap_or_default();
        let mut request = serde_json::json!({
            "model": model.id,
            "provider": model.provider,
            "baseUrl": sanitize_base_url(&model.base_url),
            "reasoning": model.reasoning,
            "apiKeyPresent": opts.api_key.as_deref().is_some_and(|key| !key.is_empty()),
            "timeoutMs": opts.timeout.map(|d| d.as_millis() as u64),
            "headerNames": header_names,
            "metadata": opts.metadata,
            "maxTokens": opts.max_tokens,
            "temperature": opts.temperature,
            // (B) the model-facing messages, so observers (e.g. kiqi-langfuse)
            // can record the real generation input. `ctx` was previously
            // discarded entirely, which is why generation/trace input was
            // always null for extensions.
            "messages": ctx.messages,
        });
        // The redacted form (header names only, never values) goes to cdylib
        // handlers, which have no grant model. The rich form additionally
        // carries the header values; the sandboxed host strips them per guest
        // unless the grant says `headers: values`, and then only for the
        // host's non-credential allowlist. Splitting the payload here keeps
        // the source-redaction guarantee for the native tier.
        let redacted = request.to_string();
        let header_values: serde_json::Map<String, serde_json::Value> = opts
            .headers
            .as_ref()
            .map(|headers| {
                headers
                    .iter()
                    .map(|(name, value)| (name.clone(), serde_json::json!(value)))
                    .collect()
            })
            .unwrap_or_default();
        if !header_values.is_empty() {
            request["headerValues"] = serde_json::Value::Object(header_values);
        }
        let rich = request.to_string();
        let patch = dispatch_before_provider_request(&self.snapshot, &redacted, &rich);
        let headers = serde_json::json!({
            "headerNames": opts
                .headers
                .as_ref()
                .map(|h| h.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        });
        dispatch_data_event(
            &self.snapshot,
            EventTag::BeforeProviderHeaders,
            &headers.to_string(),
        );
        patch.as_ref().and_then(options_patch_from_value)
    }

    // `AfterProviderResponse` 不再派发（0.8.0 契约，见 extension-abi §6.41）：
    // wasm 侧没有消费者，cdylib 侧也会在 P6 随通道删除；宿主侧同时拒绝向
    // wasm 转发该 tag（见 kiqi-extension-host `dispatch_event`）。终端 assistant
    // 消息由 `MessageEnd` 覆盖观测需求。
}

/// 观测用的 baseUrl 脱敏：只保留 `scheme://authority/path`，丢弃 userinfo 与
/// 整段查询/锚点。网关地址常把密钥内嵌在查询或 userinfo 里，观测者不需要它们。
fn sanitize_base_url(raw: &str) -> String {
    let (scheme, rest) = match raw.split_once("://") {
        Some((scheme, rest)) => (scheme, rest),
        None => return String::new(),
    };
    let rest = match rest.split_once(['?', '#']) {
        Some((path, _)) => path,
        None => rest,
    };
    let rest = match rest.rsplit_once('@') {
        Some((_, without_userinfo)) => without_userinfo,
        None => rest,
    };
    format!("{scheme}://{rest}")
}

/// Convert a host-validated patch JSON into the live options patch.
///
/// The wasm host has already enforced the clamp table and the protected-header
/// deny-list, so this is a mechanical mapping: `null` deletes a map entry.
/// Returns `None` for an empty patch (nothing to apply).
#[cfg(test)]
mod sanitize_tests {
    use super::sanitize_base_url;

    #[test]
    fn strips_userinfo_query_and_fragment() {
        assert_eq!(
            sanitize_base_url("https://user:password@gateway.test/v1?api_key=sk-live"),
            "https://gateway.test/v1"
        );
        assert_eq!(
            sanitize_base_url("http://alice@host.example/path#frag"),
            "http://host.example/path"
        );
        assert_eq!(sanitize_base_url("https://host.test"), "https://host.test");
        assert_eq!(sanitize_base_url("not a url"), "");
    }
}

fn options_patch_from_value(value: &serde_json::Value) -> Option<SimpleStreamOptionsPatch> {
    let mut patch = SimpleStreamOptionsPatch::default();
    let mut any = false;
    if let Some(map) = value.get("headers").and_then(serde_json::Value::as_object) {
        let mut out = std::collections::BTreeMap::new();
        for (name, entry) in map {
            out.insert(name.clone(), entry.as_str().map(str::to_string));
        }
        patch.headers = Some(out);
        any = true;
    }
    if let Some(map) = value.get("metadata").and_then(serde_json::Value::as_object) {
        let mut out = std::collections::BTreeMap::new();
        for (name, entry) in map {
            out.insert(name.clone(), entry.as_str().map(str::to_string));
        }
        patch.metadata = Some(out);
        any = true;
    }
    if let Some(ms) = value.get("timeoutMs").and_then(serde_json::Value::as_u64) {
        patch.timeout = Some(Some(std::time::Duration::from_millis(ms)));
        any = true;
    }
    if let Some(tokens) = value.get("maxTokens").and_then(serde_json::Value::as_u64) {
        patch.max_tokens = Some(Some(tokens));
        any = true;
    }
    if let Some(temperature) = value.get("temperature").and_then(serde_json::Value::as_f64) {
        patch.temperature = Some(Some(temperature));
        any = true;
    }
    any.then_some(patch)
}

/// The `before_agent_start` envelope: what the observer event carries and what
/// a prompt-transforming handler reads.
///
/// One builder for both, so an observer and a transformer cannot be shown
/// different views of the same turn.
///
/// `systemPrompt` is read from `ctx`, which means the **post-transform** prompt
/// when the observer event is dispatched: ki emits that event from
/// `before_request`, and `ProviderHooks::transform_context` runs first (the same
/// order pi uses in `assistant.ts` — transform, then request). So an observer
/// sees the prompt the provider is about to be given. pi's own
/// `before_agent_start` fires earlier in the run and therefore carries the base
/// prompt; an extension that needs the pre-transform value should capture it from
/// its own transformer's input instead of from this event.
fn before_agent_start_event(prompt: &str, image_count: usize, ctx: &Context) -> serde_json::Value {
    serde_json::json!({
        "prompt": prompt,
        "imageCount": image_count,
        "systemPrompt": ctx.system_prompt,
        // The session this process serves. An extension that traces the run reports
        // it as the session id (nothing else tells an observer which session it
        // is watching). Read from the environment per turn rather than captured
        // at harness build, so an in-process session swap is reflected on the
        // very next turn.
        "sessionId": std::env::var(kiqi_agent::SESSION_ID_ENV).ok(),
        // Who this extension is embedded in, so it can label its own output
        // (trace name, tags, `service.name`) without hardcoding a brand that a
        // rename would silently invalidate. Sent per turn, so it describes the
        // process actually running rather than the SDK the extension was built
        // against.
        "host": host_identity(),
    })
}

/// The `BeforeAgentStart` payload for a compiled extension, plus the user-prompt
/// timestamp that keys a once-per-prompt transform.
///
/// One source of truth for the payload shape: the native transform and a
/// compiled one must see the same fields. `None` when there is no user message
/// to key on (a continuation), in which case the transform is skipped rather
/// than applied to an unknown turn.
pub fn before_agent_start_input(ctx: &Context) -> Option<(i64, serde_json::Value)> {
    let (timestamp, prompt, image_count) = latest_user_prompt(&ctx.messages)?;
    Some((
        timestamp,
        before_agent_start_event(&prompt, image_count, ctx),
    ))
}

/// The host's own identity, as handed to extensions.
///
/// The name is the extension contract's constant ([`kiqi_agent::HOST_NAME`])
/// and the version is this host crate's own `CARGO_PKG_VERSION` — every
/// workspace crate shares one version, so this is the running host's version
/// and not the SDK an extension was compiled against. Both are compile-time
/// aliases, so there is exactly one place to change either.
fn host_identity() -> serde_json::Value {
    serde_json::json!({
        "name": kiqi_agent::HOST_NAME,
        "version": env!("CARGO_PKG_VERSION"),
    })
}

/// Text/timestamp/image-count of the most recent user message. Mirrors the
/// `prompt`/`images` fields pi puts on its `before_agent_start` event.
fn latest_user_prompt(messages: &[Message]) -> Option<(i64, String, usize)> {
    messages.iter().rev().find_map(|message| match message {
        Message::User(user) => Some((
            user.timestamp,
            user_content_text(&user.content),
            user_content_image_count(&user.content),
        )),
        _ => None,
    })
}

fn user_content_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                Content::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn user_content_image_count(content: &UserContent) -> usize {
    match content {
        UserContent::Text(_) => 0,
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter(|block| matches!(block, Content::Image(_)))
            .count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventTag;
    use crate::StableExtensionEvent;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    static PROVIDER_HITS: AtomicUsize = AtomicUsize::new(0);
    static PROVIDER_LOCK: Mutex<()> = Mutex::new(());
    static CAPTURED: Mutex<Vec<(EventTag, String)>> = Mutex::new(Vec::new());

    extern "C" fn capture_handler(ev: StableExtensionEvent, _ud: *mut std::ffi::c_void) -> i32 {
        let payload = unsafe { ev.payload.data.data.to_string_lossy() };
        CAPTURED.lock().unwrap().push((ev.tag, payload));
        0
    }

    extern "C" fn counting_provider_handler(
        ev: StableExtensionEvent,
        _ud: *mut std::ffi::c_void,
    ) -> i32 {
        // Count every dispatch on the subscribed tags (BeforeProviderRequest +
        // BeforeProviderHeaders). The `model` check validates the request-event
        // payload shape separately — it must NOT gate the count, or the headers
        // event (whose payload is `{"headers":...}` with no `model` key) would
        // be silently dropped and the fan-out count would be wrong.
        let s = unsafe { ev.payload.data.data.to_string_lossy() };
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
            // The request event carries `model`; the headers event does not.
            // Both are valid dispatches — count unconditionally, and sanity-check
            // the request payload when `model` is present.
            if v.get("model").is_some() {
                assert!(v.get("model").is_some(), "request event must carry model");
            }
            PROVIDER_HITS.fetch_add(1, Ordering::SeqCst);
        }
        0
    }

    /// `before_agent_start` fires once per NEW user prompt (not once per
    /// provider call in a run), and the request payload carries the
    /// model-facing `messages`.
    #[test]
    fn before_request_emits_before_agent_start_once_and_forwards_messages() {
        let _guard = PROVIDER_LOCK.lock().unwrap();
        CAPTURED.lock().unwrap().clear();

        let mut registry = crate::registry::ExtensionRegistry::new();
        registry.register_event_handler(
            "test-extension".to_string(),
            EventTag::BeforeAgentStart,
            capture_handler,
            std::ptr::null_mut(),
        );
        registry.register_event_handler(
            "test-extension".to_string(),
            EventTag::BeforeProviderRequest,
            capture_handler,
            std::ptr::null_mut(),
        );
        let snapshot = Arc::new(registry.snapshot());
        let hooks = ExtensionProviderHooks {
            snapshot: Arc::clone(&snapshot),
            last_before_agent_start: Mutex::new(None),
            last_forced_prompt: Mutex::new(None),
        };
        let model = kiqi_ai::Model::new(
            "m",
            "m",
            kiqi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let ctx = kiqi_ai::Context::new(vec![kiqi_ai::types::Message::User(
            kiqi_ai::types::UserMessage::new(
                kiqi_ai::types::UserContent::Text("hello".to_string()),
                42,
            ),
        )]);
        let opts = kiqi_ai::SimpleStreamOptions::default();

        // Two provider calls in the same run (same prompt) → one
        // before_agent_start, two before_provider_request observations.
        hooks.before_request(&model, &ctx, &opts);
        hooks.before_request(&model, &ctx, &opts);

        let captured = CAPTURED.lock().unwrap().clone();
        let agent_starts: Vec<_> = captured
            .iter()
            .filter(|(tag, _)| matches!(tag, EventTag::BeforeAgentStart))
            .collect();
        assert_eq!(
            agent_starts.len(),
            1,
            "before_agent_start must fire once per prompt: {captured:?}"
        );
        let start_payload: serde_json::Value =
            serde_json::from_str(&agent_starts[0].1).expect("before_agent_start payload is JSON");
        assert_eq!(start_payload["prompt"], "hello");
        assert_eq!(start_payload["imageCount"], 0);
        // The session id rides along so an observer can name the session on the
        // very first turn (the prompt is the only per-turn channel it gets).
        // Absent when this process has no session id in its environment.
        assert_eq!(
            start_payload.get("sessionId").map(|v| v.is_null()),
            Some(std::env::var(kiqi_agent::SESSION_ID_ENV).is_err()),
            "sessionId must mirror the host's session id env var: {}",
            agent_starts[0].1
        );
        // And the host identifies itself, so an extension can label its own output
        // (trace name, tags, service.name) without hardcoding a brand.
        assert_eq!(start_payload["host"]["name"], kiqi_agent::HOST_NAME);
        assert_eq!(
            start_payload["host"]["version"],
            env!("CARGO_PKG_VERSION"),
            "the host reports the version of the host, not of the extension SDK"
        );

        let requests: Vec<_> = captured
            .iter()
            .filter(|(tag, _)| matches!(tag, EventTag::BeforeProviderRequest))
            .collect();
        assert_eq!(requests.len(), 2, "every provider call is observed");
        assert!(
            requests[0].1.contains("\"messages\""),
            "request payload must forward messages: {}",
            requests[0].1
        );
    }

    /// A host-validated patch maps mechanically onto the live options patch;
    /// `null` deletes a map entry and an empty patch is "no patch".
    #[test]
    fn a_validated_patch_maps_onto_live_options() {
        let patch = options_patch_from_value(&serde_json::json!({
            "headers": { "x-trace": "abc", "x-remove": null },
            "metadata": { "user_id": "u-1" },
            "timeoutMs": 1500,
            "maxTokens": 64,
            "temperature": 0.4,
        }))
        .expect("a non-empty patch");
        let headers = patch.headers.as_ref().expect("headers");
        assert_eq!(headers.get("x-trace"), Some(&Some("abc".to_string())));
        assert_eq!(headers.get("x-remove"), Some(&None));
        assert_eq!(
            patch.metadata.as_ref().unwrap().get("user_id"),
            Some(&Some("u-1".to_string()))
        );
        assert_eq!(
            patch.timeout,
            Some(Some(std::time::Duration::from_millis(1500)))
        );
        assert_eq!(patch.max_tokens, Some(Some(64)));
        assert_eq!(patch.temperature, Some(Some(0.4)));

        assert!(options_patch_from_value(&serde_json::json!({})).is_none());
    }

    /// The whole bridge: the gate pass runs after the observe pass, its patch
    /// becomes the live options patch, and the observe payload carries the
    /// header values the host grades per guest.
    #[test]
    fn before_request_returns_the_gate_patch() {
        let _guard = PROVIDER_LOCK.lock().unwrap();
        let _sink = crate::SINK_TEST_LOCK.lock().unwrap();

        type Seen = Vec<(crate::compiled::DispatchMode, u32, serde_json::Value)>;
        let seen: Arc<Mutex<Seen>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        crate::compiled::set_compiled_event_sink(Arc::new(move |mode, tag, payload| {
            recorder.lock().unwrap().push((mode, tag, payload.clone()));
            if mode == crate::compiled::DispatchMode::ProviderGate {
                crate::compiled::CompiledVerdict::Patch(serde_json::json!({ "maxTokens": 64 }))
            } else {
                crate::compiled::CompiledVerdict::Continue
            }
        }));

        let registry = crate::registry::ExtensionRegistry::new();
        let hooks = ExtensionProviderHooks {
            snapshot: Arc::new(registry.snapshot()),
            last_before_agent_start: Mutex::new(None),
            last_forced_prompt: Mutex::new(None),
        };
        let model = kiqi_ai::Model::new(
            "m",
            "m",
            kiqi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let ctx = kiqi_ai::Context::default();
        let mut opts = kiqi_ai::SimpleStreamOptions::default();
        opts.headers = Some(std::collections::BTreeMap::from([
            ("Authorization".to_string(), "secret".to_string()),
            ("x-trace".to_string(), "abc".to_string()),
        ]));

        let patch = hooks
            .before_request(&model, &ctx, &opts)
            .expect("the gate patch");
        assert_eq!(patch.max_tokens, Some(Some(64)));

        let seen = seen.lock().unwrap();
        let request_tag = EventTag::BeforeProviderRequest as u32;
        let rich = seen
            .iter()
            .find(|(mode, tag, payload)| {
                *mode == crate::compiled::DispatchMode::Observe
                    && *tag == request_tag
                    && payload.get("headerValues").is_some()
            })
            .expect("the rich observe payload");
        assert_eq!(rich.2["headerValues"]["x-trace"], "abc");
        assert!(
            seen.iter().any(|(mode, tag, _)| {
                *mode == crate::compiled::DispatchMode::ProviderGate && *tag == request_tag
            }),
            "the gate pass runs exactly once after the observe pass"
        );
        drop(seen);
        crate::compiled::clear_compiled_event_sink();
    }

    /// Observation URLs are endpoint metadata, not a credential channel. Both
    /// the native-redacted and wasm-rich payloads must drop userinfo, query
    /// values and fragments before either serialization happens.
    #[test]
    fn before_request_sanitizes_base_url_in_both_observation_payloads() {
        let _guard = PROVIDER_LOCK.lock().unwrap();
        let _sink = crate::SINK_TEST_LOCK.lock().unwrap();
        CAPTURED.lock().unwrap().clear();

        type Seen = Vec<(crate::compiled::DispatchMode, u32, serde_json::Value)>;
        let seen: Arc<Mutex<Seen>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        crate::compiled::set_compiled_event_sink(Arc::new(move |mode, tag, payload| {
            recorder.lock().unwrap().push((mode, tag, payload.clone()));
            crate::compiled::CompiledVerdict::Continue
        }));

        let mut registry = crate::registry::ExtensionRegistry::new();
        registry.register_event_handler(
            "test-extension".to_string(),
            EventTag::BeforeProviderRequest,
            capture_handler,
            std::ptr::null_mut(),
        );
        let hooks = ExtensionProviderHooks {
            snapshot: Arc::new(registry.snapshot()),
            last_before_agent_start: Mutex::new(None),
            last_forced_prompt: Mutex::new(None),
        };
        let model = kiqi_ai::Model::new(
            "m",
            "m",
            kiqi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://user:password@gateway.test/v1?api_key=sk-live&token=also-secret#fragment-secret",
        );
        hooks.before_request(&model, &kiqi_ai::Context::default(), &Default::default());

        let request_tag = EventTag::BeforeProviderRequest as u32;
        let mut payloads: Vec<serde_json::Value> = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, tag, _)| *tag == request_tag)
            .map(|(_, _, payload)| payload.clone())
            .collect();
        payloads.extend(
            CAPTURED
                .lock()
                .unwrap()
                .iter()
                .filter(|(tag, _)| *tag == EventTag::BeforeProviderRequest)
                .map(|(_, payload)| serde_json::from_str(payload).expect("native payload is JSON")),
        );
        assert_eq!(payloads.len(), 3, "native, observe and gate payloads");
        for payload in payloads {
            let serialized = payload.to_string();
            for secret in [
                "user:password",
                "api_key=sk-live",
                "token=also-secret",
                "fragment-secret",
            ] {
                assert!(!serialized.contains(secret), "leaked {secret}: {payload}");
            }
            assert_eq!(payload["baseUrl"], "https://gateway.test/v1");
        }
        crate::compiled::clear_compiled_event_sink();
    }

    /// `dispatch_data_event` fans the JSON to the tag's handlers, and the
    /// `ExtensionProviderHooks::before_request` path dispatches BOTH the
    /// request and headers events to their subscribers.
    #[test]
    fn provider_hooks_dispatch_to_subscribers() {
        let _guard = PROVIDER_LOCK.lock().unwrap();
        PROVIDER_HITS.store(0, Ordering::SeqCst);

        let mut registry = crate::registry::ExtensionRegistry::new();
        let handler: crate::EventHandlerFn = counting_provider_handler;
        registry.register_event_handler(
            "test-extension".to_string(),
            EventTag::BeforeProviderRequest,
            handler,
            std::ptr::null_mut(),
        );
        registry.register_event_handler(
            "test-extension".to_string(),
            EventTag::BeforeProviderHeaders,
            handler,
            std::ptr::null_mut(),
        );
        let snapshot = Arc::new(registry.snapshot());

        // dispatch_data_event directly: one request event ⇒ one hit.
        assert!(dispatch_data_event(
            &snapshot,
            EventTag::BeforeProviderRequest,
            r#"{"model":"m"}"#,
        ));
        assert_eq!(PROVIDER_HITS.load(Ordering::SeqCst), 1);

        // through ExtensionProviderHooks::before_request. The hook fires BOTH
        // BeforeProviderRequest AND BeforeProviderHeaders (each with the same
        // counting handler subscribed), so 2 more hits ⇒ 3 total. The headers
        // event's payload is `{"headers":...}` (no `model` key) — the handler
        // counts it regardless (see counting_provider_handler).
        let hooks = ExtensionProviderHooks {
            snapshot: Arc::clone(&snapshot),
            last_before_agent_start: Mutex::new(None),
            last_forced_prompt: Mutex::new(None),
        };
        let model = kiqi_ai::Model::new(
            "m",
            "m",
            kiqi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let ctx = kiqi_ai::Context::default();
        let opts = kiqi_ai::SimpleStreamOptions::default();
        let patch = hooks.before_request(&model, &ctx, &opts);
        assert!(patch.is_none(), "observer-only v1: no patch returned");
        assert_eq!(PROVIDER_HITS.load(Ordering::SeqCst), 3);
    }

    /// A tag with no subscribers dispatches nothing (and doesn't free anything
    /// dangling — dispatch_data_event short-circuits before building events).
    #[test]
    fn provider_hooks_no_subscribers_is_noop() {
        let registry = crate::registry::ExtensionRegistry::new();
        let snapshot = Arc::new(registry.snapshot());
        assert!(!dispatch_data_event(
            &snapshot,
            EventTag::AfterProviderResponse,
            "{}",
        ));
    }

    // ---- before_agent_start prompt transform ------------------------------
    //
    // These use in-process `extern "C"` handlers rather than a cdylib: the ABI
    // round-trip has its own end-to-end test (`extension_stub_smoke`), so what is
    // left to pin here is the semantics — chaining, failure isolation, and the
    // "no change" signal — which are easier to express with direct handlers.

    /// Handler that returns `<marker>` + whatever prompt it was handed, so a
    /// test can observe the chain by the markers accumulating in order.
    extern "C" fn append_marker(
        event_json: crate::StbStringRef,
        out: *mut crate::StbString,
        user_data: *mut std::ffi::c_void,
    ) -> i32 {
        // SAFETY: host guarantees the ref for this call; the marker arrives as
        // `user_data` (a `&'static str` the test leaked).
        let envelope = unsafe { event_json.as_str() };
        let marker = unsafe { &*(user_data as *const &'static str) };
        let current = serde_json::from_str::<serde_json::Value>(envelope)
            .ok()
            .and_then(|v| v.get("systemPrompt")?.as_str().map(str::to_string))
            .unwrap_or_default();
        let json = serde_json::json!({ "systemPrompt": format!("{current}{marker}") });
        unsafe {
            *out = crate::StbString::from_string(json.to_string());
        }
        0
    }

    /// Handler that declines to change anything (empty object ⇒ no-op).
    extern "C" fn declines(
        _event_json: crate::StbStringRef,
        _out: *mut crate::StbString,
        _user_data: *mut std::ffi::c_void,
    ) -> i32 {
        0
    }

    /// Handler that reports a handled error.
    extern "C" fn fails(
        _event_json: crate::StbStringRef,
        _out: *mut crate::StbString,
        _user_data: *mut std::ffi::c_void,
    ) -> i32 {
        7
    }

    /// Build hooks whose snapshot has the given prompt transformers.
    fn hooks_with(
        handlers: Vec<crate::registry::BeforeAgentStartHandler>,
    ) -> ExtensionProviderHooks {
        let mut registry = crate::registry::ExtensionRegistry::new();
        for h in handlers {
            registry.register_before_agent_start(h.handler, h.extension_free_string, h.user_data);
        }
        ExtensionProviderHooks {
            snapshot: Arc::new(registry.snapshot()),
            last_before_agent_start: Mutex::new(None),
            last_forced_prompt: Mutex::new(None),
        }
    }

    /// Transformers chain in registration order: each sees the previous one's
    /// result. This is pi's `runner.ts` behavior (a later handler observes the
    /// prompt an earlier one set), and it is what makes "append" composable.
    #[test]
    fn prompt_transformers_chain_in_registration_order() {
        let a: &'static str = " A";
        let b: &'static str = " B";
        let leak = |s: &'static str| Box::leak(Box::new(s)) as *mut &'static str as *mut _;
        let hooks = hooks_with(vec![
            crate::registry::BeforeAgentStartHandler {
                handler: append_marker,
                extension_free_string: noop_free,
                user_data: leak(a),
            },
            crate::registry::BeforeAgentStartHandler {
                handler: append_marker,
                extension_free_string: noop_free,
                user_data: leak(b),
            },
        ]);

        let model = kiqi_ai::Model::new(
            "m",
            "m",
            kiqi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let mut ctx = kiqi_ai::Context::new(vec![kiqi_ai::types::Message::User(
            kiqi_ai::types::UserMessage::new(
                kiqi_ai::types::UserContent::Text("hello".to_string()),
                1,
            ),
        )]);
        ctx.system_prompt = Some("base".to_string());

        let patch = hooks
            .transform_context(&model, &ctx)
            .expect("both handlers changed the prompt");
        // Both markers present, in order — proof the second handler saw the
        // first's output rather than the original prompt.
        assert_eq!(patch.system_prompt.as_deref(), Some("base A B"));
    }

    /// A failing handler leaves the prompt as the previous handler left it and
    /// the fan-out continues (pi isolates a throwing handler; it does not abort
    /// the run or discard earlier results).
    #[test]
    fn a_failing_transformer_is_skipped_and_the_chain_continues() {
        let after: &'static str = " AFTER";
        let leaked = Box::leak(Box::new(after)) as *mut &'static str as *mut _;
        let hooks = hooks_with(vec![
            crate::registry::BeforeAgentStartHandler {
                handler: fails,
                extension_free_string: noop_free,
                user_data: std::ptr::null_mut(),
            },
            crate::registry::BeforeAgentStartHandler {
                handler: append_marker,
                extension_free_string: noop_free,
                user_data: leaked,
            },
        ]);

        let model = kiqi_ai::Model::new(
            "m",
            "m",
            kiqi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let mut ctx = kiqi_ai::Context::new(vec![kiqi_ai::types::Message::User(
            kiqi_ai::types::UserMessage::new(
                kiqi_ai::types::UserContent::Text("hello".to_string()),
                1,
            ),
        )]);
        ctx.system_prompt = Some("base".to_string());

        let patch = hooks.transform_context(&model, &ctx).expect("changed");
        assert_eq!(
            patch.system_prompt.as_deref(),
            Some("base AFTER"),
            "the failing handler must contribute nothing and not stop the next one"
        );
    }

    /// All handlers declining ⇒ `None`, i.e. the harness leaves the prompt
    /// alone. An extension that only observes must be able to say nothing without
    /// installing an empty prompt.
    #[test]
    fn declining_transformers_leave_the_prompt_untouched() {
        let hooks = hooks_with(vec![crate::registry::BeforeAgentStartHandler {
            handler: declines,
            extension_free_string: noop_free,
            user_data: std::ptr::null_mut(),
        }]);
        let model = kiqi_ai::Model::new(
            "m",
            "m",
            kiqi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let ctx = kiqi_ai::Context::new(vec![kiqi_ai::types::Message::User(
            kiqi_ai::types::UserMessage::new(
                kiqi_ai::types::UserContent::Text("hello".to_string()),
                1,
            ),
        )]);
        assert!(
            hooks.transform_context(&model, &ctx).is_none(),
            "no handler changed the prompt ⇒ no patch ⇒ harness leaves it alone"
        );
    }

    /// No registered transformer ⇒ `None` and the ABI is never entered. A
    /// session with only observer extensions must not pay for (or risk) this path.
    #[test]
    fn no_transformers_means_no_patch() {
        let hooks = hooks_with(vec![]);
        let model = kiqi_ai::Model::new(
            "m",
            "m",
            kiqi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let ctx = kiqi_ai::Context::default();
        assert!(hooks.transform_context(&model, &ctx).is_none());
    }

    /// Minimal free fn for handlers whose `out` is built by the SDK (they never
    /// hand back an extension allocation in these tests, but the registry requires
    /// the field).
    extern "C" fn noop_free(_s: crate::StbString) {}

    /// The `BeforeAgentStart` observer sees the **post-transform** prompt.
    ///
    /// ki emits that event from `before_request`, and `transform_context` runs
    /// first (the order `agent_harness::build_stream_fn` uses, mirroring pi's
    /// `assistant.ts`). So an observer is told what the provider is about to be
    /// given, not what it would have been given. This is a deliberate contract
    /// and a subtle one: swapping the two calls in `build_stream_fn` would
    /// silently change what every observer reports, with no other test failing.
    #[test]
    fn the_observer_sees_the_prompt_after_the_transform() {
        let marker: &'static str = " MARK";
        let leaked = Box::leak(Box::new(marker)) as *mut &'static str as *mut _;
        let _guard = PROVIDER_LOCK.lock().unwrap();
        CAPTURED.lock().unwrap().clear();

        let mut registry = crate::registry::ExtensionRegistry::new();
        registry.register_event_handler(
            "test-extension".to_string(),
            EventTag::BeforeAgentStart,
            capture_handler,
            std::ptr::null_mut(),
        );
        registry.register_before_agent_start(append_marker, noop_free, leaked);
        let hooks = ExtensionProviderHooks {
            snapshot: Arc::new(registry.snapshot()),
            last_before_agent_start: Mutex::new(None),
            last_forced_prompt: Mutex::new(None),
        };

        let model = kiqi_ai::Model::new(
            "m",
            "m",
            kiqi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let mut ctx = kiqi_ai::Context::new(vec![kiqi_ai::types::Message::User(
            kiqi_ai::types::UserMessage::new(
                kiqi_ai::types::UserContent::Text("hello".to_string()),
                1,
            ),
        )]);
        ctx.system_prompt = Some("base".to_string());

        // How the harness drives the two hooks: transform first, then apply, then
        // observe from the patched context.
        let patch = hooks
            .transform_context(&model, &ctx)
            .expect("transformer changed the prompt");
        patch.apply(&mut ctx);
        hooks.before_request(&model, &ctx, &kiqi_ai::SimpleStreamOptions::default());

        let captured = CAPTURED.lock().unwrap().clone();
        let start = captured
            .iter()
            .find(|(tag, _)| matches!(tag, EventTag::BeforeAgentStart))
            .expect("BeforeAgentStart was dispatched");
        let payload: serde_json::Value = serde_json::from_str(&start.1).expect("JSON");
        assert_eq!(
            payload["systemPrompt"], "base MARK",
            "observers must be shown the prompt the provider will actually get"
        );
    }
}
