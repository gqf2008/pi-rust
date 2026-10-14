//! End-to-end: build the conformance guest, load it, call its tool, unload it.
//!
//! Requires the `wasm32-wasip2` target (`rustup target add wasm32-wasip2`); the
//! `Cargo test` CI job installs it. The guest is built here rather than checked
//! in, so the test cannot pass against a stale `.wasm`.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

use kiqi_plugin::{
    Block, Event, EventAction, EventTag, HostCaps, Log, LogLevel, Plugin, PluginError, Progress,
    ProgressSink,
};
use kiqi_plugin_host::WasmPlugin;

struct RecordingSink {
    blocks: Mutex<Vec<Progress>>,
}

impl ProgressSink for RecordingSink {
    fn progress(&self, progress: Progress) {
        self.blocks.lock().unwrap().push(progress);
    }
}

struct NullLog;

impl Log for NullLog {
    fn log(&self, _level: LogLevel, _message: &str) {}
}

/// Build `kiqi-conformance-guest` for `wasm32-wasip2` and return the component.
fn fixture_wasm() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let manifest = root.join("plugins/wasm/kiqi-conformance-guest/Cargo.toml");
    let status = Command::new(env!("CARGO"))
        .args(["build", "--manifest-path"])
        .arg(&manifest)
        .args(["--target", "wasm32-wasip2", "--release"])
        .status()
        .expect("run cargo build for the conformance fixture");
    assert!(
        status.success(),
        "building the conformance fixture failed; is the `wasm32-wasip2` target installed?"
    );
    root.join("plugins/wasm/target/wasm32-wasip2/release/kiqi_conformance_guest.wasm")
}

#[test]
fn loads_calls_streams_denies_and_unloads() {
    let wasm = fixture_wasm();
    let plugin = WasmPlugin::load("conformance", &wasm).expect("load the component");

    assert_eq!(plugin.id(), "conformance");
    let log = NullLog;
    let caps = HostCaps::observer(&log);

    // The guest's declared tools are visible to the host.
    let tools = plugin.tools();
    assert_eq!(tools.len(), 1, "one tool: {tools:?}");
    assert_eq!(tools[0].name, "conformance");

    // A call: progress is streamed and the denied capability is reported.
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    let outcome = plugin
        .call("conformance", &serde_json::json!({"x": 1}), &caps, &sink)
        .expect("call the tool");
    assert!(
        !outcome.is_error,
        "a denied capability is not a failed call"
    );
    let text = match &outcome.blocks[0] {
        Block::Text(text) => text.clone(),
        other => panic!("unexpected block {other:?}"),
    };
    assert!(text.contains(r#"{"x":1}"#), "arguments echoed: {text}");
    assert!(
        text.contains("ui=") && !text.contains("ui=granted"),
        "the denied capability must be reported: {text}"
    );
    // `details` is the state channel that must survive fork/switch.
    assert_eq!(outcome.details, serde_json::json!({"conformance": true}));

    let blocks = sink.blocks.lock().unwrap();
    assert_eq!(blocks.len(), 3, "three progress blocks streamed");
    assert!(matches!(&blocks[0], Progress::Text(t) if t == "first"));
    assert!(matches!(&blocks[1], Progress::Json(_)));
    assert!(matches!(&blocks[2], Progress::Text(t) if t == "last"));

    // An undeclared tool is a typed error, not a trap or a panic.
    let error = plugin
        .call("nope", &serde_json::json!({}), &caps, &sink)
        .unwrap_err();
    assert!(matches!(error, PluginError::UnknownTool(_)), "got {error}");

    // Events carry an action: the fixture claims `BeforeAgentStart`.
    let claimed = plugin.on_event(
        &Event::new(
            EventTag::BeforeAgentStart,
            serde_json::json!({"prompt": "hi"}),
        ),
        &caps,
    );
    assert_eq!(claimed, EventAction::Claimed, "a wasm plugin can claim");
    let observed = plugin.on_event(
        &Event::new(EventTag::SessionStart, serde_json::Value::Null),
        &caps,
    );
    assert_eq!(observed, EventAction::Continue);

    // Unload releases the instance; later calls fail cleanly instead of using a
    // released store.
    plugin.unload();
    let after = plugin
        .call("conformance", &serde_json::json!({}), &caps, &sink)
        .unwrap_err();
    assert!(after.to_string().contains("unloaded"), "got {after}");
}
