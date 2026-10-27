//! Built-in slash commands and extension command registration.

use super::*;

// ---- Built-in command implementations ----

pub(super) struct HelpCommand;
impl SlashCommand for HelpCommand {
    fn name(&self) -> &'static str {
        "/help"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/?"]
    }
    fn description(&self) -> &'static str {
        "Show available commands"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        add_help_message(&ctx.chat);
        ctx.tui.request_render(false);
    }
}

pub(super) struct ClearChatCommand;
impl SlashCommand for ClearChatCommand {
    fn name(&self) -> &'static str {
        "/clear"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/new"]
    }
    // `/new` carries its own weight as a discoverable entry, so surface it.
    fn alias_visible(&self) -> &'static [&'static str] {
        &["/new"]
    }
    fn description(&self) -> &'static str {
        "Clear the conversation"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::ClearChat);
    }
}

pub(super) struct ExitCommand;
impl SlashCommand for ExitCommand {
    fn name(&self) -> &'static str {
        "/exit"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/quit", "/q"]
    }
    // `/quit` is surfaced (matches pi's BUILTIN list); `/q` stays a hidden alias.
    fn alias_visible(&self) -> &'static [&'static str] {
        &["/quit"]
    }
    fn description(&self) -> &'static str {
        "Exit the application"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        ctx.state.cancel_js_preparation();
        let _ = ctx.tx.send(TuiMessage::Exit);
    }
}

pub(super) struct VersionCommand;
impl SlashCommand for VersionCommand {
    fn name(&self) -> &'static str {
        "/version"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/v"]
    }
    fn description(&self) -> &'static str {
        "Show version information"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        add_version_message(&ctx.chat);
        ctx.tui.request_render(false);
    }
}

pub(super) struct ChangelogCommand;
impl SlashCommand for ChangelogCommand {
    fn name(&self) -> &'static str {
        "/changelog"
    }
    fn description(&self) -> &'static str {
        "Show latest release changes (/changelog all for history)"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        match args.trim() {
            "" => add_changelog_message(&ctx.chat),
            "all" => add_changelog_message_with_scope(&ctx.chat, true),
            _ => {
                ctx.chat
                    .add_child(Arc::new(Text::new("Usage: /changelog [all]", 1, 0)));
            }
        }
        ctx.tui.request_render(false);
    }
}

pub(super) struct HotkeysCommand;
impl SlashCommand for HotkeysCommand {
    fn name(&self) -> &'static str {
        "/hotkeys"
    }
    fn description(&self) -> &'static str {
        "Show keyboard shortcuts"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        add_hotkeys_message(&ctx.chat);
        ctx.tui.request_render(false);
    }
}

pub(super) struct ModelCommand;
impl SlashCommand for ModelCommand {
    fn name(&self) -> &'static str {
        "/model"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/m"]
    }
    fn description(&self) -> &'static str {
        "Choose a model (selector)"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let term = args.trim();
        if !term.is_empty() {
            // /model <name> — direct switch by id (pi handleModelCommand).
            //
            // upstream only switches on an EXACT match; a term that matches
            // nothing falls through to the selector with the query prefilled
            // instead of erroring out (`showModelSelector(searchTerm)`).
            if let Some(model) = find_model_selector_match(&ctx.model_catalog.lock().unwrap(), term)
            {
                let model_id = model.id.clone();
                ctx.state.set_current_model(&model);
                let lane = ctx.lane.clone();
                tokio::spawn(async move {
                    let _ = lane.set_model(model).await;
                });
                add_note_message(
                    &ctx.chat,
                    &format!(
                        "Model set to {} — applies to the next message.",
                        short_model_name(&model_id)
                    ),
                );
                ctx.tui.request_render(false);
                return;
            }
        }
        let current_id = ctx.state.current_model_id();
        let catalog = ctx.model_catalog.lock().unwrap();
        open_model_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &catalog,
            &ctx.lane,
            &current_id,
            &ctx.chat,
            (!term.is_empty()).then_some(term),
        );
    }
}

