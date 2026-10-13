//! `rpi configure` — interactive provider/model configuration for
//! `~/.rpi/agent/models.json`.
//!
//! The CLI counterpart of provider management: a plain stdin/stdout wizard that
//! works over SSH, in a piped shell, and on Windows terminals where a full TUI
//! is awkward. Prompts go to stderr so stdout stays parseable (see
//! `rpi configure list --json`); the persistence lives in
//! [`crate::provider_config`].

use std::io::{self, IsTerminal, Write};

use crate::config;
use crate::provider_config::{self, DraftMode, ModelDraft, ProviderDraft, PROTOCOLS};

/// Exit code for an operational error (IO failure, bad args).
const EXIT_ERROR: i32 = 2;

/// `rpi configure [sub]` entry. `args` is the slice *after* `configure`.
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
rpi configure — edit ~/.rpi/agent/models.json

USAGE:
    rpi configure               Interactive menu (list, add/edit/remove providers and models)
    rpi configure list [--json] Print the configured providers and models

Every change is written to ~/.rpi/agent/models.json. A running rpi picks the
change up automatically (it watches the file); other processes see it on the
next launch. `rpi auth login` manages the credential store (auth.json) instead."
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
        println!("No providers in models.json yet — run `rpi configure` to add one.");
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
        println!("Actions:");
        println!("  1) Add a provider");
        println!("  2) Edit a provider");
        println!("  3) Remove a provider");
        println!("  4) Add a model to a provider");
        println!("  5) Edit a model");
        println!("  6) Remove a model");
        println!("  7) List providers and models");
        println!("  q) Quit");

        let choice = match ask("> ") {
            Ok(choice) => choice,
            Err(_) => return 0, // stdin closed
        };
        let action = match choice.trim() {
            "1" => Action::AddProvider,
            "2" => Action::EditProvider,
            "3" => Action::RemoveProvider,
            "4" => Action::AddModel,
            "5" => Action::EditModel,
            "6" => Action::RemoveModel,
            "7" => Action::List,
            "" | "q" | "quit" | "exit" => return 0,
            other => {
                status = Some(format!("unknown choice \"{other}\""));
                continue;
            }
        };

        match action {
            Action::List => match run_list(&[]) {
                0 => status = None,
                _ => status = Some("could not read models.json".to_string()),
            },
            other => match run_action(other) {
                Ok(message) => status = message,
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
    let id = ask("Provider id (e.g. openrouter): ")?;
    let id = id.trim().to_string();
    if id.is_empty() {
        return Err("provider id cannot be empty".to_string());
    }
    if provider_config::provider_exists(&id) {
        return Err(format!(
            "provider \"{id}\" already exists — use edit instead"
        ));
    }
    let api = ask_protocol(None)?;
    let base_url = ask_required("Base URL (http:// or https://): ", "base URL")?;
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        return Err(format!(
            "invalid base URL \"{base_url}\" — it must start with http:// or https://"
        ));
    }
    let api_key = ask_secret("API key (a literal key or an $ENV reference): ", "")?;
    let model_id = ask_required("First model id: ", "model id")?;
    let model_name = ask("Model display name (optional): ")?;

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

    let current_api = provider.api.clone();
    // Echo the effective display name (an unset `name` renders as the id).
    let current_name = provider.name.clone().unwrap_or_else(|| id.clone());
    let name = ask_or("Display name: ", &current_name)?;
    let api = ask_protocol(current_api.as_deref())?;
    let base_url = ask_or(
        "Base URL: ",
        provider.base_url.as_deref().unwrap_or_default(),
    )?;
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        return Err(format!(
            "invalid base URL \"{base_url}\" — it must start with http:// or https://"
        ));
    }
    let api_key = ask_secret("API key: ", provider.api_key.as_deref().unwrap_or_default())?;

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
    let name = ask("Model display name (optional): ")?;
    let context_window = ask_count("Context window in tokens (optional): ", None)?;
    let max_tokens = ask_count("Max output tokens (optional): ", None)?;
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
    let context_window = ask_count("Context window in tokens (empty keeps)", def.context_window)?;
    let max_tokens = ask_count("Max output tokens (empty keeps)", def.max_tokens)?;
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
// Prompting helpers (prompts on stderr, so stdout stays parseable)
// ===========================================================================

