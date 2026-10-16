//! Connection lifecycle for ki's host-level MCP client.
//!
//! One [`ServerHandle`] per configured stdio server owns the child process and
//! its `rmcp` client; the [`McpManager`] owns the handles for the lifetime of a
//! session. Servers are connected (initialize handshake + `tools/list`) when the
//! session is built — that is where their tool schemas must exist, because the
//! harness is built with them — and the connection then lives across every tool
//! call and turn until [`McpManager::shutdown`].
//!
//! Two behaviors inherited from the retired adapter, both deliberate:
//! * a server that fails to connect is **skipped**, loudly, and never poisons
//!   the session (one bad server must not kill the build);
//! * a connection that dies mid-session is discarded and re-initialized on the
//!   next use — but the tool call that failed is **never replayed**.
//!
//! The adapter kept its connections in a process-global map; this manager puts
//! them under the session instead, so a second session cannot observe (or tear
//! down) the first session's servers.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CancelledNotification, ClientCapabilities, ClientInfo,
    ClientNotification, ClientRequest, Implementation, PaginatedRequestParams, ProtocolVersion,
    RequestId, ServerResult,
};
use rmcp::service::{PeerRequestOptions, RunningService};
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::{RoleClient, ServiceExt};
use rpi_agent::AgentTool;
use serde_json::Value;

use crate::config::{ServerConfig, StdioTransport, Transport};
use crate::tool::McpToolAdapter;

/// The client service type: a `ClientHandler` whose `get_info` carries ki's
/// identity + the protocol version ki asks for.
pub(crate) type Client = RunningService<RoleClient, ClientInfo>;

/// The protocol version ki requests. Everything ki does (streaming `tools/list`
/// pagination, `tools/call`, image content blocks) is in this revision; newer
/// revisions in the same major only add capability surfaces ki does not use.
const REQUESTED_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V_2025_06_18;

/// Protocol versions ki will accept back from a server. A 2024-11-05 server is
/// still a valid tools server; anything outside this set means the server picked
/// a revision whose framing ki has not verified, so the connection is refused
/// loudly rather than used on a guess.
const ACCEPTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

/// How many trailing stderr lines to keep so a failed call can surface *why* the
/// server broke. Bounded: a chatty server must not grow ki's memory.
const STDERR_TAIL_LINES: usize = 40;

/// `tools/list` pagination guards. A server that repeats a cursor or never
/// terminates would otherwise spin forever during session build.
const MAX_TOOL_PAGES: usize = 100;

/// The largest single JSON-RPC frame ki will accept from a server, in bytes.
/// rmcp's read path (`AsyncRwTransport::receive`) uses `read_until` into a
/// buffer it reuses without a cap — the codec's `max_length` is never consulted
/// on the read path — so an oversized, or never newline-terminated, frame would
/// otherwise grow ki's memory without bound. 16 MiB matches the retired
/// adapter's stdio line cap.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// The longest tool name a provider accepts. Anthropic and OpenAI both require
/// `^[A-Za-z0-9_-]{1,64}$`; ki sends the expanded `{server}__{tool}` name
/// verbatim, so a qualified name past this limit makes the provider reject the
/// whole request.
const MAX_TOOL_NAME_LEN: usize = 64;

/// Bound on the graceful close of one server before ki stops waiting.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Env vars a spawned MCP server inherits. Everything else is dropped, so a
/// server never sees the provider API keys ki itself runs with. Values are
/// re-read from ki's own environment; the server's configured `env` is applied
/// afterwards and wins.
const PASSTHROUGH_ENV: &[&str] = &[
    "PATH", "HOME", "TMPDIR", "TERM", "USER", "SHELL", "LANG", "LC_ALL", "LC_CTYPE",
];

fn passthrough_env(key: &str) -> bool {
    PASSTHROUGH_ENV.contains(&key) || key.starts_with("LC_")
}

