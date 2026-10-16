//! The single-session state machine for `ki acp serve`, plus the turn state
//! the cancellation discipline (abb lessons) needs.
//!
//! ONE LIVE SESSION PER PROCESS (batch 1): the compiled-plugin event sink is a
//! process-global last-writer-wins slot installed per `session::build`
//! (`kiqi-extensions::compiled::set_compiled_event_sink`), and plugins read the
//! process cwd at call time — a second concurrent session build would steal
//! the first session's plugin events. `session/new` while a session is live is
//! rejected with a JSON-RPC error; after a clean close a new one may be built.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::SessionId;
use agent_client_protocol::{Client, ConnectionTo};
use futures::future::BoxFuture;
use kiqi_agent::events::AgentEvent;
use kiqi_extensions::UiDialogMailbox;
use kiqi_harness::agent_harness::AgentHarness;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::dialogs::DialogPump;

/// Per-session dependencies the ACP machinery drives. The production factory
/// assembles these through `session::build` (resolve → veto → build →
/// lifecycle); tests build them directly from a FauxProvider harness and never
/// touch `session::build` (it writes session JSONL under cwd and installs
/// global plugin state).
pub(crate) struct SessionDeps {
    pub(crate) harness: AgentHarness,
    pub(crate) event_rx: broadcast::Receiver<AgentEvent>,
    /// The plugin UI-dialog mailbox (runtime action 17). Production wires
    /// `reload_context.ui_dialog.clone()`; dialog tests inject a
    /// hand-constructed mailbox. `None` = the session cannot ask.
    pub(crate) ui_dialog: Option<UiDialogMailbox>,
    /// Production only. Held until [`LiveSession::close`] has dispatched the
    /// `SessionShutdown` advisory, so plugin cdylibs outlive that dispatch.
    pub(crate) reload_context: Option<crate::session::ReloadContext>,
}

/// Per-`session/new` request parameters the ACP layer extracts before the
/// factory runs. Fields the pinned v1 schema carries only through `_meta`
/// land here once decoded.
#[derive(Default)]
pub(crate) struct SessionParams {
    /// `_meta.systemPrompt` as `{"append": text}` (the claude-agent-acp preset
    /// ABB sends; design doc §4.7 "systemPrompt 收下"). Appended to the base
    /// system prompt through the same `--append-system-prompt` channel.
    pub(crate) system_prompt_append: Option<String>,
    /// MCP servers the client supplied on `session/new`/`session/load`
    /// (`mcpServers`, stdio only — HTTP/SSE is refused at decode). Decoded to
    /// the host config shape so the build seam starts them exactly like the
    /// settings-configured ones; POLICY 1: the client's request IS the
    /// authorization, so no project-trust gate applies here.
    pub(crate) mcp_servers: Vec<kiqi_mcp::ServerConfig>,
}

/// Which session the factory should build: a fresh one (`session/new`, the
/// selection derived from the serve process's default args) or the restore of
/// an existing ki session (`session/load`, restore-only — never creates).
#[derive(Debug, Clone)]
pub(crate) enum SessionRequest {
    New,
    Load { id: String },
}

/// Builds the per-session dependencies for a `session/new` cwd. `Err` carries
/// the user-facing message for the JSON-RPC error response (the producer has
/// already explained it loudly on stderr).
pub(crate) type SessionFactory = Arc<
    dyn Fn(&Path, SessionParams, SessionRequest) -> BoxFuture<'static, Result<SessionDeps, String>>
        + Send
        + Sync,
>;

