//! `ki acp serve` — a host-level Agent Client Protocol server (batch 1:
//! initialize / session/new / session/load / session/prompt / session/cancel /
//! session/close, multiple live sessions per process; proposal
//! `docs/design/ki-acp-serve.md`).
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
mod dialogs;
mod projection;
mod sessions;
mod zed;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, AuthenticateRequest, AuthenticateResponse, CancelNotification,
    CloseSessionRequest, CloseSessionResponse, ContentBlock, InitializeRequest, InitializeResponse,
    LoadSessionRequest, LoadSessionResponse, LogoutRequest, LogoutResponse, McpCapabilities,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SessionCapabilities,
    SessionCloseCapabilities, SessionId, SessionNotification, SessionUpdate, StopReason,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Error, Responder, Stdio};
use kiqi_agent::events::AgentEvent;
use kiqi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome, RunResult};
use kiqi_harness::result::HarnessResult;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::acp::sessions::{InFlightGuard, LiveSession, SessionFactory, TurnState};
use crate::app::{EXIT_RUNTIME, EXIT_USAGE};

/// JSON-RPC `invalid params` — the request's own arguments are wrong.
const ERR_INVALID_PARAMS: i32 = -32602;
/// JSON-RPC `internal error` — a valid request failed inside the machinery.
const ERR_INTERNAL: i32 = -32603;
/// Server-defined — per-session constraints (one active turn).
const ERR_SERVER: i32 = -32000;
/// How long the turn drain waits for the terminal event before answering.
const TERMINAL_DRAIN: Duration = Duration::from_millis(250);
/// How often a cancelled turn re-issues `lane.abort()` until the run has
/// registered and taken it. The first attempt is refused (`no_active_run`)
/// when the cancel beats the turn's first poll — the per-run token exists only
/// from `acquire_run` on — and a refused abort is silently lost, so the retry
/// needs a fixed wakeup source (abb's 100ms tick, design doc §4.4).
const CANCEL_ABORT_TICK: Duration = Duration::from_millis(100);

/// Shared server state: live sessions keyed by ACP id, their factory, and the client
/// capability snapshot `initialize` left behind (one connection = one client).
struct ServerShared {
    live: HashMap<SessionId, LiveSession>,
    factory: SessionFactory,
    /// Whether the client advertised the `notices` session capability
    /// ([`ClientSessionCapabilities::notices`]): user-facing
    /// `SessionUpdate::Notice`s are only sent when this is set (the schema's
    /// own rule). Copied onto each live session at activation.
    client_notices: bool,
}

type SharedServer = Arc<Mutex<ServerShared>>;

/// `ki acp` entry. `serve` runs the server; `install-zed` / `print-zed` are
/// one-shots that return before the serve-only shutdown line. Returns the
/// process exit code: 0 = success or clean client EOF, 1 = runtime failure,
/// 2 = usage error (same values as [`crate::app::EXIT_RUNTIME`] /
/// [`EXIT_USAGE`]).
pub async fn run(argv: &[String]) -> i32 {
    // Best-effort legacy-layout migration (flat `~/.ki` → `~/.ki/agent/`): the
    // parsed CLI path runs it before anything reads credentials/models, but
    // this early-dispatched subcommand bypasses that path — without it, an
    // upgrading user's first `ki acp serve` would resolve against an unmigrated
    // layout (and fail every session/new) until some plain `ki` run happened.
    let _ = crate::config::migrate_legacy_layout();
    match argv.first().map(String::as_str) {
        Some("serve") if argv.len() == 1 => {
            let (code, reason) = match serve().await {
                Ok(()) => (0, "client disconnected"),
                Err(error) => {
                    eprintln!("error: acp server failed: {error}");
                    (EXIT_RUNTIME, "server error")
                }
            };
            diag::shutdown(reason, code);
            code
        }
        // One-shots: stdout/stderr are theirs alone (the serve path keeps
        // stdout protocol-clean), and the closing stderr line from
        // `diag::shutdown` belongs to serve only.
        Some("install-zed") => zed::install_zed(&argv[1..]),
        // Strict like serve: extra arguments are a usage error, never a
        // silently ignored flag behind an exit-0 snippet.
        Some("print-zed") if argv.len() == 1 => {
            println!("{}", zed::Registration::detect().snippet());
            0
        }
        Some("--help") | Some("help") => {
            eprintln!("{USAGE}");
            0
        }
        _ => {
            eprintln!("{USAGE}");
            EXIT_USAGE
        }
    }
}