/// A tool discovered from a server, with its native (namespaced) name.
#[derive(Debug, Clone)]
pub(crate) struct DiscoveredTool {
    /// The server's own tool name (what goes on the wire in `tools/call`).
    pub bare: String,
    /// The native ki tool name: `{server}__{tool}`.
    pub qualified: String,
    pub description: String,
    /// The server's `inputSchema`, passed through untouched.
    pub schema: Value,
}

/// An `AsyncRead` wrapper that fails once more than `budget` bytes have arrived
/// since the last newline, so a single frame cannot exhaust memory. The error
/// ends the rmcp read loop, which closes the connection; the next call
/// re-initializes it.
///
/// The read is staged through an internal scratch buffer rather than written
/// straight into the caller's `buf`: the `AsyncRead` contract requires a poll
/// that returns `Err` to have filled no bytes, and the over-budget rejection
/// happens after the bytes are already read.
struct CappedReader<R> {
    inner: R,
    budget: usize,
    since_newline: usize,
    scratch: Vec<u8>,
}

impl<R> CappedReader<R> {
    fn new(inner: R, budget: usize) -> Self {
        Self {
            inner,
            budget,
            since_newline: 0,
            scratch: Vec::new(),
        }
    }
}

impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for CappedReader<R> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        // Never read more than the budget allows (one past the line start, so a
        // line of exactly `budget` bytes is accepted) nor more than the caller
        // can hold. The `+ 1` is what makes the byte past the cap observable.
        let limit = (this.budget + 1)
            .saturating_sub(this.since_newline)
            .min(buf.remaining());
        if limit == 0 {
            // No room in the caller's buffer; report success, they will re-poll.
            return std::task::Poll::Ready(Ok(()));
        }
        this.scratch.clear();
        this.scratch.resize(limit, 0);
        let filled = {
            let mut tmp = tokio::io::ReadBuf::new(&mut this.scratch);
            match std::pin::Pin::new(&mut this.inner).poll_read(cx, &mut tmp) {
                std::task::Poll::Ready(Ok(())) => tmp.filled().len(),
                other => return other,
            }
        };
        if filled == 0 {
            return std::task::Poll::Ready(Ok(()));
        }
        let mut over_budget = false;
        for &byte in &this.scratch[..filled] {
            if byte == b'\n' {
                this.since_newline = 0;
            } else {
                this.since_newline += 1;
                if this.since_newline > this.budget {
                    over_budget = true;
                    break;
                }
            }
        }
        if over_budget {
            return std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "MCP server sent a frame larger than {} bytes; closing",
                    this.budget
                ),
            )));
        }
        buf.put_slice(&this.scratch[..filled]);
        std::task::Poll::Ready(Ok(()))
    }
}

/// The stdio transport ki hands to rmcp: a capped-read `AsyncRwTransport` over
/// the server's stdout/stdin, plus ownership of the child process tree.
///
/// rmcp's own read path (`AsyncRwTransport::receive`) uses `read_until` into a
/// buffer it reuses without a cap — the codec's `max_length` is never consulted
/// on the read path — so `CappedReader` bounds a single frame. Owning the child
/// here (rather than in `ServerHandle`) keeps the eager-reap behavior of rmcp's
/// `TokioChildProcess`: when the server dies, the read loop ends, the service
/// task drops this transport, and `Drop` reaps the process tree promptly.
struct ChildTransport {
    inner: AsyncRwTransport<
        RoleClient,
        CappedReader<tokio::process::ChildStdout>,
        tokio::process::ChildStdin,
    >,
    child: Option<Box<dyn process_wrap::tokio::ChildWrapper>>,
}

