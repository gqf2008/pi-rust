//! Read/write helpers for the providers and models in
//! `~/.rpi/agent/models.json`.
//!
//! Shared by the `rpi configure` subcommand (this used to live in the TUI's
//! `/provider` surface). Every write rewrites the whole file, so the helpers
//! here own the invariants: a duplicate provider/model id is refused, an edit
//! touches only the fields the caller supplied, and a display name that merely
//! repeats the id is stored as `None` instead of being duplicated.

use crate::config::{self, ModelDefinition, ProviderConfig};

/// Whether a draft creates a new entry or rewrites an existing one.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DraftMode {
    #[default]
    Add,
    Edit,
}

/// Wire-protocol tokens `models.json` understands, in display order.
pub const PROTOCOLS: [(&str, &str); 3] = [
    ("openai-completions", "OpenAI Completions"),
    ("anthropic-messages", "Anthropic Messages"),
    ("openai-responses", "OpenAI Responses"),
];

/// A provider as collected by the `rpi configure` wizard.
#[derive(Clone, Debug, Default)]
pub struct ProviderDraft {
    pub mode: DraftMode,
    /// Provider id — the `models.json` map key. `Edit` never renames it.
    pub id: String,
    /// Optional display name; empty (or equal to `id`) stores `None`.
    pub name: String,
    /// Wire protocol token (one of [`PROTOCOLS`]).
    pub api: String,
    pub base_url: String,
    pub api_key: String,
    /// `Add` only: the first model created alongside the provider.
    pub model_id: String,
    pub model_name: String,
}

/// A model as collected by the `rpi configure` wizard.
#[derive(Clone, Debug, Default)]
pub struct ModelDraft {
    pub mode: DraftMode,
    pub provider_id: String,
    /// `Edit`: the model id being replaced. Ids may be renamed, so the original
    /// key is what locates the entry.
    pub original_id: Option<String>,
    pub id: String,
    pub name: String,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
    pub reasoning: bool,
    pub images: bool,
}

/// Human-readable protocol label for a `models.json` `api` value. `None` (or an
/// empty string) means the Anthropic-compatible default runtime, matching
/// [`config::provider_is_anthropic_compatible`].
pub fn protocol_label(api: Option<&str>) -> &'static str {
    match api {
        Some("openai-completions") => "OpenAI Completions",
        Some("openai-responses") => "OpenAI Responses",
        Some("anthropic-messages") | None | Some("") => "Anthropic Messages",
        Some(_) => "custom",
    }
}

