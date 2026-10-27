//! `AgentEvent → Event` translation, plus the [`ExtensionEmitter`] that impls
//! [`AgentEmitter`] by subscribing to the host's `broadcast::Sender<AgentEvent>`,
//! folding each event into the extension event contract, and fanning it out to
//! the compiled/wasm sink.
//!
//! The ten stable lifecycle `AgentEvent` variants fold into their matching
//! `EventTag` tags. Internal host events such as retry scheduling are
//! intentionally not projected onto the fixed extension event surface.
//!
//! The event that crosses this seam is the plain serde [`Event`] from the
//! extension contract — no `repr(C)` wire format any more (the C-ABI bus was
//! removed in the pure-serde batch, `docs/design/extension-abi.md` §6.49).

use std::sync::Arc;

use crate::Event;
use crate::EventTag;
use crate::RegistrySnapshot;
use futures::future::BoxFuture;
use kiqi_agent::events::{AgentEmitter, AgentEvent};
use serde_json::Value;

/// Map a native [`AgentEvent`] to its pi `on()` [`EventTag`], or `None` if the
/// host event has no stable-event counterpart. This is the **fold** described
/// in the crate docs: the ten stable lifecycle variants map onto matching tags;
/// internal retry scheduling stays host-only, and the rest of the surface is
/// driven by direct host emission.
pub fn event_tag_for(event: &AgentEvent) -> Option<EventTag> {
    match event {
        AgentEvent::AgentStart => Some(EventTag::AgentStart),
        AgentEvent::AgentEnd { .. } => Some(EventTag::AgentEnd),
        AgentEvent::RetryScheduled { .. } => None,
        AgentEvent::TurnStart => Some(EventTag::TurnStart),
        AgentEvent::TurnEnd { .. } => Some(EventTag::TurnEnd),
        AgentEvent::MessageStart { .. } => Some(EventTag::MessageStart),
        AgentEvent::MessageUpdate { .. } => Some(EventTag::MessageUpdate),
        AgentEvent::MessageEnd { .. } => Some(EventTag::MessageEnd),
        AgentEvent::ToolExecutionStart { .. } => Some(EventTag::ToolExecutionStart),
        AgentEvent::ToolExecutionUpdate { .. } => Some(EventTag::ToolExecutionUpdate),
        AgentEvent::ToolExecutionEnd { .. } => Some(EventTag::ToolExecutionEnd),
    }
}

/// Build the extension event for an `AgentEvent`, or `None` if the event carries
/// no payload that maps onto a stable variant (always maps today, but kept as
/// `Option` for forward-compat with unmapped future variants).
pub fn translate(event: &AgentEvent) -> Option<Event> {
    let tag = event_tag_for(event)?;
    match event {
        AgentEvent::MessageStart { message } | AgentEvent::MessageEnd { message } => {
            // AgentMessage is Serialize (#[serde(tag="kind")]); use serde_json
            // round-trip. A serialization failure degrades to a null payload,
            // matching the old wire contract.
            let payload = serde_json::to_value(message).unwrap_or(Value::Null);
            Some(Event::new(tag, payload))
        }
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => {
            // Match native Pi's wire contract: message_update carries the
            // provider event, not a growing message snapshot. Consumers that
            // need live text read assistantMessageEvent.delta directly.
            let payload = assistant_message_event_to_value(assistant_message_event);
            Some(Event::new(tag, payload))
        }

        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        }
        | AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            args,
            ..
        } => Some(Event::new(
            tag,
            serde_json::json!({
                "toolCallId": tool_call_id,
                "toolName": tool_name,
                "params": args,
            }),
        )),

        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => Some(Event::new(
            tag,
            serde_json::json!({
                "toolCallId": tool_call_id,
                "toolName": tool_name,
                "result": agent_tool_result_to_value(result),
                "isError": is_error,
            }),
        )),

        // No-payload events.
        AgentEvent::AgentStart
        | AgentEvent::TurnStart
        | AgentEvent::AgentEnd { .. }
        | AgentEvent::TurnEnd { .. } => Some(Event::new(tag, Value::Null)),

        AgentEvent::RetryScheduled { .. } => None,
    }
}