impl rmcp::transport::Transport<RoleClient> for ChildTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: rmcp::service::TxJsonRpcMessage<RoleClient>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.inner.send(item)
    }

    fn receive(
        &mut self,
    ) -> impl std::future::Future<Output = Option<rmcp::service::RxJsonRpcMessage<RoleClient>>> + Send
    {
        self.inner.receive()
    }

    fn close(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        let child = self.child.take();
        let inner = self.inner.close();
        async move {
            let _ = inner.await;
            if let Some(mut child) = child {
                let _ = Box::into_pin(child.kill()).await;
            }
            Ok(())
        }
    }
}

impl Drop for ChildTransport {
    fn drop(&mut self) {
        // Cannot await in `drop`; spawn a best-effort kill of the whole tree —
        // the eager reap when a server dies without an explicit shutdown.
        if let Some(mut child) = self.child.take() {
            tokio::spawn(async move {
                let _ = Box::into_pin(child.kill()).await;
            });
        }
    }
}

/// One configured server plus its live connection. The client is owned here
/// (not shared) so shutdown can consume it; callers get a cheap `Peer` clone.
pub(crate) struct ServerHandle {
    config: ServerConfig,
    cwd: PathBuf,
    client: Mutex<Option<Client>>,
    tools: Vec<DiscoveredTool>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    stopped: AtomicBool,
    /// The child's process-group id (== its pid, since it is the group leader).
    /// `0` when the platform has no process-group wrapper (windows uses a job
    /// object, which reaps descendants on close instead).
    pgid: AtomicU32,
}

impl ServerHandle {
    /// Spawn, handshake and enumerate tools. Fails with a human-readable reason
    /// (the caller logs it and skips the server).
    async fn connect(config: ServerConfig, cwd: &Path) -> Result<Self, String> {
        let stdio = match &config.transport {
            Transport::Stdio(stdio) => stdio.clone(),
            Transport::Http(_) => return Err("http transport is not supported yet".to_string()),
        };
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        // The connect budget covers the whole handshake + listing, so a server
        // that accepts a connection then stalls cannot hold up the session build.
        let (client, pgid, tools) = tokio::time::timeout(
            config.timeout,
            connect_and_list(&config.name, &stdio, cwd, &stderr_tail),
        )
        .await
        .map_err(|_| {
            format!(
                "timed out after {}s during initialize/tools-list",
                config.timeout.as_secs()
            )
        })??;
        Ok(Self {
            config,
            cwd: cwd.to_path_buf(),
            client: Mutex::new(Some(client)),
            tools,
            stderr_tail,
            stopped: AtomicBool::new(false),
            pgid: AtomicU32::new(pgid.unwrap_or(0)),
        })
    }

    fn tools(&self) -> &[DiscoveredTool] {
        &self.tools
    }

    /// A live peer for a call, re-initializing the connection if the previous
    /// one died. A failed *call* is not retried here — only a dead connection is
    /// replaced before the next call.
    pub(crate) async fn peer(&self) -> Result<rmcp::service::Peer<RoleClient>, String> {
        if self.stopped.load(Ordering::Acquire) {
            return Err("manager is shutting down".to_string());
        }
        {
            let guard = self.client.lock().unwrap();
            if let Some(client) = guard.as_ref() {
                // `is_transport_closed` flips as soon as the service task ends
                // (child died / stdout EOF), which is the real liveness signal —
                // `RunningService::is_closed` only tracks explicit shutdown.
                if !client.peer().is_transport_closed() {
                    return Ok(client.peer().clone());
                }
            }
        }
        // Reconnect without holding the lock across the await.
        let stdio = match &self.config.transport {
            Transport::Stdio(stdio) => stdio.clone(),
            Transport::Http(_) => return Err("http transport is not supported yet".to_string()),
        };
        eprintln!(
            "[ki mcp] server {} connection lost; re-initializing on next use",
            self.config.name
        );
        let (fresh, pgid) = tokio::time::timeout(
            self.config.timeout,
            spawn_client(&self.config.name, &stdio, &self.cwd, &self.stderr_tail),
        )
        .await
        .map_err(|_| format!("reconnect to {} timed out", self.config.name))??;

        let mut guard = self.client.lock().unwrap();
        if let Some(existing) = guard.as_ref() {
            if !existing.peer().is_transport_closed() {
                // Another call reconnected first; keep that one and let `fresh`
                // drop (which closes the duplicate and reaps its child).
                return Ok(existing.peer().clone());
            }
        }
        if self.stopped.load(Ordering::Acquire) {
            // A concurrent `shutdown` ran while we were reconnecting. Its
            // awaited teardown has already finished, so installing this fresh
            // connection would leave the new child alive with no one to reap it
            // — drop it (its transport reaps the child) and report stopped.
            return Err("manager is shutting down".to_string());
        }
        let peer = fresh.peer().clone();
        *guard = Some(fresh);
        self.pgid.store(pgid.unwrap_or(0), Ordering::Release);
        Ok(peer)
    }