pub(super) struct ThinkingCommand;
impl SlashCommand for ThinkingCommand {
    fn name(&self) -> &'static str {
        "/thinking"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/think"]
    }
    fn description(&self) -> &'static str {
        "Set thinking level (selector)"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let level_name = args.trim();
        if !level_name.is_empty() {
            // /thinking <level> — direct set (pi supports the param form).
            let Some(level) = thinking_level_from_name(level_name) else {
                add_error_message(
                    &ctx.chat,
                    &format!(
                        "Unknown thinking level \"{level_name}\". Valid: {}",
                        crate::args::VALID_THINKING_LEVELS.join(", ")
                    ),
                );
                ctx.tui.request_render(false);
                return;
            };
            let lane = ctx.lane.clone();
            let footer = ctx.state.footer.clone();
            tokio::spawn(async move {
                let _ = lane.set_thinking_level(level).await;
            });
            footer.set_thinking_level(Some(thinking_level_name(level)));
            add_note_message(&ctx.chat, &format!("Thinking set to {level_name}."));
            ctx.tui.request_render(false);
            return;
        }
        let current_id = ctx.state.current_model_id();
        open_thinking_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &ctx.lane,
            &ctx.model_catalog.lock().unwrap(),
            &current_id,
            &ctx.chat,
        );
    }
}

pub(super) struct ToolsCommand;
impl SlashCommand for ToolsCommand {
    fn name(&self) -> &'static str {
        "/tools"
    }
    fn description(&self) -> &'static str {
        "Toggle tools on/off"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        open_tools_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &ctx.lane,
            &ctx.chat,
        );
    }
}

pub(super) struct ImagesCommand;
impl SlashCommand for ImagesCommand {
    fn name(&self) -> &'static str {
        "/images"
    }
    fn description(&self) -> &'static str {
        "Toggle inline images"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        open_images_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &ctx.chat,
        );
    }
}

pub(super) struct SessionCommand;
impl SlashCommand for SessionCommand {
    fn name(&self) -> &'static str {
        "/session"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/resume"]
    }
    fn description(&self) -> &'static str {
        "List saved sessions"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        open_session_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &ctx.cwd,
            &ctx.tx,
        );
    }
}

pub(super) struct ThemeCommand;
impl SlashCommand for ThemeCommand {
    fn name(&self) -> &'static str {
        "/theme"
    }
    fn description(&self) -> &'static str {
        "Choose a theme (selector)"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let name = args.trim().to_ascii_lowercase();
        if !name.is_empty() {
            // /theme <name> — direct apply + persist (matches /settings Theme).
            let preset = match name.as_str() {
                "light" => ThemePreset::Light,
                "monochrome" => ThemePreset::Monochrome,
                "claude" => ThemePreset::Claude,
                "dark" => ThemePreset::Dark,
                _ => {
                    add_error_message(
                        &ctx.chat,
                        &format!(
                            "Unknown theme \"{name}\". Valid: dark, light, monochrome, claude."
                        ),
                    );
                    ctx.tui.request_render(false);
                    return;
                }
            };
            apply_theme_preset(preset);
            let mut settings = crate::settings::load_settings().unwrap_or_default();
            settings.theme = Some(name.clone());
            let _ = crate::settings::save_settings(&settings);
            add_note_message(&ctx.chat, &format!("Theme set to {name} (saved)."));
            ctx.tui.request_render(false);
            ctx.tui.render_now(true);
            return;
        }
        open_theme_selector(&ctx.state, &ctx.editor_container, &ctx.editor, &ctx.tui);
    }
}

pub(super) struct CompactCommand;
impl SlashCommand for CompactCommand {
    fn name(&self) -> &'static str {
        "/compact"
    }
    fn description(&self) -> &'static str {
        "Compact the conversation"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::Compact);
    }
}

pub(super) struct CopyCommand;
impl SlashCommand for CopyCommand {
    fn name(&self) -> &'static str {
        "/copy"
    }
    fn description(&self) -> &'static str {
        "Copy last reply to clipboard"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::Copy);
    }
}