fn read_line(prompt: &str) -> Result<String, String> {
    eprint!("{prompt}");
    io::stderr().flush().ok();
    let mut line = String::new();
    let read = io::stdin()
        .read_line(&mut line)
        .map_err(|error| format!("could not read input: {error}"))?;
    if read == 0 {
        return Err("input closed".to_string());
    }
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

/// Free-text prompt.
fn ask(prompt: &str) -> Result<String, String> {
    read_line(prompt)
}

/// Free-text prompt where an empty answer keeps `current`.
fn ask_or(prompt: &str, current: &str) -> Result<String, String> {
    let shown = if current.is_empty() {
        format!("{prompt}(empty to skip): ")
    } else {
        format!("{prompt}[{current}]: ")
    };
    let answer = read_line(&shown)?;
    Ok(if answer.trim().is_empty() {
        current.to_string()
    } else {
        answer
    })
}

/// Free-text prompt that must not be blank.
fn ask_required(prompt: &str, what: &str) -> Result<String, String> {
    let answer = read_line(prompt)?;
    if answer.trim().is_empty() {
        return Err(format!("{what} cannot be empty"));
    }
    Ok(answer.trim().to_string())
}

/// Secret prompt: no echo on a terminal, a plain line otherwise (so a piped
/// script or a test can drive it). An empty answer keeps `current`.
fn ask_secret(prompt: &str, current: &str) -> Result<String, String> {
    let shown = if current.is_empty() {
        format!("{prompt}(empty to skip): ")
    } else {
        format!("{prompt}[unchanged]: ")
    };
    eprint!("{shown}");
    io::stderr().flush().ok();
    let answer = if io::stdin().is_terminal() {
        eprintln!();
        rpassword::read_password().map_err(|error| format!("could not read the secret: {error}"))?
    } else {
        let mut line = String::new();
        let read = io::stdin()
            .read_line(&mut line)
            .map_err(|error| format!("could not read input: {error}"))?;
        if read == 0 {
            return Err("input closed".to_string());
        }
        line.trim_end_matches(['\n', '\r']).to_string()
    };
    Ok(if answer.trim().is_empty() {
        current.to_string()
    } else {
        answer
    })
}

/// Optional positive integer; empty keeps `current`.
fn ask_count(prompt: &str, current: Option<u64>) -> Result<Option<u64>, String> {
    let shown = match current {
        Some(value) => format!("{prompt} [{value}]: "),
        None => format!("{prompt}: "),
    };
    let answer = read_line(&shown)?;
    if answer.trim().is_empty() {
        return Ok(current);
    }
    provider_config::parse_optional_count(&answer, "value")
}

/// Yes/no question; empty keeps `current`.
fn ask_yes_no(question: &str, current: bool) -> Result<bool, String> {
    let default = if current { "Y/n" } else { "y/N" };
    let answer = read_line(&format!("{question} [{default}]: "))?;
    match answer.trim().to_ascii_lowercase().as_str() {
        "" => Ok(current),
        "y" | "yes" | "true" | "1" => Ok(true),
        "n" | "no" | "false" | "0" => Ok(false),
        other => Err(format!("expected y or n (got \"{other}\")")),
    }
}

/// Confirmation for a destructive step; defaults to no.
fn confirm(question: &str) -> Result<bool, String> {
    ask_yes_no(
        &format!("{question} This cannot be undone (the file is rewritten)"),
        false,
    )
}

/// Protocol picker. Accepts a number, a protocol token, or a loose name.
fn ask_protocol(current: Option<&str>) -> Result<String, String> {
    eprintln!("Protocol:");
    for (index, (token, label)) in PROTOCOLS.iter().enumerate() {
        let marker = if Some(*token) == current {
            " (current)"
        } else {
            ""
        };
        eprintln!("  {}) {label}{marker}", index + 1);
    }
    let default = current
        .and_then(|api| PROTOCOLS.iter().position(|(token, _)| *token == api))
        .unwrap_or(1); // anthropic-compatible is the default runtime
    let answer = read_line(&format!(
        "Choose [1-{}] (default {}): ",
        PROTOCOLS.len(),
        default + 1
    ))?;
    let answer = answer.trim().to_ascii_lowercase();
    if answer.is_empty() {
        return Ok(PROTOCOLS[default].0.to_string());
    }
    if let Ok(index) = answer.parse::<usize>() {
        if (1..=PROTOCOLS.len()).contains(&index) {
            return Ok(PROTOCOLS[index - 1].0.to_string());
        }
        return Err(format!(
            "protocol number must be between 1 and {}",
            PROTOCOLS.len()
        ));
    }
    if answer.contains("responses") {
        return Ok("openai-responses".to_string());
    }
    if answer.contains("completions") {
        return Ok("openai-completions".to_string());
    }
    if answer.contains("anthropic") {
        return Ok("anthropic-messages".to_string());
    }
    if answer.contains("openai") {
        return Ok("openai-completions".to_string());
    }
    Err(format!("unknown protocol \"{answer}\""))
}

/// Provider picker (numbered, also accepts a raw id).
fn pick_provider(question: &str) -> Result<String, String> {
    let cfg = config::load_models_config().map_err(|error| error.to_string())?;
    if cfg.providers.is_empty() {
        return Err("no providers in models.json yet — add one first".to_string());
    }
    let ids: Vec<String> = cfg.providers.keys().cloned().collect();
    eprintln!("{question}");
    for (index, id) in ids.iter().enumerate() {
        let provider = &cfg.providers[id];
        eprintln!(
            "  {}) {id} — {} model(s) · {} · {}",
            index + 1,
            provider.models.len(),
            provider_config::protocol_label(provider.api.as_deref()),
            provider.base_url.as_deref().unwrap_or("(no base URL)")
        );
    }
    let answer = read_line("Choose provider (number or id): ")?;
    resolve_choice(&answer, &ids, "provider")
}

/// Model picker for one provider.
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
    eprintln!("{question}");
    for (index, model) in provider.models.iter().enumerate() {
        eprintln!(
            "  {}) {} — {} · ctx {} · out {}",
            index + 1,
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
        );
    }
    let answer = read_line("Choose model (number or id): ")?;
    resolve_choice(&answer, &ids, "model")
}

/// Resolve a numbered-or-named answer against `ids`.
fn resolve_choice(answer: &str, ids: &[String], what: &str) -> Result<String, String> {
    let answer = answer.trim();
    if let Ok(index) = answer.parse::<usize>() {
        if (1..=ids.len()).contains(&index) {
            return Ok(ids[index - 1].clone());
        }
        return Err(format!("{what} number must be between 1 and {}", ids.len()));
    }
    if let Some(id) = ids.iter().find(|id| id.eq_ignore_ascii_case(answer)) {
        return Ok(id.clone());
    }
    Err(format!("unknown {what} \"{answer}\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choice_resolution_accepts_numbers_and_ids() {
        let ids = vec!["alpha".to_string(), "beta".to_string()];
        assert_eq!(resolve_choice("1", &ids, "provider").unwrap(), "alpha");
        assert_eq!(resolve_choice("2", &ids, "provider").unwrap(), "beta");
        // case-insensitive id
        assert_eq!(resolve_choice("BETA", &ids, "provider").unwrap(), "beta");
        assert!(resolve_choice("3", &ids, "provider").is_err());
        assert!(resolve_choice("gamma", &ids, "provider").is_err());
    }
}
