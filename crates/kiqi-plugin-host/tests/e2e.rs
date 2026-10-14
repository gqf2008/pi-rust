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
    ProgressSink, Verdict,
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
    assert!(
        text.contains("fs=") && !text.contains("fs=conformance"),
        "fs is not granted here, so it must report denied: {text}"
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

/// A `UiApi` that records what the guest asked for and answers dialogs.
#[derive(Default)]
struct RecordingUi {
    dialogs: Mutex<Vec<serde_json::Value>>,
    statuses: Mutex<Vec<serde_json::Value>>,
}

impl kiqi_plugin::UiApi for RecordingUi {
    fn dialog(
        &self,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, kiqi_plugin::PluginError> {
        self.dialogs.lock().unwrap().push(request.clone());
        Ok(serde_json::json!({ "answer": "42" }))
    }
    fn set_status(&self, update: &serde_json::Value) -> Result<(), kiqi_plugin::PluginError> {
        self.statuses.lock().unwrap().push(update.clone());
        Ok(())
    }
    fn set_editor_text(&self, _edit: &serde_json::Value) -> Result<(), kiqi_plugin::PluginError> {
        Ok(())
    }
    fn component(
        &self,
        _name: &str,
        _payload: &serde_json::Value,
    ) -> Result<serde_json::Value, kiqi_plugin::PluginError> {
        Err(kiqi_plugin::PluginError::Failed(
            "no components here".to_string(),
        ))
    }
}

/// Gate §6.3.2: every contribution surface the ABI has must actually be
/// reachable, and the host must be the one asked.
#[test]
fn conformance_surface_is_reachable() {
    let wasm = fixture_wasm();
    let plugin =
        WasmPlugin::load_with_grants("conformance", &wasm, Grants::default()).expect("load");
    let ui = RecordingUi::default();
    let log = NullLog;
    let caps = HostCaps {
        session: None,
        control: None,
        inject: None,
        ui: Some(&ui),
        log: &log,
    };
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    // tools
    let tools: Vec<String> = plugin.tools().into_iter().map(|tool| tool.name).collect();
    for expected in ["conformance", "fetch", "dialog"] {
        assert!(
            tools.iter().any(|tool| tool == expected),
            "tool {expected}: {tools:?}"
        );
    }

    // commands, flags, shortcuts
    assert_eq!(plugin.commands()[0].name, "conformance");
    assert_eq!(
        plugin.run_command("conformance", "", &caps).unwrap(),
        "conformance command ran"
    );
    assert_eq!(plugin.flags()[0].name, "conformance-flag");
    assert_eq!(plugin.shortcuts()[0].key, "f8");

    // system-prompt transform
    let replaced = plugin
        .before_agent_start(&serde_json::json!("SYS"), &caps)
        .expect("the guest replaces the prompt");
    assert!(
        replaced.contains("SYS") && replaced.contains("[conformance]"),
        "got {replaced}"
    );

    // probe gate
    assert_eq!(plugin.probe_points(), vec!["before-tool-call".to_string()]);
    assert_eq!(
        plugin.probe("before-tool-call", &serde_json::json!({}), &caps),
        Verdict::Continue
    );
    assert!(matches!(
        plugin.probe(
            "before-tool-call",
            &serde_json::json!({ "x": "block" }),
            &caps
        ),
        Verdict::Block { .. }
    ));

    // ui.dialog round trip: the host really answered the guest.
    let answer = plugin
        .call(
            "dialog",
            &serde_json::json!({ "request": "pick one" }),
            &caps,
            &sink,
        )
        .expect("dialog tool");
    assert!(!answer.is_error, "{answer:?}");
    assert!(text_of(&answer).contains("42"), "got {}", text_of(&answer));
    assert_eq!(ui.dialogs.lock().unwrap().len(), 1);

    // With a granted UI, `ui.set-status` now succeeds instead of being denied.
    let outcome = plugin
        .call("conformance", &serde_json::json!({}), &caps, &sink)
        .expect("call");
    assert!(
        text_of(&outcome).contains("ui=granted"),
        "got {}",
        text_of(&outcome)
    );
    assert!(!ui.statuses.lock().unwrap().is_empty());
}

/// Gate §6.3.7: a plugin that hangs or panics must be contained, not take the
/// host down.
#[test]
fn a_spinning_guest_is_metered_and_a_panicking_one_traps() {
    let wasm = fixture_wasm();
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    // A fuel budget turns an infinite loop into a trapped call, not a hang.
    let bounded = WasmPlugin::load_with_options(
        "conformance",
        &wasm,
        Grants::default(),
        kiqi_plugin_host::Options {
            fuel: Some(5_000_000),
            ..Default::default()
        },
    )
    .expect("load");
    let spin = bounded
        .call("spin", &serde_json::json!({}), &caps, &sink)
        .unwrap_err();
    assert!(!spin.to_string().is_empty(), "a metered spin traps");

    // A panicking guest traps too; the call is an error, not a process abort.
    let panicking =
        WasmPlugin::load_with_grants("conformance", &wasm, Grants::default()).expect("load");
    let panic = panicking
        .call("panic", &serde_json::json!({}), &caps, &sink)
        .unwrap_err();
    assert!(!panic.to_string().is_empty());

    // The host is unaffected: another plugin still loads and lists its tools.
    let survivor =
        WasmPlugin::load_with_grants("conformance", &wasm, Grants::default()).expect("load");
    assert!(survivor
        .tools()
        .iter()
        .any(|tool| tool.name == "conformance"));
}

fn ask_user_wasm() -> PathBuf {
    build_guest("kiqi-ask-user-guest", "kiqi_ask_user_guest.wasm")
}

/// How the scripted host UI answers the next poll.
#[derive(Clone)]
enum DialogMode {
    Answer(serde_json::Value),
    Cancel,
    Pending,
}

/// A `UiApi` that speaks the ask_user dialog protocol: `open` -> pending, then
/// `poll` returns whatever the mode says.
struct ScriptedUi {
    mode: Mutex<DialogMode>,
    ops: Mutex<Vec<String>>,
}

impl ScriptedUi {
    fn new(mode: DialogMode) -> Self {
        Self {
            mode: Mutex::new(mode),
            ops: Mutex::new(Vec::new()),
        }
    }

    fn ops(&self) -> Vec<String> {
        self.ops.lock().unwrap().clone()
    }
}

impl kiqi_plugin::UiApi for ScriptedUi {
    fn dialog(
        &self,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, kiqi_plugin::PluginError> {
        let op = request
            .get("op")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        self.ops.lock().unwrap().push(op.clone());
        Ok(match op.as_str() {
            "poll" => match &*self.mode.lock().unwrap() {
                DialogMode::Answer(answer) => {
                    serde_json::json!({ "status": "answered", "answer": answer })
                }
                DialogMode::Cancel => serde_json::json!({ "status": "cancelled" }),
                DialogMode::Pending => serde_json::json!({ "status": "pending" }),
            },
            "cancel" => serde_json::json!({ "status": "cancelled" }),
            // open (and anything else) is pending until polled.
            _ => serde_json::json!({ "status": "pending" }),
        })
    }

    fn set_status(&self, _update: &serde_json::Value) -> Result<(), kiqi_plugin::PluginError> {
        Ok(())
    }
    fn set_editor_text(&self, _edit: &serde_json::Value) -> Result<(), kiqi_plugin::PluginError> {
        Ok(())
    }
    fn component(
        &self,
        _name: &str,
        _payload: &serde_json::Value,
    ) -> Result<serde_json::Value, kiqi_plugin::PluginError> {
        Err(kiqi_plugin::PluginError::Failed("none".to_string()))
    }
}

/// `rpi-ask-user` on wasm: a question round-trips through the host dialog and
/// the answer rides the `details` channel.
#[test]
fn ask_user_round_trips_through_the_host_dialog() {
    let wasm = ask_user_wasm();
    let plugin =
        WasmPlugin::load_with_grants("rpi-ask-user", &wasm, Grants::default()).expect("load");
    assert!(plugin.tools().iter().any(|tool| tool.name == "ask_user"));

    let ui = ScriptedUi::new(DialogMode::Answer(serde_json::json!({ "text": "7897" })));
    let log = NullLog;
    let caps = HostCaps {
        session: None,
        control: None,
        inject: None,
        ui: Some(&ui),
        log: &log,
    };
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    let outcome = plugin
        .call(
            "ask_user",
            &serde_json::json!({ "question": "Which port?" }),
            &caps,
            &sink,
        )
        .expect("ask");
    assert!(!outcome.is_error, "{outcome:?}");
    assert!(
        text_of(&outcome).contains("Which port?: 7897"),
        "got {}",
        text_of(&outcome)
    );
    assert_eq!(
        outcome.details["answers"][0]["text"],
        serde_json::json!("7897")
    );
    let ops = ui.ops();
    assert!(ops.contains(&"open".to_string()), "ops: {ops:?}");
    assert!(ops.contains(&"poll".to_string()), "ops: {ops:?}");

    // A confirm prompt maps a yes to `Confirmed`.
    let ui = ScriptedUi::new(DialogMode::Answer(serde_json::json!({ "values": ["Yes"] })));
    let caps = HostCaps {
        session: None,
        control: None,
        inject: None,
        ui: Some(&ui),
        log: &log,
    };
    let confirm = plugin
        .call(
            "ask_user",
            &serde_json::json!({ "type": "confirm", "summary": "Deploy?" }),
            &caps,
            &sink,
        )
        .expect("confirm");
    assert!(!confirm.is_error);
    assert!(
        text_of(&confirm).contains("Confirmed: Deploy?"),
        "got {}",
        text_of(&confirm)
    );
}

/// Gate §6.3.4: cancellation reaches the plugin instead of being mistaken for
/// an answer.
#[test]
fn ask_user_reports_cancellation() {
    let wasm = ask_user_wasm();
    let plugin =
        WasmPlugin::load_with_grants("rpi-ask-user", &wasm, Grants::default()).expect("load");
    let ui = ScriptedUi::new(DialogMode::Cancel);
    let log = NullLog;
    let caps = HostCaps {
        session: None,
        control: None,
        inject: None,
        ui: Some(&ui),
        log: &log,
    };
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    let outcome = plugin
        .call(
            "ask_user",
            &serde_json::json!({ "question": "Which port?" }),
            &caps,
            &sink,
        )
        .expect("ask");
    assert!(outcome.is_error, "a cancelled ask is an error result");
    assert!(
        text_of(&outcome).contains("cancelled"),
        "got {}",
        text_of(&outcome)
    );
}

/// Gate §6.3.4: a `timeout` closes the dialog and fails the ask rather than
/// waiting forever.
#[test]
fn ask_user_honours_timeout() {
    let wasm = ask_user_wasm();
    let plugin =
        WasmPlugin::load_with_grants("rpi-ask-user", &wasm, Grants::default()).expect("load");
    let ui = ScriptedUi::new(DialogMode::Pending);
    let log = NullLog;
    let caps = HostCaps {
        session: None,
        control: None,
        inject: None,
        ui: Some(&ui),
        log: &log,
    };
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    let outcome = plugin
        .call(
            "ask_user",
            &serde_json::json!({ "question": "Which port?", "timeout": 1 }),
            &caps,
            &sink,
        )
        .expect("ask");
    assert!(outcome.is_error);
    assert!(
        text_of(&outcome).contains("timed out"),
        "got {}",
        text_of(&outcome)
    );
    assert!(
        ui.ops().contains(&"cancel".to_string()),
        "a timeout must cancel the dialog: {:?}",
        ui.ops()
    );
}

/// `ki plugin install` shows the user what a component declares. Only the
/// grantable interfaces count; `log`/`progress`/`ui` are not gated.
#[test]
fn declared_capabilities_reads_the_components_imports() {
    let wasm = fixture_wasm();
    let caps = kiqi_plugin_host::declared_capabilities(&wasm).expect("read component imports");
    assert_eq!(
        caps,
        vec![
            "exec".to_string(),
            "fs".to_string(),
            "net-http".to_string(),
            "net-sockets".to_string(),
        ],
        "got {caps:?}"
    );
}

/// Load the fixture from a scratch directory, so `fs`/`exec` write into a temp
/// data directory instead of the build output.
fn load_fixture_in_temp(grants: Grants) -> (tempfile::TempDir, WasmPlugin) {
    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = dir.path().join("plugin.wasm");
    std::fs::copy(fixture_wasm(), &wasm).expect("copy the fixture next to a writable data dir");
    let plugin = WasmPlugin::load_with_grants("conformance", &wasm, grants).expect("load");
    (dir, plugin)
}

fn call_main_tool(
    plugin: &WasmPlugin,
    arguments: serde_json::Value,
) -> Result<kiqi_plugin::ToolOutcome, PluginError> {
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    plugin.call("conformance", &arguments, &caps, &sink)
}

/// `fs` is a real capability, and the jail is its policy: granted, the guest
/// round-trips a file inside its own data directory.
#[test]
fn fs_is_granted_and_jailed_to_the_plugins_data_directory() {
    let (dir, plugin) = load_fixture_in_temp(Grants {
        fs: true,
        ..Grants::default()
    });

    let outcome = call_main_tool(&plugin, serde_json::json!({})).expect("call");
    assert!(
        text_of(&outcome).contains("fs=conformance"),
        "the write/read round trip must succeed: {}",
        text_of(&outcome)
    );
    assert!(
        dir.path().join("data/conformance/probe.txt").is_file(),
        "the file lands inside the plugin's own data directory"
    );
}

/// `exec` runs an allow-listed bare command and returns its output; a command
/// outside the grant (or no grant at all) is denied, not silently skipped.
#[test]
fn exec_runs_only_allow_listed_commands() {
    use kiqi_plugin_host::ExecGrant;

    let (_dir, plugin) = load_fixture_in_temp(Grants {
        exec: Some(ExecGrant {
            commands: vec!["echo".to_string()],
            ..ExecGrant::default()
        }),
        ..Grants::default()
    });
    let allowed = call_main_tool(&plugin, serde_json::json!({})).expect("call");
    assert!(
        text_of(&allowed).contains("exec=conformance"),
        "echo must run and its stdout come back: {}",
        text_of(&allowed)
    );

    // Same plugin, a grant that does not include `echo`.
    let (_dir, plugin) = load_fixture_in_temp(Grants {
        exec: Some(ExecGrant {
            commands: vec!["ls".to_string()],
            ..ExecGrant::default()
        }),
        ..Grants::default()
    });
    let denied = call_main_tool(&plugin, serde_json::json!({})).expect("call");
    assert!(
        text_of(&denied).contains("exec=exec of `echo` is not granted"),
        "an unlisted command must report the deny reason: {}",
        text_of(&denied)
    );
}

/// Load the fixture with an explicit project directory: the data dir sits in
/// `dir`, the cwd scope `project` resolves against `project`.
fn load_fixture_with_project(grants: Grants) -> (tempfile::TempDir, tempfile::TempDir, WasmPlugin) {
    let dir = tempfile::tempdir().expect("tempdir");
    let project = tempfile::tempdir().expect("project tempdir");
    let wasm = dir.path().join("plugin.wasm");
    std::fs::copy(fixture_wasm(), &wasm).expect("copy the fixture next to a writable data dir");
    let plugin = WasmPlugin::load_with_options(
        "conformance",
        &wasm,
        grants,
        kiqi_plugin_host::Options {
            project_dir: Some(project.path().to_path_buf()),
            ..kiqi_plugin_host::Options::default()
        },
    )
    .expect("load");
    (dir, project, plugin)
}

/// The grant's cwd scope decides where the child starts: the default is the
/// plugin's own data directory, and a guest-supplied relative cwd resolves
/// inside it.
#[test]
fn exec_cwd_defaults_to_the_plugin_data_directory() {
    use kiqi_plugin_host::ExecGrant;

    let (dir, plugin) = load_fixture_in_temp(Grants {
        exec: Some(ExecGrant {
            commands: vec!["sh".to_string()],
            ..ExecGrant::default()
        }),
        ..Grants::default()
    });
    let outcome = call_main_tool(
        &plugin,
        serde_json::json!({ "exec": { "cmd": "sh", "args": ["-c", "touch marker"] } }),
    )
    .expect("call");
    assert!(
        dir.path().join("data/marker").is_file(),
        "no cwd: the child starts in the data directory: {}",
        text_of(&outcome)
    );

    let outcome = call_main_tool(
        &plugin,
        serde_json::json!({ "exec": { "cmd": "sh", "args": ["-c", "touch marker"], "cwd": "sub" } }),
    )
    .expect("call");
    assert!(
        dir.path().join("data/sub/marker").is_file(),
        "a relative cwd descends inside the data directory: {}",
        text_of(&outcome)
    );

    // `.` is the root itself: no final component to check, and it must not be
    // refused by the symlink guard comparing the root's parent against it.
    let outcome = call_main_tool(
        &plugin,
        serde_json::json!({ "exec": { "cmd": "sh", "args": ["-c", "touch dot"], "cwd": "." } }),
    )
    .expect("call");
    assert!(
        dir.path().join("data/dot").is_file(),
        "cwd `.` means the scope root: {}",
        text_of(&outcome)
    );
}

/// A cwd the guest supplies may not walk out of the granted root, whether by
/// `..` or by an absolute path.
#[test]
fn exec_cwd_cannot_escape_its_scope_root() {
    use kiqi_plugin_host::ExecGrant;

    let (dir, plugin) = load_fixture_in_temp(Grants {
        exec: Some(ExecGrant {
            commands: vec!["sh".to_string()],
            ..ExecGrant::default()
        }),
        ..Grants::default()
    });
    for cwd in ["../outside", "/tmp"] {
        let outcome = call_main_tool(
            &plugin,
            serde_json::json!({ "exec": { "cmd": "sh", "args": ["-c", "touch escaped"], "cwd": cwd } }),
        )
        .expect("call");
        assert!(
            text_of(&outcome).contains("exec=failed:"),
            "cwd {cwd:?} is refused: {}",
            text_of(&outcome)
        );
    }
    assert!(!dir.path().join("outside").exists());
}

/// A symlink inside the granted root must not be followed out of it. The exec
/// path runs the same lexical + symlink checks as `fs` — a symlink left in the
/// data directory would otherwise be followed as the child's cwd.
#[cfg(unix)]
#[test]
fn exec_cwd_refuses_a_symlinked_parent() {
    use kiqi_plugin_host::ExecGrant;

    let (dir, plugin) = load_fixture_in_temp(Grants {
        exec: Some(ExecGrant {
            commands: vec!["sh".to_string()],
            ..ExecGrant::default()
        }),
        ..Grants::default()
    });
    let outside = tempfile::tempdir().expect("outside tempdir");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).expect("create the data dir");
    std::os::unix::fs::symlink(outside.path(), data.join("link")).expect("plant the symlink");

    let outcome = call_main_tool(
        &plugin,
        serde_json::json!({ "exec": { "cmd": "sh", "args": ["-c", "touch escaped"], "cwd": "link" } }),
    )
    .expect("call");
    assert!(
        text_of(&outcome).contains("exec=failed:"),
        "a symlinked cwd is refused: {}",
        text_of(&outcome)
    );
    assert!(!outside.path().join("escaped").exists());

    // A symlinked MIDDLE component: the refusal must be clean — the jail check
    // verifies the existing prefix before creating anything, so the refused
    // call must not leave `outside/sub` behind.
    let outcome = call_main_tool(
        &plugin,
        serde_json::json!({ "exec": { "cmd": "sh", "args": ["-c", "true"], "cwd": "link/sub" } }),
    )
    .expect("call");
    assert!(
        text_of(&outcome).contains("exec=failed:"),
        "a symlinked middle component is refused: {}",
        text_of(&outcome)
    );
    assert!(
        !outside.path().join("sub").exists(),
        "a refused cwd must not create directories outside the jail"
    );
}

/// With the `project` cwd scope, the child starts in the project directory —
/// what a diagnostic plugin needs to run `cargo`/`clippy` on the user's tree.
#[test]
fn exec_cwd_project_scope_runs_in_the_project_directory() {
    use kiqi_plugin_host::{ExecCwd, ExecGrant};

    let (dir, project, plugin) = load_fixture_with_project(Grants {
        exec: Some(ExecGrant {
            commands: vec!["sh".to_string()],
            cwd: ExecCwd::Project,
            ..ExecGrant::default()
        }),
        ..Grants::default()
    });
    let outcome = call_main_tool(
        &plugin,
        serde_json::json!({ "exec": { "cmd": "sh", "args": ["-c", "touch marker"] } }),
    )
    .expect("call");
    assert!(
        project.path().join("marker").is_file(),
        "the child starts in the project directory: {}",
        text_of(&outcome)
    );
    assert!(
        !dir.path().join("data/marker").exists(),
        "and not in the plugin's data directory"
    );

    let outcome = call_main_tool(
        &plugin,
        serde_json::json!({ "exec": { "cmd": "sh", "args": ["-c", "touch marker"], "cwd": "sub" } }),
    )
    .expect("call");
    assert!(
        project.path().join("sub/marker").is_file(),
        "a relative cwd descends inside the project directory: {}",
        text_of(&outcome)
    );

    // The project root is still a root: `..` out of it is refused.
    let outcome = call_main_tool(
        &plugin,
        serde_json::json!({ "exec": { "cmd": "sh", "args": ["-c", "true"], "cwd": ".." } }),
    )
    .expect("call");
    assert!(
        text_of(&outcome).contains("exec=failed:"),
        "the project scope is still a jail: {}",
        text_of(&outcome)
    );
}

/// The time budget rides with the grant: a command that overruns it is killed
/// and reported as timed out, long before its own duration.
#[test]
fn exec_times_out_at_the_grants_budget() {
    use kiqi_plugin_host::ExecGrant;

    let (_dir, plugin) = load_fixture_in_temp(Grants {
        exec: Some(ExecGrant {
            commands: vec!["sh".to_string()],
            timeout_seconds: 1,
            ..ExecGrant::default()
        }),
        ..Grants::default()
    });
    let start = std::time::Instant::now();
    let outcome = call_main_tool(
        &plugin,
        serde_json::json!({ "exec": { "cmd": "sh", "args": ["-c", "sleep 10"] } }),
    )
    .expect("call");
    let elapsed = start.elapsed();
    assert!(
        text_of(&outcome).contains("timedout"),
        "a command past its budget is reported as timed out: {}",
        text_of(&outcome)
    );
    assert!(
        elapsed < std::time::Duration::from_secs(8),
        "the 1s budget fires long before the 10s command would finish ({elapsed:?})"
    );
}

/// `net-sockets` is allow-listed per host, and the host holds the socket: the
/// guest only ever sees a handle.
#[test]
fn net_sockets_connect_through_a_host_allow_list() {
    use kiqi_plugin_host::NetSocketsGrant;
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a loopback listener");
    let address = listener.local_addr().unwrap().to_string();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buffer = [0u8; 64];
        let read = stream.read(&mut buffer).expect("read the ping");
        stream.write_all(&buffer[..read]).expect("echo it back");
    });

    let (_dir, plugin) = load_fixture_in_temp(Grants {
        net_sockets: Some(NetSocketsGrant {
            hosts: vec!["127.0.0.1".to_string()],
        }),
        ..Grants::default()
    });
    let outcome = call_main_tool(&plugin, serde_json::json!({ "address": address })).expect("call");
    server.join().expect("server thread");
    assert!(
        text_of(&outcome).contains("socket=sent=4 recv=ping"),
        "the loopback round trip must succeed: {}",
        text_of(&outcome)
    );

    // A host outside the grant is denied before any connection is attempted.
    let (_dir, plugin) = load_fixture_in_temp(Grants {
        net_sockets: Some(NetSocketsGrant {
            hosts: vec!["example.com".to_string()],
        }),
        ..Grants::default()
    });
    let denied =
        call_main_tool(&plugin, serde_json::json!({ "address": "127.0.0.1:9" })).expect("call");
    assert!(
        text_of(&denied).contains("socket=net-sockets to `127.0.0.1` is not granted"),
        "the allow-list must be consulted: {}",
        text_of(&denied)
    );
}

