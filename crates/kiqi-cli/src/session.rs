//! Harness construction + session-storage wiring. Mirrors the Rust-side
//! equivalent of the TS `packages/coding-agent/src/core/sdk.ts`
//! (`createAgentSession`) — build the env, tools, durable session storage, and
//! `AgentHarnessOptions`, then `AgentHarness::create`.
//!
//! v1 scope cuts vs the TS SDK (tracked in `docs/m6-cli-open-questions.md`):
//! - **Skill / prompt-template / context-file discovery IS wired**
//!   (`--no-skills`/`-ns`, `--no-prompt-templates`/`-np`, `--no-context-files`/
//!   `-nc` each suppress one channel; project `.ki/<sub>` + legacy
//!   `.ki/<sub>` + global
//!   `agent_dir()<sub>` discovery with project-wins dedupe via
//!   [`crate::resource_dirs`]; SYSTEM.md/APPEND_SYSTEM.md project-wins
//!   precedence). **Extension `resources_discover` (B5b) feeds the SAME loaders:
//!   an extension's discovered skill/prompt paths merge with the static dirs and
//!   re-run through `load_skills`/`load_prompt_templates` (individual `.md` files
//!   load too — `load_skills` accepts both dirs and files). Package themes are
//!   parsed by the TUI when selected via settings or `--theme`.**
//!   Project resources load by default without a prompt. `--no-approve` or a
//!   stored negative `trust.json` decision disables them explicitly.
//! - **No `ModelRuntime`/multi-provider registry.** The resolver supports the
//!   built-in Anthropic/OpenAI-compatible providers and `models.json`, but
//!   runtime catalog mutation remains outside this layer.
//! - **Built-in tools**: `read`, `bash`, `edit`, `write`, the read-only
//!   `docs` lookup tool, and the host-owned `permissions` policy tool. The
//!   former kiqi-only `grep`, `find`, `ls`, and `powershell` tools remain
//!   library modules but are not registered by the CLI.
//! - **Session restore (`-c`/`-r`/`--session`)** is *partially* supported: a
//!   fresh session is always created. The harness's `create` rejects sessions
//!   that already have records unless `allow_existing_session` is enabled.
//!   The interactive `-c`/`-r`/`--session` paths enable that mode and replay
//!   the existing branch before appending new messages. See [`SessionSelection`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use kiqi_ai::Provider;
use kiqi_harness::agent_harness::AgentHarness;
use kiqi_harness::context_files::format_project_context;
use kiqi_harness::session::memory::{InMemorySessionStorage, SystemClock};
use kiqi_harness::session::session::DefaultIdGenerator;
use kiqi_harness::session::types::SessionMetadata;
use kiqi_harness::session::Session;
use kiqi_harness::system_prompt::compose_system_prompt;
use kiqi_harness::types::{
    AgentHarnessOptions, AgentHarnessResources, AgentHarnessStreamOptions, DrivingMode,
    HarnessTool, HarnessToolExecution, ToolReplay,
};
use kiqi_tools::{
    create_bash_tool, create_edit_tool, create_read_tool, create_write_tool, ExecutionToolContext,
    MutationQueueRegistry, OsExecutionEnv,
};

#[cfg(target_os = "windows")]
use kiqi_tools::create_powershell_tool;

use crate::args::Args;
use crate::docs_tool::create_docs_tool;
use crate::provider::ResolvedModel;
use crate::resource_dirs::{
    discover_append_system_prompt_file, discover_system_prompt_file, global_prompt_template_dirs,
    global_skill_dirs, load_prompt_templates_with_precedence, load_skills_with_precedence,
    project_prompt_template_dirs, project_skill_dirs, prompt_template_dirs, skill_dirs,
};
use kiqi_host::{emit_resources_discover, ExtensionEmitter, ExtensionSession, TeeEmitter};

/// The coding tools registered by the CLI by default.
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    "read",
    "bash",
    "edit",
    "write",
    "docs",
    "skill",
    "permissions",
    "signals",
];

/// The historical "Available tools:" block, used when the caller cannot name the
/// session's tools (tests, `/debug`, and any future caller that builds the prompt
/// before the tool set is known).
const DEFAULT_TOOLS_BLOCK: &str = "Available tools:
- read  — Read file contents
- bash  — Execute Bash commands using Bash/POSIX syntax, including on Windows
- edit  — Find/replace edits to existing files
- write — Create or overwrite files
- docs  — Look up ki usage, extension, package, and compatibility documentation";

/// One-line description for a tool in the prompt's list.
///
/// Only the built-ins are described here; extension and MCP tools appear by name
/// alone, because ki does not own their wording and a wrong description is worse
/// than none.
fn tool_description(name: &str) -> &'static str {
    match name {
        "read" => "  — Read file contents",
        "bash" => "  — Execute Bash commands using Bash/POSIX syntax, including on Windows",
        "edit" => "  — Find/replace edits to existing files",
        "write" => "  — Create or overwrite files",
        "docs" => "  — Look up ki usage, extension, package, and compatibility documentation",
        _ => "",
    }
}

/// The default coding system prompt. A condensed port of the TS
/// `packages/coding-agent/src/core/system-prompt.ts` base prompt.
pub fn default_system_prompt(cwd: &str, prior_reasoning_replayed: bool) -> String {
    default_system_prompt_for_tools(cwd, prior_reasoning_replayed, None)
}

/// [`default_system_prompt`] with the session's actual tool set.
///
/// The prompt advertises the built-in tools in prose ("Available tools: read …, bash
/// …"). That list was hard-coded, so `--no-tools` / `--no-builtin-tools` removed the
/// tools while the prompt kept promising them — the model then calls a tool it does
/// not have, which surfaces as a raw `Tool … not found` and wasted turns.
///
/// `tools` is `None` when the caller does not know yet (the prompt is also built in
/// tests and for `/debug`), which keeps the historical list; `Some(&[])` renders an
/// explicit "no tools are available" line instead of a list.
pub fn default_system_prompt_for_tools(
    cwd: &str,
    prior_reasoning_replayed: bool,
    tools: Option<&[String]>,
) -> String {
    // The working-state rule must match what actually comes back on the next
    // request. With `compat.requiresThinkingAsText` (or the native thinking
    // channels of anthropic/responses) the model *does* see its own reasoning
    // again, and claiming otherwise makes it re-derive and restate the plan
    // every turn — the re-plan symptom in `docs/llm-repetition-forensics.md`.
    let working_state_rule = if prior_reasoning_replayed {
        "- Keep a SHORT working plan visible (a few bullets, or the todo tool) and update it as you go.
  Your own reasoning is carried back to you on the next turn, so do not re-derive it or restate
  the whole plan: continue from the last unfinished step, and when a step lands, say which one.
  Re-listing the same plan without acting on it is a bug, not progress."
    } else {
        "- Keep your working state in your VISIBLE replies, not only in reasoning. Reasoning is not
  carried into your next turn: only the text you write and the tool output you produce come
  back. Before each batch of tool calls, write one short line naming the task you are on and
  what remains. When you finish a step, say which one is done. If you keep the plan only in
  your head you will re-derive it from scratch every turn."
    };

    // Guidance that names a tool must not appear when that tool is absent: a run with
    // `--no-tools` was still told "the bash tool runs Bash …" and "track work with the
    // todo tool", which is the same lie as advertising the tools themselves.
    let has = |name: &str| match tools {
        None => true,
        Some(names) => names.iter().any(|n| n == name),
    };
    let bash_rule = if has("bash") {
        "- The bash tool runs Bash, even on Windows. Use ls, find, rg, and POSIX pipelines;\n  \
         do not pass PowerShell cmdlets such as Get-ChildItem, Select-Object, or Select-String.\n  \
         If a powershell tool is available, use it for PowerShell commands.\n"
            .to_string()
    } else {
        String::new()
    };
    let todo_rule = if has("todo") {
        "- Track multi-step work with the todo tool instead of re-listing the plan in prose: \n  \
         add the steps once, then mark them done as you go. Re-stating the same plan without \n  \
         acting on it is a bug, not progress.\n"
            .to_string()
    } else {
        String::new()
    };

    // Advertise exactly what the session has. `None` keeps the historical list (a
    // caller that does not know yet); `Some(empty)` says so plainly, because an empty
    // "Available tools:" heading reads as a formatting bug.
    //
    // The resulting block is bigger than the old hard-coded one — measured 2014 B
    // versus ~330 B on a session with the extension and MCP tools installed — and that is
    // the point: the fixed text named five built-ins and silently omitted the other
    // ~39 tools the model could actually call (extensions, codegraph, walgit MCP…).
    // Naming them costs tokens; a model that does not know they exist costs turns.
    // If this ever needs trimming, drop *names* per tool family (or reuse the
    // `<available_skills>` precedent of listing only what routing needs) rather than
    // restoring a list that is wrong.
    let tools_list = match tools {
        None => DEFAULT_TOOLS_BLOCK.to_string(),
        Some(names) if names.is_empty() => {
            "No tools are available in this session: every tool call will fail. \
             Reply with what you would do instead of calling one."
                .to_string()
        }
        Some(names) => {
            let mut block = String::from("Available tools:");
            for name in names {
                let desc = tool_description(name);
                block.push_str(&format!("\n- {name}{desc}"));
            }
            block
        }
    };

    format!(
        "You are an expert coding assistant operating inside kiqi, a coding agent harness. \
You help users by reading files, executing commands, editing code, and writing new files.

{tools_list}

Guidelines:
- Reply in the SAME language the user is writing in. Detect the language of the user's
  latest message (and, when it is too short to be sure, the working language of the
  conversation so far) and use it for your prose replies. Mirror it unless the user
  explicitly asks for a different language; only fall back to English when the
  user's language cannot be determined.
- Be concise in your responses
- Show file paths clearly when working with files
- Prefer the smallest change that solves the problem
{bash_rule}
{working_state_rule}
{todo_rule}
- Read only what you need. If a file or search result was already shown earlier in this
  conversation, use it instead of fetching it again.
- When unsure about ki commands, extensions, Pi package compatibility, or .ki configuration, consult the project documentation before guessing

Current working directory: {cwd}"
    )
}

/// How the user asked to select a session. v1 honors `NoSession` (ephemeral
/// `InMemorySessionStorage`), `New` (a fresh JSONL file), and — new this pass —
/// `Latest` / `ById`, which **restore** an existing JSONL session on launch
/// (`--continue`/`-c`, `--resume`/`-r`, `--session <id|path>`). The restored
/// transcript renders into the TUI on startup and the run continues appending
/// to the same file.
#[derive(Debug, Clone)]
pub enum SessionSelection {
    /// `--no-session`: ephemeral, in-memory, nothing persisted.
    Ephemeral,
    /// Fresh durable JSONL session under `--session-dir` (or the default dir).
    New { dir: PathBuf, name: Option<String> },
    /// `-c` / `-r`: restore the most recent session in the default dir.
    Latest { dir: PathBuf },
    /// `--session <id|path>`: restore the session whose id matches, or whose
    /// file name contains the id.
    ById { id: String, dir: PathBuf },
    /// `--session-id <id>`: use the EXACT session id, creating it if missing.
    ByExactId { id: String, dir: PathBuf },
    /// `--fork <path|id>`: fork the given session into a new one and start in
    /// the fork.
    Fork { source: String },
}

/// The session directory a launch in `cwd` uses: `--session-dir` wins; then
/// the saved `sessionDir` (upstream's key, same format as the flag); then the
/// built-in default. Shared by [`select_session`] and the ACP `session/load`
/// path, so a restore looks in the same directory a fresh session wrote to.
pub fn resolve_session_dir(args: &Args, cwd: &Path) -> PathBuf {
    args.session_dir
        .clone()
        .or_else(|| {
            crate::settings::load_settings()
                .ok()
                .and_then(|settings| settings.session_dir)
        })
        .unwrap_or_else(|| default_session_dir(cwd))
}

/// Decide the session selection from parsed args + the resolved cwd.
pub fn select_session(args: &Args, cwd: &Path) -> SessionSelection {
    if args.no_session {
        return SessionSelection::Ephemeral;
    }
    // Keep this directory on restore selections too, not only on fresh
    // sessions.
    let dir = resolve_session_dir(args, cwd);
    if args.continue_session || args.resume {
        return SessionSelection::Latest { dir };
    }
    if let Some(s) = &args.fork {
        return SessionSelection::Fork { source: s.clone() };
    }
    if let Some(s) = &args.session_id {
        return SessionSelection::ByExactId { id: s.clone(), dir };
    }
    if let Some(s) = &args.session {
        return SessionSelection::ById { id: s.clone(), dir };
    }
    SessionSelection::New {
        dir,
        name: args.name.clone(),
    }
}

/// The default session directory is `<cwd>/.ki/sessions`.
pub fn default_session_dir(cwd: &Path) -> PathBuf {
    cwd.join(".ki").join("sessions")
}

/// Build the `AgentHarness` from the resolved model + parsed args + cwd.
///
/// This is the v1 equivalent of TS `createAgentSession`. It:
/// 1. Builds the `OsExecutionEnv` rooted at `cwd`.
/// 2. Constructs the built-in tools (optionally filtered by `--tools`/
///    `--exclude-tools`/`--no-tools`/`--no-builtin-tools`).
/// 3. Resolves the session storage (ephemeral vs fresh JSONL vs restore-error).
/// 4. Assembles `AgentHarnessOptions` and calls `AgentHarness::create`.
///
/// Returns the harness plus a `broadcast::Receiver<AgentEvent>` carrying the
/// live `AgentEvent` stream from every run (backed by a `BroadcastEmitter`
/// installed on the harness). Interactive mode drains this to render streaming
/// responses; the non-interactive modes simply drop it.
/// Returns the harness (`Arc` — the session owner's strong handle, which the
/// extension action host only references weakly so the capability chain cannot
/// keep the harness alive), the live `AgentEvent` broadcast receiver, and a
/// [`ReloadContext`] the interactive TUI holds to drive `/reload` (and a
/// extension's `runtime_action(Reload)` via the mailbox). Non-interactive modes
/// drop the context (no `/reload` surface in print/json mode).
pub async fn build(
    resolved: &ResolvedModel,
    args: &Args,
    cwd: &Path,
    project_trusted: bool,
) -> Result<
    (
        Arc<AgentHarness>,
        tokio::sync::broadcast::Receiver<kiqi_agent::AgentEvent>,
        ReloadContext,
    ),
    BuildError,
> {
    let selection = select_session(args, cwd);
    build_with_selection(resolved, args, cwd, project_trusted, selection, &[]).await
}

/// [`build`] with an explicit session selection — the ACP `session/load` path
/// restores a specific ki session regardless of the parsed args (which a serve
/// process never parses anyway). `client_mcp_servers` carries the MCP servers
/// an ACP client supplied on `session/new`/`session/load` (`mcpServers`,
/// already decoded and validated by the ACP layer); every other caller passes
/// `&[]` — non-ACP sessions run the settings-configured servers alone.
pub async fn build_with_selection(
    resolved: &ResolvedModel,
    args: &Args,
    cwd: &Path,
    project_trusted: bool,
    selection: SessionSelection,
    client_mcp_servers: &[kiqi_mcp::ServerConfig],
) -> Result<
    (
        Arc<AgentHarness>,
        tokio::sync::broadcast::Receiver<kiqi_agent::AgentEvent>,
        ReloadContext,
    ),
    BuildError,
