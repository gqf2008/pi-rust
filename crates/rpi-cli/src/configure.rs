//! `ki configure` — interactive provider/model configuration for
//! `~/.ki/agent/models.json`.
//!
//! The CLI counterpart of provider management: a plain stdin/stdout wizard that
//! works over SSH, in a piped shell, and on Windows terminals where a full TUI
//! is awkward. Prompts go to stderr so stdout stays parseable (see
//! `ki configure list --json`); the persistence lives in
//! [`crate::provider_config`].

use std::io::{self, IsTerminal, Write};

use crate::config;
use crate::provider_config::{self, DraftMode, ModelDraft, ProviderDraft, PROTOCOLS};

/// Exit code for an operational error (IO failure, bad args).
const EXIT_ERROR: i32 = 2;
/// Returned by the prompt helpers when stdin ends (piped input exhausted); the
/// menu exits on it instead of spinning on a prompt that can never be answered.
const INPUT_CLOSED: &str = "input closed";

/// `ki configure [sub]` entry. `args` is the slice *after* `configure`.
pub async fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str).unwrap_or("") {
        "" => run_menu(),
        "list" => run_list(&args[1..]),
        "--help" | "-h" | "help" => {
            print_help();
            0
        }
        other => {
            eprintln!("error: unknown configure subcommand \"{other}\"");
            eprintln!();
            print_help();
            EXIT_ERROR
        }
    }
}

fn print_help() {
    println!(
        "\
ki configure — edit ~/.ki/agent/models.json

USAGE:
    ki configure               Interactive menu (list, add/edit/remove providers and models)
    ki configure list [--json] Print the configured providers and models

Every prompt is a menu: pick with arrow keys (or j/k, or the number) and press
Enter; only ids, names, and custom URLs/keys are typed. In a pipe — a script or
CI — the menus degrade to numbered prompts, so answers can be piped in.

Every change is written to ~/.ki/agent/models.json. A running ki picks the
change up automatically (it watches the file); other processes see it on the
next launch. `ki auth login` manages the credential store (auth.json) instead."
    );
}

// ===========================================================================
// Non-interactive listing
// ===========================================================================

fn run_list(args: &[String]) -> i32 {
    let json = args.iter().any(|arg| arg == "--json");
    let cfg = match config::load_models_config() {
        Ok(cfg) => cfg,
        Err(error) => {
            eprintln!("error: {error}");
            return EXIT_ERROR;
        }
    };

    if json {
        let providers: Vec<serde_json::Value> = cfg
            .providers
            .iter()
            .map(|(id, provider)| {
                serde_json::json!({
                    "id": id,
                    "name": provider.name,
                    "protocol": provider.api,
                    "baseUrl": provider.base_url,
                    "hasApiKey": provider
                        .api_key
                        .as_deref()
                        .is_some_and(|key| !key.is_empty()),
                    "models": provider
                        .models
                        .iter()
                        .map(|m| m.id.clone())
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "providers": providers }))
                .unwrap_or_else(|_| "{}".to_string())
        );
        return 0;
    }

    if cfg.providers.is_empty() {
        println!("No providers in models.json yet — run `ki configure` to add one.");
        return 0;
    }
    println!(
        "{:<18} {:<20} {:<8} {}",
        "provider", "protocol", "key", "models"
    );
    for (id, provider) in &cfg.providers {
        let key = match provider.api_key.as_deref() {
            Some(value) if !value.is_empty() => "set",
            _ => "missing",
        };
        let models: Vec<&str> = provider.models.iter().map(|m| m.id.as_str()).collect();
        println!(
            "{:<18} {:<20} {:<8} {}",
            id,
            provider_config::protocol_label(provider.api.as_deref()),
            key,
            if models.is_empty() {
                "(none)".to_string()
            } else {
                models.join(", ")
            }
        );
    }
    0
}

// ===========================================================================
// Interactive menu
// ===========================================================================

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    AddProvider,
    EditProvider,
    RemoveProvider,
    AddModel,
    EditModel,
    RemoveModel,
    List,
}

