//! Test fixture: a minimal line-delimited JSON-RPC MCP server.
//!
//! Spawned as a real child process by the `kiqi-mcp` integration tests. It
//! implements just enough of the protocol — `initialize`,
//! `notifications/initialized`, `tools/list`, `tools/call` — to exercise the
//! manager end to end, and adds the failure modes the tests need (a hanging
//! tool, a crashing handshake, a repeated pagination cursor, a grandchild
//! process to prove the whole tree is reaped).
//!
//! With `--resources` it also speaks the resource half of the protocol:
//! `resources/list` | `read` | `subscribe` | `unsubscribe`, a `poke` tool that
//! emits `notifications/resources/updated`, and the subscribe failure modes
//! the subscription tests need (mirroring the retired adapter's fake server):
//!   --subscribe-notify             push a notification BEFORE replying to a
//!                                  subscribe (the racing-update scenario)
//!   --reject-duplicate-subscribe   a second subscribe for the same uri errors
//!   --reject-unsubscribe           every unsubscribe errors
//!   --no-resource-list             answer -32601 for `resources/list`
//!   --no-resource-templates        answer -32601 for `resources/templates/list`
//!                                  (the walgit shape: resources, no templates)
//!
//! Extra args beyond the protocol:
//!   --name <n>          label (informational)
//!   --pidfile <path>    write this process's pid, and `CHILD <pid>` for a
//!                       grandchild when --child is given
//!   --child             also spawn a long-lived `sleep 300` grandchild
//!   --pages <n>         spread tools/list over n pages
//!   --repeat-cursor     always answer with the same nextCursor (a broken server)
//!   --crash-on-init     exit(1) as soon as initialize arrives
//!   --stderr-msg <s>    print s to stderr once at startup
//!   --record <path>     append `hang` tool calls, `notifications/cancelled`
//!                       and every `resources/*` request to <path>
//!   --notify-uri <uri>  emit notifications/resources/updated for <uri> from a
//!                       background thread every 150ms (for update races)
//!   --env-tool          add a `env` tool returning the child's own value for
//!                       the variable named by its `name` argument (or
//!                       `<unset>`). This is how the tests assert what the host
//!                       forwards to a spawned server: `--env-tool` is opt-in so
//!                       the baseline tool count stays six.

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let opt = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    if let Some(message) = opt("--stderr-msg") {
        eprintln!("{message}");
    }
    let child_pid = if flag("--child") {
        Command::new("sleep")
            .arg("300")
            .spawn()
            .ok()
            .map(|child| child.id())
    } else {
        None
    };
    if let Some(path) = opt("--pidfile") {
        let mut contents = format!("SELF {}\n", std::process::id());
        if let Some(pid) = child_pid {
            contents.push_str(&format!("CHILD {pid}\n"));
        }
        let _ = std::fs::write(path, contents);
    }

    let crash_on_init = flag("--crash-on-init");
    let pages: usize = opt("--pages")
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    let repeat_cursor = flag("--repeat-cursor");
    let record_path: Option<String> = opt("--record");
    let resources_mode = flag("--resources");
    // A server may advertise `resources` and implement only half of it: the
    // absent half answers -32601 (`Method not found`), which the client must
    // read as an empty list, not as a failed listing.
    let no_resource_list = flag("--no-resource-list");
    let no_resource_templates = flag("--no-resource-templates");
    let env_tool = flag("--env-tool");
    let supports_subscribe = !flag("--no-resource-subscribe");
    let subscribe_notify = flag("--subscribe-notify");
    let reject_duplicate_subscribe = flag("--reject-duplicate-subscribe");
    let reject_unsubscribe = flag("--reject-unsubscribe");
    // Subscriptions this process remembers (only needed by the
    // duplicate-rejecting mode; a fresh process forgets everything, which is
    // exactly what the restore tests rely on).
    let subscribed: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    // A background notifier: one update every 150ms until the process exits.
    if let Some(uri) = opt("--notify-uri") {
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(150));
            send_notification(&uri);
        });
    }

    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => return, // stdin closed: rmcp is shutting us down
            Ok(_) => {}
            Err(_) => return,
        }
        let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let id = message.get("id").cloned();
        match method {
            "initialize" => {
                if crash_on_init {
                    std::process::exit(1);
                }
                reply(id, initialize_result(resources_mode, supports_subscribe));
            }
            "notifications/initialized" => {} // a notification: never answered
            "tools/list" => {
                let cursor = message
                    .get("params")
                    .and_then(|p| p.get("cursor"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                reply(
                    id,
                    tools_list(
                        pages,
                        repeat_cursor,
                        cursor.as_deref(),
                        resources_mode,
                        env_tool,
                    ),
                );
            }
            "tools/call" => {
                let name = message
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let arguments = message
                    .get("params")
                    .and_then(|p| p.get("arguments"))
                    .cloned()
                    .unwrap_or(Value::Null);
                if name == "hang" {
                    // Record but never answer: the caller must abandon the call
                    // (and send `notifications/cancelled`, which the loop below
                    // records). Keeping the read loop alive is what lets the
                    // cancellation actually be observed.
                    record(&record_path, &message);
                    continue;
                }
                if name == "sleep" {
                    let seconds = arguments
                        .get("seconds")
                        .and_then(Value::as_u64)
                        .unwrap_or(30);
                    std::thread::sleep(std::time::Duration::from_secs(seconds));
                }
                if name == "poke" {
                    // Emit the update(s) first, then answer — the client's
                    // dispatch may see the notification before the response.
                    let uri = arguments
                        .get("uri")
                        .and_then(Value::as_str)
                        .unwrap_or("file:///docs/a.md")
                        .to_string();
                    let times = arguments.get("times").and_then(Value::as_u64).unwrap_or(1);
                    for _ in 0..times {
                        send_notification(&uri);
                    }
                }
                reply(id, call_tool(name, &arguments));
            }
            "resources/list" => {
                if !resources_mode || no_resource_list {
                    reply_error(id, -32601, "method not found: resources/list");
                    continue;
                }
                reply(id, json!({ "resources": resource_list() }));
            }
            "resources/templates/list" => {
                if !resources_mode || no_resource_templates {
                    reply_error(id, -32601, "method not found: resources/templates/list");
                    continue;
                }
                reply(id, json!({ "resourceTemplates": resource_templates() }));
            }
            "resources/read" => {
                let uri = message
                    .get("params")
                    .and_then(|p| p.get("uri"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                match read_resource(&uri) {
                    Some(contents) => reply(id, json!({ "contents": [contents] })),
                    None => reply_error(id, -32602, &format!("unknown resource: {uri}")),
                }
            }
            "resources/subscribe" => {
                let uri = message
                    .get("params")
                    .and_then(|p| p.get("uri"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                record(&record_path, &message);
                // The racing-update scenario: the update leaves BEFORE the
                // subscribe response, so the client must already accept it.
                if subscribe_notify {
                    send_notification(&uri);
                }
                let mut known = subscribed.lock().unwrap();
                if reject_duplicate_subscribe && !known.insert(uri.clone()) {
                    reply_error(id, -32603, &format!("already subscribed: {uri}"));
                } else {
                    reply(id, json!({}));
                }
            }
            "resources/unsubscribe" => {
                record(&record_path, &message);
                if reject_unsubscribe {
                    reply_error(id, -32603, "unsubscribe rejected");
                } else {
                    subscribed
                        .lock()
                        .unwrap()
                        .remove(message["params"]["uri"].as_str().unwrap_or(""));
                    reply(id, json!({}));
                }
            }
            "notifications/cancelled" => record(&record_path, &message),
            _ => reply_error(id, -32601, &format!("method not found: {method}")),
        }
    }
}

/// Push one `notifications/resources/updated` onto stdout.
fn send_notification(uri: &str) {
    write_line(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/resources/updated",
        "params": { "uri": uri }
    }));
}

/// Append one received message to the `--record` file (a test observation log).
fn record(path: &Option<String>, message: &Value) {
    let Some(path) = path else { return };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{message}");
    }
}

fn initialize_result(resources_mode: bool, supports_subscribe: bool) -> Value {
    let mut capabilities = json!({ "tools": {} });
    if resources_mode {
        capabilities["resources"] = json!({ "subscribe": supports_subscribe });
    }
    json!({
        "protocolVersion": "2025-06-18",
        "capabilities": capabilities,
        "serverInfo": { "name": "ki-mcp-fake-server", "version": "0.1.0" }
    })
}

fn all_tools(resources_mode: bool, env_tool: bool) -> Vec<Value> {
    let mut tools = vec![
        json!({
            "name": "echo",
            "description": "Echo the `text` argument back",
            "inputSchema": {
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }
        }),
        json!({
            "name": "fail",
            "description": "Always fail with isError",
            "inputSchema": { "type": "object" }
        }),
        json!({
            "name": "img",
            "description": "Return an image content block",
            "inputSchema": { "type": "object" }
        }),
        json!({
            "name": "structured",
            "description": "Return structuredContent",
            "inputSchema": { "type": "object" }
        }),
        json!({
            "name": "sleep",
            "description": "Sleep `seconds`, then echo",
            "inputSchema": { "type": "object" }
        }),
        json!({
            "name": "hang",
            "description": "Never answer",
            "inputSchema": { "type": "object" }
        }),
    ];
    if resources_mode {
        // The model-side trigger for notifications/resources/updated.
        tools.push(json!({
            "name": "poke",
            "description": "Emit `times` notifications/resources/updated for `uri`",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uri": { "type": "string" },
                    "times": { "type": "integer" }
                },
                "required": ["uri"]
            }
        }));
    }
    if env_tool {
        // What the host forwarded into this process. Opt-in: it is a probe, not
        // part of the baseline protocol surface the other tests count.
        tools.push(json!({
            "name": "env",
            "description": "Return this process's value for the env var `name`",
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }
        }));
    }
    tools
}