> {
    let cwd_str = cwd.to_string_lossy().to_string();
    // Extension tools get the session's identity injected into every call's
    // arguments (see `kiqi_host::ToolCallContext`). It is created here,
    // before the tools are built, and completed with the session id below once
    // the session exists — the tool set has to exist before the harness does.
    let tool_context = kiqi_host::ToolCallContext::new(cwd_str.clone());
    // Shared by the `signals` tool, the TUI boundary and `/signals`. Created
    // before the tools (the tool needs it) and given the session below.
    let signal_host = Arc::new(crate::signal_host::SignalHost::new());
    if !project_trusted && args.verbose {
        eprintln!(
            "warning: current-project settings, resources, and discovered extensions are explicitly disabled (use --approve or /trust yes to re-enable)"
        );
    }
    // Pi packages are explicitly opt-in because discovery can start Node and
    // execute package code. An explicit project opt-out limits discovery to
    // global settings.

    // ---- B5a: build the action bridge BEFORE extension load ----
    // Extensions load before `AgentHarness::create` (extensions provide tools the
    // harness is built with), but an extension stores the `ActionBridge`'s raw
    // `user_data` pointer during `register` and it must remain valid + the host
    // must be ready for the whole session. So:
    //  1. Capture the current tokio `Handle` (the async main-thread runtime) —
    //     the bridge spawns dispatch from any thread via `Handle::spawn`.
    //  2. Build an *empty* `HarnessActionHost` (its harness cell is unset; no
    //     extension can call a runtime action before the harness runs).
    //  3. Wrap it as `Arc<dyn RuntimeActionHost>` + `ActionBridge`, thread
    //     `Some(bridge)` into `load_extensions` so every extension's `user_data`
    //     points at this bridge.
    //  4. After `AgentHarness::create` succeeds, call `set_harness(&cell, …)`
    //     with the session owner's `Arc` so the host's **weak** cell upgrades
    //     for the session's lifetime (a strong reference there would close
    //     `harness → snapshot → sink → caps → host → harness`).
    let runtime = tokio::runtime::Handle::try_current().map_err(|e| {
        BuildError::HarnessCreate(format!("no tokio runtime for action bridge: {e}"))
    })?;
    let catalog = crate::provider::available_catalog(resolved);
    let (action_host, harness_cell) = crate::extensions_actions::HarnessActionHost::new_empty(
        catalog.clone(),
        cwd.to_path_buf(),
        runtime.clone(),
        args.unknown_flags.clone(),
        tool_context.clone(),
        Arc::clone(&signal_host),
    );
    let host_arc: Arc<dyn kiqi_host::RuntimeActionHost> = Arc::new(action_host);
    // Kept for the compiled-extension capability adapter: `host_arc` itself is
    // moved into the action bridge below.
    let extension_caps_host = Arc::clone(&host_arc);
    // `runtime` is reused below by the compiled capability adapter, so clone it.
    //
    // B5d: build the initial bridge WITH a reload callback backed by a session-
    // long `ReloadMailbox` (cloned into `ReloadContext` + handed to the TUI). A
    // extension's `runtime_action(Reload)` then signals the TUI's main loop instead
    // of hitting the "not configured" fallback. The same mailbox is reused on
    // `/reload` (the fresh bridge carries `ctx.mailbox`), so the bridge always
    // points at the one TUI-installed sender across reloads.
    let reload_mailbox = kiqi_host::ReloadMailbox::new();
    // Session-scoped UI-dialog mailbox (runtime action 17 / `ask_user`). The
    // initial bridge carries it and the TUI attaches itself once it starts
    // polling; a headless run leaves it detached so `ask_user` fails loudly
    // instead of fabricating a user answer. The SAME mailbox is reused across
    // `/reload` so in-flight prompts + TUI attachment survive an extension swap.
    let ui_dialog_mailbox = kiqi_host::UiDialogMailbox::new();
    // Session-scoped extension status registry (runtime action 18 / `SetStatus`).
    // The bridge lets extensions write; the TUI polls the revision and repaints its
    // footer. Reused across `/reload` so an extension's status survives the swap.
    let ext_status_mailbox = kiqi_host::ExtensionStatusMailbox::new();
    // Session-scoped editor-text queue (runtime action 19 / `SetEditorText`).
    // The bridge lets an extension drop a draft into the prompt editor; the TUI
    // drains it each tick. Reused across `/reload` so a pending transcription
    // survives an extension swap.
    let editor_text_mailbox = kiqi_host::EditorTextMailbox::new();
    // Kept for compiled extensions: their UI capabilities are served by these
    // mailboxes, which the bridge also clones.
    let extension_ui = std::sync::Arc::new(crate::extensions::UiCaps::new(
        ui_dialog_mailbox.clone(),
        ext_status_mailbox.clone(),
        editor_text_mailbox.clone(),
    ));
    let action_bridge = kiqi_host::ActionBridge::with_reload_and_ui(
        runtime.clone(),
        host_arc,
        kiqi_host::reload_callback_from_mailbox(reload_mailbox.clone()),
        ui_dialog_mailbox.clone(),
        ext_status_mailbox.clone(),
        editor_text_mailbox.clone(),
    );

    // ---- Execution env + tools ----
    let env = Arc::new(OsExecutionEnv::with_cwd(cwd.to_path_buf()));
    let env_dyn: Arc<dyn kiqi_tools::ExecutionEnv> = env.clone();
    let mut_env: Arc<dyn kiqi_tools::MutatingEnv> = env.clone();
    let _registry = Arc::new(MutationQueueRegistry::new());
    // `env_dyn` is shared between the tool context (moved in) and the resource
    // loaders below (borrowed); clone one branch so both hold a reference.
    let ctx = ExecutionToolContext::new(env_dyn.clone(), Some(mut_env));

    let skill_cell = crate::skill_tool::new_skill_cell();
    let mut tools = build_tools(&ctx, args, cwd, &signal_host, &skill_cell);

    // ---- Extensions (Part B2) ----
    // Load cdylib extensions from the resolved extension dirs, merge their tools
    // into the built-in set (extension overrides same-named built-in; first-
    // extension-wins across extensions; explicit `--tools`/`--exclude-tools` still
    // apply to the merged set), and keep the loaded `Library` handles alive for
    // the harness lifetime via the returned session guard. `--no-extensions`
    // skips discovery entirely (no dirs scanned, no extensions loaded).
    //
    // P6: the cdylib loader is gone — the extension session is always the
    // empty registry, which keeps the dispatch funnels (compiled/wasm sink)
    // live while carrying no native tools/providers/renderers.
    //
    // The same capabilities serve tools and events; build the sink before the
    // session so the session's snapshot carries it (the first `SessionStart` is
    // still ahead). The sink also lands in the process-global slot as the
    // fallback for call sites without a snapshot; mounting it on the session is
    // what keeps this session's events away from every other session's.
    let extension_caps = std::sync::Arc::new(crate::extensions::owned_caps(
        std::sync::Arc::clone(&extension_caps_host),
        Some(runtime.clone()),
        Some(std::sync::Arc::clone(&extension_ui)),
    ));
    let compiled_sink =
        crate::extensions::install_compiled_event_sink(std::sync::Arc::clone(&extension_caps));
    // The caps ride on the session snapshot alongside the sink: probes,
    // extension commands and prompt transforms resolve them from the current
    // session, so a guest in another ACP session can never act on this one.
    let extension_session = ExtensionSession::none_with_compiled_sink_and_caps(
        Some(compiled_sink),
        Some(std::sync::Arc::clone(&extension_caps) as std::sync::Arc<dyn kiqi_host::SessionCaps>),
    );
    crate::extensions::merge_compiled_extension_tools(
        &mut tools,
        &crate::extensions::compiled_extension_hosts(),
        &extension_caps,
        &|name| tool_name_allowed(name, args),
    );

    // ---- Session storage ----
    // Built before MCP so the identity can be published before any MCP child is
    // spawned: `walgit mcp` inherits KI_AGENT_ID and reports it as
    // `self_session`, which the collab echo suppression compares against.
    let session = build_session(&selection, &cwd_str).await?;
    // Extension tools now know which session they serve: `todo` and friends key
    // their per-session state off it. A session without an id (ephemeral) leaves
    // the field unset and an extension is expected to fall back to project scope.
    if let Ok(metadata) = session.get_metadata().await {
        publish_session_identity(&tool_context, &metadata.id);
    }

    // ---- MCP tools (host-level capability) ----
    // Injected LAST, so an MCP tool can never silently shadow a built-in,
    // extension or extension tool: a name collision is skipped with a warning.
    // `--tools`/`--exclude-tools`/`--no-tools` gate these exactly as they gate
    // every other tool. A server that fails to start is skipped inside the
    // manager (loudly) and simply contributes no tools — one bad server must not
    // stop the session from building.
    //
    // With `--no-tools` the session exposes nothing, so don't spawn servers it
    // could never use. Client-supplied servers landing here are named loudly —
    // the client asked for them and must hear that they did not start.
    let mcp_manager = if args.no_tools {
        for server in client_mcp_servers {
            eprintln!(
                "warning: mcp: client-supplied server {} ignored: the session runs with --no-tools",
                server.name
            );
        }
        Arc::new(kiqi_mcp::McpManager::empty())
    } else {
        start_mcp_manager(cwd, project_trusted, client_mcp_servers).await
    };
    merge_mcp_tools(&mut tools, &mcp_manager, args);

    let active = filter_active_tool_names(active_tool_names(&tools, args), args);

    // The signal tool and the TUI boundary share this host; the durable mute is
    // a session value, so restore it before the first run can decide.
    if let Err(error) = signal_host
        .attach_session_and_load_mute(session.clone())
        .await
    {
        eprintln!("warning: mcp: could not restore the signal mute: {error}");
    }
    // `--name`/`-n` sets the session display name durably (a `name` fact in the
    // log). It applies to whichever session this launch works in — a fresh one
    // or a restored one — and an empty value clears a previous name. This is the
    // only place the flag is applied: `SessionSelection::New` carries it, but
    // builds that do not create a session (restore/fork) must still honor it.
    if let Some(requested) = args.name.as_deref() {
        let name = requested.trim();
        let name = if name.is_empty() { None } else { Some(name) };
        if let Err(error) = session.set_name(name).await {
            if args.verbose {
                eprintln!("warning: could not set session name: {error}");
            }
        }
    }
    // ---- System prompt base (precedence: --system-prompt > SYSTEM.md > default) ----
    // Mirrors pi `discoverSystemPromptFile` (`resource-loader.ts:1022-1034`):
    // an explicit `--system-prompt` flag wins; otherwise a discovered
    // `<cwd>/.ki/SYSTEM.md` wins, then
    // `<agent_dir>/SYSTEM.md`.
    // (global); otherwise the built-in default. **Project-wins** — the same
    // direction as skills/prompts precedence.
    // Whether this model sees its own prior reasoning again decides how the
    // working-state rule is worded (see `default_system_prompt`).
    let prior_reasoning_replayed = kiqi_ai::model::prior_reasoning_is_replayed(&resolved.model);
    let mut context_paths = Vec::new();
    // The prompt must advertise the tools this session actually has: with
    // `--no-tools` / `--no-builtin-tools` the list below would otherwise promise
    // tools the model cannot call.
    let tool_names: Vec<String> = tools.iter().map(|t| t.tool.schema().name.clone()).collect();
    let base_prompt = match args.system_prompt.as_deref() {
        Some(explicit) => explicit.to_string(),
        None if project_trusted => match discover_system_prompt_file(cwd) {
            Some(path) => read_prompt_source(&path, &mut context_paths).unwrap_or_else(|| {
                default_system_prompt_for_tools(
                    &cwd_str,
                    prior_reasoning_replayed,
                    Some(&tool_names),
                )
            }),
            None => default_system_prompt_for_tools(
                &cwd_str,
                prior_reasoning_replayed,
                Some(&tool_names),
            ),
        },
        None => {
            default_system_prompt_for_tools(&cwd_str, prior_reasoning_replayed, Some(&tool_names))
        }
    };

    // ---- Append-text sources (precedence: --append-system-prompt > APPEND_SYSTEM.md) ----
    // Mirrors pi `appendSystemPrompt` (`resource-loader.ts:525-542`). Explicit
    // `--append-system-prompt` flags are joined together; when none are given, a
    // discovered `APPEND_SYSTEM.md` (project-wins over global) provides the
    // append text. `--append-system-prompt` takes a value that may be a literal
    // string OR a readable file path (mirrors TS `resolvePromptInput`).
    let mut append_texts: Vec<String> = Vec::new();
    for extra in &args.append_system_prompt {
        let text = read_append_target(extra, &mut context_paths).unwrap_or_else(|| extra.clone());
        append_texts.push(text);
    }
    if args.append_system_prompt.is_empty() {
        if let Some(path) = project_trusted
            .then(|| discover_append_system_prompt_file(cwd))
            .flatten()
        {
            if let Some(text) = read_prompt_source(&path, &mut context_paths) {
                append_texts.push(text);
            }
        }
    }
    let append_join = if append_texts.is_empty() {
        None
    } else {
        Some(append_texts.join("\n\n"))
    };

    // ---- Resource discovery (skills + prompt-templates + context-files) ----
    // The env is OS-backed, rooted at cwd. Each `--no-*` flag suppresses its
    // channel independently (pi parity). Skills/prompts load project→global,
    // explicit/extension paths, then static packages; dedupe first-wins-by-name
    // keeps project and user resources ahead of packages. Context files walk
    // global→ancestor(cwd→root), deepest-last (pi parity).
    //
    // **Trust gate (v1 divergence):** pi gates project config discovery on
    // `isProjectTrusted()` (global resources are unconditional). ki v1 has no
    // trust prompt — project resources are discovered unconditionally (a copied
    // `.ki/` dir drops in and works). Full trust gating is deferred.
    let agent_dir = crate::config::agent_dir().ok();

    // ---- Package resources (wasm `resources.json` + `resources/`) ----
    // The host verifies each package (kinds, caps, digests, no symlinks) and
    // yields dirs only for granted extensions. They are appended AFTER every
    // static/explicit/discovered path below: the loaders dedupe
    // first-wins-by-name, so a built-in or project resource always wins and a
    // extension cannot shadow it. A collision still surfaces as a
    // `ResourceDiagnostic`. `--no-skills`/`--no-prompt-templates` gate the two
    // channels independently, exactly as they gate the static dirs.
    let extension_resources = agent_dir
        .as_deref()
        .map(crate::extensions::granted_package_resource_dirs)
        .unwrap_or_default();
    for diagnostic in &extension_resources.diagnostics {
        eprintln!("warning: extension resources: {diagnostic}");
    }

    // ---- B5b: extension resources_discover ----
    // If any extension registered a `resources_discover` handler, fan the event out
    // (reason "startup") and collect skill/prompt/theme paths. These extension-
    // contributed paths merge WITH the static Part-A dirs (project `.ki/skills`
    // and `.agents/skills`, `agent_dir/skills` and `~/.agents/skills`, plus any
    // configured dirs — see `resource_dirs::skill_dirs`) and the loaders re-run
    // over the union — the
    // coherence point: an extension's discovered skills land through the SAME loaders
    // as static skills. Static dirs load FIRST so project skills keep winning name
    // collisions (an extension must not shadow a project skill of the same name —
    // mirrors pi `extendResources` running AFTER the default load's first-wins
    // map). `load_skills` now accepts both dirs and individual `.md` files, so a
    // extension returning bare `SKILL.md` paths loads them (the gap this closes).
    // Theme paths are available to the TUI through the package resource list;
    // skill/prompt loaders are the only resources needed by the harness here.
    // A `--no-*` flag suppresses its channel for BOTH static and discovered paths.
    let discovered = extension_session
        .snapshot_arc()
        .map(|snap| emit_resources_discover(&cwd_str, "startup", &snap))
        .unwrap_or_default();

    let mut skills: Vec<kiqi_harness::types::Skill> = Vec::new();
    let mut skill_diags: Vec<kiqi_harness::skills::SkillDiagnostic> = Vec::new();
    let mut resource_diags: Vec<kiqi_harness::diagnostics::ResourceDiagnostic> = Vec::new();
    if !args.no_skills {
        let mut dirs = if args.dev_local_only || args.project_only {
            project_skill_dirs(cwd)
        } else if project_trusted {
            skill_dirs(cwd)
        } else {
            global_skill_dirs()
        };
        dirs.extend(args.skill.iter().cloned());
        dirs.extend(discovered.skill_paths.iter().map(PathBuf::from));
        // LAST: first-wins-by-name means a built-in/project skill wins.
        dirs.extend(extension_resources.skills.iter().cloned());
        let result = load_skills_with_precedence(&env_dyn, &dirs).await;
        skills = result.skills;
        skill_diags = result.diagnostics;
        resource_diags.extend(result.resource_diagnostics);
        // The `skill` tool reads this cell, so a reload's fresh set is visible
        // without rebuilding the tool.
        if let Ok(mut cell) = skill_cell.write() {
            *cell = skills.clone();
        }
    }

    let mut prompt_templates: Vec<kiqi_harness::types::PromptTemplate> = Vec::new();
    let mut prompt_diags: Vec<kiqi_harness::prompt_templates::PromptTemplateDiagnostic> =
        Vec::new();
    if !args.no_prompt_templates {
        let mut dirs = if args.dev_local_only || args.project_only {
            project_prompt_template_dirs(cwd)
        } else if project_trusted {
            prompt_template_dirs(cwd)
        } else {
            global_prompt_template_dirs()
        };
        dirs.extend(args.prompt_template.iter().cloned());
        dirs.extend(discovered.prompt_paths.iter().map(PathBuf::from));
        // LAST: first-wins-by-name means a built-in/project prompt wins.
        dirs.extend(extension_resources.prompts.iter().cloned());
        let result = load_prompt_templates_with_precedence(&env_dyn, &dirs).await;
        prompt_templates = result.prompt_templates;
        prompt_diags = result.diagnostics;
        resource_diags.extend(result.resource_diagnostics);
    }

    let context_block = if args.no_context_files || !project_trusted {
        String::new()
    } else {
        // `load_project_context_files` walks the global agentDir first then
        // ancestor-walks cwd→root (deepest last). It needs a real agent_dir; if
        // none is resolvable, pass the cwd dir so only the ancestor-walk runs
        // (the global step returns None anyway).
        let agent_dir_path = agent_dir.clone().unwrap_or_else(|| cwd.to_path_buf());
        // `~/.agents/AGENTS.md` is the user's cross-agent standing instruction; it
        // loads before the ki-specific agent dir (most general first).
        let extra_globals: Vec<std::path::PathBuf> =
            crate::config::agents_dir().into_iter().collect();
        let files = kiqi_harness::context_files::load_project_context_files_with_globals(
            &env_dyn,
            cwd,
            &agent_dir_path,
            &extra_globals,
        )
        .await;
        context_paths.extend(
            files
                .iter()
                .map(|file| file.path.to_string_lossy().into_owned()),
        );
        format_project_context(&files)
    };

    // An extension cannot shadow a built-in, but the attempt is worth saying out
    // loud: these collisions are not behind `--verbose` like the rest.
    for diagnostic in &resource_diags {
        if let kiqi_harness::diagnostics::ResourceDiagnostic::Collision(collision) = diagnostic {
            if extension_resources.contains_path(&collision.loser_path) {
                eprintln!(
                    "warning: {} name \"{}\" collision: keeping {} (shadowed {})",
                    collision.resource_type.as_str(),
                    collision.name,
                    collision.winner_path,
                    collision.loser_path
                );
            }
        }
    }

    // Surface resource-discovery diagnostics as startup warnings (verbose-only).
    if args.verbose {
        for d in &skill_diags {
            eprintln!(
                "warning: skill {} ({}): {}",
                d.path,
                d.code.as_str(),
                d.message
            );
        }
        for d in &prompt_diags {
            eprintln!(
                "warning: prompt template {} ({}): {}",
                d.path,
                d.code.as_str(),
                d.message
            );
        }
        // Structured collisions: make the winner/loser explicit. Extension
        // collisions were already printed above (visible without `--verbose`).
        for d in &resource_diags {
            if let kiqi_harness::diagnostics::ResourceDiagnostic::Collision(c) = d {
                if extension_resources.contains_path(&c.loser_path) {
                    continue;
                }
                eprintln!(
                    "warning: {} name \"{}\" collision: keeping {} (shadowed {})",
                    c.resource_type.as_str(),
                    c.name,
                    c.winner_path,
                    c.loser_path
                );
            }
        }
    }

    // ---- Compose the full system prompt ----
    // Order mirrors pi `buildSystemPrompt` (`system-prompt.ts:28-72`):
    // base → append → context → skills. The skills listing is the harness's own
    // section: `AgentHarness::compose_prompt` appends `<available_skills>` (gated
    // on the `read` tool + `disable_model_invocation`, applied inside
    // `format_skills_for_system_prompt`). So we pass None for skills here (the
    // harness adds the listing itself) and fold only base+append+context into
    // the prompt we hand the harness.
    let system_prompt = compose_system_prompt(
        Some(&base_prompt),
        &[], // skills: harness appends the listing itself
        if context_block.is_empty() {
            None
        } else {
            Some(&context_block)
        },
        append_join.as_deref(),
    );

    // ---- Debug: dump the resolved system-prompt sections (verification) ----
    // A verification affordance for Part-A resource discovery: prints the
    // composed sections + resource counts to stderr so a smoke can confirm
    // `<available_skills>` + `<project_context>` + appended text reached the
    // prompt without parsing a provider round-trip. The harness composes the
    // final prompt (base → append → context → skills); here we print the
    // pre-harness sections (the harness adds the skills listing itself, gated
    // on `read` + `disable_model_invocation`).
    if args.debug_system_prompt {
        eprintln!("=== --debug-system-prompt ===");
        let base_src = if args.system_prompt.is_some() {
            "--system-prompt"
        } else if discover_system_prompt_file(cwd).is_some() {
            "SYSTEM.md"
        } else {
            "default"
        };
        eprintln!("[base source: {base_src}]");
        eprintln!("--- base ---\n{base_prompt}");
        if let Some(append) = append_join.as_deref() {
            eprintln!("--- append ---\n{append}");
        } else {
            eprintln!("--- append: (none) ---");
        }
        if context_block.is_empty() {
            eprintln!("--- context: (none) ---");
        } else {
            eprintln!("--- context ---{context_block}");
        }
        let visible_skills = skills
            .iter()
            .filter(|s| s.disable_model_invocation != Some(true))
            .count();
        eprintln!(
            "--- skills: {} loaded ({} model-visible, {} hidden) ---",
            skills.len(),
            visible_skills,
            skills.len() - visible_skills
        );
        for s in &skills {
            let hidden = if s.disable_model_invocation == Some(true) {
                " [hidden]"
            } else {
                ""
            };
            eprintln!("    {}{hidden} — {}", s.name, s.description);
        }
        eprintln!("--- prompt templates: {} ---", prompt_templates.len());
        for t in &prompt_templates {
            eprintln!("    /{}", t.name);
        }
        // B5b: surface extension-contributed discovery paths so a smoke can confirm
        // the resources_discover round-trip fed the loaders; package themes are
        // selected by the TUI rather than injected into the harness prompt.
        eprintln!(
            "--- discovered via resources_discover: {} skill(s), {} prompt(s), {} theme(s) ---",
            discovered.skill_paths.len(),
            discovered.prompt_paths.len(),
            discovered.theme_paths.len(),
        );
        for p in &discovered.skill_paths {
            eprintln!("    skill: {p}");
        }
        for p in &discovered.prompt_paths {
            eprintln!("    prompt: {p}");
        }
        eprintln!(
            "--- final composed base+append+context (skills listing added by harness) ---\n{system_prompt}"
        );
        eprintln!("=== end --debug-system-prompt ===");
    }

    // ---- Options ----
    // Install a BroadcastEmitter so the caller (the interactive TUI) can drain
    // AgentEvents live as a run unfolds. The corresponding broadcast::Receiver
    // is returned alongside the harness; non-interactive modes simply drop it.
    let (broadcast, event_rx) = kiqi_agent::events::BroadcastEmitter::new(256);
    let broadcast_emitter: Arc<dyn kiqi_agent::AgentEmitter> = Arc::new(broadcast);
    // The broadcast half stays live for the whole session (the TUI's drain task
    // holds the receiver); reload re-wraps it in a fresh `TeeEmitter`, so keep
    // a clone for the `ReloadContext` before the tee match consumes the original.
    let broadcast_for_context: Arc<dyn kiqi_agent::AgentEmitter> = Arc::clone(&broadcast_emitter);

    // ---- Extensions emitter (Part B3a) ----
    // Wrap the broadcast emitter in a `TeeEmitter` so every `AgentEvent` flows
    // to BOTH the TUI (via the broadcast receiver above) AND the extensions (via
    // the `ExtensionEmitter`, which translates each `AgentEvent` into the serde
    // `Event` and fans it into the compiled/wasm sink).
    let emitter: Arc<dyn kiqi_agent::AgentEmitter> = Arc::new(TeeEmitter::new(vec![
        broadcast_emitter,
        Arc::new(ExtensionEmitter::for_session(&extension_session)),
    ]));

    // Saved user defaults. Best-effort: a missing or malformed file must not stop
    // a session from starting, and every accessor falls back to the harness
    // default when its key is absent.
    let saved_settings = crate::settings::load_settings().unwrap_or_default();

    // `httpProxy` applies to ki's own HTTP clients. Setting the environment is how
    // they are configured (`pi-ai/src/http.rs`), and upstream does the same thing
    // with this key — an explicit environment variable still wins, so a shell-level
    // proxy override is not silently replaced by the setting.
    if let Some(proxy) = &saved_settings.http_proxy {
        for key in ["HTTP_PROXY", "HTTPS_PROXY"] {
            if std::env::var_os(key).is_none() {
                std::env::set_var(key, proxy);
            }
        }
    }

    // Share the live session cell with both /reload and the approval hook.
    // Install the hook even when no extensions loaded yet: a later reload can
    // discover the first permission gate.
    let extension_session_cell = Arc::new(Mutex::new(extension_session.clone()));
    let policy_guard = crate::permissions::PolicyGuard::new(cwd.to_path_buf());
    let before_tool_call = Some(policy_guard.guarded_before(extension_tool_call_hook(
        cwd.to_path_buf(),
        project_trusted,
        std::sync::Arc::clone(&extension_session_cell),
    )));

    let options = AgentHarnessOptions {
        model: resolved.model.clone(),
        thinking_level: resolved.thinking_level,
        active_tool_names: active,
        tools,
        system_prompt: Some(system_prompt),
        resources: AgentHarnessResources {
            context_files: context_paths,
            skills: if skills.is_empty() {
                None
            } else {
                Some(skills)
            },
            prompt_templates: if prompt_templates.is_empty() {
                None
            } else {
                Some(prompt_templates)
            },
        },
        // A restored session (--continue/--resume/--session) already has
        // records — let the harness load it and keep appending.
        allow_existing_session: matches!(
            selection,
            SessionSelection::Latest { .. }
                | SessionSelection::ById { .. }
                | SessionSelection::ByExactId { .. }
                | SessionSelection::Fork { .. }
        ),
        stream_options: AgentHarnessStreamOptions {
            timeout: args.timeout,
            ..Default::default()
        },
        // Settings-backed harness options: upstream lets users tune retry,
        // compaction and queue drain modes from settings.json. Global only,
        // matching how `defaultTools` and the other settings-backed options in
        // this file are read.
        retry: saved_settings.retry_policy(),
        compaction: saved_settings.compaction_settings(),
        steering_mode: saved_settings.steering_mode(),
        follow_up_mode: saved_settings.follow_up_mode(),
        tool_execution: HarnessToolExecution::default(),
        drive: DrivingMode::default(),
        session,
        // The resolved providers. Wasm extension providers are hosted by the
        // wasm backend's provider host, not injected here.
        models: resolved.providers.clone(),
        to_provider_messages: None,
        entry_projectors: Default::default(),
        agent_emitter: Some(emitter),
        // B3b loop hooks. `before_tool_call` dispatches the pi `on("tool_call")`
        // extension event and honors a veto; the policy guard wraps it and
        // snapshots the policy files around shell calls. `after_tool_call`
        // restores a policy changed by a shell command and flags the result —
        // the lexical guard cannot cover every spelling. `transform_context`
        // remains `None`.
        before_tool_call,
        after_tool_call: Some(policy_guard.after_hook()),
        transform_context: None,
        entry_transforms: Vec::new(),
        // Extension provider hooks (B4): extensions subscribing to the
        // BeforeProviderRequest / BeforeProviderHeaders / AfterProviderResponse
        // events observe every provider call (observer semantics — the handler
        // ABI has no patch channel in v1). The compiled extensions' prompt
        // transforms are chained onto the same slot; both are skipped cleanly
        // when nothing subscribes.
        provider_hooks: Some(crate::extensions::compiled_prompt_hooks(
            kiqi_host::ExtensionProviderHooks::from_session(&extension_session)
                .map(|h| Arc::new(h) as Arc<dyn kiqi_ai::ProviderHooks>),
            std::sync::Arc::clone(&extension_caps) as std::sync::Arc<dyn kiqi_host::SessionCaps>,
        )),
    };

    // The `Arc` is the **session owner's** handle: it is returned to the
    // caller (and, for ACP, moved into `LiveSession`), so the weak cell the
    // action host reads stays live for exactly as long as the session does.
    // Never seed the cell from a temporary clone-backed `Arc`: it would drop at
    // the end of this block and every runtime action would answer "not ready".
    let harness = match AgentHarness::create(options).await {
        Ok(h) => Arc::new(h),
        Err(e) => return Err(BuildError::HarnessCreate(e.to_string())),
    };
    // Fill the extension action host now that the harness exists (extension
    // runtime_action calls can then reach it). The host stores a Weak, so this
    // does not close the harness → snapshot → sink → caps → host cycle.
    crate::extensions_actions::HarnessActionHost::set_harness(&harness_cell, &harness);

    // ---- Durable operation recovery (runtime layer) ----
    // A previous process can die between `operation_started` and
    // `operation_finished`, leaving the lane with an orphaned operation. Sweep
    // every lane, surface what was found, and emit `run_start`/`run_end`
    // (aborted) pairs so a TUI that already attached sees the settle. This is
    // read-only: nothing is mutated here, so a failed sweep cannot corrupt the
    // log. A clean session (the common case) reports no findings.
    {
        let runtime = kiqi_harness::runtime::SessionRuntime::new(harness.session().clone());
        match kiqi_harness::runtime::recover_all_lanes(&runtime).await {
            Ok(reports) => {
                for (lane, report) in reports {
                    if report.is_clean() {
                        continue;
                    }
                    if report.is_corrupt() {
                        eprintln!(
                            "warning: session lane '{lane}' is corrupt ({} open operations); \
                             refusing to auto-repair — inspect the session file",
                            report.open_operations.len()
                        );
                        continue;
                    }
                    for decision in runtime.reconcile(&report) {
                        if let kiqi_harness::runtime::RecoveryDecision::Abort { run_id } = decision
                        {
                            if args.verbose {
                                eprintln!(
                                    "recovered interrupted run {run_id} on lane '{lane}' (aborted)"
                                );
                            }
                        }
                    }
                    runtime.emit_recovery_events(harness.events(), &report);
                }
            }
            Err(e) => {
                if args.verbose {
                    eprintln!("warning: session recovery sweep failed: {e}");
                }
            }
        }
    }

    // ---- B5d: assemble the ReloadContext the TUI holds ----
    // Every field is cheap to clone (Arc / Vec / args Clone). The cells own the
    // live session + bridge so `/reload` can swap them; the harness itself is
    // NOT held here (the TUI already owns a `&AgentHarness` / clone at the call
    // site — passing it into `reload_extension_resources` keeps this structfree
    // of a harness back-reference so it can be `Clone` into the reload callback).
    let reload_context = ReloadContext {
        extension_session: extension_session_cell,
        skill_cell: Arc::clone(&skill_cell),
        action_bridge: Arc::new(Mutex::new(Some(Arc::clone(&action_bridge)))),
        tool_context,
        catalog,
        providers: resolved.providers.clone(),
        runtime: runtime.clone(),
        cwd: cwd.to_path_buf(),
        project_trusted,
        args: args.clone(),
        resolved_model: resolved.model.clone(),
        broadcast: broadcast_for_context,
        mailbox: reload_mailbox,
        ui_dialog: ui_dialog_mailbox,
        ext_status: ext_status_mailbox,
        editor_text: editor_text_mailbox,
        mcp: Arc::clone(&mcp_manager),
        signal_host: Arc::clone(&signal_host),
    };

    Ok((harness, event_rx, reload_context))
}

