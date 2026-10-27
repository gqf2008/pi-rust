//! `ki acp serve` 的会话状态机，以及取消语义所需的 turn 状态。
//!
//! 每个 [`LiveSession`] 独立持有 harness、事件接收器、turn/cancel 与对话框；
//! ACP 层按 session id 路由，连接能力快照只在激活时复制到各会话。
//!
//! compiled-extension 事件 sink 与 caps 都已按会话隔离：sink 与 `SessionCaps`
//! 一起挂在 `RegistrySnapshot` 上（进程级槽位只作无快照路径的 fallback），所以
//! `session::build` 之后的派发，以及 probe / 扩展命令 / prompt transform，都用各自
//! 会话的 caps，不再 last-writer-wins（`extension-abi.md` §6.50/§6.51）。
//! 已知残余：wasm **host 实例仍是进程级共享**，同一 guest 会观察所有会话的事件——
//! **隔离的是「用哪份 caps 派发」，不是「哪个 guest 收到事件」**；部分第三方插件也会在
//! 调用时读取进程 cwd。内置工具使用显式 cwd，ACP 的 run、cancel、标题和上下文不经过
//! 这些全局槽；不同 cwd 的第三方插件工具/项目状态目前仍不能按会话隔离，后续需要单独的
//! 插件路由改造。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::SessionId;
use agent_client_protocol::{Client, ConnectionTo};
use futures::future::BoxFuture;
use kiqi_agent::events::AgentEvent;
use kiqi_harness::agent_harness::AgentHarness;
use kiqi_host::UiDialogMailbox;
use tokio::sync::{broadcast, Notify};
use tokio_util::sync::CancellationToken;

use super::dialogs::DialogPump;

/// Per-session dependencies the ACP machinery drives. The production factory
/// assembles these through `session::build` (resolve → veto → build →
/// lifecycle); tests build them directly from a FauxProvider harness and never
/// touch `session::build` (it writes session JSONL under cwd and installs
/// global extension state).
pub struct SessionDeps {
    /// The session owner's strong handle. `LiveSession` keeps this exact `Arc`
    /// for its lifetime; the extension action host references it only weakly,
    /// so dropping the session releases the harness instead of leaking it
    /// through the sink → caps → host chain.
    pub harness: Arc<AgentHarness>,
    pub event_rx: broadcast::Receiver<AgentEvent>,
    /// The extension UI-dialog mailbox (runtime action 17). Production wires
    /// the host-built reload context's mailbox; dialog tests inject a
    /// hand-constructed mailbox. `None` = the session cannot ask.
    pub ui_dialog: Option<UiDialogMailbox>,
    /// MCP manager wired at build time; the between-turn boundary drain reads
    /// it. `None` = the session runs without MCP.
    pub mcp: Option<Arc<kiqi_mcp::manager::McpManager>>,
    /// Production only: the host's pre-built shutdown future (the extension
    /// `SessionShutdown` advisory flush + worker stop). The future owns the
    /// host reload context, so it is held until [`LiveSession::close`] runs
    /// it — extension runtimes outlive that dispatch.
    pub shutdown: Option<BoxFuture<'static, ()>>,
}

/// Per-`session/new` request parameters the ACP layer extracts before the
/// factory runs. Fields the pinned v1 schema carries only through `_meta`
/// land here once decoded.
#[derive(Default)]
pub struct SessionParams {
    /// `_meta.systemPrompt` as `{"append": text}` (the claude-agent-acp preset
    /// ABB sends; design doc §4.7 "systemPrompt 收下"). Appended to the base
    /// system prompt through the same `--append-system-prompt` channel.
    pub system_prompt_append: Option<String>,
    /// MCP servers the client supplied on `session/new`/`session/load`
    /// (`mcpServers`, stdio and Streamable HTTP — the legacy SSE variant is
    /// refused at decode). Decoded to the host config shape so the build seam
    /// starts them exactly like the settings-configured ones; POLICY 1: the
    /// client's request IS the authorization, so no project-trust gate applies
    /// here.
    pub mcp_servers: Vec<kiqi_mcp::ServerConfig>,
}