/// A component's self-declared `id` is reported, never trusted: the directory it
/// was installed under stays the identity and the authorization key (grants are
/// read from that directory).
#[test]
fn a_declared_id_does_not_override_the_install_directory() {
    let plugin = WasmPlugin::load("installed-as", &fixture_wasm()).expect("load the component");
    assert_eq!(
        plugin.id(),
        "installed-as",
        "the conformance guest declares `conformance`, but the load id must win"
    );
}

/// The advertised-as-working list is the host's honesty contract: `install`
/// names every declared capability that is not in it.
#[test]
fn only_enforced_capabilities_count_as_working() {
    // Every grantable capability now has an implementation, so `install` has
    // nothing to warn about for a component that declares all four.
    for enforced in ["net-http", "fs", "exec", "net-sockets"] {
        assert!(
            kiqi_plugin_host::capability_is_enforced(enforced),
            "`{enforced}` is implemented and must be advertised as working"
        );
    }
    assert!(!kiqi_plugin_host::capability_is_enforced(
        "no-such-capability"
    ));
}

/// A granted host must not be able to bounce the request somewhere the user
/// never granted: a redirect is followed only when its own host is listed too,
/// otherwise the 3xx is handed back.
#[test]
fn net_http_does_not_follow_a_redirect_to_an_ungranted_host() {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a loopback server");
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buffer = [0u8; 1024];
        let _ = stream.read(&mut buffer);
        // `example.invalid` is never allow-listed here, so the client must stop
        // rather than connect.
        let response = "HTTP/1.1 302 Found\r\n\
                        Location: http://example.invalid/evil\r\n\
                        Content-Length: 0\r\n\
                        Connection: close\r\n\r\n";
        let _ = stream.write_all(response.as_bytes());
    });

    let (_dir, plugin) = load_fixture_in_temp(Grants {
        net_http: Some(NetHttpGrant {
            hosts: vec!["127.0.0.1".to_string()],
        }),
        ..Grants::default()
    });
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    let outcome = plugin
        .call(
            "fetch",
            &serde_json::json!({ "url": format!("http://127.0.0.1:{port}/") }),
            &caps,
            &sink,
        )
        .expect("fetch");
    server.join().expect("server thread");

    let body = text_of(&outcome);
    let parsed: serde_json::Value =
        serde_json::from_str(&body).expect("the capability answers JSON");
    assert_eq!(
        parsed["status"], 302,
        "the redirect must not be followed: {body}"
    );
}