    /// The trailing stderr of this server, for error messages.
    pub(crate) fn stderr_tail(&self) -> String {
        let guard = self.stderr_tail.lock().unwrap();
        guard.iter().cloned().collect::<Vec<_>>().join("\n")
    }

    pub(crate) fn tool_timeout(&self) -> Duration {
        self.config.timeout
    }

    /// Close the connection and kill the server process tree. Idempotent.
    async fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        let client = self.client.lock().unwrap().take();
        if let Some(mut client) = client {
            // Signal the whole group *before* closing the pipe. Closing stdin is
            // the graceful shutdown, but a server that exits cleanly on EOF takes
            // its stdin with it and rmcp then only reaps it — leaving any
            // grandchildren it spawned behind. Signalling first kills the tree
            // while the pgid still belongs to us (no pid-recycling race).
            signal_process_group(
                &self.config.name,
                &client,
                self.pgid.load(Ordering::Acquire),
            );
            // `close` reaps the child (`ChildTransport::close` kills the tree),
            // and the transport's `Drop` is the backstop if `close` is skipped.
            match client.close_with_timeout(SHUTDOWN_TIMEOUT).await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    eprintln!(
                        "[ki mcp] server {} did not exit within {}s; killing",
                        self.config.name,
                        SHUTDOWN_TIMEOUT.as_secs()
                    );
                    let _ = client.cancel().await;
                }
                Err(error) => {
                    eprintln!("[ki mcp] server {} close error: {error}", self.config.name)
                }
            }
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        // Backstop for the paths where `shutdown()` was never awaited (panic, or
        // a teardown that forgot). Dropping the `RunningService` cancels its
        // service task and drops the transport, which kills the child — the
        // process-group wrapper makes that a whole-tree kill. We cannot await
        // here, so this is best-effort; `shutdown()` is the primary path.
        if let Ok(client) = self.client.get_mut() {
            client.take();
        }
    }
}

/// Session-scoped owner of every configured server.
pub struct McpManager {
    servers: Vec<Arc<ServerHandle>>,
}

impl Default for McpManager {
    /// A manager with no servers — the shape a session with no `mcpServers`
    /// (and the test harnesses) needs without spinning up a runtime.
    fn default() -> Self {
        Self {
            servers: Vec::new(),
        }
    }
}

impl McpManager {
    /// An empty manager. Same as [`Default`]; provided for readability at call
    /// sites that mean "no servers configured".
    pub fn empty() -> Self {
        Self::default()
    }
    /// Connect every enabled, supported server and enumerate its tools.
    ///
    /// Returns the manager plus one issue string per server that was skipped or
    /// partially degraded (disabled, HTTP, connect/handshake failure, a malformed
    /// tool). The caller prints them — nothing is dropped silently.
    pub async fn start(servers: &[ServerConfig], cwd: &Path) -> (Self, Vec<String>) {
        let mut handles = Vec::new();
        let mut issues = Vec::new();
        for config in servers {
            if !config.enabled {
                issues.push(format!(
                    "mcp server {}: disabled (enabled=false), skipped",
                    config.name
                ));
                continue;
            }
            if let Transport::Http(http) = &config.transport {
                issues.push(format!(
                    "mcp server {}: http transport ({}) not supported in this build, skipped",
                    config.name, http.url
                ));
                continue;
            }
            match ServerHandle::connect(config.clone(), cwd).await {
                Ok(handle) => handles.push(Arc::new(handle)),
                Err(reason) => {
                    issues.push(format!("mcp server {}: {reason} — skipped", config.name))
                }
            }
        }
        (Self { servers: handles }, issues)
    }

