//! Connection lifecycle for ki's host-level MCP client.
//!
//! One [`ServerHandle`] per configured server — a stdio child process or an
//! HTTP/Streamable session — owns the transport and its `rmcp` client; the
//! [`McpManager`] owns the handles for the lifetime of a session. Servers are
//! connected (initialize handshake + `tools/list`) when the session is built —
//! that is where their tool schemas must exist, because the harness is built
//! with them — and the connection then lives across every tool call and turn
//! until [`McpManager::shutdown`].
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
//!
//! ## Resources and subscriptions (batch 2)
//!
//! A server that advertises resources contributes a read tool,
//! `{server}__read_resource` (and, when it also advertises
//! `resources.subscribe`, a `{server}__watch_resource` verb); reads happen
//! **on demand** — the product
//! semantic for `notifications/resources/updated` is a light notice (server +
//! uri) into a shared [`McpNoticeMailbox`], never an automatic read of the
//! content. The one opt-in exception is a uri listed in the server's
//! `autoRead` config: the boundary drain reads it and appends the text to the
//! notice entry. An HTTP server is a real notification source: rmcp keeps the
//! standalone GET SSE stream open, so updates flow exactly like stdio.
//! (The adapter's blanket auto-read + turn-end batch is not ported; its
//! delivery-lifetime and restore semantics are — see
//! [`ServerHandle::subscribe_uri`], [`ServerHandle::unsubscribe_uri`] and
//! [`restore_subscriptions`].)
//!
//! Subscription state lives per server *alongside* the connection
//! (`ServerShared`), not inside it: a `peer()` re-init builds a fresh handler
//! around the same state, which is how subscriptions survive a reconnect. A
//! config-identity change cannot replay subscriptions onto a new server here —
//! each [`ServerHandle`] is constructed from exactly one config, so a changed
//! identity is a new handle with a fresh (empty) subscription set.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http::{HeaderName, HeaderValue};
use kiqi_agent::AgentTool;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CancelledNotification, ClientCapabilities, ClientInfo,
    ClientNotification, ClientRequest, ErrorCode, Implementation, PaginatedRequestParams,
    ProtocolVersion, ReadResourceRequest, ReadResourceRequestParams, RequestId,
    ResourceUpdatedNotificationParam, ServerResult, SubscribeRequestParams,
    UnsubscribeRequestParams,
};
use rmcp::service::{NotificationContext, PeerRequestOptions, RunningService};
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::{Peer, RoleClient, ServiceError, ServiceExt};
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

use crate::config::{HttpTransport, ServerConfig, StdioTransport, Transport};
use crate::notice::McpNoticeMailbox;
use crate::tool::{McpResourceToolAdapter, McpResourceWatchToolAdapter, McpToolAdapter};

/// The client service type: a [`KiClientHandler`] whose `get_info` carries ki's
/// identity + the protocol version ki asks for.
pub(crate) type Client = RunningService<RoleClient, KiClientHandler>;

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

/// The most bytes kept from a single stderr line. A progress bar written with
/// `\r` and no `\n` is one unbounded "line"; the tail deque bounds the number
/// of lines, not their size, so one such line would otherwise grow without
/// limit and then be copied into every decorated error.
const STDERR_LINE_MAX_BYTES: usize = 16 * 1024;

/// `tools/list` pagination guards. A server that repeats a cursor or never
/// terminates would otherwise spin forever during session build.
const MAX_TOOL_PAGES: usize = 100;

/// `resources/list` / `resources/templates/list` pagination guards (same shape
/// as the tools ones).
const MAX_RESOURCE_PAGES: usize = 100;

/// The largest single JSON-RPC frame ki will accept from a server, in bytes.
/// rmcp's read path (`AsyncRwTransport::receive`) uses `read_until` into a
/// buffer it reuses without a cap — the codec's `max_length` is never consulted
/// on the read path — so an oversized, or never newline-terminated, frame would
/// otherwise grow ki's memory without bound. 16 MiB matches the retired
/// adapter's stdio line cap. The HTTP client enforces the same bound per
/// response/event.
pub(crate) const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

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
///
/// The session id is forwarded separately ([`session_env`]) rather than being
/// added here: it is a deliberate grant, not ambient inheritance.
const PASSTHROUGH_ENV: &[&str] = &[
    "PATH", "HOME", "TMPDIR", "TERM", "USER", "SHELL", "LANG", "LC_ALL", "LC_CTYPE",
];

fn passthrough_env(key: &str) -> bool {
    PASSTHROUGH_ENV.contains(&key) || key.starts_with("LC_")
}

/// The extra variable every spawned server gets: this session's id.
///
/// A server that publishes resource updates needs to tell *this session's* own
/// writes from a peer's, and it can only do that if it is told which session it
/// is speaking to. `AGENT_ID_ENV` exists precisely so a child process can name
/// its session (its doc says so, and it is also what a `bash` tool call sees),
/// so forwarding it is the documented use — the value is a session uuid, not a
/// secret. `SESSION_ID_ENV` is deliberately *not* forwarded: a nested `ki`
/// adopts it and would report its ancestor's session forever.
///
/// Named through the SDK constant, never restated: a restated name is how the
/// sibling `HOST_PRIVATE_ENV` channel silently re-leaked after a rename (see
/// `kiqi_tools::os_env`).
///
/// Only stdio servers can be told this way — an HTTP server has no environment,
/// so an HTTP collab server must carry the id in its configured `headers`
/// (`HttpTransport::headers`) instead.
fn session_env() -> Option<(&'static str, std::ffi::OsString)> {
    std::env::var_os(kiqi_agent::AGENT_ID_ENV).map(|value| (kiqi_agent::AGENT_ID_ENV, value))
}

/// The native tool name separator. Part of the public surface (`{server}__{tool}`).
pub const TOOL_NAME_SEPARATOR: &str = "__";

/// The read tool's suffix: `{server}__read_resource`.
pub const READ_RESOURCE_SUFFIX: &str = "read_resource";

/// The subscription tool's suffix: `{server}__watch_resource`.
pub const WATCH_RESOURCE_SUFFIX: &str = "watch_resource";

/// The largest single resource payload (a read result's text, or the read
/// tool's description) ki will hand to the model. The retired adapter's
/// `MAX_RESOURCE_TEXT` — a large blob must not reach the model whole.
pub(crate) const MAX_RESOURCE_TEXT: usize = 20_000;

/// Char-boundary-safe truncate + marker (the adapter's `bound_resource_text`).
pub(crate) fn bound_resource_text(mut out: String) -> String {
    if out.len() > MAX_RESOURCE_TEXT {
        let mut end = MAX_RESOURCE_TEXT;
        while end > 0 && !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push_str("\n[truncated]");
    }
    out
}

/// Flatten a serialized `ReadResourceResult` to text for an `autoRead` notice.
/// Blobs become an honest one-line description rather than megabytes of
/// base64; the whole result is bounded like every other resource text.
fn resource_text(value: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(items) = value.get("contents").and_then(Value::as_array) {
        for item in items {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                parts.push(text.to_string());
            } else if let Some(blob) = item.get("blob").and_then(Value::as_str) {
                parts.push(format!("[blob, {} base64 chars]", blob.len()));
            }
        }
    }
    bound_resource_text(parts.join("\n"))
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

/// Delivery lifetime for one (connection, uri): the `Arc` pointer IS the
/// lifetime, the counter the newest-read revision within it (the adapter's
/// `Arc<AtomicU64>` pattern). ki never auto-reads, so the counter stays 0 for
/// now — the pointer identity is what the update path checks, and the counter
/// is kept so a later read-dedup needs no type change.
type SubscriptionToken = Arc<AtomicU64>;

/// Who created a subscription. Config watches are the user's durable opt-in;
/// model watches are session-scoped and must not be wake-eligible unless the
/// same uri is also configured, nor removable by the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubscriptionOrigin {
    Config,
    Model,
}

impl SubscriptionOrigin {
    /// How `/mcp-doctor` names the origin: a config subscription survives a
    /// reload, a model-started watch does not.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Model => "model",
        }
    }
}

#[derive(Clone)]
struct SubscriptionEntry {
    token: SubscriptionToken,
    origin: SubscriptionOrigin,
}
/// Cap on model-created watches. Config subscriptions are bounded by the user's
/// own settings; this is the loop/prompt-injection bound for the runtime verb.
pub const MAX_MODEL_SUBSCRIPTIONS: usize = 32;

/// State that OUTLIVES one connection: the subscription map and the
/// `list_changed` stale flags. A reconnect builds a fresh handler around the
/// same `Arc`s — that is how subscriptions survive a `peer()` re-init (the
/// adapter's entry owned them, not the session).
#[derive(Default)]
struct ServerShared {
    subscriptions: Mutex<BTreeMap<String, SubscriptionEntry>>,
    stale: StaleFlags,
    /// Set by the HTTP client when the server's session id changed: rmcp
    /// transparently re-initialized after a 404, and the new server session
    /// carries none of the old resource subscriptions. The next
    /// [`ServerHandle::ensure_live`] restores them in place.
    http_session_rotated: Arc<AtomicBool>,
}

/// Per-server freshness flags set by `notifications/tools/list_changed` and
/// `notifications/resources/list_changed`. Freshness is otherwise by re-polling
/// per call; these only mark that the NEXT boundary should re-list.
#[derive(Default)]
struct StaleFlags {
    tools: AtomicBool,
    resources: AtomicBool,
}

/// ki's client handler: the identity batch 1 sent plus the notification
/// surfaces batch 2 uses. One instance per connection; the shared state it
/// holds outlives any single connection.
pub(crate) struct KiClientHandler {
    info: ClientInfo,
    server: String,
    shared: Arc<ServerShared>,
    mailbox: Arc<McpNoticeMailbox>,
}

impl ClientHandler for KiClientHandler {
    /// Same identity the plain `ClientInfo` generic carried in batch 1.
    fn get_info(&self) -> ClientInfo {
        self.info.clone()
    }

    async fn on_resource_updated(
        &self,
        params: ResourceUpdatedNotificationParam,
        context: NotificationContext<RoleClient>,
    ) {
        // Hold the map lock across the live-token check AND the push, so a
        // concurrent subscribe/unsubscribe/restore (which rotates + purges
        // under the same lock) can never slip between them. Never take the
        // connection lock here: it is held across subscription RPCs, and this
        // runs on rmcp's dispatch task, which must not block behind them.
        let map = self.shared.subscriptions.lock().unwrap();
        if !map.contains_key(&params.uri) {
            return;
        }
        self.mailbox.push_with_provenance(
            &self.server,
            &params.uri,
            authored_sessions_from_meta(&context.meta),
        );
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.shared.stale.tools.store(true, Ordering::Release);
    }

    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.shared.stale.resources.store(true, Ordering::Release);
    }
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
///
/// The connection slot is a `tokio::sync::Mutex` — the one async lock in the
/// handle — because a subscription operation must hold the connection lock
/// ACROSS its RPC (mark-before-RPC, serialize subscribe/unsubscribe/restore,
/// exactly the adapter's `with_session` lock). Tool calls and reads only clone
/// the peer and release.
pub(crate) struct ServerHandle {
    config: ServerConfig,
    cwd: PathBuf,
    client: AsyncMutex<Option<Client>>,
    tools: Mutex<Vec<DiscoveredTool>>,
    /// The read tool's description: the server's resource + template list,
    /// refreshed when `resources/list_changed` marks it stale.
    resource_description: Mutex<String>,
    advertises_resources: AtomicBool,
    /// The server's `resources.subscribe` capability. The watch tool is only
    /// exposed when this is true; otherwise a `resources/subscribe` would be a
    /// guaranteed protocol error.
    supports_subscribe: AtomicBool,
    shared: Arc<ServerShared>,
    mailbox: Arc<McpNoticeMailbox>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    stopped: AtomicBool,
    /// Cancelled by `shutdown`: an in-flight request must fail promptly
    /// instead of waiting out its own timeout while teardown runs.
    shutdown: tokio_util::sync::CancellationToken,
    /// The child's process-group id (== its pid, since it is the group leader).
    /// `0` when the platform has no process-group wrapper (windows uses a job
    /// object, which reaps descendants on close instead) or when the transport
    /// is HTTP.
    pgid: AtomicU32,
    /// `httpIdleTimeout` from ki's settings, applied when building an HTTP
    /// client (the provider-side policy). Ignored by stdio transports.
    http_idle_timeout_ms: Option<i64>,
}

