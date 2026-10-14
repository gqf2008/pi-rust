//! Compile-time plugin event fan-out.
//!
//! First-party plugins are compiled into `ki` and observe events through
//! [`kiqi_plugin::Plugin::on_event`](kiqi_plugin) rather than the cdylib ABI.
//! This module is the seam: the native dispatch funnels call [`dispatch`], and
//! the host installs a [`CompiledEventSink`] whose closure translates the
//! stable event to the compile-time contract and calls every compiled plugin.
//!
//! `rpi-extensions` deliberately does **not** depend on `kiqi-plugin`: the sink
//! receives the wire tag plus an owned JSON payload, which is all the host
//! needs to build a `kiqi_plugin::Event`. No sink installed (a bare
//! `rpi-extensions` build, or tests) makes every call a no-op.

use std::sync::{Arc, OnceLock, RwLock};

use rpi_plugin_sdk::events::EventView;
use rpi_plugin_sdk::StablePluginEvent;
use serde_json::Value;

/// What the compiled plugins decided about one event.
///
/// Deliberately a *claim* channel, mirroring the native ABI's
/// `EVENT_HANDLER_CLAIMED` / `0`: the host has nowhere to deliver a replacement
/// payload, so `kiqi_plugin::EventAction::Replace` maps to [`Self::Claimed`]
/// (the plugin took the event) rather than pretending a rewrite can propagate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompiledVerdict {
    /// No plugin objected; later handlers still run.
    Continue,
    /// A plugin consumed the event: a routing dispatcher must stop and report it
    /// handled, exactly as a native handler's `CLAIMED` does.
    Claimed,
}

/// Receives one dispatched event as `(wire tag, JSON payload)` and reports what
/// the compiled plugins decided.
pub type CompiledEventSink = Arc<dyn Fn(u32, Value) -> CompiledVerdict + Send + Sync>;

static SINK: OnceLock<RwLock<Option<CompiledEventSink>>> = OnceLock::new();

fn sink_slot() -> &'static RwLock<Option<CompiledEventSink>> {
    SINK.get_or_init(|| RwLock::new(None))
}

/// Install (or replace) the sink the host uses to reach compiled plugins.
///
/// Idempotent; last writer wins. The host installs it once per session build
/// with the session's capabilities captured in the closure.
pub fn set_compiled_event_sink(sink: CompiledEventSink) {
    *sink_slot().write().unwrap() = Some(sink);
}

/// Remove the sink, returning the host to no compiled fan-out.
///
/// The running host never tears it down; this exists so a test can restore the
/// process-global state it shares with the rest of the test binary.
#[doc(hidden)]
pub fn clear_compiled_event_sink() {
    *sink_slot().write().unwrap() = None;
}

/// Fan one stable event out to the compiled plugins, if a sink is installed.
///
/// Called from the same dispatch funnels the cdylib handlers go through, so a
/// compiled plugin sees exactly the events a native one would — and, for the
/// routing dispatchers, its verdict counts the same way.
pub(crate) fn dispatch(event: &StablePluginEvent) -> CompiledVerdict {
    let slot = sink_slot().read().unwrap();
    let Some(sink) = slot.as_ref() else {
        return CompiledVerdict::Continue;
    };
    sink(event.tag as u32, event_payload(event))
}