    /// One native tool adapter per discovered tool, across every live server.
    pub fn tool_adapters(&self) -> Vec<Arc<dyn AgentTool>> {
        let mut adapters: Vec<Arc<dyn AgentTool>> = Vec::new();
        for server in &self.servers {
            for tool in server.tools() {
                adapters.push(Arc::new(McpToolAdapter::new(Arc::clone(server), tool)));
            }
        }
        adapters
    }

    /// Close every server and reap its process tree. Idempotent.
    pub async fn shutdown(&self) {
        for server in &self.servers {
            server.shutdown().await;
        }
    }
}

/// Spawn the child, complete the initialize handshake and enumerate tools.
async fn connect_and_list(
    server: &str,
    stdio: &StdioTransport,
    cwd: &Path,
    stderr_tail: &Arc<Mutex<VecDeque<String>>>,
) -> Result<(Client, Option<u32>, Vec<DiscoveredTool>), String> {
    let (client, pgid) = spawn_client(server, stdio, cwd, stderr_tail).await?;
    match list_all_tools(server, &client).await {
        Ok(tools) => Ok((client, pgid, tools)),
        Err(error) => {
            // The handshake succeeded but discovery did not; don't leak the child.
            let mut client = client;
            let _ = client.close_with_timeout(SHUTDOWN_TIMEOUT).await;
            Err(error)
        }
    }
}