impl ServerHandle {
    /// Spawn, handshake and enumerate tools. Fails with a human-readable reason
    /// (the caller logs it and skips the server).
    async fn connect(
        config: ServerConfig,
        cwd: &Path,
        mailbox: Arc<McpNoticeMailbox>,
        http_idle_timeout_ms: Option<i64>,
    ) -> Result<Self, String> {
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        let shared = Arc::new(ServerShared::default());
        // The connect budget covers the whole handshake + listing, so a server
        // that accepts a connection then stalls cannot hold up the session build.
        let (client, pgid, tools, advertises_resources, supports_subscribe, description) =
            tokio::time::timeout(
                config.timeout,
                connect_and_list(
                    &config,
                    cwd,
                    &stderr_tail,
                    Arc::clone(&shared),
                    Arc::clone(&mailbox),
                    http_idle_timeout_ms,
                ),
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
            client: AsyncMutex::new(Some(client)),
            tools: Mutex::new(tools),
            resource_description: Mutex::new(description),
            advertises_resources: AtomicBool::new(advertises_resources),
            supports_subscribe: AtomicBool::new(supports_subscribe),
            shared,
            mailbox,
            stderr_tail,
            stopped: AtomicBool::new(false),
            shutdown: tokio_util::sync::CancellationToken::new(),
            pgid: AtomicU32::new(pgid.unwrap_or(0)),
            http_idle_timeout_ms,
        })
    }

    pub(crate) fn server_name(&self) -> &str {
        &self.config.name
    }

    /// The qualified name of one of this server's synthetic resource tools
    /// (`{server}__{suffix}`), or `None` (with a diagnostic) when it would
    /// exceed the provider's name budget. The validated name is handed to the
    /// adapter so the gate and the registered name cannot drift apart.
    fn synthetic_tool_name(&self, suffix: &str) -> Option<String> {
        let name = format!("{}{TOOL_NAME_SEPARATOR}{suffix}", self.server_name());
        if name.len() > MAX_TOOL_NAME_LEN {
            eprintln!(
                "[ki mcp] server {}: skipping resource tool {name:?}: exceeds {MAX_TOOL_NAME_LEN} chars",
                self.server_name()
            );
            None
        } else {
            Some(name)
        }
    }

    /// A snapshot of the tools discovered from this server (re-listed when
    /// `tools/list_changed` marked the set stale and a boundary refreshed it).
    fn tools(&self) -> Vec<DiscoveredTool> {
        self.tools.lock().unwrap().clone()
    }

    fn replace_tools(&self, tools: Vec<DiscoveredTool>) {
        *self.tools.lock().unwrap() = tools;
    }

    pub(crate) fn resource_description(&self) -> String {
        self.resource_description.lock().unwrap().clone()
    }

    fn replace_resource_description(&self, description: String) {
        *self.resource_description.lock().unwrap() = description;
    }

    fn advertises_resources(&self) -> bool {
        self.advertises_resources.load(Ordering::Acquire)
    }

    fn supports_subscribe(&self) -> bool {
        self.supports_subscribe.load(Ordering::Acquire)
    }

    /// Whether the model may create runtime watches on this server.
    fn model_watch(&self) -> bool {
        self.config.model_watch
    }

    /// Re-derive the cached resource capabilities from a (re)connected peer.
    /// Both the read tool (`resources`) and the watch tool
    /// (`resources.subscribe`) are chosen from these two flags, so a capability
    /// flip across a reconnect or rmcp's transparent HTTP re-init must mark the
    /// boundary stale for a tool re-merge.
    fn refresh_resource_caps(&self, peer: &Peer<RoleClient>) {
        let caps = peer
            .peer_info()
            .and_then(|info| info.capabilities.resources.clone());
        let advertises = caps.is_some();
        let subscribes = caps
            .as_ref()
            .and_then(|caps| caps.subscribe)
            .unwrap_or(false);
        let advertises_changed = self
            .advertises_resources
            .swap(advertises, Ordering::Release)
            != advertises;
        let subscribes_changed =
            self.supports_subscribe.swap(subscribes, Ordering::Release) != subscribes;
        if advertises_changed || subscribes_changed {
            self.shared.stale.resources.store(true, Ordering::Release);
        }
    }

    /// A live peer for a call, re-initializing the connection if the previous
    /// one died. A failed *call* is not retried here — only a dead connection is
    /// replaced before the next call.
    pub(crate) async fn peer(&self) -> Result<Peer<RoleClient>, String> {
        if self.stopped.load(Ordering::Acquire) {
            return Err("manager is shutting down".to_string());
        }
        let mut guard = self.client.lock().await;
        let (peer, _) = self.ensure_live(&mut guard, None).await?;
        Ok(peer)
    }

    /// Under an already-held connection lock: a live peer, (re)connecting when
    /// the transport died. This is the one place a connection is created, and it
    /// runs the subscription restore BEFORE the new connection is installed —
    /// the adapter's ordering (after `Session::start`, before the session is
    /// installed/run), so a restore failure never leaves a half-restored
    /// connection behind: the fresh child is dropped (its transport reaps it)
    /// and the error propagates.
    ///
    /// `skip_restore_uri` is the uri an in-flight subscribe/unsubscribe call is
    /// handling itself: restore must not issue a second RPC for it — that call
    /// sends the connection's single RPC for this uri, with its fresh lifetime
    /// installed before any early notification.
    async fn ensure_live(
        &self,
        guard: &mut Option<Client>,
        skip_restore_uri: Option<&str>,
    ) -> Result<(Peer<RoleClient>, bool), String> {
        // Consume the re-init flag on BOTH paths: a reconnect restores every
        // subscription anyway, so leaving it set would restore twice (a
        // duplicate-rejecting server would then reject the second subscribe).
        let session_rotated = self
            .shared
            .http_session_rotated
            .swap(false, Ordering::AcqRel);
        if let Some(client) = guard.as_ref() {
            // `is_transport_closed` flips as soon as the service task ends
            // (child died / stdout EOF), which is the real liveness signal —
            // `RunningService::is_closed` only tracks explicit shutdown.
            if !client.peer().is_transport_closed() {
                // The transport stayed up but the server session it speaks to
                // was replaced (rmcp's transparent 404 re-init). MCP
                // subscriptions are per session, so the fresh session has
                // none: restore them here, in place, rather than leaving the
                // old map looking subscribed. Restore ALL uris (no skip):
                // `subscribe_uri` short-circuits on an existing entry, so
                // skipping the caller's uri would leave it unsubscribed on the
                // new session.
                if session_rotated {
                    self.refresh_resource_caps(client.peer());
                    if let Err(error) = restore_subscriptions(
                        client.peer(),
                        &self.config.name,
                        &self.shared.subscriptions,
                        &self.mailbox,
                        None,
                        self.config.timeout,
                    )
                    .await
                    {
                        // Keep the flag: the next call retries the restore.
                        self.shared
                            .http_session_rotated
                            .store(true, Ordering::Release);
                        return Err(error);
                    }
                }
                return Ok((client.peer().clone(), false));
            }
        }
        let transport = &self.config.transport;
        eprintln!(
            "[ki mcp] server {} connection lost; re-initializing on next use",
            self.config.name
        );
        let (fresh, pgid) = tokio::time::timeout(
            self.config.timeout,
            spawn_client(
                &self.config.name,
                transport,
                &self.cwd,
                &self.stderr_tail,
                Arc::clone(&self.shared),
                Arc::clone(&self.mailbox),
                self.http_idle_timeout_ms,
                self.config.timeout,
            ),
        )
        .await
        .map_err(|_| format!("reconnect to {} timed out", self.config.name))??;
        if self.stopped.load(Ordering::Acquire) {
            // A concurrent `shutdown` fired while we were reconnecting. Its
            // awaited teardown has already finished, so installing this fresh
            // connection would leave the new child alive with no one to reap it
            // — drop it (its transport reaps the child) and report stopped.
            return Err("manager is shutting down".to_string());
        }
        let peer = fresh.peer().clone();
        // A reconnect that flips a resource capability updates the read/watch
        // tools' presence at the next boundary (a list_changed may never come).
        self.refresh_resource_caps(&peer);
        restore_subscriptions(
            &peer,
            &self.config.name,
            &self.shared.subscriptions,
            &self.mailbox,
            skip_restore_uri,
            self.config.timeout,
        )
        .await?;
        *guard = Some(fresh);
        self.pgid.store(pgid.unwrap_or(0), Ordering::Release);
        Ok((peer, true))
    }

    // -- Subscription lifecycle (ported from the adapter) -------------------

    /// Rotate `uri` to a fresh delivery lifetime and purge its queued notices.
    /// Stale content under the old lifetime can never deliver. Always lock the
    /// subscription map before the mailbox — the update path uses the same
    /// order, so nothing can slip in after the purge.
    fn rotate_lifetime(&self, uri: &str, origin: SubscriptionOrigin) -> SubscriptionToken {
        let token = Arc::new(AtomicU64::new(0));
        // Hold the map guard across the purge: the update path checks the
        // token and pushes under this same lock, so a notice accepted by the
        // FRESH lifetime can never be destroyed by its own rotation (upstream
        // `add_subscription` purges under the subscriptions lock for exactly
        // this reason).
        let mut map = self.shared.subscriptions.lock().unwrap();
        map.insert(
            uri.to_string(),
            SubscriptionEntry {
                token: token.clone(),
                origin,
            },
        );
        self.mailbox.purge(&self.config.name, uri);
        token
    }

    /// Remove `uri`'s lifetime + purge its queue. With `expected`, remove ONLY
    /// if the map still holds that exact token — a failed older operation must
    /// never retire a newer lifetime created by a concurrent subscribe.
    fn remove_lifetime(&self, uri: &str, expected: Option<&SubscriptionToken>) {
        {
            let mut map = self.shared.subscriptions.lock().unwrap();
            let matches = match (map.get(uri), expected) {
                (_, None) => true,
                (Some(current), Some(expected)) => Arc::ptr_eq(&current.token, expected),
                (None, Some(_)) => false,
            };
            if !matches {
                return;
            }
            map.remove(uri);
        }
        self.mailbox.purge(&self.config.name, uri);
    }

    /// Drop every subscription of this server and purge its queued notices —
    /// a config-identity change or a rejected restore must never leave a
    /// half-remembered subscription set behind.
    fn clear_subscriptions(&self) {
        self.shared.subscriptions.lock().unwrap().clear();
        self.mailbox.purge_server(&self.config.name);
    }

    /// The uris this session currently watches plus their origin, sorted.
    /// Includes both config-driven and model-driven subscriptions.
    pub(crate) fn subscriptions(&self) -> Vec<(String, SubscriptionOrigin)> {
        self.shared
            .subscriptions
            .lock()
            .unwrap()
            .iter()
            .map(|(uri, entry)| (uri.clone(), entry.origin))
            .collect()
    }

    /// Host-level `resources/subscribe`. Driven by the `subscribe` config at
    /// startup and by the `{server}__watch_resource` tool at runtime. Ported
    /// semantics:
    /// * already-subscribed on a live transport sends **no** second RPC and
    ///   leaves the existing lifetime + queued notices untouched — a defensive
    ///   re-start must not drop a pending update;
    /// * a fresh connection rotates the lifetime and purges stale notices;
    /// * the mark is installed BEFORE the RPC under the connection lock, so an
    ///   update pushed during the handshake is accepted;
    /// * a server rejection rolls the token back with an expected-token guard
    ///   and propagates the error.
    ///
    /// A [`SubscriptionOrigin::Model`] subscription is capped at
    /// [`MAX_MODEL_SUBSCRIPTIONS`]; config subscriptions are bounded by the
    /// user's own settings.
    pub(crate) async fn subscribe_uri(
        &self,
        uri: &str,
        origin: SubscriptionOrigin,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<(), String> {
        if self.stopped.load(Ordering::Acquire) {
            return Err("manager is shutting down".to_string());
        }
        let mut guard = self.client.lock().await;
        let (peer, fresh) = self.ensure_live(&mut guard, Some(uri)).await?;
        let already = !fresh && self.shared.subscriptions.lock().unwrap().contains_key(uri);
        if already {
            return Ok(());
        }
        // A model `watch_resource start` on a URI the user configured must not
        // downgrade it: on a fresh connection `already` is false (the URI still
        // has to be re-subscribed on the new session), and rotating with
        // `Model` here would let a later model stop pass the "config watches
        // are not removable" guard.
        let existing_origin = self
            .shared
            .subscriptions
            .lock()
            .unwrap()
            .get(uri)
            .map(|entry| entry.origin);
        let origin = match existing_origin {
            Some(SubscriptionOrigin::Config) => SubscriptionOrigin::Config,
            _ => origin,
        };
        if origin == SubscriptionOrigin::Model {
            let model_count = self
                .shared
                .subscriptions
                .lock()
                .unwrap()
                .values()
                .filter(|entry| entry.origin == SubscriptionOrigin::Model)
                .count();
            if model_count >= MAX_MODEL_SUBSCRIPTIONS {
                return Err(format!(
                    "refusing to watch another resource: the model subscription limit ({MAX_MODEL_SUBSCRIPTIONS}) is reached"
                ));
            }
        }
        // Registered before the RPC so early notifications are accepted, while
        // the connection lock keeps subscribe/unsubscribe ordered.
        let token = self.rotate_lifetime(uri, origin);
        let request = tokio::time::timeout(
            self.config.timeout,
            peer.subscribe(SubscribeRequestParams::new(uri.to_string())),
        );
        let request = match cancel {
            Some(cancel) => tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    // Drop the local mark so a server that did install the
                    // subscription anyway is ignored until the next reconnect.
                    self.remove_lifetime(uri, Some(&token));
                    return Err("resources/subscribe cancelled".to_string());
                }
                outcome = request => outcome,
            },
            None => request.await,
        };
        match request {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                self.remove_lifetime(uri, Some(&token));
                Err(self.decorate(format!("resources/subscribe {uri}: {error}")))
            }
            Err(_) => {
                self.remove_lifetime(uri, Some(&token));
                Err(format!(
                    "resources/subscribe {uri} timed out after {}s",
                    self.config.timeout.as_secs()
                ))
            }
        }
    }

    /// Host-level `resources/unsubscribe`. Ported semantics: the local
    /// lifetime is removed BEFORE the RPC (invalidate-then-send — delivery
    /// stops even on failure); the RPC result is discarded (always reported
    /// unsubscribed); a REJECTED unsubscribe retires the transport, because the
    /// remote registration may outlive our state — the next call reconnects,
    /// restores the OTHER subscriptions once each, and this uri is freely
    /// re-subscribable.
    ///
    /// A never-watched uri is a no-op (no RPC), so a typo cannot retire the
    /// connection. A model requester may not remove a config-owned watch.
    pub(crate) async fn unsubscribe_uri(
        &self,
        uri: &str,
        requester: SubscriptionOrigin,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<(), String> {
        if self.stopped.load(Ordering::Acquire) {
            return Err("manager is shutting down".to_string());
        }
        let previous = self.shared.subscriptions.lock().unwrap().get(uri).cloned();
        let Some(previous) = previous else {
            return Ok(());
        };
        if requester == SubscriptionOrigin::Model && previous.origin == SubscriptionOrigin::Config {
            return Err(
                "refusing to stop a configured subscription; edit the server's `subscribe` list instead"
                    .to_string(),
            );
        }
        let mut guard = self.client.lock().await;
        let established = match self.ensure_live(&mut guard, Some(uri)).await {
            Ok((peer, _)) => {
                self.remove_lifetime(uri, None);
                let request = tokio::time::timeout(
                    self.config.timeout,
                    peer.unsubscribe(UnsubscribeRequestParams::new(uri.to_string())),
                );
                let result = match cancel {
                    Some(cancel) => tokio::select! {
                        biased;
                        // Local delivery already stopped; the remote registration
                        // is cleaned up on the next reconnect if it survives.
                        _ = cancel.cancelled() => return Ok(()),
                        result = request => result,
                    },
                    None => request.await,
                };
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) | Err(_) => {
                        eprintln!(
                            "[ki mcp] server {}: resources/unsubscribe {uri} failed; retiring the connection so the next call reconnects",
                            self.config.name
                        );
                        if let Some(dead) = guard.take() {
                            let _ = dead.cancel().await;
                        }
                    }
                }
                true
            }
            Err(_) => false,
        };
        if !established {
            // `ensure_live` may have rotated the token mid-restore before
            // failing; the stop still wins, and no concurrent subscribe could
            // have succeeded on a connection that was not established.
            self.remove_lifetime(uri, None);
        }
        Ok(())
    }

    /// Re-list tools / resources for servers a `list_changed` notification
    /// marked stale. Called at a session boundary; per-call freshness is
    /// otherwise by re-polling. A failed re-list keeps the stale flag (the next
    /// boundary retries). Returns whether anything changed (the caller re-merges
    /// the harness tool set).
    pub async fn refresh_if_stale(&self) -> bool {
        let mut changed = false;
        let name = self.config.name.clone();
        if self.shared.stale.tools.load(Ordering::Acquire) {
            let result = tokio::time::timeout(self.config.timeout, async {
                let peer = self.peer().await?;
                list_all_tools(&name, &peer).await
            })
            .await;
            match result {
                Ok(Ok(tools)) => {
                    self.replace_tools(tools);
                    self.shared.stale.tools.store(false, Ordering::Release);
                    changed = true;
                }
                Ok(Err(error)) => eprintln!(
                    "[ki mcp] server {name}: tools/list refresh failed ({error}); will retry at the next boundary"
                ),
                Err(_) => eprintln!(
                    "[ki mcp] server {name}: tools/list refresh timed out; will retry at the next boundary"
                ),
            }
        }
        if self.shared.stale.resources.load(Ordering::Acquire) {
            if !self.advertises_resources() {
                // The server re-negotiated WITHOUT the resources capability on
                // reconnect: `resources/list` is structurally unsupported there
                // (a conforming server answers -32601), so the refresh could
                // never succeed and retrying would burn one RPC plus one
                // stderr warning at every boundary, forever. Retire the read
                // tool's slice now instead — `tool_adapters` omits the read
                // tool when the capability is absent, and `changed` makes the
                // next boundary swap the registry.
                self.replace_resource_description(String::new());
                self.shared.stale.resources.store(false, Ordering::Release);
                changed = true;
            } else {
                let result = tokio::time::timeout(self.config.timeout, async {
                    let peer = self.peer().await?;
                    Ok::<_, String>(describe_resources(&name, &peer).await)
                })
                .await;
                match result {
                    Ok(Ok(description)) => {
                        self.replace_resource_description(description.text);
                        // A method that failed for a reason a retry could fix
                        // stays stale so the next boundary tries again; a
                        // `-32601` is settled and clears the flag (retrying it
                        // would cost one RPC plus one warning per boundary,
                        // forever).
                        if description.complete {
                            self.shared.stale.resources.store(false, Ordering::Release);
                        }
                        changed = true;
                    }
                    Ok(Err(error)) => eprintln!(
                        "[ki mcp] server {name}: resource listing refresh failed ({error}); will retry at the next boundary"
                    ),
                    Err(_) => eprintln!(
                        "[ki mcp] server {name}: resource listing refresh timed out; will retry at the next boundary"
                    ),
                }
            }
        }
        changed
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
        // Release in-flight requests first: they race this token in
        // `send_request`, so teardown is not held up by a request whose reply
        // will never arrive.
        self.shutdown.cancel();
        // Late notifications must not deliver: clear the delivery lifetimes and
        // the queued notices BEFORE the connection closes. (The handler dies
        // with the client, but an update already inside dispatch would
        // otherwise still find its token.) The adapter cancels its stop first,
        // keeps the entry registered until its dispatcher joins, then clears
        // subscriptions and removes the entry — same ordering intent.
        self.clear_subscriptions();
        let client = self.client.lock().await.take();
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

/// Re-register only resource subscriptions when a transport is recreated.
/// User tool calls are never replayed. Rotate identities and purge the old
/// connection's queued notices atomically before accepting early notifications.
///
/// Sorted uris, exactly one `resources/subscribe` per uri — except
/// `skip_uri`, the uri an in-flight subscribe/unsubscribe call is handling
/// itself. Interrupted (connection broke/stalled): subscriptions are KEPT and
/// the error says the next call will retry. Server actively rejects ONE uri:
/// only that uri is dropped (and its queued notices purged); the rest are
/// restored, so one bad model watch can never wipe the user's durable set.
async fn restore_subscriptions(
    peer: &Peer<RoleClient>,
    server: &str,
    subscriptions: &Mutex<BTreeMap<String, SubscriptionEntry>>,
    mailbox: &McpNoticeMailbox,
    skip_uri: Option<&str>,
    budget: Duration,
) -> Result<(), String> {
    let uris = {
        let mut map = subscriptions.lock().unwrap();
        let mut uris = Vec::new();
        for (uri, entry) in map.iter_mut() {
            // An explicit subscribe/unsubscribe handles this uri once the
            // connection is ready; do not issue a duplicate RPC.
            if skip_uri == Some(uri.as_str()) {
                continue;
            }
            entry.token = Arc::new(AtomicU64::new(0));
            uris.push(uri.clone());
        }
        drop(map);
        mailbox.purge_server(server);
        uris.sort();
        uris
    };
    // One budget for the whole restore, not per uri: K subscriptions must not
    // be able to hold the connection lock for K timeouts.
    let deadline = tokio::time::Instant::now() + budget;
    let mut rejected = Vec::new();
    for uri in uris {
        let outcome = tokio::time::timeout_at(
            deadline,
            peer.subscribe(SubscribeRequestParams::new(uri.clone())),
        )
        .await;
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(ServiceError::McpError(error))) => {
                // Drop just this uri; the rest of the set is still valid.
                subscriptions.lock().unwrap().remove(&uri);
                mailbox.purge(server, &uri);
                rejected.push(format!("{uri} ({error})"));
            }
            Ok(Err(error)) => {
                return Err(format!(
                    "resource subscription restoration interrupted at {uri} ({error}); the next call will retry"
                ));
            }
            Err(_) => {
                return Err(format!(
                    "resource subscription restoration interrupted at {uri} (timed out after {}s total); the next call will retry",
                    budget.as_secs()
                ));
            }
        }
    }
    if !rejected.is_empty() {
        eprintln!(
            "[ki mcp] server {server}: dropped {} subscription(s) rejected during restore: {}",
            rejected.len(),
            rejected.join(", ")
        );
    }
    Ok(())
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