/// Serialize the stream event inside native Pi's `message_update` envelope.
///
/// Do not include `partial`: it is a cumulative snapshot and the native JSON /
/// RPC contract deliberately omits it so consumers process each delta exactly
/// once and stream size stays linear.
fn assistant_message_event_to_value(event: &kiqi_ai::types::AssistantMessageEvent) -> Value {
    use kiqi_ai::types::AssistantMessageEvent as E;

    let value = match event {
        E::TextStart { content_index, .. } => serde_json::json!({
            "type": "text_start",
            "contentIndex": content_index,
        }),
        E::TextDelta {
            content_index,
            delta,
            ..
        } => serde_json::json!({
            "type": "text_delta",
            "contentIndex": content_index,
            "delta": delta,
        }),
        E::TextEnd {
            content_index,
            content,
            ..
        } => serde_json::json!({
            "type": "text_end",
            "contentIndex": content_index,
            "content": content,
        }),
        E::ThinkingStart { content_index, .. } => serde_json::json!({
            "type": "thinking_start",
            "contentIndex": content_index,
        }),
        E::ThinkingDelta {
            content_index,
            delta,
            ..
        } => serde_json::json!({
            "type": "thinking_delta",
            "contentIndex": content_index,
            "delta": delta,
        }),
        E::ThinkingEnd {
            content_index,
            content,
            ..
        } => serde_json::json!({
            "type": "thinking_end",
            "contentIndex": content_index,
            "content": content,
        }),
        E::ToolCallStart { content_index, .. } => serde_json::json!({
            "type": "toolcall_start",
            "contentIndex": content_index,
        }),
        E::ToolCallDelta {
            content_index,
            delta,
            ..
        } => serde_json::json!({
            "type": "toolcall_delta",
            "contentIndex": content_index,
            "delta": delta,
        }),
        E::ToolCallEnd {
            content_index,
            tool_call,
            ..
        } => serde_json::json!({
            "type": "toolcall_end",
            "contentIndex": content_index,
            "toolCall": tool_call,
        }),
        E::Start { .. } | E::Done { .. } | E::Error { .. } => {
            serde_json::json!({ "type": event_type_name(event) })
        }
    };

    serde_json::json!({ "assistantMessageEvent": value })
}

fn event_type_name(event: &kiqi_ai::types::AssistantMessageEvent) -> &'static str {
    use kiqi_ai::types::AssistantMessageEvent as E;
    match event {
        E::Start { .. } => "start",
        E::TextStart { .. } => "text_start",
        E::TextDelta { .. } => "text_delta",
        E::TextEnd { .. } => "text_end",
        E::ThinkingStart { .. } => "thinking_start",
        E::ThinkingDelta { .. } => "thinking_delta",
        E::ThinkingEnd { .. } => "thinking_end",
        E::ToolCallStart { .. } => "toolcall_start",
        E::ToolCallDelta { .. } => "toolcall_delta",
        E::ToolCallEnd { .. } => "toolcall_end",
        E::Done { .. } => "done",
        E::Error { .. } => "error",
    }
}

/// Serialize an `AgentToolResult` to the JSON shape extensions expect.
fn agent_tool_result_to_value(result: &kiqi_agent::types::AgentToolResult) -> Value {
    let content: Vec<Value> = result
        .content
        .iter()
        .map(|c| match c {
            kiqi_agent::types::TextContentOrImage::Text(t) => {
                serde_json::json!({ "type": "text", "text": t.text })
            }
            kiqi_agent::types::TextContentOrImage::Image(img) => serde_json::json!({
                "type": "image",
                "data": img.data,
                "mimeType": img.mime_type,
            }),
        })
        .collect();
    serde_json::json!({
        "content": content,
        "details": result.details,
        "terminate": result.terminate,
        "addedToolNames": result.added_tool_names,
    })
}

// ===========================================================================
// ExtensionEmitter — AgentEmitter impl fanning out to the compiled sink
// ===========================================================================

/// An [`AgentEmitter`] that fans each [`AgentEvent`] out to the installed
/// compiled/wasm sink. Installed into `AgentHarnessOptions.agent_emitter`
/// **alongside** the host's `BroadcastEmitter` — events flow to BOTH the TUI
/// (which drains the broadcast receiver) and the extensions.
///
/// The host composes the two via [`TeeEmitter`]; this emitter alone only
/// dispatches to extensions.
pub struct ExtensionEmitter {
    /// The session this emitter belongs to, if it was built from one. Dispatching
    /// through the session's snapshot reaches only that session's sink; `None`
    /// (a bare `ExtensionEmitter::new()`) uses the process-global fallback.
    snapshot: Option<Arc<RegistrySnapshot>>,
}