/// Host policy + the compiled/wasm extension gate for tool calls.
fn extension_tool_call_hook(
    cwd: PathBuf,
    project_trusted: bool,
    extension_session: crate::session::ExtensionSessionCell,
) -> kiqi_agent::BeforeToolCall {
    Arc::new(move |ctx: kiqi_agent::BeforeToolCallContext<'_>, _signal| {
        let tool_call_id = ctx.tool_call.id.clone();
        let tool_name = ctx.tool_call.name.clone();
        let params = ctx.args.clone();
        let policy_cwd = cwd.clone();
        let session = std::sync::Arc::clone(&extension_session);
        Box::pin(async move {
            // Host policy runs before any extension. A compiled or cdylib extension
            // must not be able to widen the host's rules, and a local deny
            // needs no approval dialog. The file is re-read per call, so
            // `/reload` and an external edit take effect without a cache. A
            // project-local policy is only honored when the project is trusted,
            // matching the settings/resource/extension gates.
            if let Some(reason) = crate::permissions::blocking_reason_for(
                &policy_cwd,
                project_trusted,
                &tool_name,
                &params,
            ) {
                return Some(kiqi_agent::BeforeToolCallResult {
                    block: true,
                    reason: Some(reason),
                    terminate: false,
                    args: None,
                });
            }
            // Compiled extensions' gate runs next: it is local and synchronous, so
            // a veto arrives without opening an approval dialog. A rewrite is
            // carried into the native dispatch so neither path sees stale args.
            let compiled = crate::extensions::probe_tool_call_in_session(
                &session,
                &tool_call_id,
                &tool_name,
                &params,
            );
            let replacement = match compiled {
                Some(kiqi_host::Verdict::Block { reason }) => {
                    return Some(kiqi_agent::BeforeToolCallResult {
                        block: true,
                        reason: Some(reason),
                        terminate: false,
                        args: None,
                    })
                }
                Some(kiqi_host::Verdict::Replace(args)) => Some(args),
                _ => None,
            };
            let effective = replacement.clone().unwrap_or_else(|| params.clone());
            // An extension rewrite is applied after the host's first evaluation, so
            // re-run the host policy on the arguments that will actually be
            // executed. Otherwise a `Replace` could turn a call the policy
            // denies into an allowed one (e.g. rewrite a `git push` command
            // into `git status`, which the deny rule does not match).
            if replacement.is_some() {
                if let Some(reason) = crate::permissions::blocking_reason_for(
                    &policy_cwd,
                    project_trusted,
                    &tool_name,
                    &effective,
                ) {
                    return Some(kiqi_agent::BeforeToolCallResult {
                        block: true,
                        reason: Some(format!("{reason} (after an extension argument rewrite)")),
                        terminate: false,
                        args: None,
                    });
                }
            }
            // A rewrite with no veto still has to reach the runtime.
            replacement.map(|args| kiqi_agent::BeforeToolCallResult {
                block: false,
                reason: None,
                terminate: false,
                args: Some(args),
            })
        })
    })
}

// ===========================================================================
// B5d — `/reload`: re-run extension + resource discovery into a LIVE harness
// ===========================================================================
//
// `/reload` (interactive TUI command, or an extension's `runtime_action(Reload)`)
// re-runs everything `build` did around resources/extensions WITHOUT rebuilding
// the `AgentHarness` itself (rebuilding would tear down the session/lane/event
// wiring + the broadcast drain task the TUI owns). Instead it:
//
//  1. Builds a fresh `ExtensionSession` (re-load the cdylibs) over the same
//     dir set, with a FRESH `ActionBridge` (the old one is `invalidate`d so
//     in-flight extension→host calls on the old bridge fail fast).
//  2. Fans `resources_discover(_, "reload")` over the fresh snapshot.
//  3. Re-runs the Part-A loaders (skills/prompts/context/SYSTEM.md/
//     APPEND_SYSTEM.md) with the discovered paths merged in — same precedence
//     + `--no-*` gates as startup.
//  4. Rebuilds the harness's live state via the B5d setters
//     (`set_system_prompt`/`set_resources`/`set_agent_emitter`/`set_models`/
//     `set_provider_hooks`/`set_tools`) so the NEXT run observes the reloaded
//     config (in-flight runs finish on the old `ConfigSnapshot`).
//  5. Swaps the cells (`ExtensionSession`, `ActionBridge`, harness action
//     host's harness cell stays — the harness is the same object) and drops
//     the old session + bridge (their keepalives unmap the old cdylibs; the
//     new session's keepalive holds the fresh mappings).
//
// The reload is a `kiqi-cli` concern (NOT a harness op): `kiqi-extensions`
// carries only the `ActionBridge` staleness flag + a `ReloadMailbox` `()` signal
// (no pi-cli `TuiMessage` type — leaf DAG preserved). The TUI owns the mailbox
// receiver + the actual reload routine; an extension's
// `runtime_action(Reload)` signals the mailbox and returns `Ok(null)`
// immediately so the calling extension's cdylib is NOT unmapped while its
// `runtime_action` frame is still on the stack (the self-unmapping race a
// synchronous extension-initiated reload would have).
//
// `reload_extension_resources` is the shared routine both `/reload` (TUI) and
// an extension's `runtime_action(Reload)` (via the mailbox) drive. It is `pub` so
// the TUI's main-loop handler + the mailbox-driven path call the same code.

/// The cell that holds the live `ExtensionSession` across a `/reload`. Cloned
/// into every site that needs the current session (the TUI, the reload
/// callback). On reload the old session is `replace`d out (its `active` flag
/// flipped) and the fresh one stored atomically. In-flight callbacks retain
/// the old keepalive until they return; callers never observe an empty
/// placeholder during the swap.
pub type ExtensionSessionCell = Arc<Mutex<ExtensionSession>>;

/// The cell that holds the live `ActionBridge` across a `/reload`. An extension
/// stores the bridge's raw `user_data` pointer during `register`; on reload the
/// old bridge is `invalidate`d (in-flight calls fail fast) and the fresh one
/// `store`d. The fresh session's extensions are handed the fresh bridge pointer.
pub type ActionBridgeCell = Arc<Mutex<Option<Arc<kiqi_host::ActionBridge>>>>;

/// Everything `/reload` needs to rebuild extension + resource state into a live
/// harness. Built once in [`build`] (alongside the harness) and held by the TUI
/// (cloned into the reload callback the bridge carries + the `/reload` command
/// handler). The harness itself is NOT held here — the TUI already owns a
/// `&AgentHarness` / a clone; passing it at the call site keeps this struct
/// free of a harness back-reference (so it can be `Clone` and moved into the
/// reload callback without borrowing the harness).
#[derive(Clone)]
pub struct ReloadContext {
    /// The live extension-session cell (swapped on reload).
    pub extension_session: ExtensionSessionCell,
    /// The session's discovered skills, shared with the `skill` tool. `/reload`
    /// rewrites this cell in place so the tool sees the new set.
    pub skill_cell: Arc<std::sync::RwLock<Vec<kiqi_harness::types::Skill>>>,
    /// The session's extension-tool context (`cwd` + session id), reused — not
    /// rebuilt — on reload so the rebuilt adapters keep serving the SAME
    /// session. A reload that made a fresh context would leave every extension
    /// tool without a session id until the next launch.
    pub tool_context: kiqi_host::ToolCallContext,
    /// The live action-bridge cell (swapped + old invalidated on reload).
    pub action_bridge: ActionBridgeCell,
    /// The extension status registry the TUI renders in its footer (`SetStatus`).
    /// Lives in the context (not the cell) so it survives a bridge swap.
    pub ext_status: kiqi_host::ExtensionStatusMailbox,
    /// The extension editor-text queue the TUI drains into the prompt editor
    /// (`SetEditorText`, runtime action 19). Lives in the context (not the cell)
    /// so it survives a bridge swap — exactly like `ext_status`. A voice extension
    /// writes a transcription here as a *draft*; the TUI applies it, starts the
    /// optional auto-send countdown, and submits through the normal path so the
    /// user bubble renders as usual.
    pub editor_text: kiqi_host::EditorTextMailbox,
    /// The model catalog (read-only) the host uses to resolve `set_model(id)`.
    /// `available_catalog(resolved)` is captured once — reload does not re-resolve
    /// the provider (auth/provider resolution is a startup concern; reloading
    /// extensions does not re-open auth).
    pub catalog: Vec<kiqi_ai::Model>,
    /// The resolved provider runtimes (for rebuilding `models`). Cheap to
    /// clone (`Arc`).
    pub providers: Vec<Arc<dyn Provider>>,
    /// The ambient runtime handle (captured in `build`) — `ActionCaps` and the
    /// fresh `ActionBridge` need a captured `Handle` to spawn from any thread.
    pub runtime: tokio::runtime::Handle,
    /// The cwd (for static resource-dir resolution + context-file walk).
    pub cwd: PathBuf,
    /// The project-trust decision captured before provider and session setup.
    /// Startup consumers reuse this value so extension code cannot change the
    /// effective policy by mutating the process cwd while it is loading.
    pub project_trusted: bool,
    /// The parsed args (cloned) — `--no-*`/`--tools`/`--exclude-tools`/
    /// `--extensions-dir`/`--no-extensions`/`--system-prompt`/etc all apply on
    /// reload exactly as at startup (a reload re-reads the same flags; it does
    /// not pick up argv changes mid-session, which is the right contract — pi's
    /// `/reload` re-runs discovery with the same config).
    pub args: Args,
    /// The resolved model + thinking level (the harness's active model stays
    /// unless `set_model` changed it; reload does not touch the model).
    pub resolved_model: kiqi_ai::Model,
    /// The broadcast emitter the harness was built with. Reload rebuilds the
    /// `TeeEmitter` over the fresh `ExtensionEmitter` (the old tee's extension
    /// child is dropped, unsubscribing from the old registry). The broadcast
    /// half stays live the whole session (the TUI's drain task holds the
    /// receiver), so we keep a handle to re-wrap.
    pub broadcast: Arc<dyn kiqi_agent::AgentEmitter>,
    /// The session-long reload mailbox (B5d). Build creates one, installs it on
    /// the initial `ActionBridge` via [`reload_callback_from_mailbox`], and hands
    /// a clone to the TUI. The TUI installs its `TuiMessage` sender so an extension's
    /// `runtime_action(Reload)` signals the main loop — the reload routine reuses
    /// THIS mailbox (not a fresh default) when building the fresh bridge, so the
    /// bridge always carries the mailbox the TUI installed across reloads.
    pub mailbox: kiqi_host::ReloadMailbox,
    /// Session-long UI-dialog mailbox (runtime action 17). The initial bridge
    /// carries it; the TUI attaches itself and polls `take_pending` from its key
    /// loop, and `/reload` reuses THIS mailbox when it builds the fresh bridge.
    /// A detached mailbox makes `ask_user` fail with "requires an interactive
    /// UI" (headless contract).
    pub ui_dialog: kiqi_host::UiDialogMailbox,
    /// The session's host-level MCP servers. Held here so every teardown path
    /// (via [`dispatch_session_shutdown_advisory`]) reaps their process trees,
    /// and so the `Drop` of the last builder is a backstop. The tool adapters
    /// also hold `Arc`s into it, which is why the field is an `Arc`.
    pub mcp: Arc<kiqi_mcp::McpManager>,
    /// Shared MCP-signal state: the wake budget, the durable session mute, and
    /// the attention inbox. Created before the tools (the `signals` tool holds
    /// it) and shared with the TUI boundary and `/signals`.
    pub(crate) signal_host: Arc<crate::signal_host::SignalHost>,
}

/// Dispatch a no-payload lifecycle event (e.g. `SessionStart`, `SessionShutdown`,
/// the kiqi-specific `BeforeTuiStart`, `SessionTree`) to the compiled/wasm sink.
///
/// Lifecycle events are observer-only: the extension contract's `EventAction`
/// has no veto, so this always returns `None`. The signature is kept because
/// every run shape funnels its shutdown through here.
pub async fn dispatch_session_event_async(
    reload_context: &ReloadContext,
    tag: kiqi_host::EventTag,
) -> Option<String> {
    // Use THIS session's sink (never the process-global one) so ACP's concurrent
    // sessions do not receive each other's lifecycle events.
    let snapshot = reload_context
        .extension_session
        .lock()
        .unwrap()
        .snapshot_arc();
    kiqi_host::dispatch_lifecycle_event_with(snapshot.as_deref(), tag).await
}

/// Notify extensions the session moved to a different point of its tree — `/tree`
/// navigation, `/session` switch, `/import`, `/fork`, `/clone`.
///
/// **No payload**, observer-only: the move has already happened, so there is
/// nothing an extension could veto. Delivery is the same funnel as every
/// lifecycle event (the compiled/wasm sink), so a wasm todo/plan-mode guest
/// restores from the swapped-in branch.
///
/// Callers fire this only AFTER the move completed (`navigate_tree` /
/// `set_active_session` returned), so a handler reading
/// [`kiqi_host::SessionRead::session_branch`] observes the NEW branch.
pub(crate) async fn notify_session_tree(reload_context: &ReloadContext) {
    if let Some(reason) =
        dispatch_session_event_async(reload_context, kiqi_host::EventTag::SessionTree).await
    {
        tracing::warn!("[ki] extension vetoed SessionTree (advisory): {reason}");
    }
}

/// Dispatch `SessionShutdown` (observer-only: the session is already closing).
/// Shared by every run shape that ends a session (interactive TUI/REPL, rpc,
/// print, json) so the last extension signal lives in one place.
pub async fn dispatch_session_shutdown_advisory(reload_context: &ReloadContext) {
    // Take the raced notices BEFORE shutdown: `McpManager::shutdown` purges
    // every queued notice (`clear_subscriptions` runs before the connection
    // closes, and once the subscription map is cleared the update path can no
    // longer repopulate), so a flush after it would be empty by construction.
    // The notices have nowhere else to go — the transcript is closed — so
    // surface them on stderr instead of dropping them silently (a print-mode
    // notice would otherwise sit forever).
    flush_mcp_notices_to_stderr(&reload_context.mcp);
    // MCP servers are ki's own child processes: reap them first so no teardown
    // path can leave a process tree behind, independent of what an extension does
    // below. Idempotent; the `Drop` chain is the backstop.
    reload_context.mcp.shutdown().await;
    if let Some(reason) =
        dispatch_session_event_async(reload_context, kiqi_host::EventTag::SessionShutdown).await
    {
        tracing::warn!("[ki] extension vetoed SessionShutdown (ignored, closing): {reason}");
    }
}

/// Join extension teardown before unloading modules. A detached timed-out handler
/// could still execute code in the old DLL, so it cannot safely permit a swap.
async fn shutdown_extensions_for_reload(
    snapshot: Option<Arc<kiqi_host::RegistrySnapshot>>,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        // The reload dispatches through the session being torn down, so an
        // observer here is the old session's, not another session's.
        kiqi_host::dispatch_empty_event_with(
            snapshot.as_deref(),
            kiqi_host::EventTag::SessionShutdown,
        );
    })
    .await
    .map_err(|error| error.to_string())
}

/// The outcome of a reload: a human-readable status line for the transcript
/// (counts of what reloaded), and whether any load diagnostics appeared.
pub struct ReloadOutcome {
    /// One-line summary for the transcript note (e.g. "Reloaded 5 skill(s),
    /// 1 prompt(s).").
    pub summary: String,
    /// True iff at least one extension load warning fired (ABI mismatch / skip).
    pub had_warnings: bool,
    /// Formatted diagnostic lines (package / skill / prompt-template warnings)
    /// that the caller should surface in its UI. Kept out of `eprintln!`
    /// because the TUI owns the terminal: direct stderr writes during a run
    /// would corrupt the alternate-screen input row.
    pub details: Vec<String>,
    /// True iff the reload itself failed hard (e.g. the extension build or
    /// settings reload errored) and the previous resources were kept. Soft
    /// diagnostics (skill shadowing, name validation) do not set this.
    pub had_errors: bool,
}

struct PreparedReloadInputs {
    skill_base_dirs: Vec<PathBuf>,
    prompt_base_dirs: Vec<PathBuf>,
}

/// Append the resource sources that are specific to a reload after the
/// conventional project/global directories. Keep this order aligned with the
/// initial build: explicit CLI paths must remain available after `/reload`,
/// while discovered and package resources retain their lower precedence.
fn append_reload_resource_paths(
    mut paths: Vec<PathBuf>,
    explicit: &[PathBuf],
    discovered: &[String],
    js_paths: &[PathBuf],
    package_paths: &[PathBuf],
) -> Vec<PathBuf> {
    paths.extend(explicit.iter().cloned());
    paths.extend(discovered.iter().map(PathBuf::from));
    paths.extend(js_paths.iter().cloned());
    paths.extend(package_paths.iter().cloned());
    paths
}

/// Re-run extension + resource discovery and push the rebuilt state into the
/// live `harness` via the B5d setters. The old `ExtensionSession` +
/// `ActionBridge` are invalidated + swapped in [`ReloadContext`]'s cells. This
/// is the single routine both `/reload` (TUI) and an extension's
/// `runtime_action(Reload)` drive (the latter via the mailbox signal).
///
/// Returns a [`ReloadOutcome`] for the transcript. Best-effort: a failure in
/// one channel (e.g. an extension that fails to reload) does not abort the others —
/// the reload completes with whatever loaded, mirroring pi's per-extension
/// skip-on-error. A hard failure (e.g. the harness is closed) surfaces as an
/// error summary.
pub async fn reload_extension_resources(
    harness: &Arc<AgentHarness>,
    ctx: &ReloadContext,
) -> ReloadOutcome {
    reload_extension_resources_inner(harness, ctx, || {}).await
}