/// A guest cannot take the machine's memory: a request past the cap traps on
/// the `memory.grow`, and one under it still succeeds (so the cap is not a
/// blanket disable).
#[test]
fn guest_memory_is_capped() {
    let wasm = fixture_wasm();
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    let bounded = WasmPlugin::load_with_options(
        "conformance",
        &wasm,
        Grants::default(),
        kiqi_plugin_host::Options {
            max_memory_bytes: Some(32 * 1024 * 1024),
            ..Default::default()
        },
    )
    .expect("load");

    // Under the cap.
    let small = bounded
        .call(
            "alloc",
            &serde_json::json!({ "bytes": 1024 * 1024 }),
            &caps,
            &sink,
        )
        .expect("an allocation under the cap succeeds");
    assert!(
        text_of(&small).contains("allocated 1048576"),
        "got {}",
        text_of(&small)
    );

    // Over it: the grow is refused and the guest traps.
    let over = bounded
        .call(
            "alloc",
            &serde_json::json!({ "bytes": 128 * 1024 * 1024 }),
            &caps,
            &sink,
        )
        .unwrap_err();
    assert!(!over.to_string().is_empty(), "an over-cap allocation traps");
}

/// A granted host must not be able to make the *host* allocate without bound:
/// an oversized response body is refused rather than buffered.
#[test]
fn net_http_refuses_an_oversized_response_body() {
    use std::io::{Read, Write};

    const BODY: usize = 9 * 1024 * 1024;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a loopback server");
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buffer = [0u8; 1024];
        let _ = stream.read(&mut buffer);
        let _ = stream.write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {BODY}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        );
        // The client stops reading at its cap, so a broken pipe here is expected.
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..(BODY / chunk.len()) {
            if stream.write_all(&chunk).is_err() {
                break;
            }
        }
    });

    let (_dir, plugin) = load_fixture_in_temp(Grants {
        net_http: Some(NetHttpGrant {
            hosts: vec!["127.0.0.1".to_string()],
        }),
        ..Grants::default()
    });
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    let outcome = plugin
        .call(
            "fetch",
            &serde_json::json!({ "url": format!("http://127.0.0.1:{port}/") }),
            &caps,
            &sink,
        )
        .expect("fetch");
    let _ = server.join();

    assert!(
        outcome.is_error,
        "an oversized body is a capability failure"
    );
    let body = text_of(&outcome);
    assert!(
        body.contains("exceeds") && body.contains("bytes"),
        "the reason must name the cap: {body}"
    );
}

