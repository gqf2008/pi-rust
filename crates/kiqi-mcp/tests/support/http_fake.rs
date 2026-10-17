//! A tiny in-process MCP Streamable HTTP server for the offline e2e tests.
//!
//! It speaks just enough HTTP/1.1 and MCP to be a real peer: a session id on
//! initialize, JSON or SSE POST replies, a standalone GET SSE stream for
//! server-initiated notifications, `resources/subscribe`, 404 after an
//! explicit session expiry, and DELETE on teardown. Everything is loopback and
//! deterministic (no sleeps are load-bearing on the server side).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// Server behaviors a test can opt into.
#[derive(Clone, Copy, Default)]
pub struct Behavior {
    /// Answer a POST that carries a JSON-RPC id with an SSE event stream
    /// instead of a JSON body.
    pub sse_post_replies: bool,
    /// `tools/call` on the `big` tool returns a body past ki's 16 MiB cap.
    pub huge_response: bool,
    /// Same, but as a chunked response with NO `Content-Length`, so the cap
    /// can only be enforced by the streaming accumulation guard.
    pub huge_response_chunked: bool,
    /// Same, but as one SSE event past the cap, exercising the POST-reply
    /// stream cap rather than the buffered JSON path.
    pub huge_sse_response: bool,
    /// Never answer the first N `tools/call` requests (the connection stays
    /// open): a hung upstream. Later calls are answered normally, so a test can
    /// tell a freed worker from one wedged on the hung POST. `usize::MAX` hangs
    /// every call (shutdown mid-call).
    pub hang_tools_calls: usize,
    /// Push one `notifications/resources/updated` on the standalone GET stream
    /// after each `resources/subscribe`.
    pub notify_on_subscribe: bool,
}

pub struct FakeHttpServer {
    pub url: String,
    state: Arc<State>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeHttpServer {
    pub async fn start(behavior: Behavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let state = Arc::new(State::new(behavior));
        let accept_state = Arc::clone(&state);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let state = Arc::clone(&accept_state);
                tokio::spawn(async move { handle_connection(stream, state).await });
            }
        });
        Self { url, state, task }
    }

    pub fn initialize_count(&self) -> usize {
        self.state.initialize_count.load(Ordering::Acquire)
    }

    pub fn saw_method(&self, method: &str) -> bool {
        self.state
            .methods
            .lock()
            .unwrap()
            .iter()
            .any(|seen| seen == method)
    }

    pub fn subscribed(&self, uri: &str) -> bool {
        self.state.subscribed.lock().unwrap().contains(uri)
    }

    pub fn session_deleted(&self) -> bool {
        self.state.deleted.load(Ordering::Acquire)
    }

    /// How many `POST`s carrying `method` arrived (a repeat subscribe after a
    /// transparent re-init is observable here).
    pub fn method_count(&self, method: &str) -> usize {
        self.state
            .methods
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| *seen == method)
            .count()
    }

    /// How many connections the fake accepted in total.
    pub fn request_count(&self) -> usize {
        self.state.requests.load(Ordering::Acquire)
    }

    /// Answer every request with a cross-origin `302` to `target`.
    pub fn redirect_to(&self, target: &str) {
        *self.state.redirect_to.lock().unwrap() = Some(target.to_string());
    }

    /// A POST arrived with this (lowercase) header and value.
    pub fn posted_with_header(&self, name: &str, value: &str) -> bool {
        self.state.posts.lock().unwrap().iter().any(|headers| {
            headers
                .get(&name.to_ascii_lowercase())
                .is_some_and(|seen| seen == value)
        })
    }

    /// Make the current session invalid: the next request carrying it gets the
    /// spec's 404 session-expired answer.
    pub fn expire_session(&self) {
        self.state.session_valid.store(false, Ordering::Release);
    }
}