/// The `ki acp` subcommand list, for --help and every usage error.
const USAGE: &str = "usage: ki acp serve
       ki acp install-zed [--settings <FILE>] [--command <FILE>] [--dry-run]
       ki acp print-zed
       ki acp help";

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
        live: HashMap::new(),
        factory,
        client_notices: false,
    }));

    let shared_init = Arc::clone(&shared);
    let shared_new = Arc::clone(&shared);
    let shared_load = Arc::clone(&shared);
    let shared_prompt = Arc::clone(&shared);
    let shared_cancel = Arc::clone(&shared);
    let shared_close = Arc::clone(&shared);

    let result = Agent
        .builder()
        .name("ki")
        .on_receive_request(
            async move |request: InitializeRequest,
                        responder: Responder<InitializeResponse>,
                        _cx: ConnectionTo<Client>| {
                // ki speaks v1 only, and in this crate build — the workspace
                // does not enable `unstable_protocol_v2` — `ProtocolCompat`
                // is a no-op: nothing validates the client's version or
                // rewrites the response. Answer V1 unconditionally so a
                // non-v1 client sees the downgrade instead of its own version
                // echoed back. Capabilities are honest: `loadSession` is
                // implemented (batch 2); request permission needs no
                // capability flag (client-side baseline — the agent simply
                // sends `session/request_permission`), auth is ki's own
                // config.
                if request.protocol_version != ProtocolVersion::V1 {
                    diag::diagln!(
                        "initialize: client asked protocolVersion {:?}, answering v1",
                        request.protocol_version
                    );
                }
                // Snapshot the notices advertisement (one connection = one
                // client): the boundary drain forwards MCP resource-update
                // notices as user-facing `SessionUpdate::Notice`s only when
                // the client declared it can present them (schema rule:
                // "Agents MUST only send this update when the Client
                // advertised ClientSessionCapabilities::notices").
                let notices = request
                    .client_capabilities
                    .session
                    .as_ref()
                    .is_some_and(|session| session.notices.is_some());
                diag::diagln!("initialize: client notices advertised = {notices}");
                shared_init.lock().unwrap().client_notices = notices;
                responder.respond(
                    InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                        AgentCapabilities::new()
                            .load_session(true)
                            .session_capabilities(
                                SessionCapabilities::new().close(SessionCloseCapabilities::new()),
                            )
                            // Stdio is always served; Streamable HTTP landed in
                            // batch 4. The legacy SSE transport is not
                            // implemented, so it stays false.
                            .mcp_capabilities(McpCapabilities::new().http(true).sse(false)),
                    ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_request: AuthenticateRequest,
                   responder: Responder<AuthenticateResponse>,
                   _cx: ConnectionTo<Client>| {
                // Credentials run through ki's own configuration and no ACP
                // auth methods are advertised, so a well-behaved client never
                // sends this; stray calls are tolerated with an empty ok
                // (design doc §5.2, kiqi-acp precedent) instead of -32601.
                diag::diagln!("authenticate tolerated (ki has no ACP auth)");
                responder.respond(AuthenticateResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_request: LogoutRequest,
                   responder: Responder<LogoutResponse>,
                   _cx: ConnectionTo<Client>| {
                diag::diagln!("logout tolerated (ki has no ACP auth)");
                responder.respond(LogoutResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest,
                        responder: Responder<NewSessionResponse>,
                        cx: ConnectionTo<Client>| {
                handle_session_new(&shared_new, request, responder, cx).await;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: LoadSessionRequest,
                        responder: Responder<LoadSessionResponse>,
                        cx: ConnectionTo<Client>| {
                handle_session_load(&shared_load, request, responder, cx).await;
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
        .on_receive_request(
            async move |request: CloseSessionRequest,
                        responder: Responder<CloseSessionResponse>,
                        _cx: ConnectionTo<Client>| {
                handle_session_close(&shared_close, request, responder).await;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(transport)
        .await;

    // Connection over (clean EOF or error): the crate has already dropped the
    // connection's task actor — and every spawned turn with it — by the time
    // `connect_to` resolves, so there is no in-flight turn left to abort here.
    // Run the SessionShutdown advisory before the reload context (and with it
    // the plugin cdylibs) drops.
    close_live_sessions(&shared).await;
    result
}

/// Dispatches the `SessionShutdown` advisory for every live session. An
/// in-flight turn does not survive to this point: the crate drops the task
/// actor (and the turn future with it) while the connection is closing, and
/// the turn's guard releases the harness run on drop.
async fn close_live_sessions(shared: &SharedServer) {
    let sessions = std::mem::take(&mut shared.lock().unwrap().live);
    for (_, session) in sessions {
        session.close().await;
    }
}

/// 插入新会话而不覆盖同 id 的活跃会话。冲突时把所有权交还调用方执行关闭。
fn insert_live_session(shared: &SharedServer, session: LiveSession) -> Result<(), LiveSession> {
    use std::collections::hash_map::Entry;

    let id = session.id.clone();
    match shared.lock().unwrap().live.entry(id) {
        Entry::Vacant(entry) => {
            entry.insert(session);
            Ok(())
        }
        Entry::Occupied(_) => Err(session),
    }
}

/// `session/close`：先从路由表移除，再取消并等待当前 turn，最后释放资源。
/// 未知或已经关闭的 id 与 prompt 使用同一种 invalid-params 错误形状。
async fn handle_session_close(
    shared: &SharedServer,
    request: CloseSessionRequest,
    responder: Responder<CloseSessionResponse>,
) {
    let session = shared.lock().unwrap().live.remove(&request.session_id);
    match session {
        Some(session) => {
            diag::diagln!("session/close id={}", request.session_id);
            session.close().await;
            let _ = responder.respond(CloseSessionResponse::new());
        }
        None => {
            let message = format!("unknown session {}", request.session_id);
            diag::diagln!("session/close rejected: {message}");
            let _ = responder.respond_with_error(Error::new(ERR_INVALID_PARAMS, message));
        }
    }
}

/// Decodes the client-supplied `mcpServers` (design doc §4.7, batch 3,
/// POLICY 1). The ACP client is the user's own editor, so a server it names is
/// accepted directly — the client's request IS the authorization — and each
/// acceptance is logged loudly on stderr. Stdio and Streamable HTTP are served;
/// the legacy SSE variant is refused with -32602 (ki does not implement it).
/// A name that could not namespace tools, an empty command/url, or two
/// same-named entries are parameter errors, never silent drops (abb lesson 7).
fn decode_client_mcp_servers(
    servers: &[agent_client_protocol::schema::v1::McpServer],
) -> Result<Vec<kiqi_mcp::ServerConfig>, String> {
    use agent_client_protocol::schema::v1::McpServer;
    let mut out = Vec::new();
    for server in servers {
        let (name, transport, acceptance) = match server {
            McpServer::Stdio(stdio) => {
                let command = stdio.command.to_string_lossy().into_owned();
                if command.is_empty() {
                    return Err(format!(
                        "mcpServers: server {} has an empty command",
                        stdio.name
                    ));
                }
                let mut env = std::collections::BTreeMap::new();
                for variable in &stdio.env {
                    env.insert(variable.name.clone(), variable.value.clone());
                }
                (
                    stdio.name.clone(),
                    kiqi_mcp::Transport::Stdio(kiqi_mcp::StdioTransport {
                        command,
                        args: stdio.args.clone(),
                        env,
                        cwd: None,
                    }),
                    format!("{} {}", stdio.command.display(), stdio.args.join(" ")),
                )
            }
            McpServer::Http(http) => {
                let url = http.url.trim();
                if url.is_empty() {
                    return Err(format!("mcpServers: server {} has an empty url", http.name));
                }
                // Refuse before the loud acceptance log below: a URL with
                // embedded credentials must never be printed to stderr, and
                // reqwest would turn it into Basic auth.
                kiqi_mcp::config::validate_http_url(url)
                    .map_err(|error| format!("mcpServers: server {}: {error}", http.name))?;
                let mut headers = std::collections::BTreeMap::new();
                for header in &http.headers {
                    let header_name = header.name.trim();
                    if header_name.is_empty() {
                        return Err(format!(
                            "mcpServers: server {} has an HTTP header with an empty name",
                            http.name
                        ));
                    }
                    headers.insert(header_name.to_string(), header.value.clone());
                }
                (
                    http.name.clone(),
                    kiqi_mcp::Transport::Http(kiqi_mcp::HttpTransport {
                        url: url.to_string(),
                        headers,
                    }),
                    url.to_string(),
                )
            }
            McpServer::Sse(sse) => {
                return Err(format!(
                    "mcpServers: server {} uses the legacy SSE transport ({}); ki speaks Streamable HTTP — use the http variant",
                    sse.name, sse.url
                ));
            }
            // The schema enum is `#[non_exhaustive]`; a future variant is a
            // parameter we cannot honor, not a silent skip.
            _ => {
                return Err("mcpServers: unsupported server transport variant".to_string());
            }
        };
        if !kiqi_mcp::config::valid_server_name(&name) {
            return Err(format!(
                "mcpServers: invalid server name {name:?}; the name must be non-empty ASCII letters, digits, '-' or '_' without the tool separator '__'"
            ));
        }
        if out.iter().any(|s: &kiqi_mcp::ServerConfig| s.name == name) {
            return Err(format!("mcpServers: duplicate server name {name}"));
        }
        // enabled implied (the client named it because it wants it), the
        // default per-call timeout, no config-driven subscriptions or
        // auto-reads (there is no ACP verb for them; both stay host/config
        // driven).
        out.push(kiqi_mcp::ServerConfig {
            name: name.clone(),
            transport,
            enabled: true,
            timeout: std::time::Duration::from_secs(kiqi_mcp::config::DEFAULT_TIMEOUT_SECS),
            subscribe: Vec::new(),
            auto_read: Vec::new(),
            // No ACP verb for subscriptions, so a client-supplied server has
            // no signal source and stays on today's behavior.
            signals: kiqi_mcp::SignalsConfig::default(),
            // The client's server list is not a signal-source opt-in, and the
            // ACP surface has no subscription verb; do not expose the model
            // watch tool for these servers.
            model_watch: false,
        });
        // The loud acceptance record (POLICY 1). Unconditional stderr — the
        // one log line a user can grep when auditing what their editor asked
        // the agent to spawn/contact. stdout stays protocol-clean.
        eprintln!("[ki mcp] accepted client-supplied server {name} ({acceptance})");
    }
    Ok(out)
}

async fn handle_session_new(
    shared: &SharedServer,
    request: NewSessionRequest,
    responder: Responder<NewSessionResponse>,
    cx: ConnectionTo<Client>,
) {
    // Client-supplied MCP servers (batch 3, POLICY 1): stdio entries are
    // accepted (each acceptance logged loudly), anything ki cannot honor is a
    // parameter error — never a silently dropped server.
    let client_mcp_servers = match decode_client_mcp_servers(&request.mcp_servers) {
        Ok(servers) => servers,
        Err(message) => {
            diag::diagln!("session/new rejected: {message}");
            let _ = responder.respond_with_error(Error::new(ERR_INVALID_PARAMS, message));
            return;
        }
    };
    // Same stance: additional workspace roots cannot be honored (the harness
    // and its plugins are rooted at cwd alone), so a multi-root client granting
    // them must hear it now instead of watching tools silently fail on paths it
    // believed were in scope.
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
    let factory = Arc::clone(&shared.lock().unwrap().factory);

    // Design doc §4.7: `systemPrompt` 收下, never silently dropped (abb
    // lesson 7). The pinned v1 schema carries it only through `_meta`.
    let mut params = match system_prompt_params(request.meta.as_ref()) {
        Ok(params) => params,
        Err(message) => {
            diag::diagln!("session/new rejected: {message}");
            let _ = responder.respond_with_error(Error::new(ERR_INVALID_PARAMS, message));
            return;
        }
    };
    params.mcp_servers = client_mcp_servers;

    let cwd = request.cwd;
    diag::diagln!("session/new cwd={}", cwd.display());
    match factory(&cwd, params, sessions::SessionRequest::New).await {
        Ok(deps) => {
            let mut session = LiveSession::new(deps);
            // The client's notices advertisement (initialize) rides on the
            // live session: the boundary drain forwards MCP resource-update
            // notices as user-facing `SessionUpdate::Notice`s only when the
            // client said it can present them.
            session.client_notices = shared.lock().unwrap().client_notices;
            // The dialog pump starts with the session: a plugin can ask a
            // permission on the very first turn, so the mailbox must be live
            // before the session id is ever handed out.
            session.start_dialog_pump(cx);
            let id = session.id.clone();
            match insert_live_session(shared, session) {
                Ok(()) => {
                    diag::diagln!("session/new ok id={id}");
                    let _ = responder.respond(NewSessionResponse::new(id));
                }
                Err(session) => {
                    // 理论上只有会话 id 冲突才会走到这里；不能覆盖仍活跃的旧会话。
                    session.close().await;
                    let message = format!("session {id} is already live");
                    diag::diagln!("session/new rejected: {message}");
                    let _ = responder.respond_with_error(Error::new(ERR_SERVER, message));
                }
            }
        }
        Err(message) => {
            diag::diagln!("session/new failed: {message}");
            let _ = responder.respond_with_error(Error::new(ERR_INTERNAL, message));
        }
    }
}

/// `session/load`: restore a persisted ki session and stream its active
/// branch back as `session/update`s that strictly precede the response (the
/// agent-side obligation: "Stream the entire conversation history back to the
/// client via notifications"; kiqi-acp `protocol.rs` replays the same way under
/// serial dispatch). Runs inline — like `session/new`, the build is the slow
/// part; other sessions' spawned turns continue on the connection's task actor.
async fn handle_session_load(
    shared: &SharedServer,
    request: LoadSessionRequest,
    responder: Responder<LoadSessionResponse>,
    cx: ConnectionTo<Client>,
) {
    let factory = Arc::clone(&shared.lock().unwrap().factory);

    // Client-supplied MCP servers on load: the same decode and the same
    // policy as session/new, word for word — stdio accepted loudly, the rest
    // a parameter error, never a silent drop (abb lesson 7).
    let client_mcp_servers = match decode_client_mcp_servers(&request.mcp_servers) {
        Ok(servers) => servers,
        Err(message) => {
            diag::diagln!("session/load rejected: {message}");
            let _ = responder.respond_with_error(Error::new(ERR_INVALID_PARAMS, message));
            return;
        }
    };
    if !request.additional_directories.is_empty() {
        diag::diagln!(
            "session/load rejected: {} additional_directories entries",
            request.additional_directories.len()
        );
        let _ = responder.respond_with_error(Error::new(
            ERR_INVALID_PARAMS,
            "ki does not accept additional_directories yet; the session is rooted at cwd alone",
        ));
        return;
    }
    // Same first-cut stance as session/new, word for word: `_meta.systemPrompt`
    // is honored ({"append": text}) or loudly refused, so a reloaded session
    // runs the prompt its client configured instead of a silently dropped one
    // (abb lesson 7). The factory applies it on the load path through the same
    // `--append-system-prompt` channel as on a fresh session.
    let mut params = match system_prompt_params(request.meta.as_ref()) {
        Ok(params) => params,
        Err(message) => {
            diag::diagln!("session/load rejected: {message}");
            let _ = responder.respond_with_error(Error::new(ERR_INVALID_PARAMS, message));
            return;
        }
    };
    params.mcp_servers = client_mcp_servers;

    // The ACP id derives from the ki session id ("ki-<uuidv7>"); anything that
    // is not that shape is unknown. Answered BEFORE any factory call so a
    // stray id can never materialize a session file.
    let unknown = || {
        Error::new(
            ERR_INVALID_PARAMS,
            format!("unknown sessionId {}", request.session_id),
        )
    };
    let Some(ki_id) = request.session_id.0.strip_prefix("ki-").map(str::to_owned) else {
        diag::diagln!("session/load rejected: id is not a ki- id");
        let _ = responder.respond_with_error(unknown());
        return;
    };
    if uuid::Uuid::parse_str(&ki_id).is_err() {
        diag::diagln!("session/load rejected: id suffix is not a uuid");
        let _ = responder.respond_with_error(unknown());
        return;
    }
    if shared
        .lock()
        .unwrap()
        .live
        .contains_key(&request.session_id)
    {
        let message = format!("session {} is already live", request.session_id);
        diag::diagln!("session/load rejected: {message}");
        let _ = responder.respond_with_error(Error::new(ERR_SERVER, message));
        return;
    }

    // Restore-only existence pre-check under the request cwd (never
    // ByExactId, which would create the missing session): the request cwd must
    // match the session's cwd, so the sessions of other trees stay invisible.
    let cwd = request.cwd;
    diag::diagln!(
        "session/load id={} cwd={}",
        request.session_id,
        cwd.display()
    );
    let found = match sessions::existing_session_id(&cwd, &ki_id).await {
        Ok(found) => found,
        Err(error) => {
            diag::diagln!("session/load pre-check failed: {error}");
            let _ = responder.respond_with_error(Error::new(ERR_INTERNAL, error));
            return;
        }
    };
    if found.is_none() {
        diag::diagln!(
            "session/load rejected: no session {ki_id} under {}",
            cwd.display()
        );
        let _ = responder.respond_with_error(unknown());
        return;
    }

    match factory(
        &cwd,
        params,
        sessions::SessionRequest::Load {
            id: ki_id.to_owned(),
        },
    )
    .await
    {
        Ok(deps) => {
            let mut session = LiveSession::new(deps);
            // Same notices gate as session/new — a loaded session forwards
            // MCP resource-update notices on the same terms.
            session.client_notices = shared.lock().unwrap().client_notices;
            // A loaded session can ask permissions too: same pump as a fresh
            // one, before the replay and the response hand the id over.
            session.start_dialog_pump(cx.clone());
            // The derived id must round-trip to what the client asked for; a
            // mismatch would leave the live session unpromptable.
            debug_assert_eq!(session.id, request.session_id);
            diag::diagln!("session/load ok id={}", session.id);

            // Replay BEFORE the response, from the built harness's own session
            // face (the factory holds the advisory lock — no second open).
            match replay_updates(&session.harness).await {
                Ok(updates) => match insert_live_session(shared, session) {
                    Ok(()) => {
                        diag::diagln!("session/load replayed {} updates", updates.len());
                        send_updates(&cx, &request.session_id, updates);
                        let _ = responder.respond(LoadSessionResponse::new());
                    }
                    Err(session) => {
                        session.close().await;
                        let message = format!("session {} is already live", request.session_id);
                        diag::diagln!("session/load rejected: {message}");
                        let _ = responder.respond_with_error(Error::new(ERR_SERVER, message));
                    }
                },
                Err(error) => {
                    // Built but unreadable: leaving it in the live map would
                    // expose an id whose history could not be replayed, so answer
                    // the error and run the advisory dispatch (close()) before
                    // the reload context — and the plugin cdylibs — drop.
                    diag::diagln!("session/load replay failed: {error}");
                    session.close().await;
                    let _ = responder.respond_with_error(Error::new(
                        ERR_INTERNAL,
                        format!("could not replay session history: {error}"),
                    ));
                }
            }
        }
        Err(message) => {
            diag::diagln!("session/load failed: {message}");
            let _ = responder.respond_with_error(Error::new(ERR_INTERNAL, message));
        }
    }
}

/// The persisted main-lane branch (leaf toward root, oldest first) projected
/// into replay updates. The lane-scoped view resolves the leaf itself, so a
/// header-only session yields an empty replay; only message entries carry
/// updates ([`projection::replay_entry`]).
async fn replay_updates(
    harness: &kiqi_harness::agent_harness::AgentHarness,
) -> Result<Vec<SessionUpdate>, String> {
    use kiqi_harness::session::types::{BranchBounds, EntryOrder, EntryQuery};

    let entries = harness
        .view()
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(entries.iter().flat_map(projection::replay_entry).collect())
}

/// Decodes `_meta.systemPrompt` (design doc §4.7). The shape ki accepts is the
/// claude-agent-acp preset ABB sends: an object `{"append": <string>}` layering
/// onto whatever base prompt discovery yields. An absent key means no
/// override; any other shape is a parameter error, never a silent drop.
fn system_prompt_params(
    meta: Option<&agent_client_protocol::schema::v1::Meta>,
) -> Result<sessions::SessionParams, String> {
    let Some(value) = meta.and_then(|meta| meta.get("systemPrompt")) else {
        return Ok(sessions::SessionParams::default());
    };
    match value.get("append").and_then(|append| append.as_str()) {
        Some(text) => Ok(sessions::SessionParams {
            system_prompt_append: Some(text.to_string()),
            ..Default::default()
        }),
        None => Err(
            "unsupported _meta.systemPrompt shape; ki accepts {\"append\": <string>}".to_string(),
        ),
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
        match state.live.get_mut(&request.session_id) {
            None => Err((
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
                    session.harness.clone(),
                    session
                        .reload_context
                        .as_ref()
                        .map(|ctx| Arc::clone(&ctx.mcp)),
                    session.client_notices,
                )),
            },
        }
    };
    let (token, lane, events, turn, harness, mcp, client_notices) = match began {
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
    let mut dropped_non_text = 0usize;
    let text: String = request
        .prompt
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            // Image/file/audio/resource blocks have no batch-1 mapping (the
            // harness `prompt_text` call below passes no attachments). Never
            // silently: the model would answer the caption having never seen
            // the payload, invisible to both ends.
            _ => {
                dropped_non_text += 1;
                None
            }
        })
        .collect::<Vec<_>>()
        .join("");
    if dropped_non_text > 0 {
        eprintln!(
            "[ki acp] session/prompt: {dropped_non_text} non-text content block(s) have no batch-1 mapping and were dropped"
        );
    }
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
        harness,
        mcp,
        client_notices,
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
    guard: InFlightGuard,
    harness: AgentHarness,
    mcp: Option<Arc<kiqi_mcp::McpManager>>,
    client_notices: bool,
) -> agent_client_protocol::Result<()> {
    // Between-turn boundary, run-side: notices gathered since the last turn
    // append NOW, before `prompt_text` starts — the run's context build then
    // includes them in THIS turn, instead of a turn later.
    if let Some(mcp) = &mcp {
        forward_mcp_notices(
            &cx,
            &session_id,
            client_notices,
            crate::session::drain_mcp_boundary(&harness, mcp).await,
        );
    }
    // A clone survives for the decision below: a cancel landing in the gap
    // between the flag swap and the guard drop trips this token, and the
    // decision must see it.
    let decision_token = token.clone();
    let prompt = prompt_cancellable(&lane, &text, token);
    tokio::pin!(prompt);

    // `None` disables the drain branch once the broadcast closes.
    let mut events: Option<broadcast::Receiver<AgentEvent>> = Some(events);
    let mut terminal_seen = false;
    let result: HarnessResult<RunResult> = loop {
        tokio::select! {
            biased;
            stepped = async {
                step_events(
                    events.as_mut().expect("drain branch guarded on events.is_some()"),
                    &cx,
                    &session_id,
                )
                .await
            }, if events.is_some() => {
                match stepped {
                    Some(true) => terminal_seen = true,
                    Some(false) => {}
                    None => events = None,
                }
            }
            outcome = &mut prompt => break outcome,
        }
    };

    // The harness resolves the prompt future before its bus delivers the
    // terminal event; drain briefly so the response never overtakes the last
    // update (precedent: `modes.rs` print mode's bounded post-run drain). A
    // harness rejection (`Err`) never started a run, so there is nothing to
    // drain and no reason to tax the response.
    if !terminal_seen && result.is_ok() {
        if let Some(receiver) = events.as_mut() {
            let _ = tokio::time::timeout(TERMINAL_DRAIN, async {
                while let Some(terminal) = step_events(receiver, &cx, &session_id).await {
                    if terminal {
                        break;
                    }
                }
            })
            .await;
        }
    }

    // A client cancel is answered `cancelled` no matter how the turn ended —
    // the protocol mandates it even when the cancellation causes underlying
    // failures. Two windows must both be consulted: the flag (set any time
    // before this line) and the token (a cancel landing in the gap between
    // the flag swap below and the guard drop cancels the token but its flag
    // would be wiped by the next turn's `begin`). After the guard drop a
    // cancel is genuinely the next turn's business: the slot is free and the
    // response is already decided.
    // Turn-end boundary: notices gathered during the run append now — the run
    // is over, so the entry cannot splice between the run's entries and the
    // client's next prompt sees it in its context build.
    if let Some(mcp) = &mcp {
        forward_mcp_notices(
            &cx,
            &session_id,
            client_notices,
            crate::session::drain_mcp_boundary(&harness, mcp).await,
        );
    }
    let was_cancelled = turn.consume_cancelled() || decision_token.is_cancelled();
    // Release the turn slot BEFORE answering: a client that pipelines the next
    // prompt off this response must not hit "a prompt turn is already in
    // flight" for a turn that has already been decided.
    drop(guard);
    match result {
        Ok(run) => match stop_reason(&run.outcome, was_cancelled) {
            Ok(stop_reason) => {
                diag::diagln!("session/prompt id={session_id} done: {stop_reason:?}");
                let _ = responder.respond(PromptResponse::new(stop_reason));
            }
            Err(error) => {
                // A failed run surfaces as the JSON-RPC error it is, never as a
                // fabricated stop reason (abb lesson).
                diag::diagln!("session/prompt id={session_id} failed: {}", error.message);
                let _ = responder.respond_with_error(Error::new(
                    ERR_INTERNAL,
                    format!("run failed: {}", error.message),
                ));
            }
        },
        Err(error) => {
            // The run never started (harness rejection). A client cancel is
            // still answered `cancelled` — the protocol mandates it even when
            // the cancellation causes underlying failures — never a JSON-RPC
            // error.
            if was_cancelled {
                diag::diagln!("session/prompt id={session_id} rejected after cancel");
                let _ = responder.respond(PromptResponse::new(StopReason::Cancelled));
            } else {
                diag::diagln!("session/prompt id={session_id} rejected: {error}");
                let _ = responder.respond_with_error(Error::new(ERR_INTERNAL, error.to_string()));
            }
        }
    }
    Ok(())
}

/// Receives one event from the turn's broadcast stream and projects it.
/// `Some(true)` = terminal event seen, `Some(false)` = non-terminal event,
/// `None` = channel closed. A `Lagged` error is exceptional — the skipped
/// events are silently lost to the client's transcript — so it is always
/// reported to stderr, not only under `KI_ACP_DEBUG`; the surviving buffered
/// events (possibly including the terminal one) stay receivable, so the
/// caller keeps draining rather than ending on it.
async fn step_events(
    receiver: &mut broadcast::Receiver<AgentEvent>,
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
) -> Option<bool> {
    match receiver.recv().await {
        Ok(event) => {
            send_updates(cx, session_id, projection::project(&event));
            Some(event.is_terminal())
        }
        Err(broadcast::error::RecvError::Lagged(skipped)) => {
            eprintln!("[ki acp] event stream lagged; {skipped} updates were dropped");
            Some(false)
        }
        Err(broadcast::error::RecvError::Closed) => None,
    }
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

/// Outcome → response. `Err` = respond with the JSON-RPC error instead (Failed
/// without a client cancel) — the mapping is total, so the caller needs no
/// `unreachable!`. `Suspended` answers `end_turn`: the run parked on a
/// provider deferred handle and ACP has no stop reason for "suspended" —
/// deferred resume is a batch-2+ question, so the why goes to stderr and the
/// turn ends.
fn stop_reason(
    outcome: &HarnessRunOutcome,
    was_cancelled: bool,
) -> Result<StopReason, &kiqi_harness::session::types::OperationError> {
    // A client cancel is answered `cancelled` no matter how the run then
    // ended: the protocol mandates it "even if the cancellation causes
    // exceptions in underlying operations", and abb checks the flag before
    // provider failure — the client's own cancel must never come back as a
    // JSON-RPC error.
    if was_cancelled {
        return Ok(StopReason::Cancelled);
    }
    match outcome {
        HarnessRunOutcome::Failed { error, .. } => Err(error),
        HarnessRunOutcome::Aborted { .. } => Ok(StopReason::Cancelled),
        HarnessRunOutcome::Suspended { .. } => {
            eprintln!(
                "[ki acp] run suspended on a deferred handle; ending the turn (deferred resume is a later batch)"
            );
            Ok(StopReason::EndTurn)
        }
        HarnessRunOutcome::Completed { .. } => Ok(StopReason::EndTurn),
    }
}

/// `session/cancel` runs inline (serial dispatch): arming here cannot race a
/// turn start, because the turn's in-flight claim happens inline in the prompt
/// handler too. Idle cancels are absorbed by [`TurnState::cancel`].
fn handle_cancel(shared: &SharedServer, notification: CancelNotification) {
    let state = shared.lock().unwrap();
    if let Some(session) = state.live.get(&notification.session_id) {
        diag::diagln!("session/cancel id={}", session.id);
        session.turn.cancel();
        // The protocol has the client answer Cancelled for in-flight
        // permission requests after `session/cancel`; on our side the mirror
        // obligation is that the plugin never hangs: release every open
        // dialog in the mailbox and stop waiting on the client for them
        // (aborting a per-dialog task drops its unanswered outbound request,
        // which auto-sends `$/cancel_request`).
        if let Some(dialogs) = &session.dialogs {
            dialogs.cancel_all();
        }
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

/// Forwards the MCP resource-update notices a boundary drain yielded as
/// user-facing `SessionUpdate::Notice`s — but only when the client advertised
/// the `notices` session capability (the schema's own rule; the fallback for a
/// client without it would be an ordinary agent message, and ki's model copy —
/// the batched transcript entry `drain_mcp_boundary` appended — already serves
/// that role, so nothing extra is sent). The Notice is fire-and-forget user
/// information; the model's copy is unchanged, so there is no double delivery.
fn forward_mcp_notices(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    client_notices: bool,
    notices: Vec<kiqi_mcp::McpNotice>,
) {
    if !client_notices || notices.is_empty() {
        return;
    }
    let updates = notices
        .into_iter()
        .map(|notice| {
            // _meta {server, uri}: a client can key on the pair without
            // parsing the description.
            let mut meta = agent_client_protocol::schema::v1::Meta::new();
            meta.insert("server".into(), serde_json::json!(notice.server));
            meta.insert("uri".into(), serde_json::json!(notice.uri));
            SessionUpdate::Notice(
                agent_client_protocol::schema::v1::Notice::new(
                    agent_client_protocol::schema::v1::NoticeSeverity::Info,
                    "Resource updated",
                )
                .description(format!("server {}: {}", notice.server, notice.uri))
                .meta(meta),
            )
        })
        .collect();
    send_updates(cx, session_id, updates);
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
        RequestPermissionResponse, SelectedPermissionOutcome,
    };
    use agent_client_protocol::schema::ProtocolVersion;
    use agent_client_protocol::ByteStreams;
    use kiqi_agent::events::BroadcastEmitter;
    use kiqi_ai::providers::faux::{FauxProvider, FauxScript};
    use kiqi_ai::types::{Api, AssistantMessage, DeferredHandle};
    use kiqi_harness::agent_harness::AgentHarness;
    use kiqi_harness::session::types::OperationError;
    use kiqi_harness::types::AgentHarnessOptions;
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

        assert_eq!(stop_reason(&completed, false), Ok(StopReason::EndTurn));
        // ACP requires `cancelled` after a client cancel, completed-or-not.
        assert_eq!(stop_reason(&completed, true), Ok(StopReason::Cancelled));
        assert_eq!(stop_reason(&aborted, false), Ok(StopReason::Cancelled));
        assert_eq!(stop_reason(&aborted, true), Ok(StopReason::Cancelled));
        assert_eq!(stop_reason(&suspended, false), Ok(StopReason::EndTurn));
        // Failed is never a stop reason on its own — it becomes the JSON-RPC
        // error…
        assert_eq!(
            stop_reason(&failed, false).err().map(|e| e.code.clone()),
            Some("boom".into())
        );
        // …but a client cancel is answered `cancelled` even when the
        // cancellation is what tore the run into a failure (protocol MUST).
        assert_eq!(stop_reason(&failed, true), Ok(StopReason::Cancelled));
    }

    // ---- unit: _meta.systemPrompt parsing (design doc §4.7) ----

    #[test]
    fn system_prompt_params_shapes() {
        use agent_client_protocol::schema::v1::Meta;
        // Absent _meta / absent key → no override.
        assert!(system_prompt_params(None)
            .unwrap()
            .system_prompt_append
            .is_none());
        assert!(system_prompt_params(Some(&Meta::new()))
            .unwrap()
            .system_prompt_append
            .is_none());
        // The claude preset ABB sends: {"append": text}.
        let mut meta = Meta::new();
        meta.insert(
            "systemPrompt".into(),
            serde_json::json!({"append": "be terse"}),
        );
        let params = system_prompt_params(Some(&meta)).unwrap();
        assert_eq!(params.system_prompt_append.as_deref(), Some("be terse"));
        // Any other shape is a loud parameter error, never a silent drop.
        let mut meta = Meta::new();
        meta.insert("systemPrompt".into(), serde_json::json!("bare string"));
        assert!(system_prompt_params(Some(&meta)).is_err());
        let mut meta = Meta::new();
        meta.insert("systemPrompt".into(), serde_json::json!({"preset": "x"}));
        assert!(system_prompt_params(Some(&meta)).is_err());
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
        faux_deps_with_responses(text, tokens_per_second, 1).await
    }

    async fn faux_deps_with_responses(
        text: &str,
        tokens_per_second: f64,
        responses: usize,
    ) -> sessions::SessionDeps {
        let mut script = FauxScript::new();
        for _ in 0..responses {
            script = script.with_text(text);
        }
        let provider = FauxProvider::new(script.with_tokens_per_second(tokens_per_second));
        let (broadcast, event_rx) = BroadcastEmitter::new(256);
        let session = kiqi_harness::session::session::Session::new(
            Arc::new(kiqi_harness::session::memory::InMemorySessionStorage::new(
                kiqi_harness::session::types::SessionMetadata {
                    id: uuid::Uuid::new_v4().to_string(),
                    created_at: 0,
                    parent_session_id: None,
                },
                Arc::new(kiqi_harness::session::memory::SystemClock),
                Arc::new(kiqi_harness::session::session::DefaultIdGenerator::new()),
            )),
            None,
        );
        let options = AgentHarnessOptions {
            model: provider.default_model().clone(),
            models: vec![provider.clone()],
            agent_emitter: Some(Arc::new(broadcast)),
            session,
            ..Default::default()
        };
        let harness = AgentHarness::create(options).await.expect("faux harness");
        sessions::SessionDeps {
            harness,
            event_rx,
            ui_dialog: None,
            reload_context: None,
        }
    }

    fn faux_factory(text: &'static str, tokens_per_second: f64) -> SessionFactory {
        faux_factory_with_responses(text, tokens_per_second, 1)
    }

    fn faux_factory_with_responses(
        text: &'static str,
        tokens_per_second: f64,
        responses: usize,
    ) -> SessionFactory {
        Arc::new(
            move |_cwd, _params: sessions::SessionParams, _request: sessions::SessionRequest| {
                Box::pin(async move {
                    Ok(faux_deps_with_responses(text, tokens_per_second, responses).await)
                })
                    as futures::future::BoxFuture<'static, Result<sessions::SessionDeps, String>>
            },
        )
    }

    /// A faux session that carries a hand-constructed dialog mailbox — the
    /// seam dialog tests use instead of a `session::build`-ed ReloadContext.
    fn dialog_factory(
        text: &'static str,
        tokens_per_second: f64,
        mailbox: kiqi_extensions::UiDialogMailbox,
    ) -> SessionFactory {
        Arc::new(
            move |_cwd, _params: sessions::SessionParams, _request: sessions::SessionRequest| {
                let mailbox = mailbox.clone();
                Box::pin(async move {
                    let mut deps = faux_deps(text, tokens_per_second).await;
                    deps.ui_dialog = Some(mailbox);
                    Ok(deps)
                })
                    as futures::future::BoxFuture<'static, Result<sessions::SessionDeps, String>>
            },
        )
    }

    /// The permission dialog the sdk's own open path accepts (two options, a
    /// question line, a tool call id).
    fn permission_ui() -> serde_json::Value {
        serde_json::json!({
            "purpose": "permission",
            "kind": "confirm",
            "question": "Allow this tool?",
            "options": [
                {"title": "允许一次", "permission": "allow_once"},
                {"title": "拒绝", "permission": "reject_once"},
            ],
        })
    }

    /// Open a dialog through the plugin envelope (`op=open`), exactly as the
    /// runtime action would.
    fn open_dialog(mailbox: &kiqi_extensions::UiDialogMailbox, request_id: &str) {
        mailbox
            .handle(serde_json::json!({
                "op": "open", "requestId": request_id, "toolCallId": "call-1", "ui": permission_ui(),
            }))
            .expect("dialog open");
    }

    /// Poll the mailbox through the plugin envelope until the request reaches
    /// a terminal state (`poll` reclaims terminal states, mirroring the
    /// plugin's own loop).
    async fn wait_for_terminal(
        mailbox: &kiqi_extensions::UiDialogMailbox,
        request_id: &str,
        deadline_ms: u64,
    ) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(deadline_ms);
        loop {
            let poll = mailbox
                .handle(serde_json::json!({ "op": "poll", "requestId": request_id }))
                .expect("poll");
            if poll["status"] != "pending" {
                return poll;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "dialog {request_id} never reached a terminal state"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The next permission request the server sent (or fail loudly).
    async fn next_permission(
        requests: &mut tokio::sync::mpsc::UnboundedReceiver<RequestPermissionRequest>,
    ) -> RequestPermissionRequest {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        match tokio::time::timeout_at(deadline, requests.recv()).await {
            Ok(Some(request)) => request,
            _ => panic!("server never sent session/request_permission"),
        }
    }

    fn selected(option_id: &str) -> RequestPermissionOutcome {
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id.to_string()))
    }

    /// Run the server over an in-memory duplex and drive a typed client
    /// against it. The client's `session/update` notifications go to
    /// `updates_tx`; the server must shut down cleanly after the client leaves.
    /// `wire_log` (when given) records `"update"` per notification at dispatch;
    /// pair it with an `on_receiving_result` callback logging `"response"` —
    /// the crate runs that callback before dispatching later inbound frames,
    /// so an `"update"` logged after `"response"` means the server really did
    /// send the update after (or as) the response: the overtaking the drain
    /// exists to prevent.
    async fn run_client<C, R>(
        factory: SessionFactory,
        updates_tx: tokio::sync::mpsc::UnboundedSender<SessionNotification>,
        wire_log: Option<Arc<std::sync::Mutex<Vec<&'static str>>>>,
        client_main: C,
    ) -> agent_client_protocol::Result<R>
    where
        C: AsyncFnOnce(ConnectionTo<Agent>) -> agent_client_protocol::Result<R> + Send + 'static,
    {
        run_client_hooked(factory, updates_tx, wire_log, None, client_main).await
    }

    /// One scripted decision the test client answers a
    /// `session/request_permission` with, in arrival order.
    #[derive(Clone)]
    enum Decision {
        Answer(RequestPermissionOutcome),
        /// Answer nothing for `window`, then answer Cancelled: the "client
        /// never answers" world, bounded because the client's serial dispatch
        /// loop is parked inside the handler meanwhile.
        Stall(Duration),
        /// Answer nothing; watch — off the dispatch loop, which is serial and
        /// would deadlock on our own handler — for the server's
        /// `$/cancel_request` for this request, bounded by the timeout, then
        /// answer Cancelled. `observed` records whether the wire cancel
        /// arrived in time.
        AwaitPeerCancel {
            timeout: Duration,
            observed: Arc<std::sync::atomic::AtomicBool>,
        },
    }

    /// The test client's face for outbound permission requests: record every
    /// request, answer per script — or park on `release` once the script runs
    /// dry (the "never answers, cancelled from the server side" world).
    #[derive(Clone)]
    struct DialogHook {
        requests_tx: tokio::sync::mpsc::UnboundedSender<RequestPermissionRequest>,
        decisions: Arc<std::sync::Mutex<std::collections::VecDeque<Decision>>>,
        release: tokio_util::sync::CancellationToken,
    }

    /// A hook with an empty script plus the receiver for the requests the
    /// server sent.
    fn dialog_hook(
        script: Vec<Decision>,
    ) -> (
        DialogHook,
        tokio::sync::mpsc::UnboundedReceiver<RequestPermissionRequest>,
    ) {
        let (requests_tx, requests_rx) = tokio::sync::mpsc::unbounded_channel();
        let hook = DialogHook {
            requests_tx,
            decisions: Arc::new(std::sync::Mutex::new(script.into())),
            release: tokio_util::sync::CancellationToken::new(),
        };
        (hook, requests_rx)
    }

    async fn run_client_hooked<C, R>(
        factory: SessionFactory,
        updates_tx: tokio::sync::mpsc::UnboundedSender<SessionNotification>,
        wire_log: Option<Arc<std::sync::Mutex<Vec<&'static str>>>>,
        hook: Option<DialogHook>,
        client_main: C,
    ) -> agent_client_protocol::Result<R>
    where
        C: AsyncFnOnce(ConnectionTo<Agent>) -> agent_client_protocol::Result<R> + Send + 'static,
    {
        let (requests_tx, decisions, release) = match &hook {
            Some(hook) => (
                Some(hook.requests_tx.clone()),
                Some(Arc::clone(&hook.decisions)),
                Some(hook.release.clone()),
            ),
            None => (None, None, None),
        };
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
                    if let Some(log) = &wire_log {
                        log.lock().unwrap().push("update");
                    }
                    let _ = updates_tx.send(notification);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: RequestPermissionRequest,
                            responder: Responder<RequestPermissionResponse>,
                            _cx: ConnectionTo<Agent>| {
                    if let Some(requests_tx) = &requests_tx {
                        let _ = requests_tx.send(request);
                    }
                    let decision = decisions
                        .as_ref()
                        .and_then(|queue| queue.lock().unwrap().pop_front());
                    match decision {
                        Some(Decision::Answer(outcome)) => {
                            responder.respond(RequestPermissionResponse::new(outcome))
                        }
                        Some(Decision::Stall(window)) => {
                            tokio::time::sleep(window).await;
                            responder.respond(RequestPermissionResponse::new(
                                RequestPermissionOutcome::Cancelled,
                            ))
                        }
                        Some(Decision::AwaitPeerCancel { timeout, observed }) => {
                            tokio::spawn(async move {
                                let told = tokio::time::timeout(
                                    timeout,
                                    responder.cancellation().cancelled(),
                                )
                                .await
                                .is_ok();
                                observed.store(told, std::sync::atomic::Ordering::Release);
                                let _ = responder.respond(RequestPermissionResponse::new(
                                    RequestPermissionOutcome::Cancelled,
                                ));
                            });
                            Ok(())
                        }
                        // Script exhausted but a release token exists: hold the
                        // answer until the test lets the connection close.
                        None if release.is_some() => {
                            release.as_ref().unwrap().cancelled().await;
                            responder.respond(RequestPermissionResponse::new(
                                RequestPermissionOutcome::Cancelled,
                            ))
                        }
                        // No hook at all: no permission is expected on this
                        // connection; answering Cancelled keeps a stray one
                        // from hanging the test.
                        None => responder.respond(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Cancelled,
                        )),
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(client_transport, client_main)
            .await;
        // The client transport dropped with its connection; the server sees
        // EOF and must land on the clean teardown path (spawned turns die
        // with the connection's task actor; SessionShutdown advisory runs).
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
        // Order-sensitive capture: the crate runs an `on_receiving_result`
        // callback before dispatching later inbound frames, so an "update"
        // logged after "response" is an update the server really sent at/after
        // the response — the overtaking the post-run drain exists to prevent.
        let wire_log: Arc<std::sync::Mutex<Vec<&'static str>>> = Arc::default();
        let log_updates = Arc::clone(&wire_log);
        let log_response = Arc::clone(&wire_log);
        let result = run_client(
            faux_factory(SCRIPT, 1000.0),
            updates_tx,
            Some(log_updates),
            async move |cx: ConnectionTo<Agent>| {
                let sid = init_and_new(&cx).await?;
                let (tx, rx) = tokio::sync::oneshot::channel();
                let sent = cx.send_request(PromptRequest::new(
                    sid.clone(),
                    vec![ContentBlock::from("hi")],
                ));
                sent.on_receiving_result(async move |result| {
                    if result.is_ok() {
                        log_response.lock().unwrap().push("response");
                    }
                    let _ = tx.send(result);
                    Ok(())
                })?;
                let response = rx
                    .await
                    .expect("prompt response channel")
                    .expect("prompt failed");
                assert_eq!(response.stop_reason, StopReason::EndTurn);
                let text = wait_for_text(updates_rx, SCRIPT).await;
                assert_eq!(text, SCRIPT);
                let order = wire_log.lock().unwrap().clone();
                let response_pos = order
                    .iter()
                    .position(|entry| *entry == "response")
                    .expect("response logged");
                assert!(
                    order[..response_pos].contains(&"update"),
                    "chunks must stream before the response: {order:?}"
                );
                assert!(
                    !order[response_pos + 1..].contains(&"update"),
                    "no chunk may overtake the response: {order:?}"
                );
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn client_eof_mid_turn_shuts_down_cleanly() {
        // The teardown path both code comments lean on: the client vanishes
        // while a turn is still streaming. The spawned drive_turn dies with
        // the connection's task actor (the InFlightGuard with it, releasing
        // the turn slot), and run_client's server join asserts the server
        // still lands on the clean path (SessionShutdown included) instead of
        // hanging or erroring.
        const SCRIPT: &str = "a considerably longer faux script that streams for a while";
        let (updates_tx, mut updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let result = run_client(
            faux_factory(SCRIPT, 10.0),
            updates_tx,
            None,
            async move |cx: ConnectionTo<Agent>| {
                let sid = init_and_new(&cx).await?;
                cx.send_request(PromptRequest::new(sid, vec![ContentBlock::from("go")]))
                    .on_receiving_result(async |_result| Ok(()))?;
                // Leave while the run is visibly mid-stream: the response was
                // never received, the turn is still emitting updates.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
                let mut saw_chunk = false;
                while !saw_chunk {
                    let Ok(Some(notification)) =
                        tokio::time::timeout_at(deadline, updates_rx.recv()).await
                    else {
                        break;
                    };
                    if matches!(notification.update, SessionUpdate::AgentMessageChunk(_)) {
                        saw_chunk = true;
                    }
                }
                assert!(saw_chunk, "run never streamed before the client left");
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
            None,
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
            None,
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
            None,
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
            None,
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
    async fn multiple_live_sessions_route_by_id_and_close_releases_them() {
        let (updates_tx, mut updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let result = run_client(
            faux_factory_with_responses("ok", 1000.0, 2),
            updates_tx,
            None,
            async move |cx: ConnectionTo<Agent>| {
                let first = init_and_new(&cx).await?;
                // 另一个会话仍活跃时，第二次 session/new 必须成功。
                let second = cx
                    .send_request(NewSessionRequest::new(std::env::temp_dir()))
                    .block_task()
                    .await?
                    .session_id;
                assert_ne!(first, second);

                for sid in [&first, &second] {
                    let response = cx
                        .send_request(PromptRequest::new(
                            sid.clone(),
                            vec![ContentBlock::from("hi")],
                        ))
                        .block_task()
                        .await?;
                    assert_eq!(response.stop_reason, StopReason::EndTurn);
                }
                let updates = drain_updates(&mut updates_rx, 2).await;
                assert_eq!(updates[0].session_id, first);
                assert_eq!(updates[1].session_id, second);

                cx.send_request(CloseSessionRequest::new(first.clone()))
                    .block_task()
                    .await?;
                // 已关闭 id 不再路由到剩下的会话，重复 close 也是同一错误形状。
                for attempt in 0..2 {
                    let error = if attempt == 0 {
                        cx.send_request(PromptRequest::new(
                            first.clone(),
                            vec![ContentBlock::from("closed")],
                        ))
                        .block_task()
                        .await
                        .expect_err("closed session prompt must fail")
                    } else {
                        cx.send_request(CloseSessionRequest::new(first.clone()))
                            .block_task()
                            .await
                            .expect_err("closing an unknown id must fail")
                    };
                    assert_eq!(i32::from(error.code), ERR_INVALID_PARAMS);
                    assert!(error.message.contains("unknown session"));
                }

                // close 只释放目标会话，另一个会话及后续新建都不受影响。
                let response = cx
                    .send_request(PromptRequest::new(
                        second,
                        vec![ContentBlock::from("still live")],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(response.stop_reason, StopReason::EndTurn);
                let third = cx
                    .send_request(NewSessionRequest::new(std::env::temp_dir()))
                    .block_task()
                    .await?;
                assert_ne!(third.session_id, first);
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn turns_and_cancellation_are_independent_between_sessions() {
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let result = run_client(
            faux_factory("both sessions can stream independently", 20.0),
            updates_tx,
            None,
            async move |cx: ConnectionTo<Agent>| {
                let first = init_and_new(&cx).await?;
                let second = cx
                    .send_request(NewSessionRequest::new(std::env::temp_dir()))
                    .block_task()
                    .await?
                    .session_id;
                let (first_tx, first_rx) = tokio::sync::oneshot::channel();
                let (second_tx, second_rx) = tokio::sync::oneshot::channel();
                for (sid, tx) in [(first.clone(), first_tx), (second.clone(), second_tx)] {
                    cx.send_request(PromptRequest::new(sid, vec![ContentBlock::from("go")]))
                        .on_receiving_result(async move |result| {
                            let _ = tx.send(result);
                            Ok(())
                        })?;
                }
                cx.send_notification(CancelNotification::new(first))?;
                let first = first_rx
                    .await
                    .expect("first prompt response channel")
                    .expect("first prompt failed");
                let second = second_rx
                    .await
                    .expect("second prompt response channel")
                    .expect("second prompt failed");
                assert_eq!(first.stop_reason, StopReason::Cancelled);
                assert_eq!(second.stop_reason, StopReason::EndTurn);
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn close_cancels_the_turn_before_releasing_the_session() {
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let result = run_client(
            faux_factory(
                "a long response that must be cancelled by session close",
                20.0,
            ),
            updates_tx,
            None,
            async move |cx: ConnectionTo<Agent>| {
                let sid = init_and_new(&cx).await?;
                let (tx, rx) = tokio::sync::oneshot::channel();
                cx.send_request(PromptRequest::new(
                    sid.clone(),
                    vec![ContentBlock::from("go")],
                ))
                .on_receiving_result(async move |result| {
                    let _ = tx.send(result);
                    Ok(())
                })?;
                cx.send_request(CloseSessionRequest::new(sid.clone()))
                    .block_task()
                    .await?;
                let response = rx
                    .await
                    .expect("prompt response channel")
                    .expect("cancelled prompt must still receive a response");
                assert_eq!(response.stop_reason, StopReason::Cancelled);
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }

    #[test]
    fn client_mcp_servers_decode_stdio_and_http_and_refuse_sse() {
        use agent_client_protocol::schema::v1::{
            EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerSse, McpServerStdio,
        };

        let stdio = McpServer::Stdio(
            McpServerStdio::new("docs", "/usr/bin/mcp-docs")
                .args(vec!["--stdio".to_string()])
                .env(vec![EnvVariable::new("TOKEN", "s3cret")]),
        );
        let http = McpServer::Http(
            McpServerHttp::new("web", " https://example.invalid/mcp ")
                .headers(vec![HttpHeader::new("Authorization", "Bearer t0k")]),
        );
        let decoded = decode_client_mcp_servers(&[stdio, http]).expect("stdio + http decode");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].name, "docs");
        assert!(decoded[0].enabled);
        match &decoded[0].transport {
            kiqi_mcp::Transport::Stdio(stdio) => {
                assert_eq!(stdio.command, "/usr/bin/mcp-docs");
                assert_eq!(stdio.args, vec!["--stdio".to_string()]);
                assert_eq!(stdio.env.get("TOKEN").map(String::as_str), Some("s3cret"));
            }
            other => panic!("expected a stdio transport, got {other:?}"),
        }
        // The url is trimmed and headers are carried verbatim; the manager
        // turns a conventional Authorization header into rmcp's bearer token.
        match &decoded[1].transport {
            kiqi_mcp::Transport::Http(http) => {
                assert_eq!(http.url, "https://example.invalid/mcp");
                assert_eq!(
                    http.headers.get("Authorization").map(String::as_str),
                    Some("Bearer t0k")
                );
            }
            other => panic!("expected an http transport, got {other:?}"),
        }

        // The legacy SSE transport stays refused: ki speaks Streamable HTTP.
        let error = decode_client_mcp_servers(&[McpServer::Sse(McpServerSse::new(
            "sse",
            "https://example.invalid/sse",
        ))])
        .expect_err("sse must be refused");
        assert!(error.contains("legacy SSE"), "{error}");
        assert!(error.contains("Streamable HTTP"), "{error}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mcp_servers_are_accepted_and_unknown_sessions_still_fail() {
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let result = run_client(
            faux_factory("ok", 1000.0),
            updates_tx,
            None,
            async move |cx: ConnectionTo<Agent>| {
                // Stdio and Streamable HTTP mcp_servers are ACCEPTED (batch 3,
                // POLICY 1: the ACP client is the user's own editor, so its
                // request is the authorization; HTTP landed in batch 4). The
                // legacy SSE variant stays a loud -32602 (see the decode unit
                // test). The test factory ignores the server list, so what is
                // pinned here is the contract — the request succeeds and a
                // session exists — while the real start path is covered by the
                // kiqi-mcp e2e tests and the decode unit test.
                let accepted = cx
                    .send_request(
                        NewSessionRequest::new(std::env::temp_dir()).mcp_servers(vec![
                            agent_client_protocol::schema::v1::McpServer::Stdio(
                                agent_client_protocol::schema::v1::McpServerStdio::new(
                                    "test",
                                    "/bin/true",
                                ),
                            ),
                            agent_client_protocol::schema::v1::McpServer::Http(
                                agent_client_protocol::schema::v1::McpServerHttp::new(
                                    "web",
                                    "https://example.invalid/mcp",
                                ),
                            ),
                        ]),
                    )
                    .block_task()
                    .await
                    .expect("stdio + http mcp_servers should be accepted (policy 1)");
                assert!(!accepted.session_id.0.is_empty());

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stray_authenticate_and_logout_are_tolerated() {
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let result = run_client(
            faux_factory("ok", 1000.0),
            updates_tx,
            None,
            async move |cx: ConnectionTo<Agent>| {
                // ki advertises no ACP auth methods, so a well-behaved client
                // never sends these — but a stray call must answer {} (design
                // doc §5.2, kiqi-acp precedent), not method-not-found.
                cx.send_request(AuthenticateRequest::new("oki"))
                    .block_task()
                    .await?;
                cx.send_request(LogoutRequest::new()).block_task().await?;
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unsupported_system_prompt_meta_shape_is_rejected() {
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let result = run_client(
            faux_factory("ok", 1000.0),
            updates_tx,
            None,
            async move |cx: ConnectionTo<Agent>| {
                let mut meta = agent_client_protocol::schema::v1::Meta::new();
                meta.insert(
                    "systemPrompt".into(),
                    serde_json::json!("a shape ki does not speak"),
                );
                let rejected = cx
                    .send_request(NewSessionRequest::new(std::env::temp_dir()).meta(meta))
                    .block_task()
                    .await;
                let error = rejected.expect_err("unsupported shape should be refused");
                assert_eq!(i32::from(error.code), ERR_INVALID_PARAMS);
                assert!(error.message.contains("append"));
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }

    // ---- e2e over duplex: session/load ----

    /// A unique per-process scratch dir (parallel test binaries must not share
    /// leftovers).
    fn tempdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ki-acp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// initialize only (no session/new); pins that `loadSession` is advertised.
    async fn init_only(cx: &ConnectionTo<Agent>) -> agent_client_protocol::Result<()> {
        let init = cx
            .send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        assert_eq!(init.protocol_version, ProtocolVersion::V1);
        assert!(
            init.agent_capabilities.load_session,
            "initialize must advertise loadSession once session/load exists"
        );
        assert!(
            init.agent_capabilities.session_capabilities.close.is_some(),
            "initialize must advertise session/close"
        );
        assert!(
            init.agent_capabilities.mcp_capabilities.http,
            "initialize must advertise HTTP MCP support (streamable HTTP landed in batch 4)"
        );
        assert!(
            !init.agent_capabilities.mcp_capabilities.sse,
            "the legacy SSE transport is not implemented and must not be advertised"
        );
        Ok(())
    }

    fn fixture_user(text: &str) -> kiqi_agent::message::AgentMessage {
        kiqi_agent::message::AgentMessage::User(kiqi_ai::types::UserMessage {
            role: kiqi_ai::types::UserRole,
            content: kiqi_ai::types::UserContent::Text(text.to_string()),
            timestamp: 0,
        })
    }

    fn fixture_assistant(
        content: Vec<kiqi_ai::types::Content>,
    ) -> kiqi_agent::message::AgentMessage {
        use kiqi_ai::types::{Api, AssistantMessage};
        let mut message = AssistantMessage::empty(Api::Faux, "faux", "faux-1", 0);
        message.content = content;
        kiqi_agent::message::AgentMessage::Assistant(Box::new(message))
    }

    fn fixture_assistant_text(text: &str) -> kiqi_agent::message::AgentMessage {
        fixture_assistant(vec![kiqi_ai::types::Content::text(text)])
    }

    fn fixture_tool_result(tool_call_id: &str) -> kiqi_agent::message::AgentMessage {
        use kiqi_ai::types::ToolResultMessage;
        kiqi_agent::message::AgentMessage::ToolResult(Box::new(ToolResultMessage {
            role: kiqi_ai::types::ToolResultRole,
            tool_call_id: tool_call_id.to_string(),
            tool_name: "bash".to_string(),
            content: vec![kiqi_ai::types::Content::text("did the thing")],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: 0,
        }))
    }

    /// A two-branch fixture session under `<cwd>/.ki/sessions`: main runs
    /// user → (thought+text+toolcall) → toolResult → "done"; a discarded
    /// sibling branch hangs off the root with "wrong branch". Returns the ki
    /// session id.
    async fn write_fixture_session(cwd: &std::path::Path) -> String {
        use kiqi_harness::session::jsonl::{
            JsonlSessionCreateOptions, JsonlSessionRepo, JsonlSessionRepoOptions,
        };
        use kiqi_harness::session::memory::SystemClock;
        use kiqi_harness::session::session::DefaultIdGenerator;
        use kiqi_harness::session::types::SessionStorage;
        use kiqi_harness::session::Session;
        use kiqi_tools::{FileSystem, OsExecutionEnv};

        let dir = crate::session::default_session_dir(cwd);
        std::fs::create_dir_all(&dir).unwrap();
        let env = Arc::new(OsExecutionEnv::with_cwd(cwd.to_path_buf()));
        let fs: Arc<dyn FileSystem> = env.clone();
        let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
            fs: fs.clone(),
            sessions_root: dir.to_string_lossy().into_owned(),
            clock: Arc::new(SystemClock),
            ids: Arc::new(DefaultIdGenerator::new()),
        });
        let opts = JsonlSessionCreateOptions {
            id: None,
            parent_session_id: None,
            cwd: cwd.to_string_lossy().into_owned(),
            metadata: None,
        };
        let storage = repo.create_typed(&opts).await.expect("fixture create");
        let session = Session::new(Arc::new(storage) as Arc<dyn SessionStorage>, None);
        let id = session.storage().metadata().id;

        let main = session.view("main");
        let root = main
            .append_message(fixture_user("hello"))
            .await
            .expect("root append");
        // Discarded sibling: created on its own lane off the root, so the
        // main-lane branch walk never reaches it.
        session
            .create_lane("side", Some(&root))
            .await
            .expect("side lane");
        session
            .view("side")
            .append_message(fixture_assistant_text("wrong branch"))
            .await
            .expect("discarded append");
        // Active branch: thought → text → tool call, result, final text.
        main.append_message(fixture_assistant(vec![
            kiqi_ai::types::Content::thinking("pondering"),
            kiqi_ai::types::Content::text("plan: "),
            kiqi_ai::types::Content::tool_call("t1", "bash", serde_json::json!({"cmd": "ls"})),
        ]))
        .await
        .expect("turn append");
        main.append_message(fixture_tool_result("t1"))
            .await
            .expect("result append");
        main.append_message(fixture_assistant_text("done"))
            .await
            .expect("done append");
        id
    }

    /// The restore half of the production load path, without `session::build`:
    /// open the fixture's JSONL storage, wrap it in the `Session` facade, and
    /// build the harness with `allow_existing_session` (salvage/rebuild runs
    /// BEFORE any replay reads the tree).
    async fn restored_deps(
        cwd: &std::path::Path,
        id: &str,
    ) -> Result<sessions::SessionDeps, String> {
        use kiqi_harness::session::jsonl::{
            JsonlSessionListOptions, JsonlSessionRepo, JsonlSessionRepoOptions,
        };
        use kiqi_harness::session::memory::SystemClock;
        use kiqi_harness::session::session::DefaultIdGenerator;
        use kiqi_harness::session::types::SessionStorage;
        use kiqi_harness::session::Session;
        use kiqi_tools::{FileSystem, OsExecutionEnv};

        let dir = crate::session::default_session_dir(cwd);
        let env = Arc::new(OsExecutionEnv::with_cwd(cwd.to_path_buf()));
        let fs: Arc<dyn FileSystem> = env.clone();
        let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
            fs: fs.clone(),
            sessions_root: dir.to_string_lossy().into_owned(),
            clock: Arc::new(SystemClock),
            ids: Arc::new(DefaultIdGenerator::new()),
        });
        let metas = repo
            .list_typed(&JsonlSessionListOptions::default())
            .await
            .map_err(|error| error.to_string())?;
        let meta = metas
            .iter()
            .find(|meta| meta.id == id)
            .ok_or_else(|| format!("fixture session {id} vanished"))?;
        let storage = repo
            .open_by_jsonl_metadata(meta)
            .await
            .map_err(|error| error.to_string())?;
        let session = Session::new(Arc::new(storage) as Arc<dyn SessionStorage>, None);

        let provider = FauxProvider::new(FauxScript::new().with_text("post-load"));
        let (broadcast, event_rx) = BroadcastEmitter::new(256);
        let options = AgentHarnessOptions {
            model: provider.default_model().clone(),
            models: vec![provider.clone()],
            agent_emitter: Some(Arc::new(broadcast)),
            allow_existing_session: true,
            session,
            ..Default::default()
        };
        let harness = AgentHarness::create(options)
            .await
            .map_err(|error| error.to_string())?;
        Ok(sessions::SessionDeps {
            harness,
            event_rx,
            ui_dialog: None,
            reload_context: None,
        })
    }

    /// A factory whose every call fails loudly: rejected loads must be
    /// answered before it ever runs.
    fn refusing_factory() -> SessionFactory {
        Arc::new(
            |_cwd: &std::path::Path,
             _params: sessions::SessionParams,
             _request: sessions::SessionRequest| {
                Box::pin(async { Err("factory must not run for a rejected load".to_string()) })
                    as futures::future::BoxFuture<'static, Result<sessions::SessionDeps, String>>
            },
        )
    }

    /// Collect `want` update notifications (or whatever arrives before the
    /// deadline).
    async fn drain_updates(updates: &mut Updates, want: usize) -> Vec<SessionNotification> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut got = Vec::new();
        while got.len() < want {
            let Ok(Some(notification)) = tokio::time::timeout_at(deadline, updates.recv()).await
            else {
                break;
            };
            got.push(notification);
        }
        got
    }

    /// Recursive sorted listing, so "storage unchanged" is checkable.
    fn dir_snapshot(root: &std::path::Path) -> Vec<String> {
        let mut paths = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    paths.push(path.display().to_string());
                }
            }
        }
        paths.sort();
        paths
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn load_streams_the_active_branch_before_the_response() {
        let tmp = tempdir("load-replay");
        let fixture_id = write_fixture_session(&tmp).await;
        let (updates_tx, mut updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let factory_cwd = tmp.clone();
        let request_cwd = tmp.clone();
        let result = run_client(
            Arc::new(
                move |_cwd: &std::path::Path,
                      _params: sessions::SessionParams,
                      request: sessions::SessionRequest| {
                    let cwd = factory_cwd.clone();
                    Box::pin(async move {
                        match request {
                            sessions::SessionRequest::Load { id } => restored_deps(&cwd, &id).await,
                            sessions::SessionRequest::New => {
                                Err("load test: unexpected session/new".to_string())
                            }
                        }
                    })
                        as futures::future::BoxFuture<
                            'static,
                            Result<sessions::SessionDeps, String>,
                        >
                },
            ),
            updates_tx,
            None,
            async move |cx: ConnectionTo<Agent>| {
                init_only(&cx).await?;
                // The ACP id derives from the ki session id.
                let sid = SessionId::from(format!("ki-{fixture_id}"));
                // Updates MUST precede the response (agent obligation); they
                // serialize through the same outgoing actor, so by the time
                // the response resolves they are already buffered client-side.
                let response = cx
                    .send_request(LoadSessionRequest::new(sid, request_cwd))
                    .block_task()
                    .await?;
                assert!(response.modes.is_none(), "ki has no session modes");

                // user chunk, thought, agent text, tool call, completed
                // update, final agent text — strictly the active branch.
                let updates = drain_updates(&mut updates_rx, 6).await;
                assert_eq!(
                    updates.len(),
                    6,
                    "expected exactly the six active-branch updates"
                );
                // Explicit indices: zip's one-ahead pull would swallow the
                // ToolCall at [3] and shift every later assertion.
                for (index, want) in ["hello", "pondering", "plan: "].iter().enumerate() {
                    let (agent_client_protocol::schema::v1::SessionUpdate::UserMessageChunk(chunk)
                    | agent_client_protocol::schema::v1::SessionUpdate::AgentMessageChunk(
                        chunk,
                    )
                    | agent_client_protocol::schema::v1::SessionUpdate::AgentThoughtChunk(
                        chunk,
                    )) = &updates[index].update
                    else {
                        panic!(
                            "expected a chunk at [{index}], got {:?}",
                            updates[index].update
                        );
                    };
                    let agent_client_protocol::schema::v1::ContentBlock::Text(text) =
                        &chunk.content
                    else {
                        panic!("expected text content");
                    };
                    assert_eq!(text.text, *want);
                }
                match &updates[3].update {
                    agent_client_protocol::schema::v1::SessionUpdate::ToolCall(call) => {
                        assert_eq!(call.tool_call_id, "t1".into());
                        assert_eq!(
                            call.kind,
                            agent_client_protocol::schema::v1::ToolKind::Execute
                        );
                    }
                    other => panic!("expected tool call, got {other:?}"),
                }
                match &updates[4].update {
                    agent_client_protocol::schema::v1::SessionUpdate::ToolCallUpdate(update) => {
                        assert_eq!(update.tool_call_id, "t1".into());
                        assert_eq!(
                            update.fields.status,
                            Some(agent_client_protocol::schema::v1::ToolCallStatus::Completed)
                        );
                    }
                    other => panic!("expected tool call update, got {other:?}"),
                }
                // And nothing else: no "wrong branch" replay, no trailing
                // updates from the faux provider (the load path never runs a
                // turn) — the six collected are exactly six.
                tokio::time::sleep(Duration::from_millis(200)).await;
                while let Ok(notification) = updates_rx.try_recv() {
                    panic!("unexpected extra update: {:?}", notification.update);
                }
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn load_rejects_unknown_ids_without_touching_storage_or_the_factory() {
        let tmp = tempdir("load-unknown");
        let sessions_dir = crate::session::default_session_dir(&tmp);
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let before = dir_snapshot(&sessions_dir);
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let request_cwd = tmp.clone();
        let result = run_client(
            refusing_factory(),
            updates_tx,
            None,
            async move |cx: ConnectionTo<Agent>| {
                init_only(&cx).await?;
                // Well-formed but unknown: restore-only semantics answer
                // "unknown sessionId", never create.
                let unknown = SessionId::from(format!("ki-{}", uuid::Uuid::new_v4()));
                let error = cx
                    .send_request(LoadSessionRequest::new(unknown, request_cwd.clone()))
                    .block_task()
                    .await
                    .expect_err("unknown id must fail");
                assert_eq!(i32::from(error.code), ERR_INVALID_PARAMS);
                assert!(error.message.contains("unknown sessionId"));
                // Malformed shapes: no ki- prefix, ki- with a non-uuid tail.
                for bad in ["not-ki", "ki-not-a-uuid"] {
                    let error = cx
                        .send_request(LoadSessionRequest::new(
                            SessionId::from(bad),
                            request_cwd.clone(),
                        ))
                        .block_task()
                        .await
                        .expect_err("malformed id must fail");
                    assert_eq!(i32::from(error.code), ERR_INVALID_PARAMS);
                    let message = error.message;
                    assert!(message.contains("unknown sessionId"), "{message}");
                }
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
        assert_eq!(
            dir_snapshot(&sessions_dir),
            before,
            "a failed load must not create any session file"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn load_while_another_session_is_live_keeps_both_working() {
        let tmp = tempdir("load-live");
        let fixture_id = write_fixture_session(&tmp).await;
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let request_cwd = tmp.clone();
        let factory_cwd = tmp.clone();
        let result = run_client(
            Arc::new(
                move |_cwd: &std::path::Path,
                      _params: sessions::SessionParams,
                      request: sessions::SessionRequest| {
                    let cwd = factory_cwd.clone();
                    Box::pin(async move {
                        match request {
                            sessions::SessionRequest::New => Ok(faux_deps("new", 1000.0).await),
                            sessions::SessionRequest::Load { id } => restored_deps(&cwd, &id).await,
                        }
                    })
                        as futures::future::BoxFuture<
                            'static,
                            Result<sessions::SessionDeps, String>,
                        >
                },
            ),
            updates_tx,
            None,
            async move |cx: ConnectionTo<Agent>| {
                init_only(&cx).await?;
                let live = cx
                    .send_request(NewSessionRequest::new(request_cwd.clone()))
                    .block_task()
                    .await?
                    .session_id;
                let loaded = SessionId::from(format!("ki-{fixture_id}"));
                cx.send_request(LoadSessionRequest::new(loaded.clone(), request_cwd.clone()))
                    .block_task()
                    .await?;
                // 两个会话都按自己的 id 继续工作；加载不再拒绝或替换原会话。
                for sid in [live, loaded] {
                    let response = cx
                        .send_request(PromptRequest::new(sid, vec![ContentBlock::from("hi")]))
                        .block_task()
                        .await?;
                    assert_eq!(response.stop_reason, StopReason::EndTurn);
                }
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn load_accepts_mcp_servers_and_still_rejects_additional_directories() {
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let result = run_client(
            refusing_factory(),
            updates_tx,
            None,
            async move |cx: ConnectionTo<Agent>| {
                init_only(&cx).await?;
                let sid = SessionId::from(format!("ki-{}", uuid::Uuid::new_v4()));
                // mcp_servers on load are accepted now (batch 3, POLICY 1).
                // This fixture stores no session for the id, so the request must
                // fall THROUGH the MCP policy and fail at the existence
                // pre-check instead — pinning that the old policy rejection is
                // gone while unknown ids still fail loudly.
                let error = cx
                    .send_request(
                        LoadSessionRequest::new(sid.clone(), std::env::temp_dir()).mcp_servers(
                            vec![agent_client_protocol::schema::v1::McpServer::Stdio(
                                agent_client_protocol::schema::v1::McpServerStdio::new(
                                    "test",
                                    "/bin/true",
                                ),
                            )],
                        ),
                    )
                    .block_task()
                    .await
                    .expect_err("an unknown session id must still fail");
                assert_eq!(i32::from(error.code), ERR_INVALID_PARAMS);
                assert!(error.message.contains("unknown sessionId"));
                let error = cx
                    .send_request(
                        LoadSessionRequest::new(sid, std::env::temp_dir()).additional_directories(
                            vec![std::env::temp_dir().join("ki-acp-load-additional-root")],
                        ),
                    )
                    .block_task()
                    .await
                    .expect_err("additional_directories on load must be refused");
                assert_eq!(i32::from(error.code), ERR_INVALID_PARAMS);
                assert!(error
                    .message
                    .contains("does not accept additional_directories yet"));
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }

    /// Same first-cut stance as session/new (abb lesson 7): an unsupported
    /// `_meta.systemPrompt` shape is a loud -32602 on load too — never a
    /// silently dropped prompt behind a successful restore.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unsupported_system_prompt_meta_shape_on_load_is_rejected() {
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let result = run_client(
            refusing_factory(),
            updates_tx,
            None,
            async move |cx: ConnectionTo<Agent>| {
                init_only(&cx).await?;
                let mut meta = agent_client_protocol::schema::v1::Meta::new();
                meta.insert(
                    "systemPrompt".into(),
                    serde_json::json!("a shape ki does not speak"),
                );
                let error = cx
                    .send_request(
                        LoadSessionRequest::new(
                            SessionId::from(format!("ki-{}", uuid::Uuid::new_v4())),
                            std::env::temp_dir(),
                        )
                        .meta(meta),
                    )
                    .block_task()
                    .await
                    .expect_err("unsupported meta shape on load must be refused");
                assert_eq!(i32::from(error.code), ERR_INVALID_PARAMS);
                assert!(error.message.contains("append"), "{error}");
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }

    // ---- e2e over duplex: session/request_permission (wave 2) ----

    /// Happy path: a permission dialog rides the live session id with the
    /// blueprint's exact shape (index option ids, original labels, title
    /// fallback chain), the selected option routes back to the plugin as the
    /// original title, and the session keeps prompting.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn permission_round_trip_answers_the_plugin_and_the_turn_continues() {
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let mailbox = kiqi_extensions::UiDialogMailbox::new();
        let (hook, mut requests_rx) = dialog_hook(vec![Decision::Answer(selected("1"))]);
        let result = run_client_hooked(
            dialog_factory("after the dialog", 1000.0, mailbox.clone()),
            updates_tx,
            None,
            Some(hook),
            async move |cx: ConnectionTo<Agent>| {
                let sid = init_and_new(&cx).await?;
                open_dialog(&mailbox, "q1");
                let request = next_permission(&mut requests_rx).await;
                assert_eq!(
                    request.session_id, sid,
                    "the permission rides the live session id"
                );
                assert_eq!(request.tool_call.tool_call_id.to_string(), "call-1");
                assert_eq!(
                    request.tool_call.fields.title.as_deref(),
                    Some("Allow this tool?")
                );
                assert_eq!(request.options.len(), 2);
                assert_eq!(request.options[0].option_id.to_string(), "0");
                assert_eq!(request.options[0].name, "允许一次");
                assert_eq!(request.options[0].kind, PermissionOptionKind::AllowOnce);
                assert_eq!(request.options[1].option_id.to_string(), "1");
                assert_eq!(request.options[1].name, "拒绝");
                assert_eq!(request.options[1].kind, PermissionOptionKind::RejectOnce);
                let answer = wait_for_terminal(&mailbox, "q1", 3000).await;
                assert_eq!(answer["status"], "answered");
                assert_eq!(answer["answer"]["values"][0], "拒绝");
                // The turn continues: the same session still prompts.
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

    /// kiqi-acp race property ①: generic questions (explicit or missing
    /// purpose) are cancelled on their original request ids and never sent to
    /// the client — ACP has no generic question UI, and its choices are never
    /// reinterpreted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn question_purpose_is_cancelled_and_never_relabeled() {
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let mailbox = kiqi_extensions::UiDialogMailbox::new();
        let (hook, mut requests_rx) = dialog_hook(vec![]);
        let result = run_client_hooked(
            dialog_factory("ok", 1000.0, mailbox.clone()),
            updates_tx,
            None,
            Some(hook),
            async move |cx: ConnectionTo<Agent>| {
                let _sid = init_and_new(&cx).await?;
                mailbox
                    .handle(serde_json::json!({
                        "op": "open", "requestId": "q1", "toolCallId": "call-1",
                        "ui": {"purpose": "question", "question": "Pick one",
                               "options": [{"title": "A"}, {"title": "B"}]},
                    }))
                    .expect("open question");
                // And a shape with no purpose at all — the sdk's validate
                // accepts it (the TUI renders questions), the ACP pump must
                // not.
                mailbox
                    .handle(serde_json::json!({
                        "op": "open", "requestId": "q2", "toolCallId": "call-1",
                        "ui": {"kind": "selector", "options": [{"title": "A"}, {"title": "B"}]},
                    }))
                    .expect("open unpurposed");
                let answer = wait_for_terminal(&mailbox, "q1", 3000).await;
                assert_eq!(answer["status"], "cancelled");
                let answer = wait_for_terminal(&mailbox, "q2", 3000).await;
                assert_eq!(answer["status"], "cancelled");
                // Nothing reached the client.
                tokio::time::sleep(Duration::from_millis(200)).await;
                assert!(
                    requests_rx.try_recv().is_err(),
                    "a question must not become session/request_permission"
                );
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }

    /// kiqi-acp race property ② (protocol half): an unknown option id and a
    /// bare Cancelled outcome are both treated as cancelled — never guessed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unknown_option_id_and_client_cancel_are_treated_as_cancelled() {
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let mailbox = kiqi_extensions::UiDialogMailbox::new();
        let (hook, mut requests_rx) = dialog_hook(vec![
            Decision::Answer(selected("999")),
            Decision::Answer(RequestPermissionOutcome::Cancelled),
        ]);
        let result = run_client_hooked(
            dialog_factory("ok", 1000.0, mailbox.clone()),
            updates_tx,
            None,
            Some(hook),
            async move |cx: ConnectionTo<Agent>| {
                let _sid = init_and_new(&cx).await?;
                open_dialog(&mailbox, "q1");
                let _request = next_permission(&mut requests_rx).await;
                let answer = wait_for_terminal(&mailbox, "q1", 3000).await;
                assert_eq!(answer["status"], "cancelled", "out-of-range option id");
                open_dialog(&mailbox, "q2");
                let _request = next_permission(&mut requests_rx).await;
                let answer = wait_for_terminal(&mailbox, "q2", 3000).await;
                assert_eq!(answer["status"], "cancelled", "client Cancelled outcome");
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }

    /// kiqi-acp race property ⑤ plus the hang-risk pin: `session/cancel` while
    /// a permission sits unanswered releases the plugin's poll immediately
    /// (long before any client answer could arrive) and still answers the
    /// prompt `cancelled`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn session_cancel_releases_a_never_answered_permission() {
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let mailbox = kiqi_extensions::UiDialogMailbox::new();
        // The client stalls far longer than the cancel needs: if the poll
        // unblocked only via a client answer, the deadline below would fire.
        let (hook, mut requests_rx) = dialog_hook(vec![Decision::Stall(Duration::from_secs(2))]);
        let result = run_client_hooked(
            dialog_factory("slow enough to cancel", 10.0, mailbox.clone()),
            updates_tx,
            None,
            Some(hook),
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
                // The dialog opens mid-turn, as the plugin's runtime action
                // would; the handler stalls on it.
                open_dialog(&mailbox, "q1");
                let _request = next_permission(&mut requests_rx).await;
                cx.send_notification(CancelNotification::new(sid))?;
                // The release must come from the server side, within a window
                // the stall cannot explain.
                let answer = wait_for_terminal(&mailbox, "q1", 700).await;
                assert_eq!(answer["status"], "cancelled");
                let response = rx
                    .await
                    .expect("prompt response channel")
                    .expect("prompt failed");
                assert_eq!(response.stop_reason, StopReason::Cancelled);
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }

    /// The wire half of the `session/cancel` obligation: releasing the
    /// plugin's poll is not enough — the client's outstanding
    /// `session/request_permission` must be told too. Aborting the per-dialog
    /// task drops its unanswered `SentRequest`, which auto-sends
    /// `$/cancel_request` (the only sender in this flow), so losing that
    /// abort fails exactly here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn session_cancel_tells_the_client_over_the_wire() {
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let mailbox = kiqi_extensions::UiDialogMailbox::new();
        let told = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (hook, mut requests_rx) = dialog_hook(vec![Decision::AwaitPeerCancel {
            timeout: Duration::from_secs(5),
            observed: Arc::clone(&told),
        }]);
        let result = run_client_hooked(
            dialog_factory("slow enough to cancel", 10.0, mailbox.clone()),
            updates_tx,
            None,
            Some(hook),
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
                open_dialog(&mailbox, "q1");
                let _request = next_permission(&mut requests_rx).await;
                cx.send_notification(CancelNotification::new(sid))?;
                // The plugin side releases, and the prompt still answers
                // cancelled (same shape as the release pin above).
                let answer = wait_for_terminal(&mailbox, "q1", 700).await;
                assert_eq!(answer["status"], "cancelled");
                let response = rx
                    .await
                    .expect("prompt response channel")
                    .expect("prompt failed");
                assert_eq!(response.stop_reason, StopReason::Cancelled);
                // The wire observable: the client's cancellation marker for
                // the outstanding permission flips only when the server's
                // `$/cancel_request` arrives for it.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
                while !told.load(std::sync::atomic::Ordering::Acquire) {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "session/cancel left the in-flight request_permission untold: \
                         no $/cancel_request reached the client"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }

    /// EOF with a permission still unanswered: the close path detaches the
    /// mailbox BEFORE the SessionShutdown advisory, so the plugin's poll
    /// cannot outlive the connection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn eof_detaches_the_mailbox_and_cancels_pending_dialogs() {
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let mailbox = kiqi_extensions::UiDialogMailbox::new();
        let (hook, mut requests_rx) = dialog_hook(vec![]);
        let release = hook.release.clone();
        let session_mailbox = mailbox.clone();
        let result = run_client_hooked(
            dialog_factory("ok", 1000.0, mailbox.clone()),
            updates_tx,
            None,
            Some(hook),
            async move |cx: ConnectionTo<Agent>| {
                let _sid = init_and_new(&cx).await?;
                open_dialog(&session_mailbox, "q1");
                let _request = next_permission(&mut requests_rx).await;
                // Walk away with the permission unanswered.
                release.cancel();
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
        // run_client_hooked joins the server task before returning, so the
        // detach has already happened here.
        let answer = wait_for_terminal(&mailbox, "q1", 3000).await;
        assert_eq!(answer["status"], "cancelled");
    }

    /// kiqi-acp race property ③: a dialog closed out from under the pump
    /// neither blocks prompt completion nor leaves anything pending.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn closed_dialog_does_not_block_prompt_nor_leave_a_pending_request() {
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let mailbox = kiqi_extensions::UiDialogMailbox::new();
        let (hook, mut requests_rx) = dialog_hook(vec![]);
        let release = hook.release.clone();
        let result = run_client_hooked(
            dialog_factory("done", 1000.0, mailbox.clone()),
            updates_tx,
            None,
            Some(hook),
            async move |cx: ConnectionTo<Agent>| {
                let sid = init_and_new(&cx).await?;
                open_dialog(&mailbox, "q1");
                let _request = next_permission(&mut requests_rx).await;
                // The dialog closes under the still-waiting task (the
                // TUI-reload/detach semantics).
                mailbox.cancel("q1").expect("cancel q1");
                release.cancel();
                let response = cx
                    .send_request(PromptRequest::new(sid, vec![ContentBlock::from("hi")]))
                    .block_task()
                    .await?;
                assert_eq!(response.stop_reason, StopReason::EndTurn);
                let answer = wait_for_terminal(&mailbox, "q1", 3000).await;
                assert_eq!(answer["status"], "cancelled");
                assert_eq!(mailbox.pending_len(), 0, "nothing may stay pending");
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }

    /// kiqi-acp race property ④: a late answer to a closed dialog lands on the
    /// closed request and can never answer the NEXT dialog — answers route by
    /// the outbound request's own identity, q2 keeps its own choice.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn late_answer_to_a_closed_dialog_cannot_answer_the_next_dialog() {
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let mailbox = kiqi_extensions::UiDialogMailbox::new();
        let (hook, mut requests_rx) = dialog_hook(vec![
            Decision::Answer(selected("0")),
            Decision::Answer(selected("1")),
        ]);
        let result = run_client_hooked(
            dialog_factory("ok", 1000.0, mailbox.clone()),
            updates_tx,
            None,
            Some(hook),
            async move |cx: ConnectionTo<Agent>| {
                let _sid = init_and_new(&cx).await?;
                open_dialog(&mailbox, "q1");
                let _first = next_permission(&mut requests_rx).await;
                // q1 closes; whether the scripted answer lands before or after
                // the close, the terminal state is cancelled either way.
                mailbox.cancel("q1").expect("cancel q1");
                open_dialog(&mailbox, "q2");
                let _second = next_permission(&mut requests_rx).await;
                let answer = wait_for_terminal(&mailbox, "q1", 3000).await;
                assert_eq!(answer["status"], "cancelled");
                let answer = wait_for_terminal(&mailbox, "q2", 3000).await;
                assert_eq!(answer["status"], "answered");
                // q2 was answered with ITS OWN second option — a crossed
                // routing would leave q2 pending (timeout) or mislabelled.
                assert_eq!(answer["answer"]["values"][0], "拒绝");
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
    }

    /// The pump starts at `session/load` too: a restored session can ask
    /// permissions on its first turn.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn load_starts_the_dialog_pump_too() {
        let tmp = tempdir("load-dialogs");
        let fixture_id = write_fixture_session(&tmp).await;
        let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let mailbox = kiqi_extensions::UiDialogMailbox::new();
        let (hook, mut requests_rx) = dialog_hook(vec![Decision::Answer(selected("1"))]);
        let factory_mailbox = mailbox.clone();
        let factory_cwd = tmp.clone();
        let request_cwd = tmp.clone();
        let result = run_client_hooked(
            Arc::new(
                move |_cwd: &std::path::Path,
                      _params: sessions::SessionParams,
                      request: sessions::SessionRequest| {
                    let cwd = factory_cwd.clone();
                    let mailbox = factory_mailbox.clone();
                    Box::pin(async move {
                        match request {
                            sessions::SessionRequest::Load { id } => {
                                let mut deps = restored_deps(&cwd, &id).await?;
                                deps.ui_dialog = Some(mailbox);
                                Ok(deps)
                            }
                            sessions::SessionRequest::New => {
                                Err("load test: unexpected session/new".to_string())
                            }
                        }
                    })
                        as futures::future::BoxFuture<
                            'static,
                            Result<sessions::SessionDeps, String>,
                        >
                },
            ),
            updates_tx,
            None,
            Some(hook),
            async move |cx: ConnectionTo<Agent>| {
                init_only(&cx).await?;
                let sid = SessionId::from(format!("ki-{fixture_id}"));
                cx.send_request(LoadSessionRequest::new(sid, request_cwd))
                    .block_task()
                    .await?;
                open_dialog(&mailbox, "q1");
                let request = next_permission(&mut requests_rx).await;
                assert_eq!(request.options[1].name, "拒绝");
                let answer = wait_for_terminal(&mailbox, "q1", 3000).await;
                assert_eq!(answer["status"], "answered");
                assert_eq!(answer["answer"]["values"][0], "拒绝");
                Ok(())
            },
        )
        .await;
        result.expect("client failed");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