/// A command that leaves a background child must not hang the call: the child's
/// group is reaped with it, so the output pipes close. Run on its own thread so a
/// regression fails an assertion instead of hanging the test binary.
#[cfg(unix)]
#[test]
fn exec_does_not_hang_when_the_command_leaves_a_background_child() {
    use kiqi_plugin_host::ExecGrant;

    // Build outside the timed window: `fixture_wasm()` runs `cargo build`, and
    // with the whole suite building guests in parallel that can take longer than
    // the call is allowed to take.
    let _ = fixture_wasm();

    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (_dir, plugin) = load_fixture_in_temp(Grants {
            exec: Some(ExecGrant {
                commands: vec!["sh".to_string()],
                ..ExecGrant::default()
            }),
            ..Grants::default()
        });
        let outcome = call_main_tool(
            &plugin,
            serde_json::json!({ "exec": { "cmd": "sh", "args": ["-c", "sleep 60 & exit 0"] } }),
        )
        .expect("call");
        let _ = sender.send(text_of(&outcome));
    });

    let text = receiver
        .recv_timeout(std::time::Duration::from_secs(15))
        .expect("exec must return even though a background child held the pipe");
    assert!(
        text.contains("exec=") && !text.contains("exec=exec of") && !text.contains("exec=failed:"),
        "the command ran (its stdout is empty) rather than being denied or failing: {text}"
    );
}