async fn reload_extension_resources_inner<F>(
    harness: &Arc<AgentHarness>,
    ctx: &ReloadContext,
    after_prepare: F,
) -> ReloadOutcome
where
    F: FnOnce() + Send,
{
    // Resolve the arguments once for this reload: every subsequent loader and
    // policy decision must observe the same effective set rather than the
    // pre-reload snapshot held in `ctx.args`.
    let effective_args = ctx.args.clone();
    let cwd_str = ctx.cwd.to_string_lossy().to_string();
    let project_trusted = resolve_project_trust(&effective_args, &ctx.cwd);
    let prepared = match prepare_reload_inputs(&effective_args, &ctx.cwd, project_trusted) {
        Ok(prepared) => prepared,
        Err(error) => {
            return ReloadOutcome {
                summary: format!(
                    "Settings reload failed: {error}. Keeping the currently loaded resources."
                ),
                had_warnings: true,
                details: Vec::new(),
                had_errors: true,
            };
        }
    };
    after_prepare();
    let PreparedReloadInputs {
        skill_base_dirs,
        prompt_base_dirs,
    } = prepared;
    let mut warnings = false;

    // Tell extensions the session is going away before the reload swaps state.
    // Dispatch on the session being torn down (its own sink), never the global
    // fallback, so a concurrent session's extensions do not see this shutdown.
    let shutdown_snapshot = ctx.extension_session.lock().unwrap().snapshot_arc();
    if let Err(reason) = shutdown_extensions_for_reload(shutdown_snapshot).await {
        return ReloadOutcome {
            summary: "Reload failed during extension shutdown".into(),
            had_warnings: true,
            details: vec![reason],
            had_errors: true,
        };
    }

    // ---- 1. Build a fresh ActionBridge + ExtensionSession ----
    // The fresh bridge carries the SAME `HarnessActionHost` (the host's harness
    // cell already points at this harness; the host impl is reusable across
    // reloads — only the bridge's staleness flag + reload callback differ). We
    // re-use the host by reading it off the OLD bridge (it's the same
    // `Arc<dyn RuntimeActionHost>`).
    let old_bridge = ctx.action_bridge.lock().unwrap().clone();
    let host: Arc<dyn kiqi_host::RuntimeActionHost> = match &old_bridge {
        Some(b) => b.clone_host(),
        None => {
            // No prior bridge (no extensions ever loaded). Build a fresh host so
            // a reload that newly discovers extensions can still drive actions.
            let (action_host, _cell) = crate::extensions_actions::HarnessActionHost::new_empty(
                ctx.catalog.clone(),
                ctx.cwd.clone(),
                ctx.runtime.clone(),
                ctx.args.unknown_flags.clone(),
                ctx.tool_context.clone(),
                ctx.signal_host.clone(),
            );
            crate::extensions_actions::HarnessActionHost::set_harness(&_cell, harness);
            Arc::new(action_host)
        }
    };
    let host_for_extensions = Arc::clone(&host);

    let reload_cb = kiqi_host::reload_callback_from_mailbox(ctx.mailbox.clone());
    let fresh_bridge = kiqi_host::ActionBridge::with_reload_and_ui(
        ctx.runtime.clone(),
        host,
        reload_cb,
        ctx.ui_dialog.clone(),
        ctx.ext_status.clone(),
        ctx.editor_text.clone(),
    );

    // Build the fresh capabilities and the fresh per-session sink BEFORE the
    // session: the snapshot that goes into the cell must carry this sink, and
    // the reload's own `resources_discover` / provider hooks below dispatch
    // through it. Installing here also refreshes the process-global fallback
    // (for sessionless callers) with the same set, which is why the later
    // tool-merge block reuses `extension_caps` instead of installing a second
    // time. The session's own surfaces resolve these caps from the fresh
    // snapshot, never from that fallback.
    let extension_caps = std::sync::Arc::new(crate::extensions::reload_caps(
        std::sync::Arc::clone(&host_for_extensions),
        ctx.ui_dialog.clone(),
        ctx.ext_status.clone(),
        ctx.editor_text.clone(),
    ));
    let compiled_sink =
        crate::extensions::install_compiled_event_sink(std::sync::Arc::clone(&extension_caps));

    // P6: the cdylib loader is gone — the fresh session is always the empty
    // registry (wasm guests reload through `extensions::reload_caps`, not here).
    // The fresh caps ride on the fresh snapshot, so the post-swap probe hook
    // (which reads the cell at call time) dispatches with this reload's caps.
    let extension_session = kiqi_host::ExtensionSession::none_with_compiled_sink_and_caps(
        Some(compiled_sink),
        Some(std::sync::Arc::clone(&extension_caps) as std::sync::Arc<dyn kiqi_host::SessionCaps>),
    );

    // ---- 2. Invalidate the old session + bridge BEFORE the swap ----
    // The old bridge's flag flips false so in-flight `runtime_action` calls
    // parked on the old `user_data` hit the staleness guard. The old snapshot's
    // `active` flag is flipped for API compatibility, but it no longer gates
    // dispatch (`docs/design/extension-abi.md` §6.50): a dispatch that already
    // captured the old snapshot keeps running through that session's sink and
    // caps until it returns. Invalidate under the session lock before exposing
    // the replacement to approval callbacks.
    //
    // Swap directly to the fresh session under the same lock that approvals
    // use. A temporary empty session would let a concurrent call bypass its gate.
    // In-flight approvals retain their old session until their callback exits;
    // the bridge's staleness flag (not the snapshot's) prevents them from
    // authorizing after this swap.
    {
        let mut session_guard = ctx.extension_session.lock().unwrap();
        let old_session = std::mem::replace(&mut *session_guard, extension_session.clone());
        if let Some(old_snap) = old_session.snapshot_arc() {
            old_snap.invalidate();
        }
        // `old_session` drops here; in-flight adapters/callbacks keep their
        // own library references until they return.
    }
    if let Some(old_b) = old_bridge {
        old_b.invalidate();
    }

    // The session was swapped atomically above; expose its matching bridge.
    *ctx.action_bridge.lock().unwrap() = Some(Arc::clone(&fresh_bridge));

    // ---- 3. resources_discover ("reload") over the fresh snapshot ----
    let discovered = extension_session
        .snapshot_arc()
        .map(|snap| kiqi_host::emit_resources_discover(&cwd_str, "reload", &snap))
        .unwrap_or_default();

    // ---- 4. Re-run the Part-A loaders (same precedence + --no-* gates) ----
    let env = Arc::new(kiqi_tools::OsExecutionEnv::with_cwd(ctx.cwd.clone()));
    let env_dyn: Arc<dyn kiqi_tools::ExecutionEnv> = env.clone();

    // Package resources are re-read here (grants + manifest + digests). That
    // does not re-instantiate wasm hosts (`compiled_extension_hosts` is a
    // `OnceLock`), so a newly installed extension's tools still need a restart;
    // its resource dirs are picked up. Appended LAST below, same as startup.
    let extension_resources = crate::config::agent_dir()
        .map(|root| crate::extensions::granted_package_resource_dirs(&root))
        .unwrap_or_default();

    let mut skills: Vec<kiqi_harness::types::Skill> = Vec::new();
    let mut skill_diags: Vec<kiqi_harness::skills::SkillDiagnostic> = Vec::new();
    let mut resource_diags: Vec<kiqi_harness::diagnostics::ResourceDiagnostic> = Vec::new();
    if !effective_args.no_skills {
        // JS discovery is backed by the session-long lazy Node host. Until
        // that host is swapped as part of a future full JS reload, preserve
        // the paths it contributed at startup across `/reload`.
        let js_paths: &[PathBuf] = &[];
        let package_paths: &[PathBuf] = &extension_resources.skills;
        let dirs = append_reload_resource_paths(
            skill_base_dirs,
            &effective_args.skill,
            &discovered.skill_paths,
            js_paths,
            package_paths,
        );
        let result = load_skills_with_precedence(&env_dyn, &dirs).await;
        skills = result.skills;
        skill_diags = result.diagnostics;
        resource_diags.extend(result.resource_diagnostics);
        // Same cell the tool reads: `/reload` swaps the set in place.
        if let Ok(mut cell) = ctx.skill_cell.write() {
            *cell = skills.clone();
        }
    }

    let mut prompt_templates: Vec<kiqi_harness::types::PromptTemplate> = Vec::new();
    let mut prompt_diags: Vec<kiqi_harness::prompt_templates::PromptTemplateDiagnostic> =
        Vec::new();
    if !effective_args.no_prompt_templates {
        let js_paths: &[PathBuf] = &[];
        let package_paths: &[PathBuf] = &extension_resources.prompts;
        let dirs = append_reload_resource_paths(
            prompt_base_dirs,
            &effective_args.prompt_template,
            &discovered.prompt_paths,
            js_paths,
            package_paths,
        );
        let result = load_prompt_templates_with_precedence(&env_dyn, &dirs).await;
        prompt_templates = result.prompt_templates;
        prompt_diags = result.diagnostics;
        resource_diags.extend(result.resource_diagnostics);
    }

    let context_files = if effective_args.no_context_files || !project_trusted {
        Vec::new()
    } else {
        let agent_dir = crate::config::agent_dir().ok();
        let agent_dir_path = agent_dir.unwrap_or_else(|| ctx.cwd.clone());
        let extra_globals: Vec<std::path::PathBuf> =
            crate::config::agents_dir().into_iter().collect();
        kiqi_harness::context_files::load_project_context_files_with_globals(
            &env_dyn,
            &ctx.cwd,
            &agent_dir_path,
            &extra_globals,
        )
        .await
    };
    let context_block = format_project_context(&context_files);

    let mut details: Vec<String> = Vec::new();
    for diagnostic in &extension_resources.diagnostics {
        details.push(format!("warning: extension resources: {diagnostic}"));
    }
    if !skill_diags.is_empty()
        || !prompt_diags.is_empty()
        || !resource_diags.is_empty()
        || !extension_resources.diagnostics.is_empty()
    {
        warnings = true;
        // Collect formatted diagnostics for the caller to surface in its UI
        // instead of writing to stderr directly: the TUI owns the terminal,
        // and a raw `eprintln!` during a run would scribble over the
        // alternate-screen input row.
        for d in &skill_diags {
            details.push(format!(
                "warning: skill {} ({}): {}",
                d.path,
                d.code.as_str(),
                d.message
            ));
        }
        for d in &prompt_diags {
            details.push(format!(
                "warning: prompt template {} ({}): {}",
                d.path,
                d.code.as_str(),
                d.message
            ));
        }
        // Structured collisions: winner (kept) · loser (shadowed).
        for d in &resource_diags {
            if let kiqi_harness::diagnostics::ResourceDiagnostic::Collision(c) = d {
                details.push(format!(
                    "warning: {} name \"{}\" collision: keeping {} (shadowed {})",
                    c.resource_type.as_str(),
                    c.name,
                    c.winner_path,
                    c.loser_path
                ));
            }
        }
    }

    // Re-merge extension tools FIRST: the prompt below lists the tools this session
    // has, so building the list afterwards would keep advertising the previous
    // reload's set (and, on the first reload, the hard-coded one).
    let mut_env: Arc<dyn kiqi_tools::MutatingEnv> = env.clone();
    let tool_ctx = kiqi_tools::ExecutionToolContext::new(env_dyn.clone(), Some(mut_env));
    let mut tools = build_tools(
        &tool_ctx,
        &effective_args,
        &ctx.cwd,
        &ctx.signal_host,
        &ctx.skill_cell,
    );
    // ---- Rebuild the emitter (TeeEmitter over fresh ExtensionEmitter) ----
    let emitter: Arc<dyn kiqi_agent::AgentEmitter> = Arc::new(TeeEmitter::new(vec![
        ctx.broadcast.clone(),
        Arc::new(ExtensionEmitter::for_session(&extension_session)),
    ]));

    // ---- 5. Push the rebuilt state into the live harness via the B5d setters ----
    let _ = harness.set_agent_emitter(Some(emitter)).await;
    let _ = harness.set_models(ctx.providers.clone()).await;
    let _ = harness
        .set_provider_hooks(Some(crate::extensions::compiled_prompt_hooks(
            kiqi_host::ExtensionProviderHooks::from_session(&extension_session)
                .map(|h| Arc::new(h) as Arc<dyn kiqi_ai::ProviderHooks>),
            std::sync::Arc::clone(&extension_caps) as std::sync::Arc<dyn kiqi_host::SessionCaps>,
        )))
        .await;

    // The rebuilt tool adapters get the session's UI mailboxes too, so a
    // `/reload` no longer costs a compiled extension its `ui.dialog` /
    // `set_status` / `set_editor_text`. The mailboxes live in the reload
    // context (not the bridge) precisely so they survive the swap, and the
    // action bridge already exposes them — the "no accessor" note that used to
    // be here was stale.
    //
    // `extension_caps` (and the sink installed with it) was built before the
    // fresh session, so the post-swap tool adapters reuse exactly the
    // capabilities the session's snapshot dispatches with.
    crate::extensions::merge_compiled_extension_tools(
        &mut tools,
        &crate::extensions::compiled_extension_hosts(),
        &extension_caps,
        &|name| tool_name_allowed(name, &effective_args),
    );
    // Re-inject the host-level MCP tools. This rebuild starts from an empty set
    // (`build_tools` + extension + compiled-extension tools) and `set_tools`
    // REPLACES the harness registry, so without this a `/reload` would silently
    // drop every `{server}__{tool}` for the rest of the session.
    merge_mcp_tools(&mut tools, &ctx.mcp, &effective_args);
    // Compose the prompt only now: it lists the tools this session has, and the
    // compiled-extension and MCP adapters are merged above. Composing earlier (as this
    // path used to) named only the built-ins and extension tools, so a `/reload`
    // advertised a set smaller than the one `set_tools` installed — and with the
    // compiled extensions enabled, `todo` and friends were silently missing.
    let reload_tool_names: Vec<String> =
        tools.iter().map(|t| t.tool.schema().name.clone()).collect();
    let mut context_paths = Vec::new();

    // ---- Re-compose the system prompt (same precedence as build) ----
    // Same working-state wording rule as `build`; a mid-session `/model` switch
    // re-composes the prompt on the next reload, which is the same staleness the
    // rest of this path already has (provider resolution is a startup concern).
    let prior_reasoning_replayed = kiqi_ai::model::prior_reasoning_is_replayed(&ctx.resolved_model);
    let base_prompt = match effective_args.system_prompt.as_deref() {
        Some(explicit) => explicit.to_string(),
        None if project_trusted => match discover_system_prompt_file(&ctx.cwd) {
            Some(path) => read_prompt_source(&path, &mut context_paths).unwrap_or_else(|| {
                default_system_prompt_for_tools(
                    &cwd_str,
                    prior_reasoning_replayed,
                    Some(&reload_tool_names),
                )
            }),
            None => default_system_prompt_for_tools(
                &cwd_str,
                prior_reasoning_replayed,
                Some(&reload_tool_names),
            ),
        },
        None => default_system_prompt_for_tools(
            &cwd_str,
            prior_reasoning_replayed,
            Some(&reload_tool_names),
        ),
    };
    let mut append_texts: Vec<String> = Vec::new();
    for extra in &effective_args.append_system_prompt {
        let text = read_append_target(extra, &mut context_paths).unwrap_or_else(|| extra.clone());
        append_texts.push(text);
    }
    if effective_args.append_system_prompt.is_empty() {
        if let Some(path) = project_trusted
            .then(|| discover_append_system_prompt_file(&ctx.cwd))
            .flatten()
        {
            if let Some(text) = read_prompt_source(&path, &mut context_paths) {
                append_texts.push(text);
            }
        }
    }
    context_paths.extend(
        context_files
            .iter()
            .map(|file| file.path.to_string_lossy().into_owned()),
    );
    let append_join = if append_texts.is_empty() {
        None
    } else {
        Some(append_texts.join("\n\n"))
    };
    let system_prompt = compose_system_prompt(
        Some(&base_prompt),
        &[],
        if context_block.is_empty() {
            None
        } else {
            Some(&context_block)
        },
        append_join.as_deref(),
    );
    // `resources.context_files` comes from the composition above: it is what
    // `read_prompt_source` recorded while building the prompt, so the panel and the
    // prompt cannot disagree about which instruction files are in force.
    let resources = AgentHarnessResources {
        context_files: context_paths,
        skills: if skills.is_empty() {
            None
        } else {
            Some(skills.clone())
        },
        prompt_templates: if prompt_templates.is_empty() {
            None
        } else {
            Some(prompt_templates.clone())
        },
    };
    let _ = harness.set_resources(resources).await;
    let _ = harness.set_system_prompt(Some(system_prompt)).await;
    let mut active = active_tool_names(&tools, &effective_args);
    active = filter_active_tool_names(active, &effective_args);
    let _ = harness.set_tools(tools, Some(active)).await;

    let summary = format!(
        "Reloaded {} skill(s), {} prompt(s).",
        skills.len(),
        prompt_templates.len(),
    );
    ReloadOutcome {
        summary,
        had_warnings: warnings,
        details,
        // Reload completed; any diagnostics here are soft (shadowing / name
        // validation / skipped optional resources), not hard failures.
        had_errors: false,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReloadSettingsFields {
    skill_dirs: Option<Vec<String>>,
    prompt_dirs: Option<Vec<String>>,
    extension_dirs: Option<Vec<String>>,
}

impl From<crate::settings::Settings> for ReloadSettingsFields {
    fn from(settings: crate::settings::Settings) -> Self {
        Self {
            skill_dirs: settings.skill_dirs,
            prompt_dirs: settings.prompt_dirs,
            extension_dirs: settings.extension_dirs,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReloadSettingsSnapshot {
    global: ReloadSettingsFields,
    project: Option<ReloadSettingsFields>,
}

fn reload_reads_settings(args: &Args) -> bool {
    !args.dev_local_only && (!args.no_extensions || !args.no_skills || !args.no_prompt_templates)
}

/// Strictly read the settings fields consumed while preparing a reload. The
/// snapshot intentionally excludes UI/model fields that `/reload` does not
/// use, so an unrelated settings save does not invalidate the operation.
fn load_reload_settings_snapshot(
    args: &Args,
    cwd: &Path,
    project_trusted: bool,
) -> Result<Option<ReloadSettingsSnapshot>, String> {
    if !reload_reads_settings(args) {
        return Ok(None);
    }
    let global = crate::settings::load_settings()
        .map_err(|error| format!("could not load global settings: {error}"))?
        .into();
    let project = if project_trusted {
        crate::settings::load_active_project_settings(cwd)
            .map_err(|error| format!("could not load project settings: {error}"))?
            .map(|(_, settings)| settings.into())
    } else {
        None
    };
    Ok(Some(ReloadSettingsSnapshot { global, project }))
}

/// Validate every settings document that this reload will consume before
/// replacing any live extension, bridge, or harness resource.
#[cfg(test)]
fn validate_settings_for_reload(
    args: &Args,
    cwd: &Path,
    project_trusted: bool,
) -> Result<(), String> {
    load_reload_settings_snapshot(args, cwd, project_trusted).map(|_| ())
}

fn prepare_reload_inputs(
    args: &Args,
    cwd: &Path,
    project_trusted: bool,
) -> Result<PreparedReloadInputs, String> {
    prepare_reload_inputs_inner(args, cwd, project_trusted, || {})
}

fn prepare_reload_inputs_inner<F>(
    args: &Args,
    cwd: &Path,
    project_trusted: bool,
    before_verify: F,
) -> Result<PreparedReloadInputs, String>
where
    F: FnOnce(),
{
    let settings_before = load_reload_settings_snapshot(args, cwd, project_trusted)?;

    let skill_base_dirs = if args.no_skills {
        Vec::new()
    } else if args.dev_local_only || args.project_only {
        project_skill_dirs(cwd)
    } else if project_trusted {
        skill_dirs(cwd)
    } else {
        global_skill_dirs()
    };

    let prompt_base_dirs = if args.no_prompt_templates {
        Vec::new()
    } else if args.dev_local_only || args.project_only {
        project_prompt_template_dirs(cwd)
    } else if project_trusted {
        prompt_template_dirs(cwd)
    } else {
        global_prompt_template_dirs()
    };

    before_verify();
    let settings_after = load_reload_settings_snapshot(args, cwd, project_trusted)?;
    if settings_before != settings_after {
        return Err(
            "settings changed while reload inputs were being prepared; retry /reload".to_string(),
        );
    }

    Ok(PreparedReloadInputs {
        skill_base_dirs,
        prompt_base_dirs,
    })
}

/// A harness-build error.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("Could not create the session directory: {0}")]
    SessionDir(String),
    #[error("Could not open the saved session: {0}")]
    SessionOpen(String),
    #[error("No session found for {requested} in {dir}. Start a fresh session instead (drop --continue/--resume/--session).")]
    SessionNotFound { requested: String, dir: String },
    #[error("Could not build the harness: {0}")]
    HarnessCreate(String),
}

/// Resolve project resource loading without prompting. Explicit CLI overrides
/// win, then a stored decision; projects with no decision load by default.
pub(crate) fn resolve_project_trust(args: &Args, cwd: &Path) -> bool {
    if let Some(override_value) = args.trust_override {
        return override_value;
    }
    crate::config::project_trust_decision(cwd)
        .ok()
        .flatten()
        .unwrap_or(true)
}

/// Build the tool list per `--tools`/`--exclude-tools`/`--no-tools`/
/// `--no-builtin-tools`. Mirrors the TS `tools`/`excludeTools`/`noTools`
/// resolution in `createAgentSession`.
/// Default bash timeout: 120s when the model doesn't pass one (prevents a
/// forgotten `timeout` from hanging the run forever — the "卡住" report).
/// `KI_BASH_TIMEOUT` overrides; a model-supplied timeout always wins.
pub fn bash_options(cwd: &Path) -> kiqi_tools::tools::bash::BashToolOptions {
    use kiqi_tools::tools::bash::BashToolOptions;
    let default = std::env::var("KI_BASH_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(120.0);
    // Saved user defaults, matching how `defaultTools` is read in `build_tools`.
    // `shellCommandPrefix` is prepended to every command (upstream's key for
    // alias/`shopt` setup). Native's `shellPath` is *not* wired: neither
    // `BashToolOptions` nor `ShellCaptureOptions` carries a shell binary today, so
    // honouring it means plumbing one through pi-tools first.
    let settings = crate::settings::load_settings().unwrap_or_default();
    // macOS: wrap the shell child tree so no command can rewrite the policy
    // after the call returns (the lexical guard and post-call restore remain
    // the second layer). `KI_SHELL_SANDBOX=0` opts out; Linux is a follow-up.
    let sandbox = if std::env::var("KI_SHELL_SANDBOX").ok().as_deref() == Some("0") {
        None
    } else {
        kiqi_tools::sandbox::ShellSandbox::macos_for(crate::permissions::shell_sandbox_paths(cwd))
    };
    BashToolOptions {
        command_prefix: settings.shell_command_prefix.clone(),
        default_timeout: Some(default),
        prepare: None,
        sandbox,
    }
}

fn build_tools(
    ctx: &ExecutionToolContext,
    args: &Args,
    cwd: &Path,
    signal_host: &Arc<crate::signal_host::SignalHost>,
    skill_cell: &Arc<std::sync::RwLock<Vec<kiqi_harness::types::Skill>>>,
) -> Vec<HarnessTool> {
    if args.no_tools {
        return Vec::new();
    }
    // Keep the coding tools aligned with Pi and expose ki's read-only docs
    // lookup as a default assistant capability.
    //
    // Replay policy (native `ToolDeclaration.replay`): only tools that are
    // genuinely free of side effects may be re-run when a crash leaves their
    // outcome unknown — everything else must stay `Never`, because replaying
    // `bash`/`write`/`edit` would duplicate a side effect. `HarnessTool::new`
    // already defaults to `Never`, so this is opt-in per read-only tool.
    let mut all: Vec<(&'static str, HarnessTool)> = vec![
        (
            "read",
            HarnessTool::new(create_read_tool(
                ctx,
                Some(kiqi_tools::tools::read::ReadToolOptions {
                    image_processor: Some(Arc::new(
                        kiqi_tools::image_processing::InlineImageProcessor,
                    )),
                    ..Default::default()
                }),
            ))
            .with_replay(ToolReplay::Safe),
        ),
        (
            "bash",
            HarnessTool::new(create_bash_tool(ctx, Some(bash_options(cwd)))),
        ),
        ("edit", HarnessTool::new(create_edit_tool(ctx))),
        ("write", HarnessTool::new(create_write_tool(ctx))),
        (
            "docs",
            HarnessTool::new(create_docs_tool()).with_replay(ToolReplay::Safe),
        ),
        (
            "permissions",
            HarnessTool::new(crate::permissions::create_permissions_tool(
                cwd.to_path_buf(),
            )),
        ),
        (
            "signals",
            HarnessTool::new(crate::signals_tool::create_signals_tool(Arc::clone(
                signal_host,
            ))),
        ),
        // Background subagents: returns as soon as the child is up, so a batch
        // of them runs in parallel and the turn keeps moving.
        (
            "subagent",
            HarnessTool::new(crate::subagent_tool::create_subagent_tool()),
        ),
    ];

    // Progressive skill loading: the system prompt lists skills, this tool
    // loads one body on demand (and re-reads it, so edits are current).
    if !args.no_skills {
        all.push((
            "skill",
            HarnessTool::new(crate::skill_tool::create_skill_tool(
                Arc::clone(skill_cell),
                Arc::clone(ctx.env()),
            ))
            .with_replay(ToolReplay::Safe),
        ));
    }

    // Add PowerShell tool on Windows
    #[cfg(target_os = "windows")]
    all.push(("powershell", HarnessTool::new(create_powershell_tool(ctx))));

    // Apply default_tools from settings (if no CLI override)
    if args.tools.is_none() {
        if let Ok(settings) = crate::settings::load_settings() {
            if let Some(default_tools) = &settings.default_tools {
                all.retain(|(name, _)| default_tools.iter().any(|t| t == name));
            }
        }
    }

    // `--no-builtin-tools` disables the built-in set but would keep
    // extension/custom tools — v1 has none, so it's equivalent to `--no-tools`
    // here. We honor it by clearing the built-ins.
    if args.no_builtin_tools {
        all.clear();
    }

    // Allowlist (`--tools`): keep only named built-ins.
    if let Some(allow) = &args.tools {
        all.retain(|(name, _)| allow.iter().any(|a| a == name));
    }
    // Denylist (`--exclude-tools`): drop named tools.
    if let Some(deny) = &args.exclude_tools {
        all.retain(|(name, _)| !deny.iter().any(|d| d == name));
    }

    all.into_iter().map(|(_, t)| t).collect()
}

/// Append the host-level MCP tool adapters to `tools`, honoring the same name
/// policy (`--tools`/`--exclude-tools`/`--no-tools`) and collision gate as every
/// other tool source. Called LAST in both `build` and `/reload` so an MCP tool
/// can never silently shadow a built-in, extension or extension tool — a collision
/// is skipped with a warning. `pub(crate)`: the ACP e2e tests mirror this exact
/// merge in their harness-building factories.
pub(crate) fn merge_mcp_tools(
    tools: &mut Vec<HarnessTool>,
    mcp: &kiqi_mcp::McpManager,
    args: &Args,
) {
    let mut taken: HashSet<String> = tools
        .iter()
        .map(|tool| tool.tool.schema().name.clone())
        .collect();
    let mut allowed = HashSet::new();
    for adapter in mcp.tool_adapters() {
        let name = adapter.schema().name.clone();
        if !tool_name_allowed(&name, args) {
            continue;
        }
        if !taken.insert(name.clone()) {
            eprintln!("warning: MCP tool {name} collides with an existing tool; skipped");
            continue;
        }
        allowed.insert(name);
        tools.push(HarnessTool::new(adapter));
    }
    // The boundary refresh (`drain_mcp_boundary`) consults these so a
    // list_changed-triggered re-merge can never resurrect a name the policy
    // excluded here at build time, and so the replace removes exactly the
    // names MCP actually injected (a collision winner that merely LOOKS
    // MCP-shaped stays untouched).
    mcp.record_allowed_tools(allowed.clone());
    mcp.record_injected_tools(allowed);
}

/// Anything still pending at teardown goes to stderr — a print-mode notice
/// would otherwise sit in the mailbox forever. Called from the shutdown
/// advisory BEFORE `McpManager::shutdown`: shutdown purges the mailbox, so a
/// flush after it would be empty by construction.
pub(crate) fn flush_mcp_notices_to_stderr(mcp: &kiqi_mcp::McpManager) {
    // Raw drain: teardown is not a place to issue fresh `resources/read`s — the
    // connection is about to close, and the notice is a diagnostic here, not a
    // model-facing entry. Content-bearing (autoRead) notices keep what the
    // boundary drain already read.
    let batch = mcp.take_notices();
    if batch.is_empty() {
        return;
    }
    eprintln!("warning: mcp: undelivered resource notices at teardown:");
    eprintln!("{}", kiqi_mcp::notice_entry_text(&batch));
}

/// Resolve the active tool names from the constructed tools when no explicit
/// `--tools` allowlist was given. Mirrors the TS default: all registered tools
/// active.
fn active_tool_names(tools: &[HarnessTool], args: &Args) -> Vec<String> {
    filter_active_tool_names(
        tools.iter().map(|tool| tool.tool.schema().name.clone()),
        args,
    )
}

/// Whether a tool name survives the command-line tool policy. Keep this check
/// centralized so extension-provided tools honor the same policy as built-ins.
pub(crate) fn tool_name_allowed(name: &str, args: &Args) -> bool {
    if args.no_tools {
        return false;
    }
    if args
        .tools
        .as_ref()
        .is_some_and(|allow| !allow.iter().any(|value| value == name))
    {
        return false;
    }
    if args
        .exclude_tools
        .as_ref()
        .is_some_and(|deny| deny.iter().any(|value| value == name))
    {
        return false;
    }
    true
}

pub(crate) fn filter_active_tool_names<I>(names: I, args: &Args) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    names
        .into_iter()
        .filter(|name| tool_name_allowed(name, args))
        .collect()
}

/// Start the session's host-level MCP servers and return the manager.
///
/// Servers come from three sources, merged in that order of specificity:
/// the global `settings.json`, the project's, and (batch 3) the servers the
/// ACP client supplied on `session/new`/`session/load` — POLICY 1: the ACP
/// client is the user's own editor, so its request IS the authorization and
/// the project-trust gate does not apply to it. A client-supplied entry
/// replaces a same-named settings entry (the session request is the more
/// specific configuration); the displaced entry is reported on stderr.
/// Every rejected, skipped or failed server is reported on stderr — nothing
/// disappears silently — and a failure never stops the session from building.
async fn start_mcp_manager(
    cwd: &Path,
    project_trusted: bool,
    client_servers: &[kiqi_mcp::ServerConfig],
) -> Arc<kiqi_mcp::McpManager> {
    let global = crate::settings::load_settings().unwrap_or_default();
    let project_settings = crate::settings::load_project_settings(cwd);
    // Top-level `signals` defaults: global first, a trusted project overrides
    // field by field. Project defaults are parsed only when trusted (an
    // untrusted project must not change host behavior), but project server
    // entries are still parsed so `merge_servers` can report them as ignored.
    let (mut defaults, mut issues) = kiqi_mcp::parse_signals_defaults(global.signals.as_ref());
    if project_trusted {
        for settings in &project_settings {
            let (project_defaults, project_issues) =
                kiqi_mcp::parse_signals_defaults(settings.signals.as_ref());
            issues.extend(project_issues);
            defaults = kiqi_mcp::merge_signals_defaults(defaults, project_defaults);
        }
    }
    let (global_servers, server_issues) =
        kiqi_mcp::parse_servers_with_defaults(global.mcp_servers.as_ref(), &defaults);
    issues.extend(server_issues);

    let mut project_servers = Vec::new();
    for settings in &project_settings {
        let (servers, project_issues) =
            kiqi_mcp::parse_servers_with_defaults(settings.mcp_servers.as_ref(), &defaults);
        project_servers.extend(servers);
        issues.extend(project_issues);
    }

    let (mut merged, merge_issues) =
        kiqi_mcp::merge_servers(global_servers, project_servers, project_trusted);
    issues.extend(merge_issues);

    if !client_servers.is_empty() {
        let client_names: HashSet<String> = client_servers.iter().map(|s| s.name.clone()).collect();
        merged.retain(|server| {
            if client_names.contains(&server.name) {
                eprintln!(
                    "warning: mcp: settings-defined server {} replaced by the client-supplied one for this session",
                    server.name
                );
                false
            } else {
                true
            }
        });
        merged.extend(client_servers.iter().cloned());
    }

    let (manager, start_issues) =
        kiqi_mcp::McpManager::start_with_http_policy(&merged, cwd, global.http_idle_timeout_ms())
            .await;
    issues.extend(start_issues);

    for issue in &issues {
        eprintln!("warning: mcp: {issue}");
    }
    Arc::new(manager)
}

/// Build the `Session` facade for the chosen selection.
async fn build_session(selection: &SessionSelection, cwd: &str) -> Result<Session, BuildError> {
    match selection {
        SessionSelection::Ephemeral => Ok(ephemeral_session()),
        SessionSelection::New { dir, .. } => {
            // Ensure the sessions directory exists, then create a fresh JSONL
            // session file inside it.
            std::fs::create_dir_all(dir)
                .map_err(|e| BuildError::SessionDir(format!("{}: {e}", dir.display())))?;
            let session = create_jsonl_session(dir, cwd)
                .await
                .map_err(|e| BuildError::SessionDir(format!("{}: {e}", dir.display())))?;
            Ok(session)
        }
        SessionSelection::Latest { .. }
        | SessionSelection::ById { .. }
        | SessionSelection::ByExactId { .. } => restore_session(selection, cwd).await,
        SessionSelection::Fork { source } => fork_session_at_launch(source, cwd).await,
    }
}

/// Open an existing JSONL session for `Latest` / `ById`. Mirrors the TS
/// `SessionManager.resume`/`open` flow: list the session dir (newest-first),
/// match the request, then open the matched file and wrap it in a `Session`
/// facade. The restored transcript renders into the TUI at startup and the
/// harness continues appending to the same file.
async fn restore_session(selection: &SessionSelection, cwd: &str) -> Result<Session, BuildError> {
    // `list_typed` is newest-first; `Latest` takes the head, `ById` matches
    // the id exactly or by file-name containment (so `--session 01a02…` or a
    // partial id works, mirroring the TS id/path matching).
    match selection {
        SessionSelection::Latest { dir } => {
            let metas = list_session_metadata_in_dir(cwd, dir).await?;
            let Some(meta) = first_resumable_session(&metas) else {
                return Err(BuildError::SessionNotFound {
                    requested: "the most recent session".to_string(),
                    dir: default_session_dir(Path::new(cwd)).display().to_string(),
                });
            };
            open_session(meta, cwd).await
        }
        SessionSelection::ById { id, dir } => open_session_by_id_in_dir(id, cwd, dir)
            .await
            .map_err(|e| match e {
                OpenError::NotFound { requested } => BuildError::SessionNotFound {
                    requested,
                    dir: default_session_dir(Path::new(cwd)).display().to_string(),
                },
                OpenError::Other(msg) => BuildError::SessionOpen(msg),
            }),
        SessionSelection::ByExactId { id, dir } => {
            // Exact id match only (pi `--session-id`): restore when the
            // session exists, else create a fresh one under the selected dir.
            let metas = list_session_metadata_in_dir(cwd, dir).await?;
            if let Some(meta) = metas.iter().find(|m| m.id == *id) {
                return open_session(meta, cwd).await;
            }
            std::fs::create_dir_all(dir)
                .map_err(|e| BuildError::SessionDir(format!("{}: {e}", dir.display())))?;
            create_jsonl_session_with_id(dir, cwd, Some(id.clone()))
                .await
                .map_err(|e| BuildError::SessionDir(format!("{}: {e}", dir.display())))
        }
        _ => unreachable!("restore_session only called for Latest/ById/ByExactId"),
    }
}

/// `--fork <path|id>`: open the source session, fork it into a new JSONL
/// session (records the parent id), and start in the fork.
async fn fork_session_at_launch(source: &str, cwd: &str) -> Result<Session, BuildError> {
    use kiqi_harness::session::jsonl::{
        JsonlSessionCreateOptions, JsonlSessionRepo, JsonlSessionRepoOptions,
    };
    use kiqi_harness::session::types::{ForkOptions, SessionStorage};
    use kiqi_tools::FileSystem;

    let dir = default_session_dir(Path::new(cwd));
    std::fs::create_dir_all(&dir)
        .map_err(|e| BuildError::SessionDir(format!("{}: {e}", dir.display())))?;
    let env = Arc::new(OsExecutionEnv::with_cwd(PathBuf::from(cwd)));
    let fs: Arc<dyn FileSystem> = env.clone();
    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs: fs.clone(),
        sessions_root: dir.to_string_lossy().into_owned(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    });
    let metas = repo
        .list_typed(&kiqi_harness::session::jsonl::JsonlSessionListOptions::default())
        .await
        .map_err(|e| BuildError::SessionDir(format!("list sessions: {e}")))?;
    let source_meta = metas
        .iter()
        .find(|m| m.id == *source || m.path.contains(source) || source.contains(&m.id))
        .ok_or_else(|| BuildError::SessionNotFound {
            requested: format!("--fork {source}"),
            dir: dir.display().to_string(),
        })?;
    let fork_storage = repo
        .fork_typed(
            source_meta,
            &JsonlSessionCreateOptions {
                id: None,
                parent_session_id: Some(source_meta.id.clone()),
                cwd: cwd.to_string(),
                metadata: None,
            },
            &ForkOptions::default(),
        )
        .await
        .map_err(|e| BuildError::SessionDir(format!("fork {}: {e}", source_meta.path)))?;
    claim_session_lock(&fork_storage.jsonl_metadata().path).map_err(BuildError::SessionOpen)?;
    let storage_arc: Arc<dyn SessionStorage> = Arc::new(fork_storage);
    Ok(Session::new(storage_arc, None))
}

/// Errors from [`open_session_by_id`], split so the CLI can map them to
/// [`BuildError`] while the TUI can surface a friendlier note.
pub enum OpenError {
    /// No session matched the request.
    NotFound { requested: String },
    /// The match existed but could not be opened/parsed.
    Other(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::NotFound { requested } => write!(f, "no session matches {requested}"),
            // `Other` carries the bare reason (see `open_session_by_id`); the
            // prefix lives here so the TUI and the CLI's
            // `BuildError::SessionOpen` wrapper each render it exactly once
            // (wrapping the already-prefixed `BuildError` string doubled it).
            OpenError::Other(msg) => write!(f, "Could not open the saved session: {msg}"),
        }
    }
}

/// List the JSONL session metadata under the default session dir, newest
/// first. Shared by startup restore and the TUI `/session` hot-switch.
pub async fn list_session_metadata(
    cwd: &str,
) -> Result<Vec<kiqi_harness::session::jsonl::JsonlSessionMetadata>, BuildError> {
    let dir = default_session_dir(Path::new(cwd));
    list_session_metadata_in_dir(cwd, &dir).await
}

pub(crate) async fn list_session_metadata_in_dir(
    cwd: &str,
    dir: &Path,
) -> Result<Vec<kiqi_harness::session::jsonl::JsonlSessionMetadata>, BuildError> {
    use kiqi_harness::session::jsonl::{
        JsonlSessionListOptions, JsonlSessionRepo, JsonlSessionRepoOptions,
    };
    use kiqi_tools::FileSystem;

    let env = Arc::new(OsExecutionEnv::with_cwd(PathBuf::from(cwd)));
    let fs: Arc<dyn FileSystem> = env.clone();
    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs: fs.clone(),
        sessions_root: dir.to_string_lossy().into_owned(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    });
    repo.list_typed(&JsonlSessionListOptions::default())
        .await
        .map_err(|e| BuildError::SessionDir(format!("list sessions: {e}")))
}

/// A cheap, display-oriented summary of one JSONL session file.
///
/// The session pickers (`-r` and `/session`) need enough context to tell saved
/// sessions apart. The header-only [`JsonlSessionMetadata`] carries no name and
/// no transcript, so a list built from it renders as a wall of identical
/// `2026-09-25T15-05-15-847Z_…` file names — including the many header-only
/// sessions an abandoned launch leaves behind. This reads just the head (and a
/// small tail, for a late `--name`/`/name` fact) of the file to recover:
///
/// - the session display name, when one was recorded;
/// - the first user prompt, single-lined and truncated, for a readable label;
/// - whether the file is header-only (nothing to resume).
///
/// It intentionally does **not** parse the whole log: sessions can be tens of
/// megabytes, and the picker runs over every session at startup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSummary {
    /// The session display name (`--name` / `/name`), if one was recorded.
    pub name: Option<String>,
    /// The first user prompt, single-lined and trimmed to a label-sized length.
    pub preview: Option<String>,
    /// True when the file holds only its header (a session with no content).
    pub empty: bool,
    /// On-disk size in bytes (a rough content-volume hint for the picker).
    pub bytes: u64,
}