fn run_menu() -> i32 {
    const ACTIONS: [&str; 8] = [
        "Add a provider",
        "Edit a provider",
        "Remove a provider",
        "Add a model to a provider",
        "Edit a model",
        "Remove a model",
        "List providers and models",
        "Quit",
    ];
    let options: Vec<String> = ACTIONS.iter().map(|s| s.to_string()).collect();
    let mut status: Option<String> = None;
    loop {
        println!();
        match config::models_path() {
            Ok(path) => println!("models.json — {}", path.display()),
            Err(error) => println!("models.json — {error}"),
        }
        print_summary();
        if let Some(message) = status.take() {
            println!("  {message}");
        }
        println!();

        let choice = match select("What do you want to do?", &options, 0) {
            Ok(choice) => choice,
            Err(error) if error == "cancelled" || error.contains(INPUT_CLOSED) => return 0,
            Err(error) => {
                status = Some(format!("error: {error}"));
                continue;
            }
        };
        let action = match choice {
            0 => Action::AddProvider,
            1 => Action::EditProvider,
            2 => Action::RemoveProvider,
            3 => Action::AddModel,
            4 => Action::EditModel,
            5 => Action::RemoveModel,
            6 => Action::List,
            _ => return 0,
        };

        match action {
            Action::List => {
                println!();
                match run_list(&[]) {
                    0 => status = None,
                    _ => status = Some("could not read models.json".to_string()),
                }
            }
            other => match run_action(other) {
                Ok(message) => status = message,
                Err(error) if error == "cancelled" => status = Some("Cancelled.".to_string()),
                Err(error) if error.contains(INPUT_CLOSED) => return 0,
                Err(error) => status = Some(format!("error: {error}")),
            },
        }
    }
}

fn run_action(action: Action) -> Result<Option<String>, String> {
    match action {
        Action::AddProvider => wizard_add_provider().map(Some),
        Action::EditProvider => wizard_edit_provider().map(Some),
        Action::RemoveProvider => wizard_remove_provider().map(Some),
        Action::AddModel => wizard_add_model().map(Some),
        Action::EditModel => wizard_edit_model().map(Some),
        Action::RemoveModel => wizard_remove_model().map(Some),
        Action::List => Ok(None),
    }
}

fn print_summary() {
    match config::load_models_config() {
        Ok(cfg) if cfg.providers.is_empty() => {
            println!("  (no providers configured yet)");
        }
        Ok(cfg) => {
            for (id, provider) in &cfg.providers {
                let models: Vec<&str> = provider.models.iter().map(|m| m.id.as_str()).collect();
                println!(
                    "  {id:<16} {} · key {} · {}",
                    provider_config::protocol_label(provider.api.as_deref()),
                    match provider.api_key.as_deref() {
                        Some(value) if !value.is_empty() => "set",
                        _ => "missing",
                    },
                    if models.is_empty() {
                        "(no models)".to_string()
                    } else {
                        models.join(", ")
                    }
                );
            }
        }
        Err(error) => println!("  could not read models.json: {error}"),
    }
}

// ===========================================================================
// Wizards
// ===========================================================================

fn wizard_add_provider() -> Result<String, String> {
    let id = ask_required("Provider id (e.g. openrouter): ", "provider id")?;
    if provider_config::provider_exists(&id) {
        return Err(format!(
            "provider \"{id}\" already exists — use edit instead"
        ));
    }
    let api = ask_protocol(None)?;
    let base_url = ask_base_url(&api, "")?;
    let api_key = ask_api_key("")?;
    let model_id = ask_required("First model id: ", "model id")?;
    let model_name = ask("Model display name (optional, Enter to skip): ")?;

    provider_config::save_provider(&ProviderDraft {
        mode: DraftMode::Add,
        id,
        name: String::new(),
        api,
        base_url,
        api_key,
        model_id,
        model_name,
    })
}

fn wizard_edit_provider() -> Result<String, String> {
    let id = pick_provider("Which provider do you want to change?")?;
    let cfg = config::load_models_config().map_err(|error| error.to_string())?;
    let provider = cfg
        .providers
        .get(&id)
        .ok_or_else(|| format!("provider \"{id}\" disappeared"))?
        .clone();

    // Echo the effective display name (an unset `name` renders as the id).
    let current_name = provider.name.clone().unwrap_or_else(|| id.clone());
    let name = ask_or("Display name: ", &current_name)?;
    let api = ask_protocol(provider.api.as_deref())?;
    let base_url = ask_base_url(&api, provider.base_url.as_deref().unwrap_or_default())?;
    let api_key = ask_api_key(provider.api_key.as_deref().unwrap_or_default())?;

    provider_config::save_provider(&ProviderDraft {
        mode: DraftMode::Edit,
        id,
        name,
        api,
        base_url,
        api_key,
        ..Default::default()
    })
}