pub(super) struct ExportCommand;
impl SlashCommand for ExportCommand {
    fn name(&self) -> &'static str {
        "/export"
    }
    fn description(&self) -> &'static str {
        "Export session (supports: md, html, jsonl)"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let format = args.trim().to_lowercase();
        if format.is_empty() || format == "md" || format == "markdown" {
            let _ = ctx.tx.send(TuiMessage::ExportSession);
        } else if format == "html" {
            let _ = ctx.tx.send(TuiMessage::ExportSessionWithFormat(
                crate::export::ExportFormat::Html,
            ));
        } else if format == "jsonl" {
            let _ = ctx.tx.send(TuiMessage::ExportSessionWithFormat(
                crate::export::ExportFormat::Jsonl,
            ));
        } else {
            // Invalid format, show error
            add_error_message(
                &ctx.chat,
                &format!(
                    "Unknown export format: {}. Supported: md, html, jsonl",
                    args.trim()
                ),
            );
            ctx.tui.request_render(false);
        }
    }
}

pub(super) struct ForkCommand;
impl SlashCommand for ForkCommand {
    fn name(&self) -> &'static str {
        "/fork"
    }
    fn description(&self) -> &'static str {
        "Fork the session into a new one"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::ForkSession);
    }
}

/// `/clone` is the upstream spelling for duplicating the current session.
/// Reuse the same durable fork path as `/fork`; both create a child session
/// and rebind the live harness to it.
pub(super) struct CloneCommand;
impl SlashCommand for CloneCommand {
    fn name(&self) -> &'static str {
        "/clone"
    }
    fn description(&self) -> &'static str {
        "Duplicate the current session"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::ForkSession);
    }
}

pub(super) struct TreeCommand;
impl SlashCommand for TreeCommand {
    fn name(&self) -> &'static str {
        "/tree"
    }
    fn description(&self) -> &'static str {
        "Navigate the current session tree"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::OpenTree);
    }
}

pub(super) struct LoginCommand;
impl SlashCommand for LoginCommand {
    fn name(&self) -> &'static str {
        "/login"
    }
    fn description(&self) -> &'static str {
        "Save an Anthropic API key"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let key = args.trim();
        if key.is_empty() {
            add_note_message(&ctx.chat, "Usage: /login <api-key>");
        } else {
            let result = crate::config::upsert_credential(
                "anthropic",
                crate::config::Credential::ApiKey {
                    key: Some(key.to_string()),
                    env: None,
                },
            );
            match result {
                Ok(()) => add_note_message(&ctx.chat, "Saved Anthropic credentials."),
                Err(error) => {
                    add_error_message(&ctx.chat, &format!("Could not save credentials: {error}"))
                }
            }
        }
        ctx.tui.request_render(false);
    }
}

pub(super) struct LogoutCommand;
impl SlashCommand for LogoutCommand {
    fn name(&self) -> &'static str {
        "/logout"
    }
    fn description(&self) -> &'static str {
        "Remove saved Anthropic credentials"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        match crate::config::delete_credential("anthropic") {
            Ok(true) => add_note_message(&ctx.chat, "Removed saved Anthropic credentials."),
            Ok(false) => add_note_message(&ctx.chat, "No saved Anthropic credentials found."),
            Err(error) => {
                add_error_message(&ctx.chat, &format!("Could not remove credentials: {error}"))
            }
        }
        ctx.tui.request_render(false);
    }
}

pub(super) fn set_project_trust_for_command(
    cwd: &std::path::Path,
    value: Option<bool>,
) -> Result<(), crate::config::ConfigError> {
    crate::config::set_project_trust(cwd, value)
}

pub(super) struct TrustCommand;
impl SlashCommand for TrustCommand {
    fn name(&self) -> &'static str {
        "/trust"
    }
    fn description(&self) -> &'static str {
        "Trust the current project"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let value = match args.trim().to_ascii_lowercase().as_str() {
            "" | "yes" | "y" | "true" => Some(true),
            "no" | "n" | "false" => Some(false),
            "clear" | "reset" | "none" => None,
            _ => {
                add_note_message(&ctx.chat, "Usage: /trust [yes|no|clear]");
                ctx.tui.request_render(false);
                return;
            }
        };
        match set_project_trust_for_command(&ctx.cwd, value) {
            Ok(()) => {
                let label = match value {
                    Some(true) => "trusted",
                    Some(false) => "untrusted",
                    None => "trust decision cleared",
                };
                add_note_message(&ctx.chat, &format!("Current project marked {label}."));
            }
            Err(error) => add_error_message(
                &ctx.chat,
                &format!("Could not save trust decision: {error}"),
            ),
        }
        ctx.tui.request_render(false);
    }
}

