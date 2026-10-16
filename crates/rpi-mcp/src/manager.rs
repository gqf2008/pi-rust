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
//!
//! ## Resources and subscriptions (batch 2)
//!
//! A server that advertises resources contributes one read tool,
//! `{server}__read_resource`; reads happen **on demand** — the product
//! semantic for `notifications/resources/updated` is a light notice (server +
//! uri) into a shared [`McpNoticeMailbox`], never an automatic read of the
//! content. The adapter's injection (auto-read + turn-end batch) is
//! deliberately not ported; its delivery-lifetime and restore semantics are
//! (see [`ServerHandle::subscribe_uri`], [`ServerHandle::unsubscribe_uri`] and
//! [`restore_subscriptions`]).
//!
//! Subscription state lives per server *alongside* the connection
//! (`ServerShared`), not inside it: a `peer()` re-init builds a fresh handler
//! around the same state, which is how subscriptions survive a reconnect. A
//! config-identity change cannot replay subscriptions onto a new server here —
//! each [`ServerHandle`] is constructed from exactly one config, so a changed
//! identity is a new handle with a fresh (empty) subscription set.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CancelledNotification, ClientCapabilities, ClientInfo,
    ClientNotification, ClientRequest, Implementation, PaginatedRequestParams, ProtocolVersion,
    ReadResourceRequest, ReadResourceRequestParams, RequestId, ResourceUpdatedNotificationParam,
    ServerResult, SubscribeRequestParams, UnsubscribeRequestParams,
};
use rmcp::service::{NotificationContext, PeerRequestOptions, RunningService};
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::{Peer, RoleClient, ServiceError, ServiceExt};
use rpi_agent::AgentTool;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

use crate::config::{ServerConfig, StdioTransport, Transport};
use crate::notice::McpNoticeMailbox;
use crate::tool::{McpResourceToolAdapter, McpToolAdapter};

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

/// The native tool name separator. Part of the public surface (`{server}__{tool}`).
pub const TOOL_NAME_SEPARATOR: &str = "__";

/// The read tool's suffix: `{server}__read_resource`.
pub const READ_RESOURCE_SUFFIX: &str = "read_resource";

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

/// State that OUTLIVES one connection: the subscription map and the
/// `list_changed` stale flags. A reconnect builds a fresh handler around the
/// same `Arc`s — that is how subscriptions survive a `peer()` re-init (the
/// adapter's entry owned them, not the session).
#[derive(Default)]
struct ServerShared {
    subscriptions: Mutex<BTreeMap<String, SubscriptionToken>>,
    stale: StaleFlags,
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
        _context: NotificationContext<RoleClient>,
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
        self.mailbox.push(&self.server, &params.uri);
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
    shared: Arc<ServerShared>,
    mailbox: Arc<McpNoticeMailbox>,
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
    async fn connect(
        config: ServerConfig,
        cwd: &Path,
        mailbox: Arc<McpNoticeMailbox>,
    ) -> Result<Self, String> {
        let stdio = match &config.transport {
            Transport::Stdio(stdio) => stdio.clone(),
            Transport::Http(_) => return Err("http transport is not supported yet".to_string()),
        };
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        let shared = Arc::new(ServerShared::default());
        // The connect budget covers the whole handshake + listing, so a server
        // that accepts a connection then stalls cannot hold up the session build.
        let (client, pgid, tools, advertises_resources, description) = tokio::time::timeout(
            config.timeout,
            connect_and_list(
                &config.name,
                &stdio,
                cwd,
                &stderr_tail,
                Arc::clone(&shared),
                Arc::clone(&mailbox),
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
            shared,
            mailbox,
            stderr_tail,
            stopped: AtomicBool::new(false),
            pgid: AtomicU32::new(pgid.unwrap_or(0)),
        })
    }

