#![cfg(feature = "wasm")]
//! End-to-end: build the conformance guest, load it, call its tool, unload it.
//!
//! Requires the `wasm32-wasip2` target (`rustup target add wasm32-wasip2`); the
//! `Cargo test` CI job installs it. The guest is built here rather than checked
//! in, so the test cannot pass against a stale `.wasm`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use kiqi_host::wasm::{Grants, NetHttpGrant, WasmExtension};
use kiqi_host::{
    Block, CancelProbe, Event, EventAction, EventTag, Extension, ExtensionError, HostCaps, Log,
    LogLevel, Progress, ProgressSink, Verdict,
};

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
/// Several tests in this binary build the same guest, and they run concurrently under
/// the default `--test-threads`. They share one `--target-dir`, so while one `cargo
/// build` is rewriting the component another test can be told "already fresh" and
/// open it mid-write — observed as
/// `load …/kiqi_conformance.wasm: failed to read from …`.
///
/// The mutex serialises builds inside this binary, and the header check covers the
/// cross-process window (a sibling `cargo test` sharing the same target dir). Both
/// keep the shared target dir so cargo's incremental cache still applies.
static BUILD_GUEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn build_guest(crate_dir: &str, artifact: &str) -> PathBuf {
    // A poisoned lock means an earlier build panicked; that says nothing about the
    // filesystem, so carry on rather than propagating the panic.
    let _guard = BUILD_GUEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let manifest = root
        .join("extensions/wasm")
        .join(crate_dir)
        .join("Cargo.toml");
    let target_dir = root.join("extensions/wasm/target");
    let path = target_dir.join("wasm32-wasip2/release").join(artifact);
    let build = || {
        Command::new(env!("CARGO"))
            .args(["build", "--manifest-path"])
            .arg(&manifest)
            .args(["--target", "wasm32-wasip2", "--release", "--target-dir"])
            .arg(&target_dir)
            .status()
            .unwrap_or_else(|error| panic!("run cargo build for {crate_dir}: {error}"))
    };
    let status = build();
    assert!(
        status.success(),
        "building {crate_dir} failed; is the `wasm32-wasip2` target installed?"
    );
    // A component still being written by another process reads as a prefix; the
    // 4-byte module header tells a complete artifact from a torn write.
    if !has_wasm_header(&path) {
        assert!(
            build().success(),
            "rebuilding {crate_dir} after a torn artifact failed"
        );
    }
    path
}

/// Whether `path` starts with the WebAssembly module magic (`\0asm`).
fn has_wasm_header(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).is_ok() && magic == *b"\0asm"
}

fn fixture_wasm() -> PathBuf {
    build_guest("kiqi-conformance", "kiqi_conformance.wasm")
}

fn todo_wasm() -> PathBuf {
    build_guest("kiqi-todo", "kiqi_todo.wasm")
}

/// A conformance component with one no-argument core function turned into an
/// infinite loop and installed as the module's start function. The rest of the
/// component is unchanged, so `World::instantiate` really reaches guest start
/// code instead of failing early on missing exports.
fn spinning_start_wasm(dir: &Path) -> PathBuf {
    let component = std::fs::read(fixture_wasm()).expect("read conformance component");
    let mut output = component[..8].to_vec();
    let mut injected = false;
    for (id, range) in binary_sections(&component) {
        let mut body = component[range.clone()].to_vec();
        if id == 1 && !injected {
            if let Some(module) = module_with_spinning_start(&body) {
                body = module;
                injected = true;
            }
        }
        output.push(id);
        encode_u32(body.len() as u32, &mut output);
        output.extend_from_slice(&body);
    }
    assert!(injected, "a core module needs a no-argument function");
    let path = dir.join("spinning-start.wasm");
    std::fs::write(&path, output).expect("write spinning-start component");
    path
}

/// Yield `(section id, contents range)` for a binary module or component.
fn binary_sections(bytes: &[u8]) -> Vec<(u8, std::ops::Range<usize>)> {
    let mut sections = Vec::new();
    let mut cursor = 8;
    while cursor < bytes.len() {
        let id = bytes[cursor];
        cursor += 1;
        let (size, used) = decode_u32(&bytes[cursor..]);
        cursor += used;
        let end = cursor + size as usize;
        sections.push((id, cursor..end));
        cursor = end;
    }
    sections
}

fn decode_u32(bytes: &[u8]) -> (u32, usize) {
    let mut value = 0u32;
    for (index, byte) in bytes.iter().copied().enumerate() {
        value |= u32::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return (value, index + 1);
        }
    }
    panic!("unterminated LEB128");
}

fn encode_u32(mut value: u32, output: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            return;
        }
    }
}

fn module_with_spinning_start(bytes: &[u8]) -> Option<Vec<u8>> {
    use wasmparser::{CompositeInnerType, Imports, Payload, TypeRef};

    let mut no_argument_types = Vec::new();
    let mut imported_functions = 0u32;
    let mut candidates = Vec::new();
    let mut code_ordinal = 0u32;
    let mut spin_function = None;
    let mut body_position = None;
    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
        match payload.expect("parse core module") {
            Payload::TypeSection(reader) => {
                for group in reader {
                    for ty in group.expect("type group").into_types() {
                        no_argument_types.push(match ty.composite_type.inner {
                            CompositeInnerType::Func(function) => {
                                function.params().is_empty() && function.results().is_empty()
                            }
                            _ => false,
                        });
                    }
                }
            }
            Payload::ImportSection(reader) => {
                for group in reader {
                    match group.expect("import group") {
                        Imports::Single(_, import) => {
                            imported_functions += u32::from(matches!(import.ty, TypeRef::Func(_)));
                        }
                        Imports::Compact1 { items, .. } => {
                            for item in items {
                                imported_functions += u32::from(matches!(
                                    item.expect("compact import").ty,
                                    TypeRef::Func(_)
                                ));
                            }
                        }
                        Imports::Compact2 { ty, names, .. } => {
                            if matches!(ty, TypeRef::Func(_)) {
                                imported_functions += names.count();
                            }
                        }
                    }
                }
            }
            Payload::FunctionSection(reader) => {
                for (ordinal, ty) in reader.into_iter().enumerate() {
                    let type_index = ty.expect("function type");
                    if no_argument_types
                        .get(type_index as usize)
                        .copied()
                        .unwrap_or(false)
                    {
                        candidates.push(ordinal as u32);
                    }
                }
            }
            Payload::CodeSectionEntry(body) => {
                if body_position.is_none() && candidates.contains(&code_ordinal) {
                    let operators = body.get_binary_reader_for_operators().expect("operators");
                    let position = operators.original_position() as usize;
                    if position + 5 <= body.range().end as usize {
                        spin_function = Some(imported_functions + code_ordinal);
                        body_position = Some(position);
                    }
                }
                code_ordinal += 1;
            }
            _ => {}
        }
    }
    let function_index = spin_function?;
    let body_position = body_position?;
    let mut module = bytes.to_vec();
    // `loop { br 0 }`, followed by the original (now unreachable) body.
    module[body_position..body_position + 5].copy_from_slice(&[0x03, 0x40, 0x0c, 0x00, 0x0b]);

    let mut start = Vec::new();
    encode_u32(function_index, &mut start);
    let mut section = vec![8];
    encode_u32(start.len() as u32, &mut section);
    section.extend_from_slice(&start);
    let mut cursor = 8;
    let insertion = loop {
        if cursor == module.len() {
            break cursor;
        }
        let section_start = cursor;
        let id = module[cursor];
        cursor += 1;
        let (size, used) = decode_u32(&module[cursor..]);
        cursor += used;
        if id > 8 {
            break section_start;
        }
        cursor += size as usize;
    };
    module.splice(insertion..insertion, section);
    Some(module)
}

/// A `SessionRead` with a fixed branch, so the todo guest has something to
/// restore from.
struct FixedSession {
    branch: serde_json::Value,
}

impl kiqi_host::SessionRead for FixedSession {
    fn active_tools(&self) -> Vec<String> {
        Vec::new()
    }
    fn system_prompt(&self) -> String {
        String::new()
    }
    fn thinking_level(&self) -> String {
        String::new()
    }
    fn cli_flag(&self, _name: &str) -> Option<kiqi_host::FlagValue> {
        None
    }
    fn session_branch(&self) -> serde_json::Value {
        self.branch.clone()
    }
}

#[test]
fn loads_calls_streams_denies_and_unloads() {
    let wasm = fixture_wasm();
    let extension = WasmExtension::load("conformance", &wasm).expect("load the component");

    assert_eq!(extension.id(), "conformance");
    let log = NullLog;
    let caps = HostCaps::observer(&log);

    // The guest's declared tools are visible to the host.
    let tools = extension.tools();
    assert!(tools.iter().any(|tool| tool.name == "conformance"));
    assert!(tools.iter().any(|tool| tool.name == "fetch"));

    // A call: progress is streamed and the denied capability is reported.
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    let outcome = extension
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
    let error = extension
        .call("nope", &serde_json::json!({}), &caps, &sink)
        .unwrap_err();
    assert!(
        matches!(error, ExtensionError::UnknownTool(_)),
        "got {error}"
    );

    // Events carry an action: the fixture claims `BeforeAgentStart`.
    let claimed = extension.on_event(
        &Event::new(
            EventTag::BeforeAgentStart,
            serde_json::json!({"prompt": "hi"}),
        ),
        &caps,
    );
    assert_eq!(claimed, EventAction::Claimed, "a wasm extension can claim");
    let observed = extension.on_event(
        &Event::new(EventTag::SessionStart, serde_json::Value::Null),
        &caps,
    );
    assert_eq!(observed, EventAction::Continue);

    // Unload releases the instance; later calls fail cleanly instead of using a
    // released store.
    extension.unload();
    let after = extension
        .call("conformance", &serde_json::json!({}), &caps, &sink)
        .unwrap_err();
    assert!(after.to_string().contains("unloaded"), "got {after}");
}