fn wizard_remove_provider() -> Result<String, String> {
    let id = pick_provider("Which provider do you want to remove?")?;
    if !confirm(&format!(
        "Remove provider \"{id}\" and all of its models from models.json?"
    ))? {
        return Ok("Cancelled.".to_string());
    }
    provider_config::remove_provider(&id)
}

fn wizard_add_model() -> Result<String, String> {
    let provider_id = pick_provider("Which provider should get the new model?")?;
    let id = ask_required("Model id: ", "model id")?;
    let name = ask("Model display name (optional, Enter to skip): ")?;
    let context_window = ask_count("Context window", None)?;
    let max_tokens = ask_count("Max output tokens", None)?;
    let reasoning = ask_yes_no("Does this model accept a reasoning/thinking budget?", false)?;
    let images = ask_yes_no("Does this model accept image attachments?", false)?;

    provider_config::save_model(&ModelDraft {
        mode: DraftMode::Add,
        provider_id,
        id,
        name,
        context_window,
        max_tokens,
        reasoning,
        images,
        ..Default::default()
    })
}

fn wizard_edit_model() -> Result<String, String> {
    let provider_id = pick_provider("Which provider's model do you want to change?")?;
    let model_id = pick_model(&provider_id, "Which model?")?;
    let cfg = config::load_models_config().map_err(|error| error.to_string())?;
    let def = cfg
        .providers
        .get(&provider_id)
        .and_then(|provider| provider.models.iter().find(|m| m.id == model_id))
        .cloned()
        .ok_or_else(|| format!("model \"{model_id}\" disappeared"))?;

    // Echo the effective display name; an empty answer keeps it.
    let current_name = def.name.clone().unwrap_or_else(|| def.id.clone());
    let id = ask_or("Model id: ", &def.id)?;
    let name = ask_or("Display name: ", &current_name)?;
    let context_window = ask_count("Context window", def.context_window)?;
    let max_tokens = ask_count("Max output tokens", def.max_tokens)?;
    let reasoning = ask_yes_no(
        "Does this model accept a reasoning/thinking budget?",
        def.reasoning.unwrap_or(false),
    )?;
    let images = ask_yes_no(
        "Does this model accept image attachments?",
        def.input
            .as_ref()
            .is_some_and(|list| list.iter().any(|s| s.eq_ignore_ascii_case("image"))),
    )?;

    provider_config::save_model(&ModelDraft {
        mode: DraftMode::Edit,
        provider_id,
        original_id: Some(def.id.clone()),
        id,
        name,
        context_window,
        max_tokens,
        reasoning,
        images,
    })
}

fn wizard_remove_model() -> Result<String, String> {
    let provider_id = pick_provider("Which provider's model do you want to remove?")?;
    let model_id = pick_model(&provider_id, "Which model?")?;
    if !confirm(&format!(
        "Remove model \"{model_id}\" from provider \"{provider_id}\"?"
    ))? {
        return Ok("Cancelled.".to_string());
    }
    provider_config::remove_model(&provider_id, &model_id)
}

// ===========================================================================
// Choice prompt (arrow keys; numbered fallback when not a terminal)
// ===========================================================================

/// Restores cooked mode even on the error path.
struct RawModeGuard;

