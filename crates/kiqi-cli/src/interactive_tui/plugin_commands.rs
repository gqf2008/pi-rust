//! Typed plugin command flows: off-thread execution and the
//! selector/editor/prompt continuation state machine.
//!
//! `SlashCommand::execute` runs on the blocking key/compose thread, so a guest
//! call there would freeze every keystroke (and a `ui.dialog` inside it would
//! deadlock against the key loop). Every plugin command and continuation
//! therefore runs on a plain `std::thread` with no tokio context, and posts its
//! outcome back through the existing `TuiMessage` channel.
//!
//! At most ONE continuation may be pending: a command result that asks for a
//! selector/editor/prompt installs a `{host, name, flow-id, kind}` slot in
//! `TuiState`; the user's answer is delivered to the guest only when it names
//! that exact slot. Esc sends `cancelled`, a second invocation replaces the
//! slot, and a stale/unknown id is rejected without reaching the guest.

use super::*;

use kiqi_host::{CommandReply, CommandResult, CommandResultBody};

/// One finished plugin call, posted back to the main loop.
pub(super) struct PluginCommandOutcome {
    pub(super) host: Arc<crate::plugins::PluginHost>,
    pub(super) name: String,
    pub(super) result: Result<CommandResult, String>,
}

/// The pure routing decision for a command result.
///
/// Split out from the UI so the "a non-Message body is a flow, not a message"
/// rule and the empty-flow-id diagnostic are unit-testable.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CommandResultRoute {
    /// Nothing to render (an empty message).
    Empty,
    /// A `Message` body, shown as a note.
    Note(String),
    /// A selector/editor/prompt body to open; the slot must be installed.
    Flow(CommandFlowKind),
    /// A body the host cannot honour (e.g. a flow with no id). Rendered as one
    /// diagnostic instead of silently doing nothing.
    Diagnostic(String),
}

pub(super) fn route_command_result(result: &CommandResult) -> CommandResultRoute {
    let flow_kind = match &result.body {
        CommandResultBody::Message(text) => {
            return if text.trim().is_empty() {
                CommandResultRoute::Empty
            } else {
                CommandResultRoute::Note(text.clone())
            };
        }
        CommandResultBody::Selector(_) => CommandFlowKind::Selector,
        CommandResultBody::Editor(_) => CommandFlowKind::Editor,
        CommandResultBody::Prompt(_) => CommandFlowKind::Prompt,
    };
    if result.flow_id.is_empty() {
        return CommandResultRoute::Diagnostic(format!(
            "a {} result carried no flow id, so its reply cannot be addressed; ignored",
            match flow_kind {
                CommandFlowKind::Selector => "selector",
                CommandFlowKind::Editor => "editor",
                CommandFlowKind::Prompt => "prompt",
            }
        ));
    }
    CommandResultRoute::Flow(flow_kind)
}

/// Start a fresh `run_command` on a plain thread.
pub(super) fn spawn_plugin_command(
    ctx: &CommandContext,
    host: Arc<crate::plugins::PluginHost>,
    name: String,
    arguments: String,
) {
    spawn_call(ctx, host, name, None, arguments);
}

/// Start one `continue_command` on a plain thread.
fn spawn_plugin_continuation(
    ctx: &CommandContext,
    host: Arc<crate::plugins::PluginHost>,
    name: String,
    flow_id: String,
    reply: CommandReply,
) {
    spawn_call(ctx, host, name, Some((flow_id, reply)), String::new());
}