/// The first real wasm extension: `kiqi-todo`. It proves the structured `details`
/// state channel and `session-read` (restoring from the branch) end to end.
#[test]
fn todo_guest_restores_from_the_session_branch() {
    let wasm = todo_wasm();
    let extension = WasmExtension::load("kiqi-todo", &wasm).expect("load the todo component");
    assert_eq!(extension.tools()[0].name, "todo");

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
    let action = extension.on_event(
        &Event::new(EventTag::SessionStart, serde_json::Value::Null),
        &caps,
    );
    assert_eq!(action, EventAction::Continue);

    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    let listed = extension
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
    let added = extension
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

impl kiqi_host::SessionRead for SwappingSession {
    fn active_tools(&self) -> Vec<String> {
        Vec::new()
    }
    fn system_prompt(&self) -> String {
        String::new()
    }
    fn thinking_level(&self) -> String {
        String::new()
    }
    fn cli_flag(&self, _name: &str) -> Option<kiqi_host::FlagValue> {
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
    let extension = WasmExtension::load("kiqi-todo", &wasm).expect("load the todo component");

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
    let list_text = |extension: &WasmExtension| -> String {
        let listed = extension
            .call("todo", &serde_json::json!({"action": "list"}), &caps, &sink)
            .expect("list");
        match &listed.blocks[0] {
            Block::Text(text) => text.clone(),
            other => panic!("unexpected block {other:?}"),
        }
    };

    // SessionStart restores branch A.
    let action = extension.on_event(
        &Event::new(EventTag::SessionStart, serde_json::Value::Null),
        &caps,
    );
    assert_eq!(action, EventAction::Continue);
    assert!(
        list_text(&extension).contains("branch A todo"),
        "restore A: {}",
        list_text(&extension)
    );

    // The branch swaps underneath the guest (the host moved to another
    // branch), then SessionTree arrives — the guest must re-restore from the
    // NEW branch, not keep A's state.
    *session.branch.lock().unwrap() = branch_with("branch B todo");
    let action = extension.on_event(
        &Event::new(EventTag::SessionTree, serde_json::Value::Null),
        &caps,
    );
    assert_eq!(action, EventAction::Continue);

    let text = list_text(&extension);
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
    let extension =
        WasmExtension::load_with_grants("conformance", &wasm, Grants::default()).expect("load");
    let denied = extension
        .call("fetch", &request, &caps, &sink)
        .expect("call");
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
    let extension =
        WasmExtension::load_with_grants("conformance", &wasm, wrong_host).expect("load");
    let denied = extension
        .call("fetch", &request, &caps, &sink)
        .expect("call");
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
    let extension = WasmExtension::load_with_grants("conformance", &wasm, granted).expect("load");
    let allowed = extension
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

fn text_of(outcome: &kiqi_host::ToolOutcome) -> String {
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
    build_guest("kiqi-langfuse", "kiqi_langfuse.wasm")
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
        // Observation is deny-by-default: without this the generation span
        // below would never be created.
        provider_request: Some(kiqi_host::grants::ProviderRequestGrant {
            observe: kiqi_host::grants::ObserveMode::Requests,
            ..Default::default()
        }),
        ..Default::default()
    };
    let extension = WasmExtension::load_with_grants("kiqi-langfuse", &wasm, grants).expect("load");
    assert!(extension
        .tools()
        .iter()
        .any(|tool| tool.name == "langfuse_config"));

    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    let configured = extension
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
        extension.on_event(&Event::new(tag, payload), &caps);
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

/// The same fixture, same turn, but no `provider-request` grant: the host must
/// not call the guest for `BeforeProviderRequest`, so no generation span exists.
/// This is the delivery half of the graded payload; the field stripping is
/// covered by the unit tests in the crate.
#[test]
fn provider_observation_is_not_delivered_without_a_grant() {
    let wasm = langfuse_wasm();
    let (base_url, server) = spawn_capturing_server();
    let grants = Grants {
        net_http: Some(NetHttpGrant {
            hosts: vec!["127.0.0.1".into()],
        }),
        ..Default::default()
    };
    let extension = WasmExtension::load_with_grants("kiqi-langfuse", &wasm, grants).expect("load");

    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let configured = extension
        .call(
            "langfuse_config",
            &serde_json::json!({
                "baseUrl": base_url,
                "publicKey": "pk-lf-test",
                "secretKey": "sk-lf-test",
            }),
            &caps,
            &RecordingSink {
                blocks: Mutex::new(Vec::new()),
            },
        )
        .expect("configure");
    assert!(!configured.is_error, "{configured:?}");

    let dispatch = |tag, payload| {
        extension.on_event(&Event::new(tag, payload), &caps);
    };
    dispatch(EventTag::SessionStart, serde_json::Value::Null);
    dispatch(
        EventTag::BeforeAgentStart,
        serde_json::json!({
            "prompt": "hello",
            "sessionId": "sess-deny",
            "host": { "name": "kiqi", "version": "9" },
        }),
    );
    dispatch(
        EventTag::BeforeProviderRequest,
        serde_json::json!({ "model": "ungranted-model" }),
    );
    dispatch(
        EventTag::MessageEnd,
        serde_json::json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": "hi there" }],
        }),
    );
    dispatch(EventTag::SessionShutdown, serde_json::Value::Null);

    let body = server.join().unwrap();
    assert!(body.contains("Conversational Turn"), "root span: {body}");
    assert!(
        !body.contains("LLM Call"),
        "an ungranted guest must not see the request: {body}"
    );
    assert!(
        !body.contains("ungranted-model"),
        "the payload must not reach the guest: {body}"
    );
}

/// All provider hooks cross the same deny-by-default wasm boundary: without a
/// `provider-request` grant none of the three tags reaches the guest. The
/// fixture claims every provider event it receives, so delivery is observable
/// from the host side as a Claimed verdict — AfterProviderResponse must stay
/// Continue forever (it has no wasm consumer).
#[test]
fn provider_hooks_require_a_grant_and_after_response_is_not_delivered() {
    let wasm = fixture_wasm();
    let events = [
        (EventTag::BeforeProviderRequest, "request-marker"),
        (EventTag::BeforeProviderHeaders, "headers-marker"),
        (EventTag::AfterProviderResponse, "response-marker"),
    ];

    let extension = WasmExtension::load_with_grants("conformance", &wasm, Grants::default())
        .expect("load without grants");
    let caps = HostCaps::observer(&NullLog);
    for (tag, marker) in events {
        let action = extension.on_event(
            &Event::new(tag, serde_json::json!({ "marker": marker })),
            &caps,
        );
        assert!(
            matches!(action, kiqi_host::EventAction::Continue),
            "an ungranted guest must not be called for {tag:?}: {action:?}"
        );
    }

    let grants = Grants {
        provider_request: Some(kiqi_host::grants::ProviderRequestGrant {
            observe: kiqi_host::grants::ObserveMode::Requests,
            ..Default::default()
        }),
        ..Default::default()
    };
    let extension = WasmExtension::load_with_grants("conformance-granted", &wasm, grants)
        .expect("load with observation grant");
    let caps = HostCaps::observer(&NullLog);
    let request = extension.on_event(
        &Event::new(
            EventTag::BeforeProviderRequest,
            serde_json::json!({ "marker": "request-marker" }),
        ),
        &caps,
    );
    assert!(
        matches!(request, kiqi_host::EventAction::Claimed),
        "a granted guest must receive BeforeProviderRequest: {request:?}"
    );
    let headers = extension.on_event(
        &Event::new(
            EventTag::BeforeProviderHeaders,
            serde_json::json!({ "marker": "headers-marker" }),
        ),
        &caps,
    );
    assert!(
        matches!(headers, kiqi_host::EventAction::Claimed),
        "a granted guest must receive BeforeProviderHeaders: {headers:?}"
    );
    let after = extension.on_event(
        &Event::new(
            EventTag::AfterProviderResponse,
            serde_json::json!({ "marker": "response-marker" }),
        ),
        &caps,
    );
    assert!(
        matches!(after, kiqi_host::EventAction::Continue),
        "AfterProviderResponse must never reach a wasm guest: {after:?}"
    );
}

/// Grants for the provider-gate fixture. `patch` is the switch under test;
/// observation is left off in most cases to prove the gate does not depend on
/// it (a patch-only grant still sees the request it patches).
fn gate_grants(patch: bool, timeout_ms: u64) -> Grants {
    Grants {
        provider_request: Some(kiqi_host::grants::ProviderRequestGrant {
            observe: kiqi_host::grants::ObserveMode::Requests,
            patch,
            patch_timeout_ms: timeout_ms,
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn gate_event(patch: serde_json::Value) -> Event {
    Event::new(
        EventTag::BeforeProviderRequest,
        serde_json::json!({ "model": "m", "gatePatch": patch }),
    )
}

/// A valid patch is returned to the caller (the guarded request path applies
/// it); the payload is the fixture's `gatePatch` verbatim.
#[test]
fn provider_gate_returns_a_validated_patch() {
    let wasm = fixture_wasm();
    let extension = WasmExtension::load_with_grants("conformance", &wasm, gate_grants(true, 500))
        .expect("load");
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let event = gate_event(serde_json::json!({
        "headers": { "x-trace": "abc" },
        "metadata": { "user_id": "u-1" },
        "timeoutMs": 1500,
        "maxTokens": 4096,
        "temperature": 0.5,
    }));

    let patch = extension
        .provider_gate(&event, &caps)
        .expect("a valid patch")
        .expect("the guest returned a patch");
    assert_eq!(patch["headers"]["x-trace"], "abc");
    assert_eq!(patch["metadata"]["user_id"], "u-1");
    assert_eq!(patch["timeoutMs"], 1500);
    assert_eq!(patch["maxTokens"], 4096);
    assert_eq!(patch["temperature"], 0.5);
}

/// One protected header rejects the WHOLE patch: the valid `maxTokens` next to
/// it must not be applied alone.
#[test]
fn provider_gate_rejects_the_whole_patch_on_one_bad_key() {
    let wasm = fixture_wasm();
    let extension = WasmExtension::load_with_grants("conformance", &wasm, gate_grants(true, 500))
        .expect("load");
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let event = gate_event(serde_json::json!({
        "maxTokens": 128,
        "headers": { "Authorization": "secret" },
    }));

    let error = extension
        .provider_gate(&event, &caps)
        .expect_err("the protected header rejects the patch");
    assert!(
        error.to_ascii_lowercase().contains("authorization"),
        "{error}"
    );
}

/// Without the `patch` switch the guest is never asked, even though it would
/// return a patch for this exact event.
#[test]
fn provider_gate_is_denied_without_a_patch_grant() {
    let wasm = fixture_wasm();
    let extension = WasmExtension::load_with_grants("conformance", &wasm, gate_grants(false, 500))
        .expect("load");
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let event = gate_event(serde_json::json!({ "maxTokens": 128 }));

    assert_eq!(extension.provider_gate(&event, &caps), Ok(None));
}

/// A patch returned for any other tag is ignored: the gate refuses it on the
/// tag check, and the observe/claiming dispatch drops the action.
#[test]
fn a_patch_for_a_non_provider_tag_is_ignored() {
    let wasm = fixture_wasm();
    let extension = WasmExtension::load_with_grants("conformance", &wasm, gate_grants(true, 500))
        .expect("load");
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let event = Event::new(
        EventTag::Context,
        serde_json::json!({ "tagPatch": { "maxTokens": 7 } }),
    );

    assert_eq!(extension.provider_gate(&event, &caps), Ok(None));
    assert_eq!(
        extension.on_event(&event, &caps),
        EventAction::Continue,
        "an observe dispatch must not surface a patch"
    );
}

/// A `UiApi` that records what the guest asked for and answers dialogs.
#[derive(Default)]
struct RecordingUi {
    dialogs: Mutex<Vec<serde_json::Value>>,
    statuses: Mutex<Vec<serde_json::Value>>,
}

impl kiqi_host::UiApi for RecordingUi {
    fn dialog(
        &self,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, kiqi_host::ExtensionError> {
        self.dialogs.lock().unwrap().push(request.clone());
        Ok(serde_json::json!({ "answer": "42" }))
    }
    fn set_status(&self, update: &serde_json::Value) -> Result<(), kiqi_host::ExtensionError> {
        self.statuses.lock().unwrap().push(update.clone());
        Ok(())
    }
    fn set_editor_text(&self, _edit: &serde_json::Value) -> Result<(), kiqi_host::ExtensionError> {
        Ok(())
    }
    fn component(
        &self,
        _name: &str,
        _payload: &serde_json::Value,
    ) -> Result<serde_json::Value, kiqi_host::ExtensionError> {
        Err(kiqi_host::ExtensionError::Failed(
            "no components here".to_string(),
        ))
    }
}

/// Gate §6.3.2: every contribution surface the ABI has must actually be
/// reachable, and the host must be the one asked.
#[test]
fn conformance_surface_is_reachable() {
    let wasm = fixture_wasm();
    let extension =
        WasmExtension::load_with_grants("conformance", &wasm, Grants::default()).expect("load");
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
    let tools: Vec<String> = extension
        .tools()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    for expected in ["conformance", "fetch", "dialog"] {
        assert!(
            tools.iter().any(|tool| tool == expected),
            "tool {expected}: {tools:?}"
        );
    }

    // commands, flags, shortcuts
    assert_eq!(extension.commands()[0].name, "conformance");
    assert_eq!(
        extension
            .run_command("conformance", "", &caps)
            .unwrap()
            .body,
        kiqi_host::CommandResultBody::Message("conformance command ran".to_string())
    );
    assert_eq!(extension.flags()[0].name, "conformance-flag");
    assert_eq!(extension.shortcuts()[0].key, "f8");

    // system-prompt transform
    let replaced = extension
        .before_agent_start(&serde_json::json!("SYS"), &caps)
        .expect("the guest replaces the prompt");
    assert!(
        replaced.contains("SYS") && replaced.contains("[conformance]"),
        "got {replaced}"
    );

    // probe gate
    assert_eq!(
        extension.probe_points(),
        vec!["before-tool-call".to_string()]
    );
    assert_eq!(
        extension.probe("before-tool-call", &serde_json::json!({}), &caps),
        Verdict::Continue
    );
    assert!(matches!(
        extension.probe(
            "before-tool-call",
            &serde_json::json!({ "x": "block" }),
            &caps
        ),
        Verdict::Block { .. }
    ));

    // ui.dialog round trip: the host really answered the guest.
    let answer = extension
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
    let outcome = extension
        .call("conformance", &serde_json::json!({}), &caps, &sink)
        .expect("call");
    assert!(
        text_of(&outcome).contains("ui=granted"),
        "got {}",
        text_of(&outcome)
    );
    assert!(!ui.statuses.lock().unwrap().is_empty());
}

/// Gate §6.3.7: an extension that hangs or panics must be contained, not take the
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
    let bounded = WasmExtension::load_with_options(
        "conformance",
        &wasm,
        Grants::default(),
        kiqi_host::wasm::Options {
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
        WasmExtension::load_with_grants("conformance", &wasm, Grants::default()).expect("load");
    let panic = panicking
        .call("panic", &serde_json::json!({}), &caps, &sink)
        .unwrap_err();
    assert!(!panic.to_string().is_empty());

    // The host is unaffected: another extension still loads and lists its tools.
    let survivor =
        WasmExtension::load_with_grants("conformance", &wasm, Grants::default()).expect("load");
    assert!(survivor
        .tools()
        .iter()
        .any(|tool| tool.name == "conformance"));
}

fn ask_user_wasm() -> PathBuf {
    build_guest("kiqi-ask-user", "kiqi_ask_user.wasm")
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

impl kiqi_host::UiApi for ScriptedUi {
    fn dialog(
        &self,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, kiqi_host::ExtensionError> {
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

    fn set_status(&self, _update: &serde_json::Value) -> Result<(), kiqi_host::ExtensionError> {
        Ok(())
    }
    fn set_editor_text(&self, _edit: &serde_json::Value) -> Result<(), kiqi_host::ExtensionError> {
        Ok(())
    }
    fn component(
        &self,
        _name: &str,
        _payload: &serde_json::Value,
    ) -> Result<serde_json::Value, kiqi_host::ExtensionError> {
        Err(kiqi_host::ExtensionError::Failed("none".to_string()))
    }
}

/// `kiqi-ask-user` on wasm: a question round-trips through the host dialog and
/// the answer rides the `details` channel.
#[test]
fn ask_user_round_trips_through_the_host_dialog() {
    let wasm = ask_user_wasm();
    let extension =
        WasmExtension::load_with_grants("kiqi-ask-user", &wasm, Grants::default()).expect("load");
    assert!(extension.tools().iter().any(|tool| tool.name == "ask_user"));

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

    let outcome = extension
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
    let confirm = extension
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

/// Gate §6.3.4: cancellation reaches the extension instead of being mistaken for
/// an answer.
#[test]
fn ask_user_reports_cancellation() {
    let wasm = ask_user_wasm();
    let extension =
        WasmExtension::load_with_grants("kiqi-ask-user", &wasm, Grants::default()).expect("load");
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

    let outcome = extension
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
/// observe event fan-out. While the guest call holds the extension's call lock
/// waiting for the user, an observe dispatch must skip instead of waiting:
/// the only person who can end the wait is the one the UI is trying to ask.
/// Without the skip this deadlocks the TUI (observed 2026-10-18).
#[test]
fn observe_events_skip_a_guest_that_is_waiting_on_the_ui() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let wasm = ask_user_wasm();
    let extension = Arc::new(
        WasmExtension::load_with_grants("kiqi-ask-user", &wasm, Grants::default()).expect("load"),
    );
    let ui = Arc::new(ScriptedUi::new(DialogMode::Pending));

    let ask = {
        let extension = extension.clone();
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
            extension.call(
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
        let extension = extension.clone();
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
            let action = extension.try_on_event(
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
    let extension =
        WasmExtension::load_with_grants("kiqi-ask-user", &wasm, Grants::default()).expect("load");
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

    let outcome = extension
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

/// `ki extension install` shows the user what a component declares. Only the
/// grantable interfaces count; `log`/`progress`/`ui` are not gated.
///
/// From 0.8.0 the three contribution exports (`oauth-api`, `provider-api`,
/// `render-api`) are mandatory world exports, so every component declares them
/// even when the extension only denies them. They are listed for disclosure and
/// stay ungranted: none of them is auto-granted by `--yes`.
#[test]
fn declared_capabilities_reads_the_components_imports_and_exports() {
    let wasm = fixture_wasm();
    let caps = kiqi_host::wasm::declared_capabilities(&wasm).expect("read component surface");
    assert_eq!(
        caps,
        vec![
            "exec".to_string(),
            "fs".to_string(),
            "net-http".to_string(),
            "net-sockets".to_string(),
            "oauth".to_string(),
            "proc".to_string(),
            "provider".to_string(),
            "render".to_string(),
        ],
        "got {caps:?}"
    );
}

/// Load the fixture from a scratch directory, so `fs`/`exec` write into a temp
/// data directory instead of the build output.
fn load_fixture_in_temp(grants: Grants) -> (tempfile::TempDir, WasmExtension) {
    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = dir.path().join("extension.wasm");
    std::fs::copy(fixture_wasm(), &wasm).expect("copy the fixture next to a writable data dir");
    let extension = WasmExtension::load_with_grants("conformance", &wasm, grants).expect("load");
    (dir, extension)
}

fn call_main_tool(
    extension: &WasmExtension,
    arguments: serde_json::Value,
) -> Result<kiqi_host::ToolOutcome, ExtensionError> {
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    extension.call("conformance", &arguments, &caps, &sink)
}

/// `fs` is a real capability, and the jail is its policy: granted, the guest
/// round-trips a file inside its own data directory.
#[test]
fn fs_is_granted_and_jailed_to_the_extensions_data_directory() {
    let (dir, extension) = load_fixture_in_temp(Grants {
        fs: Some(kiqi_host::grants::FsGrant {
            cwd: kiqi_host::grants::ExecCwd::ExtensionData,
        }),
        ..Grants::default()
    });

    let outcome = call_main_tool(&extension, serde_json::json!({})).expect("call");
    assert!(
        text_of(&outcome).contains("fs=conformance"),
        "the write/read round trip must succeed: {}",
        text_of(&outcome)
    );
    assert!(
        dir.path().join("data/conformance/probe.txt").is_file(),
        "the file lands inside the extension's own data directory"
    );
}

/// `exec` runs an allow-listed bare command and returns its output; a command
/// outside the grant (or no grant at all) is denied, not silently skipped.
#[test]
fn exec_runs_only_allow_listed_commands() {
    use kiqi_host::wasm::ExecGrant;

    let (_dir, extension) = load_fixture_in_temp(Grants {
        exec: Some(ExecGrant {
            commands: vec!["echo".to_string()],
            ..ExecGrant::default()
        }),
        ..Grants::default()
    });
    let allowed = call_main_tool(&extension, serde_json::json!({})).expect("call");
    assert!(
        text_of(&allowed).contains("exec=conformance"),
        "echo must run and its stdout come back: {}",
        text_of(&allowed)
    );

    // Same extension, a grant that does not include `echo`.
    let (_dir, extension) = load_fixture_in_temp(Grants {
        exec: Some(ExecGrant {
            commands: vec!["ls".to_string()],
            ..ExecGrant::default()
        }),
        ..Grants::default()
    });
    let denied = call_main_tool(&extension, serde_json::json!({})).expect("call");
    assert!(
        text_of(&denied).contains("exec=exec of `echo` is not granted"),
        "an unlisted command must report the deny reason: {}",
        text_of(&denied)
    );
}

/// Load the fixture with an explicit project directory: the data dir sits in
/// `dir`, the cwd scope `project` resolves against `project`.
fn load_fixture_with_project(
    grants: Grants,
) -> (tempfile::TempDir, tempfile::TempDir, WasmExtension) {
    let dir = tempfile::tempdir().expect("tempdir");
    let project = tempfile::tempdir().expect("project tempdir");
    let wasm = dir.path().join("extension.wasm");
    std::fs::copy(fixture_wasm(), &wasm).expect("copy the fixture next to a writable data dir");
    let extension = WasmExtension::load_with_options(
        "conformance",
        &wasm,
        grants,
        kiqi_host::wasm::Options {
            project_dir: Some(project.path().to_path_buf()),
            ..kiqi_host::wasm::Options::default()
        },
    )
    .expect("load");
    (dir, project, extension)
}

/// The grant's cwd scope decides where the child starts: the default is the
/// extension's own data directory, and a guest-supplied relative cwd resolves
/// inside it.
#[test]
fn exec_cwd_defaults_to_the_extension_data_directory() {
    use kiqi_host::wasm::ExecGrant;

    let (dir, extension) = load_fixture_in_temp(Grants {
        exec: Some(ExecGrant {
            commands: vec!["sh".to_string()],
            ..ExecGrant::default()
        }),
        ..Grants::default()
    });
    let outcome = call_main_tool(
        &extension,
        serde_json::json!({ "exec": { "cmd": "sh", "args": ["-c", "touch marker"] } }),
    )
    .expect("call");
    assert!(
        dir.path().join("data/marker").is_file(),
        "no cwd: the child starts in the data directory: {}",
        text_of(&outcome)
    );

    let outcome = call_main_tool(
        &extension,
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
        &extension,
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
    use kiqi_host::wasm::ExecGrant;

    let (dir, extension) = load_fixture_in_temp(Grants {
        exec: Some(ExecGrant {
            commands: vec!["sh".to_string()],
            ..ExecGrant::default()
        }),
        ..Grants::default()
    });
    for cwd in ["../outside", "/tmp"] {
        let outcome = call_main_tool(
            &extension,
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
    use kiqi_host::wasm::ExecGrant;

    let (dir, extension) = load_fixture_in_temp(Grants {
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
        &extension,
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
        &extension,
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
/// what a diagnostic extension needs to run `cargo`/`clippy` on the user's tree.
#[test]
fn exec_cwd_project_scope_runs_in_the_project_directory() {
    use kiqi_host::wasm::{ExecCwd, ExecGrant};

    let (dir, project, extension) = load_fixture_with_project(Grants {
        exec: Some(ExecGrant {
            commands: vec!["sh".to_string()],
            cwd: ExecCwd::Project,
            ..ExecGrant::default()
        }),
        ..Grants::default()
    });
    let outcome = call_main_tool(
        &extension,
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
        "and not in the extension's data directory"
    );

    // A relative cwd descends into an existing subdirectory.
    std::fs::create_dir_all(project.path().join("sub")).expect("project subdir");
    let outcome = call_main_tool(
        &extension,
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
        &extension,
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
        &extension,
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
    use kiqi_host::wasm::ExecGrant;

    let (_dir, extension) = load_fixture_in_temp(Grants {
        exec: Some(ExecGrant {
            commands: vec!["sh".to_string()],
            timeout_seconds: 1,
            ..ExecGrant::default()
        }),
        ..Grants::default()
    });
    let start = std::time::Instant::now();
    let outcome = call_main_tool(
        &extension,
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
    let wasm = build_guest("kiqi-codegraph", "kiqi_codegraph.wasm");
    let extension =
        WasmExtension::load_with_grants("kiqi-codegraph", &wasm, Grants::default()).expect("load");
    assert_eq!(extension.id(), "kiqi-codegraph");

    let mut names: Vec<String> = extension
        .tools()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
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
    // Every ported schema keeps the shape the compiled extension emitted: an
    // object schema whose properties always include `projectPath`.
    for spec in extension.tools() {
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
    let unknown = extension
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
    use kiqi_host::wasm::ExecCwd;
    use kiqi_host::wasm::ExecGrant;

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
        let wasm = build_guest("kiqi-codegraph", "kiqi_codegraph.wasm");
        let extension =
            WasmExtension::load_with_grants("kiqi-codegraph", &wasm, grants.clone()).expect("load");
        let error = extension
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
    use kiqi_host::wasm::{ExecCwd, ExecGrant, ProcGrant};

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
    let wasm = build_guest("kiqi-codegraph", "kiqi_codegraph.wasm");
    let extension = WasmExtension::load_with_grants(
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
    let outcome = extension
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
    use kiqi_host::wasm::ProcGrant;

    let wasm = build_guest("kiqi-orchestrator", "kiqi_orchestrator.wasm");
    let extension = WasmExtension::load_with_grants(
        "kiqi-orchestrator",
        &wasm,
        Grants {
            proc: Some(ProcGrant {
                commands: vec!["cat".to_string(), "test".to_string(), "sh".to_string()],
                cwd: kiqi_host::wasm::ExecCwd::Project,
                ..ProcGrant::default()
            }),
            ..Grants::default()
        },
    )
    .expect("load");
    assert_eq!(extension.id(), "kiqi-orchestrator");
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    let set = extension
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
    let wave = extension
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
    let wave = extension
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
    let refused = extension
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
    use kiqi_host::wasm::ProcGrant;

    let wasm = build_guest("kiqi-orchestrator", "kiqi_orchestrator.wasm");
    let extension = WasmExtension::load_with_grants(
        "kiqi-orchestrator",
        &wasm,
        Grants {
            proc: Some(ProcGrant {
                commands: vec!["cat".to_string()],
                cwd: kiqi_host::wasm::ExecCwd::Project,
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
    let outcome = extension
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
    let bare = WasmExtension::load_with_grants("kiqi-orchestrator", &wasm, Grants::default())
        .expect("load");
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

/// A probe that stays quiet through the pre-spawn check and the first in-loop
/// polls, then flips: the guest must notice while the child is running, not
/// merely refuse to spawn.
struct PollThenCancel(AtomicUsize);

impl CancelProbe for PollThenCancel {
    fn is_cancelled(&self) -> bool {
        // `fetch_add` returns the previous count: 0 = pre-spawn, 1 = first
        // in-loop poll, 2 = second in-loop poll (cancelled from here on).
        self.0.fetch_add(1, Ordering::SeqCst) >= 2
    }
}

/// A cancelled `subagent` must kill its child and return promptly instead of
/// waiting out `timeoutSeconds` while holding the extension's call mutex.
#[test]
fn orchestrator_guest_subagent_stops_when_the_host_cancels() {
    use kiqi_host::wasm::ProcGrant;

    let wasm = build_guest("kiqi-orchestrator", "kiqi_orchestrator.wasm");
    let extension = WasmExtension::load_with_grants(
        "kiqi-orchestrator",
        &wasm,
        Grants {
            proc: Some(ProcGrant {
                commands: vec!["sh".to_string()],
                cwd: kiqi_host::wasm::ExecCwd::Project,
                ..ProcGrant::default()
            }),
            ..Grants::default()
        },
    )
    .expect("load");
    let log = NullLog;
    let probe = PollThenCancel(AtomicUsize::new(0));
    let caps = HostCaps {
        cancel: Some(&probe),
        ..HostCaps::observer(&log)
    };
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    let started = std::time::Instant::now();
    let error = extension
        .call(
            "subagent",
            &serde_json::json!({
                "program": "sh",
                "task": "sleep 2; echo late",
                "timeoutSeconds": 10
            }),
            &caps,
            &sink,
        )
        .expect_err("the host cancelled the call");
    let elapsed = started.elapsed();

    assert!(
        error.to_string().contains("cancelled by the host"),
        "got {error}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "the guest must kill the child instead of waiting out `sleep 2`: {elapsed:?}"
    );
}

/// A session switch clears the plan, matching the compiled extension.
#[test]
fn orchestrator_guest_session_switch_clears_the_plan() {
    let wasm = build_guest("kiqi-orchestrator", "kiqi_orchestrator.wasm");
    let extension = WasmExtension::load_with_grants("kiqi-orchestrator", &wasm, Grants::default())
        .expect("load");
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    let _ = extension.before_agent_start(
        &serde_json::json!({"sessionId": "s1", "systemPrompt": "base"}),
        &caps,
    );
    extension
        .call(
            "plan",
            &serde_json::json!({"action": "set", "nodes": [
                {"id": "a", "task": "x"}
            ]}),
            &caps,
            &sink,
        )
        .expect("plan set");

    let _ = extension.before_agent_start(
        &serde_json::json!({"sessionId": "s2", "systemPrompt": "base"}),
        &caps,
    );
    let shown = extension
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
    use kiqi_host::wasm::{ExecCwd, FsGrant};

    let wasm = build_guest("kiqi-orchestrator", "kiqi_orchestrator.wasm");
    let project = tempfile::tempdir().expect("project tempdir");
    let extension = WasmExtension::load_with_options(
        "kiqi-orchestrator",
        &wasm,
        Grants {
            fs: Some(FsGrant {
                cwd: ExecCwd::Project,
            }),
            ..Grants::default()
        },
        kiqi_host::wasm::Options {
            project_dir: Some(project.path().to_path_buf()),
            ..kiqi_host::wasm::Options::default()
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
        extension
            .before_agent_start(&prompt("base"), &caps)
            .is_none(),
        "no goal file means no guidance"
    );

    // An active goal turns it on, over the base prompt.
    std::fs::write(&goals, r#"{"active":{"status":"active"},"archived":[]}"#).expect("write");
    let injected = extension
        .before_agent_start(&prompt("base"), &caps)
        .expect("the section is appended");
    assert!(
        injected.contains("## Subagent orchestration") && injected.contains("base"),
        "the base prompt plus the section: {injected}"
    );

    // The heading is never appended twice (idempotent).
    let twice = extension.before_agent_start(
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
            extension
                .before_agent_start(&prompt("base"), &caps)
                .is_none(),
            "`{state}` means no guidance"
        );
    }
}

/// Without the `fs` grant the guest cannot read the goal file, so the guidance
/// stays off — the compiled extension had no capability gate, a guest fails closed.
#[test]
fn orchestrator_guest_guidance_needs_the_fs_grant() {
    let wasm = build_guest("kiqi-orchestrator", "kiqi_orchestrator.wasm");
    let project = tempfile::tempdir().expect("project tempdir");
    std::fs::create_dir_all(project.path().join(".ki")).expect("state dir");
    std::fs::write(
        project.path().join(".ki/goals.json"),
        r#"{"active":{"status":"active"},"archived":[]}"#,
    )
    .expect("write");
    let extension = WasmExtension::load_with_options(
        "kiqi-orchestrator",
        &wasm,
        Grants::default(),
        kiqi_host::wasm::Options {
            project_dir: Some(project.path().to_path_buf()),
            ..kiqi_host::wasm::Options::default()
        },
    )
    .expect("load");
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    assert!(
        extension
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
    use kiqi_host::wasm::{ExecCwd, FsGrant};

    // One project tree, shared by both guests (their `fs --cwd project` root).
    let project = tempfile::tempdir().expect("project tempdir");

    // The goal guest starts a goal through its own `goal` tool.
    let goal_artifact = build_guest("kiqi-goal", "kiqi_goal.wasm");
    let goal_component = tempfile::tempdir().expect("goal component tempdir");
    let goal_wasm = goal_component.path().join("extension.wasm");
    std::fs::copy(&goal_artifact, &goal_wasm).expect("copy the goal guest");
    let goal_extension = WasmExtension::load_with_options(
        "kiqi-goal",
        &goal_wasm,
        goal_fs_grant(),
        kiqi_host::wasm::Options {
            project_dir: Some(project.path().to_path_buf()),
            ..kiqi_host::wasm::Options::default()
        },
    )
    .expect("load the goal guest");
    let inject_host = GoalInjectHost::default();
    let log = NullLog;
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    goal_extension
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
    let orchestrator_artifact = build_guest("kiqi-orchestrator", "kiqi_orchestrator.wasm");
    let orchestrator = WasmExtension::load_with_options(
        "kiqi-orchestrator",
        &orchestrator_artifact,
        Grants {
            fs: Some(FsGrant {
                cwd: ExecCwd::Project,
            }),
            ..Grants::default()
        },
        kiqi_host::wasm::Options {
            project_dir: Some(project.path().to_path_buf()),
            ..kiqi_host::wasm::Options::default()
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

/// The fs grant's scope: the default is the extension's own data directory
/// (unchanged), and `--cwd project` (via the CLI options) opts the guest into
/// reading and writing the project tree.
#[test]
fn fs_project_scope_reads_and_writes_the_project_tree() {
    use kiqi_host::wasm::{ExecCwd, FsGrant};

    let project = tempfile::tempdir().expect("project tempdir");
    std::fs::write(project.path().join("notes.txt"), b"project file").expect("seed file");
    let wasm = fixture_wasm();
    let extension = WasmExtension::load_with_options(
        "conformance",
        &wasm,
        Grants {
            fs: Some(FsGrant {
                cwd: ExecCwd::Project,
            }),
            ..Grants::default()
        },
        kiqi_host::wasm::Options {
            project_dir: Some(project.path().to_path_buf()),
            ..kiqi_host::wasm::Options::default()
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
    let outcome = call_main_tool(&extension, serde_json::json!({})).expect("call");
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
    use kiqi_host::wasm::ProcGrant;

    let (dir, extension) = load_fixture_in_temp(Grants {
        proc: Some(ProcGrant {
            commands: vec!["sh".to_string()],
            ..ProcGrant::default()
        }),
        ..Grants::default()
    });
    let outcome = call_main_tool(
        &extension,
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
    // The child is a real process in the extension's own data directory.
    assert!(dir.path().join("data").is_dir());
}

/// `proc` is gated exactly like `exec`: no grant, a command outside the
/// allow-list, and a cwd that leaves the scope are each refused.
#[test]
fn proc_is_gated_like_exec() {
    use kiqi_host::wasm::ProcGrant;

    let (_dir, extension) = load_fixture_in_temp(Grants::default());
    let denied = call_main_tool(
        &extension,
        serde_json::json!({"proc": {"cmd": "sh", "args": ["-c", "true"]}}),
    )
    .expect("call");
    assert!(
        text_of(&denied).contains("proc=proc is not granted"),
        "an ungranted proc is denied: {}",
        text_of(&denied)
    );

    let (_dir, extension) = load_fixture_in_temp(Grants {
        proc: Some(ProcGrant {
            commands: vec!["ls".to_string()],
            ..ProcGrant::default()
        }),
        ..Grants::default()
    });
    let denied = call_main_tool(
        &extension,
        serde_json::json!({"proc": {"cmd": "sh", "args": ["-c", "true"]}}),
    )
    .expect("call");
    assert!(
        text_of(&denied).contains("proc=proc of `sh` is not granted"),
        "a command outside the allow-list is denied by name: {}",
        text_of(&denied)
    );

    // The cwd scope is the same jail `exec` uses.
    let (_dir, extension) = load_fixture_in_temp(Grants {
        proc: Some(ProcGrant {
            commands: vec!["sh".to_string()],
            ..ProcGrant::default()
        }),
        ..Grants::default()
    });
    let escaped = call_main_tool(
        &extension,
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
    use kiqi_host::wasm::ProcGrant;

    let (_dir, extension) = load_fixture_in_temp(Grants {
        proc: Some(ProcGrant {
            commands: vec!["sh".to_string()],
            max_open: 1,
            ..ProcGrant::default()
        }),
        ..Grants::default()
    });
    let outcome = call_main_tool(
        &extension,
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
    use kiqi_host::wasm::ProcGrant;

    let (_dir, extension) = load_fixture_in_temp(Grants {
        proc: Some(ProcGrant {
            commands: vec!["sh".to_string()],
            ..ProcGrant::default()
        }),
        ..Grants::default()
    });
    // The fixture reads the child's first stdout line = its pid, writes nothing
    // useful and leaves the child sleeping (`keep`): the guest never kills it.
    let outcome = call_main_tool(
        &extension,
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
    use kiqi_host::wasm::NetSocketsGrant;
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a loopback listener");
    let address = listener.local_addr().unwrap().to_string();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buffer = [0u8; 64];
        let read = stream.read(&mut buffer).expect("read the ping");
        stream.write_all(&buffer[..read]).expect("echo it back");
    });

    let (_dir, extension) = load_fixture_in_temp(Grants {
        net_sockets: Some(NetSocketsGrant {
            hosts: vec!["127.0.0.1".to_string()],
        }),
        ..Grants::default()
    });
    let outcome =
        call_main_tool(&extension, serde_json::json!({ "address": address })).expect("call");
    server.join().expect("server thread");
    assert!(
        text_of(&outcome).contains("socket=sent=4 recv=ping"),
        "the loopback round trip must succeed: {}",
        text_of(&outcome)
    );

    // A host outside the grant is denied before any connection is attempted.
    let (_dir, extension) = load_fixture_in_temp(Grants {
        net_sockets: Some(NetSocketsGrant {
            hosts: vec!["example.com".to_string()],
        }),
        ..Grants::default()
    });
    let denied =
        call_main_tool(&extension, serde_json::json!({ "address": "127.0.0.1:9" })).expect("call");
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
    let extension =
        WasmExtension::load("installed-as", &fixture_wasm()).expect("load the component");
    assert_eq!(
        extension.id(),
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
            kiqi_host::wasm::capability_is_enforced(enforced),
            "`{enforced}` is implemented and must be advertised as working"
        );
    }
    assert!(!kiqi_host::wasm::capability_is_enforced(
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

    let (_dir, extension) = load_fixture_in_temp(Grants {
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
    let outcome = extension
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

    let bounded = WasmExtension::load_with_options(
        "conformance",
        &wasm,
        Grants::default(),
        kiqi_host::wasm::Options {
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

    let (_dir, extension) = load_fixture_in_temp(Grants {
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
    let outcome = extension
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
    use kiqi_host::wasm::ExecGrant;

    // Build outside the timed window: `fixture_wasm()` runs `cargo build`, and
    // with the whole suite building guests in parallel that can take longer than
    // the call is allowed to take.
    let _ = fixture_wasm();

    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (_dir, extension) = load_fixture_in_temp(Grants {
            exec: Some(ExecGrant {
                commands: vec!["sh".to_string()],
                ..ExecGrant::default()
            }),
            ..Grants::default()
        });
        let outcome = call_main_tool(
            &extension,
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
    let wasm = build_guest("kiqi-webfetch", "kiqi_webfetch.wasm");
    let extension = WasmExtension::load("kiqi-webfetch", &wasm).expect("load");
    assert_eq!(extension.id(), "kiqi-webfetch");
    assert!(
        extension.tools().iter().any(|tool| tool.name == "webfetch"),
        "the tool is declared: {:?}",
        extension.tools()
    );

    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    // The extension's own policy, not the host's: localhost and literal private IPs
    // are refused inside the guest.
    let local = extension
        .call(
            "webfetch",
            &serde_json::json!({ "url": "http://localhost/" }),
            &caps,
            &sink,
        )
        .unwrap_err();
    assert!(local.to_string().contains("local hosts"), "got {local}");
    let private = extension
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
    let denied = extension
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
fn kiqi_lens_runs_checks_in_the_project_directory() {
    use kiqi_host::wasm::{ExecCwd, ExecGrant};

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
        eprintln!("skipping kiqi_lens_runs_checks_in_the_project_directory: no git on PATH");
        return;
    }

    let wasm = build_guest("kiqi-lens", "kiqi_lens.wasm");
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
    let extension = WasmExtension::load_with_options(
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
        kiqi_host::wasm::Options {
            project_dir: Some(project.path().to_path_buf()),
            ..kiqi_host::wasm::Options::default()
        },
    )
    .expect("load");
    assert_eq!(extension.id(), "kiqi-lens");
    assert!(
        extension
            .tools()
            .iter()
            .any(|tool| tool.name == "code_lens"),
        "the tool is declared: {:?}",
        extension.tools()
    );
    let outcome = extension
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
    let outcome = extension
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
    let outcome = extension
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
    let refused = extension
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
    let extension = WasmExtension::load_with_grants(
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
    let outcome = extension
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

    // Same extension without the project scope: the child starts in the data
    // directory, which is not a git repository.
    let dir = tempfile::tempdir().expect("install tempdir");
    let scoped_wasm = dir.path().join("extension.wasm");
    std::fs::copy(&wasm, &scoped_wasm).expect("copy the guest next to a data dir");
    let extension = WasmExtension::load_with_grants(
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
    let outcome = extension
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
    // The advice must name BOTH commands the tool can run — `ki extension grant`
    // replaces the whole exec policy, so advice naming only the denied command
    // would steer the user into revoking the other one.
    let extension =
        WasmExtension::load_with_grants("kiqi-lens", &wasm, Grants::default()).expect("load");
    let denied = extension
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

impl kiqi_host::SessionRead for PlanSessionHost {
    fn active_tools(&self) -> Vec<String> {
        self.active.lock().unwrap().clone()
    }
    fn system_prompt(&self) -> String {
        String::new()
    }
    fn thinking_level(&self) -> String {
        String::new()
    }
    fn cli_flag(&self, _name: &str) -> Option<kiqi_host::FlagValue> {
        None
    }
    fn session_branch(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
}

impl kiqi_host::SessionControl for PlanSessionHost {
    fn set_session_name(&self, _name: &str) -> Result<(), ExtensionError> {
        Ok(())
    }
    fn set_active_tools(&self, tools: &[String]) -> Result<(), ExtensionError> {
        *self.active.lock().unwrap() = tools.to_vec();
        Ok(())
    }
    fn set_model(&self, _model: &str) -> Result<(), ExtensionError> {
        Ok(())
    }
    fn set_thinking_level(&self, _level: &str) -> Result<(), ExtensionError> {
        Ok(())
    }
    fn compact(&self) -> Result<(), ExtensionError> {
        Ok(())
    }
    fn new_session(&self) -> Result<(), ExtensionError> {
        Ok(())
    }
    fn fork(&self) -> Result<(), ExtensionError> {
        Ok(())
    }
    fn navigate_tree(&self, _target: &str) -> Result<(), ExtensionError> {
        Ok(())
    }
    fn switch_session(&self, _id: &str) -> Result<(), ExtensionError> {
        Ok(())
    }
    fn reload(&self) -> Result<(), ExtensionError> {
        Ok(())
    }
}

impl kiqi_host::InjectApi for PlanSessionHost {
    fn send_message(&self, _message: &serde_json::Value) -> Result<(), ExtensionError> {
        Ok(())
    }
    fn send_user_message(&self, text: &str) -> Result<(), ExtensionError> {
        self.messages.lock().unwrap().push(text.to_string());
        Ok(())
    }
    fn append_message(&self, _message: &serde_json::Value) -> Result<(), ExtensionError> {
        Ok(())
    }
    fn append_custom_entry(
        &self,
        custom_type: &str,
        data: &serde_json::Value,
    ) -> Result<(), ExtensionError> {
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
    let wasm = build_guest("kiqi-plan-mode", "kiqi_plan_mode.wasm");
    let extension = WasmExtension::load("kiqi-plan-mode", &wasm).expect("load");
    assert_eq!(extension.id(), "kiqi-plan-mode");
    assert!(extension
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
    extension
        .call("plan_mode_start", &serde_json::json!({}), &caps, &sink)
        .expect("start plan mode");
    assert_eq!(
        *host.active.lock().unwrap(),
        vec!["read", "bash", "docs", "plan_mode_complete"],
        "planning keeps only the read-only set plus the complete tool"
    );

    // Complete: the previous set comes back and the plan is persisted.
    extension
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
    let status = extension
        .run_command("plan", "show", &caps)
        .expect("plan show");
    let kiqi_host::CommandResultBody::Message(status) = status.body else {
        panic!("plan show returns a message");
    };
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
    let wasm = build_guest("kiqi-websearch", "kiqi_websearch.wasm");
    let extension =
        WasmExtension::load_with_grants("kiqi-websearch", &wasm, Grants::default()).expect("load");
    assert_eq!(extension.id(), "kiqi-websearch");
    assert!(
        extension
            .tools()
            .iter()
            .any(|tool| tool.name == "websearch"),
        "the tool is declared: {:?}",
        extension.tools()
    );

    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };

    // The guest's own argument policy, before any capability call: a missing or
    // empty query never reaches the network.
    let missing = extension
        .call("websearch", &serde_json::json!({}), &caps, &sink)
        .unwrap_err();
    assert!(
        missing.to_string().contains("query is required"),
        "got {missing}"
    );
    let empty = extension
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
    let denied = extension
        .call(
            "websearch",
            &serde_json::json!({ "query": "rust" }),
            &caps,
            &sink,
        )
        .unwrap_err();
    assert!(denied.to_string().contains("denied"), "got {denied}");

    // A grant for some unrelated host still does not cover api.duckduckgo.com.
    let other = WasmExtension::load_with_grants(
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
    let wasm = build_guest("kiqi-websearch", "kiqi_websearch.wasm");
    let grants = Grants {
        net_http: Some(NetHttpGrant {
            hosts: vec!["api.duckduckgo.com".into(), "en.wikipedia.org".into()],
        }),
        ..Grants::default()
    };
    let extension = WasmExtension::load_with_grants("kiqi-websearch", &wasm, grants).expect("load");
    let log = NullLog;
    let caps = HostCaps::observer(&log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    let outcome = extension
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
/// emit the *same* delegation envelope bytes the compile-time extension emits, so
/// the envelope is compared field-order-and-value, byte for byte.
#[test]
fn subagents_guest_emits_the_byte_stable_delegation_envelope() {
    let wasm = build_guest("kiqi-subagents", "kiqi_subagents.wasm");
    let extension = WasmExtension::load("kiqi-subagents", &wasm).expect("load");
    assert_eq!(extension.id(), "kiqi-subagents");

    let tool = extension
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

    // Clamping and field order match the compile-time extension exactly.
    let outcome = extension
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
    let defaults = extension
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
    let roled = extension
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
    let empty = extension
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
    let missing = extension
        .call("delegate_task", &serde_json::json!({}), &caps, &sink)
        .unwrap_err();
    assert!(
        missing
            .to_string()
            .contains("missing string parameter `task`"),
        "got {missing}"
    );
    let non_object = extension
        .call("delegate_task", &serde_json::Value::Null, &caps, &sink)
        .unwrap_err();
    assert!(
        non_object
            .to_string()
            .contains("missing string parameter `task`"),
        "got {non_object}"
    );

    // An undeclared tool is a typed error, not a trap.
    let unknown = extension
        .call("no_such_tool", &serde_json::json!({}), &caps, &sink)
        .unwrap_err();
    assert!(
        matches!(unknown, ExtensionError::UnknownTool(_)),
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

impl kiqi_host::InjectApi for GoalInjectHost {
    fn send_message(&self, _message: &serde_json::Value) -> Result<(), ExtensionError> {
        Ok(())
    }
    fn send_user_message(&self, _text: &str) -> Result<(), ExtensionError> {
        Ok(())
    }
    fn append_message(&self, message: &serde_json::Value) -> Result<(), ExtensionError> {
        self.messages.lock().unwrap().push(message.clone());
        Ok(())
    }
    fn append_custom_entry(
        &self,
        _custom_type: &str,
        _data: &serde_json::Value,
    ) -> Result<(), ExtensionError> {
        Ok(())
    }
}

/// The `fs` grant both goal and orchestrator tests need: the project tree
/// (`--cwd project`), so state lands in `<project>/.ki/goals.json` and never in
/// the repo or the build output.
fn goal_fs_grant() -> Grants {
    Grants {
        fs: Some(kiqi_host::grants::FsGrant {
            cwd: kiqi_host::grants::ExecCwd::Project,
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
fn load_goal_guest(grants: Grants) -> (tempfile::TempDir, tempfile::TempDir, WasmExtension) {
    let artifact = build_guest("kiqi-goal", "kiqi_goal.wasm");
    let component = tempfile::tempdir().expect("component tempdir");
    let project = tempfile::tempdir().expect("project tempdir");
    let wasm = component.path().join("extension.wasm");
    std::fs::copy(&artifact, &wasm).expect("copy the guest next to its component dir");
    let extension = WasmExtension::load_with_options(
        "kiqi-goal",
        &wasm,
        grants,
        kiqi_host::wasm::Options {
            project_dir: Some(project.path().to_path_buf()),
            ..kiqi_host::wasm::Options::default()
        },
    )
    .expect("load");
    (component, project, extension)
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

impl kiqi_host::SessionRead for ToolFaceSession {
    fn active_tools(&self) -> Vec<String> {
        self.tools.clone()
    }
    fn system_prompt(&self) -> String {
        String::new()
    }
    fn thinking_level(&self) -> String {
        String::new()
    }
    fn cli_flag(&self, _name: &str) -> Option<kiqi_host::FlagValue> {
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
    let (_component, _project, extension) = load_goal_guest(goal_fs_grant());
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
    extension
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
    extension
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
    let (_component2, _project2, extension2) = load_goal_guest(goal_fs_grant());
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
    extension2
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
/// between the active slot and the archive. The extension data directory is not a
/// second source of truth.
#[test]
fn goal_guest_writes_the_project_file_in_the_compiled_shape() {
    let (component, project, extension) = load_goal_guest(goal_fs_grant());
    assert_eq!(extension.id(), "kiqi-goal");
    assert!(extension.tools().iter().any(|tool| tool.name == "goal"));
    assert!(extension
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
    extension
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
    extension
        .call(
            "goal",
            &serde_json::json!({"action": "pause"}),
            &caps,
            &sink,
        )
        .expect("pause");
    assert_eq!(read_json(&file)["active"]["status"], "paused");
    extension
        .call(
            "goal",
            &serde_json::json!({"action": "resume"}),
            &caps,
            &sink,
        )
        .expect("resume");
    assert_eq!(read_json(&file)["active"]["status"], "active");

    // complete moves the goal to the archive and clears the active slot.
    extension
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
    let (_component, project, extension) = load_goal_guest(goal_fs_grant());
    let host = GoalInjectHost::default();
    let log = NullLog;
    let caps = goal_caps(&host, &log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    let file = project.path().join(".ki/goals.json");

    extension
        .call(
            "goal",
            &serde_json::json!({"action": "start", "title": "Ship the goal port", "maxTurns": 2}),
            &caps,
            &sink,
        )
        .expect("start the goal");
    assert_eq!(read_json(&file)["active"]["title"], "Ship the goal port");

    // One TurnEnd charges a turn and does not re-inject (fingerprint unchanged).
    extension.on_event(&Event::new(EventTag::TurnEnd, serde_json::json!({})), &caps);
    assert_eq!(
        host.messages.lock().unwrap().len(),
        1,
        "no re-injection for an unchanged goal"
    );
    let after_one = extension
        .call("goal", &serde_json::json!({"action": "show"}), &caps, &sink)
        .expect("show after one turn");
    assert!(
        text_of(&after_one).contains("turns 1/2"),
        "got {}",
        text_of(&after_one)
    );

    // The second TurnEnd exhausts the budget: the goal auto-pauses and a notice
    // is injected.
    extension.on_event(&Event::new(EventTag::TurnEnd, serde_json::json!({})), &caps);
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
    let paused = extension
        .call("goal", &serde_json::json!({"action": "show"}), &caps, &sink)
        .expect("show after the budget is spent");
    assert!(
        text_of(&paused).contains("turns 2/2"),
        "got {}",
        text_of(&paused)
    );

    // The `/goal` command renders the same state.
    let status = extension
        .run_command("goal", "status", &caps)
        .expect("goal status");
    let kiqi_host::CommandResultBody::Message(status) = status.body else {
        panic!("goal status returns a message");
    };
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
    let artifact = build_guest("kiqi-goal", "kiqi_goal.wasm");
    let component = tempfile::tempdir().expect("component tempdir");
    let wasm = component.path().join("extension.wasm");
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

    let extension = WasmExtension::load_with_options(
        "kiqi-goal",
        &wasm,
        goal_fs_grant(),
        kiqi_host::wasm::Options {
            project_dir: Some(data.clone()),
            ..kiqi_host::wasm::Options::default()
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
    let shown = extension
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

/// The port degrades like the compiled extension: no `fs` grant makes the write a
/// typed error (not a trap) while a read falls back to "no goal", and a missing
/// injection surface is non-fatal — the goal is still recorded.
#[test]
fn goal_guest_denies_without_fs_and_injection_is_optional() {
    // No fs: starting a goal cannot persist, so the action is a typed error.
    let (_component, _project, extension) = load_goal_guest(Grants::default());
    let host = GoalInjectHost::default();
    let log = NullLog;
    let caps = goal_caps(&host, &log);
    let sink = RecordingSink {
        blocks: Mutex::new(Vec::new()),
    };
    let denied = extension
        .call(
            "goal",
            &serde_json::json!({"action": "start", "title": "nope"}),
            &caps,
            &sink,
        )
        .unwrap_err();
    assert!(
        matches!(&denied, ExtensionError::InvalidArguments(reason) if reason.contains("fs is not granted")),
        "got {denied}"
    );
    // A read without fs degrades to "no goal" rather than failing.
    let shown = extension
        .call("goal", &serde_json::json!({"action": "show"}), &caps, &sink)
        .expect("show without fs");
    assert!(
        text_of(&shown).contains("No active goal"),
        "got {}",
        text_of(&shown)
    );

    // With fs but no injection surface, the goal is still recorded.
    let (_component, _project, extension) = load_goal_guest(goal_fs_grant());
    let log = NullLog;
    let caps = HostCaps {
        session: None,
        control: None,
        inject: None,
        ui: None,
        log: &log,
        cancel: None,
    };
    let started = extension
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

// ---------------------------------------------------------------------------
// Markdown render (`render-api`)
//
// The render instance is deliberately not a tool instance: deny-all grants,
// no host call context, and a fresh thread per call. These tests pin that from
// the guest side, where it matters.
// ---------------------------------------------------------------------------

use kiqi_host::wasm::{RenderGrant, RenderHost, RENDER_KIND_MARKDOWN};

fn render_grant(max_output_bytes: u64) -> RenderGrant {
    RenderGrant {
        kinds: vec![RENDER_KIND_MARKDOWN.to_string()],
        timeout_ms: 2_000,
        max_output_bytes,
    }
}

#[test]
fn render_host_transforms_sanitizes_caps_and_passes_failures_through() {
    let wasm = fixture_wasm();
    let host = RenderHost::new("conformance", &wasm, &render_grant(16)).expect("render host");

    assert_eq!(host.transform_markdown("hello").unwrap(), "[render] hello");
    // ANSI is stripped host-side: the TUI styles markdown itself.
    assert_eq!(host.transform_markdown("strip:red").unwrap(), "red");
    // A remaining C0 control is a hard failure, and the caller keeps the input.
    assert!(host.transform_markdown("control:x").is_err());
    assert_eq!(
        host.transform_markdown_or_original("control:x"),
        "control:x"
    );
    // The byte cap truncates rather than failing.
    assert_eq!(host.transform_markdown("cap:").unwrap(), "x".repeat(16));
    // A guest `Err` is one diagnostic and the original text survives.
    assert!(host.transform_markdown("fail:").is_err());
    assert_eq!(host.transform_markdown_or_original("fail:"), "fail:");
}

#[test]
fn deny_all_render_instance_cannot_reach_ui_session_or_exec() {
    let wasm = fixture_wasm();
    let host = RenderHost::new("conformance", &wasm, &render_grant(4096)).expect("render host");

    // The fixture asks for each capability from inside `transform-markdown`.
    // The render store carries `Grants::default()` and never installs a call
    // context, so ui/exec are denied and the session is empty.
    assert_eq!(host.transform_markdown("ui:").unwrap(), "ui-denied=true");
    assert_eq!(
        host.transform_markdown("session:").unwrap(),
        "session-id= cwd=",
        "a session id or a cwd means the render instance saw host call context"
    );
    assert_eq!(
        host.transform_markdown("exec:").unwrap(),
        "exec-denied=true"
    );
}

#[test]
fn render_host_enforces_the_guest_deadline() {
    let wasm = fixture_wasm();
    let grant = RenderGrant {
        kinds: vec![RENDER_KIND_MARKDOWN.to_string()],
        timeout_ms: 50,
        max_output_bytes: 4096,
    };
    let host = RenderHost::new("conformance", &wasm, &grant).expect("render host");
    let started = std::time::Instant::now();
    let error = host
        .transform_markdown("spin:")
        .expect_err("deadline must fire");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "the epoch deadline did not interrupt the guest: {error}"
    );
    // A trapped instance is disabled, not reused.
    assert!(host.transform_markdown("hello").is_err());
}

/// A guest parked inside a WASI host call is invisible to epoch interruption;
/// the host's wall-clock deadline must still return the original markdown
/// promptly instead of freezing the caller (the TUI event loop) forever.
#[test]
fn render_host_wall_clock_bounds_a_guest_blocked_in_wasi() {
    let wasm = fixture_wasm();
    let grant = RenderGrant {
        kinds: vec![RENDER_KIND_MARKDOWN.to_string()],
        timeout_ms: 50,
        max_output_bytes: 4096,
    };
    let host = RenderHost::new("conformance", &wasm, &grant).expect("render host");
    let started = std::time::Instant::now();
    let output = host.transform_markdown_or_original("sleep:blocked");
    assert_eq!(
        output, "sleep:blocked",
        "a deadline keeps the original markdown"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "a WASI sleep must not pin the transform (the guest sleeps 1500 ms): {:?}",
        started.elapsed()
    );
    // The abandoned instance is never reused.
    assert!(host.transform_markdown("hello").is_err());
}

#[test]
fn render_host_is_not_created_without_a_markdown_grant() {
    let wasm = fixture_wasm();
    assert!(
        RenderHost::load_with_grants("conformance", &wasm, &Grants::default())
            .expect("deny-by-default is not an error")
            .is_none()
    );
}

#[test]
fn render_instantiation_bounds_a_spinning_start_function() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = spinning_start_wasm(dir.path());
    let started = std::time::Instant::now();
    let error = match RenderHost::new("spinning-start", &wasm, &render_grant(4096)) {
        Ok(_) => panic!("a spinning start function must not produce a renderer"),
        Err(error) => error.to_string(),
    };
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "instantiation must return within its wall-clock bound: {:?}",
        started.elapsed()
    );
    assert!(error.contains("instantiate"), "{error}");
}

// ---------------------------------------------------------------------------
// P3(b): typed command flows and declaration-scoped cli-flag
// ---------------------------------------------------------------------------

/// A `SessionRead` that answers two different flags, so one can prove the
/// host scopes each read to the *calling* extension's declarations.
struct FlagSession {
    conformance: Option<kiqi_host::FlagValue>,
    todo: Option<kiqi_host::FlagValue>,
}

impl kiqi_host::SessionRead for FlagSession {
    fn active_tools(&self) -> Vec<String> {
        Vec::new()
    }
    fn system_prompt(&self) -> String {
        String::new()
    }
    fn thinking_level(&self) -> String {
        String::new()
    }
    fn cli_flag(&self, name: &str) -> Option<kiqi_host::FlagValue> {
        match name {
            "conformance-flag" => self.conformance.clone(),
            "todo-flag" => self.todo.clone(),
            _ => None,
        }
    }
    fn session_branch(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
}

fn text_of_message(result: &kiqi_host::CommandResult) -> &str {
    match &result.body {
        kiqi_host::CommandResultBody::Message(text) => text,
        other => panic!("expected a message body, got {other:?}"),
    }
}

#[test]
fn command_flows_round_trip_and_carry_flow_ids() {
    let wasm = fixture_wasm();
    let extension =
        WasmExtension::load_with_grants("conformance", &wasm, Grants::default()).expect("load");
    let log = NullLog;
    let caps = HostCaps::observer(&log);

    // run-command returns a typed selector carrying a flow id.
    let selector = extension
        .run_command("conformance", "flow:", &caps)
        .unwrap();
    assert_eq!(selector.flow_id, "flow-1");
    match selector.body {
        kiqi_host::CommandResultBody::Selector(spec) => {
            assert_eq!(spec.title, "Pick one");
            assert_eq!(spec.items, vec!["alpha".to_string(), "beta".to_string()]);
        }
        other => panic!("expected selector, got {other:?}"),
    }

    // continue-command delivers the reply to the same flow.
    let picked = extension
        .continue_command(
            "conformance",
            "flow-1",
            kiqi_host::CommandReply::Selected("alpha".to_string()),
            &caps,
        )
        .unwrap();
    assert_eq!(text_of_message(&picked), "selected:alpha");

    // Cancellation is a first-class reply.
    let cancelled = extension
        .continue_command(
            "conformance",
            "flow-ed",
            kiqi_host::CommandReply::Cancelled,
            &caps,
        )
        .unwrap();
    assert_eq!(text_of_message(&cancelled), "cancelled:flow-ed");

    // Editor and prompt bodies are typed the same way.
    let editor = extension
        .run_command("conformance", "editor:", &caps)
        .unwrap();
    assert_eq!(editor.flow_id, "flow-ed");
    assert!(matches!(
        editor.body,
        kiqi_host::CommandResultBody::Editor(_)
    ));
    let prompt = extension
        .run_command("conformance", "prompt:", &caps)
        .unwrap();
    assert_eq!(prompt.flow_id, "flow-pr");
    assert!(matches!(
        prompt.body,
        kiqi_host::CommandResultBody::Prompt(_)
    ));

    // An unknown flow id is refused by the guest.
    let stale = extension.continue_command(
        "conformance",
        "gone",
        kiqi_host::CommandReply::Selected("x".to_string()),
        &caps,
    );
    assert!(stale.is_err(), "unknown flow id must be refused: {stale:?}");
}

#[test]
fn command_results_are_sanitized_before_render() {
    let wasm = fixture_wasm();
    let extension =
        WasmExtension::load_with_grants("conformance", &wasm, Grants::default()).expect("load");
    let log = NullLog;
    let caps = HostCaps::observer(&log);

    // ANSI escapes are stripped from selector strings.
    let ansi = extension
        .run_command("conformance", "ansi:", &caps)
        .unwrap();
    match ansi.body {
        kiqi_host::CommandResultBody::Selector(spec) => {
            assert_eq!(spec.title, "Red");
            assert!(!spec.title.contains('\u{1b}'));
        }
        other => panic!("expected selector, got {other:?}"),
    }

    // A surviving C0 control character rejects the whole result.
    let control = extension.run_command("conformance", "ctl:", &caps);
    assert!(
        control.is_err(),
        "a control character must be rejected: {control:?}"
    );
}

#[test]
fn cli_flag_is_scoped_to_the_calling_extensions_declarations() {
    let conformance_wasm = fixture_wasm();
    let todo_wasm = todo_wasm();
    let conformance =
        WasmExtension::load_with_grants("conformance", &conformance_wasm, Grants::default())
            .expect("load conformance");
    let todo =
        WasmExtension::load_with_grants("todo", &todo_wasm, Grants::default()).expect("load todo");

    let session = FlagSession {
        conformance: Some(kiqi_host::FlagValue::Bare),
        todo: Some(kiqi_host::FlagValue::Text("loud".to_string())),
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

    // Each extension reads its OWN flag, including the bare form that used to be
    // dropped by the JSON reader.
    let own_bare = conformance
        .run_command("conformance", "flag:conformance-flag", &caps)
        .unwrap();
    assert_eq!(text_of_message(&own_bare), "flag=bare");
    let own_text = todo.run_command("todos", "flag:todo-flag", &caps).unwrap();
    assert_eq!(text_of_message(&own_text), "flag=text:loud");

    // …but neither can read the other's declaration.
    let cross = conformance
        .run_command("conformance", "flag:todo-flag", &caps)
        .unwrap();
    assert_eq!(text_of_message(&cross), "flag=none");
    let cross = todo
        .run_command("todos", "flag:conformance-flag", &caps)
        .unwrap();
    assert_eq!(text_of_message(&cross), "flag=none");
}

#[test]
fn typed_input_events_reach_the_guest_and_can_be_claimed() {
    let wasm = fixture_wasm();
    let extension =
        WasmExtension::load_with_grants("conformance", &wasm, Grants::default()).expect("load");
    let log = NullLog;
    let caps = HostCaps::observer(&log);

    // The declared `f8` shortcut arrives typed (`Input(event)`), not as an
    // opaque `other` record with a Debug-formatted kind.
    let claimed = extension.on_event(
        &Event::new(
            EventTag::Input,
            serde_json::json!({
                "key": "f8",
                "kind": "press",
                "ctrl": false,
                "alt": false,
                "shift": true,
            }),
        ),
        &caps,
    );
    assert_eq!(claimed, EventAction::Claimed);

    // An un-declared key still reaches the guest (the host only fans out to
    // extensions that declared a shortcut) and the guest declines it.
    let declined = extension.on_event(
        &Event::new(EventTag::Input, serde_json::json!({ "key": "f9" })),
        &caps,
    );
    assert_eq!(declined, EventAction::Continue);
}

// ---------------------------------------------------------------------------
// oauth-api: the host wrapper (deny-by-default, cross-check, deadline)
// ---------------------------------------------------------------------------

/// A valid grant profile. The conformance guest declares the matching ids.
fn oauth_profile(id: &str) -> kiqi_host::grants::OAuthProviderProfile {
    kiqi_host::grants::OAuthProviderProfile {
        id: id.to_string(),
        display_name: "Conformance OAuth".to_string(),
        scopes: vec!["openid".to_string()],
        flows: vec![
            kiqi_host::grants::OAuthFlow::AuthorizationCode,
            kiqi_host::grants::OAuthFlow::DeviceCode,
        ],
        authorization_endpoint: "https://id.acme.test/authorize".to_string(),
        token_endpoint: "https://id.acme.test/token".to_string(),
        device_authorization_endpoint: Some("https://id.acme.test/device".to_string()),
        revocation_endpoint: None,
        client_id: "client-123".to_string(),
        supports_pkce: true,
    }
}

fn oauth_grant(
    providers: Vec<kiqi_host::grants::OAuthProviderProfile>,
) -> kiqi_host::grants::OAuthGrant {
    kiqi_host::grants::OAuthGrant {
        providers,
        timeout_seconds: 60,
        max_requests: 5,
    }
}

/// Without an `oauth` grant the wrapper refuses before touching the guest. The
/// guest's `begin` answer is a distinctive interaction, so its absence proves
/// the guest was never reached.
#[test]
fn oauth_action_without_a_grant_never_calls_the_guest() {
    let wasm = fixture_wasm();
    let extension =
        WasmExtension::load_with_grants("conformance", &wasm, Grants::default()).expect("load");

    assert!(
        extension.oauth_providers().is_empty(),
        "no grant, no catalog"
    );

    let error = extension
        .oauth_action(
            "conformance-oauth",
            kiqi_host::OAuthAction::Begin(String::new()),
        )
        .expect_err("no grant means no guest call");
    let message = error.to_string();
    assert!(message.contains("not granted"), "{message}");
    assert!(
        !message.contains("conformance oauth begin"),
        "the guest must not be reached: {message}"
    );
}

/// A granted provider reaches the guest, and the host keeps the grant in
/// charge: an id the guest never declared is refused even when granted.
#[test]
fn oauth_action_reaches_a_granted_guest_and_narrows_by_declaration() {
    let wasm = fixture_wasm();
    let mut grants = Grants::default();
    grants
        .grant_oauth(oauth_grant(vec![oauth_profile("conformance-oauth")]))
        .expect("valid grant");
    let extension = WasmExtension::load_with_grants("conformance", &wasm, grants).expect("load");

    let declared = extension.oauth_providers();
    assert_eq!(declared.len(), 1);
    assert_eq!(declared[0].id, "conformance-oauth");

    match extension
        .oauth_action(
            "conformance-oauth",
            kiqi_host::OAuthAction::Begin("challenge".to_string()),
        )
        .expect("the guest answers")
    {
        kiqi_host::OAuthResponse::Interaction(interaction) => {
            assert_eq!(interaction.title, "conformance oauth begin");
            // The guest may put a URL in the response, but it is data only:
            // `ki auth` builds the URL from the grant and never opens this one.
            assert_eq!(
                interaction.url.as_deref(),
                Some("https://guest.example/never-opened")
            );
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

/// The load-time `list-providers` cross-check can only narrow the grant: an id
/// the guest does not declare is neither offered nor reachable.
#[test]
fn oauth_providers_are_narrowed_by_the_guest_list() {
    let wasm = fixture_wasm();
    let mut grants = Grants::default();
    grants
        .grant_oauth(oauth_grant(vec![
            oauth_profile("conformance-oauth"),
            oauth_profile("ghost"),
        ]))
        .expect("valid grant");
    let extension = WasmExtension::load_with_grants("conformance", &wasm, grants).expect("load");

    let ids: Vec<String> = extension
        .oauth_providers()
        .into_iter()
        .map(|provider| provider.id)
        .collect();
    assert_eq!(ids, vec!["conformance-oauth"]);

    // The guest never declared `ghost`, so the wrapper refuses without a call.
    let error = extension
        .oauth_action("ghost", kiqi_host::OAuthAction::Poll)
        .expect_err("not declared");
    assert!(error.to_string().contains("not declared"), "{error}");
}

/// A guest that never returns is bounded by the grant's wall clock. The fixture
/// spins for three seconds, so a one-second grant must give up first.
#[test]
fn oauth_action_times_out_on_a_spinning_guest() {
    let wasm = fixture_wasm();
    let mut grants = Grants::default();
    grants
        .grant_oauth(kiqi_host::grants::OAuthGrant {
            providers: vec![
                oauth_profile("conformance-spin"),
                oauth_profile("conformance-oauth"),
            ],
            timeout_seconds: 1,
            max_requests: 2,
        })
        .expect("valid grant");
    let extension = WasmExtension::load_with_grants("conformance", &wasm, grants).expect("load");

    let started = std::time::Instant::now();
    let error = extension
        .oauth_action(
            "conformance-spin",
            kiqi_host::OAuthAction::Begin(String::new()),
        )
        .expect_err("the deadline must fire");
    let elapsed = started.elapsed();
    assert!(error.to_string().contains("timed out"), "{error}");
    assert!(
        elapsed >= std::time::Duration::from_secs(1),
        "returned before the deadline: {elapsed:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(4),
        "the deadline must bound the call: {elapsed:?}"
    );

    // A timeout must not leave the extension permanently "unloaded": the host
    // rebuilds the instance from the retained engine and component.
    let next = extension
        .oauth_action("conformance-oauth", kiqi_host::OAuthAction::Poll)
        .expect("a second action after a timeout must still work");
    assert!(
        matches!(next, kiqi_host::OAuthResponse::AuthorizationPending),
        "got {next:?}"
    );
}

// ---------------------------------------------------------------------------
// provider-api / provider-http / provider-stream
// ---------------------------------------------------------------------------

use kiqi_host::grants::{
    ProviderEndpoint, ProviderGrant, ProviderModel, ProviderProfile,
    PROVIDER_MAX_RESPONSE_BYTES_DEFAULT,
};
use kiqi_host::wasm::{
    fingerprint_credential, ProviderHost, ProviderHostOptions, ProviderHttpRequest,
    ProviderHttpStream, ProviderHttpTransport, ProviderResponse,
};

/// A transport that never touches the network: it records the already-sanitized
/// request and serves one canned body.
struct FakeProviderTransport {
    status: u16,
    body: Vec<u8>,
    requests: Mutex<Vec<ProviderHttpRequest>>,
}

impl FakeProviderTransport {
    fn new(status: u16, body: &[u8]) -> Self {
        Self {
            status,
            body: body.to_vec(),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn take_requests(&self) -> Vec<ProviderHttpRequest> {
        std::mem::take(&mut *self.requests.lock().unwrap())
    }
}

struct FakeProviderStream {
    status: u16,
    chunks: Vec<Vec<u8>>,
}

impl ProviderHttpStream for FakeProviderStream {
    fn status(&self) -> u16 {
        self.status
    }

    fn read(&mut self, _max: usize) -> Result<Option<Vec<u8>>, String> {
        if self.chunks.is_empty() {
            Ok(None)
        } else {
            Ok(Some(self.chunks.remove(0)))
        }
    }
}

impl ProviderHttpTransport for FakeProviderTransport {
    fn open(&self, request: ProviderHttpRequest) -> Result<Box<dyn ProviderHttpStream>, String> {
        self.requests.lock().unwrap().push(request);
        let chunks = if self.body.is_empty() {
            Vec::new()
        } else {
            vec![self.body.clone()]
        };
        Ok(Box::new(FakeProviderStream {
            status: self.status,
            chunks,
        }))
    }
}

struct ProviderFixture {
    _dir: tempfile::TempDir,
    pub wasm: PathBuf,
    pub root: PathBuf,
}

/// A component in `extensions/conformance/`, plus the host-held credential
/// sidecar and `auth.json` the grant's handle resolves through.
fn provider_fixture() -> ProviderFixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("agent");
    let extension_dir = root.join("extensions").join("conformance");
    std::fs::create_dir_all(&extension_dir).expect("extension dir");
    let wasm = extension_dir.join("extension.wasm");
    std::fs::copy(fixture_wasm(), &wasm).expect("copy the fixture");
    std::fs::write(
        root.join("auth.json"),
        r#"{"anthropic":{"type":"api_key","key":"sk-conformance-secret"}}"#,
    )
    .expect("auth.json");
    let sidecar = serde_json::json!({
        "pc-test": {
            "source": "auth-json:anthropic",
            "provider": "anthropic",
            "display": "Anthropic",
            "fingerprint": fingerprint_credential("sk-conformance-secret"),
        }
    });
    std::fs::write(
        extension_dir.join("credentials.json"),
        serde_json::to_string(&sidecar).unwrap(),
    )
    .expect("credentials.json");
    ProviderFixture {
        _dir: dir,
        wasm,
        root,
    }
}

fn provider_model(id: &str) -> ProviderModel {
    ProviderModel {
        id: id.to_string(),
        api: "openai-completions".to_string(),
        base_url: "https://api.example.test/v1".to_string(),
        reasoning: true,
        input: vec!["text".to_string()],
    }
}

fn provider_grants(timeout_seconds: u64, models: Vec<ProviderModel>) -> Grants {
    let mut grants = Grants::default();
    grants
        .grant_provider(ProviderGrant {
            providers: vec![ProviderProfile {
                id: "conformance".to_string(),
                display_name: "Conformance Provider".to_string(),
                models,
                endpoint: ProviderEndpoint {
                    scheme: "https".to_string(),
                    host: "api.example.test".to_string(),
                    path_prefix: "/v1".to_string(),
                    methods: vec!["POST".to_string()],
                },
                credential: Some("pc-test".to_string()),
                auth: None,
                timeout_seconds,
                max_response_bytes: PROVIDER_MAX_RESPONSE_BYTES_DEFAULT,
                max_handles: 2,
            }],
        })
        .expect("valid provider grant");
    grants
}

fn provider_host(
    fixture: &ProviderFixture,
    grants: &Grants,
    transport: std::sync::Arc<dyn ProviderHttpTransport>,
) -> ProviderHost {
    ProviderHost::load_with_options(
        "conformance",
        &fixture.wasm,
        grants,
        ProviderHostOptions {
            transport: Some(transport),
            agent_root: Some(fixture.root.clone()),
            ..ProviderHostOptions::default()
        },
    )
    .expect("load")
    .expect("a granted profile instantiates the guest")
}

fn provider_text(response: &ProviderResponse) -> String {
    let value: serde_json::Value = serde_json::from_str(&response.message).expect("message json");
    value["content"][0]["text"]
        .as_str()
        .expect("text content")
        .to_string()
}

fn provider_request(mode: &str, extra: serde_json::Value) -> String {
    let mut value = serde_json::json!({"model": "conformance-model", "mode": mode});
    if let serde_json::Value::Object(entries) = extra {
        for (key, entry) in entries {
            value[key] = entry;
        }
    }
    value.to_string()
}

#[test]
fn provider_http_injects_the_credential_and_strips_guest_framing_headers() {
    let fixture = provider_fixture();
    let transport = std::sync::Arc::new(FakeProviderTransport::new(200, b"wire-body"));
    let grants = provider_grants(30, vec![provider_model("conformance-model")]);
    let host = provider_host(&fixture, &grants, transport.clone());

    let request = provider_request(
        "fetch",
        serde_json::json!({
            "url": "https://api.example.test/v1/messages",
            "method": "POST",
            "body": "hello",
            "headers": {
                "authorization": "guest-secret",
                "x-api-key": "guest-key",
                "x-goog-api-key": "guest-key",
                "proxy-authorization": "guest-proxy",
                "host": "evil.test",
                "content-length": "999",
                "transfer-encoding": "chunked",
                "connection": "close",
                "cookie": "session=1",
                "x-test": "kept",
            },
        }),
    );
    let response = host.request(&request).expect("request");
    assert_eq!(provider_text(&response), "wire-body");

    let requests = transport.take_requests();
    assert_eq!(requests.len(), 1);
    let sent = &requests[0];
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.body.as_deref(), Some(b"hello".as_slice()));
    let names: Vec<&str> = sent.headers.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, vec!["x-test", "authorization"]);
    assert_eq!(sent.headers[1].1, "Bearer sk-conformance-secret");
    for forbidden in [
        "host",
        "content-length",
        "transfer-encoding",
        "connection",
        "x-api-key",
        "x-goog-api-key",
        "proxy-authorization",
        "cookie",
        "guest-secret",
    ] {
        assert!(
            !names.contains(&forbidden)
                && !sent.headers.iter().any(|(_, value)| value == forbidden),
            "guest header `{forbidden}` must not reach the wire: {:?}",
            sent.headers
        );
    }
}

#[test]
fn provider_http_refuses_encoded_slashes_dot_segments_and_redirects() {
    let fixture = provider_fixture();
    let transport = std::sync::Arc::new(FakeProviderTransport::new(200, b"ok"));
    let grants = provider_grants(30, vec![provider_model("conformance-model")]);
    let host = provider_host(&fixture, &grants, transport.clone());

    for url in [
        "https://api.example.test/v1/%2Fadmin",
        "https://api.example.test/v1/%2e%2e/admin",
        "https://api.example.test/v1/a/../admin",
        "https://api.example.test/v1//admin",
        "https://api.example.test/v10/messages",
        "https://other.test/v1/messages",
        "http://api.example.test/v1/messages",
    ] {
        let request = provider_request("fetch", serde_json::json!({"url": url}));
        let response = host.request(&request).expect("guest answered");
        let text = provider_text(&response);
        assert!(
            text.starts_with("open-denied:"),
            "`{url}` must be refused before a connection: {text}"
        );
    }
    assert!(
        transport.take_requests().is_empty(),
        "a refused request must never reach the transport"
    );

    // A redirect is a refusal, never followed.
    let redirecting = std::sync::Arc::new(FakeProviderTransport::new(302, b"moved"));
    let host = provider_host(&fixture, &grants, redirecting.clone());
    let request = provider_request(
        "fetch",
        serde_json::json!({"url": "https://api.example.test/v1/messages"}),
    );
    let response = host.request(&request).expect("guest answered");
    let text = provider_text(&response);
    assert!(text.starts_with("open-denied:"), "{text}");
    assert!(text.contains("redirect"), "{text}");
    assert_eq!(redirecting.take_requests().len(), 1);
}

#[test]
fn provider_http_handles_are_reaped_at_call_exit() {
    let fixture = provider_fixture();
    let transport = std::sync::Arc::new(FakeProviderTransport::new(200, b"ok"));
    let grants = provider_grants(30, vec![provider_model("conformance-model")]);
    let host = provider_host(&fixture, &grants, transport.clone());

    // The guest opens a stream and never closes it.
    let leaked = provider_request(
        "leak",
        serde_json::json!({"url": "https://api.example.test/v1/messages"}),
    );
    let response = host.request(&leaked).expect("leaked request");
    assert_eq!(provider_text(&response), "opened:1");
    assert_eq!(
        host.open_handle_count(),
        0,
        "every handle is gone when the call returns"
    );

    // The next call cannot read a handle from the previous one.
    let stale = host.request(&provider_request("stale", serde_json::json!({})));
    let text = provider_text(&stale.expect("stale request"));
    assert!(text.starts_with("stale:"), "{text}");
    assert!(text.contains("no open provider-http handle 1"), "{text}");
}

#[test]
fn provider_stream_deltas_are_assembled_and_bad_order_fails() {
    let fixture = provider_fixture();
    let transport = std::sync::Arc::new(FakeProviderTransport::new(200, b""));
    let grants = provider_grants(30, vec![provider_model("conformance-model")]);
    let host = provider_host(&fixture, &grants, transport);

    let response = host
        .request(&provider_request("stream", serde_json::json!({})))
        .expect("streamed request");
    assert_eq!(response.provider, "conformance");
    assert_eq!(response.model, "conformance-model");
    assert_eq!(response.api, "openai-completions");
    assert_eq!(response.segments.len(), 3);
    assert_eq!(response.segments[0].kind, kiqi_host::wasm::DeltaKind::Text);
    assert_eq!(response.segments[0].text, "Hello world");
    assert_eq!(
        response.segments[1].kind,
        kiqi_host::wasm::DeltaKind::Thinking
    );
    assert_eq!(response.segments[1].text, "reasoned");
    assert_eq!(
        response.segments[2].kind,
        kiqi_host::wasm::DeltaKind::ToolCall
    );
    assert!(response.segments[2]
        .text
        .contains("\"name\":\"conformance\""));

    let error = host
        .request(&provider_request("stream-bad", serde_json::json!({})))
        .expect_err("a delta before start fails the request");
    assert!(error.contains("delta before start"), "{error}");
}

#[test]
fn provider_stream_block_overflow_fails_and_disables_the_instance() {
    let fixture = provider_fixture();
    let transport = std::sync::Arc::new(FakeProviderTransport::new(200, b""));
    let grants = provider_grants(30, vec![provider_model("conformance-model")]);
    let host = provider_host(&fixture, &grants, transport);

    let error = host
        .request(&provider_request(
            "stream-many-blocks",
            serde_json::json!({}),
        ))
        .expect_err("the distinct-block cap must reject the stream");
    assert!(error.contains("distinct blocks"), "{error}");
    assert!(
        host.is_disabled(),
        "a stream-budget overflow must disable the instance"
    );
}

#[test]
fn a_spinning_provider_guest_is_interrupted_by_the_deadline() {
    let fixture = provider_fixture();
    let transport = std::sync::Arc::new(FakeProviderTransport::new(200, b""));
    // The grant's own budget is the deadline: one second.
    let grants = provider_grants(1, vec![provider_model("conformance-model")]);
    let host = provider_host(&fixture, &grants, transport);
    let started = std::time::Instant::now();
    let error = host
        .request(&provider_request("spin", serde_json::json!({})))
        .expect_err("a spinning guest must not return an answer");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(6),
        "the deadline must fire promptly: {:?}",
        started.elapsed()
    );
    assert!(
        error.contains("trapped") || error.contains("deadline"),
        "{error}"
    );
}

#[test]
fn provider_instantiation_bounds_a_spinning_start_function() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = spinning_start_wasm(dir.path());
    let grants = provider_grants(30, vec![provider_model("conformance-model")]);
    let started = std::time::Instant::now();
    let error = match ProviderHost::load("conformance", &wasm, &grants) {
        Ok(_) => panic!("a spinning start function must not produce a provider"),
        Err(error) => error.to_string(),
    };
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "instantiation must return within its wall-clock bound: {:?}",
        started.elapsed()
    );
    assert!(error.contains("instantiate"), "{error}");
}

#[test]
fn without_a_provider_grant_the_guest_is_never_instantiated() {
    let fixture = provider_fixture();
    assert!(
        ProviderHost::load("conformance", &fixture.wasm, &Grants::default())
            .expect("load")
            .is_none(),
        "no grant means no instance"
    );
    let mut other = Grants::default();
    other
        .grant_provider(ProviderGrant {
            providers: vec![ProviderProfile {
                id: "someone-else".to_string(),
                display_name: String::new(),
                models: vec![provider_model("conformance-model")],
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
                max_handles: 8,
            }],
        })
        .unwrap();
    assert!(
        ProviderHost::load("conformance", &fixture.wasm, &other)
            .expect("load")
            .is_none(),
        "a grant for another id does not reach this guest"
    );
}

#[test]
fn the_guest_provider_list_can_only_narrow_the_grant() {
    let fixture = provider_fixture();
    let transport = std::sync::Arc::new(FakeProviderTransport::new(200, b""));
    let grants = provider_grants(
        30,
        vec![
            provider_model("conformance-model"),
            provider_model("ungranted-model"),
        ],
    );
    let host = provider_host(&fixture, &grants, transport);
    let specs = host.list_specs().expect("list specs");
    let ids: Vec<&str> = specs.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, vec!["conformance-model"]);
}

#[test]
fn a_changed_credential_is_refused_before_any_connection() {
    let fixture = provider_fixture();
    // The value no longer matches the fingerprint recorded at grant time.
    std::fs::write(
        fixture.root.join("auth.json"),
        r#"{"anthropic":{"type":"api_key","key":"sk-rotated-without-regrant"}}"#,
    )
    .expect("rotate auth.json");
    let transport = std::sync::Arc::new(FakeProviderTransport::new(200, b"ok"));
    let grants = provider_grants(30, vec![provider_model("conformance-model")]);
    let host = provider_host(&fixture, &grants, transport.clone());
    let error = host
        .request(&provider_request(
            "fetch",
            serde_json::json!({"url": "https://api.example.test/v1/messages"}),
        ))
        .expect_err("a changed credential is refused");
    assert!(error.contains("credential changed"), "{error}");
    assert!(transport.take_requests().is_empty());
}