impl Drop for ServerHandle {
    fn drop(&mut self) {
        // Backstop for the paths where `shutdown()` was never awaited (panic, or
        // a teardown that forgot). Dropping the `RunningService` cancels its
        // service task and drops the transport, which kills the child — the
        // process-group wrapper makes that a whole-tree kill. We cannot await
        // here, so this is best-effort; `shutdown()` is the primary path.
        drop(self.client.get_mut().take());
    }
}

/// Session-scoped owner of every configured server.
/// What happened to one configured server at the last start/reload.
#[derive(Clone, Debug)]
pub(crate) enum ServerOutcome {
    Connected,
    Disabled,
    Failed(String),
}

/// One configured server plus its outcome. Kept even when the connection failed,
/// because "configured but absent" is exactly what a diagnostic has to explain.
#[derive(Clone, Debug)]
pub(crate) struct ServerRecord {
    pub(crate) config: ServerConfig,
    pub(crate) outcome: ServerOutcome,
}

/// One `/mcp-doctor` row: what is configured, whether it connected, and what it
/// currently advertises. Built from the last start/reload — read-only, no RPCs.
#[derive(Default)]
pub struct ServerDiagnostic {
    pub name: String,
    /// `stdio: npx -y foo --bar` or `http: https://host/mcp`.
    pub launch: String,
    pub status: ServerStatus,
    /// Why it is not connected (`Disabled` / `Failed` only).
    pub reason: Option<String>,
    /// Qualified tool names this server contributes (`{server}__{tool}`).
    pub tools: Vec<String>,
    pub advertises_resources: bool,
    pub supports_subscribe: bool,
    /// The uris the settings asked for at startup.
    pub configured_subscribe: Vec<String>,
    /// The subscriptions actually live now: `(uri, "config" | "model")`.
    pub live_subscriptions: Vec<(String, &'static str)>,
    pub auto_read: Vec<String>,
    pub signals: crate::config::SignalsConfig,
    /// Mistyped keys that would otherwise behave exactly like correct ones.
    pub unknown_keys: Vec<String>,
    /// The child's last stderr lines (stdio only) — where a server explains
    /// itself when the protocol-level error is uninformative.
    pub stderr_tail: Vec<String>,
}

/// How a configured server stands right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ServerStatus {
    #[default]
    Connected,
    Disabled,
    Failed,
}

impl ServerStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Disabled => "disabled (enabled=false)",
            Self::Failed => "failed",
        }
    }
}

pub struct McpManager {
    /// Swappable so `/mcp-reload` can replace the live connections in place:
    /// every holder (tool adapters, the notice mailbox, the TUI) keeps this
    /// same `Arc<McpManager>`.
    servers: std::sync::RwLock<Vec<Arc<ServerHandle>>>,
    /// Every configured server with what happened to it at the last
    /// start/reload. `servers` only holds the handles that connected, so this is
    /// the only place a disabled or failed entry still exists — which is what
    /// `/mcp-doctor` reports.
    records: std::sync::RwLock<Vec<ServerRecord>>,
    mailbox: Arc<McpNoticeMailbox>,
    /// Every configured server name (connected, skipped or failed), so the
    /// kiqi-cli tool-slice replace can recognize every MCP-contributed name —
    /// including a server that went away after contributing tools.
    configured_names: std::sync::RwLock<Vec<String>>,
    /// The MCP tool names that passed the session's tool policy
    /// (`--tools`/`--exclude-tools`/`--no-tools`), recorded by kiqi-cli's merge.
    /// A boundary refresh consults this so it can never resurrect a name the
    /// policy excluded at build. `None` until the first merge.
    allowed_tools: Mutex<Option<HashSet<String>>>,
    /// The MCP tool names actually present in the host registry, recorded by
    /// kiqi-cli at every merge/boundary replace. The replace removes exactly
    /// these — never the wider [`McpManager::owns_tool_name`] prefix, which
    /// would also swallow a non-MCP tool that won a build-time name collision
    /// against a server's namespace. `None` until the first merge.
    injected_tools: Mutex<Option<HashSet<String>>>,
}