pub(super) struct NameCommand;
impl SlashCommand for NameCommand {
    fn name(&self) -> &'static str {
        "/name"
    }
    fn description(&self) -> &'static str {
        "Set session display name"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let name = args.trim();
        if name.is_empty() {
            add_note_message(
                &ctx.chat,
                "Usage: /name <display name> — sets the current session's name.",
            );
            ctx.tui.request_render(false);
            return;
        }
        let _ = ctx.tx.send(TuiMessage::SetSessionName(name.to_string()));
    }
}

pub(super) struct ImportCommand;
impl SlashCommand for ImportCommand {
    fn name(&self) -> &'static str {
        "/import"
    }
    fn description(&self) -> &'static str {
        "Import a session file (path)"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let path = args.trim();
        if path.is_empty() {
            add_note_message(
                &ctx.chat,
                "Usage: /import <path-to-session.jsonl> — copies the file into the session dir and switches to it.",
            );
            ctx.tui.request_render(false);
            return;
        }
        let _ = ctx.tx.send(TuiMessage::ImportSession(path.to_string()));
    }
}

pub(super) struct SettingsCommand;
impl SlashCommand for SettingsCommand {
    fn name(&self) -> &'static str {
        "/settings"
    }
    fn description(&self) -> &'static str {
        "Open settings menu"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let current_id = ctx.state.current_model_id();
        open_settings_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &ctx.lane,
            &ctx.model_catalog.lock().unwrap(),
            &current_id,
            &ctx.chat,
        );
    }
}

pub(super) struct ScopedModelsCommand;
impl SlashCommand for ScopedModelsCommand {
    fn name(&self) -> &'static str {
        "/scoped-models"
    }
    fn description(&self) -> &'static str {
        "Choose models for Ctrl+P cycling"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        open_scoped_models_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &ctx.model_catalog.lock().unwrap(),
            &ctx.chat,
        );
    }
}

pub(super) struct ShareCommand;
impl SlashCommand for ShareCommand {
    fn name(&self) -> &'static str {
        "/share"
    }
    fn description(&self) -> &'static str {
        "Share session (gist via gh, or clipboard)"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::ShareSession);
    }
}

/// `/context` — lists discovered context files, skills, and prompt templates.
/// Hidden from autocomplete (needs the resources snapshot to be meaningful as a
/// discovery surface; like `/name`, it's recognized-v1 but kept off the list).
pub(super) struct ContextCommand;
impl SlashCommand for ContextCommand {
    fn name(&self) -> &'static str {
        "/context"
    }
    fn visible(&self) -> bool {
        false
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let resources = ctx.resources.lock().unwrap().clone();
        show_context_panel(&ctx.chat, &resources);
        // Live session stats are async, so ask the main loop to append them.
        let _ = ctx.tx.send(TuiMessage::ShowUsage);
        ctx.tui.request_render(false);
    }
}

/// `/usage` — token/cost/cache totals read through the product-layer
/// `AgentSession`. This is the same numbers the footer shows, but expanded and
/// with the cache-hit breakdown that the one-line footer cannot fit.
pub(super) struct UsageCommand;
impl SlashCommand for UsageCommand {
    fn name(&self) -> &'static str {
        "/usage"
    }
    fn description(&self) -> &'static str {
        "Show token, cost, and cache totals for this session"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::ShowUsage);
        ctx.tui.request_render(false);
    }
}

/// `/reload` — re-run extension + resource discovery into the LIVE harness
/// (B5d): reload the cdylib extensions, invalidate the old `ActionBridge` +
/// registry snapshot, rebuild skills/prompts/context/SYSTEM.md/APPEND_SYSTEM.md
/// + the `TeeEmitter`, and push the rebuilt state via the B5d harness setters.
/// The command itself runs on the blocking submit thread, so it can't drive
/// the async `reload_extension_resources` routine directly — it signals the main
/// loop via `TuiMessage::ReloadExtensions`, which awaits it on the async runtime.
/// (An extension's `runtime_action(Reload)` signals the same loop via the
/// `ReloadMailbox` the TUI installs — the B5d async-reload design avoids the
/// self-unmapping race a synchronous extension-initiated reload would have.)
pub(super) struct ReloadCommand;
impl SlashCommand for ReloadCommand {
    fn name(&self) -> &'static str {
        "/reload"
    }
    fn description(&self) -> &'static str {
        "Reload extensions, skills, prompts"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        // Signal the main loop. It owns the `&AgentHarness` borrow the
        // `reload_extension_resources` routine needs (the blocking submit thread
        // only has the context's `Arc<ReloadContext>` + the `Arc<dyn AgentLane>`).
        add_note_message(&ctx.chat, "Reloading extensions + resources…");
        ctx.tui.request_render(false);
        let _ = ctx.tx.send(TuiMessage::ReloadExtensions);
    }
}