/// Bytes read from the head of a session file to recover a name + first prompt.
const SUMMARY_HEAD_BYTES: u64 = 256 * 1024;
/// Bytes read from the tail of a session file to recover a late name fact.
const SUMMARY_TAIL_BYTES: u64 = 8 * 1024;
/// Longest preview retained (characters, not bytes).
const SUMMARY_PREVIEW_CHARS: usize = 80;

/// Summarize one session file for the pickers. Best-effort: an unreadable or
/// malformed file yields a summary with `empty: false` and no label, so it is
/// still selectable rather than silently dropped.
pub fn summarize_session_file(path: &Path) -> SessionSummary {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        return SessionSummary::default();
    };
    let Ok(size) = file.metadata().map(|m| m.len()) else {
        return SessionSummary::default();
    };
    if size == 0 {
        return SessionSummary {
            name: None,
            preview: None,
            empty: true,
            bytes: 0,
        };
    }

    // Head: the header line (for the empty check) + a name fact + the first user
    // prompt. The whole window is scanned; it is already in memory.
    let mut head = vec![0u8; SUMMARY_HEAD_BYTES.min(size) as usize];
    let head_len = file.read(&mut head).unwrap_or(0);
    head.truncate(head_len);
    let head = String::from_utf8_lossy(&head);
    let mut lines: Vec<&str> = head.lines().collect();
    let header_len = lines.first().map(|line| line.len()).unwrap_or(0);
    // A header-only file is exactly the header plus an optional trailing `\n`.
    let empty = (size as usize) <= header_len + 1;
    // When the window stopped short of the file end, its final line is partial:
    // drop it so a half-read message can never be mistaken for a complete one.
    if (head_len as u64) < size && !lines.is_empty() {
        lines.pop();
    }

    let mut summary = SessionSummary {
        name: None,
        preview: None,
        empty,
        bytes: size,
    };
    for line in lines {
        apply_summary_line(line, &mut summary);
    }

    // Tail: a name set late in the session lives after the head window. Read the
    // last window and scan only complete lines so a torn/partial first line is
    // ignored. Only do this when the file is larger than the head window.
    if size > SUMMARY_HEAD_BYTES
        && file
            .seek(SeekFrom::End(-(SUMMARY_TAIL_BYTES as i64)))
            .is_ok()
    {
        let mut tail = Vec::new();
        if file.read_to_end(&mut tail).is_ok() {
            let tail = String::from_utf8_lossy(&tail);
            let mut tail_lines = tail.lines();
            // Drop the possibly-partial first line of the window.
            tail_lines.next();
            for line in tail_lines {
                if let Some(name) = name_from_line(line) {
                    summary.name = name;
                }
            }
        }
    }
    summary
}

/// Extract the name from a `{"kind":"fact","fact":"name","name":…}` line.
/// Returns `Some(None)` for an explicit `name: null` (the fact was cleared).
fn name_from_line(line: &str) -> Option<Option<String>> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("kind").and_then(|v| v.as_str()) != Some("fact") {
        return None;
    }
    if value.get("fact").and_then(|v| v.as_str()) != Some("name") {
        return None;
    }
    Some(match value.get("name") {
        Some(serde_json::Value::String(name)) => Some(name.clone()),
        _ => None,
    })
}

/// Fold one JSONL line into `summary`: record a name fact, or capture the first
/// user prompt as the preview.
fn apply_summary_line(line: &str, summary: &mut SessionSummary) {
    if let Some(name) = name_from_line(line) {
        summary.name = name;
        return;
    }
    if summary.preview.is_some() {
        return;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    if value.get("kind").and_then(|v| v.as_str()) != Some("entry") {
        return;
    }
    if value.get("type").and_then(|v| v.as_str()) != Some("message") {
        return;
    }
    let Some(message) = value.get("message") else {
        return;
    };
    if message.get("role").and_then(|v| v.as_str()) != Some("user") {
        return;
    }
    let text = match message.get("content") {
        Some(serde_json::Value::String(text)) => Some(text.as_str()),
        Some(serde_json::Value::Array(blocks)) => blocks.iter().find_map(|block| {
            (block.get("type").and_then(|v| v.as_str()) == Some("text"))
                .then(|| block.get("text").and_then(|v| v.as_str()))
                .flatten()
        }),
        _ => None,
    };
    let Some(text) = text else { return };
    let collapsed = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.is_empty() {
        return;
    }
    summary.preview = Some(collapsed.chars().take(SUMMARY_PREVIEW_CHARS).collect());
}

/// The human label for a session row: its recorded name, else the first user
/// prompt, else a marker that says the file has no transcript to show. Shared
/// by the startup picker and the in-TUI `/session` selector.
pub fn session_display_label(summary: &SessionSummary) -> String {
    if let Some(name) = summary.name.as_deref().map(str::trim) {
        if !name.is_empty() {
            return name.to_string();
        }
    }
    match summary.preview.as_deref().map(str::trim) {
        Some(preview) if !preview.is_empty() => preview.to_string(),
        _ => "(no messages)".to_string(),
    }
}

/// Compact byte count (1.2MB / 34KB / 512B), shared by the startup picker and
/// the in-TUI `/session` selector.
pub fn format_session_bytes(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1}MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{}KB", bytes / 1024)
    } else {
        format!("{bytes}B")
    }
}

/// The first 8 characters of a session id, enough to disambiguate two rows that
/// share a timestamp without shouting the whole UUID.
pub fn short_session_id(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// Synchronously list every JSONL session file under `dir`, newest-first by
/// modification time.
///
/// The repo's [`list_session_metadata`] is async and scans one cwd-encoded
/// subdirectory level; the in-TUI `/session` selector runs on the blocking key
/// thread and cannot await it, so this walks the same layout directly. Both
/// `.jsonl` files directly under `dir` and one level of subdirectories are
/// accepted.
pub fn list_session_files_sync(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    let mut visit = |path: PathBuf| {
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            return;
        }
        let modified = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        files.push((path, modified));
    };
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Ok(inner) = std::fs::read_dir(&path) {
                    for child in inner.flatten() {
                        visit(child.path());
                    }
                }
            } else {
                visit(path);
            }
        }
    }
    files.sort_by(|a, b| b.1.cmp(&a.1));
    files.into_iter().map(|(path, _)| path).collect()
}

/// Pick the session `--continue`/`-c` should open: the newest session that has
/// content (a header-only session resumes to an empty transcript). Falls back to
/// the newest overall when every session is empty, and `None` only when the
/// list is empty.
fn first_resumable_session<'a>(
    metas: &'a [kiqi_harness::session::jsonl::JsonlSessionMetadata],
) -> Option<&'a kiqi_harness::session::jsonl::JsonlSessionMetadata> {
    metas
        .iter()
        .find(|meta| !summarize_session_file(Path::new(&meta.path)).empty)
        .or_else(|| metas.first())
}

/// Open a session whose id matches exactly or by file-name containment
/// (so `--session 01a02…` / a partial id / a full file name all work). The
/// TUI `/session` hot-switch calls this with the selector's item value.
pub async fn open_session_by_id(id: &str, cwd: &str) -> Result<Session, OpenError> {
    let dir = default_session_dir(Path::new(cwd));
    open_session_by_id_in_dir(id, cwd, &dir).await
}

async fn open_session_by_id_in_dir(id: &str, cwd: &str, dir: &Path) -> Result<Session, OpenError> {
    let metas = list_session_metadata_in_dir(cwd, dir)
        .await
        .map_err(|e| OpenError::Other(e.to_string()))?;
    let Some(meta) = metas
        .iter()
        .find(|m| m.id == id || m.path.contains(id) || id.contains(&m.id))
    else {
        return Err(OpenError::NotFound {
            requested: format!("session {id}"),
        });
    };
    open_session(meta, cwd).await.map_err(|e| {
        OpenError::Other(match e {
            // `open_session` renders as "Could not open the saved session:
            // <reason>" via `BuildError`. Carry the bare reason so it is not
            // prefixed twice (once here, once by the CLI/TUI).
            BuildError::SessionOpen(reason) => reason,
            other => other.to_string(),
        })
    })
}

/// Publish the active session's id to extensions, on both channels that carry it.
///
/// Called on **every** activation of a session — the startup build AND every
/// in-process swap (`switch_to_session`, `fork_session`, and the extension
/// `NewSession`/`Fork`/`SwitchSession` runtime actions all route through
/// [`set_active_session`]). Three things must stay in step:
///
/// - `ToolCallContext` — read per extension **tool** call, which is why a stale
///   value there made an extension's `todo`-style state follow the wrong session.
/// - `KI_SESSION_ID` — read per **turn** by the provider hooks, which is the
///   only channel an observer gets for a pure-chat run. Process-global, which is
///   exactly the scope of "the session this process is serving".
/// - `KI_AGENT_ID` — the same id, addressed to commands a spawned shell runs
///   (`walgit collab claim --session "$KI_AGENT_ID"`). Deliberately NOT
///   host-private: it is the one channel that must reach a shell. The
///   unconditional overwrite below is what keeps a nested `ki` from reporting
///   an ancestor's id on that channel too.
///
/// The write overwrites unconditionally, deliberately. An inherited value is not
/// an override: `ki` spawning `ki` would otherwise hand the child the parent's
/// session id, and the child — which is *serving its own* session — would report
/// it forever. (Children no longer inherit the variable at all; see
/// `kiqi_tools::HOST_PRIVATE_ENV`. This overwrite is the second line of defence,
/// for any other way a value can arrive pre-set.)
///
/// An embedder that genuinely wants to pin the id can revert to the old
/// behaviour by setting the variable *after* this runs; there is no supported
/// way to pin it before, because a session that does not match its own id is
/// never what the caller wants.
fn publish_session_identity(tool_context: &kiqi_host::ToolCallContext, id: &str) {
    // Both names are published under one lock so no reader can observe a torn
    // pair (`KI_SESSION_ID` already swapped, `KI_AGENT_ID` not yet). See
    // `env_lock` for why this is a shared, re-entrant lock rather than a
    // test-only mutex.
    crate::env_lock::with_env_lock(|| {
        std::env::set_var(kiqi_agent::SESSION_ID_ENV, id);
        // Two audiences, two names: extensions read `KI_SESSION_ID`, while the shell
        // a `bash`/`powershell` tool spawns can name the session through
        // `KI_AGENT_ID`.
        std::env::set_var(kiqi_agent::AGENT_ID_ENV, id);
    });
    tool_context.set_session_id(id);
}

