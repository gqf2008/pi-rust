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
use kiqi_plugin_host::{Grants, NetHttpGrant, WasmPlugin};

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
    assert!(tools.iter().any(|tool| tool.name == "conformance"));
    assert!(tools.iter().any(|tool| tool.name == "fetch"));

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

/// The `net-http` capability is gated by the user's grant, and the host checks
/// the request's host against the allow-list.
#[test]
fn net_http_is_gated_by_the_users_grant() {
    let wasm = fixture_wasm();
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    let request = serde_json::json!({"url": "http://127.0.0.1:9/"});

    // No grant: the fetch tool degrades and reports the deny.
    let plugin =
        WasmPlugin::load_with_grants("conformance", &wasm, Grants::default()).expect("load");
    let denied = plugin.call("fetch", &request, &caps, &sink).expect("call");
    assert!(denied.is_error, "ungranted net-http is an error result");
    assert!(
        text_of(&denied).contains("not granted"),
        "got {}",
        text_of(&denied)
    );

    // A grant whose allow-list does not cover the host is still denied.
    let wrong_host = Grants {
        net_http: Some(NetHttpGrant {
            hosts: vec!["api.example.com".into()],
        }),
        ..Default::default()
    };
    let plugin = WasmPlugin::load_with_grants("conformance", &wasm, wrong_host).expect("load");
    let denied = plugin.call("fetch", &request, &caps, &sink).expect("call");
    assert!(denied.is_error);
    assert!(
        text_of(&denied).contains("not granted"),
        "got {}",
        text_of(&denied)
    );

    // With the host allowed, the same call reaches a local server.
    let (url, server) = spawn_http_server();
    let granted = Grants {
        net_http: Some(NetHttpGrant {
            hosts: vec!["127.0.0.1".into()],
        }),
        ..Default::default()
    };
    let plugin = WasmPlugin::load_with_grants("conformance", &wasm, granted).expect("load");
    let allowed = plugin
        .call("fetch", &serde_json::json!({"url": url}), &caps, &sink)
        .expect("call");
    assert!(!allowed.is_error, "granted fetch succeeds: {allowed:?}");
    assert!(
        text_of(&allowed).contains("hello from the host"),
        "got {}",
        text_of(&allowed)
    );
    server.join().unwrap();
}

fn text_of(outcome: &kiqi_plugin::ToolOutcome) -> String {
    outcome
        .blocks
        .iter()
        .map(|block| match block {
            Block::Text(text) => text.clone(),
            Block::Json(value) => value.to_string(),
        })
        .collect::<Vec<_>>()
        .join("")
}

/// One-shot HTTP server: replies to the first request with a fixed body.
fn spawn_http_server() -> (String, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a local port");
    let addr = listener.local_addr().expect("local addr");
    let handle = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer);
            let body = "hello from the host";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{addr}/"), handle)
}

fn langfuse_wasm() -> PathBuf {
    build_guest("kiqi-langfuse-guest", "kiqi_langfuse_guest.wasm")
}

/// A server that reads one request and hands the caller its body.
fn spawn_capturing_server() -> (String, std::thread::JoinHandle<String>) {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a local port");
    let addr = listener.local_addr().expect("local addr");
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = match listener.accept() {
            Ok(pair) => pair,
            Err(_) => return String::new(),
        };
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut header_end = None;
        while header_end.is_none() {
            let read = stream.read(&mut chunk).unwrap_or(0);
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            header_end = buffer.windows(4).position(|window| window == b"\r\n\r\n");
        }
        let Some(header_end) = header_end else {
            return String::new();
        };
        let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        let body_start = header_end + 4;
        while buffer.len() < body_start + length {
            let read = stream.read(&mut chunk).unwrap_or(0);
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        let body = String::from_utf8_lossy(&buffer[body_start..]).to_string();
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
        body
    });
    (format!("http://{addr}"), handle)
}

/// langfuse, representative subset: build one turn trace from events and flush
/// it to the OTLP endpoint through `net-http`.
#[test]
fn langfuse_subset_builds_a_trace_and_flushes_it_over_net_http() {
    let wasm = langfuse_wasm();
    let (base_url, server) = spawn_capturing_server();
    let grants = Grants {
        net_http: Some(NetHttpGrant {
            hosts: vec!["127.0.0.1".into()],
        }),
        ..Default::default()
    };
    let plugin = WasmPlugin::load_with_grants("rpi-langfuse", &wasm, grants).expect("load");
    assert!(plugin
        .tools()
        .iter()
        .any(|tool| tool.name == "langfuse_config"));

    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    let configured = plugin
        .call(
            "langfuse_config",
            &serde_json::json!({
                "baseUrl": base_url,
                "publicKey": "pk-lf-test",
                "secretKey": "sk-lf-test",
            }),
            &caps,
            &sink,
        )
        .expect("configure");
    assert!(!configured.is_error, "{configured:?}");

    // One turn: root -> generation -> assistant message -> shutdown(flush).
    let dispatch = |tag, payload| {
        plugin.on_event(&Event::new(tag, payload), &caps);
    };
    dispatch(EventTag::SessionStart, serde_json::Value::Null);
    dispatch(
        EventTag::BeforeAgentStart,
        serde_json::json!({
            "prompt": "hello",
            "sessionId": "sess-1",
            "host": { "name": "kiqi", "version": "9" },
        }),
    );
    dispatch(
        EventTag::BeforeProviderRequest,
        serde_json::json!({ "model": "test-model" }),
    );
    dispatch(
        EventTag::MessageEnd,
        serde_json::json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": "hi there" }],
            "usage": { "input": 3, "output": 4 },
            "stopReason": "endTurn",
        }),
    );
    dispatch(EventTag::SessionShutdown, serde_json::Value::Null);

    let body = server.join().unwrap();
    assert!(body.contains("Conversational Turn"), "root span: {body}");
    assert!(
        body.contains("kiqi Turn"),
        "trace name from the host: {body}"
    );
    assert!(body.contains("LLM Call"), "generation span: {body}");
    assert!(body.contains("sess-1"), "session id: {body}");
    assert!(body.contains("test-model"), "model: {body}");
}
