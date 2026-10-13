//! Interactive model add/edit/remove for `models.json` providers
//! (`/provider` → model actions).
//!
//! Built-in provider catalogs are compiled in and therefore read-only; only
//! providers declared in `~/.rpi/agent/models.json` can be edited here, and
//! every change rewrites that file. The flows reuse the "Add provider" form
//! machinery: [`ProviderForm::Model`] occupies the same single form slot, and
//! each step renders either a single-line input
//! ([`TuiState::active_provider_input`]) or a select list
//! ([`TuiState::active_provider_select`]).

use super::*;

/// Providers defined in `models.json` — the only ones whose model lists can be
/// edited. Sorted by display label so both the picker and the summary are
/// stable.
pub(super) fn editable_provider_items() -> Vec<SelectItem> {
    let cfg = crate::config::load_models_config().unwrap_or_default();
    let mut items: Vec<SelectItem> = cfg
        .providers
        .iter()
        .map(|(id, provider)| {
            let label = provider_display_name(id);
            let protocol = protocol_label(provider.api.as_deref());
            let endpoint = provider
                .base_url
                .as_deref()
                .filter(|url| !url.is_empty())
                .unwrap_or("(no base URL)");
            SelectItem::new(id, &label)
                .with_description(&format!(
                    "{} model(s) · {protocol} · {endpoint}",
                    provider.models.len()
                ))
                .with_search_text(&format!("{label} {id} {protocol} {endpoint}"))
        })
        .collect();
    items.sort_by(|a, b| {
        a.label
            .to_ascii_lowercase()
            .cmp(&b.label.to_ascii_lowercase())
    });
    items
}

/// The protocol an editable provider speaks, for titles/summaries. `None`
/// (missing `api`) means the default Anthropic-compatible runtime, matching
/// [`crate::config::provider_is_anthropic_compatible`].
pub(super) fn protocol_label(api: Option<&str>) -> &'static str {
    match api {
        Some("openai-completions") => "OpenAI Completions",
        Some("openai-responses") => "OpenAI Responses",
        Some("anthropic-messages") | None | Some("") => "Anthropic Messages",
        Some(_) => "自定义协议",
    }
}

/// One row per model under `provider_id`, describing what the model declares.
fn provider_model_items(provider_id: &str) -> Vec<SelectItem> {
    let cfg = crate::config::load_models_config().unwrap_or_default();
    let Some(provider) = cfg.providers.get(provider_id) else {
        return Vec::new();
    };
    provider
        .models
        .iter()
        .map(|m| {
            let mut caps: Vec<&str> = Vec::new();
            if m.reasoning.unwrap_or(false) {
                caps.push("reasoning");
            }
            if m.input
                .as_ref()
                .is_some_and(|l| l.iter().any(|s| s.eq_ignore_ascii_case("image")))
            {
                caps.push("image");
            }
            let mut detail = vec![
                m.context_window
                    .map(|v| format!("ctx {v}"))
                    .unwrap_or_else(|| "ctx unset".to_string()),
                m.max_tokens
                    .map(|v| format!("out {v}"))
                    .unwrap_or_else(|| "out unset".to_string()),
            ];
            if !caps.is_empty() {
                detail.push(caps.join("+"));
            }
            SelectItem::new(&m.id, m.name.as_deref().unwrap_or(&m.id))
                .with_description(&detail.join(" · "))
                .with_search_text(&format!("{} {}", m.id, m.name.as_deref().unwrap_or("")))
        })
        .collect()
}

/// Whether `model_id` is already declared under `provider_id`.
fn model_id_exists(provider_id: &str, model_id: &str) -> bool {
    crate::config::load_models_config()
        .ok()
        .and_then(|cfg| {
            cfg.providers
                .get(provider_id)
                .map(|p| p.models.iter().any(|m| m.id.eq_ignore_ascii_case(model_id)))
        })
        .unwrap_or(false)
}