/// The session id a `--no-session` run carries (see [`ephemeral_session`]).
///
/// Not unique across runs, so it never identifies a writer; `kiqi-extension`'s
/// `AGENT_ID_ENV` docs name the same literal as "no session". Declared here so
/// the two places that care (the session itself and [`session_id_from`]) cannot
/// drift apart.
const EPHEMERAL_SESSION_ID: &str = "ephemeral";

/// The attribution to derive from a session id the host is serving.
///
/// `None` when there is no id at all, and for [`EPHEMERAL_SESSION_ID`]: every
/// `--no-session` run carries that same value, so treating it as an identity
/// would let one ephemeral session silently swallow another's update as "its
/// own echo".
///
/// Called with the id of the session the caller is *attached to* — not with
/// [`kiqi_agent::AGENT_ID_ENV`], which `publish_session_identity` rewrites
/// on every activation and which is therefore a spawn-time value for any child
/// process and a process-global cell in-process.
pub(crate) fn session_id_from(published: Option<&str>) -> Option<String> {
    let id = published?.trim();
    if id.is_empty() || id == EPHEMERAL_SESSION_ID {
        return None;
    }
    Some(id.to_string())
}

/// Swap the session the harness serves, republishing the identity to extensions.
///
/// The single funnel for an in-process session change. Prefer this over calling
/// `AgentHarness::set_session` directly: the harness holds no reference to the
/// extension channels, so a bare `set_session` would leave every extension reporting
/// the session it had before the swap. The same holds for `SignalHost`, which
/// caches a `Session` handle — a swap that does not re-attach there keeps
/// persisting `mcp.signals.*` into the previous session's JSONL. Both moves live
/// here so no activation path can forget one.
///
/// `tool_context` is optional because not every caller has one (the harness's own
/// tests, a headless embedder); the env channel is always published, since the
/// provider hooks depend on it.
pub(crate) async fn set_active_session(
    harness: &AgentHarness,
    session: Session,
    tool_context: Option<&kiqi_host::ToolCallContext>,
    signal_host: &crate::signal_host::SignalHost,
) -> Result<(), String> {
    if let Ok(metadata) = session.get_metadata().await {
        // Same lock as `publish_session_identity`: this is the other path that
        // rewrites the id pair, and it runs from a background task while tests
        // (and other tasks) may be reading it.
        //
        // This is the lock that actually fixes the race. Removing it reproduces
        // the flake at 7/20 runs of `extensions_actions::tests` on a 16-thread
        // machine; removing the one in `publish_session_identity` instead does
        // not (0/20), because this path is what the failing test reaches through
        // `HostImpl::new_session`. Keep both: they write the same pair.
        crate::env_lock::with_env_lock(|| {
            std::env::set_var(kiqi_agent::SESSION_ID_ENV, &metadata.id);
            std::env::set_var(kiqi_agent::AGENT_ID_ENV, &metadata.id);
        });
        if let Some(context) = tool_context {
            context.set_session_id(metadata.id);
        }
    }
    // `Session::clone` snapshots the storage slot, so this handle keeps naming
    // the session being installed even after `set_session` moves `session`.
    let signal_session = session.clone();
    harness
        .set_session(session)
        .await
        .map_err(|e| e.to_string())?;
    // Only after a successful swap: on error the harness still serves the old
    // session, which is where the signal host must stay too. A mute-read error
    // must not fail a swap that already happened — keep the previous flag and
    // say so (the same best-effort contract as the startup restore).
    if let Err(error) = signal_host
        .attach_session_and_load_mute(signal_session)
        .await
    {
        tracing::warn!(%error, "could not restore the signal mute for the new session");
    }
    Ok(())
}

/// Fork the harness's current session into a new JSONL session (new id, parent
/// set to the source) and wrap it in a `Session`. Mirrors the TUI's
/// `fork_session` flow (`interactive_tui.rs`) — hoisted here so both the TUI
/// and the extension `runtime_action(Fork)` host share one implementation.
/// Returns the new `Session` (NOT yet swapped onto the harness — the caller
/// does `harness.set_session(...)`).
pub(crate) async fn fork_session_storage(
    harness: &AgentHarness,
    cwd: &str,
) -> Result<Session, String> {
    use kiqi_harness::session::jsonl::{JsonlSessionRepo, JsonlSessionRepoOptions};
    use kiqi_tools::FileSystem;

    let dir = default_session_dir(Path::new(cwd));
    let env = Arc::new(OsExecutionEnv::with_cwd(PathBuf::from(cwd)));
    let fs: Arc<dyn FileSystem> = env.clone();
    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs,
        sessions_root: dir.to_string_lossy().into_owned(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    });
    // The fork needs the rich JSONL metadata (with the on-disk path); resolve
    // it from the session list by the current session's id.
    let id = harness.session().storage().metadata().id.clone();
    let metas = list_session_metadata(cwd)
        .await
        .map_err(|e| e.to_string())?;
    let Some(source) = metas.iter().find(|m| m.id == id) else {
        return Err(format!("current session {id} not found on disk"));
    };
    let fork_storage = repo
        .fork_typed(
            source,
            &kiqi_harness::session::jsonl::JsonlSessionCreateOptions {
                id: None,
                parent_session_id: Some(source.id.clone()),
                cwd: cwd.to_string(),
                metadata: None,
            },
            &kiqi_harness::session::types::ForkOptions::default(),
        )
        .await
        .map_err(|e| e.to_string())?;
    claim_session_lock(&fork_storage.jsonl_metadata().path)?;
    Ok(Session::new(Arc::new(fork_storage), None))
}

/// Session logs this process has claimed, so re-claiming is a no-op.
static SESSION_LOCK_PATHS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
/// The held lock handles. Kept alive for the process lifetime — dropping the
/// `File` would release the OS lock, so the guard is parked here.
static SESSION_LOCKS: OnceLock<Mutex<Vec<std::fs::File>>> = OnceLock::new();

/// Claim the exclusive lock for a session log.
///
/// Two ki processes appending to one JSONL diverge their `sequence` counters
/// (duplicate / gapped seqs) and corrupt it. The lock is an OS advisory lock on
/// `<path>.lock`: the second process gets a clear "already open" error instead
/// of corrupting the log, and the OS releases it automatically when the holder
/// exits (no stale-lock cleanup). Re-claiming a path this process already holds
/// is a no-op, so the TUI's session hot-switch can re-open freely.
fn claim_session_lock(session_path: &str) -> Result<(), String> {
    let lock_path = PathBuf::from(format!("{session_path}.lock"));
    if SESSION_LOCK_PATHS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap()
        .contains(&lock_path)
    {
        return Ok(());
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("open session lock {}: {e}", lock_path.display()))?;
    if fs2::FileExt::try_lock_exclusive(&file).is_err() {
        return Err(format!(
            "session {session_path} is already open in another ki process"
        ));
    }
    SESSION_LOCKS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap()
        .push(file);
    SESSION_LOCK_PATHS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap()
        .insert(lock_path);
    Ok(())
}

/// Wrap an opened [`JsonlSessionStorage`] in the `Session` facade (shared by
/// startup restore + TUI hot-switch).
async fn open_session(
    meta: &kiqi_harness::session::jsonl::JsonlSessionMetadata,
    cwd: &str,
) -> Result<Session, BuildError> {
    use kiqi_harness::session::jsonl::{JsonlSessionRepo, JsonlSessionRepoOptions};
    use kiqi_harness::session::types::SessionStorage;
    use kiqi_tools::FileSystem;

    let dir = default_session_dir(Path::new(cwd));
    let env = Arc::new(OsExecutionEnv::with_cwd(PathBuf::from(cwd)));
    let fs: Arc<dyn FileSystem> = env.clone();
    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs: fs.clone(),
        sessions_root: dir.to_string_lossy().into_owned(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    });
    claim_session_lock(&meta.path).map_err(BuildError::SessionOpen)?;
    let storage = repo
        .open_by_jsonl_metadata(meta)
        .await
        .map_err(|e| BuildError::SessionOpen(format!("open {}: {e}", meta.path)))?;
    // A session records the cwd it was created in. When that directory has
    // since been removed, continuing silently in it is confusing; warn the
    // user that we fell back to the process cwd (pi `getMissingSessionCwdIssue`).
    if let Some(issue) = crate::session_cwd::get_missing_session_cwd_issue(
        Some(Path::new(&meta.path)),
        Path::new(&meta.cwd),
        Path::new(cwd),
    ) {
        eprintln!(
            "warning: {}",
            crate::session_cwd::format_missing_session_cwd_error(&issue)
        );
    }
    let storage_arc: Arc<dyn SessionStorage> = Arc::new(storage);
    Ok(Session::new(storage_arc, None))
}

/// A fresh ephemeral in-memory session (no persistence). Used for `--no-session`.
fn ephemeral_session() -> Session {
    let storage = Arc::new(InMemorySessionStorage::new(
        SessionMetadata {
            id: EPHEMERAL_SESSION_ID.into(),
            created_at: 0,
            parent_session_id: None,
        },
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    ));
    Session::new(storage, None)
}

/// Create a fresh JSONL session file under `dir` and wrap it in a `Session`.
///
/// Uses the `JsonlSessionRepo` over an `OsExecutionEnv`-backed `FileSystem`
/// rooted at the cwd, so paths resolve consistently with the tools. Mirrors the
/// TS `SessionManager.create` flow (header write + `JsonlSessionStorage` open).
/// Create a fresh JSONL session file under `dir` and wrap it in a `Session`.
///
/// Uses the `JsonlSessionRepo` over an `OsExecutionEnv`-backed `FileSystem`
/// rooted at the cwd, so paths resolve consistently with the tools. Mirrors the
/// TS `SessionManager.create` flow (header write + `JsonlSessionStorage` open).
pub(crate) async fn create_jsonl_session(dir: &Path, cwd: &str) -> Result<Session, String> {
    create_jsonl_session_with_id(dir, cwd, None).await
}

/// `create_jsonl_session` with an explicit id (the `--session-id` fixed-id
/// contract: the file is named with the given id so later `--session-id`
/// launches restore the same session).
pub(crate) async fn create_jsonl_session_with_id(
    dir: &Path,
    cwd: &str,
    id: Option<String>,
) -> Result<Session, String> {
    use kiqi_harness::session::jsonl::{
        JsonlSessionCreateOptions, JsonlSessionRepo, JsonlSessionRepoOptions,
    };
    use kiqi_tools::FileSystem;

    // A dedicated OS env for session-file I/O, rooted at the cwd so the repo's
    // relative-path resolution matches the tool env.
    let env = Arc::new(OsExecutionEnv::with_cwd(PathBuf::from(cwd)));
    let fs: Arc<dyn FileSystem> = env.clone();

    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs: fs.clone(),
        sessions_root: dir.to_string_lossy().into_owned(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    });

    let opts = JsonlSessionCreateOptions {
        id, // fresh uuidv7 when None (--session-id passes the fixed id)
        parent_session_id: None,
        cwd: cwd.to_string(),
        metadata: None,
    };
    let storage = repo
        .create_typed(&opts)
        .await
        .map_err(|e| format!("create session: {e}"))?;
    claim_session_lock(&storage.jsonl_metadata().path)
        .map_err(|e| format!("create session: {e}"))?;
    // `JsonlSessionStorage` implements `SessionStorage`; wrap in the facade.
    let storage_arc: Arc<dyn kiqi_harness::session::types::SessionStorage> = Arc::new(storage);
    Ok(Session::new(storage_arc, None))
}

/// Read an `--append-system-prompt` target: if it's a readable file path, return
/// its contents; otherwise return `None` and let the caller use the literal.
fn read_append_target(target: &str, sources: &mut Vec<String>) -> Option<String> {
    let path = Path::new(target);
    if path.is_file() {
        read_prompt_source(path, sources)
    } else {
        None
    }
}

