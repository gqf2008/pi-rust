//! Extension provider hooks. Bridges an extension's `on(BeforeProviderRequest)`
//! / `on(BeforeProviderHeaders)` handlers into kiqi-ai's [`ProviderHooks`] slot,
//! so every provider call fans the live request/response context out to the
//! subscribed extensions.
//!
//! Events reach extensions through the pure-serde [`crate::Event`] bus and the
//! compiled/wasm sink; the native (cdylib) handler tier was removed in
//! `docs/design/extension-abi.md` §6.49.
//!
//! **Observer semantics:** the contract's `EventAction` carries no request
//! mutation for these tags; a `Patch` returned on this fan-out is ignored. The
//! only mutation channel is the provider gate on `BeforeProviderRequest`
//! ([`crate::translate::dispatch_before_provider_request`]).

use std::sync::{Arc, Mutex};

use crate::EventTag;
use kiqi_ai::types::{Content, Message, UserContent};
use kiqi_ai::Context;
use kiqi_ai::{ContextPatch, Model, ProviderHooks, SimpleStreamOptions, SimpleStreamOptionsPatch};

use crate::loader::ExtensionSession;
use crate::registry::RegistrySnapshot;
use crate::translate::{dispatch_before_provider_request_with, dispatch_data_event_with};

/// A [`ProviderHooks`] that dispatches to the compiled/wasm extensions.
pub struct ExtensionProviderHooks {
    /// The session snapshot whose sink receives these events. `None` (a session
    /// with no snapshot) falls back to the process-global sink, exactly like the
    /// other dispatch funnels.
    snapshot: Option<Arc<RegistrySnapshot>>,
    /// Timestamp of the last user message we emitted `BeforeAgentStart` for.
    /// The hook fires once per provider call, but `before_agent_start` must
    /// fire once per prompt, so the prompt timestamp is the dedupe key.
    last_before_agent_start: Mutex<Option<i64>>,
}

impl ExtensionProviderHooks {
    /// Build hooks for a session. Returns `None` when neither the session nor
    /// the process has a sink (so a session without extensions runs the plain
    /// no-op path).
    ///
    /// The wasm guest's prompt transform is chained separately by `kiqi-cli`
    /// (`compiled_prompt_hooks`); this bridge is only the provider-event
    /// observer.
    pub fn from_session(session: &ExtensionSession) -> Option<Self> {
        // A wasm-only install has no native handler in the snapshot, but the
        // compiled/wasm fan-out is installed and is the observer. Without this,
        // a sandboxed extension's provider-request grant would be dead in a
        // real session (it was only ever exercised by direct host tests).
        //
        // A session carrying its own sink is self-sufficient; otherwise the
        // process-global fallback has to be installed.
        let snapshot = session.snapshot_arc();
        let has_session_sink = snapshot.as_ref().and_then(|s| s.compiled_sink()).is_some();
        if !has_session_sink && !crate::compiled::sink_installed() {
            return None;
        }
        Some(Self {
            snapshot,
            last_before_agent_start: Mutex::new(None),
        })
    }
}

impl ProviderHooks for ExtensionProviderHooks {
    fn transform_context(&self, _model: &Model, _ctx: &Context) -> Option<ContextPatch> {
        // The native prompt-transform slot is gone; wasm transforms are chained
        // by `compiled_prompt_hooks` in `kiqi-cli`.
        None
    }