/// Clone one model definition out of `models.json`.
fn load_model_def(provider_id: &str, model_id: &str) -> Option<crate::config::ModelDefinition> {
    let cfg = crate::config::load_models_config().ok()?;
    let provider = cfg.providers.get(provider_id)?;
    provider.models.iter().find(|m| m.id == model_id).cloned()
}

/// Parse an optional positive integer field. Empty means "leave unset".
pub(super) fn parse_optional_count(text: &str, field: &str) -> Result<Option<u64>, String> {
    if text.is_empty() {
        return Ok(None);
    }
    match text.parse::<u64>() {
        Ok(value) if value > 0 => Ok(Some(value)),
        _ => Err(format!(
            "{field} must be a positive whole number (got \"{text}\")."
        )),
    }
}

/// "Add model · OpenRouter" / "Edit model · Kimi".
fn form_title(form: &ModelFormState) -> String {
    let action = match form.mode {
        ModelFormMode::Add => "Add model",
        ModelFormMode::Edit => "Edit model",
        ModelFormMode::Remove => "Remove model",
    };
    if form.provider_label.is_empty() {
        action.to_string()
    } else {
        format!("{action} · {}", form.provider_label)
    }
}

/// Display an optional numeric field in the confirm summary.
fn count_or_unset(raw: &str) -> String {
    if raw.is_empty() {
        "(unset)".to_string()
    } else {
        raw.to_string()
    }
}

// ===========================================================================
// Entry points (`/provider` → model actions)
// ===========================================================================

/// `/provider` → "＋ Add model to a provider…".
pub(super) fn begin_add_model_form(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    start_model_form(state, editor_container, editor, tui, ModelFormMode::Add);
}

/// `/provider` → "✎ Edit model…".
pub(super) fn begin_edit_model_form(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    start_model_form(state, editor_container, editor, tui, ModelFormMode::Edit);
}

fn start_model_form(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    mode: ModelFormMode,
) {
    if editable_provider_items().is_empty() {
        add_note_message(
            &state.chat_container,
            "No editable providers. Model editing applies only to providers declared in ~/.rpi/agent/models.json — add a provider first.",
        );
        tui.request_render(false);
        return;
    }
    let form = ModelFormState {
        mode,
        step: ModelFormStep::Provider,
        ..Default::default()
    };
    *state.provider_form.lock().unwrap() = Some(ProviderForm::Model(form));
    // Route through the shared dispatcher: it clears the input/select slots
    // before rendering, so a stale widget can never keep receiving keys.
    show_provider_form_step(state, editor_container, editor, tui);
}

/// `/provider` → "− Remove model…": pick a provider, then a model, then delete.
pub(super) fn begin_remove_model_form(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    start_model_form(state, editor_container, editor, tui, ModelFormMode::Remove);
}

// ===========================================================================
// Step rendering
// ===========================================================================

/// Render the current model-form step. Called by the shared
/// `show_provider_form_step` dispatcher after every transition.
pub(super) fn show_model_form_step(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    form: &ModelFormState,
) {
    match form.step {
        ModelFormStep::Provider => show_model_provider_select(state, editor_container, editor, tui),
        ModelFormStep::PickModel => {
            show_model_pick_select(state, editor_container, editor, tui, form)
        }
        ModelFormStep::Id
        | ModelFormStep::Name
        | ModelFormStep::ContextWindow
        | ModelFormStep::MaxTokens => {
            show_model_text_input(state, editor_container, editor, tui, form)
        }
        ModelFormStep::Reasoning => {
            show_model_bool_select(state, editor_container, editor, tui, form, true)
        }
        ModelFormStep::Images => {
            show_model_bool_select(state, editor_container, editor, tui, form, false)
        }
        ModelFormStep::Confirm => show_model_confirm(state, editor_container, editor, tui, form),
    }
}