/// Record provenance at the successful read, rather than rediscovering files
/// during rendering (which could report a shadowed or unreadable file).
fn read_prompt_source(path: &Path, sources: &mut Vec<String>) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let source = absolute.to_string_lossy().into_owned();
    if !sources.contains(&source) {
        sources.push(source);
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::Args;
    use std::sync::atomic::Ordering;

    /// End-to-end: `~/.agents/AGENTS.md` must reach the prompt that the CLI hands
    /// the harness, ahead of the ki-specific agent-dir file. No other test covers
    /// that chain — the loader is unit-tested and the binary is checked by hand,
    /// but a wiring break between loading and composition would leave both green.
    ///
    /// Scope note: this asserts on `get_system_prompt()`, which is the base prompt
    /// the CLI composes (base → append → context). The `<available_skills>` section
    /// is appended per *run* by the harness (`compose_prompt`), so it is not
    /// visible here — it is pinned in `kiqi-harness`'s
    /// `the_skills_listing_reaches_the_provider_without_the_skill_body`.
    ///
    /// Runs in a subprocess because it needs its own `KI_CODING_AGENT_DIR` and
    /// `KI_AGENTS_DIR`, and the crate's env-mutating tests share one lock.
    /// The prompt must advertise the tools the session actually has.
    ///
    /// The "Available tools:" block used to be a hard-coded string, so
    /// `--no-builtin-tools` removed `read`/`bash`/`edit`/`write`/`docs` while the
    /// prompt kept promising them — the model then calls a tool it does not have and
    /// spends turns on `Tool … not found`. Measured on the real binary before the fix:
    /// with `--no-builtin-tools` the dump still listed all five.
    ///
    /// `None` keeps the historical block for callers that do not know the tool set;
    /// `Some(&[])` must say so plainly rather than print an empty heading.
    /// `/reload` must rebuild the prompt against the tools *after* re-merging, not
    /// against the previous set.
    ///
    /// The tool rebuild used to sit below the prompt composition in
    /// `reload_extension_resources`, so every reload rendered the list from the last
    /// reload's tools — and on the first reload, from whatever `build` had produced
    /// before the flag-driven filtering that only reload re-applies.
    ///
    /// Subprocess-isolated: the crate's env-mutating tests share one lock, and this
    /// one writes a config dir.
    #[test]
    fn reload_composes_the_prompt_from_the_tools_it_just_rebuilt() {
        const CHILD: &str = "KI_RELOAD_TOOL_PROMPT_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "session::tests::reload_composes_the_prompt_from_the_tools_it_just_rebuilt",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env(crate::config::CONFIG_DIR_ENV, dir.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let agent = crate::config::agent_dir().unwrap();
                let cwd = agent.join("project");
                std::fs::create_dir_all(&cwd).unwrap();
                let resolved = crate::provider::resolve(
                    Some("anthropic"),
                    Some(crate::provider::DEFAULT_MODEL_ID),
                    None,
                    Some("test-key"),
                    None,
                )
                .unwrap();
                // A custom system prompt would bypass the default block, so leave it
                // unset and let the default (tool-aware) prompt be used.
                let args = Args {
                    no_session: true,
                    no_extensions: true,
                    no_prompt_templates: true,
                    no_context_files: true,
                    trust_override: Some(true),
                    ..Args::default()
                };
                let (harness, _events, context) =
                    build(&resolved, &args, &cwd, true).await.unwrap();
                assert!(
                    !reload_extension_resources(&harness, &context)
                        .await
                        .had_errors
                );

                let prompt = harness.get_system_prompt().await.unwrap().unwrap();
                let tools: Vec<String> = harness
                    .get_tools()
                    .await
                    .unwrap()
                    .iter()
                    .map(|tool| tool.tool.schema().name.clone())
                    .collect();
                assert!(!tools.is_empty(), "the default session has tools");
                for name in &tools {
                    assert!(
                        prompt.contains(&format!("- {name}")),
                        "reload must advertise `{name}` in the prompt it composes: {prompt}"
                    );
                }
            });
    }

    /// Guidance that names a tool must disappear when that tool does.
    ///
    /// The tool list was made truthful in 4a3aad2, but the prose below it kept saying
    /// "the bash tool runs Bash …" and "track work with the todo tool" regardless — so
    /// `--no-tools` produced a prompt that named no tools in the list and then advised
    /// using two of them. Same class of lie, one layer down.
    #[test]
    fn guidance_about_a_tool_is_dropped_when_that_tool_is_not_available() {
        let none = default_system_prompt_for_tools(".", false, Some(&[]));
        assert!(
            !none.contains("The bash tool runs"),
            "the bash rule must go with bash: {none}"
        );
        assert!(
            !none.contains("with the todo tool"),
            "the todo rule must go with todo: {none}"
        );

        // Only bash: the todo rule survives, untouched.
        let only_bash = vec!["bash".to_string()];
        let with_bash = default_system_prompt_for_tools(".", false, Some(&only_bash));
        assert!(with_bash.contains("The bash tool runs"), "{with_bash}");
        assert!(
            !with_bash.contains("with the todo tool"),
            "todo is absent here: {with_bash}"
        );

        // Unknown tool set keeps both, matching the historical prompt.
        let unknown = default_system_prompt_for_tools(".", false, None);
        assert!(unknown.contains("The bash tool runs"), "{unknown}");
        assert!(unknown.contains("with the todo tool"), "{unknown}");
    }

    #[test]
    fn the_prompt_lists_the_tools_the_session_actually_has() {
        let absent = default_system_prompt_for_tools(".", false, Some(&[]));
        assert!(
            absent.contains("No tools are available"),
            "an empty tool set must say so: {absent}"
        );
        assert!(
            !absent.contains("- bash"),
            "and must not list a tool that is gone: {absent}"
        );

        let some = vec!["todo".to_string(), "bash".to_string()];
        let listed = default_system_prompt_for_tools(".", false, Some(&some));
        assert!(listed.contains("- todo"), "{listed}");
        assert!(
            listed.contains("- bash  — Execute Bash"),
            "a known tool keeps its description: {listed}"
        );
        assert!(
            !listed.contains("- read"),
            "a tool that is not in the set must not be advertised: {listed}"
        );

        // `None` = unknown, so the historical block stays (tests and `/debug`).
        let unknown = default_system_prompt_for_tools(".", false, None);
        assert!(
            unknown.contains("- read  — Read file contents"),
            "{unknown}"
        );
    }

    #[test]
    fn the_composed_prompt_carries_shared_agents_instructions_before_the_ki_ones() {
        const CHILD: &str = "KI_SHARED_AGENTS_PROMPT_REGRESSION_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "session::tests::the_composed_prompt_carries_shared_agents_instructions_before_the_ki_ones",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env(crate::config::CONFIG_DIR_ENV, dir.path().join("agent"))
                .env(crate::config::AGENTS_DIR_ENV, dir.path().join("shared"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let agent = crate::config::agent_dir().unwrap();
                let shared = crate::config::agents_dir().expect("KI_AGENTS_DIR is set");
                let cwd = agent.join("project");
                std::fs::create_dir_all(&cwd).unwrap();
                std::fs::create_dir_all(&shared).unwrap();
                std::fs::write(shared.join("AGENTS.md"), "SHARED_GLOBAL_INSTRUCTIONS").unwrap();
                std::fs::write(agent.join("AGENTS.md"), "KI_GLOBAL_INSTRUCTIONS").unwrap();

                let resolved = crate::provider::resolve(
                    Some("anthropic"),
                    Some(crate::provider::DEFAULT_MODEL_ID),
                    None,
                    Some("test-key"),
                    None,
                )
                .unwrap();
                let args = Args {
                    no_session: true,
                    no_extensions: true,
                    no_prompt_templates: true,
                    trust_override: Some(true),
                    ..Default::default()
                };
                // `true`: a trusted project is what makes the context chain load at
                // all (`session.rs` short-circuits the whole block otherwise).
                let (harness, _events, _context) =
                    build(&resolved, &args, &cwd, true).await.unwrap();
                let prompt = harness.get_system_prompt().await.unwrap().unwrap();

                assert!(
                    prompt.contains("SHARED_GLOBAL_INSTRUCTIONS"),
                    "`~/.agents/AGENTS.md` must reach the prompt: {prompt}"
                );
                assert!(
                    prompt.contains("KI_GLOBAL_INSTRUCTIONS"),
                    "the agent-dir AGENTS.md must still reach the prompt: {prompt}"
                );
                let shared_at = prompt.find("SHARED_GLOBAL_INSTRUCTIONS").unwrap();
                let ki_at = prompt.find("KI_GLOBAL_INSTRUCTIONS").unwrap();
                assert!(
                    shared_at < ki_at,
                    "the shared (most general) instructions must come first"
                );
            });
    }

    /// `/reload` must re-read `~/.agents`, not just the ki dirs.
    ///
    /// `reload_extension_resources` re-runs the Part-A loaders, and the shared
    /// directory is part of that chain (`load_project_context_files_with_globals`).
    /// If the reload path ever stopped passing `agents_dir()`, a file added while
    /// the session runs would never appear — and the initial-build test would stay
    /// green, because it only checks the first composition.
    ///
    /// Subprocess-isolated: needs its own `KI_AGENTS_DIR` / `KI_CODING_AGENT_DIR`,
    /// and the crate's env-mutating tests share one lock.
    #[test]
    fn reload_picks_up_a_shared_agents_file_added_mid_session() {
        const CHILD: &str = "KI_SHARED_AGENTS_RELOAD_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "session::tests::reload_picks_up_a_shared_agents_file_added_mid_session",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env(crate::config::CONFIG_DIR_ENV, dir.path().join("agent"))
                .env(crate::config::AGENTS_DIR_ENV, dir.path().join("shared"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let agent = crate::config::agent_dir().unwrap();
                let shared = crate::config::agents_dir().expect("KI_AGENTS_DIR is set");
                let cwd = agent.join("project");
                std::fs::create_dir_all(&cwd).unwrap();
                std::fs::create_dir_all(&shared).unwrap();

                let resolved = crate::provider::resolve(
                    Some("anthropic"),
                    Some(crate::provider::DEFAULT_MODEL_ID),
                    None,
                    Some("test-key"),
                    None,
                )
                .unwrap();
                let args = Args {
                    no_session: true,
                    no_extensions: true,
                    no_skills: true,
                    no_prompt_templates: true,
                    trust_override: Some(true),
                    ..Default::default()
                };
                let (harness, _events, context) =
                    build(&resolved, &args, &cwd, true).await.unwrap();

                // Nothing shared yet.
                let before = harness.get_system_prompt().await.unwrap().unwrap();
                assert!(
                    !before.contains("SHARED_ARRIVED_LATER"),
                    "precondition: the marker must not be there yet"
                );

                // The user drops a shared instruction file while the session runs.
                std::fs::write(shared.join("AGENTS.md"), "SHARED_ARRIVED_LATER").unwrap();
                assert!(
                    !reload_extension_resources(&harness, &context)
                        .await
                        .had_errors
                );

                let after = harness.get_system_prompt().await.unwrap().unwrap();
                assert!(
                    after.contains("SHARED_ARRIVED_LATER"),
                    "`/reload` must re-read `~/.agents`: {after}"
                );
            });
    }

    #[test]
    fn loaded_context_paths_track_precedence_and_reload() {
        const CHILD: &str = "KI_CONTEXT_PATHS_REGRESSION_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "session::tests::loaded_context_paths_track_precedence_and_reload",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env(crate::config::CONFIG_DIR_ENV, dir.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let agent = crate::config::agent_dir().unwrap();
                let cwd = agent.join("project");
                let project_config = cwd.join(".ki");
                std::fs::create_dir_all(&project_config).unwrap();
                for (path, content) in [
                    (agent.join("SYSTEM.md"), "GLOBAL_BASE"),
                    (agent.join("APPEND_SYSTEM.md"), "GLOBAL_APPEND"),
                    (agent.join("AGENTS.md"), "GLOBAL_INSTRUCTIONS"),
                    (project_config.join("SYSTEM.md"), "PROJECT_BASE"),
                    (project_config.join("APPEND_SYSTEM.md"), "PROJECT_APPEND"),
                    (cwd.join("AGENTS.md"), "PROJECT_INSTRUCTIONS"),
                    (cwd.join("CLAUDE.md"), "SHADOWED_CLAUDE"),
                ] {
                    std::fs::write(path, content).unwrap();
                }
                let resolved = crate::provider::resolve(
                    Some("anthropic"),
                    Some(crate::provider::DEFAULT_MODEL_ID),
                    None,
                    Some("test-key"),
                    None,
                )
                .unwrap();
                let args = Args {
                    no_session: true,
                    no_extensions: true,
                    no_skills: true,
                    no_prompt_templates: true,
                    trust_override: Some(true),
                    ..Default::default()
                };
                let (harness, _events, mut context) =
                    build(&resolved, &args, &cwd, true).await.unwrap();
                let resources = harness.get_resources().await.unwrap();
                let path = |p: PathBuf| p.to_string_lossy().into_owned();
                assert_eq!(
                    &resources.context_files[..2],
                    &[
                        path(project_config.join("SYSTEM.md")),
                        path(project_config.join("APPEND_SYSTEM.md")),
                    ]
                );
                assert!(resources
                    .context_files
                    .contains(&path(agent.join("AGENTS.md"))));
                assert!(resources
                    .context_files
                    .contains(&path(cwd.join("AGENTS.md"))));
                assert!(!resources
                    .context_files
                    .contains(&path(cwd.join("CLAUDE.md"))));
                assert!(!resources
                    .context_files
                    .contains(&path(agent.join("SYSTEM.md"))));
                let prompt = harness.get_system_prompt().await.unwrap().unwrap();
                assert!(prompt.contains("PROJECT_BASE") && prompt.contains("PROJECT_APPEND"));
                assert!(!prompt.contains("GLOBAL_BASE") && !prompt.contains("SHADOWED_CLAUDE"));

                // Removing the selected project files makes reload fall back to
                // globals; removed instruction paths must disappear as well.
                std::fs::remove_file(project_config.join("SYSTEM.md")).unwrap();
                std::fs::remove_file(project_config.join("APPEND_SYSTEM.md")).unwrap();
                std::fs::remove_file(cwd.join("AGENTS.md")).unwrap();
                assert!(
                    !reload_extension_resources(&harness, &context)
                        .await
                        .had_errors
                );
                let resources = harness.get_resources().await.unwrap();
                assert_eq!(
                    &resources.context_files[..2],
                    &[
                        path(agent.join("SYSTEM.md")),
                        path(agent.join("APPEND_SYSTEM.md")),
                    ]
                );
                assert!(resources
                    .context_files
                    .contains(&path(cwd.join("CLAUDE.md"))));
                assert!(!resources
                    .context_files
                    .contains(&path(project_config.join("SYSTEM.md"))));

                // Explicit append files are listed; literal text and overridden
                // discovered base/append files are not invented as file sources.
                let explicit = agent.join("custom-append.md");
                std::fs::write(&explicit, "EXPLICIT_APPEND").unwrap();
                context.args.system_prompt = Some("EXPLICIT_BASE".into());
                context.args.append_system_prompt =
                    vec![path(explicit.clone()), "literal append".into()];
                context.args.no_context_files = true;
                assert!(
                    !reload_extension_resources(&harness, &context)
                        .await
                        .had_errors
                );
                assert_eq!(
                    harness.get_resources().await.unwrap().context_files,
                    vec![path(explicit)]
                );

                // The reload must honor the startup opt-out from project context.
                context.args.system_prompt = None;
                context.args.append_system_prompt.clear();
                context.args.no_context_files = false;
                context.args.trust_override = Some(false);
                assert!(
                    !reload_extension_resources(&harness, &context)
                        .await
                        .had_errors
                );
                assert!(harness
                    .get_resources()
                    .await
                    .unwrap()
                    .context_files
                    .is_empty());
                let prompt = harness.get_system_prompt().await.unwrap().unwrap();
                assert!(
                    !prompt.contains("SHADOWED_CLAUDE") && !prompt.contains("GLOBAL_INSTRUCTIONS")
                );
            });
    }

    #[test]
    fn prompt_source_read_failures_and_literals_do_not_create_file_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("SYSTEM.md");
        let mut sources = Vec::new();
        assert!(read_prompt_source(&file, &mut sources).is_none());
        std::fs::write(&file, [0xff, 0xfe]).unwrap();
        assert!(read_prompt_source(&file, &mut sources).is_none());
        assert!(read_append_target("literal instructions", &mut sources).is_none());
        assert!(sources.is_empty());
        std::fs::write(&file, "loaded instructions").unwrap();
        assert_eq!(
            read_prompt_source(&file, &mut sources).as_deref(),
            Some("loaded instructions")
        );
        assert!(read_prompt_source(&file, &mut sources).is_some());
        assert_eq!(sources, vec![file.to_string_lossy().into_owned()]);
    }

    async fn invoke_tool_hook(
        hook: &kiqi_agent::BeforeToolCall,
        tool: &str,
        args: serde_json::Value,
    ) -> Option<kiqi_agent::BeforeToolCallResult> {
        let message = kiqi_ai::providers::faux::faux_assistant_message(
            "approval",
            kiqi_ai::types::StopReason::ToolUse,
        );
        let kiqi_ai::types::Content::ToolCall(call) =
            kiqi_ai::types::Content::tool_call("call", tool, args)
        else {
            unreachable!()
        };
        hook(
            kiqi_agent::BeforeToolCallContext {
                assistant_message: &message,
                tool_call: &call,
                args: &call.arguments,
                context: &kiqi_agent::AgentContext::default(),
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
    }

    /// A project directory with an empty policy file, so a policy test never
    /// falls through to the developer's real global `permissions.json`.
    fn hook_project() -> (tempfile::TempDir, std::path::PathBuf) {
        let project = tempfile::TempDir::new().unwrap();
        let policy = project.path().join(".ki").join("permissions.json");
        std::fs::create_dir_all(policy.parent().unwrap()).unwrap();
        std::fs::write(&policy, r#"{"permissions":{}}"#).unwrap();
        (project, policy)
    }

    #[tokio::test]
    async fn host_permission_gate_blocks_and_allow_lists_restrict() {
        let (project, policy) = hook_project();
        std::fs::write(
            &policy,
            r#"{"permissions":{"allow":["Bash(git *)"],"deny":["Bash(git push *)"]}}"#,
        )
        .unwrap();
        let hook = extension_tool_call_hook(
            project.path().to_path_buf(),
            true,
            std::sync::Arc::new(std::sync::Mutex::new(kiqi_host::ExtensionSession::none())),
        );

        // Deny wins, and the reason names both the rule and the tool.
        let result = invoke_tool_hook(
            &hook,
            "bash",
            serde_json::json!({"command": "git push origin main"}),
        )
        .await
        .expect("the deny rule blocks");
        assert!(result.block);
        let reason = result.reason.unwrap();
        assert!(reason.contains("Bash(git push *)"), "got {reason}");
        assert!(reason.contains("bash"), "got {reason}");

        // A matching allow rule runs.
        assert!(
            invoke_tool_hook(&hook, "bash", serde_json::json!({"command": "git status"}))
                .await
                .is_none()
        );

        // An unmatched call is restricted while the allow list exists.
        // A target outside the policy's ancestors, so the allow-list rule (not
        // the policy-path guard) is what produces the denial.
        let result = invoke_tool_hook(
            &hook,
            "bash",
            serde_json::json!({"command": "rm -rf build/out"}),
        )
        .await
        .expect("the allow list restricts");
        assert!(result.block);
        assert!(result.reason.unwrap().contains("allow"));

        // No allow rule names `read`, so it keeps the default-allow behavior.
        assert!(
            invoke_tool_hook(&hook, "read", serde_json::json!({"path": "src/main.rs"}))
                .await
                .is_none()
        );

        // An edit is visible to the next call without rebuilding the hook —
        // the same property that makes `/reload` pick up policy changes.
        std::fs::write(&policy, r#"{"permissions":{}}"#).unwrap();
        assert!(invoke_tool_hook(
            &hook,
            "bash",
            serde_json::json!({"command": "git push origin main"})
        )
        .await
        .is_none());
    }

    #[tokio::test]
    async fn host_permission_gate_ignores_an_untrusted_project_policy() {
        // Isolate the global lookup so the developer's real policy cannot
        // affect or receive this test.
        let _guard = crate::config::test_support::env_lock().lock();
        let previous = std::env::var_os("KI_AGENT_DIR");
        let global = tempfile::TempDir::new().unwrap();
        std::env::set_var("KI_AGENT_DIR", global.path());

        let (project, policy) = hook_project();
        std::fs::write(&policy, r#"{"permissions":{"deny":["Bash(*)"]}}"#).unwrap();
        let make_hook = |project_trusted: bool| {
            extension_tool_call_hook(
                project.path().to_path_buf(),
                project_trusted,
                std::sync::Arc::new(std::sync::Mutex::new(kiqi_host::ExtensionSession::none())),
            )
        };

        // An untrusted project's policy is project-controlled input: ignore it.
        assert!(invoke_tool_hook(
            &make_hook(false),
            "bash",
            serde_json::json!({"command": "echo hi"})
        )
        .await
        .is_none());
        // The same session with project trust enforces it.
        assert!(invoke_tool_hook(
            &make_hook(true),
            "bash",
            serde_json::json!({"command": "echo hi"})
        )
        .await
        .is_some());

        match previous {
            Some(value) => std::env::set_var("KI_AGENT_DIR", value),
            None => std::env::remove_var("KI_AGENT_DIR"),
        }
    }

    /// A unique scratch file for one summary test.
    fn scratch(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "kiqi-summary-{}-{nanos}-{name}.jsonl",
            std::process::id()
        ));
        path
    }

    #[test]
    fn summary_marks_a_header_only_session_empty() {
        let path = scratch("empty");
        std::fs::write(
            &path,
            "{\"kind\":\"header\",\"version\":4,\"id\":\"abc\",\"createdAt\":1,\"cwd\":\"x\"}\n",
        )
        .unwrap();
        let summary = summarize_session_file(&path);
        assert!(summary.empty, "a header-only file has nothing to resume");
        assert!(summary.preview.is_none());
        assert_eq!(summary.bytes, std::fs::metadata(&path).unwrap().len());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn summary_reads_the_first_user_prompt_and_name() {
        let path = scratch("content");
        let body = concat!(
            "{\"kind\":\"header\",\"version\":4,\"id\":\"abc\",\"createdAt\":1,\"cwd\":\"x\"}\n",
            "{\"kind\":\"fact\",\"seq\":1,\"fact\":\"name\",\"name\":\"fix resume picker\"}\n",
            "{\"kind\":\"record\",\"type\":\"operation_started\",\"id\":\"r1\"}\n",
            "{\"kind\":\"entry\",\"lane\":\"main\",\"type\":\"message\",\"id\":\"e1\",\"message\":{\"kind\":\"user\",\"role\":\"user\",\"content\":\"read the ki -r picker\"}}\n",
            "{\"kind\":\"entry\",\"lane\":\"main\",\"type\":\"message\",\"id\":\"e2\",\"message\":{\"kind\":\"assistant\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"second\"}]}}\n",
        );
        std::fs::write(&path, body).unwrap();
        let summary = summarize_session_file(&path);
        assert!(!summary.empty);
        assert_eq!(summary.name.as_deref(), Some("fix resume picker"));
        assert_eq!(summary.preview.as_deref(), Some("read the ki -r picker"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn summary_extracts_a_block_style_user_prompt() {
        let path = scratch("blocks");
        let body = concat!(
            "{\"kind\":\"header\",\"version\":4,\"id\":\"abc\",\"createdAt\":1,\"cwd\":\"x\"}\n",
            "{\"kind\":\"entry\",\"type\":\"message\",\"id\":\"e1\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"line one\\nline two\"}]}}\n",
        );
        std::fs::write(&path, body).unwrap();
        let summary = summarize_session_file(&path);
        assert_eq!(summary.preview.as_deref(), Some("line one line two"));
        assert_eq!(session_display_label(&summary), "line one line two");
        std::fs::remove_file(&path).ok();
    }

    fn meta_for(path: &Path, id: &str) -> kiqi_harness::session::jsonl::JsonlSessionMetadata {
        kiqi_harness::session::jsonl::JsonlSessionMetadata {
            id: id.to_string(),
            created_at: 0,
            cwd: ".".to_string(),
            path: path.to_string_lossy().into_owned(),
            modified_at: 0,
            source_format: kiqi_harness::session::jsonl::JsonlSourceFormat::V4,
            parent_session_id: None,
            legacy_parent_session_path: None,
            metadata: None,
        }
    }

    #[test]
    fn continue_skips_newer_empty_sessions() {
        let empty = scratch("newest-empty");
        std::fs::write(
            &empty,
            "{\"kind\":\"header\",\"version\":4,\"id\":\"empty\",\"createdAt\":1,\"cwd\":\"x\"}\n",
        )
        .unwrap();
        let content = scratch("older-content");
        std::fs::write(
            &content,
            concat!(
                "{\"kind\":\"header\",\"version\":4,\"id\":\"old\",\"createdAt\":1,\"cwd\":\"x\"}\n",
                "{\"kind\":\"entry\",\"type\":\"message\",\"id\":\"e1\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n",
            ),
        )
        .unwrap();

        let metas = vec![meta_for(&empty, "empty"), meta_for(&content, "old")];
        let picked = first_resumable_session(&metas).expect("a resumable session");
        assert_eq!(picked.id, "old");

        // When every session is empty, fall back to the newest rather than
        // erroring.
        let all_empty = vec![meta_for(&empty, "empty")];
        assert_eq!(first_resumable_session(&all_empty).unwrap().id, "empty");

        std::fs::remove_file(&empty).ok();
        std::fs::remove_file(&content).ok();
    }

    #[test]
    fn list_session_files_sync_walks_the_cwd_subdirectory() {
        let root = std::env::temp_dir().join(format!("kiqi-summary-{}-dir", std::process::id()));
        let nested = root.join("--proj--");
        std::fs::create_dir_all(&nested).unwrap();
        let file = nested.join("2026-01-01_a.jsonl");
        std::fs::write(&file, "{}\n").unwrap();
        let found = list_session_files_sync(&root);
        assert!(found.iter().any(|p| p == &file));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn default_prompt_mentions_cwd_and_tools() {
        let p = default_system_prompt("/tmp/proj", false);
        assert!(p.contains("/tmp/proj"));
        assert!(p.contains("read"));
        assert!(p.contains("bash"));
        assert!(p.contains("edit"));
        assert!(p.contains("write"));
        assert!(p.contains("docs"));
        assert!(!p.contains("- grep"));
        assert!(!p.contains("- find"));
        assert!(!p.contains("- ls"));
        assert!(!p.contains("- powershell"));
        assert!(p.contains("Bash/POSIX syntax, including on Windows"));
        assert!(p.contains("do not pass PowerShell cmdlets"));
    }

    #[test]
    fn default_prompt_instructs_language_matching() {
        // The model must detect the user's language and answer in it, so a
        // Chinese (or any non-English) user gets a same-language reply without
        // having to ask every turn.
        let p = default_system_prompt("/tmp/proj", false);
        let lower = p.to_lowercase();
        assert!(
            lower.contains("same language") && lower.contains("detect"),
            "the prompt must tell the model to detect and mirror the user's language: {p}"
        );
    }

    #[test]
    fn default_prompt_keeps_working_state_in_the_visible_channel() {
        // Regression guard for the re-plan loop: on openai-compatible providers
        // the assistant's `thinking` blocks are dropped when the next request is
        // built (`providers/openai_completions.rs`: `Content::text_only`), so a
        // plan that lives only in reasoning is re-derived every turn. One
        // observed session paid for that 112 times in a single run, and twice
        // hallucinated the user's input ("The user is greeting me with hello",
        // with no such message). See `docs/llm-repetition-forensics.md` §八.
        let p = default_system_prompt("/tmp/proj", false);
        let lower = p.to_lowercase();
        // The rule must state the mechanism, not just "be clear".
        assert!(
            lower.contains("reasoning is not") && lower.contains("carried into your next turn"),
            "the prompt must explain that reasoning does not survive the turn: {p}"
        );
        // …and the remedy: keep state visible, use the todo tool.
        assert!(
            lower.contains("visible") && lower.contains("todo tool"),
            "the prompt must point at visible text and the todo tool: {p}"
        );
        // …plus the anti-repetition rule itself.
        assert!(
            lower.contains("re-stating the same plan"),
            "the prompt must call out plan re-statement as a bug: {p}"
        );
        // And the guidance must not instruct re-reading what is already shown.
        assert!(
            lower.contains("already shown earlier"),
            "the prompt must discourage re-fetching known content: {p}"
        );
    }

    #[test]
    fn default_prompt_does_not_claim_reasoning_is_lost_when_it_is_replayed() {
        // `compat.requiresThinkingAsText` (and the native thinking channels of
        // anthropic-messages / openai-responses) DO carry the model's reasoning
        // back on the next request. Telling such a model "reasoning is not
        // carried into your next turn" is false, and the instruction to restate
        // the plan every turn is exactly the busy-work the re-plan analysis
        // measured. The wording must flip with the mechanism.
        let p = default_system_prompt("/tmp/proj", true);
        let lower = p.to_lowercase();
        assert!(
            lower.contains("carried back"),
            "a replaying model must be told its reasoning survives: {p}"
        );
        assert!(
            !lower.contains("reasoning is not"),
            "the replayed variant must not claim reasoning is dropped: {p}"
        );
        assert!(
            lower.contains("do not re-derive"),
            "the replayed variant must forbid re-deriving the plan: {p}"
        );
        assert!(
            lower.contains("do not re-derive") && lower.contains("re-listing the same plan"),
            "the anti-repetition rule must survive the flip: {p}"
        );
        assert!(
            lower.contains("already shown earlier"),
            "the re-read rule must survive the flip: {p}"
        );
    }

    /// Only side-effect-free tools may be replayed after a crash.
    ///
    /// Regression guard for an inverted default: `ToolReplay` used to default to
    /// `Safe` and the CLI then marked *every* tool `Safe`, so the first time
    /// recovery acted on the flag it would have re-run `bash`/`write`/`edit` and
    /// duplicated their side effects. upstream defaults to `"never"`
    /// (`drive/tools.ts: tool.replay ?? "never"`).
    #[test]
    fn only_read_only_tools_are_replayable() {
        let env = Arc::new(OsExecutionEnv::with_cwd(PathBuf::from(".")));
        let env_dyn: Arc<dyn kiqi_tools::ExecutionEnv> = env.clone();
        let mut_env: Arc<dyn kiqi_tools::MutatingEnv> = env.clone();
        let context = ExecutionToolContext::new(env_dyn, Some(mut_env));
        let signal_host = Arc::new(crate::signal_host::SignalHost::new());
        let tools = build_tools(
            &context,
            &Args::default(),
            Path::new("."),
            &signal_host,
            &crate::skill_tool::new_skill_cell(),
        );

        let replay_of = |name: &str| {
            tools
                .iter()
                .find(|tool| tool.tool.schema().name == name)
                .map(|tool| tool.replay)
        };

        for read_only in ["read", "docs"] {
            assert_eq!(
                replay_of(read_only),
                Some(ToolReplay::Safe),
                "{read_only} is side-effect free, so recovery may re-run it"
            );
        }
        for mutating in ["bash", "edit", "write", "powershell"] {
            if let Some(replay) = replay_of(mutating) {
                assert_eq!(
                    replay,
                    ToolReplay::Never,
                    "{mutating} must never be replayed: re-running it would duplicate its effect"
                );
            }
        }
    }

    #[test]
    fn default_tools_include_docs_lookup() {
        let env = Arc::new(OsExecutionEnv::with_cwd(PathBuf::from(".")));
        let env_dyn: Arc<dyn kiqi_tools::ExecutionEnv> = env.clone();
        let mut_env: Arc<dyn kiqi_tools::MutatingEnv> = env.clone();
        let context = ExecutionToolContext::new(env_dyn, Some(mut_env));
        let signal_host = Arc::new(crate::signal_host::SignalHost::new());
        let names: Vec<String> = build_tools(
            &context,
            &Args::default(),
            Path::new("."),
            &signal_host,
            &crate::skill_tool::new_skill_cell(),
        )
        .iter()
        .map(|tool| tool.tool.schema().name.clone())
        .collect();

        // `docs` and the host-owned `permissions` policy tool are default
        // capabilities; the legacy kiqi-only lookup tools stay unregistered.
        // Asserted by membership (not exact equality) because `powershell` is
        // Windows-only and `settings.json#defaultTools` may narrow the set in a
        // developer's environment.
        assert!(names.contains(&"docs".to_string()));
        assert!(names.contains(&"permissions".to_string()));
        // The signal inbox tool is a host capability, not an extension: it must be
        // present so an auto-wake can settle its disposition durably.
        assert!(names.contains(&"signals".to_string()));
        for legacy in ["grep", "find", "ls"] {
            assert!(
                !names.contains(&legacy.to_string()),
                "legacy tool {legacy} should not be registered by default"
            );
        }
    }

    #[test]
    fn reload_settings_preflight_respects_trust() {
        struct RestoreConfigDir(Option<std::ffi::OsString>);

        impl Drop for RestoreConfigDir {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                    None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
                }
            }
        }

        let _guard = crate::config::test_support::env_lock().lock();
        let _restore = RestoreConfigDir(std::env::var_os(crate::config::CONFIG_DIR_ENV));
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        let cwd = tmp.path().join("project");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(cwd.join(".ki")).unwrap();
        std::fs::write(agent.join("settings.json"), "{}").unwrap();
        std::fs::write(cwd.join(".ki/settings.json"), "{ malformed").unwrap();
        std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);
        let args = Args::default();

        let trusted = validate_settings_for_reload(&args, &cwd, true);
        let untrusted = validate_settings_for_reload(&args, &cwd, false);
        assert!(trusted
            .unwrap_err()
            .contains("could not load project settings"));
        assert!(untrusted.is_ok());

        std::fs::write(agent.join("settings.json"), "{ malformed").unwrap();
        assert!(validate_settings_for_reload(&args, &cwd, false)
            .unwrap_err()
            .contains("could not load global settings"));

        let local_only = Args {
            dev_local_only: true,
            ..Args::default()
        };
        assert!(validate_settings_for_reload(&local_only, &cwd, true).is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reload_preserves_live_resources_when_settings_break() {
        struct RestoreConfigDir(Option<std::ffi::OsString>);

        impl Drop for RestoreConfigDir {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                    None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
                }
            }
        }

        let _guard = crate::config::test_support::env_lock().lock();
        let _restore = RestoreConfigDir(std::env::var_os(crate::config::CONFIG_DIR_ENV));
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        let cwd = tmp.path().join("project");
        let skill_dir = tmp.path().join("configured-skills");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: keep-me\ndescription: Reload sentinel\n---\nKeep this skill loaded.",
        )
        .unwrap();
        std::fs::write(
            agent.join("settings.json"),
            serde_json::json!({"skillDirs": [skill_dir]}).to_string(),
        )
        .unwrap();
        std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);

        let resolved = crate::provider::resolve(
            Some("anthropic"),
            Some(crate::provider::DEFAULT_MODEL_ID),
            None,
            Some("test-key"),
            None,
        )
        .unwrap();
        let args = Args {
            trust_override: Some(false),
            no_session: true,
            no_extensions: true,
            no_prompt_templates: true,
            no_context_files: true,
            system_prompt: Some("stable system prompt".into()),
            ..Args::default()
        };
        let (harness, _events, context) = build(&resolved, &args, &cwd, false).await.unwrap();
        let before_resources = harness.get_resources().await.unwrap();
        assert_eq!(before_resources.skills.as_ref().unwrap().len(), 1);
        assert_eq!(before_resources.skills.as_ref().unwrap()[0].name, "keep-me");
        let before_prompt = harness.get_system_prompt().await.unwrap();
        let before_tools: Vec<String> = harness
            .get_tools()
            .await
            .unwrap()
            .iter()
            .map(|tool| tool.tool.schema().name.clone())
            .collect();
        let before_bridge = context
            .action_bridge
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone();

        std::fs::write(agent.join("settings.json"), "{ malformed").unwrap();
        let outcome = reload_extension_resources(&harness, &context).await;

        assert!(outcome.had_warnings);
        assert!(outcome.summary.contains("Settings reload failed"));
        assert!(outcome.summary.contains("could not load global settings"));
        assert_eq!(harness.get_resources().await.unwrap(), before_resources);
        assert_eq!(harness.get_system_prompt().await.unwrap(), before_prompt);
        let after_tools: Vec<String> = harness
            .get_tools()
            .await
            .unwrap()
            .iter()
            .map(|tool| tool.tool.schema().name.clone())
            .collect();
        assert_eq!(after_tools, before_tools);
        let after_bridge = context
            .action_bridge
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone();
        assert!(Arc::ptr_eq(&after_bridge, &before_bridge));
    }

    #[test]
    fn reload_preparation_rejects_settings_changed_during_derivation() {
        struct RestoreConfigDir(Option<std::ffi::OsString>);

        impl Drop for RestoreConfigDir {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                    None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
                }
            }
        }

        let _guard = crate::config::test_support::env_lock().lock();
        let _restore = RestoreConfigDir(std::env::var_os(crate::config::CONFIG_DIR_ENV));
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        let cwd = tmp.path().join("project");
        let first_skill_dir = tmp.path().join("first-skills");
        let second_skill_dir = tmp.path().join("second-skills");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(
            agent.join("settings.json"),
            serde_json::json!({"skillDirs": [first_skill_dir]}).to_string(),
        )
        .unwrap();
        std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);
        let args = Args {
            trust_override: Some(false),
            no_extensions: true,
            no_prompt_templates: true,
            ..Args::default()
        };

        let result = prepare_reload_inputs_inner(&args, &cwd, false, || {
            std::fs::write(
                agent.join("settings.json"),
                serde_json::json!({"skillDirs": [second_skill_dir]}).to_string(),
            )
            .unwrap();
        });

        let error = result
            .err()
            .expect("settings mutation must fail preparation");
        assert!(error.contains("settings changed while reload inputs were being prepared"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reload_uses_frozen_settings_inputs_after_preparation() {
        struct RestoreConfigDir(Option<std::ffi::OsString>);

        impl Drop for RestoreConfigDir {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                    None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
                }
            }
        }

        let _guard = crate::config::test_support::env_lock().lock();
        let _restore = RestoreConfigDir(std::env::var_os(crate::config::CONFIG_DIR_ENV));
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        let cwd = tmp.path().join("project");
        let skill_dir = tmp.path().join("configured-skills");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: frozen-skill\ndescription: Reload sentinel\n---\nFrozen input.",
        )
        .unwrap();
        std::fs::write(
            agent.join("settings.json"),
            serde_json::json!({"skillDirs": [skill_dir]}).to_string(),
        )
        .unwrap();
        std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);

        let resolved = crate::provider::resolve(
            Some("anthropic"),
            Some(crate::provider::DEFAULT_MODEL_ID),
            None,
            Some("test-key"),
            None,
        )
        .unwrap();
        let args = Args {
            trust_override: Some(false),
            no_session: true,
            no_extensions: true,
            no_prompt_templates: true,
            no_context_files: true,
            system_prompt: Some("stable system prompt".into()),
            ..Args::default()
        };
        let (harness, _events, context) = build(&resolved, &args, &cwd, false).await.unwrap();

        let outcome = reload_extension_resources_inner(&harness, &context, || {
            std::fs::write(agent.join("settings.json"), "{ malformed").unwrap();
        })
        .await;

        assert!(!outcome.summary.contains("Settings reload failed"));
        assert_eq!(outcome.summary, "Reloaded 1 skill(s), 0 prompt(s).");
        let resources = harness.get_resources().await.unwrap();
        let skills = resources.skills.as_ref().unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "frozen-skill");
    }

    #[tokio::test]
    async fn reload_resource_paths_include_explicit_skill_and_prompt_files() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_path = tmp.path().join("explicit-skill.md");
        std::fs::write(
            &skill_path,
            "---\nname: explicit-skill\ndescription: Explicit skill\n---\nSkill body",
        )
        .unwrap();
        let prompt_path = tmp.path().join("explicit-prompt.md");
        std::fs::write(
            &prompt_path,
            "---\ndescription: Explicit prompt\n---\nPrompt body",
        )
        .unwrap();

        let args = Args {
            skill: vec![skill_path.clone()],
            prompt_template: vec![prompt_path.clone()],
            ..Args::default()
        };
        let skill_paths = append_reload_resource_paths(Vec::new(), &args.skill, &[], &[], &[]);
        let prompt_paths =
            append_reload_resource_paths(Vec::new(), &args.prompt_template, &[], &[], &[]);
        let env = Arc::new(OsExecutionEnv::with_cwd(tmp.path().to_path_buf()));
        let env_dyn: Arc<dyn kiqi_tools::ExecutionEnv> = env;

        let skills = load_skills_with_precedence(&env_dyn, &skill_paths).await;
        assert_eq!(skills.skills.len(), 1, "{:?}", skills.diagnostics);
        assert_eq!(skills.skills[0].name, "explicit-skill");

        let prompts = load_prompt_templates_with_precedence(&env_dyn, &prompt_paths).await;
        assert_eq!(
            prompts.prompt_templates.len(),
            1,
            "{:?}",
            prompts.diagnostics
        );
        assert_eq!(prompts.prompt_templates[0].name, "explicit-prompt");
    }

    #[test]
    fn tool_policy_applies_to_rust_and_js_active_names() {
        let names = vec![
            "read".to_string(),
            "ask_user_question".to_string(),
            "write".to_string(),
        ];

        let args = Args {
            tools: Some(vec!["read".into(), "ask_user_question".into()]),
            ..Args::default()
        };
        assert_eq!(
            filter_active_tool_names(names.clone(), &args),
            vec!["read", "ask_user_question"]
        );

        let args = Args {
            exclude_tools: Some(vec!["ask_user_question".into()]),
            ..Args::default()
        };
        assert_eq!(
            filter_active_tool_names(names.clone(), &args),
            vec!["read", "write"]
        );

        let args = Args {
            no_tools: true,
            ..Args::default()
        };
        assert!(filter_active_tool_names(names, &args).is_empty());
    }

    #[test]
    fn select_ephemeral_when_no_session() {
        let args = Args {
            no_session: true,
            ..Args::default()
        };
        let cwd = Path::new("/tmp");
        assert!(matches!(
            select_session(&args, cwd),
            SessionSelection::Ephemeral
        ));
    }

    #[test]
    fn project_resources_load_by_default_and_allow_explicit_opt_out() {
        let defaults = Args::default();
        assert!(resolve_project_trust(
            &defaults,
            Path::new("C:/definitely-not-a-project")
        ));

        let denied = Args {
            trust_override: Some(false),
            ..Args::default()
        };
        assert!(!resolve_project_trust(
            &denied,
            Path::new("C:/definitely-not-a-project")
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn build_preserves_the_supplied_startup_project_snapshot() {
        struct RestoreConfigDir(Option<std::ffi::OsString>);

        impl Drop for RestoreConfigDir {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                    None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
                }
            }
        }

        let _guard = crate::config::test_support::env_lock().lock();
        let _restore = RestoreConfigDir(std::env::var_os(crate::config::CONFIG_DIR_ENV));
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        let cwd = tmp.path().join("project");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);

        let resolved = crate::provider::resolve(
            Some("anthropic"),
            Some(crate::provider::DEFAULT_MODEL_ID),
            None,
            Some("test-key"),
            None,
        )
        .unwrap();
        let args = Args {
            trust_override: Some(false),
            no_session: true,
            no_tools: true,
            no_extensions: true,
            no_skills: true,
            no_prompt_templates: true,
            no_context_files: true,
            system_prompt: Some("test prompt".into()),
            ..Args::default()
        };

        let (_, _, context) = build(&resolved, &args, &cwd, true).await.unwrap();

        assert_eq!(context.cwd, cwd);
        assert!(context.project_trusted);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn build_applies_the_name_flag_to_the_session() {
        struct RestoreConfigDir(Option<std::ffi::OsString>);

        impl Drop for RestoreConfigDir {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                    None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
                }
            }
        }

        let _guard = crate::config::test_support::env_lock().lock();
        let _restore = RestoreConfigDir(std::env::var_os(crate::config::CONFIG_DIR_ENV));
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        let cwd = tmp.path().join("project");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);

        let resolved = crate::provider::resolve(
            Some("anthropic"),
            Some(crate::provider::DEFAULT_MODEL_ID),
            None,
            Some("test-key"),
            None,
        )
        .unwrap();
        let args = Args {
            trust_override: Some(false),
            no_tools: true,
            no_extensions: true,
            no_skills: true,
            no_prompt_templates: true,
            no_context_files: true,
            system_prompt: Some("test prompt".into()),
            name: Some("smoke name".into()),
            session_dir: Some(cwd.join("sessions")),
            ..Args::default()
        };

        let (harness, _, _) = build(&resolved, &args, &cwd, true).await.unwrap();
        assert_eq!(
            harness.get_name().await.unwrap().as_deref(),
            Some("smoke name")
        );
    }

    #[test]
    fn select_latest_for_continue_and_resume() {
        let args = Args {
            continue_session: true,
            ..Args::default()
        };
        let cwd = Path::new("/tmp");
        assert!(matches!(
            select_session(&args, cwd),
            SessionSelection::Latest { .. }
        ));

        let args = Args {
            resume: true,
            ..Args::default()
        };
        assert!(matches!(
            select_session(&args, cwd),
            SessionSelection::Latest { .. }
        ));
    }

    #[test]
    fn select_by_id_for_session_flag() {
        let args = Args {
            session: Some("01a02ece".into()),
            ..Args::default()
        };
        let cwd = Path::new("/tmp");
        assert!(matches!(
            select_session(&args, cwd),
            SessionSelection::ById { id, .. } if id == "01a02ece"
        ));
    }

    #[test]
    fn select_new_with_custom_dir() {
        let args = Args {
            session_dir: Some(PathBuf::from("/tmp/sess")),
            ..Args::default()
        };
        let cwd = Path::new("/tmp");
        match select_session(&args, cwd) {
            SessionSelection::New { dir, .. } => assert_eq!(dir, PathBuf::from("/tmp/sess")),
            other => panic!("expected New, got {other:?}"),
        }
    }

    #[test]
    fn select_new_default_dir() {
        let args = Args::default();
        let cwd = Path::new("/proj");
        match select_session(&args, cwd) {
            SessionSelection::New { dir, .. } => {
                assert_eq!(dir, Path::new("/proj/.ki/sessions"));
            }
            other => panic!("expected New, got {other:?}"),
        }
    }

    #[test]
    fn default_session_dir_only_uses_ki() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        std::fs::create_dir_all(cwd.join(".ki/sessions")).unwrap();
        assert_eq!(default_session_dir(cwd), cwd.join(".ki/sessions"));
        std::fs::create_dir_all(cwd.join(".ki/sessions")).unwrap();
        assert_eq!(default_session_dir(cwd), cwd.join(".ki/sessions"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ephemeral_session_builds_roundtrips() {
        // Sanity: the ephemeral path produces a usable Session facade (the
        // harness build itself needs a provider; tested via the integration
        // path in tests/build.rs instead).
        let s = ephemeral_session();
        let leaf = s.get_leaf_id().await;
        assert!(leaf.is_ok());
    }

    /// Attribution must treat `--no-session` as no session at all.
    ///
    /// Every ephemeral run publishes the same literal, so accepting it would let
    /// one ephemeral session dismiss another's collab write as its own echo —
    /// the whole point of comparing ids. Pure input, so no process-global env
    /// write and no ordering dependence on the crate's other env tests.
    #[test]
    fn an_ephemeral_session_id_is_not_an_identity() {
        assert_eq!(session_id_from(Some(EPHEMERAL_SESSION_ID)), None);
        // Nothing published (an extension-only run) or a blank value: also no id.
        assert_eq!(session_id_from(None), None);
        assert_eq!(session_id_from(Some("   \n")), None);
        // A real id survives, trimmed — the value a child process receives.
        assert_eq!(
            session_id_from(Some(" 0191f2c4-6a1b-7c3d-8e4f-5a6b7c8d9e0f\n")),
            Some("0191f2c4-6a1b-7c3d-8e4f-5a6b7c8d9e0f".to_string())
        );
        // The literal is the one `ephemeral_session` writes: if these drift the
        // rule silently stops applying, so pin the connection rather than the
        // string in two places.
        assert_eq!(EPHEMERAL_SESSION_ID, "ephemeral");
    }

    // NOTE: `build_tools`/`active_tool_names` integration is exercised by the
    // `tests/build.rs` harness-build test (needs a provider + multi-thread rt).

    /// The exclusion policy, checked against the constants rather than copies.
    ///
    /// `kiqi-tools` builds its exclusion from `kiqi-extension`'s declarations, so
    /// within this repo a rename is a compile error — this test is not the link
    /// between those two crates any more. What it still guards is the part the
    /// compiler cannot see:
    ///
    /// - **The wire values.** `kiqi-langfuse` lives in a different repository and
    ///   agrees on the *strings*, not the constant paths. Rewriting a constant's
    ///   value would compile here and silently stop matching there, so the exact
    ///   values are pinned below. Changing one is a deliberate, breaking change to
    ///   a cross-repo contract, and it should fail here first.
    /// - **Coverage.** Every name the SDK declares must actually be excluded.
    /// - **Narrowness.** The user's own configuration must still reach the shell.
    #[test]
    fn host_private_env_matches_the_declared_contract() {
        // 1. The wire values. Pinned literally on purpose: see the doc comment.
        assert_eq!(kiqi_agent::SESSION_ID_ENV, "KI_SESSION_ID");
        assert_eq!(kiqi_agent::AGENT_ID_ENV, "KI_AGENT_ID");
        assert_eq!(
            kiqi_agent::SUBAGENT_PARENT_ENV,
            [
                "LANGFUSE_PI_PARENT_TRACE_ID",
                "LANGFUSE_PI_PARENT_SPAN_ID",
                "LANGFUSE_PI_PARENT_SESSION_ID",
                "LANGFUSE_PI_PARENT_DEPTH",
            ],
            "these strings are what kiqi-langfuse (a different repo) matches; a \
             change here is a breaking cross-repo change, not a refactor"
        );

        // 2. Coverage — derived from the declarations, so a newly added family
        //    member is checked without anyone remembering to extend this test.
        for name in kiqi_agent::SUBAGENT_PARENT_ENV
            .iter()
            .chain(std::iter::once(&kiqi_agent::SESSION_ID_ENV))
        {
            assert!(
                kiqi_tools::is_host_private_env(name),
                "`{name}` must not reach a spawned shell, but kiqi-tools excludes only {:?}",
                kiqi_tools::HOST_PRIVATE_ENV
            );
        }

        // 3. Narrowness — these name how the *user* configured the run and are
        //    exactly what a command inside the shell should still see.
        for visible in [
            crate::args::KI_OFFLINE_ENV,
            crate::config::CONFIG_DIR_ENV,
            kiqi_agent::AGENT_ID_ENV,
            "PATH",
        ] {
            assert!(
                !kiqi_tools::is_host_private_env(visible),
                "{visible} describes the user's configuration and must still reach the shell"
            );
        }
    }
    /// `KI_AGENT_ID` is the shell-facing twin of `KI_SESSION_ID`: same value,
    /// written at the same moments and overwritten on every activation — never
    /// "set once", or a child command would name a session that is gone.
    #[test]
    fn publishing_session_identity_writes_the_agent_channel_too() {
        struct Restore {
            session: Option<std::ffi::OsString>,
            agent: Option<std::ffi::OsString>,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                for (name, saved) in [
                    (kiqi_agent::SESSION_ID_ENV, &self.session),
                    (kiqi_agent::AGENT_ID_ENV, &self.agent),
                ] {
                    match saved {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }

        let _guard = crate::config::test_support::env_lock().lock();
        let _restore = Restore {
            session: std::env::var_os(kiqi_agent::SESSION_ID_ENV),
            agent: std::env::var_os(kiqi_agent::AGENT_ID_ENV),
        };

        let context = kiqi_host::ToolCallContext::new("/tmp");
        publish_session_identity(&context, "01a0f60b-first");
        assert_eq!(
            std::env::var(kiqi_agent::SESSION_ID_ENV).ok().as_deref(),
            Some("01a0f60b-first")
        );
        assert_eq!(
            std::env::var(kiqi_agent::AGENT_ID_ENV).ok().as_deref(),
            Some("01a0f60b-first")
        );
        assert_eq!(context.session_id().as_deref(), Some("01a0f60b-first"));

        // A second activation must overwrite, not keep the first value.
        publish_session_identity(&context, "01a0f60b-second");
        assert_eq!(
            std::env::var(kiqi_agent::AGENT_ID_ENV).ok().as_deref(),
            Some("01a0f60b-second")
        );
        assert_eq!(
            std::env::var(kiqi_agent::SESSION_ID_ENV).ok().as_deref(),
            Some("01a0f60b-second")
        );
        assert_eq!(context.session_id().as_deref(), Some("01a0f60b-second"));
    }

    #[tokio::test]
    async fn reload_shutdown_dispatches_session_shutdown_to_the_sink() {
        // Hold the crate's process-global sink gate across install -> dispatch ->
        // clear: an ungated write from a neighbouring test would otherwise land in
        // this window and steal the sink (modes.rs's SessionShutdown tests do the
        // same). The guard is deliberately held across the await; nothing reached
        // from `shutdown_extensions_for_reload` re-takes it.
        let _gate = crate::extensions::lock_test_sink();
        use std::sync::atomic::AtomicUsize;
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        kiqi_host::set_compiled_event_sink(std::sync::Arc::new(move |_mode, event| {
            if event.tag == kiqi_host::EventTag::SessionShutdown {
                counter.fetch_add(1, Ordering::SeqCst);
            }
            kiqi_host::CompiledVerdict::Continue
        }));
        super::shutdown_extensions_for_reload(None).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        kiqi_host::clear_compiled_event_sink();
    }

    /// The same shutdown dispatch through a session's own sink: it must reach
    /// that sink and NOT the process-global one, which is how a reload stops
    /// leaking SessionShutdown into a concurrent ACP session.
    #[tokio::test]
    async fn reload_shutdown_prefers_the_sessions_own_sink() {
        let _gate = crate::extensions::lock_test_sink();
        use std::sync::atomic::{AtomicUsize, Ordering};
        let global = Arc::new(AtomicUsize::new(0));
        let global_counter = Arc::clone(&global);
        kiqi_host::set_compiled_event_sink(std::sync::Arc::new(move |_mode, event| {
            if event.tag == kiqi_host::EventTag::SessionShutdown {
                global_counter.fetch_add(1, Ordering::SeqCst);
            }
            kiqi_host::CompiledVerdict::Continue
        }));

        let session_calls = Arc::new(AtomicUsize::new(0));
        let session_counter = Arc::clone(&session_calls);
        let session = ExtensionSession::none_with_compiled_sink(Some(std::sync::Arc::new(
            move |_mode, event| {
                if event.tag == kiqi_host::EventTag::SessionShutdown {
                    session_counter.fetch_add(1, Ordering::SeqCst);
                }
                kiqi_host::CompiledVerdict::Continue
            },
        )));

        super::shutdown_extensions_for_reload(session.snapshot_arc())
            .await
            .unwrap();
        assert_eq!(session_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            global.load(Ordering::SeqCst),
            0,
            "the session sink must receive SessionShutdown, not the global fallback"
        );
        kiqi_host::clear_compiled_event_sink();
    }

    /// `SessionTree` fires on `/tree` navigate, `/session` switch, `/import`,
    /// `/fork` and `/clone` so extensions (compiled + wasm) re-restore from the
    /// swapped-in branch. The dispatch must reach handlers through the same
    /// lifecycle funnel as `SessionStart`, and a handler's `ABORT` veto is
    /// **advisory** — the tree move already happened, so the notification
    /// completes instead of propagating the veto.
    #[tokio::test]
    async fn session_tree_notification_reaches_the_compiled_sink() {
        // Same gate as above: install, dispatch and clear must be one atomic
        // window relative to every other test that writes the global sink.
        let _gate = crate::extensions::lock_test_sink();
        use std::sync::atomic::AtomicUsize;
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        kiqi_host::set_compiled_event_sink(std::sync::Arc::new(move |_mode, event| {
            if event.tag == kiqi_host::EventTag::SessionTree {
                counter.fetch_add(1, Ordering::SeqCst);
            }
            kiqi_host::CompiledVerdict::Continue
        }));
        let reload_context = ReloadContext {
            extension_session: Arc::new(Mutex::new(ExtensionSession::none())),
            skill_cell: crate::skill_tool::new_skill_cell(),
            tool_context: kiqi_host::ToolCallContext::default(),
            action_bridge: Arc::new(Mutex::new(None)),
            ext_status: kiqi_host::ExtensionStatusMailbox::default(),
            editor_text: kiqi_host::EditorTextMailbox::default(),
            catalog: Vec::new(),
            providers: Vec::new(),
            runtime: tokio::runtime::Handle::current(),
            cwd: PathBuf::from("."),
            project_trusted: true,
            args: Args::default(),
            resolved_model: kiqi_ai::Model::new(
                "test-model",
                "test",
                kiqi_ai::types::Api::Faux,
                "faux",
                "",
            ),
            broadcast: Arc::new(kiqi_agent::CollectorEmitter::new().0),
            mailbox: kiqi_host::ReloadMailbox::default(),
            ui_dialog: kiqi_host::UiDialogMailbox::default(),
            signal_host: Arc::new(crate::signal_host::SignalHost::new()),
            mcp: Arc::new(kiqi_mcp::McpManager::empty()),
        };

        super::notify_session_tree(&reload_context).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "SessionTree must reach the compiled sink exactly once"
        );
        kiqi_host::clear_compiled_event_sink();
    }
}