impl Default for McpManager {
    /// A manager with no servers — the shape a session with no `mcpServers`
    /// (and the test harnesses) needs without spinning up a runtime.
    fn default() -> Self {
        Self {
            servers: std::sync::RwLock::new(Vec::new()),
            records: std::sync::RwLock::new(Vec::new()),
            mailbox: Arc::new(McpNoticeMailbox::new()),
            configured_names: std::sync::RwLock::new(Vec::new()),
            allowed_tools: Mutex::new(None),
            injected_tools: Mutex::new(None),
        }
    }
}

impl McpManager {
    /// An empty manager. Same as [`Default`]; provided for readability at call
    /// sites that mean "no servers configured".
    pub fn empty() -> Self {
        Self::default()
    }
    /// Connect every enabled server and enumerate its tools.
    ///
    /// Returns the manager plus one issue string per server that was skipped or
    /// partially degraded (disabled, connect/handshake failure, a malformed
    /// tool, a failed config-driven subscribe). The caller prints them — nothing
    /// is dropped silently.
    pub async fn start(servers: &[ServerConfig], cwd: &Path) -> (Self, Vec<String>) {
        Self::start_with_http_policy(servers, cwd, None).await
    }

    /// [`McpManager::start`] with the provider-side HTTP policy applied to HTTP
    /// servers: `http_idle_timeout_ms` is ki's `httpIdleTimeout` setting (`0`
    /// disables idle expiry, `None` keeps the library default). Proxy
    /// resolution always follows ki's environment, exactly like the providers.
    pub async fn start_with_http_policy(
        servers: &[ServerConfig],
        cwd: &Path,
        http_idle_timeout_ms: Option<i64>,
    ) -> (Self, Vec<String>) {
        let mailbox = Arc::new(McpNoticeMailbox::new());
        let (handles, records, issues) =
            connect_servers(servers, cwd, Arc::clone(&mailbox), http_idle_timeout_ms).await;
        let configured_names = records
            .iter()
            .map(|record| record.config.name.clone())
            .collect();
        (
            Self {
                servers: std::sync::RwLock::new(handles),
                records: std::sync::RwLock::new(records),
                mailbox,
                configured_names: std::sync::RwLock::new(configured_names),
                allowed_tools: Mutex::new(None),
                injected_tools: Mutex::new(None),
            },
            issues,
        )
    }

    /// Replace the live connections with a fresh set from `servers`.
    ///
    /// The manager keeps its identity — and its notice mailbox — so a reload is
    /// invisible to everything holding an `Arc<McpManager>`: new notices land in
    /// the mailbox the session already drains, and the tool registry is rebuilt
    /// from [`Self::tool_adapters`] by the caller's normal reload path. The old
    /// connections are shut down (process trees reaped) only after the swap.
    pub async fn reload(
        &self,
        servers: &[ServerConfig],
        cwd: &Path,
        http_idle_timeout_ms: Option<i64>,
    ) -> Vec<String> {
        let (handles, records, issues) = connect_servers(
            servers,
            cwd,
            Arc::clone(&self.mailbox),
            http_idle_timeout_ms,
        )
        .await;
        let configured_names = records
            .iter()
            .map(|record| record.config.name.clone())
            .collect();
        let previous = std::mem::replace(&mut *self.servers.write().unwrap(), handles);
        *self.records.write().unwrap() = records;
        *self.configured_names.write().unwrap() = configured_names;
        // The names a boundary replace may remove are recomputed by the next
        // merge; keeping the old records would let it delete a name that a new
        // server just re-registered.
        *self.allowed_tools.lock().unwrap() = None;
        *self.injected_tools.lock().unwrap() = None;
        for handle in previous {
            handle.shutdown().await;
        }
        issues
    }

    /// One native tool adapter per discovered tool, across every live server —
    /// plus one `{server}__read_resource` read tool per server advertising
    /// resources, and one `{server}__watch_resource` subscription tool when the
    /// server also advertises `resources.subscribe`. A tools-only server adds
    /// zero tools beyond its tools.
    pub fn tool_adapters(&self) -> Vec<Arc<dyn AgentTool>> {
        let mut adapters: Vec<Arc<dyn AgentTool>> = Vec::new();
        for server in self.servers.read().unwrap().iter() {
            for tool in server.tools() {
                // `read_resource` / `watch_resource` are reserved synthetic
                // verbs. A server tool with the same bare name would otherwise
                // win the CLI's first-wins collision gate and hide the
                // documented resource verbs.
                let reserved = (server.advertises_resources() && tool.bare == READ_RESOURCE_SUFFIX)
                    || (server.supports_subscribe() && tool.bare == WATCH_RESOURCE_SUFFIX);
                if reserved {
                    eprintln!(
                        "[ki mcp] server {}: server tool {:?} collides with a synthetic resource tool; skipped",
                        server.server_name(),
                        tool.qualified
                    );
                    continue;
                }
                adapters.push(Arc::new(McpToolAdapter::new(Arc::clone(server), &tool)));
            }
            if server.advertises_resources() {
                if let Some(name) = server.synthetic_tool_name(READ_RESOURCE_SUFFIX) {
                    adapters.push(Arc::new(McpResourceToolAdapter::new(
                        Arc::clone(server),
                        name,
                        server.resource_description(),
                    )));
                }
                if server.supports_subscribe() && server.model_watch() {
                    if let Some(name) = server.synthetic_tool_name(WATCH_RESOURCE_SUFFIX) {
                        adapters.push(Arc::new(McpResourceWatchToolAdapter::new(
                            Arc::clone(server),
                            name,
                        )));
                    }
                }
            }
        }
        adapters
    }

    /// Record the MCP tool names that passed the session's tool policy. kiqi-cli
    /// calls this on every merge; [`McpManager::tool_allowed`] then gates the
    /// boundary refresh.
    pub fn record_allowed_tools(&self, names: HashSet<String>) {
        *self.allowed_tools.lock().unwrap() = Some(names);
    }

    /// Record the MCP tool names actually present in the host registry. kiqi-cli
    /// records at every merge/boundary replace; the next replace then removes
    /// exactly these names. This — not [`McpManager::owns_tool_name`] — is the
    /// removal boundary: the prefix heuristic also matches a non-MCP tool that
    /// happened to win a build-time name collision against a server's
    /// namespace, and removing it would silently delete a user tool.
    pub fn record_injected_tools(&self, names: HashSet<String>) {
        *self.injected_tools.lock().unwrap() = Some(names);
    }

    /// The last recorded injected set — [`McpManager::record_injected_tools`] —
    /// or `None` before the first merge/replace (the caller falls back to the
    /// prefix heuristic).
    pub fn injected_tools(&self) -> Option<HashSet<String>> {
        self.injected_tools.lock().unwrap().clone()
    }

    /// Whether `name` may (re)enter the harness tool set on a boundary refresh.
    /// A name that never passed the build-time policy can never be resurrected
    /// by a refresh.
    pub fn tool_allowed(&self, name: &str) -> bool {
        let guard = self.allowed_tools.lock().unwrap();
        let Some(set) = guard.as_ref() else {
            return true;
        };
        if set.contains(name) {
            return true;
        }
        // A watch tool can appear after a reconnect capability flip, after the
        // build-time allow set was recorded. Its read sibling did exist then;
        // if that sibling passed the policy, let the watch verb follow it.
        if let Some(prefix) = name.strip_suffix(WATCH_RESOURCE_SUFFIX) {
            let read_name = format!("{prefix}{READ_RESOURCE_SUFFIX}");
            return set.contains(&read_name);
        }
        false
    }

    /// Whether a native tool name's first `__` segment names one of this
    /// manager's configured servers — a NAMESPACE check. It answers true for
    /// any tool shaped `{server}__…`, including a non-MCP tool that won a
    /// build-time name collision, so it must NOT be used as the slice-replace
    /// removal boundary: use [`McpManager::injected_tools`] (the names this
    /// manager actually contributed) for that.
    pub fn owns_tool_name(&self, name: &str) -> bool {
        let Some((prefix, _)) = name.split_once(TOOL_NAME_SEPARATOR) else {
            return false;
        };
        self.configured_names
            .read()
            .unwrap()
            .iter()
            .any(|server| server == prefix)
    }

    /// Drain the resource-update notice mailbox. Called at session boundaries
    /// (never mid-run); empty between drains.
    pub fn take_notices(&self) -> crate::notice::McpNoticeBatch {
        self.mailbox.take()
    }

    /// Boundary drain with the `autoRead` opt-in applied: a notice whose uri a
    /// server lists in `autoRead` is read through that server's connection now,
    /// and the text rides the notice entry (bounded at [`MAX_RESOURCE_TEXT`]).
    /// Every other notice stays a light pointer. A failed read degrades to the
    /// light notice — the update is still delivered, loudly degraded.
    pub async fn take_notices_with_auto_read(&self) -> crate::notice::McpNoticeBatch {
        let mut batch = self.mailbox.take();
        // Snapshot the live set once: a reload may swap it mid-batch, and the
        // handles here must stay the ones the notices came from.
        let servers = self.servers.read().unwrap().clone();
        // ONE deadline for the whole batch. A per-read timeout alone multiplies
        // by the number of notices (up to `MAX_NOTICE_ENTRIES`), so a server
        // that stops responding would stall a turn boundary for N x timeout --
        // and the boundary is never inside the caller's cancel token. The
        // budget is the largest per-call timeout among the servers that will
        // actually be read, so the batch never waits longer than one server
        // timeout no matter how many updates queued.
        let budget = batch
            .notices
            .iter()
            .filter_map(|notice| {
                let handle = servers
                    .iter()
                    .find(|handle| handle.server_name() == notice.server)?;
                handle
                    .config
                    .auto_read
                    .iter()
                    .any(|uri| uri == &notice.uri)
                    .then(|| handle.tool_timeout())
            })
            .max();
        let deadline = budget.map(|budget| tokio::time::Instant::now() + budget);
        for notice in &mut batch.notices {
            let Some(handle) = servers
                .iter()
                .find(|handle| handle.server_name() == notice.server)
            else {
                continue;
            };
            if !handle.config.auto_read.iter().any(|uri| uri == &notice.uri) {
                continue;
            }
            let Some(deadline) = deadline else {
                continue;
            };
            let signal = tokio_util::sync::CancellationToken::new();
            let outcome =
                tokio::time::timeout_at(deadline, read_resource(handle, &notice.uri, &signal))
                    .await;
            match outcome {
                Ok(Ok(value)) => notice.content = Some(resource_text(&value)),
                Ok(Err(CallError::Failed(error))) => eprintln!(
                    "[ki mcp] server {}: autoRead {} failed ({error}); delivering the light notice",
                    notice.server, notice.uri
                ),
                Ok(Err(CallError::Cancelled)) => eprintln!(
                    "[ki mcp] server {}: autoRead {} was cancelled; delivering the light notice",
                    notice.server, notice.uri
                ),
                Err(_) => {
                    let budget_secs = budget.map(|budget| budget.as_secs()).unwrap_or(0);
                    eprintln!(
                        "[ki mcp] server {}: autoRead {} timed out (batch budget {}s); delivering the light notice",
                        notice.server, notice.uri, budget_secs
                    );
                }
            }
        }
        batch
    }