/// `/signals` — the user's view of the MCP signal host: session mute and the
/// durable attention inbox. The agent-facing counterpart is the `signals` tool.
pub(super) struct SignalsCommand;
impl SlashCommand for SignalsCommand {
    fn name(&self) -> &'static str {
        "/signals"
    }
    fn description(&self) -> &'static str {
        "Show MCP signals; /signals on|off controls automatic wakes"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        match args.trim().to_ascii_lowercase().as_str() {
            "" | "status" | "list" => {
                add_note_message(
                    &ctx.chat,
                    &format!("MCP signals: muted={}", ctx.signal_wake.session_muted()),
                );
                let host = ctx.signal_wake.clone();
                let chat = ctx.chat.clone();
                let tui = ctx.tui.clone();
                tokio::spawn(async move {
                    match host.inbox_entries().await {
                        Ok(entries) => add_note_message(&chat, &format_inbox(&entries)),
                        Err(error) => add_error_message(
                            &chat,
                            &format!("Could not read the MCP signal inbox: {error}"),
                        ),
                    }
                    tui.request_render(false);
                });
            }
            "off" | "mute" => {
                ctx.signal_wake.set_session_muted(true);
                add_note_message(&ctx.chat, "MCP signal wake muted for this session.");
                persist_mute(ctx, true);
            }
            "on" | "unmute" => {
                ctx.signal_wake.set_session_muted(false);
                add_note_message(&ctx.chat, "MCP signal wake enabled for this session.");
                persist_mute(ctx, false);
            }
            other => add_note_message(
                &ctx.chat,
                &format!("Usage: /signals [on|off|list] (got {other:?})"),
            ),
        }
        ctx.tui.request_render(false);
    }
}

fn persist_mute(ctx: &CommandContext, muted: bool) {
    let host = ctx.signal_wake.clone();
    let chat = ctx.chat.clone();
    let tui = ctx.tui.clone();
    tokio::spawn(async move {
        if let Err(error) = host.persist_session_muted(muted).await {
            // Keep the in-memory flag honest if the durable write failed.
            host.set_session_muted(!muted);
            add_error_message(&chat, &error);
        }
        tui.request_render(false);
    });
}

fn format_inbox(entries: &[crate::signal_host::InboxEntry]) -> String {
    if entries.is_empty() {
        return "MCP signal inbox is empty.".to_string();
    }
    let mut text = format!("MCP signal inbox: {} open\n", entries.len());
    for entry in entries {
        let status = match entry.status {
            crate::signal_host::InboxStatus::Pending => "pending",
            crate::signal_host::InboxStatus::Deferred => "deferred",
        };
        text.push_str(&format!(
            "- id {} [{}] {}: {}",
            entry.id, status, entry.server, entry.uri
        ));
        if let Some(note) = &entry.note {
            text.push_str(&format!(" — {note}"));
        }
        text.push('\n');
    }
    text
}