/// The one live ACP session.
pub(crate) struct LiveSession {
    pub(crate) id: SessionId,
    pub(crate) harness: AgentHarness,
    /// The master event receiver; turns subscribe their own clone so projected
    /// updates and the prompt response serialize in send order. Never `recv`ed
    /// itself — `resubscribe` needs only `&self`, so no lock wraps it.
    event_rx: broadcast::Receiver<AgentEvent>,
    pub(crate) turn: Arc<TurnState>,
    pub(crate) reload_context: Option<crate::session::ReloadContext>,
    /// The plugin UI-dialog mailbox the session was built with.
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
            reload_context: deps.reload_context,
            ui_dialog: deps.ui_dialog,
            dialogs: None,
            // Overwritten by the activation handlers from the connection's
            // initialize snapshot (the factory knows nothing of the client).
            client_notices: false,
        }
    }

    /// Spawns the dialog pump when the session carries a plugin UI mailbox.
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

    /// The dialog mailbox is detached BEFORE the `SessionShutdown` advisory
    /// (the rpc-mode EOF order): cancelled mailbox entries release the plugin
    /// polls while the plugin runtime is still alive to observe them; the
    /// advisory then flushes state, and only afterwards does the reload
    /// context — and with it the cdylibs — drop.
    pub(crate) async fn close(self) {
        if let Some(dialogs) = &self.dialogs {
            dialogs.shutdown();
        }
        if let Some(reload_context) = self.reload_context {
            crate::session::dispatch_session_shutdown_advisory(&reload_context).await;
            drop(reload_context);
        }
    }
}

/// Restore-only existence pre-check for `session/load`: the session must exist
/// under the request cwd BEFORE the factory runs, so a stray id can never
/// materialize a session file (`ByExactId` would create one — never used here).
/// Mirrors the `ById` matching semantics (exact id, else file-name containment).
///
/// The directory is the same resolution a fresh session in this process uses
/// (`resolve_session_dir`: `--session-dir`, then the saved `sessionDir`
/// setting, then `<cwd>/.ki/sessions`) — serve passes default args, so this
/// matches the factory's `ById` selection and an id minted under a configured
/// directory stays loadable instead of dying as "unknown sessionId".
pub(crate) async fn existing_session_id(cwd: &Path, id: &str) -> Result<Option<String>, String> {
    let dir = crate::session::resolve_session_dir(&crate::args::Args::default(), cwd);
    let metas = crate::session::list_session_metadata_in_dir(&cwd.to_string_lossy(), &dir as &Path)
        .await
        .map_err(|error| error.to_string())?;
    Ok(metas
        .iter()
        .find(|meta| meta.id == id || meta.path.contains(id) || id.contains(&meta.id))
        .map(|meta| meta.id.clone()))
}

/// The two cancellation flags from the abb lessons, mapped onto ki: `cancel`
/// arms only while a turn is in flight (an idle cancel must NOT poison the
/// next turn), and `begin` claims in-flight BEFORE clearing a stale cancel.
pub(crate) struct TurnState {
    in_flight: AtomicBool,
    cancelled: AtomicBool,
    token: Mutex<Option<CancellationToken>>,
}