impl ExtensionEmitter {
    /// Build an emitter with no session attached: dispatch uses the
    /// process-global sink. Kept for tests and for callers that only ever run
    /// one session per process.
    pub fn new() -> Self {
        Self { snapshot: None }
    }

    /// Build an emitter bound to `session`'s snapshot, so every event it emits
    /// reaches the session's own sink — the per-session isolation path.
    pub fn for_session(session: &crate::loader::ExtensionSession) -> Self {
        Self {
            snapshot: session.snapshot_arc(),
        }
    }

    /// Build an emitter from an already-shared snapshot.
    pub fn with_snapshot(snapshot: Option<Arc<RegistrySnapshot>>) -> Self {
        Self { snapshot }
    }

    /// Dispatch one event to the compiled extensions.
    fn dispatch(&self, event: &Event) {
        dispatch_to_handlers_with(self.snapshot.as_deref(), event);
    }
}

impl Default for ExtensionEmitter {
    fn default() -> Self {
        Self::new()
    }
}

/// Fan an already-built event out to the compiled/wasm sink.
///
/// Compatibility entry point for call sites without a session snapshot: it uses
/// the process-global sink. Per-session paths call
/// [`dispatch_to_handlers_with`].
pub fn dispatch_to_handlers(event: &Event) {
    dispatch_to_handlers_with(None, event);
}

/// Fan an already-built event out through `snapshot`'s sink, falling back to
/// the process-global sink when the snapshot has none (or is absent).
pub fn dispatch_to_handlers_with(snapshot: Option<&RegistrySnapshot>, event: &Event) {
    // The verdict is ignored here on purpose: this is the observe fan-out, and
    // only the routing dispatchers act on `Claimed`.
    let sink = snapshot.and_then(RegistrySnapshot::compiled_sink);
    let _ = crate::compiled::dispatch_observe_with(sink, event);
}

/// Dispatch one `before-provider-request` turn: observe pass + provider gate.
///
/// Both passes receive the same rich payload; the wasm host strips what its
/// `provider-request` grant does not allow, per guest. The observe pass invokes
/// only observe-only hosts (a patch-capable host is skipped by the sink), and
/// the gate pass invokes only patch-capable hosts. Each host is therefore called
/// at most once per request. Returns the validated patch to apply, if any.
pub fn dispatch_before_provider_request(rich: &str) -> Option<Value> {
    dispatch_before_provider_request_with(None, rich)
}

/// [`dispatch_before_provider_request`] through `snapshot`'s sink, falling back
/// to the process-global sink when the snapshot has none.
pub fn dispatch_before_provider_request_with(
    snapshot: Option<&RegistrySnapshot>,
    rich: &str,
) -> Option<Value> {
    let event = crate::Event::new(
        EventTag::BeforeProviderRequest,
        serde_json::from_str(rich).unwrap_or(Value::Null),
    );
    let sink = snapshot.and_then(RegistrySnapshot::compiled_sink);
    let _ = crate::compiled::dispatch_observe_with(sink, &event);
    crate::compiled::dispatch_provider_gate_with(sink, &event)
}

/// Build + dispatch a generic `data` event (JSON payload). Returns whether any
/// native handler was invoked — always `false` now that the native registration
/// channel is gone; kept for call-site compatibility with the observe funnels.
pub fn dispatch_data_event(tag: EventTag, data: &str) -> bool {
    dispatch_data_event_with(None, tag, data)
}

/// [`dispatch_data_event`] through `snapshot`'s sink, falling back to the
/// process-global sink when the snapshot has none.
pub fn dispatch_data_event_with(
    snapshot: Option<&RegistrySnapshot>,
    tag: EventTag,
    data: &str,
) -> bool {
    let event = build_data_event(tag, data);
    dispatch_to_handlers_with(snapshot, &event);
    false
}

