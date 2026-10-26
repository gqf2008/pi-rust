//! `ki acp` host side: argv dispatch, the production session factory, and the
//! `session/load` existence pre-check — the assembly roots the protocol crate
//! (`kiqi-acp`) deliberately does not know about. Everything protocol-shaped
//! lives there ([`kiqi_acp`]); everything that needs `args` / `config` /
//! `provider` / `session::build` lives here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::BoxFuture;
use kiqi_acp::{RunOptions, SessionDeps, SessionParams, SessionRequest};

use crate::args::Args;

/// `ki acp` entry. Best-effort legacy-layout migration (flat `~/.ki` →
/// `~/.ki/agent/`) first: the parsed CLI path runs it before anything reads
/// credentials/models, but this early-dispatched subcommand bypasses that
/// path — without it, an upgrading user's first `ki acp serve` would resolve
/// against an unmigrated layout (and fail every session/new) until some plain
/// `ki` run happened. Then the protocol crate takes over.
pub async fn run(argv: &[String]) -> i32 {
    let _ = crate::config::migrate_legacy_layout();
    kiqi_acp::run(
        argv,
        RunOptions {
            factory: production_factory(Args::default()),
            load_precheck: load_precheck(),
        },
    )
    .await
}

/// The restore-only `session/load` pre-check: the same dir resolution the
/// factory's `ById` selection uses (`--session-dir`, then the saved
/// `sessionDir` setting, then `<cwd>/.ki/sessions`) plus the repo listing, so
/// an id minted under a configured directory stays loadable by the same serve
/// process. `Ok(None)` rejects the load before the factory runs — a stray id
/// must never materialize a session file.
fn load_precheck() -> kiqi_acp::LoadPrecheck {
    Arc::new(move |cwd: PathBuf, id: String| {
        Box::pin(async move {
            let dir = crate::session::resolve_session_dir(&Args::default(), &cwd);
            let metas =
                crate::session::list_session_metadata_in_dir(&cwd.to_string_lossy(), &dir as &Path)
                    .await
                    .map_err(|error| error.to_string())?;
            Ok(kiqi_acp::match_session_id(&metas, &id))
        }) as BoxFuture<'static, Result<Option<String>, String>>
    })
}

/// The production factory: the full `session::build` pipeline for a
/// `session/new` cwd — model/credential resolution (loud fast-fail, never the
/// unconfigured interactive fallback), chdir (loud — a session rooted at the
/// wrong tree is worse than no session), harness build, the `ownsStdio`
/// hand-off check, the `BeforeTuiStart` veto (loud — fails session/new), and
/// the `SessionStart` advisory (warn-and-continue). Mirrors the headless RPC
/// arm of `app.rs`.
fn production_factory(args: Args) -> kiqi_acp::SessionFactory {
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
    mut args: Args,
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
        kiqi_plugin::EventTag::BeforeTuiStart,
    )
    .await
    {
        eprintln!("[ki] startup vetoed by extension: {reason}");
        crate::session::dispatch_session_shutdown_advisory(&reload_context).await;
        return Err(format!("startup vetoed by extension: {reason}"));
    }
    if let Some(reason) = crate::session::dispatch_session_event_async(
        &reload_context,
        kiqi_plugin::EventTag::SessionStart,
    )
    .await
    {
        // No tracing subscriber is installed anywhere in kiqi-cli, so a
        // `tracing::warn!` here would vanish; §6 sends everything to stderr.
        eprintln!("[ki] extension vetoed SessionStart (headless, advisory): {reason}");
    }
    // The protocol crate never sees the reload context itself — only the two
    // things it drives across the wire: the MCP manager (boundary drain) and
    // the pre-built shutdown future (which owns the context, so the cdylibs
    // outlive the dispatch).
    let shutdown: BoxFuture<'static, ()> = {
        let context = reload_context.clone();
        Box::pin(async move {
            crate::session::dispatch_session_shutdown_advisory(&context).await;
        })
    };
    Ok(SessionDeps {
        harness,
        event_rx,
        ui_dialog: Some(reload_context.ui_dialog.clone()),
        mcp: Some(Arc::clone(&reload_context.mcp)),
        shutdown: Some(shutdown),
    })
}