    /// Fan the planned request out to `BeforeProviderRequest` +
    /// `BeforeProviderHeaders` subscribers. Returns the provider-gate patch, if
    /// any.
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
                dispatch_data_event_with(
                    self.snapshot.as_deref(),
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
        // The rich form additionally carries the header values; the sandboxed
        // host strips them per guest unless the grant says `headers: values`,
        // and then only for the host's non-credential allowlist.
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
        let patch = dispatch_before_provider_request_with(self.snapshot.as_deref(), &rich);
        let headers = serde_json::json!({
            "headerNames": opts
                .headers
                .as_ref()
                .map(|h| h.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        });
        dispatch_data_event_with(
            self.snapshot.as_deref(),
            EventTag::BeforeProviderHeaders,
            &headers.to_string(),
        );
        patch.as_ref().and_then(options_patch_from_value)
    }
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
/// when the observer event is dispatched: a wasm transform runs first (the same
/// order pi uses in `assistant.ts` — transform, then request). So an observer
/// sees the prompt the provider is about to be given.
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
/// One source of truth for the payload shape. `None` when there is no user
/// message to key on (a continuation), in which case the transform is skipped
/// rather than applied to an unknown turn.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SINK_TEST_LOCK as TEST_LOCK;
    use serde_json::{json, Value};
    use std::sync::Arc;

    fn model() -> Model {
        Model::new(
            "m",
            "m",
            kiqi_ai::types::Api::AnthropicMessages,
            "p",
            "https://user:pass@host.test/v1?key=secret",
        )
    }

    fn collect_events() -> Arc<Mutex<Vec<(EventTag, Value)>>> {
        let seen: Arc<Mutex<Vec<(EventTag, Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        crate::compiled::set_compiled_event_sink(Arc::new(move |_mode, event| {
            record
                .lock()
                .unwrap()
                .push((event.tag, event.payload.clone()));
            crate::compiled::CompiledVerdict::Continue
        }));
        seen
    }

    #[test]
    fn before_request_emits_before_agent_start_once_and_forwards_messages() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let seen = collect_events();
        let hooks = ExtensionProviderHooks::from_session(&ExtensionSession::none())
            .expect("a sink is installed");

        let ctx = Context {
            system_prompt: Some("sys".into()),
            messages: vec![Message::User(kiqi_ai::types::UserMessage::new(
                UserContent::Text("hello".into()),
                7,
            ))],
            ..Default::default()
        };
        let opts = SimpleStreamOptions::default();
        hooks.before_request(&model(), &ctx, &opts);
        hooks.before_request(&model(), &ctx, &opts);

        let seen = seen.lock().unwrap();
        let before_agent_start: Vec<_> = seen
            .iter()
            .filter(|(tag, _)| *tag == EventTag::BeforeAgentStart)
            .collect();
        assert_eq!(before_agent_start.len(), 1, "once per prompt timestamp");
        assert_eq!(before_agent_start[0].1["prompt"], "hello");
        assert_eq!(before_agent_start[0].1["systemPrompt"], "sys");

        let requests: Vec<_> = seen
            .iter()
            .filter(|(tag, _)| *tag == EventTag::BeforeProviderRequest)
            .collect();
        // Two observe dispatches + two provider-gate dispatches per call pair.
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].1["messages"][0]["content"], "hello");
        assert_eq!(requests[0].1["baseUrl"], "https://host.test/v1");

        let headers: Vec<_> = seen
            .iter()
            .filter(|(tag, _)| *tag == EventTag::BeforeProviderHeaders)
            .collect();
        assert_eq!(headers.len(), 2);

        drop(seen);
        crate::compiled::clear_compiled_event_sink();
    }

    #[test]
    fn before_request_returns_the_gate_patch() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::compiled::set_compiled_event_sink(Arc::new(|mode, event| {
            if mode != crate::compiled::DispatchMode::ProviderGate {
                return crate::compiled::CompiledVerdict::Continue;
            }
            assert_eq!(event.tag, EventTag::BeforeProviderRequest);
            crate::compiled::CompiledVerdict::Patch(json!({"headers": {"x-trace": "abc"}}))
        }));
        let hooks = ExtensionProviderHooks::from_session(&ExtensionSession::none())
            .expect("a sink is installed");
        let patch = hooks
            .before_request(
                &model(),
                &Context::default(),
                &SimpleStreamOptions::default(),
            )
            .expect("the gate patch reaches the options");
        let headers = patch.headers.expect("headers patched");
        assert_eq!(
            headers.get("x-trace").and_then(|v| v.as_deref()),
            Some("abc")
        );
        crate::compiled::clear_compiled_event_sink();
    }

    #[test]
    fn without_a_sink_the_bridge_is_not_installed() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::compiled::clear_compiled_event_sink();
        assert!(ExtensionProviderHooks::from_session(&ExtensionSession::none()).is_none());
    }

    #[test]
    fn before_request_sanitizes_base_url_in_both_observation_payloads() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let seen = collect_events();
        let hooks = ExtensionProviderHooks::from_session(&ExtensionSession::none())
            .expect("a sink is installed");
        hooks.before_request(
            &model(),
            &Context::default(),
            &SimpleStreamOptions::default(),
        );

        let seen = seen.lock().unwrap();
        for (tag, payload) in seen.iter() {
            if tag == &EventTag::BeforeProviderRequest {
                assert_eq!(payload["baseUrl"], "https://host.test/v1");
                assert!(
                    !payload.to_string().contains("secret"),
                    "credential must not leak: {payload}"
                );
            }
        }
        drop(seen);
        crate::compiled::clear_compiled_event_sink();
    }
}