/// Build + dispatch a **no-payload** event (e.g. `session_start`,
/// `session_shutdown`, the kiqi-specific `before_tui_start`). Returns whether
/// any native handler was invoked — always `false`.
pub fn dispatch_empty_event(tag: EventTag) -> bool {
    dispatch_empty_event_with(None, tag)
}

/// [`dispatch_empty_event`] through `snapshot`'s sink, falling back to the
/// process-global sink when the snapshot has none.
pub fn dispatch_empty_event_with(snapshot: Option<&RegistrySnapshot>, tag: EventTag) -> bool {
    dispatch_to_handlers_with(snapshot, &Event::new(tag, Value::Null));
    false
}

/// Dispatch a data event and report whether it was **claimed** by a compiled
/// extension (anything other than `EventAction::Continue`).
pub fn dispatch_data_event_claiming(tag: EventTag, data: &str) -> bool {
    dispatch_data_event_claiming_with(None, tag, data)
}

/// [`dispatch_data_event_claiming`] through `snapshot`'s sink, falling back to
/// the process-global sink when the snapshot has none.
pub fn dispatch_data_event_claiming_with(
    snapshot: Option<&RegistrySnapshot>,
    tag: EventTag,
    data: &str,
) -> bool {
    let event = build_data_event(tag, data);
    let sink = snapshot.and_then(RegistrySnapshot::compiled_sink);
    crate::compiled::dispatch_claiming_with(sink, &event)
        == crate::compiled::CompiledVerdict::Claimed
}

/// Build a data event, parsing the JSON string payload. A malformed payload
/// degrades to `null` rather than dropping the event.
fn build_data_event(tag: EventTag, data: &str) -> Event {
    Event::new(tag, serde_json::from_str(data).unwrap_or(Value::Null))
}

/// Fan a **no-payload lifecycle event** (`BeforeTuiStart` / `SessionStart` /
/// `SessionShutdown`) out to the compiled extensions.
///
/// The extension contract's `EventAction` has no abort/veto, so this never
/// returns a reason; the signature is kept for the callers that used to honor a
/// native veto.
pub async fn dispatch_lifecycle_event(tag: EventTag) -> Option<String> {
    dispatch_lifecycle_event_with(None, tag).await
}

/// [`dispatch_lifecycle_event`] through `snapshot`'s sink, falling back to the
/// process-global sink when the snapshot has none.
pub async fn dispatch_lifecycle_event_with(
    snapshot: Option<&RegistrySnapshot>,
    tag: EventTag,
) -> Option<String> {
    let sink = snapshot.and_then(RegistrySnapshot::compiled_sink);
    let _ = crate::compiled::dispatch_observe_with(sink, &Event::new(tag, Value::Null));
    None
}

impl AgentEmitter for ExtensionEmitter {
    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        // Translate + dispatch synchronously (the emitter's emit is awaited by
        // the loop in event order). If the event maps to no tag, do nothing.
        if let Some(event) = translate(&event) {
            self.dispatch(&event);
        }
        Box::pin(async {})
    }

    fn try_emit(&self, event: AgentEvent) {
        if let Some(event) = translate(&event) {
            self.dispatch(&event);
        }
    }
}

// ===========================================================================
// TeeEmitter — fan an AgentEvent out to N AgentEmitters (host + extensions)
// ===========================================================================

/// An [`AgentEmitter`] that forwards every event to each of its children, in
/// registration order. The host builds one around `[BroadcastEmitter (→ TUI),
/// ExtensionEmitter (→ extensions)]` so a single `AgentHarnessOptions
/// .agent_emitter` slot feeds both consumers.
///
/// `emit` awaits each child in turn (the loop awaits `emit`, so order matches
/// registration); `try_emit` calls each child's `try_emit` (the tool
/// `on_update` path — non-blocking). We do NOT wrap the fan-out itself in
/// `catch_unwind` — each child is responsible for its own soundness, and a
/// generic wrapper would mask a child's contract violation.
pub struct TeeEmitter {
    emitters: Vec<Arc<dyn AgentEmitter>>,
}

impl TeeEmitter {
    /// Build a tee over the given emitters. Order is preserved: `emit`/`try_emit`
    /// visit them front-to-back. A single-child tee is a trivial passthrough.
    pub fn new(emitters: Vec<Arc<dyn AgentEmitter>>) -> Self {
        Self { emitters }
    }
}