/// `rpi-webfetch` ported to wasm: the tool's own URL policy runs in the guest,
/// and where it may actually go is the user's grant — checked before any network
/// access, so this stays hermetic.
#[test]
fn webfetch_guest_keeps_its_url_policy_and_needs_the_grant() {
    let wasm = build_guest("kiqi-webfetch-guest", "kiqi_webfetch_guest.wasm");
    let plugin = WasmPlugin::load("rpi-webfetch", &wasm).expect("load");
    assert_eq!(plugin.id(), "rpi-webfetch");
    assert!(
        plugin.tools().iter().any(|tool| tool.name == "webfetch"),
        "the tool is declared: {:?}",
        plugin.tools()
    );

    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    // The plugin's own policy, not the host's: localhost and literal private IPs
    // are refused inside the guest.
    let local = plugin
        .call(
            "webfetch",
            &serde_json::json!({ "url": "http://localhost/" }),
            &caps,
            &sink,
        )
        .unwrap_err();
    assert!(local.to_string().contains("local hosts"), "got {local}");
    let private = plugin
        .call(
            "webfetch",
            &serde_json::json!({ "url": "http://127.0.0.1:1/" }),
            &caps,
            &sink,
        )
        .unwrap_err();
    assert!(
        private.to_string().contains("private or local IPs"),
        "got {private}"
    );

    // A public URL passes the policy and is then the grant's to refuse.
    let denied = plugin
        .call(
            "webfetch",
            &serde_json::json!({ "url": "http://example.com/" }),
            &caps,
            &sink,
        )
        .unwrap_err();
    assert!(denied.to_string().contains("denied"), "got {denied}");
}

