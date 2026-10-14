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

/// Build one guest crate for `wasm32-wasip2` and return the component.
fn build_guest(crate_dir: &str, artifact: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let manifest = root.join("plugins/wasm").join(crate_dir).join("Cargo.toml");
    let status = Command::new(env!("CARGO"))
        .args(["build", "--manifest-path"])
        .arg(&manifest)
        .args(["--target", "wasm32-wasip2", "--release"])
        .status()
        .unwrap_or_else(|error| panic!("run cargo build for {crate_dir}: {error}"));
    assert!(
        status.success(),
        "building {crate_dir} failed; is the `wasm32-wasip2` target installed?"
    );
    root.join("plugins/wasm/target/wasm32-wasip2/release")
        .join(artifact)
}

fn fixture_wasm() -> PathBuf {
    build_guest("kiqi-conformance-guest", "kiqi_conformance_guest.wasm")
}

fn todo_wasm() -> PathBuf {
    build_guest("kiqi-todo-guest", "kiqi_todo_guest.wasm")
}

/// A `SessionRead` with a fixed branch, so the todo guest has something to
/// restore from.
struct FixedSession {
    branch: serde_json::Value,
}

impl kiqi_plugin::SessionRead for FixedSession {
    fn active_tools(&self) -> Vec<String> {
        Vec::new()
    }
    fn system_prompt(&self) -> String {
        String::new()
    }
    fn thinking_level(&self) -> String {
        String::new()
    }
    fn cli_flag(&self, _name: &str) -> Option<String> {
        None
    }
    fn session_branch(&self) -> serde_json::Value {
        self.branch.clone()
    }
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

/// The first real wasm plugin: `rpi-todo`. It proves the structured `details`
/// state channel and `session-read` (restoring from the branch) end to end.
#[test]
fn todo_guest_restores_from_the_session_branch() {
    let wasm = todo_wasm();
    let plugin = WasmPlugin::load("rpi-todo", &wasm).expect("load the todo component");
    assert_eq!(plugin.tools()[0].name, "todo");

    let session = FixedSession {
        branch: serde_json::json!({
            "entries": [{
                "message": {
                    "role": "toolResult",
                    "toolName": "todo",
                    "details": {
                        "todos": [{"id": 1, "text": "from the branch", "done": false}],
                        "nextId": 2,
                    },
                }
            }]
        }),
    };
    let log = NullLog;
    let caps = HostCaps {
        session: Some(&session),
        control: None,
        inject: None,
        ui: None,
        log: &log,
    };

    // A session (re)start restores the list from the branch through
    // `session-read`.
    let action = plugin.on_event(
        &Event::new(EventTag::SessionStart, serde_json::Value::Null),
        &caps,
    );
    assert_eq!(action, EventAction::Continue);

    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    let listed = plugin
        .call("todo", &serde_json::json!({"action": "list"}), &caps, &sink)
        .expect("list");
    let text = match &listed.blocks[0] {
        Block::Text(text) => text.clone(),
        other => panic!("unexpected block {other:?}"),
    };
    assert!(text.contains("from the branch"), "restored list: {text}");
    assert_eq!(
        listed.details["todos"][0]["text"],
        serde_json::json!("from the branch"),
        "the details channel carries the state"
    );

    // Mutations update that state and report it back.
    let added = plugin
        .call(
            "todo",
            &serde_json::json!({"action": "add", "text": "new one"}),
            &caps,
            &sink,
        )
        .expect("add");
    assert_eq!(added.details["action"], serde_json::json!("add"));
    assert!(
        added.details["todos"]
            .as_array()
            .unwrap()
            .iter()
            .any(|todo| todo["text"] == "new one"),
        "the added task is in the details: {}",
        added.details
    );
}