/// Build the bordered frame + select list shared by every choice-style step,
/// wiring `on_select` and routing Esc to the form cancel path.
pub(super) fn show_form_select(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    title: &str,
    question: &str,
    hint: &str,
    items: Vec<SelectItem>,
    selected_index: usize,
    on_select: Arc<dyn Fn(&SelectItem) + Send + Sync>,
) {
    let list = Arc::new(SelectList::new(items, 10));
    list.set_selected_index(selected_index);
    list.on_select(on_select);

    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        cancel_provider_form(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    let frame = provider_form_frame(title, question, list.clone(), hint);
    editor_container.clear();
    editor_container.add_child(frame);
    *state.active_provider_select.lock().unwrap() = Some(list.clone());
    tui.set_focus(Some(list.clone()));
    tui.request_render(false);
}

fn show_model_provider_select(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    let Some(ProviderForm::Model(form)) = state.provider_form.lock().unwrap().clone() else {
        return;
    };
    let items = editable_provider_items();
    let title = format!("{} · provider", form_title(&form));
    let question = if form.mode == ModelFormMode::Remove {
        "Which provider's model list do you want to change?".to_string()
    } else {
        "Which models.json provider?".to_string()
    };

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    show_form_select(
        state,
        editor_container,
        editor,
        tui,
        &title,
        &question,
        "Enter selects · Esc cancels",
        items,
        0,
        Arc::new(move |item| {
            let Some(ProviderForm::Model(mut form)) =
                state_sel.provider_form.lock().unwrap().take()
            else {
                return;
            };
            form.provider_id = item.value.clone();
            form.provider_label = item.label.clone();
            form.step = match form.mode {
                ModelFormMode::Add => ModelFormStep::Id,
                ModelFormMode::Edit | ModelFormMode::Remove => ModelFormStep::PickModel,
            };
            *state_sel.provider_form.lock().unwrap() = Some(ProviderForm::Model(form));
            show_provider_form_step(&state_sel, &ec_sel, &editor_sel, &tui_sel);
        }),
    );
}

fn show_model_pick_select(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    form: &ModelFormState,
) {
    let items = provider_model_items(&form.provider_id);
    if items.is_empty() {
        add_note_message(
            &state.chat_container,
            &format!(
                "Provider \"{}\" has no models yet — use \"＋ Add model to a provider…\".",
                form.provider_label
            ),
        );
        finish_provider_form(state, editor_container, editor, tui);
        return;
    }
    let title = format!("{} · model", form_title(form));
    let (question, hint) = if form.mode == ModelFormMode::Remove {
        (
            "Which model should be removed?".to_string(),
            "Enter deletes · Esc cancels",
        )
    } else {
        ("Which model?".to_string(), "Enter selects · Esc cancels")
    };

    let provider_id = form.provider_id.clone();
    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    show_form_select(
        state,
        editor_container,
        editor,
        tui,
        &title,
        &question,
        hint,
        items,
        0,
        Arc::new(move |item| {
            let Some(ProviderForm::Model(mut form)) =
                state_sel.provider_form.lock().unwrap().take()
            else {
                return;
            };
            if form.mode == ModelFormMode::Remove {
                match remove_model_from_provider(&provider_id, &item.value) {
                    Ok(note) => add_note_message(&state_sel.chat_container, &note),
                    Err(error) => add_error_message(&state_sel.chat_container, &error),
                }
                finish_provider_form(&state_sel, &ec_sel, &editor_sel, &tui_sel);
                return;
            }
            if let Some(def) = load_model_def(&provider_id, &item.value) {
                form.original_id = Some(def.id.clone());
                form.id = def.id.clone();
                // Echo the effective display name (`None` renders as the id in
                // /model), otherwise the field looks unloaded on edit.
                form.name = def.name.clone().unwrap_or_else(|| def.id.clone());
                form.context_window = def
                    .context_window
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                form.max_tokens = def.max_tokens.map(|v| v.to_string()).unwrap_or_default();
                form.reasoning = def.reasoning.unwrap_or(false);
                form.images = def
                    .input
                    .as_ref()
                    .is_some_and(|list| list.iter().any(|s| s.eq_ignore_ascii_case("image")));
            }
            form.step = ModelFormStep::Id;
            *state_sel.provider_form.lock().unwrap() = Some(ProviderForm::Model(form));
            show_provider_form_step(&state_sel, &ec_sel, &editor_sel, &tui_sel);
        }),
    );
}

/// A text step: titled single-line input, prefilled on edit.
fn show_model_text_input(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    form: &ModelFormState,
) {
    let (step_label, question, placeholder, initial) = match form.step {
        ModelFormStep::Id => (
            "model id",
            "Model id — the exact name the endpoint expects, e.g. gpt-5-mini.",
            "gpt-5-mini",
            form.id.clone(),
        ),
        ModelFormStep::Name => (
            "display name",
            "Display name — optional; shown in /model. Empty keeps the id.",
            "GPT-5 mini",
            form.name.clone(),
        ),
        ModelFormStep::ContextWindow => (
            "context window",
            "Context window in tokens — optional; empty leaves it unset.",
            "128000",
            form.context_window.clone(),
        ),
        _ => (
            "max output",
            "Max output tokens — optional; empty leaves it unset.",
            "8192",
            form.max_tokens.clone(),
        ),
    };
    let title = format!("{} · {step_label}", form_title(form));

    let input = Arc::new(Input::with_placeholder(placeholder));
    if !initial.is_empty() {
        input.set_value(&initial);
    }
    input.set_focused(true);

    let frame = provider_form_frame(&title, question, input.clone(), "Enter next · Esc cancels");
    editor_container.clear();
    editor_container.add_child(frame);
    *state.active_provider_input.lock().unwrap() = Some(input.clone());
    tui.set_focus(Some(input.clone()));

    let state_submit = state.clone();
    let ec_submit = editor_container.clone();
    let editor_submit = editor.clone();
    let tui_submit = tui.clone();
    input.on_submit(Arc::new(move |text: &str| {
        handle_model_form_input(&state_submit, &ec_submit, &editor_submit, &tui_submit, text);
    }));

    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    input.on_escape(Arc::new(move || {
        cancel_provider_form(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));
    tui.request_render(false);
}

/// A Yes/No step for a capability flag (`reasoning` or image input). Two
/// single-select rows keep "keep the current value" unambiguous, unlike a
/// multi-select preset where a bare Enter means "only the highlighted row".
fn show_model_bool_select(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    form: &ModelFormState,
    is_reasoning: bool,
) {
    let current = if is_reasoning {
        form.reasoning
    } else {
        form.images
    };
    let (step_label, question) = if is_reasoning {
        (
            "reasoning",
            "Reasoning — does this model accept a thinking/reasoning budget?",
        )
    } else {
        (
            "image input",
            "Image input — does this model accept image attachments?",
        )
    };
    let items = vec![
        SelectItem::new("yes", "Yes").with_description("Declares the capability in models.json"),
        SelectItem::new("no", "No").with_description("Plain text model"),
    ];
    let title = format!("{} · {step_label}", form_title(form));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    show_form_select(
        state,
        editor_container,
        editor,
        tui,
        &title,
        question,
        "↑/↓ choose · Enter next · Esc cancels",
        items,
        if current { 0 } else { 1 },
        Arc::new(move |item| {
            let Some(ProviderForm::Model(mut form)) =
                state_sel.provider_form.lock().unwrap().take()
            else {
                return;
            };
            let yes = item.value == "yes";
            if is_reasoning {
                form.reasoning = yes;
                form.step = ModelFormStep::Images;
            } else {
                form.images = yes;
                form.step = ModelFormStep::Confirm;
            }
            *state_sel.provider_form.lock().unwrap() = Some(ProviderForm::Model(form));
            show_provider_form_step(&state_sel, &ec_sel, &editor_sel, &tui_sel);
        }),
    );
}

fn model_summary_lines(form: &ModelFormState) -> Vec<String> {
    let provider = if form.provider_label.is_empty() {
        form.provider_id.clone()
    } else {
        format!("{} ({})", form.provider_label, form.provider_id)
    };
    let api = crate::config::load_models_config()
        .ok()
        .and_then(|cfg| cfg.providers.get(&form.provider_id).map(|p| p.api.clone()))
        .unwrap_or(None);
    vec![
        format!("provider     : {provider}"),
        format!("protocol     : {}", protocol_label(api.as_deref())),
        format!("model id     : {}", form.id),
        format!(
            "display name : {}",
            if form.name.is_empty() {
                "(keeps id)".to_string()
            } else {
                form.name.clone()
            }
        ),
        format!("context      : {}", count_or_unset(&form.context_window)),
        format!("max output   : {}", count_or_unset(&form.max_tokens)),
        format!(
            "reasoning    : {}",
            if form.reasoning { "yes" } else { "no" }
        ),
        format!("image input  : {}", if form.images { "yes" } else { "no" }),
        "file         : ~/.rpi/agent/models.json".to_string(),
    ]
}

/// The final step: re-show the assembled definition (the "echo") and only
/// write after an explicit Save, so the recorded protocol/fields are always
/// exactly what the user just saw.
fn show_model_confirm(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    form: &ModelFormState,
) {
    let items = vec![
        SelectItem::new("save", "✓ Save to models.json")
            .with_description("写入 ~/.rpi/agent/models.json"),
        SelectItem::new("cancel", "✗ Cancel").with_description("不写入，丢弃本次修改"),
    ];
    let list = Arc::new(SelectList::new(items, 5));

    let body = Arc::new(Container::new());
    for line in model_summary_lines(form) {
        body.add_child(Arc::new(Text::new(line, 1, 0)));
    }
    body.add_child(Arc::new(Spacer::new(1)));
    body.add_child(list.clone());

    let title = form_title(form);
    let frame = provider_form_frame(
        &title,
        "Review — this is exactly what will be written:",
        body,
        "↑/↓ choose · Enter confirms · Esc cancels",
    );
    editor_container.clear();
    editor_container.add_child(frame);
    *state.active_provider_select.lock().unwrap() = Some(list.clone());
    tui.set_focus(Some(list.clone()));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    list.on_select(Arc::new(move |item| {
        if item.value == "save" {
            let Some(ProviderForm::Model(form)) = state_sel.provider_form.lock().unwrap().take()
            else {
                return;
            };
            match save_model_from_form(&form) {
                Ok(note) => add_note_message(&state_sel.chat_container, &note),
                Err(error) => add_error_message(&state_sel.chat_container, &error),
            }
            finish_provider_form(&state_sel, &ec_sel, &editor_sel, &tui_sel);
        } else {
            cancel_provider_form(&state_sel, &ec_sel, &editor_sel, &tui_sel);
        }
    }));

    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        cancel_provider_form(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));
    tui.request_render(false);
}

// ===========================================================================
// Input handling
// ===========================================================================

/// Text-step handler: validate, advance, and re-render. Empty Enter is only
/// "cancel" for the id step (which cannot be empty); every other field accepts
/// empty as "leave unset".
pub(super) fn handle_model_form_input(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    text: &str,
) {
    let Some(ProviderForm::Model(mut form)) = state.provider_form.lock().unwrap().take() else {
        return;
    };
    let text = text.trim();

    let mut error: Option<String> = None;
    match form.step {
        ModelFormStep::Id => {
            if text.is_empty() {
                error = Some("Model id cannot be empty.".to_string());
            } else if form.mode == ModelFormMode::Add && model_id_exists(&form.provider_id, text) {
                error = Some(format!(
                    "Model \"{text}\" already exists in provider \"{}\". Use \"✎ Edit model…\" instead.",
                    form.provider_label
                ));
            } else {
                form.id = text.to_string();
                form.step = ModelFormStep::Name;
            }
        }
        ModelFormStep::Name => {
            form.name = text.to_string();
            form.step = ModelFormStep::ContextWindow;
        }
        ModelFormStep::ContextWindow => match parse_optional_count(text, "Context window") {
            Ok(_) => {
                form.context_window = text.to_string();
                form.step = ModelFormStep::MaxTokens;
            }
            Err(message) => error = Some(message),
        },
        ModelFormStep::MaxTokens => match parse_optional_count(text, "Max output") {
            Ok(_) => {
                form.max_tokens = text.to_string();
                form.step = ModelFormStep::Reasoning;
            }
            Err(message) => error = Some(message),
        },
        _ => {}
    }

    if let Some(message) = error {
        add_error_message(&state.chat_container, &message);
    }
    *state.provider_form.lock().unwrap() = Some(ProviderForm::Model(form));
    show_provider_form_step(state, editor_container, editor, tui);
}

// ===========================================================================
// Persistence
// ===========================================================================

/// Write the completed model form into `models.json`. Add appends a fresh
/// definition; Edit mutates the existing entry in place so unmanaged fields
/// (`baseUrl`/`headers`/`compat`) are preserved.
pub(super) fn save_model_from_form(form: &ModelFormState) -> Result<String, String> {
    let mut cfg = crate::config::load_models_config().map_err(|error| error.to_string())?;
    let Some(provider) = cfg.providers.get_mut(&form.provider_id) else {
        return Err(format!(
            "Provider \"{}\" is no longer in models.json.",
            form.provider_id
        ));
    };
    let name = (!form.name.is_empty() && form.name != form.id).then(|| form.name.clone());
    let context_window = parse_optional_count(&form.context_window, "Context window")?;
    let max_tokens = parse_optional_count(&form.max_tokens, "Max output")?;
    let input = Some(if form.images {
        vec!["text".to_string(), "image".to_string()]
    } else {
        vec!["text".to_string()]
    });

    match form.mode {
        ModelFormMode::Remove => {
            return Err("Internal error: the remove flow deletes directly.".to_string())
        }
        ModelFormMode::Add => {
            if provider
                .models
                .iter()
                .any(|m| m.id.eq_ignore_ascii_case(&form.id))
            {
                return Err(format!(
                    "Model \"{}\" already exists in provider \"{}\".",
                    form.id, form.provider_id
                ));
            }
            provider.models.push(crate::config::ModelDefinition {
                id: form.id.clone(),
                name,
                base_url: None,
                reasoning: Some(form.reasoning),
                context_window,
                max_tokens,
                input,
                headers: None,
                compat: None,
            });
        }
        ModelFormMode::Edit => {
            let original = form.original_id.clone().unwrap_or_else(|| form.id.clone());
            let Some(slot) = provider.models.iter_mut().find(|m| m.id == original) else {
                return Err(format!(
                    "Model \"{original}\" is no longer in provider \"{}\".",
                    form.provider_id
                ));
            };
            // Only the fields this form owns are overwritten; `baseUrl`,
            // `headers`, and `compat` survive an edit.
            slot.id = form.id.clone();
            slot.name = name;
            slot.reasoning = Some(form.reasoning);
            slot.context_window = context_window;
            slot.max_tokens = max_tokens;
            slot.input = input;
        }
    }

    let action = match form.mode {
        ModelFormMode::Add => "Added",
        ModelFormMode::Edit => "Updated",
        ModelFormMode::Remove => "Removed",
    };
    let api = provider.api.clone();
    crate::config::save_models_config(&cfg).map_err(|error| error.to_string())?;
    Ok(format!(
        "{action} model \"{}\" in provider \"{}\" ({}) — applying now.",
        form.id,
        form.provider_id,
        protocol_label(api.as_deref())
    ))
}

/// Delete one model from a `models.json` provider.
pub(super) fn remove_model_from_provider(
    provider_id: &str,
    model_id: &str,
) -> Result<String, String> {
    let mut cfg = crate::config::load_models_config().map_err(|error| error.to_string())?;
    let Some(provider) = cfg.providers.get_mut(provider_id) else {
        return Err(format!("Provider \"{provider_id}\" is not in models.json."));
    };
    let before = provider.models.len();
    provider.models.retain(|m| m.id != model_id);
    if provider.models.len() == before {
        return Err(format!(
            "Model \"{model_id}\" is not in provider \"{provider_id}\"."
        ));
    }
    crate::config::save_models_config(&cfg).map_err(|error| error.to_string())?;
    Ok(format!(
        "Removed model \"{model_id}\" from provider \"{provider_id}\" — applying now."
    ))
}