impl Drop for FakeHttpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct State {
    behavior: Behavior,
    session_valid: AtomicBool,
    session_counter: AtomicU32,
    initialize_count: AtomicUsize,
    deleted: AtomicBool,
    requests: AtomicUsize,
    hung_calls: AtomicUsize,
    /// The session id the last `initialize` response advertised.
    session_id: Mutex<Option<String>>,
    redirect_to: Mutex<Option<String>>,
    methods: Mutex<Vec<String>>,
    posts: Mutex<Vec<HashMap<String, String>>>,
    subscribed: Mutex<HashSet<String>>,
    sse_tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
    pending_sse: Mutex<Vec<String>>,
}

impl State {
    fn new(behavior: Behavior) -> Self {
        Self {
            behavior,
            session_valid: AtomicBool::new(false),
            session_counter: AtomicU32::new(0),
            initialize_count: AtomicUsize::new(0),
            deleted: AtomicBool::new(false),
            requests: AtomicUsize::new(0),
            hung_calls: AtomicUsize::new(0),
            session_id: Mutex::new(None),
            redirect_to: Mutex::new(None),
            methods: Mutex::new(Vec::new()),
            posts: Mutex::new(Vec::new()),
            subscribed: Mutex::new(HashSet::new()),
            sse_tx: Mutex::new(None),
            pending_sse: Mutex::new(Vec::new()),
        }
    }

    fn broadcast(&self, event: String) {
        // Hold the sender lock across the pending push: serve_sse installs the
        // sender while draining pending under the same lock, so an event can
        // never slip into pending after the drain but before the install.
        let mut sender = self.sse_tx.lock().unwrap();
        if let Some(tx) = sender.as_ref() {
            // The send error hands the event back, so a dead stream queues it
            // for the next registration instead of dropping it.
            if let Err(mpsc::error::SendError(event)) = tx.send(event) {
                *sender = None;
                self.pending_sse.lock().unwrap().push(event);
            }
            return;
        }
        self.pending_sse.lock().unwrap().push(event);
    }

    /// The GET stream and DELETE must carry the session the initialize
    /// response advertised. A missing or foreign id is refused, so a client
    /// regression (dropping the header) cannot pass the suite.
    fn accepts_session(&self, header: Option<&str>) -> bool {
        let current = self.session_id.lock().unwrap();
        match (header, current.as_deref()) {
            (Some(got), Some(current)) => got == current,
            _ => false,
        }
    }
}

struct Request {
    method: String,
    #[allow(dead_code)]
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

struct Response {
    status: &'static str,
    content_type: Option<&'static str>,
    session_id: Option<String>,
    location: Option<String>,
    body: Vec<u8>,
    chunked: bool,
    close: bool,
}

impl Response {
    fn empty(status: &'static str, session_id: Option<String>) -> Self {
        Self {
            status,
            content_type: None,
            session_id,
            location: None,
            body: Vec::new(),
            chunked: false,
            close: false,
        }
    }

    fn json(status: &'static str, value: Value, session_id: Option<String>) -> Self {
        Self {
            status,
            content_type: Some("application/json"),
            session_id,
            location: None,
            body: serde_json::to_vec(&value).unwrap(),
            chunked: false,
            close: false,
        }
    }