impl RawModeGuard {
    fn new() -> Result<Self, String> {
        crossterm::terminal::enable_raw_mode().map_err(|error| error.to_string())?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Pick one of `options` with ↑/↓ + Enter. Prompts render on stderr so stdout
/// stays parseable; when stdin or stderr is not a terminal the prompt falls back
/// to a numbered line (so piped scripts and CI still work), where an option can
/// also be named by a unique prefix.
fn select(prompt: &str, options: &[String], default: usize) -> Result<usize, String> {
    if options.is_empty() {
        return Err("nothing to choose from".to_string());
    }
    let default = default.min(options.len() - 1);

    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        eprintln!("{prompt}");
        for (index, option) in options.iter().enumerate() {
            eprintln!("  {}) {option}", index + 1);
        }
        let answer = read_line(&format!(
            "Choose 1-{} (default {}): ",
            options.len(),
            default + 1
        ))?;
        return parse_choice(&answer, options, default);
    }

    let mut cursor = default;
    let guard = RawModeGuard::new()?;
    let mut stderr = io::stderr();
    let mut drawn = 0usize;
    loop {
        if drawn > 0 {
            let _ = crossterm::execute!(
                stderr,
                crossterm::cursor::MoveUp(drawn as u16),
                crossterm::terminal::Clear(crossterm::terminal::ClearType::FromCursorDown)
            );
        }
        let _ = write!(stderr, "{prompt}\r\n");
        for (index, option) in options.iter().enumerate() {
            if index == cursor {
                let _ = write!(stderr, "  \x1b[7m❯ {option}\x1b[0m\r\n");
            } else {
                let _ = write!(stderr, "    {option}\r\n");
            }
        }
        let _ = stderr.flush();
        drawn = options.len() + 1;

        match crossterm::event::read() {
            Ok(crossterm::event::Event::Key(key))
                if matches!(
                    key.kind,
                    crossterm::event::KeyEventKind::Press | crossterm::event::KeyEventKind::Repeat
                ) =>
            {
                match key.code {
                    crossterm::event::KeyCode::Up | crossterm::event::KeyCode::Char('k') => {
                        cursor = if cursor == 0 {
                            options.len() - 1
                        } else {
                            cursor - 1
                        };
                    }
                    crossterm::event::KeyCode::Down | crossterm::event::KeyCode::Char('j') => {
                        cursor = (cursor + 1) % options.len();
                    }
                    crossterm::event::KeyCode::Enter | crossterm::event::KeyCode::Char(' ') => {
                        break
                    }
                    crossterm::event::KeyCode::Esc => {
                        drop(guard);
                        return Err("cancelled".to_string());
                    }
                    crossterm::event::KeyCode::Char(c) if c.is_ascii_digit() => {
                        let digit = c.to_digit(10).unwrap_or(0) as usize;
                        if digit >= 1 && digit <= options.len() {
                            cursor = digit - 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            Ok(_) => {}
            Err(error) => {
                drop(guard);
                return Err(format!("could not read a key: {error}"));
            }
        }
    }

    // Collapse the moving cursor into a plain record of the answer.
    let _ = crossterm::execute!(
        stderr,
        crossterm::cursor::MoveUp(drawn as u16),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::FromCursorDown)
    );
    let _ = write!(stderr, "{prompt}\r\n");
    for (index, option) in options.iter().enumerate() {
        let marker = if index == cursor { "❯" } else { " " };
        let _ = write!(stderr, "  {marker} {option}\r\n");
    }
    let _ = stderr.flush();
    drop(guard);
    Ok(cursor)
}

/// Parse a non-terminal answer: a 1-based number, or a (case-insensitive)
/// prefix of one of the option labels.
fn parse_choice(answer: &str, options: &[String], default: usize) -> Result<usize, String> {
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(default);
    }
    if let Ok(number) = answer.parse::<usize>() {
        if number >= 1 && number <= options.len() {
            return Ok(number - 1);
        }
        return Err(format!("choice must be between 1 and {}", options.len()));
    }
    let matches: Vec<usize> = options
        .iter()
        .enumerate()
        .filter(|(_, option)| {
            option
                .to_ascii_lowercase()
                .starts_with(&answer.to_ascii_lowercase())
        })
        .map(|(index, _)| index)
        .collect();
    match matches.as_slice() {
        [only] => Ok(*only),
        [] => Err(format!("no option matches \"{answer}\"")),
        _ => Err(format!(
            "\"{answer}\" is ambiguous — use a number 1-{}",
            options.len()
        )),
    }
}

/// Yes/no as a two-item choice.
fn ask_yes_no(question: &str, current: bool) -> Result<bool, String> {
    let options = vec!["Yes".to_string(), "No".to_string()];
    Ok(select(question, &options, if current { 0 } else { 1 })? == 0)
}

/// Confirmation for a destructive step; default is "no".
fn confirm(question: &str) -> Result<bool, String> {
    let options = vec!["No, keep it".to_string(), "Yes, remove it".to_string()];
    Ok(select(question, &options, 0)? == 1)
}

/// Protocol picker; the stored protocol is labelled `(current)`.
fn ask_protocol(current: Option<&str>) -> Result<String, String> {
    let options: Vec<String> = PROTOCOLS
        .iter()
        .map(|(token, label)| {
            if Some(*token) == current {
                format!("{label}  (current)")
            } else {
                (*label).to_string()
            }
        })
        .collect();
    let default = current
        .and_then(|api| PROTOCOLS.iter().position(|(token, _)| *token == api))
        .unwrap_or(1); // anthropic-compatible is the default runtime
    Ok(PROTOCOLS[select("Protocol:", &options, default)?]
        .0
        .to_string())
}

/// API key as a choice: keep the current value, reference a common environment
/// variable, or type one. Only the last option needs the keyboard.
fn ask_api_key(current: &str) -> Result<String, String> {
    const COMMON: [&str; 6] = [
        "$ANTHROPIC_API_KEY",
        "$OPENAI_API_KEY",
        "$DEEPSEEK_API_KEY",
        "$OPENROUTER_API_KEY",
        "$MOONSHOT_API_KEY",
        "$GEMINI_API_KEY",
    ];
    // (label, value) where `None` means "ask for a literal value".
    let mut choices: Vec<(String, Option<String>)> = Vec::new();
    if !current.is_empty() {
        choices.push((
            format!("Keep the current value  ({})", preview_secret(current)),
            Some(current.to_string()),
        ));
    }
    for env in COMMON {
        let marker = if current == env { "  (current)" } else { "" };
        choices.push((format!("Use {env}{marker}"), Some(env.to_string())));
    }
    choices.push(("Type a key or $ENV reference…".to_string(), None));
    choices.push(("Leave empty".to_string(), Some(String::new())));

    let (labels, values): (Vec<String>, Vec<Option<String>>) = choices.into_iter().unzip();
    let index = select("API key:", &labels, 0)?;
    match &values[index] {
        Some(value) => Ok(value.clone()),
        None => read_line("API key (or $ENV reference): "),
    }
}

/// Base URL as a choice: keep the current value, or pick a well-known endpoint
/// for the chosen protocol, or type a custom one.
fn ask_base_url(api: &str, current: &str) -> Result<String, String> {
    let known: &[(&str, &str)] = match api {
        "openai-completions" | "openai-responses" => &[
            ("OpenAI", "https://api.openai.com/v1"),
            ("DeepSeek", "https://api.deepseek.com"),
            ("OpenRouter", "https://openrouter.ai/api/v1"),
            ("Together", "https://api.together.xyz/v1"),
        ],
        _ => &[
            ("Anthropic", "https://api.anthropic.com"),
            ("DeepSeek (anthropic)", "https://api.deepseek.com/anthropic"),
            (
                "Moonshot/Kimi (anthropic)",
                "https://api.moonshot.cn/anthropic",
            ),
        ],
    };
    let mut choices: Vec<(String, Option<String>)> = Vec::new();
    if !current.is_empty() {
        choices.push((
            format!("Keep the current URL  ({current})"),
            Some(current.to_string()),
        ));
    }
    for (name, url) in known {
        let marker = if current == *url { "  (current)" } else { "" };
        choices.push((format!("{name}  {url}{marker}"), Some((*url).to_string())));
    }
    choices.push(("Type a custom URL…".to_string(), None));

    let (labels, values): (Vec<String>, Vec<Option<String>>) = choices.into_iter().unzip();
    let index = select("Base URL:", &labels, 0)?;
    let url = match &values[index] {
        Some(value) => value.clone(),
        None => read_line("Base URL (http:// or https://): ")?,
    };
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(format!(
            "invalid base URL \"{url}\" — it must start with http:// or https://"
        ));
    }
    Ok(url)
}

/// Optional token count as a choice of common sizes plus a custom entry.
fn ask_count(prompt: &str, current: Option<u64>) -> Result<Option<u64>, String> {
    let presets: &[u64] = if prompt.starts_with("Context") {
        &[32_000, 128_000, 200_000, 1_000_000]
    } else {
        &[4_096, 8_192, 16_384, 32_768]
    };
    let mut choices: Vec<(String, Option<u64>)> = Vec::new();
    if let Some(value) = current {
        choices.push((format!("Keep {value}  (current)"), Some(value)));
    }
    choices.push(("Leave unset".to_string(), None));
    for preset in presets {
        let marker = if current == Some(*preset) {
            "  (current)"
        } else {
            ""
        };
        choices.push((format!("{preset}{marker}"), Some(*preset)));
    }
    choices.push(("Enter another number…".to_string(), None));

    let (labels, values): (Vec<String>, Vec<Option<u64>>) = choices.into_iter().unzip();
    let index = select(prompt, &labels, 0)?;
    match values[index] {
        Some(value) => Ok(Some(value)),
        // `None` covers both "Leave unset" and "Enter another number…"; the
        // first `None` entry is the unset one, the last is the custom one.
        None if labels[index].starts_with("Leave unset") => Ok(None),
        None => {
            let answer = read_line("Value (empty to leave unset): ")?;
            provider_config::parse_optional_count(&answer, "value")
        }
    }
}

fn pick_provider(question: &str) -> Result<String, String> {
    let cfg = config::load_models_config().map_err(|error| error.to_string())?;
    if cfg.providers.is_empty() {
        return Err("no providers in models.json yet — add one first".to_string());
    }
    let ids: Vec<String> = cfg.providers.keys().cloned().collect();
    let labels: Vec<String> = ids
        .iter()
        .map(|id| {
            let provider = &cfg.providers[id];
            format!(
                "{id}  —  {} model(s) · {} · {}",
                provider.models.len(),
                provider_config::protocol_label(provider.api.as_deref()),
                provider.base_url.as_deref().unwrap_or("(no base URL)")
            )
        })
        .collect();
    Ok(ids[select(question, &labels, 0)?].clone())
}

fn pick_model(provider_id: &str, question: &str) -> Result<String, String> {
    let cfg = config::load_models_config().map_err(|error| error.to_string())?;
    let provider = cfg
        .providers
        .get(provider_id)
        .ok_or_else(|| format!("provider \"{provider_id}\" is not in models.json"))?;
    if provider.models.is_empty() {
        return Err(format!("provider \"{provider_id}\" has no models yet"));
    }
    let ids: Vec<String> = provider.models.iter().map(|m| m.id.clone()).collect();
    let labels: Vec<String> = provider
        .models
        .iter()
        .map(|model| {
            format!(
                "{}  —  {} · ctx {} · out {}",
                model.id,
                model.name.as_deref().unwrap_or(&model.id),
                model
                    .context_window
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "unset".to_string()),
                model
                    .max_tokens
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "unset".to_string()),
            )
        })
        .collect();
    Ok(ids[select(question, &labels, 0)?].clone())
}