fn spawn_call(
    ctx: &CommandContext,
    host: Arc<crate::plugins::PluginHost>,
    name: String,
    continuation: Option<(String, CommandReply)>,
    arguments: String,
) {
    let tx = ctx.tx.clone();
    let chat = ctx.chat.clone();
    let tui = ctx.tui.clone();
    let display = name.clone();
    let thread_host = host.clone();
    let thread_name = name.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("plugin-command {name}"))
        .spawn(move || {
            // Read the live caps inside the worker: the process-wide sink may
            // have been replaced since the command was typed.
            let caps = crate::plugins::compiled_caps();
            let result = match continuation {
                Some((flow_id, reply)) => thread_host
                    .continue_command(&thread_name, &flow_id, reply, &caps.caps())
                    .map_err(|error| error.to_string()),
                None => thread_host
                    .run_command(&thread_name, &arguments, &caps.caps())
                    .map_err(|error| error.to_string()),
            };
            let _ = tx.send(TuiMessage::PluginCommandDone(PluginCommandOutcome {
                host: thread_host,
                name: thread_name,
                result,
            }));
        });
    if let Err(error) = spawned {
        add_error_message(
            &chat,
            &format!("Could not start plugin command /{display}: {error}"),
        );
        tui.request_render(false);
    }
}

/// Route a finished command call: render a note, surface a diagnostic, or open
/// the requested flow (installing the single pending slot first).
pub(super) fn handle_command_outcome(ctx: &CommandContext, outcome: PluginCommandOutcome) {
    let display = format!("/{}", outcome.name);
    let result = match outcome.result {
        Ok(result) => result,
        Err(error) => {
            add_error_message(&ctx.chat, &format!("{display} failed: {error}"));
            ctx.tui.request_render(false);
            return;
        }
    };
    match route_command_result(&result) {
        CommandResultRoute::Empty => {}
        CommandResultRoute::Note(text) => add_note_message(&ctx.chat, &text),
        CommandResultRoute::Diagnostic(message) => {
            add_error_message(&ctx.chat, &format!("{display}: {message}"))
        }
        CommandResultRoute::Flow(kind) => {
            // "A second invocation of the same command replaces a stale slot."
            // The old flow's UI is torn down by the open call below; clearing
            // the slot first means an old callback can no longer be accepted.
            let _ = ctx.state.cancel_command_flow();
            ctx.state.set_command_flow(PendingCommandFlow {
                host: outcome.host.clone(),
                name: outcome.name.clone(),
                flow_id: result.flow_id.clone(),
                kind,
            });
            let flow = FlowContext {
                host: outcome.host,
                name: outcome.name,
                flow_id: result.flow_id,
            };
            match result.body {
                CommandResultBody::Selector(spec) => open_selector_flow(ctx, flow, spec),
                CommandResultBody::Editor(spec) => open_editor_flow(ctx, flow, spec),
                CommandResultBody::Prompt(spec) => open_prompt_flow(ctx, flow, spec),
                CommandResultBody::Message(_) => unreachable!("routed as a flow"),
            }
        }
    }
    ctx.tui.request_render(false);
}

#[derive(Clone)]
struct FlowContext {
    host: Arc<crate::plugins::PluginHost>,
    name: String,
    flow_id: String,
}

/// Deliver the user's answer, but only for the live slot. A stale/unknown id
/// never reaches the guest.
fn reply_flow(
    ctx: &CommandContext,
    flow: &FlowContext,
    kind: CommandFlowKind,
    reply: CommandReply,
) {
    let host_id = flow.host.id().to_string();
    let Some(pending) = ctx
        .state
        .take_command_flow(&host_id, &flow.name, &flow.flow_id, kind)
    else {
        add_error_message(
            &ctx.chat,
            "Ignored a reply to a plugin command flow that is no longer pending.",
        );
        ctx.tui.request_render(false);
        return;
    };
    spawn_plugin_continuation(ctx, pending.host, pending.name, pending.flow_id, reply);
}

/// Esc: cancel whatever flow is pending and tell the guest.
fn cancel_flow(ctx: &CommandContext) {
    let Some(pending) = ctx.state.cancel_command_flow() else {
        return;
    };
    spawn_plugin_continuation(
        ctx,
        pending.host,
        pending.name,
        pending.flow_id,
        CommandReply::Cancelled,
    );
}

