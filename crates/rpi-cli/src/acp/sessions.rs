//! The single-session state machine for `ki acp serve`, plus the turn state
//! the cancellation discipline (abb lessons) needs.
//!
//! ONE LIVE SESSION PER PROCESS (batch 1): the compiled-plugin event sink is a
//! process-global last-writer-wins slot installed per `session::build`
//! (`rpi-extensions::compiled::set_compiled_event_sink`), and plugins read the
//! process cwd at call time — a second concurrent session build would steal
//! the first session's plugin events. `session/new` while a session is live is
//! rejected with a JSON-RPC error; after a clean close a new one may be built.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::SessionId;
use futures::future::BoxFuture;
use rpi_agent::events::AgentEvent;
use rpi_harness::agent_harness::AgentHarness;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Per-session dependencies the ACP machinery drives. The production factory
/// assembles these through `session::build` (resolve → veto → build →
/// lifecycle); tests build them directly from a FauxProvider harness and never
/// touch `session::build` (it writes session JSONL under cwd and installs
/// global plugin state).
pub(crate) struct SessionDeps {
    pub(crate) harness: AgentHarness,
    pub(crate) event_rx: broadcast::Receiver<AgentEvent>,
    /// Production only. Held until [`LiveSession::close`] has dispatched the
    /// `SessionShutdown` advisory, so plugin cdylibs outlive that dispatch.
    pub(crate) reload_context: Option<crate::session::ReloadContext>,
}

/// Builds the per-session dependencies for a `session/new` cwd. `Err` carries
/// the user-facing message for the JSON-RPC error response (the producer has
/// already explained it loudly on stderr).
pub(crate) type SessionFactory =
    Arc<dyn Fn(&Path) -> BoxFuture<'static, Result<SessionDeps, String>> + Send + Sync>;

/// The one live ACP session.
pub(crate) struct LiveSession {
    pub(crate) id: SessionId,
    pub(crate) harness: AgentHarness,
    /// The master event receiver; turns subscribe their own clone so projected
    /// updates and the prompt response serialize in send order.
    event_rx: Mutex<broadcast::Receiver<AgentEvent>>,
    pub(crate) turn: Arc<TurnState>,
    reload_context: Option<crate::session::ReloadContext>,
}

impl LiveSession {
    pub(crate) fn new(deps: SessionDeps) -> Self {
        Self {
            id: new_session_id(),
            harness: deps.harness,
            event_rx: Mutex::new(deps.event_rx),
            turn: Arc::new(TurnState::new()),
            reload_context: deps.reload_context,
        }
    }

    /// A receiver positioned at "now" — created inside the prompt handler
    /// before the turn spawns, so no turn event can slip past it.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.event_rx.lock().unwrap().resubscribe()
    }

    /// `SessionShutdown` advisory BEFORE the reload context drops: plugins
    /// flush state there, and the cdylibs must stay mapped through dispatch.
    pub(crate) async fn close(self) {
        if let Some(reload_context) = self.reload_context {
            crate::session::dispatch_session_shutdown_advisory(&reload_context).await;
            drop(reload_context);
        }
    }
}

fn new_session_id() -> SessionId {
    SessionId::from(format!("ki-{}", uuid::Uuid::new_v4()))
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
/// unconfigured interactive fallback), best-effort chdir, harness build, the
/// `BeforeTuiStart` veto (loud — fails session/new), and the `SessionStart`
/// advisory (warn-and-continue). Mirrors the headless RPC arm of `app.rs`.
pub(crate) fn production_factory(args: crate::args::Args) -> SessionFactory {
    Arc::new(move |cwd: &Path| {
        Box::pin(build_session(args.clone(), cwd.to_path_buf()))
            as BoxFuture<'static, Result<SessionDeps, String>>
    })
}

async fn build_session(args: crate::args::Args, cwd: PathBuf) -> Result<SessionDeps, String> {
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
    // become the process cwd. Best-effort: a read-only cwd still runs.
    if let Err(error) = std::env::set_current_dir(&cwd) {
        eprintln!("warning: could not chdir to {}: {error}", cwd.display());
    }
    let (harness, event_rx, reload_context) =
        crate::session::build(&resolved, &args, &cwd, project_trusted)
            .await
            .map_err(|error| format!("harness build failed: {error}"))?;
    if let Some(reason) = crate::session::dispatch_session_event_async(
        &reload_context,
        rpi_plugin_sdk::EventTag::BeforeTuiStart,
    )
    .await
    {
        eprintln!("[ki] startup vetoed by extension: {reason}");
        return Err(format!("startup vetoed by extension: {reason}"));
    }
    if let Some(reason) = crate::session::dispatch_session_event_async(
        &reload_context,
        rpi_plugin_sdk::EventTag::SessionStart,
    )
    .await
    {
        tracing::warn!("[ki] extension vetoed SessionStart (headless, advisory): {reason}");
    }
    Ok(SessionDeps {
        harness,
        event_rx,
        reload_context: Some(reload_context),
    })
}