impl TurnState {
    pub(crate) fn new() -> Self {
        Self {
            in_flight: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            token: Mutex::new(None),
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
    }
}

/// The production factory: the full `session::build` pipeline for a
/// `session/new` cwd — model/credential resolution (loud fast-fail, never the
/// unconfigured interactive fallback), chdir (loud — a session rooted at the
/// wrong tree is worse than no session), harness build, the `ownsStdio`
/// hand-off check, the `BeforeTuiStart` veto (loud — fails session/new), and
/// the `SessionStart` advisory (warn-and-continue). Mirrors the headless RPC
/// arm of `app.rs`.
pub(crate) fn production_factory(args: crate::args::Args) -> SessionFactory {
    Arc::new(
        move |cwd: &Path, params: SessionParams, request: SessionRequest| {
            Box::pin(build_session(
                args.clone(),
                cwd.to_path_buf(),
                params,
                request,
            )) as BoxFuture<'static, Result<SessionDeps, String>>
        },
    )
}

async fn build_session(
    mut args: crate::args::Args,
    cwd: PathBuf,
    params: SessionParams,
    request: SessionRequest,
) -> Result<SessionDeps, String> {
    // `session/load` restores one specific ki session and never creates; the
    // `ById` selection carries restore-only semantics (NotFound on a miss) and
    // flips `allow_existing_session`, so the harness loads the existing
    // transcript and salvages any interrupted state BEFORE replay reads it.
    // The restore directory goes through the same resolution as a fresh
    // session (`resolve_session_dir`), so an id `session/new` minted under a
    // configured `sessionDir` is loadable by the same serve process.
    let selection = match request {
        SessionRequest::New => crate::session::select_session(&args, &cwd),
        SessionRequest::Load { id } => crate::session::SessionSelection::ById {
            id,
            dir: crate::session::resolve_session_dir(&args, &cwd),
        },
    };
    let project_trusted = crate::session::resolve_project_trust(&args, &cwd);
    let resolved =
        match crate::provider::resolve_for_cwd(None, None, None, None, None, &cwd, project_trusted)
        {
            Ok(resolved) => resolved,
            Err(error) => {
                crate::app::print_resolve_error(&error);
                return Err(format!(
                    "model resolution failed for {}: {error}",
                    cwd.display()
                ));
            }
        };
    // Plugins read the process cwd at call time, so the session's cwd must
    // become the process cwd. A chdir that cannot land (missing directory, no
    // search permission) would silently re-root every relative path the
    // session touches — fail the session/new loudly instead of running
    // against the launch directory while the client believes otherwise.
    if let Err(error) = std::env::set_current_dir(&cwd) {
        let message = format!("could not chdir to {}: {error}", cwd.display());
        eprintln!("error: acp session/new: {message}");
        return Err(message);
    }
    // Design doc §4.7: `systemPrompt` 收下, not silently dropped. The claude
    // preset ABB sends (`_meta.systemPrompt = {"append": text}`) layers onto
    // whatever base prompt discovery yields, which is exactly the
    // `--append-system-prompt` channel.
    if let Some(append) = params.system_prompt_append {
        args.append_system_prompt.push(append);
    }
    let (harness, event_rx, reload_context) = crate::session::build_with_selection(
        &resolved,
        &args,
        &cwd,
        project_trusted,
        selection,
        &params.mcp_servers,
    )
    .await
    .map_err(|error| format!("harness build failed: {error}"))?;
    // A plugin declaring `{"ownsStdio": true}` owns this process's stdio (it
    // is an ACP/MCP stdio server itself). The TUI/RPC arms detect this after
    // load and yield the streams (`app.rs` post-load stdin claim); an ACP
    // server cannot — stdout IS the JSON-RPC wire — so the only honest move
    // is to refuse the session before the two servers corrupt each other.
    let owns_stdio = reload_context
        .extension_session
        .lock()
        .map(|session| session.owns_stdio())
        .unwrap_or(false);
    if owns_stdio {
        let message = "an installed extension declares ownsStdio and owns this process's stdio; `ki acp serve` cannot share the wire".to_string();
        eprintln!("error: acp session/new: {message}");
        // Unlike the app.rs RPC precedent this mirrors — which exits the
        // process, tearing every cdylib down with it — this server keeps
        // running, so the loaded extensions must be given the shutdown
        // advisory (flush + worker stop) before their context drops, and a
        // retried session/new must not stack on an untorn-down first build.
        crate::session::dispatch_session_shutdown_advisory(&reload_context).await;
        return Err(message);
    }
    if let Some(reason) = crate::session::dispatch_session_event_async(
        &reload_context,
        kiqi_plugin_sdk::EventTag::BeforeTuiStart,
    )
    .await
    {
        eprintln!("[ki] startup vetoed by extension: {reason}");
        crate::session::dispatch_session_shutdown_advisory(&reload_context).await;
        return Err(format!("startup vetoed by extension: {reason}"));
    }
    if let Some(reason) = crate::session::dispatch_session_event_async(
        &reload_context,
        kiqi_plugin_sdk::EventTag::SessionStart,
    )
    .await
    {
        // No tracing subscriber is installed anywhere in kiqi-cli, so a
        // `tracing::warn!` here would vanish; §6 sends everything to stderr.
        eprintln!("[ki] extension vetoed SessionStart (headless, advisory): {reason}");
    }
    Ok(SessionDeps {
        harness,
        event_rx,
        ui_dialog: Some(reload_context.ui_dialog.clone()),
        reload_context: Some(reload_context),
    })
}
