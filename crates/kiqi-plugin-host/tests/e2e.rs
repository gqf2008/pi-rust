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
///
/// `--target-dir` is passed explicitly: a machine-level `build.target-dir` (or
/// `CARGO_TARGET_DIR`) would otherwise move the artifact away from the path
/// this helper returns, and the test would fail on a machine where it cannot be
/// reproduced from the repo alone.
fn build_guest(crate_dir: &str, artifact: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let manifest = root.join("plugins/wasm").join(crate_dir).join("Cargo.toml");
    let target_dir = root.join("plugins/wasm/target");
    let status = Command::new(env!("CARGO"))
        .args(["build", "--manifest-path"])
        .arg(&manifest)
        .args(["--target", "wasm32-wasip2", "--release", "--target-dir"])
        .arg(&target_dir)
        .status()
        .unwrap_or_else(|error| panic!("run cargo build for {crate_dir}: {error}"));
    assert!(
        status.success(),
        "building {crate_dir} failed; is the `wasm32-wasip2` target installed?"
    );
    target_dir.join("wasm32-wasip2/release").join(artifact)
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

/// The first real wasm plugin: `kiqi-todo`. It proves the structured `details`
/// state channel and `session-read` (restoring from the branch) end to end.
#[test]
fn todo_guest_restores_from_the_session_branch() {
    let wasm = todo_wasm();
    let plugin = WasmPlugin::load("kiqi-todo", &wasm).expect("load the todo component");
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
        cancel: None,
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

/// A `SessionRead` whose branch can be swapped mid-test, so a restore
/// triggered by a second event can be observed against different branch data.
struct SwappingSession {
    branch: Mutex<serde_json::Value>,
}

impl kiqi_plugin::SessionRead for SwappingSession {
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
        self.branch.lock().unwrap().clone()
    }
}

/// `SessionTree` (the host fires it on `/tree` navigation, `/session` switch,
/// `/import`, `/fork`, `/clone`) must restore the guest's state exactly like
/// `SessionStart` does — otherwise a wasm todo guest keeps the PREVIOUS
/// branch's tasks after the user moves to another branch.
#[test]
fn session_tree_restores_from_a_swapped_branch() {
    fn branch_with(text: &str) -> serde_json::Value {
        serde_json::json!({
            "entries": [{
                "message": {
                    "role": "toolResult",
                    "toolName": "todo",
                    "details": {
                        "todos": [{"id": 1, "text": text, "done": false}],
                        "nextId": 2,
                    },
                }
            }]
        })
    }

    let wasm = todo_wasm();
    let plugin = WasmPlugin::load("kiqi-todo", &wasm).expect("load the todo component");

    let session = SwappingSession {
        branch: Mutex::new(branch_with("branch A todo")),
    };
    let log = NullLog;
    let caps = HostCaps {
        session: Some(&session),
        control: None,
        inject: None,
        ui: None,
        log: &log,
        cancel: None,
    };
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    let list_text = |plugin: &WasmPlugin| -> String {
        let listed = plugin
            .call("todo", &serde_json::json!({"action": "list"}), &caps, &sink)
            .expect("list");
        match &listed.blocks[0] {
            Block::Text(text) => text.clone(),
            other => panic!("unexpected block {other:?}"),
        }
    };

    // SessionStart restores branch A.
    let action = plugin.on_event(
        &Event::new(EventTag::SessionStart, serde_json::Value::Null),
        &caps,
    );
    assert_eq!(action, EventAction::Continue);
    assert!(
        list_text(&plugin).contains("branch A todo"),
        "restore A: {}",
        list_text(&plugin)
    );

    // The branch swaps underneath the guest (the host moved to another
    // branch), then SessionTree arrives — the guest must re-restore from the
    // NEW branch, not keep A's state.
    *session.branch.lock().unwrap() = branch_with("branch B todo");
    let action = plugin.on_event(
        &Event::new(EventTag::SessionTree, serde_json::Value::Null),
        &caps,
    );
    assert_eq!(action, EventAction::Continue);

    let text = list_text(&plugin);
    assert!(
        text.contains("branch B todo"),
        "SessionTree must restore the swapped-in branch: {text}"
    );
    assert!(
        !text.contains("branch A todo"),
        "the previous branch's state must be gone: {text}"
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
    let plugin = WasmPlugin::load_with_grants("kiqi-langfuse", &wasm, grants).expect("load");
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
        cancel: None,
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

/// `kiqi-ask-user` on wasm: a question round-trips through the host dialog and
/// the answer rides the `details` channel.
#[test]
fn ask_user_round_trips_through_the_host_dialog() {
    let wasm = ask_user_wasm();
    let plugin =
        WasmPlugin::load_with_grants("kiqi-ask-user", &wasm, Grants::default()).expect("load");
    assert!(plugin.tools().iter().any(|tool| tool.name == "ask_user"));

    let ui = ScriptedUi::new(DialogMode::Answer(serde_json::json!({ "text": "7897" })));
    let log = NullLog;
    let caps = HostCaps {
        session: None,
        control: None,
        inject: None,
        ui: Some(&ui),
        log: &log,
        cancel: None,
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
        cancel: None,
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
        WasmPlugin::load_with_grants("kiqi-ask-user", &wasm, Grants::default()).expect("load");
    let ui = ScriptedUi::new(DialogMode::Cancel);
    let log = NullLog;
    let caps = HostCaps {
        session: None,
        control: None,
        inject: None,
        ui: Some(&ui),
        log: &log,
        cancel: None,
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

/// The TUI opens an `ask_user` dialog from the same key loop that runs the
/// observe event fan-out. While the guest call holds the plugin's call lock
/// waiting for the user, an observe dispatch must skip instead of waiting:
/// the only person who can end the wait is the one the UI is trying to ask.
/// Without the skip this deadlocks the TUI (observed 2026-10-18).
#[test]
fn observe_events_skip_a_guest_that_is_waiting_on_the_ui() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let wasm = ask_user_wasm();
    let plugin = Arc::new(
        WasmPlugin::load_with_grants("kiqi-ask-user", &wasm, Grants::default()).expect("load"),
    );
    let ui = Arc::new(ScriptedUi::new(DialogMode::Pending));

    let ask = {
        let plugin = plugin.clone();
        let ui = ui.clone();
        std::thread::spawn(move || {
            let log = NullLog;
            let caps = HostCaps {
                session: None,
                control: None,
                inject: None,
                ui: Some(ui.as_ref()),
                log: &log,
                cancel: None,
            };
            let sink = RecordingSink {
                blocks: Mutex::new(Vec::new()),
            };
            plugin.call(
                "ask_user",
                &serde_json::json!({ "question": "Which port?" }),
                &caps,
                &sink,
            )
        })
    };

    let deadline = Instant::now() + Duration::from_secs(10);
    while !ui.ops().contains(&"open".to_string()) {
        assert!(
            Instant::now() < deadline,
            "the guest never opened the dialog: {:?}",
            ui.ops()
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // The guest is now polling for an answer and holds the call lock. The
    // observe probe must come back on its own.
    let (tx, rx) = std::sync::mpsc::channel();
    let observe = {
        let plugin = plugin.clone();
        let ui = ui.clone();
        std::thread::spawn(move || {
            let log = NullLog;
            let caps = HostCaps {
                session: None,
                control: None,
                inject: None,
                ui: Some(ui.as_ref()),
                log: &log,
                cancel: None,
            };
            let action = plugin.try_on_event(
                &Event::new(EventTag::SessionStart, serde_json::Value::Null),
                &caps,
            );
            let _ = tx.send(action);
        })
    };

    let observed = rx.recv_timeout(Duration::from_secs(2));
    // Release the blocked call in every outcome so the threads can join.
    *ui.mode.lock().unwrap() = DialogMode::Cancel;
    let outcome = ask.join().expect("ask thread").expect("call returns");
    observe.join().expect("observe thread");
    let action = match observed {
        Ok(action) => action,
        Err(_) => panic!("observe dispatch waited on a guest that is waiting on the UI"),
    };
    assert_eq!(
        action,
        EventAction::Continue,
        "a skipped observe is Continue"
    );
    assert!(outcome.is_error, "the cancelled ask is an error result");
}

/// Gate §6.3.4 + decision-fallback §4.1: a `timeout` closes the dialog and
/// returns a structured `unanswered` result — not an error, and never a
/// fabricated answer.
#[test]
fn ask_user_honours_timeout() {
    let wasm = ask_user_wasm();
    let plugin =
        WasmPlugin::load_with_grants("kiqi-ask-user", &wasm, Grants::default()).expect("load");
    let ui = ScriptedUi::new(DialogMode::Pending);
    let log = NullLog;
    let caps = HostCaps {
        session: None,
        control: None,
        inject: None,
        ui: Some(&ui),
        log: &log,
        cancel: None,
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
    assert!(
        !outcome.is_error,
        "a timeout is a result, not a failure: {outcome:?}"
    );
    assert_eq!(outcome.details["status"], serde_json::json!("unanswered"));
    assert_eq!(outcome.details["reason"], serde_json::json!("timeout"));
    assert_eq!(outcome.details["fallback"], serde_json::json!("keep"));
    assert_eq!(
        outcome.details["question"],
        serde_json::json!("Which port?")
    );
    assert!(
        outcome.details.get("answer").is_none(),
        "a timeout must not fake an answer: {outcome:?}"
    );
    assert!(
        text_of(&outcome).contains("unanswered"),
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
            "proc".to_string(),
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
        fs: Some(kiqi_plugin::grants::FsGrant {
            cwd: kiqi_plugin::grants::ExecCwd::PluginData,
        }),
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

    // A relative cwd descends into an existing subdirectory.
    std::fs::create_dir_all(project.path().join("sub")).expect("project subdir");
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

    // But the project tree is the user's: a cwd that does not exist must fail,
    // not be `mkdir -p`'d into it.
    let outcome = call_main_tool(
        &plugin,
        serde_json::json!({ "exec": { "cmd": "sh", "args": ["-c", "true"], "cwd": "typo-dir" } }),
    )
    .expect("call");
    assert!(
        text_of(&outcome).contains("does not exist"),
        "a missing project cwd is an error: {}",
        text_of(&outcome)
    );
    assert!(
        !project.path().join("typo-dir").exists(),
        "a refused cwd must not be created inside the project"
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

/// `kiqi-codegraph` ported to wasm: the model still sees the same eight tools
/// with the same schemas; `init`/`sync` and the MCP session go through the
/// host's `exec`/`proc` grants instead of spawning processes itself.
#[test]
fn codegraph_guest_declares_the_eight_tools() {
    let wasm = build_guest("kiqi-codegraph-guest", "kiqi_codegraph_guest.wasm");
    let plugin =
        WasmPlugin::load_with_grants("kiqi-codegraph", &wasm, Grants::default()).expect("load");
    assert_eq!(plugin.id(), "kiqi-codegraph");

    let mut names: Vec<String> = plugin.tools().into_iter().map(|tool| tool.name).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "codegraph_callees",
            "codegraph_callers",
            "codegraph_explore",
            "codegraph_files",
            "codegraph_impact",
            "codegraph_node",
            "codegraph_search",
            "codegraph_status",
        ]
    );
    // Every ported schema keeps the shape the compiled plugin emitted: an
    // object schema whose properties always include `projectPath`.
    for spec in plugin.tools() {
        assert_eq!(spec.parameters["type"], "object", "{}", spec.name);
        assert!(
            spec.parameters["properties"]["projectPath"].is_object(),
            "{} lost its projectPath parameter",
            spec.name
        );
    }

    // An unknown tool is refused, not silently dispatched.
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    let unknown = plugin
        .call("codegraph_nope", &serde_json::json!({}), &caps, &sink)
        .unwrap_err();
    assert!(unknown.to_string().contains("codegraph_nope"), "{unknown}");
}

/// The ported decision logic, asserted where it is observable: the guest asks
/// `test -d .codegraph` (it cannot stat the project tree itself), so with the
/// index present it goes straight to `sync`, and without it it tries `init`
/// first. Granting only `test` makes each path's *next* call the error, and the
/// error names the subcommand — which is how the test tells them apart.
#[test]
fn codegraph_guest_probes_the_index_then_needs_the_codegraph_grant() {
    use kiqi_plugin_host::ExecCwd;
    use kiqi_plugin_host::ExecGrant;

    // projectPath must live inside the session's directory (the guest resolves
    // it lexically against `info().cwd`), so the fixture project goes in a temp
    // directory under the test's own cwd — auto-removed on drop.
    let with_index = tempfile::Builder::new()
        .prefix(".kq-codegraph-indexed-")
        .tempdir_in(".")
        .expect("temp project");
    std::fs::create_dir_all(with_index.path().join(".codegraph")).expect("index dir");
    let without_index = tempfile::Builder::new()
        .prefix(".kq-codegraph-fresh-")
        .tempdir_in(".")
        .expect("temp project");

    let grants = Grants {
        exec: Some(ExecGrant {
            commands: vec!["test".to_string()],
            cwd: ExecCwd::Project,
            ..ExecGrant::default()
        }),
        ..Grants::default()
    };

    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    for (project, expected) in [
        (with_index.path(), "codegraph sync"),
        (without_index.path(), "codegraph init"),
    ] {
        let wasm = build_guest("kiqi-codegraph-guest", "kiqi_codegraph_guest.wasm");
        let plugin =
            WasmPlugin::load_with_grants("kiqi-codegraph", &wasm, grants.clone()).expect("load");
        let error = plugin
            .call(
                "codegraph_status",
                &serde_json::json!({ "projectPath": project.display().to_string() }),
                &caps,
                &sink,
            )
            .unwrap_err()
            .to_string();
        assert!(
            !error.contains("test -d"),
            "the probe itself must have succeeded with `test` granted: {error}"
        );
        assert!(
            error.contains(expected),
            "expected `{expected}` to be the call that needs the codegraph grant: {error}"
        );
        assert!(
            error.contains("not granted"),
            "the refusal names the missing grant: {error}"
        );
    }
}

/// The MCP session itself needs the real `codegraph` CLI (an npm package), so
/// this is the repo's detect-and-return pattern: say so and skip when it is not
/// installed, rather than pretending the protocol path was covered.
#[test]
fn codegraph_guest_live_session_when_asked() {
    use kiqi_plugin_host::{ExecCwd, ExecGrant, ProcGrant};

    let available = std::process::Command::new("codegraph")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !available {
        eprintln!(
            "skipping codegraph_guest_live_session_when_asked: no `codegraph` on PATH \
             (npm i -g @colbymchenry/codegraph)"
        );
        return;
    }

    let project = tempfile::Builder::new()
        .prefix(".kq-codegraph-live-")
        .tempdir_in(".")
        .expect("temp project");
    let wasm = build_guest("kiqi-codegraph-guest", "kiqi_codegraph_guest.wasm");
    let plugin = WasmPlugin::load_with_grants(
        "kiqi-codegraph",
        &wasm,
        Grants {
            exec: Some(ExecGrant {
                commands: vec!["test".to_string(), "codegraph".to_string()],
                cwd: ExecCwd::Project,
                timeout_seconds: 300,
                ..ExecGrant::default()
            }),
            proc: Some(ProcGrant {
                commands: vec!["codegraph".to_string()],
                cwd: ExecCwd::Project,
                ..ProcGrant::default()
            }),
            ..Grants::default()
        },
    )
    .expect("load");
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    let outcome = plugin
        .call(
            "codegraph_status",
            &serde_json::json!({ "projectPath": project.path().display().to_string() }),
            &caps,
            &sink,
        )
        .expect("call");
    assert!(!outcome.is_error, "{outcome:?}");
    assert!(
        !text_of(&outcome).trim().is_empty(),
        "the MCP session answered with something"
    );
}

/// `kiqi-orchestrator` ported to wasm, driven hermetically with `cat` as the
/// child program: the task rides stdin, `cat` echoes it back, so a whole wave
/// can complete without any real agent.
#[test]
fn orchestrator_guest_runs_a_wave_via_cat() {
    use kiqi_plugin_host::ProcGrant;

    let wasm = build_guest("kiqi-orchestrator-guest", "kiqi_orchestrator_guest.wasm");
    let plugin = WasmPlugin::load_with_grants(
        "kiqi-orchestrator",
        &wasm,
        Grants {
            proc: Some(ProcGrant {
                commands: vec!["cat".to_string(), "test".to_string(), "sh".to_string()],
                cwd: kiqi_plugin_host::ExecCwd::Project,
                ..ProcGrant::default()
            }),
            ..Grants::default()
        },
    )
    .expect("load");
    assert_eq!(plugin.id(), "kiqi-orchestrator");
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    let set = plugin
        .call(
            "plan",
            &serde_json::json!({"action": "set", "nodes": [
                {"id": "a", "task": "alpha"},
                {"id": "b", "task": "beta", "deps": ["a"]}
            ]}),
            &caps,
            &sink,
        )
        .expect("plan set");
    assert!(text_of(&set).contains("0/2 done"), "{}", text_of(&set));

    // First wave: only `a` is ready (`b` depends on it).
    let wave = plugin
        .call(
            "dispatch",
            &serde_json::json!({ "program": "cat" }),
            &caps,
            &sink,
        )
        .expect("dispatch");
    assert!(
        text_of(&wave).contains("started 1") && text_of(&wave).contains("1/2 done"),
        "{}",
        text_of(&wave)
    );

    // Second wave: `b` is ready now; the wave completes the plan.
    let wave = plugin
        .call(
            "dispatch",
            &serde_json::json!({ "program": "cat" }),
            &caps,
            &sink,
        )
        .expect("dispatch");
    assert!(
        text_of(&wave).contains("2/2 done"),
        "the plan completed: {}",
        text_of(&wave)
    );

    // waitSeconds:0 is refused with the wasm-tier explanation.
    let refused = plugin
        .call(
            "dispatch",
            &serde_json::json!({ "waitSeconds": 0, "program": "cat" }),
            &caps,
            &sink,
        )
        .unwrap_err();
    assert!(
        refused.to_string().contains("cannot outlive this call"),
        "{refused}"
    );
}

/// The subagent tool drives one child via `proc` (task on stdin, EOF, collect).
#[test]
fn orchestrator_guest_subagent_round_trips_via_cat() {
    use kiqi_plugin_host::ProcGrant;

    let wasm = build_guest("kiqi-orchestrator-guest", "kiqi_orchestrator_guest.wasm");
    let plugin = WasmPlugin::load_with_grants(
        "kiqi-orchestrator",
        &wasm,
        Grants {
            proc: Some(ProcGrant {
                commands: vec!["cat".to_string()],
                cwd: kiqi_plugin_host::ExecCwd::Project,
                ..ProcGrant::default()
            }),
            ..Grants::default()
        },
    )
    .expect("load");
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    let outcome = plugin
        .call(
            "subagent",
            &serde_json::json!({ "task": "hello orchestrator", "program": "cat" }),
            &caps,
            &sink,
        )
        .expect("call");
    assert!(
        text_of(&outcome).contains("hello orchestrator"),
        "the child echoed the task: {}",
        text_of(&outcome)
    );

    // Without the grant the same call is denied with the remediation.
    let bare =
        WasmPlugin::load_with_grants("kiqi-orchestrator", &wasm, Grants::default()).expect("load");
    let denied = bare
        .call(
            "subagent",
            &serde_json::json!({ "task": "hello", "program": "cat" }),
            &caps,
            &sink,
        )
        .unwrap_err();
    assert!(
        denied.to_string().contains("proc is not granted"),
        "{denied}"
    );
}

/// A session switch clears the plan, matching the compiled plugin.
#[test]
fn orchestrator_guest_session_switch_clears_the_plan() {
    let wasm = build_guest("kiqi-orchestrator-guest", "kiqi_orchestrator_guest.wasm");
    let plugin =
        WasmPlugin::load_with_grants("kiqi-orchestrator", &wasm, Grants::default()).expect("load");
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    let _ = plugin.before_agent_start(
        &serde_json::json!({"sessionId": "s1", "systemPrompt": "base"}),
        &caps,
    );
    plugin
        .call(
            "plan",
            &serde_json::json!({"action": "set", "nodes": [
                {"id": "a", "task": "x"}
            ]}),
            &caps,
            &sink,
        )
        .expect("plan set");

    let _ = plugin.before_agent_start(
        &serde_json::json!({"sessionId": "s2", "systemPrompt": "base"}),
        &caps,
    );
    let shown = plugin
        .call("plan", &serde_json::json!({"action": "show"}), &caps, &sink)
        .expect("plan show");
    assert!(
        text_of(&shown).contains("plan is empty"),
        "the session switch cleared the graph: {}",
        text_of(&shown)
    );
}

/// The goal-coupled guidance gate, restored through the project-scoped `fs`
/// grant: the section is appended only when `<project>/.ki/goals.json` says an
/// active goal exists, never twice, and always fail-closed.
#[test]
fn orchestrator_guest_gates_guidance_on_the_active_goal() {
    use kiqi_plugin_host::{ExecCwd, FsGrant};

    let wasm = build_guest("kiqi-orchestrator-guest", "kiqi_orchestrator_guest.wasm");
    let project = tempfile::tempdir().expect("project tempdir");
    let plugin = WasmPlugin::load_with_options(
        "kiqi-orchestrator",
        &wasm,
        Grants {
            fs: Some(FsGrant {
                cwd: ExecCwd::Project,
            }),
            ..Grants::default()
        },
        kiqi_plugin_host::Options {
            project_dir: Some(project.path().to_path_buf()),
            ..kiqi_plugin_host::Options::default()
        },
    )
    .expect("load");
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let prompt = |base: &str| serde_json::json!({"sessionId": "s", "systemPrompt": base});
    let goals = project.path().join(".ki/goals.json");
    std::fs::create_dir_all(project.path().join(".ki")).expect("state dir");

    // No goal file → no guidance (fail-closed).
    assert!(
        plugin.before_agent_start(&prompt("base"), &caps).is_none(),
        "no goal file means no guidance"
    );

    // An active goal turns it on, over the base prompt.
    std::fs::write(&goals, r#"{"active":{"status":"active"},"archived":[]}"#).expect("write");
    let injected = plugin
        .before_agent_start(&prompt("base"), &caps)
        .expect("the section is appended");
    assert!(
        injected.contains("## Subagent orchestration") && injected.contains("base"),
        "the base prompt plus the section: {injected}"
    );

    // The heading is never appended twice (idempotent).
    let twice = plugin.before_agent_start(
        &prompt("base\n\n## Subagent orchestration\nalready here"),
        &caps,
    );
    assert!(twice.is_none(), "no second append: {twice:?}");

    // A paused goal, a completed goal, a null active, and a malformed file all
    // mean "no guidance".
    for state in [
        r#"{"active":{"status":"paused"},"archived":[]}"#,
        r#"{"active":{"status":"complete"},"archived":[]}"#,
        r#"{"active":null,"archived":[]}"#,
        "not json",
    ] {
        std::fs::write(&goals, state).expect("write");
        assert!(
            plugin.before_agent_start(&prompt("base"), &caps).is_none(),
            "`{state}` means no guidance"
        );
    }
}

/// Without the `fs` grant the guest cannot read the goal file, so the guidance
/// stays off — the compiled plugin had no capability gate, a guest fails closed.
#[test]
fn orchestrator_guest_guidance_needs_the_fs_grant() {
    let wasm = build_guest("kiqi-orchestrator-guest", "kiqi_orchestrator_guest.wasm");
    let project = tempfile::tempdir().expect("project tempdir");
    std::fs::create_dir_all(project.path().join(".ki")).expect("state dir");
    std::fs::write(
        project.path().join(".ki/goals.json"),
        r#"{"active":{"status":"active"},"archived":[]}"#,
    )
    .expect("write");
    let plugin = WasmPlugin::load_with_options(
        "kiqi-orchestrator",
        &wasm,
        Grants::default(),
        kiqi_plugin_host::Options {
            project_dir: Some(project.path().to_path_buf()),
            ..kiqi_plugin_host::Options::default()
        },
    )
    .expect("load");
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    assert!(
        plugin
            .before_agent_start(
                &serde_json::json!({"sessionId": "s", "systemPrompt": "base"}),
                &caps
            )
            .is_none(),
        "no fs grant means the file cannot be read, so no guidance"
    );
}

/// The coupling this batch restores, end to end: the **goal** guest starts a
/// goal through its own tool, and the **orchestrator** guest turns its
/// guidance on by reading the file the goal guest actually wrote. Neither
/// side's literals are used here — a drift in the file's field names or shape
/// between the two guests (exactly the coupling) fails this test, where the
/// two per-guest suites would both still pass.
#[test]
fn goal_guest_output_turns_the_orchestrator_guidance_on() {
    use kiqi_plugin_host::{ExecCwd, FsGrant};

    // One project tree, shared by both guests (their `fs --cwd project` root).
    let project = tempfile::tempdir().expect("project tempdir");

    // The goal guest starts a goal through its own `goal` tool.
    let goal_artifact = build_guest("kiqi-goal-guest", "kiqi_goal_guest.wasm");
    let goal_component = tempfile::tempdir().expect("goal component tempdir");
    let goal_wasm = goal_component.path().join("plugin.wasm");
    std::fs::copy(&goal_artifact, &goal_wasm).expect("copy the goal guest");
    let goal_plugin = WasmPlugin::load_with_options(
        "kiqi-goal",
        &goal_wasm,
        goal_fs_grant(),
        kiqi_plugin_host::Options {
            project_dir: Some(project.path().to_path_buf()),
            ..kiqi_plugin_host::Options::default()
        },
    )
    .expect("load the goal guest");
    let inject_host = GoalInjectHost::default();
    let log = NullLog;
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    goal_plugin
        .call(
            "goal",
            &serde_json::json!({"action": "start", "title": "Couple the tiers"}),
            &goal_caps(&inject_host, &log),
            &sink,
        )
        .expect("start a goal");
    let goals = project.path().join(".ki/goals.json");
    assert!(
        goals.is_file(),
        "the goal guest wrote the project file the orchestrator reads"
    );

    // The orchestrator guest reads that same file and turns the guidance on.
    let orchestrator_artifact =
        build_guest("kiqi-orchestrator-guest", "kiqi_orchestrator_guest.wasm");
    let orchestrator = WasmPlugin::load_with_options(
        "kiqi-orchestrator",
        &orchestrator_artifact,
        Grants {
            fs: Some(FsGrant {
                cwd: ExecCwd::Project,
            }),
            ..Grants::default()
        },
        kiqi_plugin_host::Options {
            project_dir: Some(project.path().to_path_buf()),
            ..kiqi_plugin_host::Options::default()
        },
    )
    .expect("load the orchestrator guest");
    let caps = HostCaps::observer(&log);
    let injected = orchestrator
        .before_agent_start(
            &serde_json::json!({"sessionId": "s", "systemPrompt": "base"}),
            &caps,
        )
        .expect("the goal guest's active goal turns the guidance on");
    assert!(
        injected.contains("## Subagent orchestration") && injected.contains("base"),
        "the coupling fired: {injected}"
    );
}

/// The fs grant's scope: the default is the plugin's own data directory
/// (unchanged), and `--cwd project` (via the CLI options) opts the guest into
/// reading and writing the project tree.
#[test]
fn fs_project_scope_reads_and_writes_the_project_tree() {
    use kiqi_plugin_host::{ExecCwd, FsGrant};

    let project = tempfile::tempdir().expect("project tempdir");
    std::fs::write(project.path().join("notes.txt"), b"project file").expect("seed file");
    let wasm = fixture_wasm();
    let plugin = WasmPlugin::load_with_options(
        "conformance",
        &wasm,
        Grants {
            fs: Some(FsGrant {
                cwd: ExecCwd::Project,
            }),
            ..Grants::default()
        },
        kiqi_plugin_host::Options {
            project_dir: Some(project.path().to_path_buf()),
            ..kiqi_plugin_host::Options::default()
        },
    )
    .expect("load");

    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    // The conformance tool's fs probe writes `conformance/probe.txt` then reads
    // it back — inside the project directory when the scope is `project`.
    let outcome = call_main_tool(&plugin, serde_json::json!({})).expect("call");
    assert!(
        text_of(&outcome).contains("fs=conformance"),
        "the guest round-tripped a file through the scoped fs: {}",
        text_of(&outcome)
    );
    assert!(
        project.path().join("conformance/probe.txt").is_file(),
        "the write landed in the project tree"
    );
}

/// `proc` round-trips a line through a child the guest talks to: spawn, write,
/// read-line, wait, read-stderr, kill — every function of the interface, driven
/// by the conformance fixture inside one tool call.
#[test]
fn proc_round_trips_a_line_through_the_host() {
    use kiqi_plugin_host::ProcGrant;

    let (dir, plugin) = load_fixture_in_temp(Grants {
        proc: Some(ProcGrant {
            commands: vec!["sh".to_string()],
            ..ProcGrant::default()
        }),
        ..Grants::default()
    });
    let outcome = call_main_tool(
        &plugin,
        serde_json::json!({"proc": {
            "cmd": "sh",
            "args": ["-c", "read line; echo \"reply:$line\""],
            "line": "hello"
        }}),
    )
    .expect("call");
    let text = text_of(&outcome);
    assert!(
        text.contains("proc=spawned=1 reply=reply:hello wait=0 stderr=0"),
        "the whole conversation happened: {text}"
    );
    // The child is a real process in the plugin's own data directory.
    assert!(dir.path().join("data").is_dir());
}

/// `proc` is gated exactly like `exec`: no grant, a command outside the
/// allow-list, and a cwd that leaves the scope are each refused.
#[test]
fn proc_is_gated_like_exec() {
    use kiqi_plugin_host::ProcGrant;

    let (_dir, plugin) = load_fixture_in_temp(Grants::default());
    let denied = call_main_tool(
        &plugin,
        serde_json::json!({"proc": {"cmd": "sh", "args": ["-c", "true"]}}),
    )
    .expect("call");
    assert!(
        text_of(&denied).contains("proc=proc is not granted"),
        "an ungranted proc is denied: {}",
        text_of(&denied)
    );

    let (_dir, plugin) = load_fixture_in_temp(Grants {
        proc: Some(ProcGrant {
            commands: vec!["ls".to_string()],
            ..ProcGrant::default()
        }),
        ..Grants::default()
    });
    let denied = call_main_tool(
        &plugin,
        serde_json::json!({"proc": {"cmd": "sh", "args": ["-c", "true"]}}),
    )
    .expect("call");
    assert!(
        text_of(&denied).contains("proc=proc of `sh` is not granted"),
        "a command outside the allow-list is denied by name: {}",
        text_of(&denied)
    );

    // The cwd scope is the same jail `exec` uses.
    let (_dir, plugin) = load_fixture_in_temp(Grants {
        proc: Some(ProcGrant {
            commands: vec!["sh".to_string()],
            ..ProcGrant::default()
        }),
        ..Grants::default()
    });
    let escaped = call_main_tool(
        &plugin,
        serde_json::json!({"proc": {"cmd": "sh", "args": ["-c", "true"], "cwd": "../outside"}}),
    )
    .expect("call");
    assert!(
        text_of(&escaped).contains("proc=failed:"),
        "a cwd that leaves the scope is refused: {}",
        text_of(&escaped)
    );
}

/// The grant's `max-open` bounds how many children one call may hold.
#[test]
fn proc_max_open_is_enforced() {
    use kiqi_plugin_host::ProcGrant;

    let (_dir, plugin) = load_fixture_in_temp(Grants {
        proc: Some(ProcGrant {
            commands: vec!["sh".to_string()],
            max_open: 1,
            ..ProcGrant::default()
        }),
        ..Grants::default()
    });
    let outcome = call_main_tool(
        &plugin,
        serde_json::json!({"proc": {
            "cmd": "sh", "args": ["-c", "true"], "count": 2, "keep": true
        }}),
    )
    .expect("call");
    assert!(
        text_of(&outcome)
            .contains("proc=denied:proc already holds 1 children and the grant allows 1 at once"),
        "the second spawn is refused with the bound in the reason: {}",
        text_of(&outcome)
    );
}

/// A handle lives for exactly one call: a child the guest left running (or
/// forgot to kill) is killed — process group and all — when the call returns.
/// Asserted on the OS, not on the host's bookkeeping: the child printed its own
/// pid, and after the call that pid must be gone.
#[cfg(unix)]
#[test]
fn proc_reaps_children_when_the_call_ends() {
    use kiqi_plugin_host::ProcGrant;

    let (_dir, plugin) = load_fixture_in_temp(Grants {
        proc: Some(ProcGrant {
            commands: vec!["sh".to_string()],
            ..ProcGrant::default()
        }),
        ..Grants::default()
    });
    // The fixture reads the child's first stdout line = its pid, writes nothing
    // useful and leaves the child sleeping (`keep`): the guest never kills it.
    let outcome = call_main_tool(
        &plugin,
        serde_json::json!({"proc": {
            "cmd": "sh",
            "args": ["-c", "echo $$; sleep 30"],
            "line": "ignored",
            "keep": true
        }}),
    )
    .expect("call");
    let text = text_of(&outcome);
    assert!(
        text.contains("proc=spawned=1 reply=") && text.contains("wait=skipped"),
        "the child was left running on purpose: {text}"
    );
    let pid: i32 = text
        .split("reply=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|pid| pid.trim().parse().ok())
        .unwrap_or_else(|| panic!("the child's pid did not come back: {text}"));

    // The call has returned, so the host has reaped: the pid is gone (and, since
    // the host waited on it, not even a zombie is left).
    // SAFETY: signal 0 only probes for the process's existence.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(
        !alive,
        "the child ({pid}) outlived the call that spawned it: {text}"
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

/// `kiqi-webfetch` ported to wasm: the tool's own URL policy runs in the guest,
/// and where it may actually go is the user's grant — checked before any network
/// access, so this stays hermetic.
#[test]
fn webfetch_guest_keeps_its_url_policy_and_needs_the_grant() {
    let wasm = build_guest("kiqi-webfetch-guest", "kiqi_webfetch_guest.wasm");
    let plugin = WasmPlugin::load("kiqi-webfetch", &wasm).expect("load");
    assert_eq!(plugin.id(), "kiqi-webfetch");
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

/// `kiqi-lens` ported to wasm: the check runs through the host's `exec`
/// capability with a `project` cwd scope, so `git diff --check` runs against
/// the project directory — and without the grant the tool degrades to a clear
/// deny instead of running.
#[test]
fn kiqi_lens_guest_runs_checks_in_the_project_directory() {
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
        eprintln!("skipping kiqi_lens_guest_runs_checks_in_the_project_directory: no git on PATH");
        return;
    }

    let wasm = build_guest("kiqi-lens-guest", "kiqi_lens_guest.wasm");
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
        "kiqi-lens",
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
    assert_eq!(plugin.id(), "kiqi-lens");
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

    // The tool's cwd mapping: `.` is the project root itself, a relative
    // subdir descends, and an absolute path outside the project is refused by
    // the guest before the host is even asked.
    std::fs::create_dir_all(project.path().join("sub")).expect("project subdir");
    let outcome = plugin
        .call(
            "code_lens",
            &serde_json::json!({ "check": "diff", "cwd": "." }),
            &caps,
            &sink,
        )
        .expect("call");
    assert!(
        text_of(&outcome).contains("check=diff exit=0"),
        "cwd `.` is the project root: {}",
        text_of(&outcome)
    );
    let outcome = plugin
        .call(
            "code_lens",
            &serde_json::json!({ "check": "diff", "cwd": "sub" }),
            &caps,
            &sink,
        )
        .expect("call");
    assert!(
        text_of(&outcome).contains("check=diff exit=0"),
        "a relative subdir of the project works: {}",
        text_of(&outcome)
    );
    let refused = plugin
        .call(
            "code_lens",
            &serde_json::json!({ "check": "diff", "cwd": "/" }),
            &caps,
            &sink,
        )
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("outside the project directory"),
        "an absolute cwd outside the project is refused by the guest: {refused}"
    );

    // Trailing slash + the production fallback: with no `Options::project_dir`
    // the host resolves against the process cwd, which is also the guest's
    // strip anchor — so an absolute path spelled the way shell completion
    // emits it (with a trailing slash) is accepted, and `git diff --check`
    // runs read-only in the repo the test process sits in.
    let plugin = WasmPlugin::load_with_grants(
        "kiqi-lens",
        &wasm,
        Grants {
            exec: Some(ExecGrant {
                commands: vec!["git".to_string()],
                cwd: ExecCwd::Project,
                ..ExecGrant::default()
            }),
            ..Grants::default()
        },
    )
    .expect("load");
    let cwd_with_slash = format!("{}/", std::env::current_dir().unwrap().display());
    let outcome = plugin
        .call(
            "code_lens",
            &serde_json::json!({ "check": "diff", "cwd": cwd_with_slash }),
            &caps,
            &sink,
        )
        .expect("call");
    assert!(
        text_of(&outcome).contains("check=diff exit="),
        "an absolute cwd with a trailing slash is accepted: {}",
        text_of(&outcome)
    );

    // Same plugin without the project scope: the child starts in the data
    // directory, which is not a git repository.
    let dir = tempfile::tempdir().expect("install tempdir");
    let scoped_wasm = dir.path().join("plugin.wasm");
    std::fs::copy(&wasm, &scoped_wasm).expect("copy the guest next to a data dir");
    let plugin = WasmPlugin::load_with_grants(
        "kiqi-lens",
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
    let plugin = WasmPlugin::load_with_grants("kiqi-lens", &wasm, Grants::default()).expect("load");
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
        denied.contains("grant kiqi-lens exec --command cargo --command git"),
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

/// `kiqi-plan-mode` ported to wasm: entering swaps the tool set through
/// `session-control`, completing restores it and persists the plan through
/// `inject.append-custom-entry`, and `/plan show` reads it back.
#[test]
fn plan_mode_guest_swaps_tools_and_persists_through_the_host() {
    let wasm = build_guest("kiqi-plan-mode-guest", "kiqi_plan_mode_guest.wasm");
    let plugin = WasmPlugin::load("kiqi-plan-mode", &wasm).expect("load");
    assert_eq!(plugin.id(), "kiqi-plan-mode");
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
        cancel: None,
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

// ---------------------------------------------------------------------------
// kiqi-websearch: guest-side argument policy, then the grant gates the fetch.
// ---------------------------------------------------------------------------

/// `kiqi-websearch` ported to wasm: the query validation is the guest's own (it
/// runs before any fetch), and the two hosts it calls — DuckDuckGo and Wikipedia
/// — are the user's grant's to allow. No network is touched: the guest's own
/// argument policy answers first, and a granted-less fetch is refused by the
/// host before a request leaves the process.
#[test]
fn websearch_guest_validates_in_the_guest_and_needs_the_grant() {
    let wasm = build_guest("kiqi-websearch-guest", "kiqi_websearch_guest.wasm");
    let plugin =
        WasmPlugin::load_with_grants("kiqi-websearch", &wasm, Grants::default()).expect("load");
    assert_eq!(plugin.id(), "kiqi-websearch");
    assert!(
        plugin.tools().iter().any(|tool| tool.name == "websearch"),
        "the tool is declared: {:?}",
        plugin.tools()
    );

    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    // The guest's own argument policy, before any capability call: a missing or
    // empty query never reaches the network.
    let missing = plugin
        .call("websearch", &serde_json::json!({}), &caps, &sink)
        .unwrap_err();
    assert!(
        missing.to_string().contains("query is required"),
        "got {missing}"
    );
    let empty = plugin
        .call(
            "websearch",
            &serde_json::json!({ "query": "   " }),
            &caps,
            &sink,
        )
        .unwrap_err();
    assert!(
        empty.to_string().contains("query must not be empty"),
        "got {empty}"
    );

    // A valid query passes the guest's policy and is then the grant's to refuse.
    let denied = plugin
        .call(
            "websearch",
            &serde_json::json!({ "query": "rust" }),
            &caps,
            &sink,
        )
        .unwrap_err();
    assert!(denied.to_string().contains("denied"), "got {denied}");

    // A grant for some unrelated host still does not cover api.duckduckgo.com.
    let other = WasmPlugin::load_with_grants(
        "kiqi-websearch",
        &wasm,
        Grants {
            net_http: Some(NetHttpGrant {
                hosts: vec!["example.com".into()],
            }),
            ..Grants::default()
        },
    )
    .expect("load");
    let wrong_host = other
        .call(
            "websearch",
            &serde_json::json!({ "query": "rust" }),
            &caps,
            &sink,
        )
        .unwrap_err();
    assert!(
        wrong_host.to_string().contains("denied"),
        "got {wrong_host}"
    );
}

/// The parse/filter path can only run against the real endpoints (they are
/// hardcoded, unlike webfetch's URL argument, so no local server can stand in).
/// Opt in with `KIQI_WEBSEARCH_LIVE=1`; without it the test says so and returns
/// rather than pretending to have covered the path.
#[test]
fn websearch_guest_live_search_when_asked() {
    if std::env::var("KIQI_WEBSEARCH_LIVE").as_deref() != Ok("1") {
        eprintln!(
            "skipping websearch_guest_live_search_when_asked: set KIQI_WEBSEARCH_LIVE=1 to \
             exercise the parse/filter path against the real endpoints"
        );
        return;
    }
    let wasm = build_guest("kiqi-websearch-guest", "kiqi_websearch_guest.wasm");
    let grants = Grants {
        net_http: Some(NetHttpGrant {
            hosts: vec!["api.duckduckgo.com".into(), "en.wikipedia.org".into()],
        }),
        ..Grants::default()
    };
    let plugin = WasmPlugin::load_with_grants("kiqi-websearch", &wasm, grants).expect("load");
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    let outcome = plugin
        .call(
            "websearch",
            &serde_json::json!({ "query": "rust programming language", "limit": 3 }),
            &caps,
            &sink,
        )
        .expect("call");
    assert!(!outcome.is_error, "{outcome:?}");
    // The trait face's shape: the block text is the result envelope JSON.
    let envelope: serde_json::Value =
        serde_json::from_str(&text_of(&outcome)).expect("the block text is the envelope JSON");
    assert_eq!(envelope["query"], "rust programming language");
    assert!(
        envelope["count"].as_u64().unwrap_or(0) >= 1,
        "at least one hit: {envelope}"
    );
}

// ---------------------------------------------------------------------------
// kiqi-subagents: a pure envelope tool, no capabilities at all.
// ---------------------------------------------------------------------------

/// `kiqi-subagents` ported to wasm: pure logic, no capabilities. The guest must
/// emit the *same* delegation envelope bytes the compile-time plugin emits, so
/// the envelope is compared field-order-and-value, byte for byte.
#[test]
fn subagents_guest_emits_the_byte_stable_delegation_envelope() {
    let wasm = build_guest("kiqi-subagents-guest", "kiqi_subagents_guest.wasm");
    let plugin = WasmPlugin::load("kiqi-subagents", &wasm).expect("load");
    assert_eq!(plugin.id(), "kiqi-subagents");

    let tool = plugin
        .tools()
        .into_iter()
        .find(|tool| tool.name == "delegate_task")
        .expect("delegate_task is declared");
    assert_eq!(
        tool.description,
        "Create a bounded, non-recursive subagent delegation envelope."
    );

    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    // Clamping and field order match the compile-time plugin exactly.
    let outcome = plugin
        .call(
            "delegate_task",
            &serde_json::json!({"task": "review", "maxTurns": 999, "timeoutSeconds": 99999}),
            &caps,
            &sink,
        )
        .expect("call delegate_task");
    assert!(!outcome.is_error);
    assert_eq!(
        text_of(&outcome),
        r#"{"kind":"kiqi-delegation","task":"review","role":"worker","maxTurns":32,"timeoutSeconds":1800,"recursive":false}"#
    );
    assert_eq!(outcome.details, serde_json::Value::Null);

    // Defaults apply when the optional fields are absent.
    let defaults = plugin
        .call(
            "delegate_task",
            &serde_json::json!({"task": "x"}),
            &caps,
            &sink,
        )
        .expect("call with defaults");
    assert_eq!(
        text_of(&defaults),
        r#"{"kind":"kiqi-delegation","task":"x","role":"worker","maxTurns":8,"timeoutSeconds":300,"recursive":false}"#
    );

    // A role is carried through verbatim.
    let roled = plugin
        .call(
            "delegate_task",
            &serde_json::json!({"task": "x", "role": "planner"}),
            &caps,
            &sink,
        )
        .expect("call with a role");
    assert!(
        text_of(&roled).contains(r#""role":"planner""#),
        "got {}",
        text_of(&roled)
    );

    // Both validation errors survive the port, message and all — including the
    // lenient-parse path, where a non-object argument reads as a missing task
    // exactly as the compile-time `kit` does (`unwrap_or(Value::Null)`).
    let empty = plugin
        .call(
            "delegate_task",
            &serde_json::json!({"task": "   "}),
            &caps,
            &sink,
        )
        .unwrap_err();
    assert!(
        empty
            .to_string()
            .contains("task must contain 1-20000 characters"),
        "got {empty}"
    );
    let missing = plugin
        .call("delegate_task", &serde_json::json!({}), &caps, &sink)
        .unwrap_err();
    assert!(
        missing
            .to_string()
            .contains("missing string parameter `task`"),
        "got {missing}"
    );
    let non_object = plugin
        .call("delegate_task", &serde_json::Value::Null, &caps, &sink)
        .unwrap_err();
    assert!(
        non_object
            .to_string()
            .contains("missing string parameter `task`"),
        "got {non_object}"
    );

    // An undeclared tool is a typed error, not a trap.
    let unknown = plugin
        .call("no_such_tool", &serde_json::json!({}), &caps, &sink)
        .unwrap_err();
    assert!(
        matches!(unknown, PluginError::UnknownTool(_)),
        "got {unknown}"
    );
}

// ---------------------------------------------------------------------------
// kiqi-goal: the project file (compiled shape), inject, TurnEnd hook.
// ---------------------------------------------------------------------------

/// A host that records what the goal guest injects, so the port's
/// `inject.append-entry` payload can be checked against `InjectApi::append_message`.
#[derive(Default)]
struct GoalInjectHost {
    messages: Mutex<Vec<serde_json::Value>>,
}

impl kiqi_plugin::InjectApi for GoalInjectHost {
    fn send_message(&self, _message: &serde_json::Value) -> Result<(), PluginError> {
        Ok(())
    }
    fn send_user_message(&self, _text: &str) -> Result<(), PluginError> {
        Ok(())
    }
    fn append_message(&self, message: &serde_json::Value) -> Result<(), PluginError> {
        self.messages.lock().unwrap().push(message.clone());
        Ok(())
    }
    fn append_custom_entry(
        &self,
        _custom_type: &str,
        _data: &serde_json::Value,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

/// The `fs` grant both goal and orchestrator tests need: the project tree
/// (`--cwd project`), so state lands in `<project>/.ki/goals.json` and never in
/// the repo or the build output.
fn goal_fs_grant() -> Grants {
    Grants {
        fs: Some(kiqi_plugin::grants::FsGrant {
            cwd: kiqi_plugin::grants::ExecCwd::Project,
        }),
        ..Grants::default()
    }
}

fn read_json(path: &std::path::Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path).expect("read the file");
    serde_json::from_str(&text).expect("the file is JSON")
}

/// Load the goal guest from a scratch component directory, with the project
/// scope pointed at a temp project: writes land there, not in the repo. Returns
/// the component dir (whose `data/` was the old jail) and the project dir.
fn load_goal_guest(grants: Grants) -> (tempfile::TempDir, tempfile::TempDir, WasmPlugin) {
    let artifact = build_guest("kiqi-goal-guest", "kiqi_goal_guest.wasm");
    let component = tempfile::tempdir().expect("component tempdir");
    let project = tempfile::tempdir().expect("project tempdir");
    let wasm = component.path().join("plugin.wasm");
    std::fs::copy(&artifact, &wasm).expect("copy the guest next to its component dir");
    let plugin = WasmPlugin::load_with_options(
        "kiqi-goal",
        &wasm,
        grants,
        kiqi_plugin_host::Options {
            project_dir: Some(project.path().to_path_buf()),
            ..kiqi_plugin_host::Options::default()
        },
    )
    .expect("load");
    (component, project, plugin)
}

fn goal_caps<'a>(host: &'a GoalInjectHost, log: &'a NullLog) -> HostCaps<'a> {
    HostCaps {
        session: None,
        control: None,
        inject: Some(host),
        ui: None,
        log,
        cancel: None,
    }
}

/// A `SessionRead` whose tool face the test controls.
struct ToolFaceSession {
    tools: Vec<String>,
}

impl kiqi_plugin::SessionRead for ToolFaceSession {
    fn active_tools(&self) -> Vec<String> {
        self.tools.clone()
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

/// The goal→orchestrator coupling at the prompt level: an active goal's
/// injection tells the model how to advance it (plan → dispatch → subagent) —
/// but only when those tools are actually on the session's tool face. Naming
/// tools from an install without the orchestrator would send the model hunting.
#[test]
fn goal_injection_carries_the_orchestration_hint_only_when_its_tools_exist() {
    const HINT: &str = "用 `plan` 写 DAG";

    let null_log = NullLog;
    let host = GoalInjectHost::default();
    let (_component, _project, plugin) = load_goal_guest(goal_fs_grant());
    let with_orchestrator = ToolFaceSession {
        tools: vec!["plan".into(), "dispatch".into(), "subagent".into()],
    };
    let caps = HostCaps {
        session: Some(&with_orchestrator),
        control: None,
        inject: Some(&host),
        ui: None,
        log: &null_log,
        cancel: None,
    };
    plugin
        .call(
            "goal",
            &serde_json::json!({"action": "start", "title": "hinted"}),
            &caps,
            &RecordingSink {
                blocks: Mutex::new(Vec::new()),
            },
        )
        .expect("start the goal");
    let injected = host.messages.lock().unwrap()[0].to_string();
    assert!(
        injected.contains(HINT),
        "the active-goal injection must carry the orchestration hint: {injected}"
    );

    // A status that is no longer `active` must drop the nudge even though the
    // orchestrator tools are still on the tool face.
    plugin
        .call(
            "goal",
            &serde_json::json!({"action": "pause"}),
            &caps,
            &RecordingSink {
                blocks: Mutex::new(Vec::new()),
            },
        )
        .expect("pause the goal");
    let paused = host.messages.lock().unwrap()[1].to_string();
    assert!(paused.contains("Paused goal"), "paused tag: {paused}");
    assert!(
        !paused.contains(HINT),
        "a paused goal must not carry the orchestration hint: {paused}"
    );

    // The live site is an install *without* the orchestrator but with other
    // tools: the tool face is not empty, it just lacks plan/dispatch/subagent.
    let host_bare = GoalInjectHost::default();
    let (_component2, _project2, plugin2) = load_goal_guest(goal_fs_grant());
    let without_orchestrator = ToolFaceSession {
        tools: vec!["bash".into(), "read".into(), "edit".into()],
    };
    let bare = HostCaps {
        session: Some(&without_orchestrator),
        control: None,
        inject: Some(&host_bare),
        ui: None,
        log: &null_log,
        cancel: None,
    };
    plugin2
        .call(
            "goal",
            &serde_json::json!({"action": "start", "title": "bare"}),
            &bare,
            &RecordingSink {
                blocks: Mutex::new(Vec::new()),
            },
        )
        .expect("start the goal");
    let injected_bare = host_bare.messages.lock().unwrap()[0].to_string();
    assert!(
        !injected_bare.contains(HINT),
        "no hint when the orchestration tools are absent: {injected_bare}"
    );
}

/// The core port: `goal` records state in `<project>/.ki/goals.json` in the
/// compiled shape (`{active, archived}`), and the lifecycle moves the goal
/// between the active slot and the archive. The plugin data directory is not a
/// second source of truth.
#[test]
fn goal_guest_writes_the_project_file_in_the_compiled_shape() {
    let (component, project, plugin) = load_goal_guest(goal_fs_grant());
    assert_eq!(plugin.id(), "kiqi-goal");
    assert!(plugin.tools().iter().any(|tool| tool.name == "goal"));
    assert!(plugin
        .commands()
        .iter()
        .any(|command| command.name == "goal"));

    let host = GoalInjectHost::default();
    let log = NullLog;
    let caps = goal_caps(&host, &log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    let file = project.path().join(".ki/goals.json");

    // start writes the project file in the compiled shape.
    plugin
        .call(
            "goal",
            &serde_json::json!({"action": "start", "title": "Ship the goal port"}),
            &caps,
            &sink,
        )
        .expect("start the goal");
    assert!(file.is_file(), "goal start writes <project>/.ki/goals.json");
    let value = read_json(&file);
    assert!(
        value["active"].is_object(),
        "compiled shape: an `active` object: {value}"
    );
    assert!(
        value["archived"].is_array(),
        "compiled shape: an `archived` array: {value}"
    );
    assert_eq!(value["active"]["title"], "Ship the goal port");
    assert_eq!(value["active"]["status"], "active");
    // The pre-scope jail holds nothing: the project file is the only truth.
    assert!(
        !component.path().join("data/goals.json").exists(),
        "no data-directory authority"
    );

    // The injection reached `InjectApi::append_message` as a bare custom message.
    {
        let messages = host.messages.lock().unwrap();
        assert_eq!(messages.len(), 1, "one injection on start");
        assert_eq!(messages[0]["kind"], "custom");
        assert_eq!(messages[0]["data"]["customType"], "goal");
    }

    // pause / resume flip the status in the file.
    plugin
        .call(
            "goal",
            &serde_json::json!({"action": "pause"}),
            &caps,
            &sink,
        )
        .expect("pause");
    assert_eq!(read_json(&file)["active"]["status"], "paused");
    plugin
        .call(
            "goal",
            &serde_json::json!({"action": "resume"}),
            &caps,
            &sink,
        )
        .expect("resume");
    assert_eq!(read_json(&file)["active"]["status"], "active");

    // complete moves the goal to the archive and clears the active slot.
    plugin
        .call(
            "goal",
            &serde_json::json!({"action": "complete"}),
            &caps,
            &sink,
        )
        .expect("complete");
    let value = read_json(&file);
    assert!(
        value["active"].is_null(),
        "complete clears the active slot: {value}"
    );
    assert_eq!(value["archived"][0]["title"], "Ship the goal port");
    assert_eq!(value["archived"][0]["status"], "complete");
}

/// The budget + fingerprint dedupe survive the move to the project file: a
/// `TurnEnd` (delivered as an untyped `other` event, kind `"TurnEnd"`) charges a
/// turn, re-injects only on a state change, and auto-pauses an exhausted goal.
#[test]
fn goal_guest_charges_the_turn_budget_from_the_project_file() {
    let (_component, project, plugin) = load_goal_guest(goal_fs_grant());
    let host = GoalInjectHost::default();
    let log = NullLog;
    let caps = goal_caps(&host, &log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    let file = project.path().join(".ki/goals.json");

    plugin
        .call(
            "goal",
            &serde_json::json!({"action": "start", "title": "Ship the goal port", "maxTurns": 2}),
            &caps,
            &sink,
        )
        .expect("start the goal");
    assert_eq!(read_json(&file)["active"]["title"], "Ship the goal port");

    // One TurnEnd charges a turn and does not re-inject (fingerprint unchanged).
    plugin.on_event(&Event::new(EventTag::TurnEnd, serde_json::json!({})), &caps);
    assert_eq!(
        host.messages.lock().unwrap().len(),
        1,
        "no re-injection for an unchanged goal"
    );
    let after_one = plugin
        .call("goal", &serde_json::json!({"action": "show"}), &caps, &sink)
        .expect("show after one turn");
    assert!(
        text_of(&after_one).contains("turns 1/2"),
        "got {}",
        text_of(&after_one)
    );

    // The second TurnEnd exhausts the budget: the goal auto-pauses and a notice
    // is injected.
    plugin.on_event(&Event::new(EventTag::TurnEnd, serde_json::json!({})), &caps);
    {
        let messages = host.messages.lock().unwrap();
        assert_eq!(
            messages.len(),
            2,
            "the budget notice is injected on exhaustion"
        );
        let notice = messages[1]["content"][0]["text"]
            .as_str()
            .expect("the notice has a text block");
        assert!(notice.contains("paused"), "got {notice}");
    }
    assert_eq!(read_json(&file)["active"]["status"], "paused");
    let paused = plugin
        .call("goal", &serde_json::json!({"action": "show"}), &caps, &sink)
        .expect("show after the budget is spent");
    assert!(
        text_of(&paused).contains("turns 2/2"),
        "got {}",
        text_of(&paused)
    );

    // The `/goal` command renders the same state.
    let status = plugin
        .run_command("goal", "status", &caps)
        .expect("goal status");
    assert!(status.contains("Ship the goal port"), "got {status}");
}

/// Migration: a pre-scope entry — the `{ "<project>": Goals }` map the guest
/// kept in its data directory — surfaces in the project file on first use.
///
/// The pre-scope store is reachable through the `fs` root only when that root
/// *is* the old data directory, so the fixture points the project scope there.
/// In the shipped layout the data directory sits outside the project tree, so a
/// real upgrade's migration is a no-op and the guest starts fresh.
#[test]
fn goal_guest_migrates_the_pre_scope_store() {
    let artifact = build_guest("kiqi-goal-guest", "kiqi_goal_guest.wasm");
    let component = tempfile::tempdir().expect("component tempdir");
    let wasm = component.path().join("plugin.wasm");
    std::fs::copy(&artifact, &wasm).expect("copy the guest");
    let data = component.path().join("data");
    std::fs::create_dir_all(&data).expect("data dir");
    let key = std::env::current_dir()
        .expect("process cwd")
        .to_string_lossy()
        .into_owned();
    let legacy = serde_json::json!({
        key: {"active": {"title": "Old project goal", "status": "active"}, "archived": []}
    });
    std::fs::write(data.join("goals.json"), legacy.to_string()).expect("seed the pre-scope store");

    let plugin = WasmPlugin::load_with_options(
        "kiqi-goal",
        &wasm,
        goal_fs_grant(),
        kiqi_plugin_host::Options {
            project_dir: Some(data.clone()),
            ..kiqi_plugin_host::Options::default()
        },
    )
    .expect("load");
    let host = GoalInjectHost::default();
    let log = NullLog;
    let caps = goal_caps(&host, &log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    // A read surfaces the migrated goal and writes it out.
    let shown = plugin
        .call("goal", &serde_json::json!({"action": "show"}), &caps, &sink)
        .expect("show");
    assert!(
        text_of(&shown).contains("Old project goal"),
        "the pre-scope goal is visible after migration: {}",
        text_of(&shown)
    );
    let file = data.join(".ki/goals.json");
    assert!(
        file.is_file(),
        "first use surfaces the pre-scope entry in the project file"
    );
    let value = read_json(&file);
    assert_eq!(value["active"]["title"], "Old project goal");
    assert_eq!(value["active"]["status"], "active");
    assert!(value["archived"].is_array());
}

/// The port degrades like the compiled plugin: no `fs` grant makes the write a
/// typed error (not a trap) while a read falls back to "no goal", and a missing
/// injection surface is non-fatal — the goal is still recorded.
#[test]
fn goal_guest_denies_without_fs_and_injection_is_optional() {
    // No fs: starting a goal cannot persist, so the action is a typed error.
    let (_component, _project, plugin) = load_goal_guest(Grants::default());
    let host = GoalInjectHost::default();
    let log = NullLog;
    let caps = goal_caps(&host, &log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    let denied = plugin
        .call(
            "goal",
            &serde_json::json!({"action": "start", "title": "nope"}),
            &caps,
            &sink,
        )
        .unwrap_err();
    assert!(
        matches!(&denied, PluginError::InvalidArguments(reason) if reason.contains("fs is not granted")),
        "got {denied}"
    );
    // A read without fs degrades to "no goal" rather than failing.
    let shown = plugin
        .call("goal", &serde_json::json!({"action": "show"}), &caps, &sink)
        .expect("show without fs");
    assert!(
        text_of(&shown).contains("No active goal"),
        "got {}",
        text_of(&shown)
    );

    // With fs but no injection surface, the goal is still recorded.
    let (_component, _project, plugin) = load_goal_guest(goal_fs_grant());
    let log = NullLog;
    let caps = HostCaps {
        session: None,
        control: None,
        inject: None,
        ui: None,
        log: &log,
        cancel: None,
    };
    let started = plugin
        .call(
            "goal",
            &serde_json::json!({"action": "start", "title": "recorded anyway"}),
            &caps,
            &sink,
        )
        .expect("start without an injection surface");
    assert!(
        text_of(&started).contains("Goal started"),
        "got {}",
        text_of(&started)
    );
}