impl AgentEmitter for TeeEmitter {
    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        let emitters = self.emitters.clone();
        Box::pin(async move {
            for e in &emitters {
                e.emit(event.clone()).await;
            }
        })
    }

    fn try_emit(&self, event: AgentEvent) {
        for e in &self.emitters {
            e.try_emit(event.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventTag;
    use kiqi_agent::message::AgentMessage;
    use kiqi_agent::types::AgentToolResult;
    use kiqi_ai::types::{AssistantMessage, AssistantMessageEvent, Usage};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    use crate::SINK_TEST_LOCK as TEST_LOCK;

    fn assistant_message(text: &str) -> AgentMessage {
        AgentMessage::Assistant(Box::new(AssistantMessage {
            role: kiqi_ai::types::AssistantRole,
            content: vec![kiqi_ai::types::Content::text(text)],
            api: kiqi_ai::types::Api::AnthropicMessages,
            provider: "anthropic".to_string(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason: kiqi_ai::types::StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        }))
    }

    #[test]
    fn all_ten_agent_events_map_to_a_tag() {
        let am = assistant_message("hi");
        let events = vec![
            AgentEvent::AgentStart,
            AgentEvent::AgentEnd { messages: vec![] },
            AgentEvent::TurnStart,
            AgentEvent::TurnEnd {
                message: am.clone(),
                tool_results: vec![],
            },
            AgentEvent::MessageStart {
                message: am.clone(),
            },
            AgentEvent::MessageUpdate {
                // `MessageUpdate` carries the shared partial snapshot, so build
                // the same `Arc` the provider would have handed over.
                message: std::sync::Arc::new((*am.as_assistant().unwrap()).clone()),
                assistant_message_event: kiqi_ai::types::AssistantMessageEvent::Start {
                    partial: std::sync::Arc::new((*am.as_assistant().unwrap()).clone()),
                },
            },
            AgentEvent::MessageEnd { message: am },
            AgentEvent::ToolExecutionStart {
                tool_call_id: "c1".into(),
                tool_name: "echo".into(),
                args: serde_json::json!({}),
            },
            AgentEvent::ToolExecutionUpdate {
                tool_call_id: "c1".into(),
                tool_name: "echo".into(),
                args: serde_json::json!({}),
                partial_result: std::sync::Arc::new(AgentToolResult::text("...")),
            },
            AgentEvent::ToolExecutionEnd {
                tool_call_id: "c1".into(),
                tool_name: "echo".into(),
                result: AgentToolResult::text("done"),
                is_error: false,
            },
        ];
        for e in &events {
            assert!(
                event_tag_for(e).is_some(),
                "event {:?} should map",
                e.type_tag()
            );
        }
        // Spot-check the fold targets.
        assert_eq!(event_tag_for(&events[0]), Some(EventTag::AgentStart));
        assert_eq!(event_tag_for(&events[2]), Some(EventTag::TurnStart));
        assert_eq!(
            event_tag_for(&events[7]),
            Some(EventTag::ToolExecutionStart)
        );
        assert!(event_tag_for(&AgentEvent::RetryScheduled {
            attempt: 1,
            max_retries: 10,
            delay_ms: 2_000,
            error: "503".into(),
        })
        .is_none());
    }

    #[test]
    fn translate_message_update_forwards_native_text_deltas_without_snapshot() {
        let partial = Arc::new(AssistantMessage {
            role: kiqi_ai::types::AssistantRole,
            content: vec![kiqi_ai::types::Content::text("你好世界")],
            api: kiqi_ai::types::Api::AnthropicMessages,
            provider: "anthropic".to_string(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason: kiqi_ai::types::StopReason::Pending,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        });
        let translate_delta = |delta: &str| {
            let event = AgentEvent::MessageUpdate {
                message: Arc::clone(&partial),
                assistant_message_event: AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: delta.to_string(),
                    partial: Arc::clone(&partial),
                },
            };
            translate(&event).expect("message_update maps").payload
        };

        let first = translate_delta("你好");
        let second = translate_delta("世界");
        assert_eq!(first["assistantMessageEvent"]["type"], "text_delta");
        assert_eq!(first["assistantMessageEvent"]["contentIndex"], 0);
        assert_eq!(first["assistantMessageEvent"]["delta"], "你好");
        assert_eq!(second["assistantMessageEvent"]["delta"], "世界");
        assert!(first.get("message").is_none());
        assert!(first["assistantMessageEvent"].get("partial").is_none());
    }

    #[test]
    fn translate_message_end_produces_message_payload() {
        let ev = AgentEvent::MessageEnd {
            message: assistant_message("hi"),
        };
        let event = translate(&ev).expect("maps");
        assert_eq!(event.tag, EventTag::MessageEnd);
        assert_eq!(event.payload["content"][0]["text"], "hi");
    }

    #[test]
    fn translate_tool_events_use_camel_case_payloads() {
        let start = translate(&AgentEvent::ToolExecutionStart {
            tool_call_id: "c1".into(),
            tool_name: "bash".into(),
            args: json!({"command": "ls"}),
        })
        .unwrap();
        assert_eq!(start.payload["toolCallId"], "c1");
        assert_eq!(start.payload["toolName"], "bash");
        assert_eq!(start.payload["params"]["command"], "ls");

        let end = translate(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "c1".into(),
            tool_name: "bash".into(),
            result: AgentToolResult::text("ok"),
            is_error: true,
        })
        .unwrap();
        assert_eq!(end.payload["toolCallId"], "c1");
        assert_eq!(end.payload["result"]["content"][0]["text"], "ok");
        assert_eq!(end.payload["isError"], true);
    }

    /// The emitter fans each translated event into the installed sink.
    #[test]
    fn emitter_dispatches_to_the_compiled_sink() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let seen: Arc<Mutex<Vec<(EventTag, Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        crate::compiled::set_compiled_event_sink(Arc::new(move |_mode, event| {
            record
                .lock()
                .unwrap()
                .push((event.tag, event.payload.clone()));
            crate::compiled::CompiledVerdict::Continue
        }));

        let emitter = ExtensionEmitter::new();
        emitter.try_emit(AgentEvent::MessageEnd {
            message: assistant_message("hi"),
        });
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, EventTag::MessageEnd);
        assert_eq!(seen[0].1["content"][0]["text"], "hi");
        drop(seen);
        crate::compiled::clear_compiled_event_sink();
    }

    // --- TeeEmitter: fan-out to both the broadcast + the extension emitter ----

    #[tokio::test]
    async fn tee_emitter_fans_out_to_every_child() {
        use kiqi_agent::events::{AgentEmitter, CollectorEmitter};

        let (collector_a, events_a) = CollectorEmitter::new();
        let (collector_b, events_b) = CollectorEmitter::new();

        let tee = TeeEmitter::new(vec![
            Arc::new(collector_a),
            Arc::new(collector_b),
            Arc::new(ExtensionEmitter::new()),
        ]);

        tee.emit(AgentEvent::MessageEnd {
            message: assistant_message("hi"),
        })
        .await;
        assert_eq!(
            events_a.lock().unwrap().len(),
            1,
            "collector A got the event"
        );
        assert_eq!(
            events_b.lock().unwrap().len(),
            1,
            "collector B got the event"
        );
    }

    #[test]
    fn lifecycle_and_data_funnels_only_reach_the_compiled_sink() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tags: Arc<Mutex<Vec<EventTag>>> = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&tags);
        crate::compiled::set_compiled_event_sink(Arc::new(move |_mode, event| {
            record.lock().unwrap().push(event.tag);
            crate::compiled::CompiledVerdict::Continue
        }));

        assert!(!dispatch_empty_event(EventTag::SessionStart));
        assert!(!dispatch_data_event(
            EventTag::EditorChange,
            r#"{"chars":4,"empty":false}"#
        ));
        assert!(!dispatch_data_event_claiming(
            EventTag::Input,
            r#"{"key":"space"}"#
        ));
        let seen: Vec<EventTag> = tags
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|tag| {
                matches!(
                    tag,
                    EventTag::SessionStart | EventTag::EditorChange | EventTag::Input
                )
            })
            .collect();
        assert_eq!(
            seen,
            vec![
                EventTag::SessionStart,
                EventTag::EditorChange,
                EventTag::Input
            ]
        );

        crate::compiled::clear_compiled_event_sink();
    }

    #[test]
    fn a_data_payload_is_parsed_and_malformed_input_becomes_null() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let payloads: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&payloads);
        crate::compiled::set_compiled_event_sink(Arc::new(move |_mode, event| {
            record.lock().unwrap().push(event.payload.clone());
            crate::compiled::CompiledVerdict::Continue
        }));

        dispatch_data_event(EventTag::EditorChange, r#"{"chars":4}"#);
        dispatch_data_event(EventTag::EditorChange, "not json");
        assert_eq!(
            *payloads.lock().unwrap(),
            vec![json!({"chars": 4}), Value::Null]
        );

        crate::compiled::clear_compiled_event_sink();
    }

    /// Two sessions in one process each dispatch through their own sink. The
    /// process-global slot must stay untouched (it would otherwise cross-talk).
    #[test]
    fn two_sessions_dispatch_only_to_their_own_sinks() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use crate::compiled::CompiledVerdict;
        use crate::loader::ExtensionSession;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let global_hits = Arc::new(AtomicUsize::new(0));
        let global_counter = Arc::clone(&global_hits);
        crate::compiled::set_compiled_event_sink(Arc::new(move |_, _| {
            global_counter.fetch_add(1, Ordering::SeqCst);
            CompiledVerdict::Continue
        }));

        let a: Arc<Mutex<Vec<EventTag>>> = Arc::new(Mutex::new(Vec::new()));
        let b: Arc<Mutex<Vec<EventTag>>> = Arc::new(Mutex::new(Vec::new()));
        let record_a = Arc::clone(&a);
        let record_b = Arc::clone(&b);
        let session_a =
            ExtensionSession::none_with_compiled_sink(Some(Arc::new(move |_, event| {
                record_a.lock().unwrap().push(event.tag);
                CompiledVerdict::Continue
            })));
        let session_b =
            ExtensionSession::none_with_compiled_sink(Some(Arc::new(move |_, event| {
                record_b.lock().unwrap().push(event.tag);
                CompiledVerdict::Continue
            })));

        assert!(!dispatch_empty_event_with(
            session_a.snapshot(),
            EventTag::SessionStart
        ));
        assert!(!dispatch_empty_event_with(
            session_b.snapshot(),
            EventTag::SessionTree
        ));
        assert!(!dispatch_empty_event_with(
            session_a.snapshot(),
            EventTag::SessionStart
        ));

        assert_eq!(
            *a.lock().unwrap(),
            vec![EventTag::SessionStart, EventTag::SessionStart]
        );
        assert_eq!(*b.lock().unwrap(), vec![EventTag::SessionTree]);
        assert_eq!(
            global_hits.load(Ordering::SeqCst),
            0,
            "a session sink must shadow the global one, not feed it too"
        );

        crate::compiled::clear_compiled_event_sink();
    }

    /// A snapshot that carries no sink (and the no-snapshot path) still reaches
    /// the process-global sink: the compatibility fallback.
    #[test]
    fn a_snapshot_without_a_sink_falls_back_to_the_global_sink() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use crate::compiled::CompiledVerdict;
        use crate::loader::ExtensionSession;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        crate::compiled::set_compiled_event_sink(Arc::new(move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            CompiledVerdict::Continue
        }));

        let session = ExtensionSession::none();
        assert!(!dispatch_empty_event_with(
            session.snapshot(),
            EventTag::SessionStart
        ));
        assert!(!dispatch_empty_event_with(None, EventTag::SessionTree));
        assert_eq!(hits.load(Ordering::SeqCst), 2);

        crate::compiled::clear_compiled_event_sink();
    }

    /// The emitter built from a session carries that session's sink, so the
    /// harness's agent events reach the right extensions even when the global
    /// slot belongs to another session.
    #[test]
    fn an_extension_emitter_uses_its_sessions_sink() {
        use crate::compiled::CompiledVerdict;
        use crate::loader::ExtensionSession;

        let seen: Arc<Mutex<Vec<EventTag>>> = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        let session = ExtensionSession::none_with_compiled_sink(Some(Arc::new(move |_, event| {
            record.lock().unwrap().push(event.tag);
            CompiledVerdict::Continue
        })));

        let emitter = ExtensionEmitter::for_session(&session);
        emitter.try_emit(AgentEvent::AgentStart);

        assert_eq!(*seen.lock().unwrap(), vec![EventTag::AgentStart]);
    }
}