/// Spawn the child and complete initialize + notifications/initialized.
///
/// Returns the running client and the child's process-group id (see
/// [`ServerHandle::pgid`]).
async fn spawn_client(
    server: &str,
    stdio: &StdioTransport,
    cwd: &Path,
    stderr_tail: &Arc<Mutex<VecDeque<String>>>,
) -> Result<(Client, Option<u32>), String> {
    eprintln!(
        "[ki mcp] starting server {}: {} {}",
        server,
        stdio.command,
        stdio.args.join(" ")
    );

    let mut command = tokio::process::Command::new(&stdio.command);
    command.args(&stdio.args);
    // Clear first, then re-add only the allowlisted vars, then the server's own
    // `env` (highest priority) — see PASSTHROUGH_ENV.
    command.env_clear();
    // `vars_os`, not `vars`: std's `vars` panics if any variable in ki's own
    // environment is not valid Unicode, which would abort the whole session
    // build instead of skipping one bad server. Only UTF-8 keys can match the
    // ASCII allowlist; a non-UTF-8 value is forwarded losslessly.
    for (key, value) in std::env::vars_os() {
        if let Some(key) = key.to_str() {
            if passthrough_env(key) {
                command.env(key, value);
            }
        }
    }
    for (key, value) in &stdio.env {
        command.env(key, value);
    }
    match stdio.cwd.as_deref() {
        Some(dir) => command.current_dir(dir),
        None => command.current_dir(cwd),
    };

    // All three stdio pipes are ki's to wire up (rmcp's child transport used to
    // do it): the server writes JSON-RPC to stdout, reads it from stdin, and
    // its stderr is drained into the bounded tail.
    command.stdin(std::process::Stdio::piped());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    // Wrap in a process group (unix) / job object (windows) so a kill reaps the
    // whole tree — the repo's orphan-grandchild lesson. Without this the kill
    // reaches only the leader, leaving a server's own children behind.
    let mut wrapped = process_wrap::tokio::CommandWrap::from(command);
    #[cfg(unix)]
    wrapped.wrap(process_wrap::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    wrapped.wrap(process_wrap::tokio::JobObject);

    let mut child = wrapped
        .spawn()
        .map_err(|error| format!("spawn {:?}: {error}", stdio.command))?;
    // The child leads its own process group (the wrapper above), so its pid is
    // the pgid teardown signals.
    let pgid = child.id();
    let stdout = child
        .stdout()
        .take()
        .ok_or_else(|| format!("spawn {:?}: child stdout was already taken", stdio.command))?;
    let stdin = child
        .stdin()
        .take()
        .ok_or_else(|| format!("spawn {:?}: child stdin was already taken", stdio.command))?;
    if let Some(stderr) = child.stderr().take() {
        spawn_stderr_drain(stderr, Arc::clone(stderr_tail));
    }
    // ki owns the child now: the transport reaps it if the handshake below
    // fails, and on every teardown. rmcp gets a transport built from the raw
    // pipes with a *capped* stdout reader, so one oversized frame cannot OOM the
    // session (rmcp's own read path has no such bound).
    let transport = ChildTransport {
        inner: AsyncRwTransport::<RoleClient, _, _>::new(
            CappedReader::new(stdout, MAX_LINE_BYTES),
            stdin,
        ),
        child: Some(child),
    };

    let client = client_info()
        .serve(transport)
        .await
        .map_err(|error| format!("initialize: {error}"))?;

    if let Some(info) = client.peer().peer_info() {
        let negotiated = info.protocol_version.as_str().to_string();
        if !ACCEPTED_PROTOCOL_VERSIONS.contains(&negotiated.as_str()) {
            let mut client = client;
            let _ = client.close_with_timeout(SHUTDOWN_TIMEOUT).await;
            return Err(format!(
                "server negotiated unsupported protocol version {negotiated}"
            ));
        }
    }
    Ok((client, pgid))
}

/// ki's client identity + requested protocol version for the initialize request.
fn client_info() -> ClientInfo {
    ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("ki", env!("CARGO_PKG_VERSION")),
    )
    .with_protocol_version(REQUESTED_PROTOCOL_VERSION)
}

/// Read every tool via `tools/list`, following cursors. Rejects a repeated
/// cursor and caps the page count so a misbehaving server cannot hang the build.
async fn list_all_tools(server: &str, client: &Client) -> Result<Vec<DiscoveredTool>, String> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors = std::collections::HashSet::new();
    for _ in 0..MAX_TOOL_PAGES {
        let mut params = PaginatedRequestParams::default();
        params.cursor = cursor.clone();
        let result = client
            .peer()
            .list_tools(Some(params))
            .await
            .map_err(|error| format!("tools/list: {error}"))?;
        // Read through serde_json: rmcp's typed `Tool` fields have churned
        // across versions, while the wire shape is part of the protocol.
        let value =
            serde_json::to_value(&result).map_err(|error| format!("tools/list encode: {error}"))?;
        if let Some(items) = value.get("tools").and_then(Value::as_array) {
            for item in items {
                if let Some(tool) = discovered_tool(server, item) {
                    tools.push(tool);
                }
            }
        }
        match value.get("nextCursor").and_then(Value::as_str) {
            None => return Ok(tools),
            Some(next) => {
                if !seen_cursors.insert(next.to_string()) {
                    return Err(format!("tools/list repeated cursor {next:?}"));
                }
                cursor = Some(next.to_string());
            }
        }
    }
    Err(format!("tools/list exceeded {MAX_TOOL_PAGES} pages"))
}

/// The native tool name separator. Part of the public surface (`{server}__{tool}`).
pub(crate) const TOOL_NAME_SEPARATOR: &str = "__";