// ===========================================================================
// Typed fallbacks (ids, names, custom URLs/keys)
// ===========================================================================

fn read_line(prompt: &str) -> Result<String, String> {
    eprint!("{prompt}");
    io::stderr().flush().ok();
    let mut line = String::new();
    let read = io::stdin()
        .read_line(&mut line)
        .map_err(|error| format!("could not read input: {error}"))?;
    if read == 0 {
        return Err(INPUT_CLOSED.to_string());
    }
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

fn ask(prompt: &str) -> Result<String, String> {
    read_line(prompt)
}

/// Free-text prompt where an empty answer keeps `current`.
fn ask_or(prompt: &str, current: &str) -> Result<String, String> {
    let answer = read_line(&format!("{prompt}[{current}]: "))?;
    Ok(if answer.trim().is_empty() {
        current.to_string()
    } else {
        answer.trim().to_string()
    })
}

fn ask_required(prompt: &str, what: &str) -> Result<String, String> {
    let answer = read_line(prompt)?;
    if answer.trim().is_empty() {
        return Err(format!("{what} cannot be empty"));
    }
    Ok(answer.trim().to_string())
}

/// Show a credential without printing it.
fn preview_secret(raw: &str) -> String {
    if raw.starts_with('$') || raw.starts_with('!') {
        return raw.to_string();
    }
    let count = raw.chars().count();
    if count <= 8 {
        return "*".repeat(count.max(1));
    }
    let head: String = raw.chars().take(4).collect();
    let tail: String = raw.chars().skip(count - 4).collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Vec<String> {
        vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]
    }

    #[test]
    fn non_terminal_answers_accept_numbers_or_unique_prefixes() {
        let opts = options();
        assert_eq!(parse_choice("1", &opts, 9).unwrap(), 0);
        assert_eq!(parse_choice("3", &opts, 9).unwrap(), 2);
        // empty keeps the default (0 if out of range)
        assert_eq!(parse_choice("  ", &opts, 2).unwrap(), 2);
        // a unique label prefix works too, case-insensitively
        assert_eq!(parse_choice("be", &opts, 0).unwrap(), 1);
        assert_eq!(parse_choice("GAM", &opts, 0).unwrap(), 2);
        // out-of-range numbers and unknown labels are errors
        assert!(parse_choice("4", &opts, 0).is_err());
        assert!(parse_choice("0", &opts, 0).is_err());
        assert!(parse_choice("delta", &opts, 0).is_err());
    }

    #[test]
    fn preview_secret_never_prints_the_key() {
        assert_eq!(preview_secret("$KIMI_API_KEY"), "$KIMI_API_KEY");
        assert_eq!(preview_secret("short"), "*****");
        let masked = preview_secret("sk-abcdefghijklmnop");
        assert!(
            masked.starts_with("sk-a") && masked.ends_with("mnop"),
            "{masked}"
        );
        assert!(!masked.contains("efgh"), "{masked}");
    }
}