/// `rpi-lens` ported to wasm: the check runs through the host's `exec`
/// capability with a `project` cwd scope, so `git diff --check` runs against
/// the project directory — and without the grant the tool degrades to a clear
/// deny instead of running.
#[test]
fn rpi_lens_guest_runs_checks_in_the_project_directory() {
    use kiqi_plugin_host::{ExecCwd, ExecGrant};

    // The test needs `git` for real (the fixture init below and the guest's
    // diff check). If it is not installed, say so and skip rather than fail on
    // an environment fact.
    if Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| !status.success())
        .unwrap_or(true)
    {
        eprintln!("skipping rpi_lens_guest_runs_checks_in_the_project_directory: no git on PATH");
        return;
    }

    let wasm = build_guest("kiqi-rpi-lens-guest", "kiqi_rpi_lens_guest.wasm");
    let project = tempfile::tempdir().expect("project tempdir");
    let init = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(project.path())
        .status()
        .expect("git init the project dir");
    assert!(init.success());

    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    // Granted with the project scope: the diff check runs in the project dir.
    let plugin = WasmPlugin::load_with_options(
        "rpi-lens",
        &wasm,
        Grants {
            exec: Some(ExecGrant {
                commands: vec!["git".to_string()],
                cwd: ExecCwd::Project,
                ..ExecGrant::default()
            }),
            ..Grants::default()
        },
        kiqi_plugin_host::Options {
            project_dir: Some(project.path().to_path_buf()),
            ..kiqi_plugin_host::Options::default()
        },
    )
    .expect("load");
    assert_eq!(plugin.id(), "rpi-lens");
    assert!(
        plugin.tools().iter().any(|tool| tool.name == "code_lens"),
        "the tool is declared: {:?}",
        plugin.tools()
    );
    let outcome = plugin
        .call(
            "code_lens",
            &serde_json::json!({ "check": "diff" }),
            &caps,
            &sink,
        )
        .expect("call");
    assert!(
        text_of(&outcome).contains("check=diff exit=0"),
        "git diff --check in a fresh repo passes cleanly: {}",
        text_of(&outcome)
    );

    // Same plugin without the project scope: the child starts in the data
    // directory, which is not a git repository.
    let dir = tempfile::tempdir().expect("install tempdir");
    let scoped_wasm = dir.path().join("plugin.wasm");
    std::fs::copy(&wasm, &scoped_wasm).expect("copy the guest next to a data dir");
    let plugin = WasmPlugin::load_with_grants(
        "rpi-lens",
        &scoped_wasm,
        Grants {
            exec: Some(ExecGrant {
                commands: vec!["git".to_string()],
                ..ExecGrant::default()
            }),
            ..Grants::default()
        },
    )
    .expect("load");
    let outcome = plugin
        .call(
            "code_lens",
            &serde_json::json!({ "check": "diff" }),
            &caps,
            &sink,
        )
        .expect("call");
    assert!(
        text_of(&outcome).contains("check=diff exit=")
            && text_of(&outcome).contains("Not a git repository"),
        "without the project scope the check runs in the data dir (not a repo): {}",
        text_of(&outcome)
    );

    // Without the grant at all, the tool reports the deny and how to fix it.
    // The advice must name BOTH commands the tool can run — `ki plugin grant`
    // replaces the whole exec policy, so advice naming only the denied command
    // would steer the user into revoking the other one.
    let plugin = WasmPlugin::load_with_grants("rpi-lens", &wasm, Grants::default()).expect("load");
    let denied = plugin
        .call(
            "code_lens",
            &serde_json::json!({ "check": "diff" }),
            &caps,
            &sink,
        )
        .unwrap_err();
    let denied = denied.to_string();
    assert!(
        denied.contains("not granted"),
        "the deny names the missing grant: {denied}"
    );
    assert!(
        denied.contains("grant rpi-lens exec --command cargo --command git"),
        "the remediation advice covers every command the tool needs: {denied}"
    );
}