fn tools_list(
    pages: usize,
    repeat_cursor: bool,
    cursor: Option<&str>,
    resources_mode: bool,
    env_tool: bool,
) -> Value {
    if repeat_cursor {
        return json!({ "tools": [all_tools(resources_mode, env_tool)[0].clone()], "nextCursor": "same" });
    }
    let tools = all_tools(resources_mode, env_tool);
    if pages <= 1 {
        return json!({ "tools": tools });
    }
    let per_page = tools.len().div_ceil(pages).max(1);
    let page = match cursor {
        None => 0,
        Some(cursor) => cursor
            .strip_prefix("page-")
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or(0),
    };
    let start = page * per_page;
    let end = (start + per_page).min(tools.len());
    let slice: Vec<Value> = tools[start.min(tools.len())..end].to_vec();
    let has_more = end < tools.len();
    let mut result = json!({ "tools": slice });
    if has_more {
        result["nextCursor"] = json!(format!("page-{}", page + 1));
    }
    result
}

fn resource_list() -> Vec<Value> {
    vec![
        json!({
            "uri": "file:///docs/a.md",
            "name": "a",
            "description": "first doc",
            "mimeType": "text/markdown"
        }),
        json!({
            "uri": "file:///docs/pic.png",
            "name": "pic",
            "description": "an image resource",
            "mimeType": "image/png"
        }),
        json!({
            "uri": "file:///docs/big.txt",
            "name": "big",
            "description": "larger than any read bound",
            "mimeType": "text/plain"
        }),
    ]
}