fn open_selector_flow(ctx: &CommandContext, flow: FlowContext, spec: kiqi_host::SelectorSpec) {
    if spec.items.is_empty() {
        // The caller installed the slot before opening; an empty selector has
        // no reply path, so drop it rather than leak a pending flow.
        let _ = ctx.state.cancel_command_flow();
        add_error_message(
            &ctx.chat,
            &format!("/{} returned an empty selector; ignored.", flow.name),
        );
        ctx.tui.request_render(false);
        return;
    }
    let items: Vec<SelectItem> = spec
        .items
        .into_iter()
        .map(|item| SelectItem::new(&item, &item))
        .collect();
    let list = Arc::new(SelectList::new(items, 10));
    let title = if spec.title.is_empty() {
        "Select"
    } else {
        spec.title.as_str()
    };
    let frame = Arc::new(Container::new());
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_title(title, true));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(list.clone());
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_hint(
        "↑↓ navigate · Enter select · Esc/Ctrl+C cancel",
    ));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(DynamicBorder::new()));

    let state = ctx.state.clone();
    let ec = ctx.editor_container.clone();
    let editor = ctx.editor.clone();
    let tui = ctx.tui.clone();
    let ctx_select = ctx.clone();
    let flow_select = flow.clone();
    list.on_select(Arc::new(move |item| {
        close_selector(&state, &ec, &editor, &tui);
        reply_flow(
            &ctx_select,
            &flow_select,
            CommandFlowKind::Selector,
            CommandReply::Selected(item.value.clone()),
        );
    }));
    let state_cancel = ctx.state.clone();
    let ec_cancel = ctx.editor_container.clone();
    let editor_cancel = ctx.editor.clone();
    let tui_cancel = ctx.tui.clone();
    let ctx_cancel = ctx.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
        cancel_flow(&ctx_cancel);
    }));
    open_selector_with_view(
        &ctx.state,
        &ctx.editor_container,
        &ctx.editor,
        &ctx.tui,
        SelectorView::List(list),
        frame,
        SelectorKind::Extension,
    );
}

fn open_editor_flow(ctx: &CommandContext, flow: FlowContext, spec: kiqi_host::EditorSpec) {
    let editor = Arc::new(Editor::new(
        EditorOptions {
            padding_x: 1,
            autocomplete_max_visible: 0,
            placeholder: None,
            initial_text: Some(spec.initial_text),
        },
        EditorStyle {
            prompt: "> ".to_string(),
            placeholder: String::new(),
        },
        Arc::new(kiqi_tui::Keybindings::new()),
    ));
    editor.set_focused(true);
    let frame = Arc::new(Container::new());
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_title(&format!("/{}", flow.name), false));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(editor.clone());
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_hint(
        "Enter submit · Shift+Enter newline · Esc/Ctrl+C cancel",
    ));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(DynamicBorder::new()));

    *ctx.state.active_extension_editor.lock().unwrap() = Some(editor.clone());
    *ctx.state.active_extension_input.lock().unwrap() = None;
    ctx.editor_container.clear();
    ctx.editor_container.add_child(frame);

    let state = ctx.state.clone();
    let ec = ctx.editor_container.clone();
    let original = ctx.editor.clone();
    let tui = ctx.tui.clone();
    let ctx_submit = ctx.clone();
    let flow_submit = flow.clone();
    editor.on_submit(Arc::new(move |text| {
        close_extension_editor(&state, &ec, &original, &tui);
        reply_flow(
            &ctx_submit,
            &flow_submit,
            CommandFlowKind::Editor,
            CommandReply::Submitted(text.to_string()),
        );
    }));

    let state_cancel = ctx.state.clone();
    let ec_cancel = ctx.editor_container.clone();
    let original_cancel = ctx.editor.clone();
    let tui_cancel = ctx.tui.clone();
    let ctx_cancel = ctx.clone();
    *ctx.state.active_extension_cancel.lock().unwrap() = Some(Arc::new(move || {
        close_extension_editor(&state_cancel, &ec_cancel, &original_cancel, &tui_cancel);
        cancel_flow(&ctx_cancel);
    }));

    ctx.tui.set_focus(Some(editor));
    ctx.tui.request_render(false);
}