/// A session that records what plan mode asked of it.
#[derive(Default)]
struct PlanSessionHost {
    active: Mutex<Vec<String>>,
    entries: Mutex<Vec<(String, serde_json::Value)>>,
    messages: Mutex<Vec<String>>,
}

impl kiqi_plugin::SessionRead for PlanSessionHost {
    fn active_tools(&self) -> Vec<String> {
        self.active.lock().unwrap().clone()
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
        serde_json::Value::Null
    }
}

impl kiqi_plugin::SessionControl for PlanSessionHost {
    fn set_session_name(&self, _name: &str) -> Result<(), PluginError> {
        Ok(())
    }
    fn set_active_tools(&self, tools: &[String]) -> Result<(), PluginError> {
        *self.active.lock().unwrap() = tools.to_vec();
        Ok(())
    }
    fn set_model(&self, _model: &str) -> Result<(), PluginError> {
        Ok(())
    }
    fn set_thinking_level(&self, _level: &str) -> Result<(), PluginError> {
        Ok(())
    }
    fn compact(&self) -> Result<(), PluginError> {
        Ok(())
    }
    fn new_session(&self) -> Result<(), PluginError> {
        Ok(())
    }
    fn fork(&self) -> Result<(), PluginError> {
        Ok(())
    }
    fn navigate_tree(&self, _target: &str) -> Result<(), PluginError> {
        Ok(())
    }
    fn switch_session(&self, _id: &str) -> Result<(), PluginError> {
        Ok(())
    }
    fn reload(&self) -> Result<(), PluginError> {
        Ok(())
    }
}