    /// `/mcp-doctor`: one row per configured server, including the ones that
    /// never connected. Read-only — no RPC, no blocking on a dead socket — so a
    /// user (or the model reading the note) can run it at any time, including
    /// after the symptom appeared.
    pub fn doctor(&self) -> Vec<ServerDiagnostic> {
        let servers = self.servers.read().unwrap();
        let records = self.records.read().unwrap();
        records
            .iter()
            .map(|record| {
                let handle = servers
                    .iter()
                    .find(|handle| handle.server_name() == record.config.name);
                // `Connected` is the start-time outcome. A stdio child that died
                // since then is the case this command exists for, so ask the
                // transport — `try_lock` keeps the doctor non-blocking.
                let transport_closed = handle
                    .map(|handle| match handle.client.try_lock() {
                        Ok(client) => client
                            .as_ref()
                            .map(|client| client.peer().is_transport_closed())
                            .unwrap_or(false),
                        Err(_) => false,
                    })
                    .unwrap_or(false);
                ServerDiagnostic {
                    name: record.config.name.clone(),
                    launch: launch_of(&record.config),
                    status: if transport_closed {
                        ServerStatus::Failed
                    } else {
                        match &record.outcome {
                            ServerOutcome::Connected => ServerStatus::Connected,
                            ServerOutcome::Disabled => ServerStatus::Disabled,
                            ServerOutcome::Failed(_) => ServerStatus::Failed,
                        }
                    },
                    reason: if transport_closed {
                        Some(
                            "the transport is closed: the server exited or the connection \
                             dropped; run /mcp-reload to start it again"
                                .to_string(),
                        )
                    } else {
                        match &record.outcome {
                            ServerOutcome::Failed(reason) => Some(reason.clone()),
                            _ => None,
                        }
                    },
                    tools: handle
                        .map(|handle| {
                            handle
                                .tools()
                                .iter()
                                .map(|tool| tool.qualified.clone())
                                .collect()
                        })
                        .unwrap_or_default(),
                    advertises_resources: handle
                        .map(|handle| handle.advertises_resources())
                        .unwrap_or(false),
                    supports_subscribe: handle
                        .map(|handle| handle.supports_subscribe())
                        .unwrap_or(false),
                    configured_subscribe: record.config.subscribe.clone(),
                    live_subscriptions: handle
                        .map(|handle| {
                            handle
                                .subscriptions()
                                .into_iter()
                                .map(|(uri, origin)| (uri, origin.as_str()))
                                .collect()
                        })
                        .unwrap_or_default(),
                    auto_read: record.config.auto_read.clone(),
                    signals: record.config.signals.clone(),
                    unknown_keys: record.config.unknown_keys.clone(),
                    stderr_tail: handle
                        .map(|handle| split_stderr_tail(&handle.stderr_tail()))
                        .unwrap_or_default(),
                }
            })
            .collect()
    }

    /// The rendered `/mcp-doctor` report: the shape the TUI prints, so the CLI
    /// and the tests share one renderer instead of drifting apart.
    pub fn doctor_report(&self) -> String {
        render_doctor(&self.doctor(), self.pending_notice_count())
    }

    /// How many notices are currently queued, WITHOUT draining them (drain
    /// with [`McpManager::take_notices`]). Lets a caller wait for a push to
    /// have landed before acting on the mailbox.
    pub fn pending_notice_count(&self) -> usize {
        self.mailbox.pending_count()
    }

    /// Wait for the next notice pushed into this session's mailbox. This is an
    /// edge, not the data: the caller still drains with
    /// [`McpManager::take_notices`] / [`McpManager::take_notices_with_auto_read`].
    pub async fn wait_for_notice(&self) {
        self.mailbox.notified().await;
    }

    /// The parsed wake policy for a connected server. `None` for an unknown or
    /// failed server — it cannot produce notices, so it has nothing to wake.
    pub fn signal_config(&self, server: &str) -> Option<crate::config::SignalsConfig> {
        self.servers
            .read()
            .unwrap()
            .iter()
            .find(|handle| handle.server_name() == server)
            .map(|handle| handle.config.signals.clone())
    }

    /// Wake policy for one `(server, uri)`. Only a uri in the server's config
    /// `subscribe` list is wake-eligible: a model-started watch is record-only,
    /// so it cannot manufacture a wake source or consume the user's budget.
    pub fn signal_config_for(
        &self,
        server: &str,
        uri: &str,
    ) -> Option<crate::config::SignalsConfig> {
        let handle = self
            .servers
            .read()
            .unwrap()
            .iter()
            .find(|handle| handle.server_name() == server)
            .cloned()?;
        if !handle
            .config
            .subscribe
            .iter()
            .any(|configured| configured == uri)
        {
            return None;
        }
        Some(handle.config.signals.clone())
    }

    /// Subscribe one server's resource (config startup or the model's watch
    /// tool).
    pub async fn subscribe(&self, server: &str, uri: &str) -> Result<(), String> {
        let handle = self
            .servers
            .read()
            .unwrap()
            .iter()
            .find(|handle| handle.server_name() == server)
            .cloned()
            .ok_or_else(|| format!("mcp server {server} is not running"))?;
        handle
            .subscribe_uri(uri, SubscriptionOrigin::Config, None)
            .await
    }

    /// Unsubscribe one server's resource. Always reports unsubscribed, even
    /// when the remote unsubscribe failed (the local delivery stopped either
    /// way; a rejected unsubscribe retires the transport instead).
    pub async fn unsubscribe(&self, server: &str, uri: &str) -> Result<(), String> {
        let handle = self
            .servers
            .read()
            .unwrap()
            .iter()
            .find(|handle| handle.server_name() == server)
            .cloned()
            .ok_or_else(|| format!("mcp server {server} is not running"))?;
        handle
            .unsubscribe_uri(uri, SubscriptionOrigin::Config, None)
            .await
    }

    /// Re-list tools / resources for servers marked stale by `list_changed`.
    /// Returns whether anything changed.
    pub async fn refresh_if_stale(&self) -> bool {
        let mut changed = false;
        // Snapshot before awaiting: a lock guard is not `Send`, and the future
        // is driven on other threads.
        let servers = self.servers.read().unwrap().clone();
        for server in servers {
            changed |= server.refresh_if_stale().await;
        }
        changed
    }

    /// Close every server and reap its process tree. Idempotent. Also clears
    /// every subscription (and queued notice) — a torn-down connection must
    /// never leave deliverable state behind.
    pub async fn shutdown(&self) {
        let servers = self.servers.read().unwrap().clone();
        for server in servers {
            server.shutdown().await;
        }
    }
}

/// Connect every enabled server and auto-subscribe the configured uris.
/// Shared by [`McpManager::start_with_http_policy`] and
/// [`McpManager::reload`]; the mailbox is the caller's, so a reload keeps
/// delivering notices to the session that installed it.
async fn connect_servers(
    servers: &[ServerConfig],
    cwd: &Path,
    mailbox: Arc<McpNoticeMailbox>,
    http_idle_timeout_ms: Option<i64>,
) -> (Vec<Arc<ServerHandle>>, Vec<ServerRecord>, Vec<String>) {
    let mut records = Vec::new();
    let mut handles = Vec::new();
    let mut issues = Vec::new();
    for config in servers {
        if !config.enabled {
            issues.push(format!(
                "mcp server {}: disabled (enabled=false), skipped",
                config.name
            ));
            records.push(ServerRecord {
                config: config.clone(),
                outcome: ServerOutcome::Disabled,
            });
            continue;
        }
        match ServerHandle::connect(
            config.clone(),
            cwd,
            Arc::clone(&mailbox),
            http_idle_timeout_ms,
        )
        .await
        {
            Ok(handle) => {
                let handle = Arc::new(handle);
                for uri in &handle.config.subscribe {
                    if let Err(error) = handle
                        .subscribe_uri(uri, SubscriptionOrigin::Config, None)
                        .await
                    {
                        issues.push(format!(
                            "mcp server {}: subscribing {uri}: {error} — subscription skipped",
                            config.name
                        ));
                    }
                }
                // Record AFTER the config subscriptions: the diagnostic reads
                // the live subscription map, so it must be the post-subscribe
                // state or it would report every configured uri as missing.
                records.push(ServerRecord {
                    config: config.clone(),
                    outcome: ServerOutcome::Connected,
                });
                handles.push(handle);
            }
            Err(reason) => {
                issues.push(format!("mcp server {}: {reason} — skipped", config.name));
                records.push(ServerRecord {
                    config: config.clone(),
                    outcome: ServerOutcome::Failed(reason),
                });
            }
        }
    }
    (handles, records, issues)
}

/// Connect the configured transport, complete the initialize handshake,
/// enumerate tools and (when advertised) describe the resource surface for the
/// read tool.
#[allow(clippy::type_complexity)]
async fn connect_and_list(
    config: &ServerConfig,
    cwd: &Path,
    stderr_tail: &Arc<Mutex<VecDeque<String>>>,
    shared: Arc<ServerShared>,
    mailbox: Arc<McpNoticeMailbox>,
    http_idle_timeout_ms: Option<i64>,
) -> Result<(Client, Option<u32>, Vec<DiscoveredTool>, bool, bool, String), String> {
    let server = config.name.as_str();
    let (client, pgid) = spawn_client(
        server,
        &config.transport,
        cwd,
        stderr_tail,
        Arc::clone(&shared),
        mailbox,
        http_idle_timeout_ms,
        config.timeout,
    )
    .await?;
    let resource_caps = client
        .peer()
        .peer_info()
        .and_then(|info| info.capabilities.resources.clone());
    let advertises_resources = resource_caps.is_some();
    let supports_subscribe = resource_caps
        .as_ref()
        .and_then(|caps| caps.subscribe)
        .unwrap_or(false);
    let peer = client.peer();
    match list_all_tools(server, &peer).await {
        Ok(tools) => {
            let description = if advertises_resources {
                let description = describe_resources(server, &peer).await;
                // A listing that half-failed is retried at the next boundary.
                // Nothing else marks it stale (`resources/list_changed` does),
                // so without this the warning below would be the last word on
                // it and the read tool would carry the gap forever.
                if !description.complete {
                    shared.stale.resources.store(true, Ordering::Release);
                }
                description.text
            } else {
                String::new()
            };
            Ok((
                client,
                pgid,
                tools,
                advertises_resources,
                supports_subscribe,
                description,
            ))
        }
        Err(error) => {
            // The handshake succeeded but discovery did not; don't leak the child.
            let mut client = client;
            let _ = client.close_with_timeout(SHUTDOWN_TIMEOUT).await;
            Err(error)
        }
    }
}

/// Connect the configured transport. Stdio spawns a child process; HTTP hands
/// rmcp's streamable-HTTP worker a repo-owned reqwest client (see `src/http.rs`)
/// and lets rmcp own the session/SSE protocol.
#[allow(clippy::too_many_arguments)]
async fn spawn_client(
    server: &str,
    transport: &Transport,
    cwd: &Path,
    stderr_tail: &Arc<Mutex<VecDeque<String>>>,
    shared: Arc<ServerShared>,
    mailbox: Arc<McpNoticeMailbox>,
    http_idle_timeout_ms: Option<i64>,
    request_timeout: Duration,
) -> Result<(Client, Option<u32>), String> {
    match transport {
        Transport::Stdio(stdio) => {
            spawn_stdio_client(server, stdio, cwd, stderr_tail, shared, mailbox).await
        }
        Transport::Http(http) => {
            spawn_http_client(
                server,
                http,
                shared,
                mailbox,
                http_idle_timeout_ms,
                request_timeout,
            )
            .await
        }
    }
}

/// Spawn the child and complete initialize + notifications/initialized.
///
/// Returns the running client and the child's process-group id (see
/// [`ServerHandle::pgid`]).
async fn spawn_stdio_client(
    server: &str,
    stdio: &StdioTransport,
    cwd: &Path,
    stderr_tail: &Arc<Mutex<VecDeque<String>>>,
    shared: Arc<ServerShared>,
    mailbox: Arc<McpNoticeMailbox>,
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
    // The session id, granted explicitly: a collab server cannot attribute an
    // update to "mine" vs "a peer's" without it, so a scrubbed environment made
    // every own write look like someone else's. Lowest priority — a configured
    // `env` below still wins.
    if let Some((key, value)) = session_env() {
        command.env(key, value);
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

    let handler = KiClientHandler {
        info: client_info(),
        server: server.to_string(),
        shared,
        mailbox,
    };
    let client = handler
        .serve(transport)
        .await
        .map_err(|error| format!("initialize: {error}"))?;

    Ok((verify_protocol(client).await?, pgid))
}

/// Start one HTTP server through rmcp's streamable-HTTP worker.
///
/// The worker owns the protocol state machine (session id, protocol-version
/// header, standalone GET SSE, 404 re-init + replay, DELETE on exit); ki owns
/// the HTTP client, so the dependency stays on reqwest 0.12 and the response
/// cap applies.
async fn spawn_http_client(
    server: &str,
    http: &HttpTransport,
    shared: Arc<ServerShared>,
    mailbox: Arc<McpNoticeMailbox>,
    http_idle_timeout_ms: Option<i64>,
    request_timeout: Duration,
) -> Result<(Client, Option<u32>), String> {
    // Validate/build BEFORE logging the URL: a URL rejected for embedding
    // credentials must never reach stderr.
    let http_client = crate::http::KiHttpClient::new(
        &http.url,
        http_idle_timeout_ms,
        Some(request_timeout),
        Arc::clone(&shared.http_session_rotated),
    )?;
    eprintln!("[ki mcp] starting server {server} (http): {}", http.url);
    // Config entries carry `headers` verbatim (adapter parity). rmcp's config
    // wants a bearer token WITHOUT the "Bearer " prefix, so a conventional
    // Authorization header is split out; every other header passes through.
    let mut auth_header = None;
    let mut custom_headers = HashMap::new();
    for (name, value) in &http.headers {
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| format!("invalid HTTP header name {name:?}: {error}"))?;
        if header_name == http::header::AUTHORIZATION {
            if let Some(token) = value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
            {
                auth_header = Some(token.to_string());
                continue;
            }
        }
        let header_value = HeaderValue::from_str(value)
            .map_err(|error| format!("invalid value for HTTP header {name:?}: {error}"))?;
        custom_headers.insert(header_name, header_value);
    }
    let mut config = StreamableHttpClientTransportConfig::with_uri(http.url.as_str());
    if let Some(token) = auth_header {
        config = config.auth_header(token);
    }
    if !custom_headers.is_empty() {
        config = config.custom_headers(custom_headers);
    }
    let transport = StreamableHttpClientTransport::with_client(http_client, config);
    let handler = KiClientHandler {
        info: client_info(),
        server: server.to_string(),
        shared,
        mailbox,
    };
    let client = handler
        .serve(transport)
        .await
        .map_err(|error| format!("initialize: {error}"))?;
    Ok((verify_protocol(client).await?, None))
}

