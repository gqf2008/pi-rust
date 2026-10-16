//! Test fixture: a minimal line-delimited JSON-RPC MCP server.
//!
//! Spawned as a real child process by the `rpi-mcp` integration tests. It
//! implements just enough of the protocol — `initialize`,
//! `notifications/initialized`, `tools/list`, `tools/call` — to exercise the
//! manager end to end, and adds the failure modes the tests need (a hanging
//! tool, a crashing handshake, a repeated pagination cursor, a grandchild
//! process to prove the whole tree is reaped).
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
//!   --record <path>     append every `hang` tool call and every
//!                       `notifications/cancelled` to <path>

use std::io::{BufRead, Write};
use std::process::Command;

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
                reply(id, initialize_result());
            }
            "notifications/initialized" => {} // a notification: never answered
            "tools/list" => {
                let cursor = message
                    .get("params")
                    .and_then(|p| p.get("cursor"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                reply(id, tools_list(pages, repeat_cursor, cursor.as_deref()));
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
                reply(id, call_tool(name, &arguments));
            }
            "notifications/cancelled" => record(&record_path, &message),
            _ => reply_error(id, -32601, &format!("method not found: {method}")),
        }
    }
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

fn initialize_result() -> Value {
    json!({
        "protocolVersion": "2025-06-18",
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "ki-mcp-fake-server", "version": "0.1.0" }
    })
}

fn all_tools() -> Vec<Value> {
    vec![
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
    ]
}

fn tools_list(pages: usize, repeat_cursor: bool, cursor: Option<&str>) -> Value {
    if repeat_cursor {
        return json!({ "tools": [all_tools()[0].clone()], "nextCursor": "same" });
    }
    let tools = all_tools();
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

fn call_tool(name: &str, arguments: &Value) -> Value {
    match name {
        "echo" => {
            let text = arguments
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("(no text)");
            json!({ "content": [{ "type": "text", "text": text }] })
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
