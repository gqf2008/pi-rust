//! `ki acp serve` — a host-level Agent Client Protocol server (batch 1:
//! initialize / session/new / session/prompt / session/cancel, one live
//! session per process; proposal `docs/design/ki-acp-serve.md`).
//!
//! The server owns the process's stdio for its lifetime (the same claim as
//! `--connect`, bypassing arg parsing, provider resolution, and stdin
//! draining): stdout carries only the protocol, everything human goes to
//! stderr ([`diag`]). Semantics that matter:
//!
//! - **Dispatch is serial** in the official crate — a slow inline handler
//!   blocks the read loop. Only `session/prompt` is long-running, so it does
//!   its bookkeeping inline and moves the turn into `ConnectionTo::spawn` (the
//!   official escape hatch). A spawned task returning `Err` tears down the
//!   whole connection, so turn failures report through the responder instead.
//! - **Cancellation** (`session/cancel`) runs inline: it arms only while a
//!   turn is in flight ([`sessions::TurnState`]) and trips the turn's token;
//!   the turn aborts the lane — re-issuing the abort on a bounded tick until
//!   the run has registered and taken it (abb's wakeup source, design doc
//!   §4.4) — and answers with the real outcome.
//! - **Ordering**: the turn drains the event stream until the terminal
//!   `AgentEnd` (bounded) before responding, so the response never overtakes
//!   the last `session/update` — both serialize through the outgoing actor.

mod diag;
mod projection;
mod sessions;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, InitializeRequest, InitializeResponse,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SessionId,
    SessionNotification, SessionUpdate, StopReason,
};
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Error, Responder, Stdio};
use rpi_agent::events::AgentEvent;
use rpi_harness::agent_harness::{AgentLane, HarnessRunOutcome, RunResult};
use rpi_harness::result::HarnessResult;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::acp::sessions::{InFlightGuard, LiveSession, SessionFactory, TurnState};
use crate::app::{EXIT_RUNTIME, EXIT_USAGE};

/// JSON-RPC `invalid params` — the request's own arguments are wrong.
const ERR_INVALID_PARAMS: i32 = -32602;
/// JSON-RPC `internal error` — a valid request failed inside the machinery.
const ERR_INTERNAL: i32 = -32603;
/// Server-defined — process-level constraints (one live session, one turn).
const ERR_SERVER: i32 = -32000;
/// How long the turn drain waits for the terminal event before answering.
const TERMINAL_DRAIN: Duration = Duration::from_millis(250);
/// How often a cancelled turn re-issues `lane.abort()` until the run has
/// registered and taken it. The first attempt is refused (`no_active_run`)
/// when the cancel beats the turn's first poll — the per-run token exists only
/// from `acquire_run` on — and a refused abort is silently lost, so the retry
/// needs a fixed wakeup source (abb's 100ms tick, design doc §4.4).
const CANCEL_ABORT_TICK: Duration = Duration::from_millis(100);

/// Shared server state: the one live session plus its factory.
struct ServerShared {
    live: Option<LiveSession>,
    factory: SessionFactory,
}

type SharedServer = Arc<Mutex<ServerShared>>;

/// `ki acp` entry. Expects exactly `["serve"]`; anything else is a usage error
/// (exit 2). Returns the process exit code: 0 = clean client EOF, 1 = runtime
/// failure (same values as [`crate::app::EXIT_RUNTIME`] / [`EXIT_USAGE`]).
pub async fn run(argv: &[String]) -> i32 {
    let (code, reason) = if argv.len() == 1 && argv[0] == "serve" {
        match serve().await {
            Ok(()) => (0, "client disconnected"),
            Err(error) => {
                eprintln!("error: acp server failed: {error}");
                (EXIT_RUNTIME, "server error")
            }
        }
    } else {
        eprintln!("usage: ki acp serve");
        (EXIT_USAGE, "usage error")
    };
    diag::shutdown(reason, code);
    code
}

/// The production server over stdio. `normalize_offline_mode` has already run
/// (the `app.rs` dispatcher precedes this match arm); `ki acp serve` takes no
/// flags of its own, so the session factory gets default args.
async fn serve() -> agent_client_protocol::Result<()> {
    let factory = sessions::production_factory(crate::args::Args::default());
    run_server(Stdio::new(), factory).await
}