/// Refuse a connection whose negotiated protocol revision ki has not verified.
async fn verify_protocol(client: Client) -> Result<Client, String> {
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
    Ok(client)
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
async fn list_all_tools(
    server: &str,
    peer: &Peer<RoleClient>,
) -> Result<Vec<DiscoveredTool>, String> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors = std::collections::HashSet::new();
    for _ in 0..MAX_TOOL_PAGES {
        let mut params = PaginatedRequestParams::default();
        params.cursor = cursor.clone();
        let result = peer
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

/// The read tool's discovery text, and whether every listing behind it
/// completed.
struct ResourceDescription {
    text: String,
    /// False when a listing failed for a reason a retry could fix. A `-32601`
    /// is settled (the method will never exist) and does not count.
    complete: bool,
}

/// One paginated listing's outcome. `unimplemented` is kept apart from `error`
/// on purpose: `-32601` means the server advertises `resources` and does not
/// implement this method, which is the empty list — not a failure, and nothing
/// a retry can change.
struct ResourceListing {
    items: Vec<Value>,
    unimplemented: bool,
    error: Option<String>,
}

impl ResourceListing {
    fn done(items: Vec<Value>) -> Self {
        Self {
            items,
            unimplemented: false,
            error: None,
        }
    }

    fn unimplemented(items: Vec<Value>) -> Self {
        Self {
            items,
            unimplemented: true,
            error: None,
        }
    }

    fn failed(items: Vec<Value>, error: String) -> Self {
        Self {
            items,
            unimplemented: false,
            error: Some(error),
        }
    }
}

/// Why one page of a listing could not be read.
enum PageError {
    /// `-32601 Method not found` for the listing method itself.
    Unimplemented,
    /// Anything else, already labelled with its method.
    Failed(String),
}

/// Paginated `resources/list` + `resources/templates/list`, rendered as the
/// read tool's description. The list is the discoverability of the read tool —
/// bounded like every other server-provided text.
///
/// The two listings are assembled independently, because a server can
/// legitimately implement only half of the resource surface: walgit lists
/// `resources` and answers `-32601` for `resources/templates/list`. That answer
/// must cost the empty template list, not the resources it did list — the read
/// tool's discovery is the whole point of this call.
async fn describe_resources(server: &str, peer: &Peer<RoleClient>) -> ResourceDescription {
    let resources = list_all_resources(peer, "resources", "resources").await;
    let templates = list_all_resources(peer, "resourceTemplates", "resources/templates").await;

    let mut complete = true;
    let mut notes: Vec<String> = Vec::new();
    for (method, listing) in [
        ("resources/list", &resources),
        ("resources/templates/list", &templates),
    ] {
        if listing.unimplemented {
            // Reported once per method, named the way a reader greps for it.
            // Counting this as failure is what used to throw away the other
            // listing (and warn under the wrong method name).
            eprintln!(
                "[ki mcp] server {server}: {method} is not implemented (-32601); treating it as an empty list"
            );
        }
        if let Some(error) = &listing.error {
            complete = false;
            eprintln!(
                "[ki mcp] server {server}: {method} failed ({error}); the rest of the list is still usable, retrying at the next boundary"
            );
            notes.push(format!("({method} is currently unavailable: {error})"));
        }
    }

    let mut text = String::from("Read a resource from this server by uri. Available resources:");
    if resources.items.is_empty() && templates.items.is_empty() {
        text.push_str("\n(none currently listed)");
    }
    for item in &resources.items {
        text.push_str(&format!("\n- {}", render_resource_entry(item)));
    }
    if !templates.items.is_empty() {
        text.push_str("\nTemplates (fill the braces into a uri):");
        for item in &templates.items {
            text.push_str(&format!("\n- {}", render_resource_entry(item)));
        }
    }
    for note in notes {
        text.push_str(&format!("\n{note}"));
    }
    ResourceDescription {
        text: bound_resource_text(text),
        complete,
    }
}

/// One page batch of a paginated listing. `items_field` is the result array's
/// key ("resources" for both resource kinds). Values are read through
/// `serde_json`: rmcp's typed result structs churn across versions, while the
/// wire shape is part of the protocol.
async fn list_all_resources(
    peer: &Peer<RoleClient>,
    items_field: &str,
    method_label: &str,
) -> ResourceListing {
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors = std::collections::HashSet::new();
    for _ in 0..MAX_RESOURCE_PAGES {
        let mut params = PaginatedRequestParams::default();
        params.cursor = cursor.clone();
        let value = match resource_page(peer, method_label, params).await {
            Ok(value) => value,
            // Whatever earlier pages returned stays usable: partial discovery
            // beats none, and the caller reports the shape it is in.
            Err(PageError::Unimplemented) => return ResourceListing::unimplemented(items),
            Err(PageError::Failed(error)) => return ResourceListing::failed(items, error),
        };
        if let Some(batch) = value.get(items_field).and_then(Value::as_array) {
            items.extend(batch.iter().cloned());
        }
        match value.get("nextCursor").and_then(Value::as_str) {
            None => return ResourceListing::done(items),
            Some(next) => {
                if !seen_cursors.insert(next.to_string()) {
                    return ResourceListing::failed(
                        items,
                        format!("{method_label} repeated cursor {next:?}"),
                    );
                }
                cursor = Some(next.to_string());
            }
        }
    }
    ResourceListing::failed(
        items,
        format!("{method_label} exceeded {MAX_RESOURCE_PAGES} pages"),
    )
}

/// One page of a listing as wire JSON. The two methods differ only in the typed
/// call; both errors are classified the same way.
async fn resource_page(
    peer: &Peer<RoleClient>,
    method_label: &str,
    params: PaginatedRequestParams,
) -> Result<Value, PageError> {
    let encoded = if method_label == "resources" {
        match peer.list_resources(Some(params)).await {
            Ok(result) => serde_json::to_value(&result),
            Err(error) => return Err(classify_page_error(error)),
        }
    } else {
        match peer.list_resource_templates(Some(params)).await {
            Ok(result) => serde_json::to_value(&result),
            Err(error) => return Err(classify_page_error(error)),
        }
    };
    encoded.map_err(|error| PageError::Failed(format!("{method_label} encode: {error}")))
}

/// `-32601 Method not found` is the server saying it advertises `resources` and
/// implements nothing behind this method — an empty listing, not a failure.
fn classify_page_error(error: ServiceError) -> PageError {
    match error {
        ServiceError::McpError(data) if data.code == ErrorCode::METHOD_NOT_FOUND => {
            PageError::Unimplemented
        }
        other => PageError::Failed(format!("{other}")),
    }
}

fn render_resource_entry(item: &Value) -> String {
    let uri = item
        .get("uri")
        .or_else(|| item.get("uriTemplate"))
        .and_then(Value::as_str)
        .unwrap_or("(no uri)");
    let mime = item.get("mimeType").and_then(Value::as_str);
    let name = item.get("name").and_then(Value::as_str);
    let description = item.get("description").and_then(Value::as_str);
    let mut line = match mime {
        Some(mime) => format!("{uri} ({mime})"),
        None => uri.to_string(),
    };
    if let Some(name) = name {
        line.push_str(&format!(" — {name}"));
    }
    if let Some(description) = description {
        let description: String = description.chars().take(120).collect();
        line.push_str(&format!(": {description}"));
    }
    line
}

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

/// Why a `tools/call` (or `resources/read`) did not produce a result.
/// `Cancelled` is kept distinct so the adapter can report an abort (not a
/// server failure) back to the loop.
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

/// Run one request on this server and return the raw result.
///
/// Honours `signal` (the caller's cancellation token) at both await points and
/// applies the per-request timeout on the wire. A failure returns the server's
/// trailing stderr appended, so "it broke" comes with "and here is why".
async fn send_request(
    handle: &ServerHandle,
    request: ClientRequest,
    signal: &tokio_util::sync::CancellationToken,
) -> Result<ServerResult, CallError> {
    // `peer()` re-initializes a dead connection, which can take up to the
    // server timeout; honor the caller's cancellation there too, not only once
    // the request is on the wire.
    let peer = tokio::select! {
        biased;
        _ = signal.cancelled() => return Err(CallError::Cancelled),
        _ = handle.shutdown.cancelled() => return Err(CallError::Cancelled),
        result = handle.peer() => {
            result.map_err(|error| CallError::Failed(handle.decorate(error)))?
        }
    };
    let options = PeerRequestOptions::with_timeout(handle.tool_timeout());

    let handle_req = tokio::select! {
        biased;
        _ = signal.cancelled() => return Err(CallError::Cancelled),
        _ = handle.shutdown.cancelled() => return Err(CallError::Cancelled),
        result = peer.send_cancellable_request(request, options) => {
            result.map_err(|error| CallError::Failed(handle.decorate(error.to_string())))?
        }
    };
    // Once the request is on the wire, an abort must tell the server to stop
    // working on it (the MCP cancellation contract) rather than just walk away.
    let request_id = handle_req.id.clone();
    let raw: Result<ServerResult, rmcp::ServiceError> = tokio::select! {
        biased;
        _ = handle.shutdown.cancelled() => return Err(CallError::Cancelled),
        _ = signal.cancelled() => {
            if let Some(notification) = cancellation(request_id) {
                let _ = peer.send_notification(notification).await;
            }
            return Err(CallError::Cancelled);
        }
        result = handle_req.await_response() => result,
    };
    raw.map_err(|error| CallError::Failed(handle.decorate(error.to_string())))
}

/// Run a `tools/call` on this server and return the raw result JSON.
pub(crate) async fn call_tool(
    handle: &ServerHandle,
    bare_tool: &str,
    arguments: Option<serde_json::Map<String, Value>>,
    signal: &tokio_util::sync::CancellationToken,
) -> Result<Value, CallError> {
    let mut params = CallToolRequestParams::new(bare_tool.to_string());
    params.arguments = arguments;
    let result = send_request(
        handle,
        ClientRequest::CallToolRequest(CallToolRequest::new(params)),
        signal,
    )
    .await?;
    match result {
        ServerResult::CallToolResult(result) => serde_json::to_value(&result)
            .map_err(|error| CallError::Failed(handle.decorate(format!("result encode: {error}")))),
        _ => Err(CallError::Failed(
            handle.decorate("unexpected response type to tools/call".to_string()),
        )),
    }
}

/// Run a `resources/read` on this server and return the raw result JSON — the
/// on-demand read behind the light-notice semantic. Never auto-invoked.
pub(crate) async fn read_resource(
    handle: &ServerHandle,
    uri: &str,
    signal: &tokio_util::sync::CancellationToken,
) -> Result<Value, CallError> {
    let result = send_request(
        handle,
        ClientRequest::ReadResourceRequest(ReadResourceRequest::new(
            ReadResourceRequestParams::new(uri.to_string()),
        )),
        signal,
    )
    .await?;
    match result {
        ServerResult::ReadResourceResult(result) => serde_json::to_value(&result)
            .map_err(|error| CallError::Failed(handle.decorate(format!("result encode: {error}")))),
        _ => Err(CallError::Failed(handle.decorate(
            "unexpected response type to resources/read".to_string(),
        ))),
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
///
/// Reads bytes, not lines: `BufReader::lines` yields `Err(InvalidData)` on the
/// first non-UTF-8 byte and the loop then exits, leaving the pipe undrained
/// (the server blocks or gets EPIPE on its next write). Every chunk is decoded
/// lossily instead, and a newline-less line is capped rather than accumulated.
fn spawn_stderr_drain(stderr: tokio::process::ChildStderr, tail: Arc<Mutex<VecDeque<String>>>) {
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut reader = stderr;
        let mut chunk = [0u8; 8192];
        let mut pending: Vec<u8> = Vec::new();
        let push = |text: String| {
            let mut guard = tail.lock().unwrap();
            if guard.len() >= STDERR_TAIL_LINES {
                guard.pop_front();
            }
            guard.push_back(text);
        };
        // Truncate to a UTF-8 boundary so `String::truncate` cannot panic.
        let bounded = |text: &mut String| {
            let mut end = STDERR_LINE_MAX_BYTES.min(text.len());
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push_str("…[truncated]");
        };
        loop {
            let read = match reader.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            pending.extend_from_slice(&chunk[..read]);
            while let Some(pos) = pending.iter().position(|&byte| byte == b'\n') {
                let line: Vec<u8> = pending.drain(..=pos).collect();
                let mut text = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
                if text.ends_with('\r') {
                    text.pop();
                }
                if text.len() > STDERR_LINE_MAX_BYTES {
                    bounded(&mut text);
                }
                push(text);
            }
            if pending.len() > STDERR_LINE_MAX_BYTES {
                let mut text = String::from_utf8_lossy(&pending).into_owned();
                pending.clear();
                bounded(&mut text);
                push(text);
            }
        }
        if !pending.is_empty() {
            let mut text = String::from_utf8_lossy(&pending).into_owned();
            if text.ends_with('\r') {
                text.pop();
            }
            if text.len() > STDERR_LINE_MAX_BYTES {
                bounded(&mut text);
            }
            push(text);
        }
    });
}

/// `stdio: npx -y foo --bar` / `http: https://host/mcp` — the launch line of a
/// doctor row. Printed raw on purpose: a wrong path or a missing `cmd` wrapper
/// is visible here and nowhere else.
fn launch_of(config: &ServerConfig) -> String {
    match &config.transport {
        Transport::Stdio(stdio) => {
            let mut out = String::from("stdio: ");
            out.push_str(&stdio.command);
            for arg in &stdio.args {
                out.push(' ');
                out.push_str(arg);
            }
            out
        }
        Transport::Http(http) => format!("http: {}", http.url),
    }
}

fn split_stderr_tail(tail: &str) -> Vec<String> {
    tail.lines()
        .map(|line| line.trim_end().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

/// A long uri list is a settings file reading exercise, not a diagnosis: show
/// the first few and a count.
const LIST_CAP: usize = 6;
/// Hard bound on the whole report. It is written into the transcript, the
/// session file and the model's context, so a pathological config (many
/// servers, long uris) must not turn a diagnostic into a wall of text.
const REPORT_MAX_CHARS: usize = 16_000;

/// How much of a child's stderr reaches the report. stderr is the one input ki
/// does not control (a server may print a megabyte), and the report is written
/// into the transcript, the session file and the model's context — so keep only
/// the tail that explains a failure.
const STDERR_REPORT_LINES: usize = 5;
/// Per-line cap, in characters (not bytes: never split a char).
const STDERR_REPORT_CHARS: usize = 160;
/// The launch line is the most useful row of the report (a wrong path or a
/// missing `cmd` wrapper shows up here), so it gets more room than a stderr line.
const LAUNCH_REPORT_CHARS: usize = 240;
const REASON_REPORT_CHARS: usize = 400;

/// Make child-process text safe to *draw*.
///
/// The renderer passes `ESC` sequences through to the terminal (that is how ki
/// colors its own output), so a server that prints `\x1b[2J` clears the screen,
/// `\x1b[?1049l` drops the alternate screen out from under the TUI, and an OSC
/// title sequence rewrites the window title. A tab is just as bad in a subtler
/// way: the wrapper counts it as zero width but the terminal advances to the
/// next tab stop, so the row no longer matches the column math the frame was
/// laid out with. Everything below is text a user can read and nothing else.
fn report_stderr_lines(lines: &[String]) -> Vec<String> {
    let cleaned: Vec<String> = lines
        .iter()
        .map(|line| sanitize_for_report(line))
        .filter(|line| !line.is_empty())
        .collect();
    let skip = cleaned.len().saturating_sub(STDERR_REPORT_LINES);
    cleaned.into_iter().skip(skip).collect()
}

/// Strip everything from `text` that is not drawable text: escape sequences,
/// control characters, tabs. No length limit — callers that need one use
/// [`cap_chars`].
fn strip_escapes(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            skip_escape(&mut chars);
            continue;
        }
        if c.is_control() {
            // A tab advances to the next tab stop but counts as zero width in
            // ki's wrapper: one space keeps the columns honest.
            if c == '\t' && !out.ends_with(' ') {
                out.push(' ');
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Char-safe truncation (never splits a UTF-8 char, never splits a panic out of
/// `String::truncate`).
fn cap_chars(mut text: String, max: usize) -> String {
    if text.chars().count() <= max {
        return text;
    }
    // The byte offset of the first char past `max`: truncating there keeps
    // exactly `max` chars and can never split one.
    let cutoff = text
        .char_indices()
        .nth(max)
        .map_or(text.len(), |(index, _)| index);
    text.truncate(cutoff);
    text.push('…');
    text
}

/// Whole-report belt and braces: every line stripped, newlines preserved (the
/// report is multi-line, so the per-line helper cannot be applied to it as a
/// whole). Anything that reaches the transcript has already been sanitized field
/// by field; this catches a field someone adds later.
fn sanitize_report(report: &str) -> String {
    report
        .split('\n')
        .map(strip_escapes)
        .collect::<Vec<_>>()
        .join("\n")
}

/// One line of child-process text, reduced to what is safe to put in front of
/// the terminal.
fn sanitize_for_report(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    let mut visible = 0usize;
    let mut truncated = false;
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            skip_escape(&mut chars);
            continue;
        }
        if c.is_control() {
            // A tab advances to the next tab stop but counts as zero width in
            // ki's wrapper: one space keeps the columns honest.
            if c == '\t' && !out.ends_with(' ') {
                out.push(' ');
                visible += 1;
            }
            continue;
        }
        if visible == STDERR_REPORT_CHARS {
            truncated = true;
            break;
        }
        out.push(c);
        visible += 1;
    }
    // A tab at the start of a stack-trace line would otherwise become a stray
    // leading space in the report.
    let mut text = out.trim().to_string();
    if truncated {
        text.push('…');
    }
    text
}

/// Consume one escape sequence after its `ESC` intro: CSI, OSC/APC/DCS/PM, or a
/// two-character escape. Mirrors `kiqi_tui::ansi::escape_len` (this crate must
/// not depend on the TUI, but both have to agree on where a sequence ends).
fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        Some('[') => {
            // CSI: parameter bytes, then a final byte in 0x40..=0x7e.
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        }
        Some(']' | '_' | 'P' | '^') => {
            // OSC / APC / DCS / PM: terminated by BEL or ST (ESC \\).
            for c in chars.by_ref() {
                if c == '\x07' {
                    break;
                }
                if c == '\x1b' {
                    if matches!(chars.peek(), Some('\\')) {
                        chars.next();
                    }
                    break;
                }
            }
        }
        _ => {}
    }
}

/// The rendered doctor report. Pure: the only input is the rows, so the format
/// is testable without a live server.
pub fn render_doctor(rows: &[ServerDiagnostic], pending_notices: usize) -> String {
    if rows.is_empty() {
        return "MCP doctor — no servers configured.\n  put them under `mcpServers` in \
                ~/.ki/agent/settings.json (global) or <repo>/.ki/settings.json \
                (the project must be trusted)."
            .to_string();
    }
    let connected = rows
        .iter()
        .filter(|row| row.status == ServerStatus::Connected)
        .count();
    let disabled = rows
        .iter()
        .filter(|row| row.status == ServerStatus::Disabled)
        .count();
    let failed = rows.len() - connected - disabled;
    let mut out = format!(
        "MCP doctor — {} configured: {connected} connected, {disabled} disabled, \
         {failed} failed; {pending_notices} notice(s) pending",
        rows.len()
    );
    for row in rows {
        // `name`, `launch` and `reason` can all carry config or OS text; the
        // launch line is echoed verbatim on purpose (a wrong path is the bug),
        // but never raw control bytes.
        out.push_str(&format!(
            "\n\n{} [{}]",
            cap_chars(strip_escapes(&row.name), 60),
            cap_chars(strip_escapes(&row.launch), LAUNCH_REPORT_CHARS)
        ));
        out.push_str(&format!("\n  status:    {}", row.status.as_str()));
        if let Some(reason) = &row.reason {
            out.push_str(&format!(
                "\n  reason:    {}",
                cap_chars(strip_escapes(reason), REASON_REPORT_CHARS)
            ));
        }
        if row.status == ServerStatus::Connected {
            out.push_str(&format!("\n  tools:     {}", row.tools.len()));
            if !row.tools.is_empty() {
                out.push_str("\n             ");
                out.push_str(&join_capped(&row.tools, 12));
            }
            out.push_str(&format!(
                "\n  resources: {}",
                match (row.advertises_resources, row.supports_subscribe) {
                    (true, true) => "advertised, subscribe-capable".to_string(),
                    (true, false) => "advertised, no subscribe capability".to_string(),
                    (false, _) => "not advertised".to_string(),
                }
            ));
            out.push_str(&format!(
                "\n  subscribe: {} configured, {} live{}",
                row.configured_subscribe.len(),
                row.live_subscriptions.len(),
                render_live(&row.live_subscriptions)
            ));
        }
        // Subscription and wake policy only mean something for a server that is
        // actually running; on a failed row they would be noise in the way of
        // the `reason`.
        if row.status == ServerStatus::Connected {
            out.push_str(&format!(
                "\n  autoRead:  {}",
                if row.auto_read.is_empty() {
                    "(none)".to_string()
                } else {
                    join_capped(&report_list(&row.auto_read), LIST_CAP)
                }
            ));
            out.push_str(&format!(
                "\n  signals:   wake={} minInterval={}s maxRunsPerHour={} maxConsecutive={}",
                row.signals.wake.as_str(),
                row.signals.min_interval.as_secs(),
                row.signals.max_runs_per_hour,
                row.signals.max_consecutive_runs
            ));
        }
        if !row.unknown_keys.is_empty() {
            out.push_str(&format!(
                "\n  config:    unknown key(s): {}",
                join_capped(&report_list(&row.unknown_keys), LIST_CAP)
            ));
        }
        // `stderr_tail` is raw child output; sanitize here so every path into
        // the transcript (this one included) gets text and nothing else.
        let stderr = report_stderr_lines(&row.stderr_tail);
        if !stderr.is_empty() {
            out.push_str("\n  stderr:");
            for line in &stderr {
                out.push_str(&format!("\n             | {line}"));
            }
        }
        for hint in doctor_hints(row) {
            out.push_str(&format!("\n  hint:      {hint}"));
        }
    }
    // The one thing a doctor must not omit: what a connected server still does
    // not have.
    let unrunnable_hint = if connected == 0 {
        Some(
            "no server is connected, so no `{server}__*` tool exists this session; \
             fix a `reason:` above and run /mcp-reload",
        )
    } else {
        None
    };
    if let Some(hint) = unrunnable_hint {
        out.push_str(&format!("\n\nhint: {hint}"));
    }
    truncate_report(sanitize_report(&out))
}

fn truncate_report(out: String) -> String {
    if out.chars().count() <= REPORT_MAX_CHARS {
        return out;
    }
    let mut kept = cap_chars(out, REPORT_MAX_CHARS);
    kept.push_str("\n…(report truncated)");
    kept
}

fn render_live(live: &[(String, &'static str)]) -> String {
    if live.is_empty() {
        return String::new();
    }
    let mut out = String::from(": ");
    for (index, (uri, origin)) in live.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        // Uris come from a server or from ki's settings; a newline in one would
        // forge a line of this report (see `sanitize_report`).
        out.push_str(&format!("{} ({origin})", strip_escapes(uri)));
    }
    out
}

/// Report-ready copy of a list of uris or keys. Escapes and control characters
/// are removed, so an entry cannot span a line and pose as a `hint:`.
fn report_list(items: &[String]) -> Vec<String> {
    items.iter().map(|item| strip_escapes(item)).collect()
}

fn join_capped(items: &[String], cap: usize) -> String {
    if items.len() <= cap {
        return items.join(", ");
    }
    format!(
        "{}, … ({} more)",
        items[..cap].join(", "),
        items.len() - cap
    )
}

/// What the report should tell the user to do next. Derived from the recorded
/// reason rather than from the platform, so the same text is produced everywhere
/// (and asserted in tests).
fn doctor_hints(row: &ServerDiagnostic) -> Vec<String> {
    let mut hints = Vec::new();
    if !row.unknown_keys.is_empty() {
        hints.push(format!(
            "fix the unknown key(s) — ki ignores them, so a mistyped key looks \
             exactly like a correct one: {}",
            join_capped(&report_list(&row.unknown_keys), LIST_CAP)
        ));
    }
    if !row.configured_subscribe.is_empty() && row.status == ServerStatus::Connected {
        let missing: Vec<&String> = row
            .configured_subscribe
            .iter()
            .filter(|uri| !row.live_subscriptions.iter().any(|(live, _)| live == *uri))
            .collect();
        if !missing.is_empty() {
            hints.push(format!(
                "configured but not subscribed: {} — the server rejected \
                 `resources/subscribe` (see the reload issue list)",
                join_capped(
                    &missing
                        .iter()
                        .map(|uri| uri.to_string())
                        .collect::<Vec<_>>(),
                    LIST_CAP
                )
            ));
        }
        if !row.supports_subscribe && !row.configured_subscribe.is_empty() {
            hints.push(
                "this server does not advertise `resources.subscribe`; a \
                 `subscribe` list on it can never be honored"
                    .to_string(),
            );
        }
    }
    if let Some(reason) = &row.reason {
        if reason.contains("spawn") {
            hints.push(
                "the process never started: check `command`/`args` and that the \
                 binary is on ki's PATH. ki spawns stdio servers without a shell, \
                 so on Windows a `.cmd`/`.bat` shim (npx, uvx, pnpm) needs \
                 command `cmd` with args ['/d', '/s', '/c', …]"
                    .to_string(),
            );
            hints.push(
                "a stdio server inherits only PATH/HOME/TMPDIR/TERM/USER/SHELL/\
                 LANG/LC_* — everything else it needs must be listed in its \
                 `env` block"
                    .to_string(),
            );
        }
        if reason.contains("timed out") {
            hints.push(
                "the connection opened but initialize/tools-list did not finish: \
                 raise `timeoutSeconds`, or run the command by hand to see its \
                 own error"
                    .to_string(),
            );
        }
    }
    hints
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected_row(name: &str) -> ServerDiagnostic {
        ServerDiagnostic {
            name: name.to_string(),
            launch: "stdio: walgit mcp --repo /repo".to_string(),
            status: ServerStatus::Connected,
            tools: vec![
                "walgit__repo_list".to_string(),
                "walgit__watch_resource".to_string(),
            ],
            advertises_resources: true,
            supports_subscribe: true,
            configured_subscribe: vec!["walgit://collab/board/x".to_string()],
            live_subscriptions: vec![("walgit://collab/board/x".to_string(), "config")],
            auto_read: vec!["walgit://collab/board/x".to_string()],
            signals: crate::config::SignalsConfig {
                wake: crate::config::WakeMode::Auto,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// The report has to answer the two questions a user actually has: did it
    /// connect, and what is still missing now that it did.
    #[test]
    fn doctor_report_states_status_subscriptions_and_hints() {
        let mut row = connected_row("walgit");
        row.unknown_keys = vec!["subscribeUri (did you mean `subscribe`?)".to_string()];
        let report = render_doctor(&[row], 3);

        assert!(
            report.starts_with(
                "MCP doctor — 1 configured: 1 connected, 0 disabled, 0 failed; 3 notice(s) pending"
            ),
            "{report}"
        );
        assert!(
            report.contains("walgit [stdio: walgit mcp --repo /repo]"),
            "{report}"
        );
        assert!(report.contains("  status:    connected"), "{report}");
        assert!(report.contains("  tools:     2"), "{report}");
        assert!(
            report.contains("  subscribe: 1 configured, 1 live: walgit://collab/board/x (config)"),
            "{report}"
        );
        assert!(
            report.contains("  signals:   wake=auto minInterval=30s"),
            "{report}"
        );
        assert!(
            report.contains("unknown key(s): subscribeUri (did you mean `subscribe`?)"),
            "{report}"
        );
        assert!(report.contains("fix the unknown key(s)"), "{report}");
        assert!(
            !report.contains("configured but not subscribed"),
            "a live subscription must not be reported as missing: {report}"
        );
    }

    /// A configured-but-unsubscribed uri is the quiet failure this command
    /// exists for: the settings look right, nothing arrives, nothing warns.
    #[test]
    fn doctor_report_flags_a_subscription_that_never_went_live() {
        let mut row = connected_row("walgit");
        row.configured_subscribe
            .push("walgit://not-there".to_string());
        let report = render_doctor(&[row], 0);
        assert!(
            report.contains("configured but not subscribed: walgit://not-there"),
            "{report}"
        );
    }

    /// A failed spawn is the Windows case: `.cmd` shims and an almost-empty
    /// inherited environment.
    #[test]
    fn doctor_report_explains_a_spawn_failure() {
        let row = ServerDiagnostic {
            name: "docs".to_string(),
            launch: "stdio: npx -y docs-mcp".to_string(),
            status: ServerStatus::Failed,
            reason: Some("spawn: No such file or directory (os error 2)".to_string()),
            stderr_tail: vec!["npm ERR! could not determine executable".to_string()],
            ..Default::default()
        };
        let report = render_doctor(&[row], 0);
        assert!(report.contains("  status:    failed"), "{report}");
        assert!(report.contains("  reason:    spawn:"), "{report}");
        assert!(report.contains("             | npm ERR!"), "{report}");
        assert!(
            report.contains("ki spawns stdio servers without a shell"),
            "{report}"
        );
        assert!(report.contains("only PATH/HOME/TMPDIR"), "{report}");
        assert!(
            report.contains("no server is connected, so no `{server}__*` tool exists this session"),
            "{report}"
        );
    }

    /// stderr from a child process is untrusted *for display*: the renderer
    /// passes escape sequences straight to the terminal, so an unmodified tail
    /// can clear the screen, drop the alternate screen out from under the TUI,
    /// rewrite the window title, or desync the columns with a tab.
    #[test]
    fn doctor_report_sanitizes_server_stderr() {
        let mut row = ServerDiagnostic {
            name: "noisy".to_string(),
            launch: "stdio: noisy".to_string(),
            status: ServerStatus::Failed,
            reason: Some("exit status 1".to_string()),
            ..Default::default()
        };
        row.stderr_tail = vec![
            "\u{1b}[2J\u{1b}]0;hijacked\u{7}\ttabbed \u{1b}[31mred\u{1b}[0m".to_string(),
            "   ".to_string(),
            "a".repeat(STDERR_REPORT_CHARS * 3),
        ];
        let report = render_doctor(&[row], 0);

        assert!(report.contains("| tabbed red"), "{report}");
        for forbidden in ['\u{1b}', '\u{7}', '\t', '\r'] {
            assert!(
                !report.contains(forbidden),
                "{forbidden:?} leaked into the report: {report}"
            );
        }
        assert!(
            !report.contains("hijacked"),
            "an OSC payload is not text: {report}"
        );
        // The over-long line is cut on a char boundary and marked.
        let capped = report
            .lines()
            .find(|line| line.contains("| aaaa"))
            .expect("the long line is still shown");
        assert_eq!(
            capped.chars().filter(|c| *c == 'a').count(),
            STDERR_REPORT_CHARS
        );
        assert!(capped.ends_with('…'), "{capped}");
        // A blank line does not become a `|` row.
        assert!(!report.contains("| \n"), "{report}");
    }

    #[test]
    fn report_truncation_keeps_char_boundaries() {
        // "医" is three bytes: a byte-sliced truncation would panic here.
        let out = truncate_report("医".repeat(REPORT_MAX_CHARS + 10));
        assert!(out.ends_with("…(report truncated)"), "{out}");
        assert!(out.chars().count() <= REPORT_MAX_CHARS + 32);
    }

    #[test]
    fn doctor_report_without_servers_says_where_to_configure() {
        let report = render_doctor(&[], 0);
        assert!(report.contains("no servers configured"), "{report}");
        assert!(report.contains("mcpServers"), "{report}");
    }

    /// `/mcp-reload` swaps the live set in place: the manager keeps serving the
    /// same mailbox and reports what the new config produced.
    #[tokio::test]
    async fn reload_swaps_the_server_set_and_reports_issues() {
        let (manager, issues) = McpManager::start(&[], Path::new(".")).await;
        assert!(issues.is_empty());

        let (servers, parse_issues) = crate::config::parse_servers_with_defaults(
            Some(&serde_json::json!({
                "ghost": {"transport": "stdio", "command": "true", "enabled": false}
            })),
            &crate::config::SignalsDefaults::default(),
        );
        // An old file still names its transport; the key is ignored, and the
        // new reporter says so instead of letting the entry look clean.
        assert_eq!(
            parse_issues,
            vec!["mcpServers.ghost: unknown key transport".to_string()]
        );

        let issues = manager.reload(&servers, Path::new("."), None).await;
        assert!(
            issues.iter().any(|issue| issue.contains("disabled")),
            "{issues:?}"
        );
        // A disabled server contributes no tools, and the policy records a
        // boundary replace consults are reset for the next merge.
        assert!(manager.tool_adapters().is_empty());
        assert!(manager.injected_tools().is_none());
        assert!(manager
            .configured_names
            .read()
            .unwrap()
            .iter()
            .any(|name| name == "ghost"));

        // The diagnostic follows the reload, and it carries what the parse
        // collected: a swap that keeps the count still shows up by name.
        let rows = manager.doctor();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "ghost");
        assert_eq!(rows[0].status, ServerStatus::Disabled);
        assert_eq!(rows[0].unknown_keys, vec!["transport".to_string()]);
    }

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

    #[test]
    fn resource_entries_render_uri_mime_and_description() {
        let line = render_resource_entry(&serde_json::json!({
            "uri": "file:///a.md", "mimeType": "text/markdown", "name": "a",
            "description": "first doc"
        }));
        assert_eq!(line, "file:///a.md (text/markdown) — a: first doc");

        let template = render_resource_entry(&serde_json::json!({
            "uriTemplate": "file:///x/{p}", "name": "tpl"
        }));
        assert_eq!(template, "file:///x/{p} — tpl");
    }

    #[test]
    fn only_method_not_found_is_read_as_an_empty_listing() {
        // `-32601` is the server saying it implements nothing behind this
        // method: an empty listing, not a failure.
        let unimplemented =
            classify_page_error(ServiceError::McpError(rmcp::model::ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                "method not found: resources/templates/list",
                None,
            )));
        assert!(matches!(unimplemented, PageError::Unimplemented));

        // Every other code is a real failure the boundary retries.
        let failed = classify_page_error(ServiceError::McpError(rmcp::model::ErrorData::new(
            ErrorCode::INTERNAL_ERROR,
            "boom",
            None,
        )));
        assert!(matches!(failed, PageError::Failed(ref message) if message.contains("boom")));

        // A transport-level error is never an unimplemented method.
        assert!(matches!(
            classify_page_error(ServiceError::TransportClosed),
            PageError::Failed(_)
        ));
    }

    #[test]
    fn oversized_text_is_truncated_on_a_char_boundary_with_a_marker() {
        let ascii: String = "a".repeat(MAX_RESOURCE_TEXT + 10);
        let bounded = bound_resource_text(ascii);
        assert!(bounded.len() <= MAX_RESOURCE_TEXT + "\n[truncated]".len());
        assert!(bounded.ends_with("\n[truncated]"));

        let multibyte = "漢".repeat(MAX_RESOURCE_TEXT / 3 + 5);
        let bounded = bound_resource_text(multibyte);
        // The truncated text must still be valid to the last char boundary.
        assert!(bounded.ends_with("\n[truncated]"));
        assert!(String::from_utf8(bounded.into_bytes()).is_ok());
    }
}

/// The sessions a `resources/updated` notification says authored the changed
/// entries, from `_meta.sessions` (`_meta: {"sessions": [...], "self_session":
/// "<sid>"}`).
///
/// The *client* decides what counts as its own echo, comparing this list against
/// the session it is serving at decision time — not against the server's
/// `self_session`, which is a spawn-time snapshot. An empty list means "no
/// provenance" (an older server, or entries written before sessions were
/// recorded) and never counts as an echo.
fn authored_sessions_from_meta(meta: &rmcp::model::Meta) -> Vec<String> {
    meta.0
        .get("sessions")
        .and_then(serde_json::Value::as_array)
        .map(|sessions| {
            sessions
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod provenance_tests {
    use super::authored_sessions_from_meta;

    fn meta(value: serde_json::Value) -> rmcp::model::Meta {
        rmcp::model::Meta(value.as_object().expect("object").clone())
    }

    #[test]
    fn the_client_learns_the_raw_authored_sessions() {
        assert_eq!(
            authored_sessions_from_meta(&meta(serde_json::json!({
                "sessions": ["s1"], "self_session": "s1"
            }))),
            vec!["s1".to_string()]
        );
        // The peer's session must survive into the notice: dropping it here
        // would make a mixed batch look like a pure echo.
        assert_eq!(
            authored_sessions_from_meta(&meta(serde_json::json!({
                "sessions": ["s1", "s2"], "self_session": "s1"
            }))),
            vec!["s1".to_string(), "s2".to_string()]
        );
        // `self_session` alone is a spawn-time snapshot and is not read here;
        // the decision compares against the session ki serves at that moment.
        assert_eq!(
            authored_sessions_from_meta(&meta(serde_json::json!({
                "self_session": "s1"
            }))),
            Vec::<String>::new()
        );
        assert_eq!(
            authored_sessions_from_meta(&meta(serde_json::json!({}))),
            Vec::<String>::new(),
            "an older server without provenance must never look like an echo"
        );
        // Actor-only metadata is not session-scoped: the same principal can own
        // many sessions, so it cannot distinguish our write from a peer's.
        assert_eq!(
            authored_sessions_from_meta(&meta(serde_json::json!({
                "actors": ["me"], "self": "me"
            }))),
            Vec::<String>::new()
        );
    }
}