impl kiqi_plugin::InjectApi for PlanSessionHost {
    fn send_message(&self, _message: &serde_json::Value) -> Result<(), PluginError> {
        Ok(())
    }
    fn send_user_message(&self, text: &str) -> Result<(), PluginError> {
        self.messages.lock().unwrap().push(text.to_string());
        Ok(())
    }
    fn append_message(&self, _message: &serde_json::Value) -> Result<(), PluginError> {
        Ok(())
    }
    fn append_custom_entry(
        &self,
        custom_type: &str,
        data: &serde_json::Value,
    ) -> Result<(), PluginError> {
        self.entries
            .lock()
            .unwrap()
            .push((custom_type.to_string(), data.clone()));
        Ok(())
    }
}

/// `rpi-plan-mode` ported to wasm: entering swaps the tool set through
/// `session-control`, completing restores it and persists the plan through
/// `inject.append-custom-entry`, and `/plan show` reads it back.
#[test]
fn plan_mode_guest_swaps_tools_and_persists_through_the_host() {
    let wasm = build_guest("kiqi-plan-mode-guest", "kiqi_plan_mode_guest.wasm");
    let plugin = WasmPlugin::load("rpi-plan-mode", &wasm).expect("load");
    assert_eq!(plugin.id(), "rpi-plan-mode");
    assert!(plugin
        .tools()
        .iter()
        .any(|tool| tool.name == "plan_mode_complete"));

    let host = PlanSessionHost::default();
    *host.active.lock().unwrap() = vec!["read".into(), "bash".into(), "edit".into()];
    let log = NullLog;
    let caps = HostCaps {
        session: Some(&host),
        control: Some(&host),
        inject: Some(&host),
        ui: None,
        log: &log,
    };
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    // Enter: the active set becomes the planning set.
    plugin
        .call("plan_mode_start", &serde_json::json!({}), &caps, &sink)
        .expect("start plan mode");
    assert_eq!(
        *host.active.lock().unwrap(),
        vec!["read", "bash", "docs", "plan_mode_complete"],
        "planning keeps only the read-only set plus the complete tool"
    );

    // Complete: the previous set comes back and the plan is persisted.
    plugin
        .call(
            "plan_mode_complete",
            &serde_json::json!({ "plan": "1. Do the thing" }),
            &caps,
            &sink,
        )
        .expect("complete the plan");
    assert_eq!(*host.active.lock().unwrap(), vec!["read", "bash", "edit"]);
    let entries = host.entries.lock().unwrap();
    assert!(
        entries
            .iter()
            .any(|(kind, data)| kind == "plan-mode" && data["plan"] == "1. Do the thing"),
        "the plan rides a custom entry: {entries:?}"
    );

    // `/plan show` reports what was stored.
    let status = plugin
        .run_command("plan", "show", &caps)
        .expect("plan show");
    assert!(status.contains("Do the thing"), "got {status}");
}