/// The protocol server, generic over the transport (stdio in production,
/// in-memory duplexes in tests) and the [`SessionFactory`] (the
/// `session::build` pipeline in production, FauxProvider harnesses in tests).
pub(crate) async fn run_server(
    transport: impl ConnectTo<Agent> + 'static,
    factory: SessionFactory,
) -> agent_client_protocol::Result<()> {
    let shared: SharedServer = Arc::new(Mutex::new(ServerShared {
        live: None,
        factory,
    }));

    let shared_new = Arc::clone(&shared);
    let shared_prompt = Arc::clone(&shared);
    let shared_cancel = Arc::clone(&shared);

    let result = Agent
        .builder()
        .name("ki")
        .on_receive_request(
            async |request: InitializeRequest,
                   responder: Responder<InitializeResponse>,
                   _cx: ConnectionTo<Client>| {
                // The crate's v1 agent guard rejects non-v1 clients before this
                // handler runs and rewrites the response version to V1 — echo
                // the client's version and stay out of the way. Capabilities
                // are honest: batch 1 declares none (no load, no permissions).
                responder.respond(
                    InitializeResponse::new(request.protocol_version)
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest,
                        responder: Responder<NewSessionResponse>,
                        _cx: ConnectionTo<Client>| {
                handle_session_new(&shared_new, request, responder).await;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest,
                        responder: Responder<PromptResponse>,
                        cx: ConnectionTo<Client>| {
                handle_prompt(&shared_prompt, request, responder, cx).await;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: CancelNotification, _cx: ConnectionTo<Client>| {
                handle_cancel(&shared_cancel, notification);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(transport)
        .await;

    // Connection over (clean EOF or error): the crate has already dropped the
    // connection's task actor — and every spawned turn with it — by the time
    // `connect_to` resolves, so there is no in-flight turn left to abort here.
    // Run the SessionShutdown advisory before the reload context (and with it
    // the plugin cdylibs) drops.
    close_live_session(&shared).await;
    result
}

/// Dispatches the `SessionShutdown` advisory for the live session, if any. An
/// in-flight turn does not survive to this point: the crate drops the task
/// actor (and the turn future with it) while the connection is closing, and
/// the turn's guard releases the harness run on drop.
async fn close_live_session(shared: &SharedServer) {
    let session = shared.lock().unwrap().live.take();
    if let Some(session) = session {
        session.close().await;
    }
}

async fn handle_session_new(
    shared: &SharedServer,
    request: NewSessionRequest,
    responder: Responder<NewSessionResponse>,
) {
    // First-cut parameter parsing is loud, never silently ignoring (abb
    // lesson): ki's MCP runs through its own configuration.
    if !request.mcp_servers.is_empty() {
        diag::diagln!(
            "session/new rejected: {} mcp_servers entries",
            request.mcp_servers.len()
        );
        let _ = responder.respond_with_error(Error::new(
            ERR_INVALID_PARAMS,
            "ki does not accept session-level MCP servers; MCP runs through ki's own configuration",
        ));
        return;
    }
    // Same stance: additional workspace roots cannot be honored in batch 1
    // (the harness and its plugins are rooted at cwd alone), so a multi-root
    // client granting them must hear it now instead of watching tools silently
    // fail on paths it believed were in scope.
    if !request.additional_directories.is_empty() {
        diag::diagln!(
            "session/new rejected: {} additional_directories entries",
            request.additional_directories.len()
        );
        let _ = responder.respond_with_error(Error::new(
            ERR_INVALID_PARAMS,
            "ki does not accept additional_directories yet; the session is rooted at cwd alone",
        ));
        return;
    }
    let factory = {
        let state = shared.lock().unwrap();
        if state.live.is_some() {
            None
        } else {
            Some(Arc::clone(&state.factory))
        }
    };
    let Some(factory) = factory else {
        diag::diagln!("session/new rejected: a session is already live");
        let _ = responder.respond_with_error(Error::new(
            ERR_SERVER,
            "ki acp serve supports one live session per process; the current session is still live",
        ));
        return;
    };

    let cwd = request.cwd;
    diag::diagln!("session/new cwd={}", cwd.display());
    match factory(&cwd).await {
        Ok(deps) => {
            let session = LiveSession::new(deps);
            let id = session.id.clone();
            diag::diagln!("session/new ok id={id}");
            shared.lock().unwrap().live = Some(session);
            let _ = responder.respond(NewSessionResponse::new(id));
        }
        Err(message) => {
            diag::diagln!("session/new failed: {message}");
            let _ = responder.respond_with_error(Error::new(ERR_INTERNAL, message));
        }
    }
}

async fn handle_prompt(
    shared: &SharedServer,
    request: PromptRequest,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
) {
    let began = {
        let mut state = shared.lock().unwrap();
        match state.live.as_mut() {
            None => Err((
                ERR_INVALID_PARAMS,
                "no live session; send session/new first".to_string(),
            )),
            Some(session) if session.id != request.session_id => Err((
                ERR_INVALID_PARAMS,
                format!("unknown session {}", request.session_id),
            )),
            Some(session) => match session.turn.begin() {
                None => Err((ERR_SERVER, "a prompt turn is already in flight".to_string())),
                Some(token) => Ok((
                    token,
                    session.harness.lane("main"),
                    session.subscribe(),
                    Arc::clone(&session.turn),
                )),
            },
        }
    };
    let (token, lane, events, turn) = match began {
        Ok(parts) => parts,
        Err((code, message)) => {
            diag::diagln!("session/prompt rejected: {message}");
            let _ = responder.respond_with_error(Error::new(code, message));
            return;
        }
    };
    // Held from here on, so every path out of the turn — completion, panic, or
    // connection teardown dropping the future — clears in-flight.
    let guard = InFlightGuard::new(turn.clone());

    // User text = the join of the text content blocks (spec: text only in
    // batch 1); an empty join is a parameter error, not a silent no-op prompt.
    let text: String = request
        .prompt
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    if text.is_empty() {
        drop(guard);
        diag::diagln!("session/prompt rejected: no text content");
        let _ = responder
            .respond_with_error(Error::new(ERR_INVALID_PARAMS, "prompt has no text content"));
        return;
    }

    let session_id = request.session_id.clone();
    diag::diagln!("session/prompt id={session_id} chars={}", text.len());
    if let Err(error) = cx.spawn(drive_turn(
        lane,
        text,
        token,
        events,
        session_id,
        cx.clone(),
        responder,
        turn,
        guard,
    )) {
        // Spawn is only refused while the connection is shutting down; the
        // dropped future releases the guard, and the client is already gone.
        diag::diagln!("session/prompt spawn refused: {error}");
    }
}

/// The spawned prompt turn: drive `prompt_text` under the turn's token while
/// projecting the event stream, then answer. Failures go through the responder
/// — a spawned task returning `Err` would shut the connection down.
async fn drive_turn(
    lane: Arc<dyn AgentLane>,
    text: String,
    token: CancellationToken,
    events: broadcast::Receiver<AgentEvent>,
    session_id: SessionId,
    cx: ConnectionTo<Client>,
    responder: Responder<PromptResponse>,
    turn: Arc<TurnState>,
    _guard: InFlightGuard,
) -> agent_client_protocol::Result<()> {
    let prompt = prompt_cancellable(&lane, &text, token);
    tokio::pin!(prompt);

    // `None` disables the drain branch once the broadcast closes.
    let mut events: Option<broadcast::Receiver<AgentEvent>> = Some(events);
    let mut terminal_seen = false;
    let result: HarnessResult<RunResult> = loop {
        tokio::select! {
            biased;
            received = async {
                events
                    .as_mut()
                    .expect("drain branch guarded on events.is_some()")
                    .recv()
                    .await
            }, if events.is_some() => {
                match received {
                    Ok(event) => {
                        send_updates(&cx, &session_id, projection::project(&event));
                        if event.is_terminal() {
                            terminal_seen = true;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        diag::diagln!("event stream lagged; {skipped} updates were dropped");
                    }
                    Err(broadcast::error::RecvError::Closed) => events = None,
                }
            }
            outcome = &mut prompt => break outcome,
        }
    };

    // The harness resolves the prompt future before its bus delivers the
    // terminal event; drain briefly so the response never overtakes the last
    // update (precedent: `modes.rs` print mode's bounded post-run drain).
    if !terminal_seen {
        if let Some(receiver) = events.as_mut() {
            let _ = tokio::time::timeout(TERMINAL_DRAIN, async {
                // Mirror the main loop's error handling: a Lagged error leaves
                // the surviving buffered events receivable — possibly including
                // the terminal one — so it must log and keep draining, not end
                // the drain.
                loop {
                    match receiver.recv().await {
                        Ok(event) => {
                            send_updates(&cx, &session_id, projection::project(&event));
                            if event.is_terminal() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            diag::diagln!("event stream lagged; {skipped} updates were dropped");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            })
            .await;
        }
    }

    let was_cancelled = turn.consume_cancelled();
    match result {
        Ok(run) => match stop_reason(&run.outcome, was_cancelled) {
            Some(stop_reason) => {
                diag::diagln!("session/prompt id={session_id} done: {stop_reason:?}");
                let _ = responder.respond(PromptResponse::new(stop_reason));
            }
            None => {
                // A failed run surfaces as the JSON-RPC error it is, never as a
                // fabricated stop reason (abb lesson).
                let HarnessRunOutcome::Failed { error, .. } = &run.outcome else {
                    unreachable!("stop_reason returns None only for Failed");
                };
                diag::diagln!("session/prompt id={session_id} failed: {}", error.message);
                let _ = responder.respond_with_error(Error::new(
                    ERR_INTERNAL,
                    format!("run failed: {}", error.message),
                ));
            }
        },
        Err(error) => {
            // The run never started (harness rejection).
            diag::diagln!("session/prompt id={session_id} rejected: {error}");
            let _ = responder.respond_with_error(Error::new(ERR_INTERNAL, error.to_string()));
        }
    }
    Ok(())
}

/// Cancellation is delivered even when the cancel fires before `prompt_text`
/// has registered a run: trip the token, abort the lane, then await the real
/// outcome (precedent: `modes.rs::rpc_prompt_cancellable`). The first `abort`
/// is refused with `no_active_run` when the cancel beats the turn's first poll
/// (the per-run token is minted only inside `acquire_run`), so the abort is
/// re-issued on [`CANCEL_ABORT_TICK`] until the run registers and takes it —
/// a turn must never run to completion under an answered cancel.
async fn prompt_cancellable(
    lane: &Arc<dyn AgentLane>,
    text: &str,
    cancel: CancellationToken,
) -> HarnessResult<RunResult> {
    let prompt = lane.prompt_text(text, Vec::new());
    tokio::pin!(prompt);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {}
        result = &mut prompt => return result,
    }
    loop {
        // Idempotent: lands as `Ok` once the run is active, stays a harmless
        // `Err` while it is not (yet/anymore) registered.
        let _ = lane.abort().await;
        tokio::select! {
            biased;
            result = &mut prompt => return result,
            _ = tokio::time::sleep(CANCEL_ABORT_TICK) => {}
        }
    }
}

/// Outcome → stop reason. `None` = respond with an error instead (Failed
/// without a client cancel). `Suspended` answers `end_turn`: the run parked on
/// a provider deferred handle and ACP has no stop reason for "suspended" —
/// deferred resume is a batch-2+ question, so the why goes to stderr and the
/// turn ends.
fn stop_reason(outcome: &HarnessRunOutcome, was_cancelled: bool) -> Option<StopReason> {
    // A client cancel is answered `cancelled` no matter how the run then
    // ended: the protocol mandates it "even if the cancellation causes
    // exceptions in underlying operations", and abb checks the flag before
    // provider failure — the client's own cancel must never come back as a
    // JSON-RPC error.
    if was_cancelled {
        return Some(StopReason::Cancelled);
    }
    match outcome {
        HarnessRunOutcome::Failed { .. } => None,
        HarnessRunOutcome::Aborted { .. } => Some(StopReason::Cancelled),
        HarnessRunOutcome::Suspended { .. } => {
            eprintln!(
                "[ki acp] run suspended on a deferred handle; ending the turn (deferred resume is a later batch)"
            );
            Some(StopReason::EndTurn)
        }
        HarnessRunOutcome::Completed { .. } => Some(StopReason::EndTurn),
    }
}

/// `session/cancel` runs inline (serial dispatch): arming here cannot race a
/// turn start, because the turn's in-flight claim happens inline in the prompt
/// handler too. Idle cancels are absorbed by [`TurnState::cancel`].
fn handle_cancel(shared: &SharedServer, notification: CancelNotification) {
    let state = shared.lock().unwrap();
    if let Some(session) = state
        .live
        .as_ref()
        .filter(|session| session.id == notification.session_id)
    {
        diag::diagln!("session/cancel id={}", session.id);
        session.turn.cancel();
    }
}

fn send_updates(cx: &ConnectionTo<Client>, session_id: &SessionId, updates: Vec<SessionUpdate>) {
    for update in updates {
        if let Err(error) =
            cx.send_notification(SessionNotification::new(session_id.clone(), update))
        {
            diag::diagln!("session/update send failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::ProtocolVersion;
    use agent_client_protocol::ByteStreams;
    use rpi_agent::events::BroadcastEmitter;
    use rpi_ai::providers::faux::{FauxProvider, FauxScript};
    use rpi_ai::types::{Api, AssistantMessage, DeferredHandle};
    use rpi_harness::agent_harness::AgentHarness;
    use rpi_harness::session::types::OperationError;
    use rpi_harness::types::AgentHarnessOptions;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    // ---- unit: stop-reason mapping table ----

    fn assistant() -> AssistantMessage {
        AssistantMessage::empty(Api::Faux, "faux", "faux-1", 0)
    }

    #[test]
    fn stop_reason_mapping_table() {
        let completed = HarnessRunOutcome::Completed {
            leaf_id: "l".into(),
            final_entry_id: "e".into(),
            final_message: assistant(),
        };
        let aborted = HarnessRunOutcome::Aborted {
            leaf_id: "l".into(),
            final_entry_id: "e".into(),
            final_message: assistant(),
        };
        let suspended = HarnessRunOutcome::Suspended {
            leaf_id: "l".into(),
            final_entry_id: "e".into(),
            deferred: DeferredHandle {
                provider: "faux".into(),
                model_id: "faux-1".into(),
                api: "faux".into(),
                id: "d1".into(),
                expires_at: None,
                poll_after_ms: None,
                data: None,
            },
        };
        let failed = HarnessRunOutcome::Failed {
            leaf_id: "l".into(),
            error: OperationError {
                code: "boom".into(),
                message: "the provider exploded".into(),
            },
            final_entry_id: None,
            final_message: None,
        };

        assert_eq!(stop_reason(&completed, false), Some(StopReason::EndTurn));
        // ACP requires `cancelled` after a client cancel, completed-or-not.
        assert_eq!(stop_reason(&completed, true), Some(StopReason::Cancelled));
        assert_eq!(stop_reason(&aborted, false), Some(StopReason::Cancelled));
        assert_eq!(stop_reason(&aborted, true), Some(StopReason::Cancelled));
        assert_eq!(stop_reason(&suspended, false), Some(StopReason::EndTurn));
        // Failed is never a stop reason on its own — it becomes the JSON-RPC
        // error…
        assert_eq!(stop_reason(&failed, false), None);
        // …but a client cancel is answered `cancelled` even when the
        // cancellation is what tore the run into a failure (protocol MUST).
        assert_eq!(stop_reason(&failed, true), Some(StopReason::Cancelled));
    }

    // ---- unit: TurnState + InFlightGuard ----

    #[test]
    fn idle_cancel_is_absorbed_and_guard_clears_in_flight() {
        let turn = Arc::new(TurnState::new());
        // Idle cancel: a no-op that must not poison the next turn.
        turn.cancel();

        let token = turn.begin().expect("first begin");
        assert!(
            !turn.consume_cancelled(),
            "idle cancel leaked into the turn"
        );
        {
            let _guard = InFlightGuard::new(Arc::clone(&turn));
            // Second begin while in flight is refused.
            assert!(turn.begin().is_none());
            turn.cancel();
            assert!(token.is_cancelled());
            assert!(turn.consume_cancelled());
        }

        // Guard drop cleared in-flight, so a fresh turn can begin — and it
        // starts uncancelled (begin clears the stale flag after claiming
        // in-flight — the abb ordering).
        let _token = turn.begin().expect("guard drop did not clear in-flight");
        assert!(
            !turn.consume_cancelled(),
            "previous turn's cancel leaked into the next turn"
        );
    }

    // ---- e2e over duplex ----

    type Updates = tokio::sync::mpsc::UnboundedReceiver<SessionNotification>;

    /// A deterministic FauxProvider harness with its broadcast receiver — the
    /// seam tests use instead of `session::build` (which writes session JSONL
    /// and installs global plugin state).
    async fn faux_deps(text: &str, tokens_per_second: f64) -> sessions::SessionDeps {
        let provider = FauxProvider::new(
            FauxScript::new()
                .with_text(text)
                .with_tokens_per_second(tokens_per_second),
        );
        let (broadcast, event_rx) = BroadcastEmitter::new(256);
        let options = AgentHarnessOptions {
            model: provider.default_model().clone(),
            models: vec![provider.clone()],
            agent_emitter: Some(Arc::new(broadcast)),
            ..Default::default()
        };
        let harness = AgentHarness::create(options).await.expect("faux harness");
        sessions::SessionDeps {
            harness,
            event_rx,
            reload_context: None,
        }
    }

    fn faux_factory(text: &'static str, tokens_per_second: f64) -> SessionFactory {
        Arc::new(move |_cwd| {
            Box::pin(async move { Ok(faux_deps(text, tokens_per_second).await) })
                as futures::future::BoxFuture<'static, Result<sessions::SessionDeps, String>>
        })
    }

    /// Run the server over an in-memory duplex and drive a typed client
    /// against it. The client's `session/update` notifications go to
    /// `updates_tx`; the server must shut down cleanly after the client leaves.
    async fn run_client<C, R>(
        factory: SessionFactory,
        updates_tx: tokio::sync::mpsc::UnboundedSender<SessionNotification>,
        client_main: C,
    ) -> agent_client_protocol::Result<R>
    where
        C: AsyncFnOnce(ConnectionTo<Agent>) -> agent_client_protocol::Result<R> + Send + 'static,
    {
        let (client_writer, server_reader) = tokio::io::duplex(64 * 1024);
        let (server_writer, client_reader) = tokio::io::duplex(64 * 1024);
        let server_transport =
            ByteStreams::new(server_writer.compat_write(), server_reader.compat());
        let client_transport =
            ByteStreams::new(client_writer.compat_write(), client_reader.compat());
        let server = tokio::spawn(run_server(server_transport, factory));
        let result = Client
            .builder()
            .on_receive_notification(
                async move |notification: SessionNotification, _cx: ConnectionTo<Agent>| {
                    let _ = updates_tx.send(notification);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(client_transport, client_main)
            .await;
        // The client transport dropped with its connection; the server sees
        // EOF, runs teardown (turn grace + SessionShutdown), and must land on
        // the clean path.
        let server_result = tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("server did not stop after client eof")
            .expect("server task panicked");
        server_result.expect("server errored");
        result
    }

    /// initialize + session/new; returns the session id.
    async fn init_and_new(cx: &ConnectionTo<Agent>) -> agent_client_protocol::Result<SessionId> {
        let init = cx
            .send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        assert_eq!(init.protocol_version, ProtocolVersion::V1);
        let new = cx
            .send_request(NewSessionRequest::new(std::env::temp_dir()))
            .block_task()
            .await?;
        Ok(new.session_id)
    }

    /// Drain agent message chunks until they join to `want` (or the deadline
    /// passes) and return the joined text.
    async fn wait_for_text(mut updates: Updates, want: &str) -> String {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut text = String::new();
        while text != want {
            let Ok(Some(notification)) = tokio::time::timeout_at(deadline, updates.recv()).await
            else {
                break;
            };
            if let SessionUpdate::AgentMessageChunk(chunk) = notification.update {
                if let ContentBlock::Text(received) = chunk.content {
                    text.push_str(&received.text);
                }
            }
        }
        text
    }

    /// Collect everything already delivered on the updates channel (after a
    /// short settle so trailing notifications land), joining chunk text.
    async fn collect_text(updates: &mut Updates) -> String {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut text = String::new();
        while let Ok(notification) = updates.try_recv() {
            if let SessionUpdate::AgentMessageChunk(chunk) = notification.update {
                if let ContentBlock::Text(received) = chunk.content {
                    text.push_str(&received.text);
                }
            }
        }
        text
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn happy_path_streams_chunks_and_answers_end_turn() {
        const SCRIPT: &str = "hello from faux";
        let (updates_tx, updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let result = run_client(
            faux_factory(SCRIPT, 1000.0),
            updates_tx,
            async move |cx: ConnectionTo<Agent>| {
                let sid = init_and_new(&cx).await?;
                let response = cx
                    .send_request(PromptRequest::new(
                        sid.clone(),
                        vec![ContentBlock::from("hi")],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(response.stop_reason, StopReason::EndTurn);
                // Updates precede the response on the wire; the client only
                // dispatches the already-received notifications asynchronously,
                // so wait for the full script on the channel.
                let text = wait_for_text(updates_rx, SCRIPT).await;
                assert_eq!(text, SCRIPT);
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancel_during_prompt_answers_cancelled() {
        const SCRIPT: &str = "still streaming the response";
        let (updates_tx, mut updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let result = run_client(
            faux_factory(SCRIPT, 20.0),
            updates_tx,
            async move |cx: ConnectionTo<Agent>| {
                let sid = init_and_new(&cx).await?;
                let (tx, rx) = tokio::sync::oneshot::channel();
                let sent = cx.send_request(PromptRequest::new(
                    sid.clone(),
                    vec![ContentBlock::from("go")],
                ));
                sent.on_receiving_result(async move |result| {
                    let _ = tx.send(result);
                    Ok(())
                })?;
                // Same burst as the prompt: serial dispatch reads this cancel
                // before the spawned turn's first poll, so the run has not
                // registered yet and the first abort is refused. It must still
                // land (re-issued until the run takes it), cutting the stream
                // short — `stop_reason` alone cannot tell the lost-abort world
                // apart, so the streamed text is the real assertion.
                cx.send_notification(CancelNotification::new(sid))?;
                let response = rx
                    .await
                    .expect("prompt response channel")
                    .expect("prompt failed");
                assert_eq!(response.stop_reason, StopReason::Cancelled);
                let text = collect_text(&mut updates_rx).await;
                assert!(
                    text.len() < SCRIPT.len(),
                    "cancel must abort the run, not let it stream to completion: got {text:?}"
                );
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancel_mid_stream_aborts_the_run() {
        const SCRIPT: &str =
            "a considerably longer faux script that needs several seconds to stream fully";
        let (updates_tx, mut updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let result = run_client(
            faux_factory(SCRIPT, 20.0),
            updates_tx,
            async move |cx: ConnectionTo<Agent>| {
                let sid = init_and_new(&cx).await?;
                let (tx, rx) = tokio::sync::oneshot::channel();
                let sent = cx.send_request(PromptRequest::new(
                    sid.clone(),
                    vec![ContentBlock::from("go")],
                ));
                sent.on_receiving_result(async move |result| {
                    let _ = tx.send(result);
                    Ok(())
                })?;
                // Consistency test ① (design doc §4.4): cancel only once the
                // run is visibly streaming — the abort must reach the
                // registered run and cut it short.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
                let mut text = String::new();
                while text.is_empty() {
                    let Ok(Some(notification)) =
                        tokio::time::timeout_at(deadline, updates_rx.recv()).await
                    else {
                        break;
                    };
                    if let SessionUpdate::AgentMessageChunk(chunk) = notification.update {
                        if let ContentBlock::Text(received) = chunk.content {
                            text.push_str(&received.text);
                        }
                    }
                }
                assert!(!text.is_empty(), "run never streamed before the cancel");
                cx.send_notification(CancelNotification::new(sid))?;
                let response = rx
                    .await
                    .expect("prompt response channel")
                    .expect("prompt failed");
                assert_eq!(response.stop_reason, StopReason::Cancelled);
                text.push_str(&collect_text(&mut updates_rx).await);
                assert!(
                    text.len() < SCRIPT.len(),
                    "cancel must abort the registered run mid-stream: got {text:?}"
                );
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn idle_cancel_is_a_noop_and_next_prompt_completes() {
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let result = run_client(
            faux_factory("untouched", 1000.0),
            updates_tx,
            async move |cx: ConnectionTo<Agent>| {
                let sid = init_and_new(&cx).await?;
                // Cancel with nothing in flight: absorbed, must not poison the
                // next turn.
                cx.send_notification(CancelNotification::new(sid.clone()))?;
                tokio::time::sleep(Duration::from_millis(100)).await;
                let response = cx
                    .send_request(PromptRequest::new(sid, vec![ContentBlock::from("hi")]))
                    .block_task()
                    .await?;
                assert_eq!(response.stop_reason, StopReason::EndTurn);
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn second_prompt_while_one_is_active_is_an_error() {
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let result = run_client(
            faux_factory("still streaming the response", 20.0),
            updates_tx,
            async move |cx: ConnectionTo<Agent>| {
                let sid = init_and_new(&cx).await?;
                let (tx, rx) = tokio::sync::oneshot::channel();
                let sent = cx.send_request(PromptRequest::new(
                    sid.clone(),
                    vec![ContentBlock::from("first")],
                ));
                sent.on_receiving_result(async move |result| {
                    let _ = tx.send(result);
                    Ok(())
                })?;
                // The first turn is still in flight (slow faux), so the second
                // prompt is refused without disturbing it.
                let second = cx
                    .send_request(PromptRequest::new(sid, vec![ContentBlock::from("second")]))
                    .block_task()
                    .await;
                let error = second.expect_err("second prompt should fail");
                assert_eq!(i32::from(error.code), ERR_SERVER);
                let first = rx
                    .await
                    .expect("first prompt response channel")
                    .expect("first prompt failed");
                assert_eq!(first.stop_reason, StopReason::EndTurn);
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn second_live_session_is_refused_with_server_error() {
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let result = run_client(
            faux_factory("ok", 1000.0),
            updates_tx,
            async move |cx: ConnectionTo<Agent>| {
                let _sid = init_and_new(&cx).await?;
                let second = cx
                    .send_request(NewSessionRequest::new(std::env::temp_dir()))
                    .block_task()
                    .await;
                let error = second.expect_err("second session should fail");
                assert_eq!(i32::from(error.code), ERR_SERVER);
                assert!(error.message.contains("one live session per process"));
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mcp_servers_are_rejected_and_unknown_sessions_fail() {
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let result = run_client(
            faux_factory("ok", 1000.0),
            updates_tx,
            async move |cx: ConnectionTo<Agent>| {
                // mcp_servers non-empty → invalid params, before any session.
                let rejected =
                    cx.send_request(NewSessionRequest::new(std::env::temp_dir()).mcp_servers(
                        vec![agent_client_protocol::schema::v1::McpServer::Stdio(
                            agent_client_protocol::schema::v1::McpServerStdio::new(
                                "test",
                                "/bin/true",
                            ),
                        )],
                    ))
                    .block_task()
                    .await;
                let error = rejected.expect_err("mcp_servers should be refused");
                assert_eq!(i32::from(error.code), ERR_INVALID_PARAMS);

                // additional_directories non-empty → invalid params too (never
                // silently ignored: the session is rooted at cwd alone).
                let rejected = cx
                    .send_request(
                        NewSessionRequest::new(std::env::temp_dir()).additional_directories(vec![
                            std::env::temp_dir().join("ki-acp-additional-root"),
                        ]),
                    )
                    .block_task()
                    .await;
                let error = rejected.expect_err("additional_directories should be refused");
                assert_eq!(i32::from(error.code), ERR_INVALID_PARAMS);

                // Prompt without a session → invalid params.
                let ghost = SessionId::from("ki-ghost");
                let orphan = cx
                    .send_request(PromptRequest::new(ghost, vec![ContentBlock::from("hi")]))
                    .block_task()
                    .await;
                let error = orphan.expect_err("prompt without a session should fail");
                assert_eq!(i32::from(error.code), ERR_INVALID_PARAMS);
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }
}