/// Which session the factory should build: a fresh one (`session/new`, the
/// selection derived from the serve process's default args) or the restore of
/// an existing ki session (`session/load`, restore-only — never creates).
#[derive(Debug, Clone)]
pub enum SessionRequest {
    New,
    Load { id: String },
}

/// Builds the per-session dependencies for a `session/new` cwd. `Err` carries
/// the user-facing message for the JSON-RPC error response (the producer has
/// already explained it loudly on stderr).
pub type SessionFactory = Arc<
    dyn Fn(&Path, SessionParams, SessionRequest) -> BoxFuture<'static, Result<SessionDeps, String>>
        + Send
        + Sync,
>;

/// Host-supplied `session/load` existence pre-check. Runs BEFORE the factory
/// on every load, so a stray id can never materialize a session file (the
/// restore selection would create one — never allowed here): `Ok(None)`
/// rejects the load without touching the factory. It lives with the host
/// crate because it needs the arg/config session-dir resolution.
pub type LoadPrecheck = Arc<
    dyn Fn(PathBuf, String) -> BoxFuture<'static, Result<Option<String>, String>> + Send + Sync,
>;

/// The host-injected seams for one `ki acp` process. Both halves assemble
/// from the host crate's roots (`args` / `config` / `provider` /
/// `session::build`); the protocol crate stays agnostic of them.
pub struct RunOptions {
    /// Builds a [`SessionDeps`] per `session/new` / `session/load`.
    pub factory: SessionFactory,
    /// Restore-only existence pre-check (see [`LoadPrecheck`]).
    pub load_precheck: LoadPrecheck,
}

/// The id-match rule for a load pre-check: exact id, else file-name
/// containment in either direction. Shared so the host and any test stand-in
/// agree on what "exists" means.
pub fn match_session_id(
    metas: &[kiqi_harness::session::jsonl::JsonlSessionMetadata],
    id: &str,
) -> Option<String> {
    metas
        .iter()
        .find(|meta| meta.id == id || meta.path.contains(id) || id.contains(&meta.id))
        .map(|meta| meta.id.clone())
}

/// 一个活跃的 ACP 会话；不同实例之间不共享 turn 或事件流。
pub(crate) struct LiveSession {
    pub(crate) id: SessionId,
    pub(crate) harness: Arc<AgentHarness>,
    /// The master event receiver; turns subscribe their own clone so projected
    /// updates and the prompt response serialize in send order. Never `recv`ed
    /// itself — `resubscribe` needs only `&self`, so no lock wraps it.
    event_rx: broadcast::Receiver<AgentEvent>,
    pub(crate) turn: Arc<TurnState>,
    /// MCP manager wired at build time (`None` = no MCP); read per prompt for
    /// the between-turn boundary drain.
    pub(crate) mcp: Option<Arc<kiqi_mcp::manager::McpManager>>,
    /// The host's pre-built shutdown future, run exactly once by
    /// [`LiveSession::close`] before the context drops.
    shutdown: Option<BoxFuture<'static, ()>>,
    /// The extension UI-dialog mailbox the session was built with.
    ui_dialog: Option<UiDialogMailbox>,
    /// The dialog pump started once a connection exists to carry the outbound
    /// permission requests (`start_dialog_pump`).
    pub(crate) dialogs: Option<Arc<DialogPump>>,
    /// Whether the ACP client advertised the `notices` session capability.
    /// The boundary drain forwards MCP resource-update notices as user-facing
    /// `SessionUpdate::Notice`s only when this is set (the schema's rule).
    /// Copied from the connection snapshot at session activation.
    pub(crate) client_notices: bool,
}

impl LiveSession {
    pub(crate) fn new(deps: SessionDeps) -> Self {
        Self {
            // The ACP session id DERIVES from the ki session id (production
            // mints uuid v7): restart-stable with zero persistence — a later
            // session/load strips the `ki-` prefix and restores exactly this
            // ki session.
            id: SessionId::from(format!(
                "ki-{}",
                deps.harness.session().storage().metadata().id
            )),
            harness: deps.harness,
            event_rx: deps.event_rx,
            turn: Arc::new(TurnState::new()),
            mcp: deps.mcp,
            shutdown: deps.shutdown,
            ui_dialog: deps.ui_dialog,
            dialogs: None,
            // Overwritten by the activation handlers from the connection's
            // initialize snapshot (the factory knows nothing of the client).
            client_notices: false,
        }
    }