/// The JSON payload the compile-time contract carries.
///
/// Mirrors what `EventView`'s typed accessors expose to a cdylib handler, as a
/// `serde_json::Value`: data/message payloads were JSON *strings* on the native
/// side and are parsed here, tool payloads become the camelCase object the
/// `kiqi_plugin::Event::tool_result` accessor already understands.
fn event_payload(event: &StablePluginEvent) -> Value {
    // SAFETY: `event` is the live event the host is dispatching; the view only
    // reads borrows of its payload and copies out before it returns.
    let view = unsafe { EventView::from_raw(event) };
    let parse = |text: String| serde_json::from_str(&text).unwrap_or(Value::Null);

    if let Some(text) = view.data_text() {
        return parse(text);
    }
    if let Some(text) = view.message_text() {
        return parse(text);
    }
    if let Some(call) = view.tool_call() {
        return serde_json::json!({
            "toolCallId": call.tool_call_id,
            "toolName": call.tool_name,
            "params": parse(call.params),
        });
    }
    if let Some(result) = view.tool_result() {
        return serde_json::json!({
            "toolCallId": result.tool_call_id,
            "toolName": result.tool_name,
            "result": parse(result.result),
            "isError": result.is_error,
        });
    }
    Value::Null
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_plugin_sdk::{EventTag, StbString};
    use std::sync::Mutex;

    /// Serializes the tests in this module; the sink is process-global.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn a_sink_observes_data_and_empty_events() {
        let _guard = TEST_LOCK.lock().unwrap();
        let seen: Arc<Mutex<Vec<(u32, Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        set_compiled_event_sink(Arc::new(move |tag, payload| {
            record.lock().unwrap().push((tag, payload));
            CompiledVerdict::Continue
        }));

        // A registry with no native handlers still reaches the sink: this is the
        // `ki` with no dylibs case.
        let snapshot = crate::registry::ExtensionRegistry::new().snapshot();
        assert!(!crate::translate::dispatch_data_event(
            &snapshot,
            EventTag::BeforeAgentStart,
            r#"{"prompt":"hi"}"#,
        ));
        assert!(!crate::translate::dispatch_empty_event(
            &snapshot,
            EventTag::SessionStart,
        ));

        let seen = seen.lock().unwrap();
        assert!(
            seen.iter()
                .any(|(tag, payload)| *tag == 15 && payload["prompt"] == "hi"),
            "the data payload must arrive parsed: {seen:?}"
        );
        assert!(
            seen.iter()
                .any(|(tag, payload)| *tag == 2 && payload.is_null()),
            "the empty lifecycle payload must arrive null: {seen:?}"
        );
        drop(seen);
        clear_compiled_event_sink();
    }

    #[test]
    fn tool_payloads_become_camel_case_json() {
        let call = StablePluginEvent::tool_call(
            EventTag::ToolExecutionStart,
            StbString::from_string("c1".into()),
            StbString::from_string("bash".into()),
            StbString::from_string(r#"{"command":"ls"}"#.into()),
        );
        let payload = event_payload(&call);
        assert_eq!(payload["toolCallId"], "c1");
        assert_eq!(payload["toolName"], "bash");
        assert_eq!(payload["params"]["command"], "ls");

        let result = StablePluginEvent::tool_result(
            EventTag::ToolExecutionEnd,
            StbString::from_string("c1".into()),
            StbString::from_string("bash".into()),
            StbString::from_string(r#"{"content":"ok"}"#.into()),
            true,
        );
        let payload = event_payload(&result);
        assert_eq!(payload["toolCallId"], "c1");
        assert_eq!(payload["result"]["content"], "ok");
        assert_eq!(payload["isError"], true);
    }

    #[test]
    fn without_a_sink_dispatch_is_a_noop() {
        let _guard = TEST_LOCK.lock().unwrap();
        clear_compiled_event_sink();
        let event = StablePluginEvent::empty(EventTag::SessionStart);
        assert_eq!(dispatch(&event), CompiledVerdict::Continue);
    }

    /// A claim from the compiled side reaches the routing dispatcher, so a
    /// compiled plugin can consume a key exactly like a cdylib handler.
    #[test]
    fn a_claimed_verdict_reaches_the_routing_dispatcher() {
        let _guard = TEST_LOCK.lock().unwrap();
        set_compiled_event_sink(Arc::new(|_, _| CompiledVerdict::Claimed));
        // No native handlers at all: the compiled verdict alone must decide.
        assert!(crate::translate::dispatch_data_event_claiming(
            None,
            EventTag::Input,
            r#"{"key":"f8"}"#,
        ));
        clear_compiled_event_sink();

        // With no sink the same call is unclaimed.
        assert!(!crate::translate::dispatch_data_event_claiming(
            None,
            EventTag::Input,
            r#"{"key":"f8"}"#,
        ));
    }
}