    fn redirect(location: String) -> Self {
        Self {
            status: "302 Found",
            content_type: None,
            session_id: None,
            location: Some(location),
            body: Vec::new(),
            chunked: false,
            close: false,
        }
    }
}

async fn handle_connection(stream: tokio::net::TcpStream, state: Arc<State>) {
    state.requests.fetch_add(1, Ordering::AcqRel);
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    while let Some(request) = read_request(&mut reader).await {
        if request.method == "GET" {
            if !state.accepts_session(request.header("mcp-session-id")) {
                let _ =
                    write_http_response(&mut write_half, Response::empty("400 Bad Request", None))
                        .await;
                return;
            }
            serve_sse(&state, &mut write_half, &mut reader).await;
            return;
        }
        if state.behavior.hang_tools_calls > 0
            && request.method == "POST"
            && request_method(&request).as_deref() == Some("tools/call")
            && state.hung_calls.fetch_add(1, Ordering::AcqRel) < state.behavior.hang_tools_calls
        {
            // Record it, then never answer — the connection stays open so the
            // client observes a hung upstream.
            state.methods.lock().unwrap().push("tools/call".to_string());
            continue;
        }
        let response = handle_request(&state, &request).await;
        let close = response.close;
        if write_http_response(&mut write_half, response)
            .await
            .is_err()
            || close
        {
            return;
        }
    }
}

/// The JSON-RPC `method` of a POST body, without recording anything.
fn request_method(request: &Request) -> Option<String> {
    serde_json::from_slice::<Value>(&request.body)
        .ok()
        .and_then(|value| {
            value
                .get("method")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

async fn read_request(reader: &mut BufReader<OwnedReadHalf>) -> Option<Request> {
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await.ok()? == 0 {
        return None;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body).await.ok()?;
    }
    Some(Request {
        method,
        path,
        headers,
        body,
    })
}

async fn write_http_response(
    writer: &mut OwnedWriteHalf,
    response: Response,
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {}\r\n", response.status);
    if response.chunked {
        head.push_str("Transfer-Encoding: chunked\r\n");
    } else {
        head.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
    }
    if let Some(location) = response.location {
        head.push_str(&format!("Location: {location}\r\n"));
    }
    if let Some(content_type) = response.content_type {
        head.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    if let Some(session_id) = response.session_id {
        head.push_str(&format!("Mcp-Session-Id: {session_id}\r\n"));
    }
    head.push_str(if response.close {
        "Connection: close\r\n\r\n"
    } else {
        "Connection: keep-alive\r\n\r\n"
    });
    writer.write_all(head.as_bytes()).await?;
    if response.chunked {
        if response.body.is_empty() {
            writer.write_all(b"0\r\n\r\n").await?;
        } else {
            writer
                .write_all(format!("{:x}\r\n", response.body.len()).as_bytes())
                .await?;
            writer.write_all(&response.body).await?;
            writer.write_all(b"\r\n0\r\n\r\n").await?;
        }
    } else if !response.body.is_empty() {
        writer.write_all(&response.body).await?;
    }
    writer.flush().await
}

/// The standalone GET stream: register the connection, flush anything queued
/// before it arrived, then relay events until the client goes away.
async fn serve_sse(
    state: &Arc<State>,
    writer: &mut OwnedWriteHalf,
    reader: &mut BufReader<OwnedReadHalf>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    {
        // One critical section (sender lock held across the pending drain):
        // a broadcast either lands in pending BEFORE this drain, or sees the
        // installed sender AFTER it. Draining first and installing second
        // would strand an event queued in between until the next connection.
        let mut sender = state.sse_tx.lock().unwrap();
        let pending = std::mem::take(&mut *state.pending_sse.lock().unwrap());
        for event in pending {
            let _ = tx.send(event);
        }
        *sender = Some(tx);
    }
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
    if writer.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    let mut buffer = [0u8; 64];
    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Some(event) => {
                    if writer.write_all(event.as_bytes()).await.is_err() {
                        break;
                    }
                    let _ = writer.flush().await;
                }
                None => break,
            },
            // The client sends nothing on this connection; EOF/error is the
            // teardown signal.
            read = reader.read(&mut buffer) => match read {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            },
        }
    }
}

async fn handle_request(state: &Arc<State>, request: &Request) -> Response {
    if let Some(location) = state.redirect_to.lock().unwrap().clone() {
        return Response::redirect(location);
    }
    if request.method == "DELETE" {
        if !state.accepts_session(request.header("mcp-session-id")) {
            return Response::empty("404 Not Found", None);
        }
        state.deleted.store(true, Ordering::Release);
        return Response::empty("204 No Content", None);
    }
    if request.method != "POST" {
        return Response::empty("405 Method Not Allowed", None);
    }
    state.posts.lock().unwrap().push(request.headers.clone());

    let value: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let id = value.get("id").cloned();
    if !method.is_empty() {
        state.methods.lock().unwrap().push(method.clone());
    }

    let session_header = request.header("mcp-session-id").map(str::to_string);
    // The spec's session-loss signal: any non-initialize request carrying a
    // session the server no longer knows gets a 404.
    if method != "initialize"
        && session_header.is_some()
        && !state.session_valid.load(Ordering::Acquire)
    {
        return Response::json("404 Not Found", json!({ "error": "session expired" }), None);
    }

    let session_id = if method == "initialize" {
        state.session_valid.store(true, Ordering::Release);
        state.initialize_count.fetch_add(1, Ordering::AcqRel);
        let n = state.session_counter.fetch_add(1, Ordering::AcqRel) + 1;
        let id = format!("sess-{n}");
        *state.session_id.lock().unwrap() = Some(id.clone());
        Some(id)
    } else {
        session_header
    };

    let Some(id) = id else {
        // A notification: no response payload.
        return Response::empty("202 Accepted", session_id);
    };

    let params = value.get("params").cloned().unwrap_or(Value::Null);
    let result = match method.as_str() {
        "initialize" => Some(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {}, "resources": { "subscribe": true } },
            "serverInfo": { "name": "fake-http", "version": "0.0.0" }
        })),
        "tools/list" => Some(json!({
            "tools": [
                {
                    "name": "echo",
                    "description": "echo the text back",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "text": { "type": "string" } }
                    }
                },
                {
                    "name": "big",
                    "description": "return a body past the client cap",
                    "inputSchema": { "type": "object" }
                }
            ]
        })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if state.behavior.huge_response && name == "big" {
                let text = "x".repeat(16 * 1024 * 1024 + 1024);
                return Response::json(
                    "200 OK",
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "content": [{ "type": "text", "text": text }] }
                    }),
                    session_id,
                );
            }
            if state.behavior.huge_response_chunked && name == "big" {
                let text = "x".repeat(16 * 1024 * 1024 + 1024);
                let mut response = Response::json(
                    "200 OK",
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "content": [{ "type": "text", "text": text }] }
                    }),
                    session_id,
                );
                // No Content-Length: the cap can only come from the streaming
                // accumulation guard.
                response.chunked = true;
                return response;
            }
            if state.behavior.huge_sse_response && name == "big" {
                let text = "x".repeat(16 * 1024 * 1024 + 1024);
                let message = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "content": [{ "type": "text", "text": text }] }
                });
                return Response {
                    status: "200 OK",
                    content_type: Some("text/event-stream"),
                    session_id,
                    location: None,
                    body: format!("data: {message}\n\n").into_bytes(),
                    chunked: false,
                    close: true,
                };
            }
            let text = if name == "big" {
                "big".to_string()
            } else {
                params
                    .get("arguments")
                    .and_then(|arguments| arguments.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            Some(json!({
                "content": [{ "type": "text", "text": format!("echo: {text}") }]
            }))
        }
        "resources/list" => Some(json!({
            "resources": [{ "uri": "mem://doc", "name": "doc", "mimeType": "text/plain" }]
        })),
        "resources/templates/list" => Some(json!({ "resourceTemplates": [] })),
        "resources/read" => Some(json!({
            "contents": [{ "uri": "mem://doc", "mimeType": "text/plain", "text": "fresh content" }]
        })),
        "resources/subscribe" => {
            if let Some(uri) = params.get("uri").and_then(Value::as_str) {
                state.subscribed.lock().unwrap().insert(uri.to_string());
                if state.behavior.notify_on_subscribe {
                    state.broadcast(format!(
                        "data: {}\n\n",
                        json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/resources/updated",
                            "params": { "uri": uri }
                        })
                    ));
                }
            }
            // A request needs a JSON-RPC response (a 202 acknowledges delivery
            // but leaves `subscribe` waiting for its result).
            Some(json!({}))
        }
        "resources/unsubscribe" => Some(json!({})),
        _ => Some(json!({})),
    };

    let Some(result) = result else {
        return Response::empty("202 Accepted", session_id);
    };
    let message = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    if state.behavior.sse_post_replies {
        let event = format!("data: {message}\n\n");
        return Response {
            status: "200 OK",
            content_type: Some("text/event-stream"),
            session_id,
            location: None,
            body: event.into_bytes(),
            chunked: false,
            close: true,
        };
    }
    Response::json("200 OK", message, session_id)
}