fn open_prompt_flow(ctx: &CommandContext, flow: FlowContext, spec: kiqi_host::PromptSpec) {
    let input = if spec.placeholder.is_empty() {
        Input::new()
    } else {
        Input::with_placeholder(&spec.placeholder)
    };
    let input = Arc::new(input);
    input.set_focused(true);
    let title = if spec.title.is_empty() {
        format!("/{}", flow.name)
    } else {
        spec.title.clone()
    };
    let frame = Arc::new(Container::new());
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_title(&title, false));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(input.clone());
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_hint("Enter submit · Esc/Ctrl+C cancel"));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(DynamicBorder::new()));

    *ctx.state.active_extension_editor.lock().unwrap() = None;
    *ctx.state.active_extension_input.lock().unwrap() = Some(input.clone());
    ctx.editor_container.clear();
    ctx.editor_container.add_child(frame);

    let state = ctx.state.clone();
    let ec = ctx.editor_container.clone();
    let original = ctx.editor.clone();
    let tui = ctx.tui.clone();
    let ctx_submit = ctx.clone();
    let flow_submit = flow.clone();
    input.on_submit(Arc::new(move |text| {
        close_extension_editor(&state, &ec, &original, &tui);
        reply_flow(
            &ctx_submit,
            &flow_submit,
            CommandFlowKind::Prompt,
            CommandReply::Submitted(text.to_string()),
        );
    }));

    let state_cancel = ctx.state.clone();
    let ec_cancel = ctx.editor_container.clone();
    let original_cancel = ctx.editor.clone();
    let tui_cancel = ctx.tui.clone();
    let ctx_cancel = ctx.clone();
    *ctx.state.active_extension_cancel.lock().unwrap() = Some(Arc::new(move || {
        close_extension_editor(&state_cancel, &ec_cancel, &original_cancel, &tui_cancel);
        cancel_flow(&ctx_cancel);
    }));

    ctx.tui.set_focus(Some(input));
    ctx.tui.request_render(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(flow_id: &str, body: CommandResultBody) -> CommandResult {
        CommandResult {
            flow_id: flow_id.to_string(),
            body,
        }
    }

    #[test]
    fn message_bodies_are_notes_or_empty() {
        assert_eq!(
            route_command_result(&result("", CommandResultBody::Message("hi".to_string()))),
            CommandResultRoute::Note("hi".to_string())
        );
        assert_eq!(
            route_command_result(&result("", CommandResultBody::Message("   ".to_string()))),
            CommandResultRoute::Empty
        );
    }

    #[test]
    fn non_message_bodies_are_flows_not_messages() {
        // A selector is a flow, never rendered as a transcript note.
        assert_eq!(
            route_command_result(&result(
                "f1",
                CommandResultBody::Selector(kiqi_host::SelectorSpec {
                    title: "t".to_string(),
                    items: vec!["a".to_string()],
                })
            )),
            CommandResultRoute::Flow(CommandFlowKind::Selector)
        );
        assert_eq!(
            route_command_result(&result(
                "f2",
                CommandResultBody::Editor(kiqi_host::EditorSpec {
                    initial_text: "x".to_string(),
                })
            )),
            CommandResultRoute::Flow(CommandFlowKind::Editor)
        );
        assert_eq!(
            route_command_result(&result(
                "f3",
                CommandResultBody::Prompt(kiqi_host::PromptSpec {
                    title: "t".to_string(),
                    placeholder: "p".to_string(),
                })
            )),
            CommandResultRoute::Flow(CommandFlowKind::Prompt)
        );
    }

    #[test]
    fn a_flow_without_an_id_is_a_diagnostic() {
        let route = route_command_result(&result(
            "",
            CommandResultBody::Selector(kiqi_host::SelectorSpec {
                title: "t".to_string(),
                items: vec!["a".to_string()],
            }),
        ));
        assert!(matches!(route, CommandResultRoute::Diagnostic(_)));
        if let CommandResultRoute::Diagnostic(message) = route {
            assert!(message.contains("flow id"), "got {message}");
        }
    }
}