    pub(crate) fn server_name(&self) -> &str {
        &self.config.name
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
        if let Some(client) = guard.as_ref() {
            // `is_transport_closed` flips as soon as the service task ends
            // (child died / stdout EOF), which is the real liveness signal —
            // `RunningService::is_closed` only tracks explicit shutdown.
            if !client.peer().is_transport_closed() {
                return Ok((client.peer().clone(), false));
            }
        }
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
            spawn_client(
                &self.config.name,
                &stdio,
                &self.cwd,
                &self.stderr_tail,
                Arc::clone(&self.shared),
                Arc::clone(&self.mailbox),
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
        // A reconnect that flips the resources capability updates the read
        // tool's presence at the next boundary (a list_changed may never come).
        let advertises = fresh
            .peer()
            .peer_info()
            .is_some_and(|info| info.capabilities.resources.is_some());
        if self
            .advertises_resources
            .swap(advertises, Ordering::Release)
            != advertises
        {
            self.shared.stale.resources.store(true, Ordering::Release);
        }
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
    fn rotate_lifetime(&self, uri: &str) -> SubscriptionToken {
        let token = Arc::new(AtomicU64::new(0));
        // Hold the map guard across the purge: the update path checks the
        // token and pushes under this same lock, so a notice accepted by the
        // FRESH lifetime can never be destroyed by its own rotation (upstream
        // `add_subscription` purges under the subscriptions lock for exactly
        // this reason).
        let mut map = self.shared.subscriptions.lock().unwrap();
        map.insert(uri.to_string(), token.clone());
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
                (Some(current), Some(expected)) => Arc::ptr_eq(current, expected),
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

    /// Host-level `resources/subscribe` (no model verb — the host and the
    /// config drive it). Ported semantics:
    /// * already-subscribed on a live transport sends **no** second RPC, so a
    ///   duplicate-rejecting server never sees one;
    /// * the token ALWAYS rotates + the uri's queue purges, even on that no-op
    ///   path — a fresh delivery lifetime, so stale content can never deliver;
    /// * the mark is installed BEFORE the RPC under the connection lock, so an
    ///   update pushed during the handshake is accepted;
    /// * a server rejection rolls the token back with an expected-token guard
    ///   and propagates the error.
    pub(crate) async fn subscribe_uri(&self, uri: &str) -> Result<(), String> {
        if self.stopped.load(Ordering::Acquire) {
            return Err("manager is shutting down".to_string());
        }
        let mut guard = self.client.lock().await;
        let (peer, fresh) = self.ensure_live(&mut guard, Some(uri)).await?;
        let already = !fresh && self.shared.subscriptions.lock().unwrap().contains_key(uri);
        // Registered before the RPC so early notifications are accepted, while
        // the connection lock keeps subscribe/unsubscribe ordered.
        let token = self.rotate_lifetime(uri);
        if already {
            return Ok(());
        }
        let request = tokio::time::timeout(
            self.config.timeout,
            peer.subscribe(SubscribeRequestParams::new(uri.to_string())),
        )
        .await;
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
    /// re-subscribable. If the connection could not even be established, only
    /// the lifetime we observed is removed (expected-token guard).
    pub(crate) async fn unsubscribe_uri(&self, uri: &str) -> Result<(), String> {
        if self.stopped.load(Ordering::Acquire) {
            return Err("manager is shutting down".to_string());
        }
        let previous = self.shared.subscriptions.lock().unwrap().get(uri).cloned();
        let mut guard = self.client.lock().await;
        let established = match self.ensure_live(&mut guard, Some(uri)).await {
            Ok((peer, _)) => {
                self.remove_lifetime(uri, None);
                let result = tokio::time::timeout(
                    self.config.timeout,
                    peer.unsubscribe(UnsubscribeRequestParams::new(uri.to_string())),
                )
                .await;
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
            if let Some(previous) = previous {
                self.remove_lifetime(uri, Some(&previous));
            }
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
                    describe_resources(&peer).await
                })
                .await;
                match result {
                    Ok(Ok(description)) => {
                        self.replace_resource_description(description);
                        self.shared.stale.resources.store(false, Ordering::Release);
                        changed = true;
                    }
                    Ok(Err(error)) => eprintln!(
                        "[ki mcp] server {name}: resources/list refresh failed ({error}); will retry at the next boundary"
                    ),
                    Err(_) => eprintln!(
                        "[ki mcp] server {name}: resources/list refresh timed out; will retry at the next boundary"
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
/// the error says the next call will retry. Server actively rejects: the whole
/// set is cleared — our state and the server's can only diverge further.
async fn restore_subscriptions(
    peer: &Peer<RoleClient>,
    server: &str,
    subscriptions: &Mutex<BTreeMap<String, SubscriptionToken>>,
    mailbox: &McpNoticeMailbox,
    skip_uri: Option<&str>,
    budget: Duration,
) -> Result<(), String> {
    let uris = {
        let mut map = subscriptions.lock().unwrap();
        let mut uris = Vec::new();
        for (uri, token) in map.iter_mut() {
            // An explicit subscribe/unsubscribe handles this uri once the
            // connection is ready; do not issue a duplicate RPC.
            if skip_uri == Some(uri.as_str()) {
                continue;
            }
            *token = Arc::new(AtomicU64::new(0));
            uris.push(uri.clone());
        }
        drop(map);
        mailbox.purge_server(server);
        uris.sort();
        uris
    };
    for uri in uris {
        let outcome = tokio::time::timeout(
            budget,
            peer.subscribe(SubscribeRequestParams::new(uri.clone())),
        )
        .await;
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(ServiceError::McpError(error))) => {
                subscriptions.lock().unwrap().clear();
                mailbox.purge_server(server);
                return Err(format!(
                    "resources/subscribe {uri} was rejected ({error}); subscriptions cleared, subscribe again"
                ));
            }
            Ok(Err(error)) => {
                return Err(format!(
                    "resource subscription restoration interrupted at {uri} ({error}); the next call will retry"
                ));
            }
            Err(_) => {
                return Err(format!(
                    "resource subscription restoration interrupted at {uri} (timed out after {}s); the next call will retry",
                    budget.as_secs()
                ));
            }
        }
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
pub struct McpManager {
    servers: Vec<Arc<ServerHandle>>,
    mailbox: Arc<McpNoticeMailbox>,
    /// Every configured server name (connected, skipped or failed), so the
    /// rpi-cli tool-slice replace can recognize every MCP-contributed name —
    /// including a server that went away after contributing tools.
    configured_names: Vec<String>,
    /// The MCP tool names that passed the session's tool policy
    /// (`--tools`/`--exclude-tools`/`--no-tools`), recorded by rpi-cli's merge.
    /// A boundary refresh consults this so it can never resurrect a name the
    /// policy excluded at build. `None` until the first merge.
    allowed_tools: Mutex<Option<HashSet<String>>>,
    /// The MCP tool names actually present in the host registry, recorded by
    /// rpi-cli at every merge/boundary replace. The replace removes exactly
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
            servers: Vec::new(),
            mailbox: Arc::new(McpNoticeMailbox::new()),
            configured_names: Vec::new(),
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
    /// Connect every enabled, supported server and enumerate its tools.
    ///
    /// Returns the manager plus one issue string per server that was skipped or
    /// partially degraded (disabled, HTTP, connect/handshake failure, a malformed
    /// tool, a failed config-driven subscribe). The caller prints them — nothing
    /// is dropped silently.
    pub async fn start(servers: &[ServerConfig], cwd: &Path) -> (Self, Vec<String>) {
        let mailbox = Arc::new(McpNoticeMailbox::new());
        let mut configured_names = Vec::new();
        let mut handles = Vec::new();
        let mut issues = Vec::new();
        for config in servers {
            configured_names.push(config.name.clone());
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
            match ServerHandle::connect(config.clone(), cwd, Arc::clone(&mailbox)).await {
                Ok(handle) => {
                    let handle = Arc::new(handle);
                    // Config-driven auto-subscribe, after the handshake. One
                    // failed uri is loud but never poisons the server.
                    for uri in &handle.config.subscribe {
                        if let Err(error) = handle.subscribe_uri(uri).await {
                            issues.push(format!(
                                "mcp server {}: subscribing {uri}: {error} — subscription skipped",
                                config.name
                            ));
                        }
                    }
                    handles.push(handle);
                }
                Err(reason) => {
                    issues.push(format!("mcp server {}: {reason} — skipped", config.name))
                }
            }
        }
        (
            Self {
                servers: handles,
                mailbox,
                configured_names,
                allowed_tools: Mutex::new(None),
                injected_tools: Mutex::new(None),
            },
            issues,
        )
    }

    /// One native tool adapter per discovered tool, across every live server —
    /// plus one `{server}__read_resource` read tool per server advertising
    /// resources. A tools-only server adds zero tools beyond its tools.
    pub fn tool_adapters(&self) -> Vec<Arc<dyn AgentTool>> {
        let mut adapters: Vec<Arc<dyn AgentTool>> = Vec::new();
        for server in &self.servers {
            for tool in server.tools() {
                adapters.push(Arc::new(McpToolAdapter::new(Arc::clone(server), &tool)));
            }
            if server.advertises_resources() {
                let qualified = format!(
                    "{}{TOOL_NAME_SEPARATOR}{READ_RESOURCE_SUFFIX}",
                    server.server_name()
                );
                if qualified.len() > MAX_TOOL_NAME_LEN {
                    eprintln!(
                        "[ki mcp] server {}: skipping read tool {qualified:?}: exceeds {MAX_TOOL_NAME_LEN} chars",
                        server.server_name()
                    );
                } else {
                    adapters.push(Arc::new(McpResourceToolAdapter::new(
                        Arc::clone(server),
                        server.resource_description(),
                    )));
                }
            }
        }
        adapters
    }

    /// Record the MCP tool names that passed the session's tool policy. rpi-cli
    /// calls this on every merge; [`McpManager::tool_allowed`] then gates the
    /// boundary refresh.
    pub fn record_allowed_tools(&self, names: HashSet<String>) {
        *self.allowed_tools.lock().unwrap() = Some(names);
    }

    /// Record the MCP tool names actually present in the host registry. rpi-cli
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
        match &*self.allowed_tools.lock().unwrap() {
            Some(set) => set.contains(name),
            None => true,
        }
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
        self.configured_names.iter().any(|server| server == prefix)
    }

    /// Drain the resource-update notice mailbox. Called at session boundaries
    /// (never mid-run); empty between drains.
    pub fn take_notices(&self) -> crate::notice::McpNoticeBatch {
        self.mailbox.take()
    }

    /// How many notices are currently queued, WITHOUT draining them (drain
    /// with [`McpManager::take_notices`]). Lets a caller wait for a push to
    /// have landed before acting on the mailbox.
    pub fn pending_notice_count(&self) -> usize {
        self.mailbox.pending_count()
    }

    /// Subscribe one server's resource (host/config-driven; no model verb).
    pub async fn subscribe(&self, server: &str, uri: &str) -> Result<(), String> {
        let handle = self
            .servers
            .iter()
            .find(|handle| handle.server_name() == server)
            .ok_or_else(|| format!("mcp server {server} is not running"))?;
        handle.subscribe_uri(uri).await
    }

    /// Unsubscribe one server's resource. Always reports unsubscribed, even
    /// when the remote unsubscribe failed (the local delivery stopped either
    /// way; a rejected unsubscribe retires the transport instead).
    pub async fn unsubscribe(&self, server: &str, uri: &str) -> Result<(), String> {
        let handle = self
            .servers
            .iter()
            .find(|handle| handle.server_name() == server)
            .ok_or_else(|| format!("mcp server {server} is not running"))?;
        handle.unsubscribe_uri(uri).await
    }

    /// Re-list tools / resources for servers marked stale by `list_changed`.
    /// Returns whether anything changed.
    pub async fn refresh_if_stale(&self) -> bool {
        let mut changed = false;
        for server in &self.servers {
            changed |= server.refresh_if_stale().await;
        }
        changed
    }

    /// Close every server and reap its process tree. Idempotent. Also clears
    /// every subscription (and queued notice) — a torn-down connection must
    /// never leave deliverable state behind.
    pub async fn shutdown(&self) {
        for server in &self.servers {
            server.shutdown().await;
        }
    }
}

/// Spawn the child, complete the initialize handshake, enumerate tools and
/// (when advertised) describe the resource surface for the read tool.
#[allow(clippy::type_complexity)]
async fn connect_and_list(
    server: &str,
    stdio: &StdioTransport,
    cwd: &Path,
    stderr_tail: &Arc<Mutex<VecDeque<String>>>,
    shared: Arc<ServerShared>,
    mailbox: Arc<McpNoticeMailbox>,
) -> Result<(Client, Option<u32>, Vec<DiscoveredTool>, bool, String), String> {
    let (client, pgid) = spawn_client(server, stdio, cwd, stderr_tail, shared, mailbox).await?;
    let advertises_resources = client
        .peer()
        .peer_info()
        .is_some_and(|info| info.capabilities.resources.is_some());
    let peer = client.peer();
    match list_all_tools(server, &peer).await {
        Ok(tools) => {
            let description = if advertises_resources {
                describe_resources(&peer)
                    .await
                    .unwrap_or_else(|error| {
                        eprintln!(
                            "[ki mcp] server {server}: resources/list failed ({error}); the read tool starts without a resource list"
                        );
                        format!(
                            "Read a resource from this server by uri. (The resource list is currently unavailable: {error})"
                        )
                    })
            } else {
                String::new()
            };
            Ok((client, pgid, tools, advertises_resources, description))
        }
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

/// Paginated `resources/list` + `resources/templates/list`, rendered as the
/// read tool's description. The list is the discoverability of the read tool —
/// bounded like every other server-provided text.
async fn describe_resources(peer: &Peer<RoleClient>) -> Result<String, String> {
    let resources = list_all_resources(peer, "resources", "resources").await?;
    let templates = list_all_resources(peer, "resourceTemplates", "resources/templates").await?;
    let mut text = String::from("Read a resource from this server by uri. Available resources:");
    if resources.is_empty() && templates.is_empty() {
        text.push_str("\n(none currently listed)");
    }
    for item in &resources {
        text.push_str(&format!("\n- {}", render_resource_entry(item)));
    }
    if !templates.is_empty() {
        text.push_str("\nTemplates (fill the braces into a uri):");
        for item in &templates {
            text.push_str(&format!("\n- {}", render_resource_entry(item)));
        }
    }
    Ok(bound_resource_text(text))
}

/// One page batch of a paginated listing. `items_field` is the result array's
/// key ("resources" for both resource kinds). Values are read through
/// `serde_json`: rmcp's typed result structs churn across versions, while the
/// wire shape is part of the protocol.
async fn list_all_resources(
    peer: &Peer<RoleClient>,
    items_field: &str,
    method_label: &str,
) -> Result<Vec<Value>, String> {
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors = std::collections::HashSet::new();
    for _ in 0..MAX_RESOURCE_PAGES {
        let mut params = PaginatedRequestParams::default();
        params.cursor = cursor.clone();
        let value: Value = if method_label == "resources" {
            let result = peer
                .list_resources(Some(params))
                .await
                .map_err(|error| format!("{method_label}: {error}"))?;
            serde_json::to_value(&result)
                .map_err(|error| format!("{method_label} encode: {error}"))?
        } else {
            let result = peer
                .list_resource_templates(Some(params))
                .await
                .map_err(|error| format!("{method_label}: {error}"))?;
            serde_json::to_value(&result)
                .map_err(|error| format!("{method_label} encode: {error}"))?
        };
        if let Some(batch) = value.get(items_field).and_then(Value::as_array) {
            items.extend(batch.iter().cloned());
        }
        match value.get("nextCursor").and_then(Value::as_str) {
            None => return Ok(items),
            Some(next) => {
                if !seen_cursors.insert(next.to_string()) {
                    return Err(format!("{method_label} repeated cursor {next:?}"));
                }
                cursor = Some(next.to_string());
            }
        }
    }
    Err(format!(
        "{method_label} exceeded {MAX_RESOURCE_PAGES} pages"
    ))
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
        result = handle.peer() => {
            result.map_err(|error| CallError::Failed(handle.decorate(error)))?
        }
    };
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