    /// Spawns the dialog pump when the session carries an extension UI mailbox.
    /// Runs at every session activation — `session/new` and `session/load`
    /// alike, a loaded session can ask permissions too.
    pub(crate) fn start_dialog_pump(&mut self, cx: ConnectionTo<Client>) {
        if let Some(mailbox) = self.ui_dialog.clone() {
            self.dialogs = Some(DialogPump::start(mailbox, cx, self.id.clone()));
        }
    }

    /// A receiver positioned at "now" — created inside the prompt handler
    /// before the turn spawns, so no turn event can slip past it.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.event_rx.resubscribe()
    }

    /// The dialog mailbox is detached BEFORE the host shutdown future (the
    /// rpc-mode EOF order): cancelled mailbox entries release the extension
    /// polls while the extension runtime is still alive to observe them; the
    /// shutdown future then flushes state, and only afterwards does the
    /// reload context it owns — and with it the cdylibs — drop.
    pub(crate) async fn close(self) {
        // ACP session/close 要求先取消相关工作；等待 turn 真正退出后再销毁
        // 插件/MCP 上下文，避免运行中的工具看到被提前释放的资源。
        self.turn.cancel();
        if let Some(dialogs) = &self.dialogs {
            // 与 session/cancel 一样先释放权限等待，否则 turn 可能停在对话框上，
            // 而 close 又在等 turn，双方互等。
            dialogs.cancel_all();
        }
        self.turn.wait_idle().await;
        if let Some(dialogs) = &self.dialogs {
            dialogs.shutdown();
        }
        if let Some(shutdown) = self.shutdown {
            shutdown.await;
        }
    }
}

/// The two cancellation flags from the abb lessons, mapped onto ki: `cancel`
/// arms only while a turn is in flight (an idle cancel must NOT poison the
/// next turn), and `begin` claims in-flight BEFORE clearing a stale cancel.
pub(crate) struct TurnState {
    in_flight: AtomicBool,
    cancelled: AtomicBool,
    token: Mutex<Option<CancellationToken>>,
    idle: Notify,
}

impl TurnState {
    pub(crate) fn new() -> Self {
        Self {
            in_flight: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            token: Mutex::new(None),
            idle: Notify::new(),
        }
    }

    /// Claim the in-flight slot and mint the turn's cancellation token.
    /// `None` = a turn is already active (second concurrent prompt).
    /// Order matters: claim FIRST, then clear `cancelled`, so a cancel that
    /// raced just ahead of this turn cannot land inside it.
    pub(crate) fn begin(&self) -> Option<CancellationToken> {
        if self.in_flight.swap(true, Ordering::AcqRel) {
            return None;
        }
        self.cancelled.store(false, Ordering::Release);
        let token = CancellationToken::new();
        *self.token.lock().unwrap() = Some(token.clone());
        Some(token)
    }

    /// `session/cancel` (or connection EOF): a no-op while idle.
    pub(crate) fn cancel(&self) {
        if !self.in_flight.load(Ordering::Acquire) {
            return;
        }
        self.cancelled.store(true, Ordering::Release);
        if let Some(token) = self.token.lock().unwrap().clone() {
            token.cancel();
        }
    }

    /// 等待当前 turn 释放执行槽。先注册通知再复查状态，避免完成通知落在
    /// 状态检查与 await 之间而永久等待。
    pub(crate) async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            if !self.in_flight.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    /// Whether a client cancel was armed for the turn that is ending. The ACP
    /// spec requires `cancelled` as the stop reason after `session/cancel`,
    /// even when the run finished on its own in the race window.
    pub(crate) fn consume_cancelled(&self) -> bool {
        self.cancelled.swap(false, Ordering::AcqRel)
    }
}

/// Clears the in-flight slot on drop — covering the panic and
/// connection-teardown paths where the turn future dies mid-flight.
pub(crate) struct InFlightGuard(Arc<TurnState>);

impl InFlightGuard {
    pub(crate) fn new(state: Arc<TurnState>) -> Self {
        Self(state)
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.in_flight.store(false, Ordering::Release);
        *self.0.token.lock().unwrap() = None;
        self.0.idle.notify_waiters();
    }
}