fn discovered_tool(server: &str, item: &Value) -> Option<DiscoveredTool> {
    let bare = item.get("name").and_then(Value::as_str)?.to_string();
    if !valid_tool_name(&bare) {
        eprintln!(
            "[ki mcp] server {server}: skipping tool with unsupported name {bare:?} (allowed: [A-Za-z0-9_-], non-empty, no \"{TOOL_NAME_SEPARATOR}\")"
        );
        return None;
    }
    let description = item
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let schema = item
        .get("inputSchema")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({ "type": "object" }));
    let qualified = format!("{server}{TOOL_NAME_SEPARATOR}{bare}");
    // Providers require tool names matching `[A-Za-z0-9_-]{1,64}`; the expanded
    // name is sent verbatim, so one past the limit would make the provider
    // reject the whole request. Skip it (loudly) rather than poison every turn.
    if qualified.len() > MAX_TOOL_NAME_LEN {
        eprintln!(
            "[ki mcp] server {server}: skipping tool {bare:?}: qualified name {qualified:?} exceeds {MAX_TOOL_NAME_LEN} chars"
        );
        return None;
    }
    Some(DiscoveredTool {
        qualified,
        bare,
        description,
        schema,
    })
}

/// Tool names can be wider than server names (dots are common), but must not be
/// empty and must not contain the separator — that would make the `{server}__{tool}`
/// expansion ambiguous.
fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(TOOL_NAME_SEPARATOR)
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Why a `tools/call` did not produce a result. `Cancelled` is kept distinct so
/// the adapter can report an abort (not a server failure) back to the loop.
pub(crate) enum CallError {
    Cancelled,
    Failed(String),
}

/// The `notifications/cancelled` a client sends when it abandons an in-flight
/// request, so the server can stop the work instead of computing a result
/// nobody will read.
///
/// Built through `serde_json` rather than a struct expression: rmcp's
/// `Notification` is `#[non_exhaustive]` (and its serde impl is the documented
/// wire shape), so the JSON path is both the only legal one from outside the
/// crate and the version-robust one.
fn cancellation(request_id: RequestId) -> Option<ClientNotification> {
    let value = serde_json::json!({
        "method": "notifications/cancelled",
        "params": { "requestId": request_id, "reason": "cancelled by ki" }
    });
    let notification: CancelledNotification = serde_json::from_value(value).ok()?;
    Some(ClientNotification::CancelledNotification(notification))
}

/// Run a `tools/call` on this server and return the raw result JSON.
///
/// Honours `signal` (the caller's cancellation token) at both await points and
/// applies the per-request timeout on the wire. A failure returns the server's
/// trailing stderr appended, so "it broke" comes with "and here is why".
pub(crate) async fn call_tool(
    handle: &ServerHandle,
    bare_tool: &str,
    arguments: Option<serde_json::Map<String, Value>>,
    signal: &tokio_util::sync::CancellationToken,
) -> Result<Value, CallError> {
    // `peer()` re-initializes a dead connection, which can take up to the
    // server timeout; honor the caller's cancellation there too, not only once
    // the request is on the wire.
    let peer = tokio::select! {
        biased;
        _ = signal.cancelled() => return Err(CallError::Cancelled),
        result = handle.peer() => {
            result.map_err(|error| CallError::Failed(handle.decorate(error)))?
        }
    };
    let mut params = CallToolRequestParams::new(bare_tool.to_string());
    params.arguments = arguments;
    let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
    let options = PeerRequestOptions::with_timeout(handle.tool_timeout());

    let handle_req = tokio::select! {
        biased;
        _ = signal.cancelled() => return Err(CallError::Cancelled),
        result = peer.send_cancellable_request(request, options) => {
            result.map_err(|error| CallError::Failed(handle.decorate(error.to_string())))?
        }
    };
    // Once the request is on the wire, an abort must tell the server to stop
    // working on it (the MCP cancellation contract) rather than just walk away.
    let request_id = handle_req.id.clone();
    let raw: Result<ServerResult, rmcp::ServiceError> = tokio::select! {
        biased;
        _ = signal.cancelled() => {
            if let Some(notification) = cancellation(request_id) {
                let _ = peer.send_notification(notification).await;
            }
            return Err(CallError::Cancelled);
        }
        result = handle_req.await_response() => result,
    };
    match raw {
        Ok(ServerResult::CallToolResult(result)) => serde_json::to_value(&result)
            .map_err(|error| CallError::Failed(handle.decorate(format!("result encode: {error}")))),
        Ok(_) => Err(CallError::Failed(
            handle.decorate("unexpected response type to tools/call".to_string()),
        )),
        Err(error) => Err(CallError::Failed(handle.decorate(error.to_string()))),
    }
}