/// Build the full command registry: active built-ins first (so they win on a
/// fuzzy autocomplete tie), then the v1-out-of-scope stubs. Prompt-template
/// commands are merged in separately by the autocomplete builder (they dispatch
/// via template expansion, not this registry).
pub(super) fn build_builtin_registry() -> CommandRegistry {
    let mut r = CommandRegistry::new();
    r.register(Arc::new(HelpCommand));
    r.register(Arc::new(ClearChatCommand));
    r.register(Arc::new(ExitCommand));
    r.register(Arc::new(VersionCommand));
    r.register(Arc::new(ChangelogCommand));
    r.register(Arc::new(ModelCommand));
    r.register(Arc::new(ThinkingCommand));
    r.register(Arc::new(ToolsCommand));
    r.register(Arc::new(ImagesCommand));
    r.register(Arc::new(SessionCommand));
    r.register(Arc::new(ThemeCommand));
    r.register(Arc::new(CompactCommand));
    r.register(Arc::new(CopyCommand));
    r.register(Arc::new(HotkeysCommand));
    r.register(Arc::new(ContextCommand));
    r.register(Arc::new(UsageCommand));
    r.register(Arc::new(SignalsCommand));
    // Recognized but inert in v1 (one struct backs them all). The TS builtins
    // out of v1 scope; each carries a description so autocomplete surfaces its
    // existence even though running it reports "not supported".
    r.register(Arc::new(NameCommand));
    r.register(Arc::new(SettingsCommand));
    r.register(Arc::new(ScopedModelsCommand));
    r.register(Arc::new(ExportCommand));
    r.register(Arc::new(ImportCommand));
    r.register(Arc::new(ShareCommand));
    r.register(Arc::new(ForkCommand));
    r.register(Arc::new(CloneCommand));
    r.register(Arc::new(TreeCommand));
    r.register(Arc::new(TrustCommand));
    r.register(Arc::new(LoginCommand));
    r.register(Arc::new(LogoutCommand));
    r.register(Arc::new(ReloadCommand));
    r
}

pub(super) fn register_extension_commands(
    registry: &mut CommandRegistry,
    _session: crate::session::ExtensionSessionCell,
) {
    // Native (cdylib) commands were removed with the registration machine
    // (docs/design/extension-abi.md §6.49). Wasm and compiled-in commands are
    // the only contributors now.
    register_compiled_commands(registry);
}

/// Register the `/commands` compiled-in and wasm extensions contribute.
///
/// A compiled extension's `run_command` is one synchronous call that returns the
/// text to show, so there is no selector/editor round-trip to resume — unlike
/// [`ExtensionCommand`], which keeps the native session alive for a dialog.
/// Built-ins already registered win, so an extension cannot shadow `/help`.
pub(super) fn register_compiled_commands(registry: &mut CommandRegistry) {
    for host in crate::extensions::compiled_extension_hosts() {
        for spec in host.extension().commands() {
            let name = if spec.name.starts_with('/') {
                spec.name.clone()
            } else {
                format!("/{}", spec.name)
            };
            if registry.find(&name).is_some() {
                continue;
            }
            registry.register(Arc::new(CompiledCommand {
                name,
                description: spec.description,
                host: host.clone(),
            }));
        }
    }
}

/// A slash command contributed by a compiled-in or wasm extension.
struct CompiledCommand {
    name: String,
    description: String,
    host: std::sync::Arc<crate::extensions::ExtensionHost>,
}

impl SlashCommand for CompiledCommand {
    fn name(&self) -> &str {
        &self.name
    }

    fn description_owned(&self) -> String {
        self.description.clone()
    }

    fn execute(&self, ctx: &CommandContext, args: &str) {
        // The key/compose thread must never block on a guest: run the command
        // on a plain thread and post the typed result back to the main loop.
        // Any previously pending flow is replaced (its reply will then fail the
        // stale-id check).
        let _ = ctx.state.cancel_command_flow();
        spawn_extension_command(
            ctx,
            self.host.clone(),
            self.name.trim_start_matches('/').to_string(),
            args.to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A compiled-in extension's `/command` lands in the same registry the TUI
    /// dispatches from — kiqi-todo contributes `/todos`.
    /// Needs a compiled extension that contributes a command; the default build
    /// has none (they are opt-in features now, or wasm guests).
    #[cfg(feature = "extension-kiqi-todo")]
    #[test]
    fn compiled_extension_commands_are_registered() {
        let mut registry = CommandRegistry::new();
        register_compiled_commands(&mut registry);
        let command = registry.find("/todos").expect("/todos from kiqi-todo");
        assert_eq!(command.name(), "/todos");
        assert!(
            !command.description_owned().is_empty(),
            "autocomplete needs a description"
        );
    }

    /// An extension cannot shadow a built-in: built-ins register first and the
    /// duplicate check keeps them, so `/help` stays the real one.
    #[test]
    fn compiled_commands_do_not_shadow_builtins() {
        let mut registry = build_builtin_registry();
        let builtin = registry.find("/help").unwrap().name().to_string();
        register_compiled_commands(&mut registry);
        assert_eq!(registry.find("/help").unwrap().name(), builtin);
    }
}