fn resource_templates() -> Vec<Value> {
    vec![json!({
        "uriTemplate": "file:///docs/{path}",
        "name": "docs",
        "description": "a doc by path",
        "mimeType": "text/markdown"
    })]
}

fn read_resource(uri: &str) -> Option<Value> {
    match uri {
        "file:///docs/a.md" => Some(json!({
            "uri": uri, "mimeType": "text/markdown", "text": "doc-a body"
        })),
        "file:///docs/pic.png" => Some(json!({
            "uri": uri, "mimeType": "image/png", "blob": "aGVsbG8="
        })),
        "file:///docs/big.txt" => Some(json!({
            "uri": uri, "mimeType": "text/plain", "text": "x".repeat(30_000)
        })),
        other if other.starts_with("file:///docs/") => Some(json!({
            "uri": other, "mimeType": "text/markdown",
            "text": format!("body of {other}")
        })),
        _ => None,
    }
}

fn call_tool(name: &str, arguments: &Value) -> Value {
    match name {
        "echo" => {
            let text = arguments
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("(no text)");
            json!({ "content": [{ "type": "text", "text": text }] })
        }
        "poke" => json!({ "content": [{ "type": "text", "text": "poked" }] }),
        "env" => {
            let name = arguments
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("KI_AGENT_ID");
            let value = std::env::var(name).unwrap_or_else(|_| "<unset>".to_string());
            json!({ "content": [{ "type": "text", "text": value }] })
        }
        "fail" => json!({
            "content": [{ "type": "text", "text": "tool exploded on purpose" }],
            "isError": true
        }),
        "img" => json!({
            "content": [{ "type": "image", "data": "aGVsbG8=", "mimeType": "image/png" }]
        }),
        "structured" => json!({
            "content": [{ "type": "text", "text": "see structuredContent" }],
            "structuredContent": { "answer": 42 }
        }),
        "sleep" => {
            let seconds = arguments
                .get("seconds")
                .and_then(Value::as_u64)
                .unwrap_or(1);
            json!({ "content": [{ "type": "text", "text": format!("slept {seconds}") }] })
        }
        _ => json!({
            "content": [{ "type": "text", "text": format!("unknown tool {name}") }],
            "isError": true
        }),
    }
}

fn reply(id: Option<Value>, result: Value) {
    let message = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    write_line(&message);
}

fn reply_error(id: Option<Value>, code: i64, message: &str) {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    });
    write_line(&payload);
}

fn write_line(value: &Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{value}");
    let _ = stdout.flush();
}