impl ServerHandle {
    /// Append this server's stderr tail to an error message (once).
    fn decorate(&self, message: String) -> String {
        let tail = self.stderr_tail();
        if tail.trim().is_empty() {
            message
        } else {
            format!("{message}\n--- server stderr ---\n{tail}")
        }
    }
}

/// Ask the child's whole process group to terminate. See [`ServerHandle::shutdown`]
/// for why this runs before the transport is closed.
#[cfg(unix)]
fn signal_process_group(server: &str, client: &Client, pgid: u32) {
    if pgid == 0 || client.peer().is_transport_closed() {
        return;
    }
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;
    if let Err(error) = killpg(Pid::from_raw(pgid as i32), Signal::SIGTERM) {
        eprintln!("[ki mcp] server {server}: could not signal process group {pgid}: {error}");
    }
}

/// On windows the child is in a job object with `KILL_ON_JOB_CLOSE`, so the
/// descendants die when the transport (and thus the job) is dropped.
#[cfg(not(unix))]
fn signal_process_group(_server: &str, _client: &Client, _pgid: u32) {}

/// Drain a child's stderr into the bounded tail buffer. Continuous draining is
/// required regardless — a full pipe would block the server.
fn spawn_stderr_drain(stderr: tokio::process::ChildStderr, tail: Arc<Mutex<VecDeque<String>>>) {
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut guard = tail.lock().unwrap();
            if guard.len() >= STDERR_TAIL_LINES {
                guard.pop_front();
            }
            guard.push_back(line);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_reject_dots_and_the_separator() {
        assert!(valid_tool_name("search_index"));
        assert!(valid_tool_name("search-index"));
        assert!(valid_tool_name("Search123"));
        // A dot would make the provider reject the whole request.
        assert!(!valid_tool_name("search.index"));
        // The separator would make the `{server}__{tool}` expansion ambiguous.
        assert!(!valid_tool_name("a__b"));
        assert!(!valid_tool_name(""));
    }

    #[test]
    fn oversized_or_dotted_qualified_tool_names_are_skipped() {
        let ok = serde_json::json!({ "name": "echo" });
        assert!(discovered_tool("srv", &ok).is_some());
        // `srv__` (5) + 60 chars = 65, past the provider's 64-char limit.
        let long = serde_json::json!({ "name": "a".repeat(60) });
        assert!(discovered_tool("srv", &long).is_none());
        let dotted = serde_json::json!({ "name": "search.index" });
        assert!(discovered_tool("srv", &dotted).is_none());
    }

    #[tokio::test]
    async fn capped_reader_rejects_a_frame_over_the_budget() {
        use tokio::io::AsyncReadExt;
        // 32 bytes with no newline, budget 16: the read must fail rather than
        // buffer without bound.
        let data = vec![b'x'; 32];
        let mut reader = CappedReader::new(&data[..], 16);
        let mut out = Vec::new();
        let error = reader.read_to_end(&mut out).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        // Newline-delimited data within the budget streams through unchanged.
        let data = b"hello\nworld\n".to_vec();
        let mut reader = CappedReader::new(&data[..], 16);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, data);
    }
}