/// Parse an optional positive integer field. Empty means "leave unset".
pub fn parse_optional_count(text: &str, field: &str) -> Result<Option<u64>, String> {
    let text = text.trim();
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

/// `None` when the name is blank or just repeats the id.
fn display_name_or_none(name: &str, id: &str) -> Option<String> {
    let name = name.trim();
    (!name.is_empty() && name != id).then(|| name.to_string())
}

/// Whether the provider id is already taken in `models.json`.
pub fn provider_exists(id: &str) -> bool {
    config::load_models_config()
        .map(|cfg| cfg.providers.contains_key(id))
        .unwrap_or(false)
}

/// Insert a new provider (carrying its first model) or rewrite an existing one.
///
/// `Edit` only touches the fields this draft owns — `models`, `headers`, and
/// `authHeader` survive.
pub fn save_provider(draft: &ProviderDraft) -> Result<String, String> {
    let mut cfg = config::load_models_config().map_err(|error| error.to_string())?;

    if draft.mode == DraftMode::Edit {
        let id = draft.id.trim().to_string();
        let (api, models) = {
            let Some(provider) = cfg.providers.get_mut(&id) else {
                return Err(format!("Provider \"{id}\" is not in models.json."));
            };
            provider.name = display_name_or_none(&draft.name, &id);
            provider.base_url = Some(draft.base_url.clone());
            provider.api_key = Some(draft.api_key.clone());
            provider.api = Some(draft.api.clone());
            (provider.api.clone(), provider.models.len())
        };
        config::save_models_config(&cfg).map_err(|error| error.to_string())?;
        return Ok(format!(
            "Updated provider \"{id}\" ({} · {models} model(s) kept).",
            protocol_label(api.as_deref())
        ));
    }

    let id = draft.id.trim().to_string();
    if cfg.providers.contains_key(&id) {
        return Err(format!(
            "Provider \"{id}\" already exists. Use `rpi configure` → edit instead."
        ));
    }
    if draft.model_id.trim().is_empty() {
        return Err("A new provider needs at least one model id.".to_string());
    }
    cfg.providers.insert(
        id.clone(),
        ProviderConfig {
            name: display_name_or_none(&draft.name, &id),
            base_url: Some(draft.base_url.clone()),
            api_key: Some(draft.api_key.clone()),
            api: Some(draft.api.clone()),
            headers: None,
            auth_header: None,
            models: vec![ModelDefinition {
                id: draft.model_id.trim().to_string(),
                name: display_name_or_none(&draft.model_name, draft.model_id.trim()),
                base_url: None,
                reasoning: None,
                context_window: None,
                max_tokens: None,
                input: None,
                headers: None,
                compat: None,
            }],
        },
    );
    config::save_models_config(&cfg).map_err(|error| error.to_string())?;
    Ok(format!(
        "Added provider \"{id}\" ({} · 1 model).",
        protocol_label(Some(draft.api.as_str()))
    ))
}

/// Append a model to a provider, or rewrite an existing one in place.
pub fn save_model(draft: &ModelDraft) -> Result<String, String> {
    let mut cfg = config::load_models_config().map_err(|error| error.to_string())?;
    let Some(provider) = cfg.providers.get_mut(&draft.provider_id) else {
        return Err(format!(
            "Provider \"{}\" is not in models.json.",
            draft.provider_id
        ));
    };
    let id = draft.id.trim().to_string();
    let name = display_name_or_none(&draft.name, &id);
    let input = Some(if draft.images {
        vec!["text".to_string(), "image".to_string()]
    } else {
        vec!["text".to_string()]
    });

    match draft.mode {
        DraftMode::Add => {
            if provider
                .models
                .iter()
                .any(|m| m.id.eq_ignore_ascii_case(&id))
            {
                return Err(format!(
                    "Model \"{id}\" already exists in \"{}\".",
                    draft.provider_id
                ));
            }
            provider.models.push(ModelDefinition {
                id: id.clone(),
                name,
                base_url: None,
                reasoning: Some(draft.reasoning),
                context_window: draft.context_window,
                max_tokens: draft.max_tokens,
                input,
                headers: None,
                compat: None,
            });
        }
        DraftMode::Edit => {
            let original = draft.original_id.clone().unwrap_or_else(|| id.clone());
            let Some(slot) = provider.models.iter_mut().find(|m| m.id == original) else {
                return Err(format!(
                    "Model \"{original}\" is not in \"{}\".",
                    draft.provider_id
                ));
            };
            // Only the fields this draft owns change; `baseUrl`, `headers`, and
            // `compat` are preserved.
            slot.id = id.clone();
            slot.name = name;
            slot.reasoning = Some(draft.reasoning);
            slot.context_window = draft.context_window;
            slot.max_tokens = draft.max_tokens;
            slot.input = input;
        }
    }

    let action = match draft.mode {
        DraftMode::Add => "Added",
        DraftMode::Edit => "Updated",
    };
    let api = provider.api.clone();
    config::save_models_config(&cfg).map_err(|error| error.to_string())?;
    Ok(format!(
        "{action} model \"{id}\" in \"{}\" ({}).",
        draft.provider_id,
        protocol_label(api.as_deref())
    ))
}

/// Remove a provider and drop a saved default that pointed at it.
pub fn remove_provider(id: &str) -> Result<String, String> {
    let id = id.trim();
    let mut cfg = config::load_models_config().map_err(|error| error.to_string())?;
    if cfg.providers.shift_remove(id).is_none() {
        return Err(format!(
            "Unknown or built-in provider \"{id}\". Only providers defined in models.json can be removed."
        ));
    }
    config::save_models_config(&cfg).map_err(|error| error.to_string())?;

    let mut settings = crate::settings::load_settings().unwrap_or_default();
    if settings.default_provider.as_deref() == Some(id) {
        settings.default_provider = None;
        let _ = crate::settings::save_settings(&settings);
    }
    Ok(format!("Removed provider \"{id}\"."))
}

/// Remove one model from a provider.
pub fn remove_model(provider_id: &str, model_id: &str) -> Result<String, String> {
    let mut cfg = config::load_models_config().map_err(|error| error.to_string())?;
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
    config::save_models_config(&cfg).map_err(|error| error.to_string())?;
    Ok(format!(
        "Removed model \"{model_id}\" from provider \"{provider_id}\"."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::env_lock;

    /// Run `body` against a throwaway `~/.rpi/agent` (holds the shared env lock
    /// so parallel env-mutating tests serialise).
    fn with_isolated_config_dir<T>(body: impl FnOnce() -> T) -> T {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var_os(config::CONFIG_DIR_ENV);
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var(config::CONFIG_DIR_ENV, tmp.path());
        let result = body();
        match prev {
            Some(value) => std::env::set_var(config::CONFIG_DIR_ENV, value),
            None => std::env::remove_var(config::CONFIG_DIR_ENV),
        }
        result
    }

    fn write_models_json(json: &str) {
        std::fs::write(config::models_path().unwrap(), json).unwrap();
    }

    #[test]
    fn adds_a_provider_with_its_first_model() {
        with_isolated_config_dir(|| {
            write_models_json(r#"{"providers":{}}"#);
            let draft = ProviderDraft {
                mode: DraftMode::Add,
                id: "gateway".into(),
                name: String::new(),
                api: "openai-completions".into(),
                base_url: "https://api.example.com/v1".into(),
                api_key: "$GATEWAY_KEY".into(),
                model_id: "gpt-x".into(),
                model_name: String::new(),
            };
            let note = save_provider(&draft).unwrap();
            assert!(note.contains("gateway"), "{note}");

            let cfg = config::load_models_config().unwrap();
            let provider = &cfg.providers["gateway"];
            assert_eq!(provider.api.as_deref(), Some("openai-completions"));
            assert_eq!(
                provider.base_url.as_deref(),
                Some("https://api.example.com/v1")
            );
            assert_eq!(provider.api_key.as_deref(), Some("$GATEWAY_KEY"));
            assert_eq!(provider.models.len(), 1);
            assert_eq!(provider.models[0].id, "gpt-x");
            // A name equal to the id (or blank) stays implicit.
            assert_eq!(provider.models[0].name, None);
            assert_eq!(provider.name, None);

            // Duplicate provider id is refused.
            assert!(save_provider(&draft).is_err());
        });
    }

    #[test]
    fn edit_touches_only_owned_fields() {
        with_isolated_config_dir(|| {
            write_models_json(
                r#"{"providers":{"Kimi":{"name":"K","baseUrl":"https://old.example.com","apiKey":"sk-old","api":"anthropic-messages","headers":{"x-a":"1"},"authHeader":true,"models":[{"id":"k1"},{"id":"k2"}]}}}"#,
            );
            let draft = ProviderDraft {
                mode: DraftMode::Edit,
                id: "Kimi".into(),
                name: "Kimi CN".into(),
                api: "openai-completions".into(),
                base_url: "https://new.example.com/v1".into(),
                api_key: "$KIMI_API_KEY".into(),
                ..Default::default()
            };
            let note = save_provider(&draft).unwrap();
            assert!(note.contains("OpenAI Completions"), "{note}");
            assert!(note.contains("2 model(s) kept"), "{note}");

            let cfg = config::load_models_config().unwrap();
            let provider = &cfg.providers["Kimi"];
            assert_eq!(provider.name.as_deref(), Some("Kimi CN"));
            assert_eq!(provider.api.as_deref(), Some("openai-completions"));
            assert_eq!(
                provider.base_url.as_deref(),
                Some("https://new.example.com/v1")
            );
            assert_eq!(provider.api_key.as_deref(), Some("$KIMI_API_KEY"));
            assert_eq!(provider.auth_header, Some(true));
            assert_eq!(provider.headers.as_ref().unwrap()["x-a"], "1");
            assert_eq!(provider.models.len(), 2);

            // Editing a provider that is gone is an error.
            let missing = ProviderDraft {
                mode: DraftMode::Edit,
                id: "nope".into(),
                ..Default::default()
            };
            assert!(save_provider(&missing).is_err());
        });
    }

    #[test]
    fn adds_and_edits_models() {
        with_isolated_config_dir(|| {
            write_models_json(
                r#"{"providers":{"p1":{"api":"openai-completions","baseUrl":"https://a.example.com/v1","apiKey":"k","models":[{"id":"m1","name":"One","baseUrl":"https://alt.example.com","headers":{"x-model":"y"},"compat":{"foo":1}}]}}}"#,
            );
            let add = ModelDraft {
                mode: DraftMode::Add,
                provider_id: "p1".into(),
                id: "m2".into(),
                name: "Two".into(),
                context_window: Some(128000),
                max_tokens: Some(8192),
                reasoning: true,
                images: true,
                ..Default::default()
            };
            let note = save_model(&add).unwrap();
            assert!(note.contains("m2"), "{note}");
            let cfg = config::load_models_config().unwrap();
            let models = &cfg.providers["p1"].models;
            assert_eq!(models.len(), 2);
            let m2 = models.iter().find(|m| m.id == "m2").unwrap();
            assert_eq!(m2.context_window, Some(128000));
            assert_eq!(m2.max_tokens, Some(8192));
            assert_eq!(m2.reasoning, Some(true));
            assert_eq!(
                m2.input.as_deref(),
                Some(&["text".to_string(), "image".to_string()][..])
            );
            // Duplicate model id is refused.
            assert!(save_model(&add).is_err());

            // Edit rewrites in place and preserves the fields it does not own.
            let edit = ModelDraft {
                mode: DraftMode::Edit,
                provider_id: "p1".into(),
                original_id: Some("m1".into()),
                id: "m1-renamed".into(),
                name: String::new(),
                context_window: None,
                max_tokens: Some(4096),
                reasoning: true,
                images: false,
                ..Default::default()
            };
            save_model(&edit).unwrap();
            let cfg = config::load_models_config().unwrap();
            let models = &cfg.providers["p1"].models;
            assert_eq!(models.len(), 2);
            let m1 = models.iter().find(|m| m.id == "m1-renamed").unwrap();
            assert_eq!(m1.name, None);
            assert_eq!(m1.max_tokens, Some(4096));
            assert_eq!(m1.base_url.as_deref(), Some("https://alt.example.com"));
            assert_eq!(m1.headers.as_ref().unwrap()["x-model"], "y");
            assert_eq!(m1.compat, Some(serde_json::json!({"foo": 1})));
        });
    }

    #[test]
    fn removes_one_model_and_one_provider() {
        with_isolated_config_dir(|| {
            write_models_json(
                r#"{"providers":{"mine":{"api":"openai-completions","models":[{"id":"m1"},{"id":"m2"}]},"other":{"api":"openai-completions","models":[{"id":"m1"}]}}}"#,
            );
            remove_model("mine", "m1").unwrap();
            let cfg = config::load_models_config().unwrap();
            let remaining: Vec<&str> = cfg.providers["mine"]
                .models
                .iter()
                .map(|m| m.id.as_str())
                .collect();
            assert_eq!(remaining, vec!["m2"]);
            // Another provider's same-named model is untouched.
            assert_eq!(cfg.providers["other"].models.len(), 1);
            assert!(remove_model("mine", "nope").is_err());
            assert!(remove_model("nope", "m1").is_err());

            remove_provider("mine").unwrap();
            assert!(!provider_exists("mine"));
            assert!(provider_exists("other"));
            assert!(remove_provider("mine").is_err());
        });
    }

    #[test]
    fn count_parsing_and_protocol_labels() {
        assert_eq!(parse_optional_count("", "ctx").unwrap(), None);
        assert_eq!(
            parse_optional_count(" 128000 ", "ctx").unwrap(),
            Some(128000)
        );
        assert!(parse_optional_count("0", "ctx").is_err());
        assert!(parse_optional_count("-5", "ctx").is_err());
        assert!(parse_optional_count("abc", "ctx").is_err());

        assert_eq!(protocol_label(Some("openai-responses")), "OpenAI Responses");
        assert_eq!(
            protocol_label(Some("anthropic-messages")),
            "Anthropic Messages"
        );
        assert_eq!(protocol_label(None), "Anthropic Messages");
        assert_eq!(protocol_label(Some("who-knows")), "custom");
    }
}
