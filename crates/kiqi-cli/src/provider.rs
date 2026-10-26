//! Provider + model resolution. Mirrors the provider/model selection and
//! request-auth portions of upstream's model runtime.
//!
//! The built-in lane uses Anthropic's protocol, while `models.json` may add
//! named Anthropic-compatible, OpenAI Completions, and OpenAI Responses
//! providers. OAuth/Copilot remains deferred. This module resolves one provider
//! identity and its isolated model catalog for each run.
//!
//! # Auth resolution and isolation
//!
//! Credentials are resolved independently for every provider id:
//!
//! 1. `--api-key` overrides only the finally selected provider.
//! 2. `auth.json[provider-id]` wins over that provider's configured key.
//! 3. `models.json.providers[provider-id].apiKey` (plus `authHeader`) applies
//!    only to models owned by that provider.
//! 4. Provider-specific environment variables are fallbacks. The global
//!    `ANTHROPIC_*` and `OPENAI_API_KEY` variables belong only to the built-in
//!    `anthropic` and `openai` identities respectively.
//!
//! A credential never makes another provider's model available and is never
//! passed to another provider's runtime, even when endpoints or model ids are
//! identical.
//!
//! # Endpoint + catalog
//!
//! - `--base-url` overrides the selected `model.base_url` at resolve time;
//!   `ANTHROPIC_BASE_URL` is the fallback for the built-in `anthropic` identity
//!   only (the request URL is built from it per-request in kiqi-ai).
//! - `~/.ki/models.json` (if present) merges/overrides the built-in catalog:
//!   each `anthropic-messages` provider contributes its models, with
//!   provider-level `base_url`/`headers`/`authHeader` folded in. The models.json
//!   provider id (e.g. `gateway`) remains the routing and credential identity,
//!   so providers may safely share a protocol, endpoint, or model id.
//!
//! # Model pattern precedence (mirrors `resolveCliModel`)
//!
//! 1. `--model` may carry `provider/id[:thinking]`. The prefix is interpreted
//!    as a provider only when it names a known built-in or `models.json`
//!    provider. Otherwise the slash remains part of the raw model id (for
//!    example `meta-llama/llama-*`). If both interpretations exist, the known
//!    provider wins while authenticated; an unauthenticated inferred provider
//!    yields to one uniquely authenticated raw-id match, as in upstream.
//! 2. Otherwise treat `--model` as `id[:thinking]`: if a trailing `:level` is a
//!    valid thinking level, strip it and apply it (overriding `--thinking`);
//!    else the whole string is the id.
//! 3. `--provider` must name a built-in provider or a configured models.json
//!    provider and restricts selection to that identity.
//! 4. The model id is matched **exactly, case-insensitively** against the
//!    catalog. The TS resolver additionally does fuzzy/partial matching; v1
//!    keeps it exact to avoid surprising model picks (partial match is a common
//!    source of "got the wrong model" bugs — documented as a divergence in
//!    the initial port notes (retired)).
//! 5. No `--model` ⇒ [`pick_default_model`]:
//!    (a) scan upstream's `defaultModelPerProvider` entries in their declared
//!    order and take the first authenticated match; otherwise (b) take the
//!    **first authenticated model** in the catalog — mirroring the TS
//!    `findInitialModel` fallback over `availableModels`. This lets a
//!    `models.json`-only gateway config "just work": the built-in Anthropic
//!    models carry no auth, so the gateway model (the only authenticated one)
//!    is picked. The all-builtin/no-custom-code default (`ANTHROPIC_API_KEY`
//!    path) selects `claude-opus-4-8`. Last resort falls back to
//!    [`DEFAULT_MODEL_ID`] (or the catalog head) — unreachable in practice
//!    because the auth gate refuses an unauthed catalog earlier.
//!
//! [`AnthropicProvider`]: kiqi_ai::providers::anthropic::AnthropicProvider

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kiqi_ai::providers::anthropic::models::anthropic_models;
use kiqi_ai::providers::anthropic::AnthropicProvider;
use kiqi_ai::providers::openai_completions::OpenAiCompletionsProvider;
use kiqi_ai::providers::openai_responses::openai_responses_models;
use kiqi_ai::providers::openai_responses::OpenAiResponsesProvider;
use kiqi_ai::{
    AssistantMessageEventStream, Context, Model, Provider, SimpleStreamOptions, ThinkingLevel,
};

use crate::args::parse_thinking_level;
use crate::config::{self, Credential, DEFAULT_PROVIDER_ID};
use crate::settings;
use crate::wasm_provider::WasmProvider;

/// The default Anthropic model when `--model` is absent. Kept in sync with the
/// current upstream `defaultModelPerProvider.anthropic` entry.
pub const DEFAULT_MODEL_ID: &str = "claude-opus-4-8";

/// Provider ids (and protocol aliases) that belong to the built-in tier. A
/// extension may never use one of these, so a grant cannot shadow them.
const BUILTIN_PROVIDER_IDS: &[&str] = &[
    "anthropic",
    "openai",
    "openai-completions",
    "openai-responses",
];

/// upstream checks these provider defaults in declaration order before it
/// falls back to `availableModels[0]`. Configured providers using one of ki's
/// supported wire protocols participate too, even when they are not built in.
const DEFAULT_MODELS_PER_PROVIDER: &[(&str, &str)] = &[
    ("amazon-bedrock", "us.anthropic.claude-opus-4-6-v1"),
    ("ant-ling", "Ring-2.6-1T"),
    ("anthropic", DEFAULT_MODEL_ID),
    ("openai", "gpt-5.5"),
    ("azure-openai-responses", "gpt-5.4"),
    ("openai-codex", "gpt-5.5"),
    ("radius", "auto"),
    ("nvidia", "nvidia/nemotron-3-super-120b-a12b"),
    ("deepseek", "deepseek-v4-pro"),
    ("google", "gemini-3.1-pro-preview"),
    ("google-vertex", "gemini-3.1-pro-preview"),
    ("github-copilot", "gpt-5.4"),
    ("openrouter", "moonshotai/kimi-k2.6"),
    ("vercel-ai-gateway", "zai/glm-5.1"),
    ("xai", "grok-4.6"),
    ("groq", "openai/gpt-oss-120b"),
    ("cerebras", "gpt-oss-120b"),
    ("zai", "glm-5.3"),
    ("zai-coding-cn", "glm-5.3"),
    ("mistral", "devstral-medium-latest"),
    ("minimax", "MiniMax-M2.7"),
    ("minimax-cn", "MiniMax-M2.7"),
    ("moonshotai", "kimi-k2.6"),
    ("moonshotai-cn", "kimi-k2.6"),
    ("huggingface", "moonshotai/Kimi-K2.6"),
    ("fireworks", "accounts/fireworks/models/kimi-k2p6"),
    ("together", "moonshotai/Kimi-K2.6"),
    ("baseten", "zai-org/GLM-5.2"),
    ("opencode", "kimi-k2.6"),
    ("opencode-go", "kimi-k2.6"),
    ("kimi-coding", "kimi-for-coding"),
    ("cloudflare-workers-ai", "@cf/moonshotai/kimi-k2.6"),
    (
        "cloudflare-ai-gateway",
        "workers-ai/@cf/moonshotai/kimi-k2.6",
    ),
    ("qwen-token-plan", "qwen3.7-max"),
    ("qwen-token-plan-cn", "qwen3.7-max"),
    ("qwen-token-plan-individual", "qwen3.8-max"),
    ("xiaomi", "mimo-v2.5-pro"),
    ("xiaomi-token-plan-cn", "mimo-v2.5-pro"),
    ("xiaomi-token-plan-ams", "mimo-v2.5-pro"),
    ("xiaomi-token-plan-sgp", "mimo-v2.5-pro"),
];

/// The default thinking level when neither `--thinking` nor a `:level` suffix
/// is present. Mirrors the TS `DEFAULT_THINKING_LEVEL` (`"medium"`, clamped to
/// model capabilities by the harness's provider build_params).
pub const DEFAULT_THINKING_LEVEL: ThinkingLevel = ThinkingLevel::Medium;

/// The resolved run configuration: the provider handle, the chosen model, and
/// the effective thinking level (after `--thinking` / `:level` / model-clamp).
#[derive(Clone)]
pub struct ResolvedModel {
    /// The selected provider. Cheap to clone through the trait-object `Arc`.
    pub provider: Arc<dyn Provider>,
    /// The chosen model from the catalog.
    pub model: Model,
    /// Effective thinking level (the requested level, before model-clamp — the
    /// harness/provider clamps to the model's supported set).
    pub thinking_level: ThinkingLevel,
    /// Whether the selected runtime carries a provider-default key. When
    /// false, authentication is owned by the selected model's headers.
    ///
    /// Kept so [`available_catalog`] can reproduce the auth-filtered snapshot
    /// (pi `getAvailableSnapshot`: `available = all.filter(m =>
    /// configuredProviders.has(m.provider))`) and surface only models that
    /// won't fail at request time with "No API key for provider".
    pub has_provider_key: bool,
    /// Provider ids owned by an installed extension whose `provider` grant the
    /// user recorded. The grant is the only authority: a model under one of
    /// these ids is selectable and visible, and no built-in credential is ever
    /// needed (or consulted) for it.
    pub granted_extension_providers: BTreeSet<String>,
    /// Saved theme name from `~/.ki/agent/settings.json`, if any. Best-effort:
    /// the TUI applies it at startup when it matches a known preset
    /// (dark/light/monochrome); otherwise ignored.
    pub theme: Option<String>,
    /// Every authenticated provider runtime, with the selected provider first.
    /// The harness resolves a provider lazily by `model.provider`, so this lets
    /// `/model` hot-switch across providers without a restart.
    pub providers: Vec<Arc<dyn Provider>>,
}

impl std::fmt::Debug for ResolvedModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedModel")
            .field("provider", &self.provider.id())
            .field("model", &self.model.id)
            .field("thinking_level", &self.thinking_level)
            .field("has_provider_key", &self.has_provider_key)
            .field(
                "granted_extension_providers",
                &self.granted_extension_providers,
            )
            .field("theme", &self.theme)
            .finish()
    }
}

/// Bind the Anthropic wire implementation to the provider id declared in
/// models.json. upstream keeps providers isolated by id even when they share
/// the same protocol and model ids; the wrapper preserves that routing identity
/// without duplicating the protocol implementation.
struct NamedAnthropicProvider {
    id: String,
    inner: AnthropicProvider,
}

#[async_trait::async_trait]
impl Provider for NamedAnthropicProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn models(&self) -> &[Model] {
        self.inner.models()
    }

    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.inner.stream_simple(model, ctx, opts).await
    }
}

/// The env var consulted for the API key. Mirrors TS `ANTHROPIC_API_KEY`.
pub const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// The env var consulted for a bearer token (routed as
/// `Authorization: Bearer`). Mirrors TS `ANTHROPIC_AUTH_TOKEN` — used by
/// third-party Anthropic-compatible gateways (one-api/new-api/claude-code-router
/// and private reverse proxies) that authenticate via `Authorization` rather
/// than `x-api-key`.
pub const ANTHROPIC_AUTH_TOKEN_ENV: &str = "ANTHROPIC_AUTH_TOKEN";

/// The env var that overrides the Anthropic endpoint base URL. Mirrors TS
/// `ANTHROPIC_BASE_URL` — point this at a gateway/proxy that speaks the
/// `/v1/messages` protocol.
pub const ANTHROPIC_BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";

/// Standard OpenAI API-key environment variable used by the
/// `openai-completions` provider.
pub const OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";

/// Hint text surfaced when no credential source is available. Lists every
/// accepted source so the user can pick the one that fits their setup.
pub const NO_API_KEY_HINT: &str =
    "models.json apiKey, OPENAI_API_KEY / ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN env, --api-key, or `ki auth login`";

/// Provider/model identity used when the interactive TUI starts with nothing
/// configured. Opening the TUI instead of exiting lets the user run
/// `ki configure` in another terminal (or `/login`) — the startup gate only
/// made sense before the configuration could be reached at all.
pub const UNCONFIGURED_PROVIDER_ID: &str = "unconfigured";
pub const UNCONFIGURED_MODEL_ID: &str = "unconfigured";

/// A provider that serves no models and answers every request with the
/// "nothing configured yet" guidance. Only ever installed for the
/// no-credentials interactive startup; it must never be reached with a real
/// model, and it never touches the network.
#[derive(Debug)]
struct UnconfiguredProvider;

#[async_trait::async_trait]
impl Provider for UnconfiguredProvider {
    fn id(&self) -> &str {
        UNCONFIGURED_PROVIDER_ID
    }

    fn models(&self) -> &[Model] {
        &[]
    }

    async fn stream_simple(
        &self,
        model: &Model,
        _ctx: &Context,
        _opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        use kiqi_ai::event_stream::create_assistant_message_event_stream;
        use kiqi_ai::{AssistantMessage, AssistantMessageEvent, ErrorReason, StopReason};

        let (mut producer, stream) = create_assistant_message_event_stream();
        let message = AssistantMessage::terminal(
            model.api.clone(),
            UNCONFIGURED_PROVIDER_ID,
            model.id.clone(),
            StopReason::Error,
            format!(
                "No provider is configured yet, so there is nothing to send this to.\n\n\
                 Run `ki configure` in another terminal to add a provider (or \
                 `ki auth login` for an API-key/OAuth credential), then restart ki.\n\n{}",
                crate::auth_guidance::provider_login_help()
            ),
            now_millis(),
        );
        producer.push(AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error: message,
        });
        stream
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// The [`ResolvedModel`] handed to the harness when the interactive TUI is
/// opened with no usable provider. Any send attempt is answered by
/// [`UnconfiguredProvider`] with the configuration guidance, so the TUI can be
/// used to fix the very thing that used to abort startup.
pub fn unconfigured_resolved(theme: Option<String>) -> ResolvedModel {
    let mut model = Model::new(
        UNCONFIGURED_MODEL_ID,
        "No model configured",
        kiqi_ai::Api::AnthropicMessages,
        UNCONFIGURED_PROVIDER_ID,
        "http://unconfigured.invalid",
    );
    model.context_window = 0;
    model.max_tokens = 0;
    ResolvedModel {
        provider: Arc::new(UnconfiguredProvider),
        model,
        thinking_level: ThinkingLevel::Off,
        has_provider_key: false,
        granted_extension_providers: BTreeSet::new(),
        theme,
        providers: vec![Arc::new(UnconfiguredProvider)],
    }
}

/// Whether this resolved model is the "nothing configured" placeholder.
pub fn is_unconfigured(model: &Model) -> bool {
    model.id == UNCONFIGURED_MODEL_ID && model.provider == UNCONFIGURED_PROVIDER_ID
}

/// A resolution error. The TS resolver returns `{ error, warning }`; v1 folds
/// both into a single enum since the CLI treats them the same (print + non-zero
/// exit) except `NoApiKey`, which prints guidance then exits.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("Unknown provider \"{0}\". Supported: anthropic, openai-completions, openai-responses, or a models.json provider id")]
    UnknownProvider(String),
    #[error(
        "Provider \"{requested}\" is ambiguous: {matches}. Use the exact provider id to select one."
    )]
    AmbiguousProvider { requested: String, matches: String },
    #[error("No model matches \"{pattern}\". Available: {available}")]
    NoMatch { pattern: String, available: String },
    #[error(
        "Model \"{pattern}\" is ambiguous across providers: {matches}. {auth_hint} Use --provider or provider/model."
    )]
    AmbiguousModel {
        pattern: String,
        matches: String,
        auth_hint: &'static str,
    },
    #[error("Invalid thinking level \"{0}\" in model pattern. Valid: {1}")]
    InvalidThinkingLevel(String, String),
    #[error("No API key. Set one of: {hint}")]
    NoApiKey { hint: &'static str },
    #[error(
        "Provider id \"{id}\" from an installed extension collides with {existing}. \
         An extension may not shadow a built-in provider, a models.json provider, or another \
         extension; rename or uninstall the extension."
    )]
    ProviderIdCollision { id: String, existing: String },
    #[error("Could not read config: {0}")]
    Config(#[from] config::ConfigError),
}

/// Resolve the provider + model + thinking level from the CLI flags + env +
/// `~/.ki/` config.
///
/// `cli_provider` is the `--provider` value (optional). `cli_model` is the
/// `--model` value (optional; may be `provider/id[:thinking]` or `id[:thinking]`).
/// `cli_thinking` is the `--thinking` value (optional). `cli_api_key` is the
/// `--api-key` value (optional; highest-priority `x-api-key` source).
/// `cli_base_url` is the `--base-url` value (optional; overrides
/// `ANTHROPIC_BASE_URL` + each model's `base_url`).
/// Build the provider HTTP client honoring system proxy settings and the
/// `httpIdleTimeout` setting. Uses the model's own `base_url` when present (so
/// `NO_PROXY` matches the real host), else `default_base_url`.
fn provider_http_client(
    models: &[Model],
    default_base_url: &str,
    settings: &settings::Settings,
) -> reqwest::Client {
    let base_url = models
        .iter()
        .map(|m| m.base_url.as_str())
        .find(|u| !u.is_empty())
        .unwrap_or(default_base_url);
    kiqi_ai::http::build_client(base_url, settings.http_idle_timeout_ms(), None)
        .unwrap_or_else(|_| reqwest::Client::new())
}

pub fn resolve(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    cli_thinking: Option<ThinkingLevel>,
    cli_api_key: Option<&str>,
    cli_base_url: Option<&str>,
) -> Result<ResolvedModel, ResolveError> {
    let settings = settings::load_settings().unwrap_or_default();
    resolve_with_settings(
        cli_provider,
        cli_model,
        cli_thinking,
        cli_api_key,
        cli_base_url,
        settings,
        None,
    )
}

/// Resolve using upstream's global -> trusted-project settings precedence.
pub fn resolve_for_cwd(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    cli_thinking: Option<ThinkingLevel>,
    cli_api_key: Option<&str>,
    cli_base_url: Option<&str>,
    cwd: &std::path::Path,
    project_trusted: bool,
) -> Result<ResolvedModel, ResolveError> {
    let settings = settings::load_effective_model_settings(cwd, project_trusted)?;
    resolve_with_settings(
        cli_provider,
        cli_model,
        cli_thinking,
        cli_api_key,
        cli_base_url,
        settings,
        None,
    )
}

fn resolve_with_settings(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    cli_thinking: Option<ThinkingLevel>,
    cli_api_key: Option<&str>,
    cli_base_url: Option<&str>,
    settings: settings::Settings,
    agent_root: Option<&Path>,
) -> Result<ResolvedModel, ResolveError> {
    // Load models/auth once, then resolve Anthropic-wire credentials per
    // provider id. A key belonging to `anthropic` must never make an unrelated
    // gateway available or become that gateway's provider default.
    let models_cfg = config::load_models_config()?;
    // ---- Granted extension providers: read before selection so their ids are
    // valid `--provider` / `provider/id` spellings and their models are authed.
    let granted_root: Option<PathBuf> = agent_root
        .map(Path::to_path_buf)
        .or_else(kiqi_host::grants::agent_dir);
    let granted_extensions = match &granted_root {
        Some(root) => granted_extension_models(root),
        None => Vec::new(),
    };
    let granted_extension_providers: BTreeSet<String> = granted_extensions
        .iter()
        .map(|extension| extension.id.clone())
        .collect();
    let canonical_cli_provider = cli_provider
        .map(|requested| {
            canonicalize_cli_provider(requested, &models_cfg, &granted_extension_providers)
        })
        .transpose()?;
    let cli_provider = canonical_cli_provider.as_deref();
    let auth_store = config::read_auth()?;
    let anthropic_credentials = resolve_anthropic_credentials(&models_cfg, &auth_store);
    let openai_credentials = resolve_openai_credentials(&models_cfg, &auth_store);
    // The CLI override is intentionally not inserted into either provider map:
    // it is applied only after the final model/provider identity is selected.
    let cli_api_key = cli_api_key
        .filter(|key| !key.is_empty())
        .map(str::to_string);

    // ---- Endpoint override (--base-url → ANTHROPIC_BASE_URL) ----
    let cli_base_url_override = cli_base_url.map(str::to_string);
    let anthropic_base_url_override = std::env::var(ANTHROPIC_BASE_URL_ENV)
        .ok()
        .filter(|value| !value.is_empty());

    // ---- Catalog: built-in, then ~/.ki/models.json, then granted extension
    // providers (fixed order; an id collision is a hard refusal) ----
    let mut catalog = catalog_with_granted_extensions(&models_cfg, &granted_extensions)?;

    // Apply an explicit CLI endpoint override to the selected catalog lane.
    // The ambient Anthropic override belongs only to the built-in `anthropic`
    // identity; applying it to custom Anthropic-wire providers would redirect
    // their provider-specific credentials to an unrelated endpoint.
    for model in &mut catalog {
        if let Some(base) = &cli_base_url_override {
            model.base_url = base.clone();
        } else if matches!(model.api, kiqi_ai::Api::AnthropicMessages)
            && model.provider == DEFAULT_PROVIDER_ID
        {
            if let Some(base) = &anthropic_base_url_override {
                model.base_url = base.clone();
            }
        }
    }

    // Snapshot the full (un-retained) catalog so auxiliary provider runtimes
    // for `/model` hot-switching can be built after the selected one.
    let full_catalog = catalog.clone();

    if let Some(requested) = cli_provider {
        catalog.retain(|model| provider_matches(model, requested, &models_cfg));
    }

    let available = catalog
        .iter()
        .map(|m| m.id.clone())
        .collect::<Vec<_>>()
        .join(", ");
    if catalog.is_empty() {
        return Err(ResolveError::NoMatch {
            pattern: cli_provider.unwrap_or("default").to_string(),
            available,
        });
    }

    // ---- Model selection ----
    // With `--model`: parse the pattern (`provider/id[:thinking]`), match it
    // exactly against the catalog (TS fuzzy/partial match is a deliberate v1
    // omission — see module docs §5). Without `--model`: pi `findInitialModel`
    // precedence — (3) the saved default from settings (when present + authed),
    // then (4) `pick_default_model` (built-in default if authed, else first
    // authed). The saved default mirrors `findInitialModel` step 3 and lets a
    // copied pi `settings.json`'s `defaultModel` come alive on launch.
    let (mut model, thinking_level) = match cli_model {
        Some(raw) => {
            let parsed_pattern = split_model_pattern(
                raw,
                cli_provider,
                &catalog,
                &models_cfg,
                &granted_extension_providers,
            )?;
            if let Some(provider) = parsed_pattern.provider.as_deref() {
                if !provider_is_known(provider, &models_cfg, &granted_extension_providers) {
                    return Err(ResolveError::UnknownProvider(provider.to_string()));
                }
            }
            let mut pattern_thinking = parsed_pattern.thinking;
            let mut selected = find_cli_model(
                &parsed_pattern.model_id,
                parsed_pattern.provider.as_deref(),
                &catalog,
                &models_cfg,
                &anthropic_credentials,
                &openai_credentials,
                &granted_extension_providers,
            )?;

            if parsed_pattern.inferred_provider {
                if let Some(inferred) = selected.as_ref() {
                    if !model_is_authed_for_resolution(
                        inferred,
                        &anthropic_credentials,
                        &openai_credentials,
                        false,
                        &granted_extension_providers,
                    ) {
                        let authenticated_raw_matches = |candidate: &str| {
                            catalog
                                .iter()
                                .filter(|model| {
                                    model.id.eq_ignore_ascii_case(candidate)
                                        && (model.provider != inferred.provider
                                            || model.id != inferred.id)
                                        && model_is_authed_for_resolution(
                                            model,
                                            &anthropic_credentials,
                                            &openai_credentials,
                                            false,
                                            &granted_extension_providers,
                                        )
                                })
                                .collect::<Vec<_>>()
                        };
                        let mut raw_matches =
                            authenticated_raw_matches(&parsed_pattern.raw_model_id);
                        let mut matched_complete_raw_id = true;
                        if raw_matches.is_empty() && parsed_pattern.thinking.is_some() {
                            if let Some((raw_without_thinking, _)) =
                                parsed_pattern.raw_model_id.rsplit_once(':')
                            {
                                raw_matches = authenticated_raw_matches(raw_without_thinking);
                                matched_complete_raw_id = false;
                            }
                        }
                        if let [raw_match] = raw_matches.as_slice() {
                            selected = Some((*raw_match).clone());
                            if matched_complete_raw_id {
                                pattern_thinking = None;
                            }
                        }
                    }
                } else {
                    selected = find_cli_model(
                        &parsed_pattern.raw_model_id,
                        None,
                        &catalog,
                        &models_cfg,
                        &anthropic_credentials,
                        &openai_credentials,
                        &granted_extension_providers,
                    )?;
                    if selected.is_some() {
                        pattern_thinking = None;
                    } else if parsed_pattern.thinking.is_some() {
                        if let Some((raw_without_thinking, _)) =
                            parsed_pattern.raw_model_id.rsplit_once(':')
                        {
                            selected = find_cli_model(
                                raw_without_thinking,
                                None,
                                &catalog,
                                &models_cfg,
                                &anthropic_credentials,
                                &openai_credentials,
                                &granted_extension_providers,
                            )?;
                        }
                    }
                }
            }

            let model = match selected {
                Some(m) => m,
                None => {
                    return Err(ResolveError::NoMatch {
                        pattern: parsed_pattern.model_id,
                        available,
                    });
                }
            };
            // `--thinking` wins over a parsed `:level` suffix; an exact model
            // id containing that suffix does not implicitly set thinking.
            let thinking_level = cli_thinking
                .or(pattern_thinking)
                .unwrap_or(DEFAULT_THINKING_LEVEL);
            (model, thinking_level)
        }
        None => {
            // `--thinking` > settings `defaultThinkingLevel` > built-in default.
            // The settings level is honored only when its model is also the
            // saved default (matches pi, which applies `defaultThinkingLevel`
            // inside the step-3 branch). For the fallback default, keep
            // `DEFAULT_THINKING_LEVEL`.
            let settings_thinking = settings
                .default_thinking_level
                .as_deref()
                .and_then(parse_thinking_level);

            // (3) Saved default from settings, when the provider is anthropic
            // (or absent — v1 is anthropic-only) OR names a configured
            // models.json gateway (config-namespacing: the saved
            // `defaultProvider` id matches a `~/.ki/models.json` provider
            // key), and the saved model is authed. Without the gateway arm a
            // copied pi settings.json (`defaultProvider:
            // "cc-switch-deep-seek-copy-2"`) is ignored and the default falls
            // to first-authed — which, once a second gateway is enabled, may
            // NOT be the user's saved choice (BTreeMap provider order).
            let saved_provider = settings.default_provider.as_deref().filter(|provider| {
                provider_is_known(provider, &models_cfg, &granted_extension_providers)
            });
            let saved = settings.default_model.as_deref().and_then(|id| {
                if settings.default_provider.is_some() && saved_provider.is_none() {
                    return None;
                }
                find_model(id, saved_provider, &catalog, &models_cfg).filter(|m| {
                    model_is_authed_for_resolution(
                        m,
                        &anthropic_credentials,
                        &openai_credentials,
                        cli_api_key.is_some(),
                        &granted_extension_providers,
                    )
                })
            });
            if let Some(model) = saved {
                let thinking_level = cli_thinking
                    .or(settings_thinking)
                    .unwrap_or(DEFAULT_THINKING_LEVEL);
                (model, thinking_level)
            } else {
                // (4) Fallback: built-in default if authed, else first authed.
                let thinking_level = cli_thinking.unwrap_or(DEFAULT_THINKING_LEVEL);
                let model = pick_default_model(
                    &catalog,
                    &models_cfg,
                    &anthropic_credentials,
                    &openai_credentials,
                    cli_api_key.is_some(),
                    &granted_extension_providers,
                );
                (model, thinking_level)
            }
        }
    };

    // ---- Provider build ----
    let selected_api = model.api.clone();
    let selected_provider = model.provider.clone();
    // A granted extension provider never gets a built-in credential folded into
    // its catalog entry: the host-held credential is injected by `provider-http`
    // for a matching request, and nothing else may carry it.
    let is_granted_extension = granted_extension_providers.contains(&selected_provider);
    let selected_anthropic_credential = if is_granted_extension {
        None
    } else if matches!(selected_api, kiqi_ai::Api::AnthropicMessages) {
        let auth_header = models_cfg
            .providers
            .get(&selected_provider)
            .and_then(|provider| provider.auth_header)
            .unwrap_or(false);
        credential_for_selected_anthropic_provider(
            cli_api_key.as_deref(),
            &anthropic_credentials,
            &selected_provider,
            auth_header,
        )
    } else {
        None
    };
    let selected_openai_key = if is_granted_extension {
        None
    } else if matches!(
        selected_api,
        kiqi_ai::Api::OpenaiCompletions | kiqi_ai::Api::OpenaiResponses
    ) {
        cli_api_key
            .clone()
            .or_else(|| openai_credential_for(&openai_credentials, &selected_provider).cloned())
    } else {
        None
    };
    let selected_is_authed = model_has_header_auth(&model)
        || granted_extension_providers.contains(&selected_provider)
        || match selected_api {
            kiqi_ai::Api::AnthropicMessages => selected_anthropic_credential.is_some(),
            kiqi_ai::Api::OpenaiCompletions | kiqi_ai::Api::OpenaiResponses => {
                selected_openai_key.is_some()
            }
            _ => false,
        };
    if !selected_is_authed {
        return Err(ResolveError::NoApiKey {
            hint: NO_API_KEY_HINT,
        });
    }

    if let Some(AnthropicCredential::Headers(headers)) = &selected_anthropic_credential {
        merge_auth_headers(&mut model, headers);
    }
    if let Some(key) = &selected_openai_key {
        merge_auth_headers(
            &mut model,
            &BTreeMap::from([("authorization".into(), format!("Bearer {key}"))]),
        );
    }
    // A granted extension provider never goes through `build_runtime`: that would
    // build a keyless direct HTTP client for a model whose requests belong on
    // the grant-owned `provider-http` channel. `WasmProvider` creates the
    // dedicated wasm host lazily, on the first request.
    let (provider, has_provider_key) = if is_granted_extension {
        let granted = granted_extensions
            .iter()
            .find(|extension| extension.id == selected_provider)
            .expect("a selected granted provider comes from the grant scan");
        (
            Arc::new(WasmProvider::new(
                granted.id.clone(),
                full_catalog
                    .iter()
                    .filter(|candidate| candidate.provider == selected_provider)
                    .cloned()
                    .collect(),
                granted.wasm.clone(),
                granted_root.clone(),
            )) as Arc<dyn Provider>,
            false,
        )
    } else {
        build_runtime(
            &selected_provider,
            &selected_api,
            &full_catalog,
            selected_anthropic_credential.as_ref(),
            selected_openai_key.as_deref(),
            &settings,
        )
    };

    // Build runtimes for every OTHER authenticated provider so the harness can
    // hot-switch providers by `model.provider` (its lazy resolve matches by
    // id). The selected provider is already built above.
    let mut providers: Vec<Arc<dyn Provider>> = vec![provider.clone()];
    {
        let mut seen: Vec<(String, kiqi_ai::Api)> =
            vec![(selected_provider.clone(), selected_api.clone())];
        for candidate in &full_catalog {
            // Extension providers are built below from their grant, never from a
            // built-in credential.
            if granted_extension_providers.contains(&candidate.provider) {
                continue;
            }
            let key = (candidate.provider.clone(), candidate.api.clone());
            if seen.contains(&key) {
                continue;
            }
            seen.push(key.clone());
            let (anth, openai_key) = match &candidate.api {
                kiqi_ai::Api::AnthropicMessages => {
                    let auth_header = models_cfg
                        .providers
                        .get(&candidate.provider)
                        .and_then(|p| p.auth_header)
                        .unwrap_or(false);
                    (
                        credential_for_selected_anthropic_provider(
                            None,
                            &anthropic_credentials,
                            &candidate.provider,
                            auth_header,
                        ),
                        None,
                    )
                }
                kiqi_ai::Api::OpenaiCompletions | kiqi_ai::Api::OpenaiResponses => (
                    None,
                    openai_credential_for(&openai_credentials, &candidate.provider).cloned(),
                ),
                _ => (None, None),
            };
            let authed = model_has_header_auth(candidate)
                || match candidate.api {
                    kiqi_ai::Api::AnthropicMessages => anth.is_some(),
                    kiqi_ai::Api::OpenaiCompletions | kiqi_ai::Api::OpenaiResponses => {
                        openai_key.is_some()
                    }
                    _ => false,
                };
            if !authed {
                continue;
            }
            let (runtime, _) = build_runtime(
                &candidate.provider,
                &candidate.api,
                &full_catalog,
                anth.as_ref(),
                openai_key.as_deref(),
                &settings,
            );
            providers.push(runtime);
        }
    }
    // One runtime handle per granted extension provider, so
    // `authenticated_catalog` (and therefore `/model`) sees them. Each adapter
    // is independent and compiles nothing until its first request.
    for extension in &granted_extensions {
        if providers.iter().any(|runtime| runtime.id() == extension.id) {
            continue;
        }
        let models: Vec<Model> = full_catalog
            .iter()
            .filter(|candidate| candidate.provider == extension.id)
            .cloned()
            .collect();
        providers.push(Arc::new(WasmProvider::new(
            extension.id.clone(),
            models,
            extension.wasm.clone(),
            granted_root.clone(),
        )));
    }

    Ok(ResolvedModel {
        provider,
        model,
        thinking_level,
        has_provider_key,
        granted_extension_providers,
        theme: settings.theme.clone(),
        providers,
    })
}

/// Build one provider runtime from a pre-computed credential. Shared by
/// [`resolve_with_settings`] (for the selected provider) and the
/// all-provider runtime list built for `/model` hot-switching.
fn build_runtime(
    provider_id: &str,
    api: &kiqi_ai::Api,
    catalog: &[Model],
    anthropic_credential: Option<&AnthropicCredential>,
    openai_key: Option<&str>,
    settings: &settings::Settings,
) -> (Arc<dyn Provider>, bool) {
    let mut provider_models: Vec<Model> = catalog
        .iter()
        .filter(|candidate| candidate.api == *api && candidate.provider == provider_id)
        .cloned()
        .collect();
    if let Some(AnthropicCredential::Headers(headers)) = anthropic_credential {
        for candidate in &mut provider_models {
            merge_auth_headers(candidate, headers);
        }
    }
    if let Some(key) = openai_key {
        let headers = BTreeMap::from([("authorization".into(), format!("Bearer {key}"))]);
        for candidate in &mut provider_models {
            merge_auth_headers(candidate, &headers);
        }
    }
    match api {
        kiqi_ai::Api::AnthropicMessages => {
            let provider_key = anthropic_credential
                .and_then(AnthropicCredential::provider_key)
                .map(str::to_string);
            let has_key = provider_key.is_some();
            let http =
                provider_http_client(&provider_models, "https://api.anthropic.com", settings);
            let inner = if provider_id == DEFAULT_PROVIDER_ID
                && !matches!(anthropic_credential, Some(AnthropicCredential::Headers(_)))
            {
                AnthropicProvider::with_models(provider_key, http, provider_models)
            } else {
                AnthropicProvider::with_models_without_env_api_key(
                    provider_key,
                    http,
                    provider_models,
                )
            };
            (
                Arc::new(NamedAnthropicProvider {
                    id: provider_id.to_string(),
                    inner,
                }),
                has_key,
            )
        }
        kiqi_ai::Api::OpenaiCompletions => {
            let has_key = openai_key.is_some();
            let http =
                provider_http_client(&provider_models, "https://api.openai.com/v1", settings);
            let inner = if provider_id == "openai" {
                OpenAiCompletionsProvider::with_models(
                    provider_id.to_string(),
                    openai_key.map(str::to_string),
                    http,
                    provider_models,
                )
            } else {
                OpenAiCompletionsProvider::with_models_without_env_api_key(
                    provider_id.to_string(),
                    openai_key.map(str::to_string),
                    http,
                    provider_models,
                )
            };
            (Arc::new(inner), has_key)
        }
        kiqi_ai::Api::OpenaiResponses => {
            let has_key = openai_key.is_some();
            let http =
                provider_http_client(&provider_models, "https://api.openai.com/v1", settings);
            let inner = if provider_id == "openai" {
                OpenAiResponsesProvider::with_models(
                    provider_id.to_string(),
                    openai_key.map(str::to_string),
                    http,
                    provider_models,
                )
            } else {
                OpenAiResponsesProvider::with_models_without_env_api_key(
                    provider_id.to_string(),
                    openai_key.map(str::to_string),
                    http,
                    provider_models,
                )
            };
            (Arc::new(inner), has_key)
        }
        _ => unreachable!("unsupported APIs are filtered while loading models.json"),
    }
}

/// The catalog the TUI's `/model` selector displays (read-only). Re-derives the
/// **auth-filtered** snapshot the provider was built from so the selector shows
/// exactly the models that can actually run (mirrors pi `getAvailableSnapshot`:
/// `available = all.filter(m => configuredProviders.has(m.provider))` — v1's
/// single-provider equivalent of "configured" is [`model_is_authed`]).
///
/// The runtime provider already owns one provider-id-specific catalog. The
/// additional auth filter keeps header-only and provider-key paths consistent
/// if a future runtime exposes a broader snapshot.
///
/// On any config read error it falls back to the built-in Anthropic catalog —
/// the selector is non-critical and must never block the TUI from starting.
pub fn available_catalog(resolved: &ResolvedModel) -> Vec<Model> {
    let selected_api = &resolved.model.api;
    let selected_provider = &resolved.model.provider;
    let mut seen = std::collections::HashSet::new();
    resolved
        .provider
        .models()
        .iter()
        // A provider snapshot is expected to be homogeneous, but extension
        // and gateway providers can expose a broader catalog. The selector
        // must only offer models the current lane can actually route to.
        .filter(|m| m.api == *selected_api && m.provider == *selected_provider)
        .filter(|m| {
            model_is_authed(
                m,
                resolved.has_provider_key,
                &resolved.granted_extension_providers,
            )
        })
        .filter(|m| seen.insert((m.api.clone(), m.provider.clone(), m.id.to_ascii_lowercase())))
        .cloned()
        .collect()
}

/// Return every authenticated model across ALL configured providers (built-in
/// + models.json), not just the currently selected one. Derived from the
/// already-built provider runtimes so each model keeps its merged auth headers
/// and base-url override (a `catalog_all()`-based derivation would drop both).
pub fn authenticated_catalog(resolved: &ResolvedModel) -> Vec<Model> {
    let mut seen = std::collections::HashSet::new();
    resolved
        .providers
        .iter()
        .flat_map(|provider| provider.models())
        .filter(|m| seen.insert((m.api.clone(), m.provider.clone(), m.id.to_ascii_lowercase())))
        .cloned()
        .collect()
}

/// Return the merged built-in + `models.json` catalog without requiring a
/// credential or selecting a runnable model. This is used by CLI commands
/// such as `--list-models`, which must remain useful before authentication.
pub fn catalog_all() -> Result<Vec<Model>, config::ConfigError> {
    let cfg = config::load_models_config()?;
    let mut catalog = anthropic_models();
    catalog.extend(openai_responses_models());
    merge_user_catalog(&mut catalog, &cfg);
    catalog.sort_by(|a, b| {
        a.provider
            .to_ascii_lowercase()
            .cmp(&b.provider.to_ascii_lowercase())
            .then_with(|| a.id.to_ascii_lowercase().cmp(&b.id.to_ascii_lowercase()))
    });
    Ok(catalog)
}

/// Merge `~/.ki/models.json` providers into the built-in catalog. Models from
/// the same runtime provider and API replace entries with the same id; models
/// with the same id under different OpenAI-compatible providers remain
/// distinct so `provider/id` can select the intended endpoint.
fn merge_user_catalog(catalog: &mut Vec<Model>, cfg: &config::ModelsConfig) {
    for (provider_id, provider_cfg) in &cfg.providers {
        let Some(models) = config::provider_to_models(provider_id, provider_cfg) else {
            // Unsupported protocol.
            continue;
        };
        for m in models {
            if let Some(existing) = catalog.iter_mut().find(|candidate| {
                candidate.api == m.api
                    && candidate.provider == m.provider
                    && candidate.id.eq_ignore_ascii_case(&m.id)
            }) {
                *existing = m;
            } else {
                catalog.push(m);
            }
        }
    }
}

/// One granted extension provider, as read off disk for catalog construction.
struct GrantedExtension {
    /// The install-dir id. A grant's provider profile must name this id; the
    /// host refuses to instantiate any profile that does not.
    id: String,
    /// `extensions/<id>/extension.wasm`, verified through the digest sidecar before
    /// its catalog is surfaced. The provider adapter loads from here.
    wasm: PathBuf,
    grant: kiqi_host::grants::ProviderGrant,
}

/// Read `extensions/*/grants.json` and return the providers the user actually
/// granted under `root`.
///
/// Deny-by-default: an extension without a `provider` grant contributes nothing,
/// and a profile whose id is not the install id is ignored (the host would
/// never instantiate it). The digest sidecar is enforced here for the same
/// reason it is enforced before loading: a catalog entry is a promise that the
/// recorded bytes will run, so mismatched bytes must not appear.
fn granted_extension_models(root: &Path) -> Vec<GrantedExtension> {
    let mut granted = Vec::new();
    for install_id in kiqi_host::grants::installed_extensions_at(root) {
        let grants = kiqi_host::grants::Grants::load_for_at(root, &install_id);
        let Some(grant) = grants.provider.clone() else {
            continue;
        };
        // Only the profile that names this install id is instantiable; a
        // hand-edited grant naming another id grants nothing.
        if !grant
            .providers
            .iter()
            .any(|profile| profile.id == install_id)
        {
            continue;
        }
        let wasm = kiqi_host::grants::extension_dir_at(root, &install_id).join("extension.wasm");
        if !wasm.is_file() {
            continue;
        }
        if !wasm_digest_permits_catalog(&wasm) {
            continue;
        }
        granted.push(GrantedExtension {
            id: install_id,
            wasm,
            grant,
        });
    }
    granted
}

/// Whether the component's bytes match the digest sidecar recorded at install
/// time. `Ok(Some("missing"))` (an install that predates digests) is allowed,
/// exactly as the session loader allows it; a mismatch or an unreadable
/// sidecar is not.
#[cfg(feature = "extension-wasm")]
fn wasm_digest_permits_catalog(wasm: &Path) -> bool {
    match crate::extensions::digest_gate(wasm) {
        Ok(_) => true,
        Err(diagnostic) => {
            tracing::warn!("{diagnostic}; its provider catalog is not offered");
            false
        }
    }
}

#[cfg(not(feature = "extension-wasm"))]
fn wasm_digest_permits_catalog(_wasm: &Path) -> bool {
    false
}

/// Translate one granted model into the runtime catalog entry. The grant is
/// the authority for every field, so nothing here reads a guest declaration.
fn granted_model_to_model(
    provider_id: &str,
    model: &kiqi_host::grants::ProviderModel,
) -> Option<Model> {
    let api: kiqi_ai::Api =
        serde_json::from_value(serde_json::Value::String(model.api.clone())).ok()?;
    if matches!(api, kiqi_ai::Api::Other(_) | kiqi_ai::Api::Faux) {
        return None;
    }
    let mut entry = Model::new(
        model.id.clone(),
        model.id.clone(),
        api,
        provider_id,
        model.base_url.clone(),
    );
    entry.reasoning = model.reasoning;
    if !model.input.is_empty() {
        entry.input = model
            .input
            .iter()
            .filter_map(|name| match name.as_str() {
                "text" => Some(kiqi_ai::InputModality::Text),
                "image" => Some(kiqi_ai::InputModality::Image),
                _ => None,
            })
            .collect();
        if entry.input.is_empty() {
            entry.input = vec![kiqi_ai::InputModality::Text];
        }
    }
    Some(entry)
}

/// Refuse an id collision instead of silently shadowing. Built-ins and
/// models.json come first by construction; an extension may add only a new id.
fn refuse_extension_provider_collisions(
    granted: &[GrantedExtension],
    builtin: &BTreeSet<String>,
    configured: &BTreeSet<String>,
) -> Result<(), ResolveError> {
    let mut seen = BTreeSet::new();
    for extension in granted {
        if builtin.contains(&extension.id) {
            return Err(ResolveError::ProviderIdCollision {
                id: extension.id.clone(),
                existing: "a built-in provider".to_string(),
            });
        }
        if configured.contains(&extension.id) {
            return Err(ResolveError::ProviderIdCollision {
                id: extension.id.clone(),
                existing: "a models.json provider".to_string(),
            });
        }
        if !seen.insert(extension.id.clone()) {
            return Err(ResolveError::ProviderIdCollision {
                id: extension.id.clone(),
                existing: "another extension".to_string(),
            });
        }
    }
    Ok(())
}

/// Build the effective catalog in its fixed order: built-ins, then
/// `models.json`, then granted extension providers.
fn catalog_with_granted_extensions(
    models_cfg: &config::ModelsConfig,
    granted: &[GrantedExtension],
) -> Result<Vec<Model>, ResolveError> {
    let mut catalog = anthropic_models();
    catalog.extend(openai_responses_models());
    let mut builtin: BTreeSet<String> =
        catalog.iter().map(|model| model.provider.clone()).collect();
    builtin.extend(BUILTIN_PROVIDER_IDS.iter().map(|id| (*id).to_string()));
    merge_user_catalog(&mut catalog, models_cfg);
    let configured: BTreeSet<String> = models_cfg.providers.keys().cloned().collect();

    refuse_extension_provider_collisions(granted, &builtin, &configured)?;

    for extension in granted {
        for profile in &extension.grant.providers {
            if profile.id != extension.id {
                continue;
            }
            for model in &profile.models {
                if let Some(entry) = granted_model_to_model(&extension.id, model) {
                    catalog.push(entry);
                }
            }
        }
    }
    Ok(catalog)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AnthropicCredential {
    /// Passed as the selected provider's default `x-api-key`.
    ProviderKey(String),
    /// Folded only onto models owned by the selected provider.
    Headers(BTreeMap<String, String>),
}

impl AnthropicCredential {
    fn provider_key(&self) -> Option<&str> {
        match self {
            Self::ProviderKey(key) => Some(key),
            Self::Headers(_) => None,
        }
    }
}

type AnthropicCredentials = BTreeMap<String, AnthropicCredential>;
type OpenAiCredentials = BTreeMap<String, String>;

/// Resolve one independent Anthropic-wire credential for each provider id.
/// Stored credentials win over `models.json`; only the built-in `anthropic`
/// identity may fall back to the `ANTHROPIC_*` environment variables.
fn resolve_anthropic_credentials(
    cfg: &config::ModelsConfig,
    auth_store: &config::AuthStore,
) -> AnthropicCredentials {
    let mut provider_ids = vec![DEFAULT_PROVIDER_ID.to_string()];
    for (provider_id, provider_cfg) in &cfg.providers {
        if config::provider_is_anthropic_compatible(provider_cfg)
            && !provider_ids.iter().any(|known| known == provider_id)
        {
            provider_ids.push(provider_id.clone());
        }
    }

    let mut credentials = BTreeMap::new();
    for provider_id in provider_ids {
        let provider_cfg = cfg.providers.get(&provider_id);
        let auth_header = provider_cfg
            .and_then(|provider| provider.auth_header)
            .unwrap_or(false);
        let stored_key = stored_api_key(auth_store, &provider_id);
        let configured_key = provider_cfg.and_then(|provider| {
            provider
                .api_key
                .as_deref()
                .filter(|raw| !raw.is_empty())
                .and_then(|raw| config::resolve_config_value(raw, None))
                .filter(|key| !key.is_empty())
        });

        let credential = stored_key
            .or(configured_key)
            .map(|key| anthropic_credential_from_key(key, auth_header))
            .or_else(|| {
                if provider_id != DEFAULT_PROVIDER_ID {
                    return None;
                }
                std::env::var(ANTHROPIC_AUTH_TOKEN_ENV)
                    .ok()
                    .filter(|token| !token.is_empty())
                    .map(|token| {
                        AnthropicCredential::Headers(BTreeMap::from([(
                            "authorization".to_string(),
                            format!("Bearer {token}"),
                        )]))
                    })
                    .or_else(|| {
                        std::env::var(ANTHROPIC_API_KEY_ENV)
                            .ok()
                            .filter(|key| !key.is_empty())
                            .map(|key| anthropic_credential_from_key(key, auth_header))
                    })
            });
        if let Some(credential) = credential {
            credentials.insert(provider_id, credential);
        }
    }
    credentials
}

fn stored_api_key(auth_store: &config::AuthStore, provider_id: &str) -> Option<String> {
    auth_store
        .get(provider_id)
        .and_then(|credential| match credential {
            Credential::ApiKey {
                key: Some(key),
                env,
            } => config::resolve_config_value(key, env.as_ref()).filter(|key| !key.is_empty()),
            _ => None,
        })
}

fn anthropic_credential_from_key(key: String, auth_header: bool) -> AnthropicCredential {
    if auth_header {
        AnthropicCredential::Headers(BTreeMap::from([(
            "authorization".to_string(),
            format!("Bearer {key}"),
        )]))
    } else {
        AnthropicCredential::ProviderKey(key)
    }
}

fn anthropic_credential_for<'a>(
    credentials: &'a AnthropicCredentials,
    provider_id: &str,
) -> Option<&'a AnthropicCredential> {
    credentials.get(provider_id)
}

fn credential_for_selected_anthropic_provider(
    cli_api_key: Option<&str>,
    credentials: &AnthropicCredentials,
    provider_id: &str,
    auth_header: bool,
) -> Option<AnthropicCredential> {
    cli_api_key
        .filter(|key| !key.is_empty())
        .map(|key| anthropic_credential_from_key(key.to_string(), auth_header))
        .or_else(|| anthropic_credential_for(credentials, provider_id).cloned())
}

fn resolve_openai_credentials(
    cfg: &config::ModelsConfig,
    auth_store: &config::AuthStore,
) -> OpenAiCredentials {
    let mut provider_ids = vec!["openai".to_string()];
    for (provider_id, provider_cfg) in &cfg.providers {
        if (config::provider_is_openai_completions(provider_cfg)
            || config::provider_is_openai_responses(provider_cfg))
            && !provider_ids.iter().any(|known| known == provider_id)
        {
            provider_ids.push(provider_id.clone());
        }
    }

    let mut credentials = BTreeMap::new();
    for provider_id in provider_ids {
        let provider_cfg = cfg.providers.get(&provider_id);
        let key = stored_api_key(auth_store, &provider_id)
            .or_else(|| configured_openai_api_key(&provider_id, provider_cfg));
        if let Some(key) = key {
            credentials.insert(provider_id, key);
        }
    }
    credentials
}

fn configured_openai_api_key(
    provider_id: &str,
    provider_cfg: Option<&config::ProviderConfig>,
) -> Option<String> {
    if let Some(provider_cfg) = provider_cfg {
        return config::openai_provider_api_key(provider_id, provider_cfg);
    }

    (provider_id == "openai")
        .then(|| std::env::var(OPENAI_API_KEY_ENV).ok())
        .flatten()
        .filter(|value| !value.is_empty())
}

fn openai_credential_for<'a>(
    credentials: &'a OpenAiCredentials,
    provider_id: &str,
) -> Option<&'a String> {
    credentials.get(provider_id)
}

fn merge_auth_headers(model: &mut Model, auth_headers: &BTreeMap<String, String>) {
    let headers = model.headers.get_or_insert_with(BTreeMap::new);
    for (name, value) in auth_headers {
        headers.retain(|existing, _| !existing.eq_ignore_ascii_case(name));
        headers.insert(name.clone(), value.clone());
    }
}

struct SplitModelPattern {
    provider: Option<String>,
    model_id: String,
    raw_model_id: String,
    thinking: Option<ThinkingLevel>,
    inferred_provider: bool,
}

/// Split a `--model` value into its provider, id, and thinking components.
///
/// A slash prefix is provider syntax only when it names a known provider (or
/// matches an explicit `--provider`). Unknown prefixes stay in the raw model
/// id, which is required for OpenRouter-style ids such as
/// `meta-llama/llama-3.3`.
///
/// Mirrors the TS `parseModelPattern` last-colon split + recurse-on-prefix.
fn split_model_pattern(
    value: &str,
    cli_provider: Option<&str>,
    catalog: &[Model],
    cfg: &config::ModelsConfig,
    granted_extension_providers: &BTreeSet<String>,
) -> Result<SplitModelPattern, ResolveError> {
    let mut provider = cli_provider.map(str::to_string);
    let mut model_id = value.to_string();
    let mut inferred_provider = false;

    if let Some((prefix, remainder)) = value.split_once('/') {
        if !prefix.is_empty() && !remainder.is_empty() {
            let matches_explicit = cli_provider
                .and_then(|requested| {
                    canonicalize_cli_provider(prefix, cfg, granted_extension_providers)
                        .ok()
                        .map(|canonical| canonical == requested)
                })
                .unwrap_or(false);
            if matches_explicit {
                model_id = remainder.to_string();
            } else if cli_provider.is_none() {
                match canonicalize_cli_provider(prefix, cfg, granted_extension_providers) {
                    Ok(canonical) => {
                        provider = Some(canonical);
                        model_id = remainder.to_string();
                        inferred_provider = true;
                    }
                    Err(ResolveError::UnknownProvider(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }

    // upstream attempts the complete id first. Only peel a valid thinking
    // suffix when that complete id does not exist in the selected scope.
    let has_full_exact_match = catalog.iter().any(|model| {
        model.id.eq_ignore_ascii_case(&model_id)
            && provider
                .as_deref()
                .map_or(true, |requested| provider_matches(model, requested, cfg))
    });
    let thinking = if has_full_exact_match {
        None
    } else if let Some((head, suffix)) = model_id.rsplit_once(':') {
        if let Some(level) = parse_thinking_level(suffix) {
            model_id = head.to_string();
            Some(level)
        } else {
            None
        }
    } else {
        None
    };

    Ok(SplitModelPattern {
        provider,
        model_id,
        raw_model_id: value.to_string(),
        thinking,
        inferred_provider,
    })
}

/// Case-insensitive exact id match, optionally scoped to a provider.
fn find_model(
    pattern: &str,
    provider: Option<&str>,
    catalog: &[Model],
    cfg: &config::ModelsConfig,
) -> Option<Model> {
    catalog
        .iter()
        .find(|model| {
            model.id.eq_ignore_ascii_case(pattern)
                && provider.map_or(true, |requested| provider_matches(model, requested, cfg))
        })
        .cloned()
}

/// Resolve an exact CLI model reference without silently choosing the first
/// provider when a bare model id is duplicated. This mirrors upstream's
/// `resolveCliModel`: a sole configured-auth match wins; zero or multiple
/// configured-auth matches require an explicit provider.
fn find_cli_model(
    pattern: &str,
    provider: Option<&str>,
    catalog: &[Model],
    cfg: &config::ModelsConfig,
    anthropic_credentials: &AnthropicCredentials,
    openai_credentials: &OpenAiCredentials,
    granted_extension_providers: &BTreeSet<String>,
) -> Result<Option<Model>, ResolveError> {
    let exact_matches: Vec<&Model> = catalog
        .iter()
        .filter(|model| {
            model.id.eq_ignore_ascii_case(pattern)
                && provider.map_or(true, |requested| provider_matches(model, requested, cfg))
        })
        .collect();

    match exact_matches.as_slice() {
        [] => Ok(None),
        [model] => Ok(Some((*model).clone())),
        _ => {
            let authenticated: Vec<&Model> = exact_matches
                .iter()
                .copied()
                .filter(|model| {
                    model_is_authed_for_resolution(
                        model,
                        anthropic_credentials,
                        openai_credentials,
                        false,
                        granted_extension_providers,
                    )
                })
                .collect();
            if let [model] = authenticated.as_slice() {
                return Ok(Some((*model).clone()));
            }

            let mut matches = exact_matches
                .iter()
                .map(|model| format!("{}/{}", model.provider, model.id))
                .collect::<Vec<_>>();
            matches.sort();
            let auth_hint = if authenticated.is_empty() {
                "No matching provider is authenticated."
            } else {
                "More than one matching provider is authenticated."
            };
            Err(ResolveError::AmbiguousModel {
                pattern: pattern.to_string(),
                matches: matches.join(", "),
                auth_hint,
            })
        }
    }
}

fn provider_is_known(
    requested: &str,
    cfg: &config::ModelsConfig,
    granted_extension_providers: &BTreeSet<String>,
) -> bool {
    granted_extension_providers.contains(requested)
        || cfg.providers.contains_key(requested)
        || requested.eq_ignore_ascii_case("anthropic")
        || requested.eq_ignore_ascii_case("openai")
        || requested.eq_ignore_ascii_case("openai-completions")
        || requested.eq_ignore_ascii_case("openai-responses")
}

/// Canonicalize user-facing `--provider` input without weakening provider
/// identity. Exact configured ids win. A case-insensitive custom match is
/// accepted only when unique; otherwise the user must provide exact casing so
/// credentials and endpoints can never cross between distinct ids.
fn canonicalize_cli_provider(
    requested: &str,
    cfg: &config::ModelsConfig,
    granted_extension_providers: &BTreeSet<String>,
) -> Result<String, ResolveError> {
    if granted_extension_providers.contains(requested) {
        return Ok(requested.to_string());
    }
    if cfg.providers.contains_key(requested) {
        return Ok(requested.to_string());
    }

    if BUILTIN_PROVIDER_IDS.contains(&requested) {
        return Ok(requested.to_string());
    }

    let mut matches = cfg
        .providers
        .keys()
        .filter(|provider| provider.eq_ignore_ascii_case(requested))
        .cloned()
        .collect::<Vec<_>>();
    matches.extend(
        BUILTIN_PROVIDER_IDS
            .iter()
            .filter(|provider| provider.eq_ignore_ascii_case(requested))
            .map(|provider| (*provider).to_string()),
    );
    matches.extend(
        granted_extension_providers
            .iter()
            .filter(|provider| provider.eq_ignore_ascii_case(requested))
            .cloned(),
    );
    matches.sort();
    matches.dedup();

    match matches.as_slice() {
        [provider] => return Ok(provider.clone()),
        [] => {}
        _ => {
            return Err(ResolveError::AmbiguousProvider {
                requested: requested.to_string(),
                matches: matches.join(", "),
            });
        }
    }

    Err(ResolveError::UnknownProvider(requested.to_string()))
}

fn provider_matches(model: &Model, requested: &str, cfg: &config::ModelsConfig) -> bool {
    // Configured provider ids are exact identities in upstream. An exact
    // config match takes precedence over the case-insensitive built-in/protocol
    // aliases below, so a custom `Anthropic` remains distinct from `anthropic`.
    if model.provider == requested {
        return true;
    }
    if cfg.providers.contains_key(requested) {
        return false;
    }
    if requested.eq_ignore_ascii_case("anthropic") {
        return model.provider == DEFAULT_PROVIDER_ID;
    }
    if requested.eq_ignore_ascii_case("openai") {
        return model.provider == "openai";
    }
    if requested.eq_ignore_ascii_case("openai-completions") {
        return matches!(model.api, kiqi_ai::Api::OpenaiCompletions);
    }
    if requested.eq_ignore_ascii_case("openai-responses") {
        return matches!(model.api, kiqi_ai::Api::OpenaiResponses);
    }
    false
}

/// Provider identity matching for upstream's default-model table. Unlike the
/// CLI matcher, this intentionally does not treat `openai` as a protocol alias:
/// a default belonging to OpenAI must not select the same model id from an
/// unrelated OpenAI-compatible gateway.
fn model_belongs_to_default_provider(
    model: &Model,
    requested: &str,
    _cfg: &config::ModelsConfig,
) -> bool {
    model.provider == requested
}

/// Whether a catalog model is "configured-auth" — i.e. the request built for it
/// would pass `assertRequestAuth` and not return "No API key". Mirrors the TS
/// `hasConfiguredAuth(providerId)` filter that `getAvailableSnapshot()` applies
/// (`available = all.filter(m => configuredProviders.has(m.provider))`).
///
/// The selected provider snapshot is identity-homogeneous. A model is runnable
/// when its own headers carry auth, the selected provider has a default key,
/// or the model's provider is granted to an installed extension.
fn model_is_authed(
    m: &Model,
    has_provider_key: bool,
    granted_extension_providers: &BTreeSet<String>,
) -> bool {
    model_has_header_auth(m)
        || has_provider_key
        || granted_extension_providers.contains(&m.provider)
}

fn model_is_authed_for_resolution(
    model: &Model,
    anthropic_credentials: &AnthropicCredentials,
    openai_credentials: &OpenAiCredentials,
    has_cli_key: bool,
    granted_extension_providers: &BTreeSet<String>,
) -> bool {
    model_has_header_auth(model)
        || has_cli_key
        || granted_extension_providers.contains(&model.provider)
        || match model.api {
            kiqi_ai::Api::AnthropicMessages => {
                anthropic_credential_for(anthropic_credentials, &model.provider).is_some()
            }
            kiqi_ai::Api::OpenaiCompletions | kiqi_ai::Api::OpenaiResponses => {
                openai_credential_for(openai_credentials, &model.provider).is_some()
            }
            _ => false,
        }
}

/// Same three-name check as kiqi-ai's `has_header_auth`, but called from the
/// CLI layer (kiqi-ai's `has_header_auth` is private to the provider module, so
/// we mirror it here over the model's `headers` map).
fn model_has_header_auth(m: &Model) -> bool {
    let Some(h) = &m.headers else { return false };
    const NAMES: &[&str] = &["authorization", "x-api-key", "cf-aig-authorization"];
    h.keys()
        .any(|k| NAMES.contains(&k.to_ascii_lowercase().as_str()))
}

/// Choose the default model when `--model` is absent. Mirrors upstream
/// `findInitialModel` [`packages/coding-agent/src/core/model-resolver.ts`]:
/// known-provider defaults are checked in native declaration order, followed by
/// the first authenticated model in the catalog (`availableModels[0]`). This
/// also keeps a models.json-only gateway from accidentally selecting an
/// unauthenticated built-in model.
///
/// Credential maps are keyed by provider id, so only a credential belonging to
/// the candidate model can make it participate in default selection.
fn pick_default_model(
    catalog: &[Model],
    models_cfg: &config::ModelsConfig,
    anthropic_credentials: &AnthropicCredentials,
    openai_credentials: &OpenAiCredentials,
    has_cli_key: bool,
    granted_extension_providers: &BTreeSet<String>,
) -> Model {
    // 1. upstream's known-provider defaults, in its declared priority order.
    for (provider, model_id) in DEFAULT_MODELS_PER_PROVIDER {
        if let Some(model) = catalog.iter().find(|model| {
            model.id.eq_ignore_ascii_case(model_id)
                && model_belongs_to_default_provider(model, provider, models_cfg)
                && model_is_authed_for_resolution(
                    model,
                    anthropic_credentials,
                    openai_credentials,
                    has_cli_key,
                    granted_extension_providers,
                )
        }) {
            return model.clone();
        }
    }

    // 2. First authed model (TS `availableModels[0]`). Provider and model array
    //    declaration order is preserved while loading models.json.
    //    this is the gateway model (Bearer folded onto it, base_url = gateway).
    if let Some(m) = catalog.iter().find(|m| {
        model_is_authed_for_resolution(
            m,
            anthropic_credentials,
            openai_credentials,
            has_cli_key,
            granted_extension_providers,
        )
    }) {
        return m.clone();
    }
    // 3. Last resort: the built-in Anthropic default, authed or not. The auth gate above
    //    already errored when no source resolved, so reaching here means *some*
    //    auth exists but none folded/attached to a model we can see — keep the
    //    historical default to avoid a NoMatch surprise.
    catalog
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(DEFAULT_MODEL_ID))
        .or_else(|| catalog.first())
        .expect("catalog is never empty (built-in anthropic_models)")
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{parse_thinking_level, VALID_THINKING_LEVELS};
    use crate::config::test_support::env_lock;

    /// Scope a test to a throwaway config dir + clear the `ANTHROPIC_*` env
    /// vars, restoring both on drop. Holds the shared env lock for its whole
    /// lifetime so parallel env-mutating tests across config/provider/auth all
    /// serialize on one mutex.
    struct TestEnv {
        _guard: crate::env_lock::EnvGuard,
        prev_key: Option<std::ffi::OsString>,
        prev_tok: Option<std::ffi::OsString>,
        prev_base: Option<std::ffi::OsString>,
        prev_openai_key: Option<std::ffi::OsString>,
        prev_dir: Option<std::ffi::OsString>,
        _tmp: tempfile::TempDir,
    }
    impl TestEnv {
        fn new() -> Self {
            let guard = env_lock().lock();
            let prev_key = std::env::var_os(ANTHROPIC_API_KEY_ENV);
            let prev_tok = std::env::var_os(ANTHROPIC_AUTH_TOKEN_ENV);
            let prev_base = std::env::var_os(ANTHROPIC_BASE_URL_ENV);
            let prev_openai_key = std::env::var_os(OPENAI_API_KEY_ENV);
            let prev_dir = std::env::var_os(config::CONFIG_DIR_ENV);
            std::env::remove_var(ANTHROPIC_API_KEY_ENV);
            std::env::remove_var(ANTHROPIC_AUTH_TOKEN_ENV);
            std::env::remove_var(ANTHROPIC_BASE_URL_ENV);
            std::env::remove_var(OPENAI_API_KEY_ENV);
            let tmp = tempfile::TempDir::new().unwrap();
            std::env::set_var(config::CONFIG_DIR_ENV, tmp.path());
            Self {
                _guard: guard,
                prev_key,
                prev_tok,
                prev_base,
                prev_openai_key,
                prev_dir,
                _tmp: tmp,
            }
        }
    }
    impl Drop for TestEnv {
        fn drop(&mut self) {
            restore(ANTHROPIC_API_KEY_ENV, self.prev_key.take());
            restore(ANTHROPIC_AUTH_TOKEN_ENV, self.prev_tok.take());
            restore(ANTHROPIC_BASE_URL_ENV, self.prev_base.take());
            restore(OPENAI_API_KEY_ENV, self.prev_openai_key.take());
            restore(config::CONFIG_DIR_ENV, self.prev_dir.take());
        }
    }
    fn restore(name: &str, prev: Option<std::ffi::OsString>) {
        match prev {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
    }

    // These tests hit the network-free resolution path only (provider/model
    // selection). They set a throwaway credential so `resolve` clears the
    // `NoApiKey` gate, then assert the model + thinking choice — never making
    // a real request.

    fn resolve_with_key(
        provider: Option<&str>,
        model: Option<&str>,
        thinking: Option<ThinkingLevel>,
    ) -> Result<ResolvedModel, ResolveError> {
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_API_KEY_ENV, "test-key");
        resolve(provider, model, thinking, None, None)
    }

    #[test]
    fn default_model_matches_native_anthropic_default() {
        let r = resolve_with_key(None, None, None).unwrap();
        assert_eq!(r.model.id, DEFAULT_MODEL_ID);
        assert_eq!(r.thinking_level, DEFAULT_THINKING_LEVEL);
        assert_eq!(r.provider.id(), "anthropic");
    }

    #[test]
    fn settings_default_model_wins_when_authed() {
        // A copied pi `settings.json` carrying `defaultModel` (step 3 of pi's
        // `findInitialModel`) overrides the built-in Anthropic default
        // when that model is in the catalog and authed. Mirrors the on-disk-
        // Drop an agent dir at `~/.ki/agent/` and the saved
        // default comes alive on launch (no `--model` needed).
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_API_KEY_ENV, "k");
        let path = config::settings_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"defaultProvider":"anthropic","defaultModel":"claude-haiku-4-5","defaultThinkingLevel":"high"}"#,
        )
        .unwrap();
        let r = resolve(None, None, None, None, None).unwrap();
        assert_eq!(r.model.id, "claude-haiku-4-5");
        assert_eq!(r.thinking_level, ThinkingLevel::High);
        // An unauthed saved default (unknown id) falls through to the built-in.
        std::fs::write(&path, r#"{"defaultModel":"claude-does-not-exist"}"#).unwrap();
        let r = resolve(None, None, None, None, None).unwrap();
        assert_eq!(r.model.id, DEFAULT_MODEL_ID);
    }

    #[test]
    fn explicit_id_match() {
        let r = resolve_with_key(None, Some("claude-haiku-4-5"), None).unwrap();
        assert_eq!(r.model.id, "claude-haiku-4-5");
    }

    #[test]
    fn case_insensitive_id() {
        let r = resolve_with_key(None, Some("CLAUDE-OPUS-5"), None).unwrap();
        assert_eq!(r.model.id, "claude-opus-5");
    }

    #[test]
    fn provider_prefix_stripped() {
        let r = resolve_with_key(None, Some("anthropic/claude-sonnet-5"), None).unwrap();
        assert_eq!(r.model.id, "claude-sonnet-5");
    }

    #[test]
    fn custom_provider_prefix_stripped() {
        // `gateway/custom-claude` resolves to the catalog id `custom-claude`
        // after the `foo/` prefix is stripped.
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_API_KEY_ENV, "official-key");
        std::fs::write(
            config::models_path().unwrap(),
            r#"{ "providers": { "gateway": { "baseUrl": "https://gw", "apiKey": "gateway-key", "models": [{"id":"custom-claude"}] } } }"#,
        )
        .unwrap();
        let r = resolve(None, Some("gateway/custom-claude"), None, None, None).unwrap();
        assert_eq!(r.model.id, "custom-claude");
    }

    #[test]
    fn unknown_slash_prefix_remains_part_of_the_raw_model_id() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "api": "openai-completions",
      "apiKey": "gateway-key",
      "models": [{"id":"meta-llama/llama-3.3"}]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, Some("meta-llama/llama-3.3:high"), None, None, None).unwrap();
        assert_eq!(resolved.model.provider, "gateway");
        assert_eq!(resolved.model.id, "meta-llama/llama-3.3");
        assert_eq!(resolved.thinking_level, ThinkingLevel::High);
    }

    #[test]
    fn complete_model_id_wins_before_parsing_a_thinking_suffix() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "api": "openai-completions",
      "apiKey": "gateway-key",
      "models": [
        {"id":"vendor/model"},
        {"id":"vendor/model:high"}
      ]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, Some("gateway/vendor/model:high"), None, None, None).unwrap();
        assert_eq!(resolved.model.provider, "gateway");
        assert_eq!(resolved.model.id, "vendor/model:high");
        assert_eq!(resolved.thinking_level, DEFAULT_THINKING_LEVEL);
    }

    #[test]
    fn authenticated_inferred_provider_beats_a_matching_raw_model_id() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "baseUrl": "https://alpha.example.com",
      "apiKey": "alpha-key",
      "models": [{"id":"target"}]
    },
    "gateway": {
      "api": "openai-completions",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "gateway-key",
      "models": [{"id":"alpha/target"}]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, Some("alpha/target"), None, None, None).unwrap();
        assert_eq!(resolved.model.provider, "alpha");
        assert_eq!(resolved.model.id, "target");
        assert_eq!(resolved.model.base_url, "https://alpha.example.com");
    }

    #[test]
    fn authenticated_raw_model_beats_an_unauthenticated_inferred_provider() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "baseUrl": "https://alpha.example.com",
      "models": [{"id":"target"}]
    },
    "gateway": {
      "api": "openai-completions",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "gateway-key",
      "models": [{"id":"alpha/target"}]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, Some("alpha/target"), None, None, None).unwrap();
        assert_eq!(resolved.model.provider, "gateway");
        assert_eq!(resolved.model.id, "alpha/target");
        assert_eq!(resolved.model.base_url, "https://gateway.example.com");
    }

    #[test]
    fn thinking_suffix_raw_model_beats_an_unauthenticated_inferred_provider() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "baseUrl": "https://alpha.example.com",
      "models": [{"id":"target"}]
    },
    "gateway": {
      "api": "openai-completions",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "gateway-key",
      "models": [{"id":"alpha/target"}]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, Some("alpha/target:high"), None, None, None).unwrap();
        assert_eq!(resolved.model.provider, "gateway");
        assert_eq!(resolved.model.id, "alpha/target");
        assert_eq!(resolved.model.base_url, "https://gateway.example.com");
        assert_eq!(resolved.thinking_level, ThinkingLevel::High);
    }

    #[test]
    fn thinking_suffix_in_model() {
        let r = resolve_with_key(None, Some("claude-sonnet-5:high"), None).unwrap();
        assert_eq!(r.model.id, "claude-sonnet-5");
        assert_eq!(r.thinking_level, ThinkingLevel::High);
    }

    #[test]
    fn thinking_flag_overrides_suffix() {
        // `--thinking low` wins over a `:high` suffix.
        let r =
            resolve_with_key(None, Some("claude-sonnet-5:high"), Some(ThinkingLevel::Low)).unwrap();
        assert_eq!(r.thinking_level, ThinkingLevel::Low);
    }

    #[test]
    fn explicit_provider_anthropic_ok() {
        let r = resolve_with_key(Some("anthropic"), Some("claude-sonnet-5"), None).unwrap();
        assert_eq!(r.model.id, "claude-sonnet-5");

        let r = resolve_with_key(Some("ANTHROPIC"), Some("claude-sonnet-5"), None).unwrap();
        assert_eq!(r.provider.id(), DEFAULT_PROVIDER_ID);
    }

    #[test]
    fn unknown_provider_rejected() {
        let err = resolve_with_key(Some("unsupported-provider"), None, None).unwrap_err();
        assert!(matches!(err, ResolveError::UnknownProvider(_)));
    }

    #[test]
    fn no_match_lists_available() {
        let err = resolve_with_key(None, Some("claude-does-not-exist"), None).unwrap_err();
        match err {
            ResolveError::NoMatch { pattern, available } => {
                assert_eq!(pattern, "claude-does-not-exist");
                assert!(available.contains("claude-sonnet-5"));
            }
            other => panic!("expected NoMatch, got {other:?}"),
        }
    }

    #[test]
    fn colon_not_a_thinking_level_kept_in_id() {
        // A trailing `:foo` that isn't a thinking level stays part of the id
        // pattern → no match (no model id contains `:foo`).
        let err = resolve_with_key(None, Some("claude-sonnet-5:foo"), None).unwrap_err();
        assert!(matches!(err, ResolveError::NoMatch { .. }));
    }

    #[test]
    fn parse_thinking_level_roundtrip() {
        assert_eq!(parse_thinking_level("xhigh"), Some(ThinkingLevel::Xhigh));
        assert_eq!(parse_thinking_level("bogus"), None);
        // Sanity: the valid set matches what help advertises.
        for lvl in VALID_THINKING_LEVELS {
            assert!(parse_thinking_level(lvl).is_some(), "{lvl} should parse");
        }
    }

    #[test]
    fn no_api_key_errors_with_hint() {
        let _env = TestEnv::new();
        let err = resolve(None, None, None, None, None).unwrap_err();
        match err {
            ResolveError::NoApiKey { hint } => {
                assert!(hint.contains("ANTHROPIC_API_KEY"));
                assert!(hint.contains("auth login"));
            }
            other => panic!("expected NoApiKey, got {other:?}"),
        }
    }

    #[test]
    fn stored_credential_satisfies_auth() {
        let _env = TestEnv::new();
        config::upsert_credential(
            DEFAULT_PROVIDER_ID,
            Credential::ApiKey {
                key: Some("stored-key".into()),
                env: None,
            },
        )
        .unwrap();
        let r = resolve(None, None, None, None, None).unwrap();
        assert_eq!(r.model.id, DEFAULT_MODEL_ID);
        // x-api-key path: no Bearer header folded onto the model (auth rides on
        // the provider's default key, surfaced to the provider at build time).
        assert!(
            r.model
                .headers
                .as_ref()
                .and_then(|h| h.get("authorization"))
                .is_none(),
            "x-api-key path should not synthesize a Bearer header"
        );
    }

    #[test]
    fn gateway_models_key_wins_over_unrelated_anthropic_auth() {
        let _env = TestEnv::new();
        config::upsert_credential(
            DEFAULT_PROVIDER_ID,
            Credential::ApiKey {
                key: Some("official-key".into()),
                env: None,
            },
        )
        .unwrap();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "gateway-key",
      "models": [{"id":"gateway-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, Some("gateway/gateway-model"), None, None, None).unwrap();
        assert_eq!(resolved.provider.id(), "gateway");
        assert_eq!(resolved.model.provider, "gateway");
        assert!(resolved.has_provider_key);

        let cfg = config::load_models_config().unwrap();
        let credentials = resolve_anthropic_credentials(&cfg, &config::read_auth().unwrap());
        assert_eq!(
            anthropic_credential_for(&credentials, DEFAULT_PROVIDER_ID),
            Some(&AnthropicCredential::ProviderKey("official-key".into()))
        );
        assert_eq!(
            anthropic_credential_for(&credentials, "gateway"),
            Some(&AnthropicCredential::ProviderKey("gateway-key".into()))
        );
    }

    #[test]
    fn gateway_auth_json_credential_is_usable() {
        let _env = TestEnv::new();
        config::upsert_credential(
            "gateway",
            Credential::ApiKey {
                key: Some("stored-gateway-key".into()),
                env: None,
            },
        )
        .unwrap();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "models": [{"id":"gateway-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(Some("gateway"), None, None, None, None).unwrap();
        assert_eq!(resolved.model.id, "gateway-model");
        assert_eq!(resolved.provider.id(), "gateway");
        assert!(resolved.has_provider_key);
    }

    #[test]
    fn anthropic_auth_does_not_authenticate_an_unkeyed_gateway() {
        let _env = TestEnv::new();
        config::upsert_credential(
            DEFAULT_PROVIDER_ID,
            Credential::ApiKey {
                key: Some("official-key".into()),
                env: None,
            },
        )
        .unwrap();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "models": [{"id":"gateway-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let error = resolve(
            Some("gateway"),
            Some("gateway/gateway-model"),
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(matches!(error, ResolveError::NoApiKey { .. }));

        let default = resolve(None, None, None, None, None).unwrap();
        assert_eq!(default.provider.id(), DEFAULT_PROVIDER_ID);
        assert_eq!(default.model.id, DEFAULT_MODEL_ID);
    }

    #[test]
    fn two_anthropic_gateways_keep_credentials_isolated_by_id() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "alpha": {
      "api": "anthropic-messages",
      "baseUrl": "https://shared.example.com",
      "apiKey": "alpha-key",
      "models": [{"id":"shared-model"}]
    },
    "beta": {
      "api": "anthropic-messages",
      "baseUrl": "https://shared.example.com",
      "apiKey": "beta-key",
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let cfg = config::load_models_config().unwrap();
        let credentials = resolve_anthropic_credentials(&cfg, &config::read_auth().unwrap());
        assert_eq!(
            anthropic_credential_for(&credentials, "alpha"),
            Some(&AnthropicCredential::ProviderKey("alpha-key".into()))
        );
        assert_eq!(
            anthropic_credential_for(&credentials, "beta"),
            Some(&AnthropicCredential::ProviderKey("beta-key".into()))
        );
        let alpha = resolve(None, Some("alpha/shared-model"), None, None, None).unwrap();
        let beta = resolve(None, Some("beta/shared-model"), None, None, None).unwrap();
        assert_eq!(alpha.provider.id(), "alpha");
        assert_eq!(beta.provider.id(), "beta");
        assert!(alpha.has_provider_key && beta.has_provider_key);
    }

    #[test]
    fn case_distinct_provider_ids_keep_endpoints_and_credentials_isolated() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "alpha": {
      "api": "anthropic-messages",
      "baseUrl": "https://lower.example.com",
      "authHeader": true,
      "models": [{"id":"shared-model"}]
    },
    "ALPHA": {
      "api": "anthropic-messages",
      "baseUrl": "https://upper.example.com",
      "authHeader": true,
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
        )
        .unwrap();
        config::upsert_credential(
            "alpha",
            Credential::ApiKey {
                key: Some("lower-key".into()),
                env: None,
            },
        )
        .unwrap();
        config::upsert_credential(
            "ALPHA",
            Credential::ApiKey {
                key: Some("upper-key".into()),
                env: None,
            },
        )
        .unwrap();

        let lower = resolve(None, Some("alpha/shared-model"), None, None, None).unwrap();
        assert_eq!(lower.provider.id(), "alpha");
        assert_eq!(lower.model.base_url, "https://lower.example.com");
        assert_eq!(
            lower
                .model
                .headers
                .as_ref()
                .and_then(|headers| headers.get("authorization"))
                .map(String::as_str),
            Some("Bearer lower-key")
        );

        let upper = resolve(None, Some("ALPHA/shared-model"), None, None, None).unwrap();
        assert_eq!(upper.provider.id(), "ALPHA");
        assert_eq!(upper.model.base_url, "https://upper.example.com");
        assert_eq!(
            upper
                .model
                .headers
                .as_ref()
                .and_then(|headers| headers.get("authorization"))
                .map(String::as_str),
            Some("Bearer upper-key")
        );

        let error = resolve(Some("Alpha"), None, None, None, None).unwrap_err();
        assert!(matches!(
            error,
            ResolveError::AmbiguousProvider {
                requested,
                matches
            } if requested == "Alpha" && matches == "ALPHA, alpha"
        ));

        let prefix_error = resolve(None, Some("Alpha/shared-model"), None, None, None).unwrap_err();
        assert!(matches!(
            prefix_error,
            ResolveError::AmbiguousProvider {
                requested,
                matches
            } if requested == "Alpha" && matches == "ALPHA, alpha"
        ));
    }

    #[test]
    fn cli_provider_case_insensitively_selects_one_canonical_custom_id() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "routeryo-copy": {
      "api": "openai-completions",
      "baseUrl": "https://routeryo-copy.example.com",
      "apiKey": "copy-key",
      "models": [{"id":"copy-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let resolved =
            resolve(Some("ROUTERYO-COPY"), Some("copy-model"), None, None, None).unwrap();
        assert_eq!(resolved.model.provider, "routeryo-copy");
        assert_eq!(resolved.provider.id(), "routeryo-copy");
        assert_eq!(resolved.model.base_url, "https://routeryo-copy.example.com");

        let prefixed = resolve(None, Some("ROUTERYO-COPY/copy-model"), None, None, None).unwrap();
        assert_eq!(prefixed.model.provider, "routeryo-copy");
        assert_eq!(prefixed.model.id, "copy-model");
    }

    #[test]
    fn cli_provider_canonicalization_keeps_builtin_and_custom_case_ids_distinct() {
        let _env = TestEnv::new();
        std::env::set_var(OPENAI_API_KEY_ENV, "official-openai-key");
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "OpenAI": {
      "api": "openai-completions",
      "baseUrl": "https://custom-openai.example.com",
      "apiKey": "custom-openai-key",
      "models": [{"id":"custom-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let custom = resolve(Some("OpenAI"), Some("custom-model"), None, None, None).unwrap();
        assert_eq!(custom.model.provider, "OpenAI");
        assert_eq!(custom.model.base_url, "https://custom-openai.example.com");

        let builtin = resolve(Some("openai"), Some("gpt-6-astra"), None, None, None).unwrap();
        assert_eq!(builtin.model.provider, "openai");

        let error = resolve(Some("OPENAI"), None, None, None, None).unwrap_err();
        assert!(matches!(
            error,
            ResolveError::AmbiguousProvider {
                requested,
                matches
            } if requested == "OPENAI" && matches == "OpenAI, openai"
        ));
    }

    #[test]
    fn cli_api_key_overrides_only_the_selected_anthropic_provider() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "authHeader": true,
      "apiKey": "configured-key",
      "models": [{"id":"gateway-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let cfg = config::load_models_config().unwrap();
        let credentials = resolve_anthropic_credentials(&cfg, &config::read_auth().unwrap());
        assert_eq!(
            credential_for_selected_anthropic_provider(
                Some("cli-key"),
                &credentials,
                "gateway",
                true,
            ),
            Some(AnthropicCredential::Headers(BTreeMap::from([(
                "authorization".into(),
                "Bearer cli-key".into(),
            )])))
        );
        assert_eq!(
            anthropic_credential_for(&credentials, "gateway"),
            Some(&AnthropicCredential::Headers(BTreeMap::from([(
                "authorization".into(),
                "Bearer configured-key".into(),
            )])))
        );

        let resolved = resolve(
            Some("gateway"),
            Some("gateway/gateway-model"),
            None,
            Some("cli-key"),
            None,
        )
        .unwrap();
        assert!(!resolved.has_provider_key);
        assert_eq!(
            resolved
                .model
                .headers
                .as_ref()
                .and_then(|headers| headers.get("authorization"))
                .map(String::as_str),
            Some("Bearer cli-key")
        );
    }

    #[test]
    fn auth_token_routes_via_bearer_header() {
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_AUTH_TOKEN_ENV, "tok-123");
        let r = resolve(None, None, None, None, None).unwrap();
        // No provider key carries auth — it lives on the model header.
        let headers = r.model.headers.as_ref().expect("bearer header on model");
        assert_eq!(
            headers.get("authorization").map(|s| s.as_str()),
            Some("Bearer tok-123")
        );
        // ANTHROPIC_AUTH_TOKEN is a *global* credential (not endpoint-specific
        // like a models.json gateway key): the default claude-sonnet-5 is picked
        // (it carries the env Bearer) — NOT a gateway model.
        assert_eq!(r.model.id, DEFAULT_MODEL_ID);
    }

    #[test]
    fn api_key_flag_beats_env_and_stored() {
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_API_KEY_ENV, "env-key");
        config::upsert_credential(
            DEFAULT_PROVIDER_ID,
            Credential::ApiKey {
                key: Some("stored-key".into()),
                env: None,
            },
        )
        .unwrap();
        // `--api-key flag-key` wins; resolve succeeds + takes the x-api-key path
        // (no Bearer header on the model).
        let r = resolve(None, None, None, Some("flag-key"), None).unwrap();
        assert!(
            r.model
                .headers
                .as_ref()
                .and_then(|h| h.get("authorization"))
                .is_none(),
            "--api-key should take the x-api-key path, not Bearer"
        );
    }

    #[test]
    fn base_url_override_applies_to_model() {
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_API_KEY_ENV, "k");
        let r = resolve(None, None, None, None, Some("https://gw.example.com")).unwrap();
        assert_eq!(r.model.base_url, "https://gw.example.com");
    }

    #[test]
    fn base_url_env_is_fallback_for_flag() {
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_API_KEY_ENV, "k");
        std::env::set_var(ANTHROPIC_BASE_URL_ENV, "https://env-gw.example.com");
        let r = resolve(None, None, None, None, None).unwrap();
        assert_eq!(r.model.base_url, "https://env-gw.example.com");
    }

    #[test]
    fn anthropic_base_url_env_does_not_redirect_a_custom_provider() {
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_BASE_URL_ENV, "https://ambient-proxy.example.com");
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "gateway-secret",
      "models": [{"id":"gateway-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let r = resolve(
            Some("gateway"),
            Some("gateway/gateway-model"),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(r.model.base_url, "https://gateway.example.com");
        assert!(r.has_provider_key);
    }

    #[test]
    fn models_json_adds_custom_model() {
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_API_KEY_ENV, "k");
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "authHeader": true,
      "apiKey": "gw-secret",
      "models": [
        { "id": "custom-claude", "name": "Custom" }
      ]
    }
  }
}"#,
        )
        .unwrap();
        let r = resolve(None, Some("custom-claude"), None, None, None).unwrap();
        assert_eq!(r.model.id, "custom-claude");
        assert_eq!(r.model.base_url, "https://gw.example.com");
        assert_eq!(r.model.provider, "gateway");
        assert_eq!(r.provider.id(), "gateway");
        // Provider-level authHeader folded in.
        let headers = r.model.headers.as_ref().expect("headers merged");
        assert_eq!(
            headers.get("authorization").map(|s| s.as_str()),
            Some("Bearer gw-secret")
        );
    }

    #[test]
    fn resolved_providers_include_all_authenticated_runtimes() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway-a": {
      "baseUrl": "https://a.example.com",
      "api": "openai-completions",
      "apiKey": "key-a",
      "models": [ { "id": "model-a" } ]
    },
    "gateway-b": {
      "baseUrl": "https://b.example.com",
      "api": "openai-completions",
      "apiKey": "key-b",
      "models": [ { "id": "model-b" } ]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, None, None, None, None).unwrap();
        let ids: Vec<&str> = resolved.providers.iter().map(|p| p.id()).collect();
        // Both runtimes are registered so the harness can hot-switch by
        // `model.provider`; the selected one comes first.
        assert_eq!(resolved.provider.id(), "gateway-a");
        assert_eq!(ids[0], "gateway-a");
        assert!(
            ids.contains(&"gateway-b"),
            "gateway-b runtime missing: {ids:?}"
        );
    }

    #[test]
    fn authenticated_catalog_spans_all_configured_providers() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway-a": {
      "baseUrl": "https://a.example.com",
      "api": "openai-completions",
      "apiKey": "key-a",
      "models": [ { "id": "model-a" } ]
    },
    "gateway-b": {
      "baseUrl": "https://b.example.com",
      "api": "openai-completions",
      "apiKey": "key-b",
      "models": [ { "id": "model-b" } ]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, None, None, None, None).unwrap();
        let catalog = authenticated_catalog(&resolved);
        let providers: std::collections::BTreeSet<&str> =
            catalog.iter().map(|m| m.provider.as_str()).collect();
        assert!(
            providers.contains("gateway-a"),
            "gateway-a models missing: {providers:?}"
        );
        assert!(
            providers.contains("gateway-b"),
            "gateway-b models missing: {providers:?}"
        );
        // Unauthenticated built-ins are excluded (no ANTHROPIC/OPENAI key here).
        assert!(!providers.contains("anthropic"));
        assert!(!providers.contains("openai"));
        // Auth headers merged at build time survive into the selector catalog.
        assert!(catalog.iter().all(|m| m.headers.is_some()));
    }

    #[test]
    fn openai_completions_models_json_is_a_complete_provider_config() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "routeryo": {
      "baseUrl": "https://api.routeryo.com",
      "api": "openai-completions",
      "apiKey": "router-secret",
      "models": [
        {
          "id": "gpt-5.6-sol",
          "name": "GPT 5.6",
          "reasoning": true,
          "contextWindow": 200000,
          "maxTokens": 32768
        }
      ]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, None, None, None, None).unwrap();
        assert_eq!(resolved.model.id, "gpt-5.6-sol");
        assert_eq!(resolved.model.api, kiqi_ai::Api::OpenaiCompletions);
        assert_eq!(resolved.model.provider, "routeryo");
        assert_eq!(resolved.provider.id(), "routeryo");
        assert!(resolved.has_provider_key);
        assert_eq!(
            resolved
                .model
                .headers
                .as_ref()
                .and_then(|headers| headers.get("authorization"))
                .map(String::as_str),
            Some("Bearer router-secret")
        );

        let explicit = resolve(
            Some("routeryo"),
            Some("routeryo/gpt-5.6-sol"),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(explicit.provider.id(), "routeryo");
        assert_eq!(explicit.model.id, "gpt-5.6-sol");
    }

    #[test]
    fn openai_completions_provider_alias_excludes_responses_models() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "chat-gateway": {
      "api": "openai-completions",
      "baseUrl": "https://chat.example.com/v1",
      "apiKey": "chat-key",
      "models": [{"id":"chat-model"}]
    },
    "responses-gateway": {
      "api": "openai-responses",
      "baseUrl": "https://responses.example.com/v1",
      "apiKey": "responses-key",
      "models": [{"id":"responses-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(Some("openai-completions"), None, None, None, None).unwrap();
        assert_eq!(resolved.model.api, kiqi_ai::Api::OpenaiCompletions);
        assert_eq!(resolved.model.provider, "chat-gateway");
        assert_eq!(resolved.model.id, "chat-model");
    }

    #[test]
    fn openai_model_prefix_disambiguates_providers_with_the_same_model_id() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "baseUrl": "https://alpha.example.com",
      "apiKey": "alpha-secret",
      "models": [{"id":"shared-model"}]
    },
    "beta": {
      "api": "openai-completions",
      "baseUrl": "https://beta.example.com",
      "apiKey": "beta-secret",
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let alpha = resolve(None, Some("alpha/shared-model"), None, None, None).unwrap();
        assert_eq!(alpha.provider.id(), "alpha");
        assert_eq!(alpha.model.base_url, "https://alpha.example.com");
        assert_eq!(
            alpha
                .model
                .headers
                .as_ref()
                .and_then(|headers| headers.get("authorization"))
                .map(String::as_str),
            Some("Bearer alpha-secret")
        );

        let beta = resolve(None, Some("beta/shared-model"), None, None, None).unwrap();
        assert_eq!(beta.provider.id(), "beta");
        assert_eq!(beta.model.base_url, "https://beta.example.com");
        assert_eq!(
            beta.model
                .headers
                .as_ref()
                .and_then(|headers| headers.get("authorization"))
                .map(String::as_str),
            Some("Bearer beta-secret")
        );
    }

    #[test]
    fn bare_duplicate_model_id_prefers_the_only_authenticated_provider() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "baseUrl": "https://alpha.example.com",
      "models": [{"id":"shared-model"}]
    },
    "beta": {
      "api": "openai-completions",
      "baseUrl": "https://beta.example.com",
      "apiKey": "beta-secret",
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, Some("shared-model"), None, None, None).unwrap();
        assert_eq!(resolved.model.provider, "beta");
        assert_eq!(resolved.model.base_url, "https://beta.example.com");
    }

    #[test]
    fn bare_duplicate_model_id_rejects_multiple_authenticated_providers() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "apiKey": "alpha-secret",
      "models": [{"id":"shared-model"}]
    },
    "beta": {
      "api": "openai-completions",
      "apiKey": "beta-secret",
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let error = resolve(None, Some("shared-model"), None, None, None).unwrap_err();
        match error {
            ResolveError::AmbiguousModel {
                pattern,
                matches,
                auth_hint,
            } => {
                assert_eq!(pattern, "shared-model");
                assert_eq!(matches, "alpha/shared-model, beta/shared-model");
                assert_eq!(
                    auth_hint,
                    "More than one matching provider is authenticated."
                );
            }
            other => panic!("expected AmbiguousModel, got {other:?}"),
        }
    }

    #[test]
    fn bare_duplicate_model_id_rejects_when_no_provider_is_authenticated() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "models": [{"id":"shared-model"}]
    },
    "beta": {
      "api": "openai-completions",
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let error = resolve(None, Some("shared-model"), None, None, None).unwrap_err();
        assert!(matches!(
            error,
            ResolveError::AmbiguousModel {
                auth_hint: "No matching provider is authenticated.",
                ..
            }
        ));
    }

    #[test]
    fn openai_auth_json_overrides_config_for_both_protocols() {
        let _env = TestEnv::new();
        config::upsert_credential(
            "chat-gateway",
            Credential::ApiKey {
                key: Some("stored-chat-key".into()),
                env: None,
            },
        )
        .unwrap();
        config::upsert_credential(
            "responses-gateway",
            Credential::ApiKey {
                key: Some("stored-responses-key".into()),
                env: None,
            },
        )
        .unwrap();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "chat-gateway": {
      "api": "openai-completions",
      "baseUrl": "https://chat.example.com/v1",
      "apiKey": "configured-chat-key",
      "models": [{"id":"chat-model"}]
    },
    "responses-gateway": {
      "api": "openai-responses",
      "baseUrl": "https://responses.example.com/v1",
      "apiKey": "configured-responses-key",
      "models": [{"id":"responses-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let chat = resolve(
            Some("chat-gateway"),
            Some("chat-gateway/chat-model"),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(chat.has_provider_key);
        assert_eq!(
            chat.model
                .headers
                .as_ref()
                .and_then(|headers| headers.get("authorization"))
                .map(String::as_str),
            Some("Bearer stored-chat-key")
        );

        let responses = resolve(
            Some("responses-gateway"),
            Some("responses-gateway/responses-model"),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(responses.has_provider_key);
        assert_eq!(
            responses
                .model
                .headers
                .as_ref()
                .and_then(|headers| headers.get("authorization"))
                .map(String::as_str),
            Some("Bearer stored-responses-key")
        );
    }

    #[test]
    fn openai_env_does_not_authenticate_custom_providers() {
        let _env = TestEnv::new();
        std::env::set_var(OPENAI_API_KEY_ENV, "official-openai-key");
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "chat-gateway": {
      "api": "openai-completions",
      "baseUrl": "https://chat.example.com/v1",
      "models": [{"id":"chat-model"}]
    },
    "responses-gateway": {
      "api": "openai-responses",
      "baseUrl": "https://responses.example.com/v1",
      "models": [{"id":"responses-model"}]
    },
    "openai-completions": {
      "api": "openai-completions",
      "baseUrl": "https://alias-chat.example.com/v1",
      "models": [{"id":"alias-chat-model"}]
    },
    "openai-responses": {
      "api": "openai-responses",
      "baseUrl": "https://alias-responses.example.com/v1",
      "models": [{"id":"alias-responses-model"}]
    },
    "OpenAI": {
      "api": "openai-responses",
      "baseUrl": "https://case-distinct.example.com/v1",
      "models": [{"id":"case-distinct-model"}]
    }
  }
}"#,
        )
        .unwrap();

        for (provider, model) in [
            ("chat-gateway", "chat-gateway/chat-model"),
            ("responses-gateway", "responses-gateway/responses-model"),
            ("openai-completions", "openai-completions/alias-chat-model"),
            ("openai-responses", "openai-responses/alias-responses-model"),
            ("OpenAI", "OpenAI/case-distinct-model"),
        ] {
            let error = resolve(Some(provider), Some(model), None, None, None).unwrap_err();
            assert!(matches!(error, ResolveError::NoApiKey { .. }));
        }

        let official = resolve(None, None, None, None, None).unwrap();
        assert_eq!(official.provider.id(), "openai");
        assert_eq!(official.model.id, "gpt-6-astra");
        assert!(official.has_provider_key);
    }

    #[test]
    fn cli_api_key_overrides_selected_openai_provider() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "chat-gateway": {
      "api": "openai-completions",
      "baseUrl": "https://chat.example.com/v1",
      "apiKey": "configured-chat-key",
      "models": [{"id":"chat-model"}]
    },
    "responses-gateway": {
      "api": "openai-responses",
      "baseUrl": "https://responses.example.com/v1",
      "apiKey": "configured-responses-key",
      "models": [{"id":"responses-model"}]
    }
  }
}"#,
        )
        .unwrap();

        for (provider, model) in [
            ("chat-gateway", "chat-gateway/chat-model"),
            ("responses-gateway", "responses-gateway/responses-model"),
        ] {
            let resolved =
                resolve(Some(provider), Some(model), None, Some("cli-key"), None).unwrap();
            assert_eq!(resolved.provider.id(), provider);
            assert!(resolved.has_provider_key);
            assert_eq!(
                resolved
                    .model
                    .headers
                    .as_ref()
                    .and_then(|headers| headers.get("authorization"))
                    .map(String::as_str),
                Some("Bearer cli-key")
            );
        }
    }

    #[test]
    fn anthropic_model_prefix_disambiguates_providers_with_the_same_model_id() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "alpha": {
      "api": "anthropic-messages",
      "baseUrl": "https://alpha.example.com",
      "apiKey": "alpha-secret",
      "models": [{"id":"shared-model"}]
    },
    "beta": {
      "api": "anthropic-messages",
      "baseUrl": "https://beta.example.com",
      "authHeader": true,
      "apiKey": "beta-secret",
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let alpha = resolve(None, Some("alpha/shared-model"), None, None, None).unwrap();
        assert_eq!(alpha.provider.id(), "alpha");
        assert_eq!(alpha.model.provider, "alpha");
        assert_eq!(alpha.model.base_url, "https://alpha.example.com");
        assert!(alpha.has_provider_key);
        assert!(alpha.model.headers.as_ref().map_or(true, |headers| {
            headers
                .keys()
                .all(|name| !name.eq_ignore_ascii_case("x-api-key"))
        }));

        let beta = resolve(None, Some("beta/shared-model"), None, None, None).unwrap();
        assert_eq!(beta.provider.id(), "beta");
        assert_eq!(beta.model.provider, "beta");
        assert_eq!(beta.model.base_url, "https://beta.example.com");
        assert_eq!(
            beta.model
                .headers
                .as_ref()
                .and_then(|headers| headers.get("authorization"))
                .map(String::as_str),
            Some("Bearer beta-secret")
        );
    }

    #[test]
    fn trusted_project_defaults_override_global_defaults_for_resolution() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "global": {
      "api": "anthropic-messages",
      "apiKey": "global-secret",
      "models": [{"id":"global-model"}]
    },
    "project": {
      "api": "anthropic-messages",
      "apiKey": "project-secret",
      "models": [{"id":"project-model"}]
    }
  }
}"#,
        )
        .unwrap();
        std::fs::write(
            config::settings_path().unwrap(),
            r#"{"defaultProvider":"global","defaultModel":"global-model"}"#,
        )
        .unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".ki")).unwrap();
        std::fs::write(
            project.path().join(".ki/settings.json"),
            r#"{"defaultProvider":"project","defaultModel":"project-model"}"#,
        )
        .unwrap();

        let trusted = resolve_for_cwd(None, None, None, None, None, project.path(), true).unwrap();
        assert_eq!(trusted.provider.id(), "project");
        assert_eq!(trusted.model.id, "project-model");

        let untrusted =
            resolve_for_cwd(None, None, None, None, None, project.path(), false).unwrap();
        assert_eq!(untrusted.provider.id(), "global");
        assert_eq!(untrusted.model.id, "global-model");
    }

    #[test]
    fn unknown_model_prefix_without_a_raw_match_returns_no_match() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "routeryo": {
      "api": "openai-completions",
      "apiKey": "secret",
      "models": [{"id":"gpt-test"}]
    }
  }
}"#,
        )
        .unwrap();

        let error = resolve(None, Some("misspelled/gpt-test"), None, None, None).unwrap_err();
        assert!(matches!(
            error,
            ResolveError::NoMatch { pattern, .. } if pattern == "misspelled/gpt-test"
        ));
    }

    /// A models.json gateway with `authHeader:true` + `apiKey` is itself an auth
    /// source — it satisfies the `resolve` auth gate WITHOUT any env var, stored
    /// cred, or `--api-key`. This is the "models.json file alone sets up a
    /// third-party endpoint" path. The Bearer folds onto the gateway model only
    /// (built-in claude-* stays Bearer-less), and — with no `--model` — the
    /// default selector picks that gateway model (the only authed one).
    #[test]
    fn models_json_auth_header_satisfies_auth_without_env() {
        let _env = TestEnv::new();
        // No ANTHROPIC_* env, no auth.json — only the models.json gateway.
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "api": "anthropic-messages",
      "authHeader": true,
      "apiKey": "gw-secret",
      "models": [
        { "id": "custom-claude", "contextWindow": 200000, "maxTokens": 8192 }
      ]
    }
  }
}"#,
        )
        .unwrap();
        let r = resolve(None, Some("custom-claude"), None, None, None).unwrap();
        assert_eq!(r.model.id, "custom-claude");
        assert_eq!(r.model.base_url, "https://gw.example.com");
        let headers = r.model.headers.as_ref().expect("bearer folded onto model");
        assert_eq!(
            headers.get("authorization").map(|s| s.as_str()),
            Some("Bearer gw-secret")
        );
    }

    /// The `--api-key` flag wins over a models.json `authHeader:true` gateway
    /// key while preserving the provider's Bearer transport contract.
    /// A `models.json`-only gateway config (no `--model`, no env, no auth.json)
    /// should pick the gateway model by default — mirroring the TS
    /// `findInitialModel` step-4 fallback `availableModels[0]` over the
    /// auth-filtered snapshot. The built-in Anthropic models carry no auth in a
    /// gateway-only setup, so the gateway model is the first (and only)
    /// authenticated model. This is the `ki -p hi` (no `--model`) case.
    #[test]
    fn default_prefers_gateway_when_only_gateway_configured() {
        // TestEnv already holds the shared env_lock for its whole lifetime —
        // don't take it again here (would self-deadlock and poison the mutex).
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "api": "anthropic-messages",
      "authHeader": true,
      "apiKey": "gw-secret",
      "models": [
        { "id": "custom-claude", "contextWindow": 200000, "maxTokens": 8192 }
      ]
    }
  }
}"#,
        )
        .unwrap();
        // No --model (None): the default selector must pick the gateway model,
        // NOT the built-in claude-sonnet-5 (which would carry a foreign Bearer
        // to api.anthropic.com → 401, the bug this fixes).
        let r = resolve(None, None, None, None, None).unwrap();
        assert_eq!(r.model.id, "custom-claude");
        assert_eq!(r.model.base_url, "https://gw.example.com");
        // Gateway model carries the folded Bearer.
        let headers = r.model.headers.as_ref().expect("bearer on gateway model");
        assert_eq!(
            headers.get("authorization").map(|s| s.as_str()),
            Some("Bearer gw-secret")
        );
    }

    #[test]
    fn api_key_flag_honors_models_json_auth_header() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "authHeader": true,
      "apiKey": "gw-secret",
      "models": [ { "id": "custom-claude" } ]
    }
  }
}"#,
        )
        .unwrap();
        let r = resolve(None, Some("custom-claude"), None, Some("flag-key"), None).unwrap();
        assert_eq!(
            r.model
                .headers
                .as_ref()
                .and_then(|h| h.get("authorization"))
                .map(String::as_str),
            Some("Bearer flag-key")
        );
        assert!(!r.has_provider_key);
    }

    /// A models.json gateway with a **bare** `apiKey` (no `authHeader`) is the
    /// `composeApiKeyAuth` arm — it satisfies the `resolve` auth gate WITHOUT
    /// any env var, stored cred, or `--api-key`, routing the resolved key as
    /// the selected provider's default `x-api-key`. It is not copied into model
    /// headers, so a different provider can never inherit it. This is the
    /// default copied-pi `models.json` shape.
    #[test]
    fn models_json_bare_apikey_satisfies_auth_without_env() {
        let _env = TestEnv::new();
        // No ANTHROPIC_* env, no auth.json — only the bare-apiKey models.json gateway.
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "api": "anthropic-messages",
      "apiKey": "gw-secret",
      "models": [
        { "id": "custom-claude", "contextWindow": 200000, "maxTokens": 8192 }
      ]
    }
  }
}"#,
        )
        .unwrap();
        let r = resolve(None, Some("custom-claude"), None, None, None).unwrap();
        assert_eq!(r.model.id, "custom-claude");
        assert_eq!(r.model.base_url, "https://gw.example.com");
        assert!(r.has_provider_key);
        assert!(r.model.headers.as_ref().map_or(true, |headers| {
            headers.keys().all(|name| {
                !name.eq_ignore_ascii_case("x-api-key")
                    && !name.eq_ignore_ascii_case("authorization")
            })
        }));
    }

    /// The bare-`apiKey` x-api-key fold is endpoint-specific: with no `--model`,
    /// the default selector must pick the gateway model (the only authed one),
    // NOT the built-in claude-sonnet-5 — which would carry a gateway x-api-key to
    // api.anthropic.com → 401, the same misrouting the Bearer fold guards
    // against. This is the `ki -p hi` (no `--model`) case for a bare-apiKey
    /// gateway.
    #[test]
    fn default_prefers_gateway_when_only_bare_apikey_configured() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "api": "anthropic-messages",
      "apiKey": "gw-secret",
      "models": [
        { "id": "custom-claude", "contextWindow": 200000, "maxTokens": 8192 }
      ]
    }
  }
}"#,
        )
        .unwrap();
        // No --model (None): the default selector must pick the gateway model.
        let r = resolve(None, None, None, None, None).unwrap();
        assert_eq!(r.model.id, "custom-claude");
        assert_eq!(r.model.base_url, "https://gw.example.com");
        assert!(r.has_provider_key);
    }

    /// A bare `apiKey` that references an unset env var resolves to `None` and
    /// is skipped (mirrors pi `resolveConfigValue` semantics) — the auth gate
    /// falls through to the env/`ki auth login` sources rather than partially
    /// authenticating with an empty key.
    #[test]
    fn models_json_bare_apikey_env_template_resolves() {
        let _env = TestEnv::new();
        // Prime the env var the apiKey references.
        std::env::set_var("KI_TEST_GATEWAY_KEY", "env-resolved-secret");
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "api": "anthropic-messages",
      "apiKey": "$KI_TEST_GATEWAY_KEY",
      "models": [
        { "id": "custom-claude", "contextWindow": 200000, "maxTokens": 8192 }
      ]
    }
  }
}"#,
        )
        .unwrap();
        let r = resolve(None, Some("custom-claude"), None, None, None).unwrap();
        assert!(r.has_provider_key);
        let cfg = config::load_models_config().unwrap();
        let credentials = resolve_anthropic_credentials(&cfg, &config::read_auth().unwrap());
        assert_eq!(
            anthropic_credential_for(&credentials, "gateway"),
            Some(&AnthropicCredential::ProviderKey(
                "env-resolved-secret".into()
            ))
        );
        std::env::remove_var("KI_TEST_GATEWAY_KEY");
    }

    /// `authHeader: true` takes precedence over a bare `apiKey` on the SAME or a
    /// later provider: the Bearer step (3a) runs before the bare-apiKey step
    /// A models.json with BOTH auth shapes — `authHeader:true` and bare
    /// `apiKey` — routes each provider's credential onto ITS OWN models
    /// (per-provider fold, mirroring upstream `composeApiKeyAuth`): the
    /// authHeader provider's key becomes `Authorization: Bearer` on its model,
    /// the bare-apiKey provider's key becomes `x-api-key` on its model. A
    /// copied pi models.json mixing both shapes works end-to-end — no model
    /// ends up unauthenticated because another provider "won" the gate.
    #[test]
    fn auth_header_provider_and_bare_apikey_provider_each_fold_their_own() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "bearer-gw": {
      "baseUrl": "https://bearer.example.com",
      "api": "anthropic-messages",
      "authHeader": true,
      "apiKey": "bearer-secret",
      "models": [ { "id": "bearer-model" } ]
    },
    "xkey-gw": {
      "baseUrl": "https://xkey.example.com",
      "api": "anthropic-messages",
      "apiKey": "xkey-secret",
      "models": [ { "id": "xkey-model" } ]
    }
  }
}"#,
        )
        .unwrap();
        // Both providers satisfy the auth gate together (no env / stored cred
        // needed); the default selector picks the first authed model.
        let r = resolve(None, None, None, None, None).unwrap();
        assert_eq!(r.model.id, "bearer-model");

        // bearer-gw's key folds as Bearer onto bearer-model only.
        let r = resolve(None, Some("bearer-model"), None, None, None).unwrap();
        let h = r.model.headers.as_ref().expect("bearer folded");
        assert_eq!(
            h.get("authorization").map(|s| s.as_str()),
            Some("Bearer bearer-secret")
        );
        assert!(
            h.get("x-api-key").is_none(),
            "authHeader path must not synthesize x-api-key"
        );

        // xkey-gw's bare apiKey becomes only that provider's default key.
        let r2 = resolve(None, Some("xkey-model"), None, None, None).unwrap();
        assert!(r2.has_provider_key);
        assert!(r2.model.headers.as_ref().map_or(true, |headers| {
            headers.keys().all(|name| {
                !name.eq_ignore_ascii_case("x-api-key")
                    && !name.eq_ignore_ascii_case("authorization")
            })
        }));

        // The active provider exposes only its own catalog. Selecting the
        // other provider explicitly creates a separately keyed provider.
        let catalog = available_catalog(&r);
        let ids: Vec<&str> = catalog.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["bearer-model"]);
        let other_catalog = available_catalog(&r2);
        let other_ids: Vec<&str> = other_catalog.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(other_ids, vec!["xkey-model"]);
    }

    /// A copied pi settings.json whose `defaultProvider` names a **models.json
    /// gateway** (not "anthropic") must still honor the saved `defaultModel` —
    /// pi's `findInitialModel` step-3 applies `defaultModelPerProvider`
    /// regardless of provider id. Without this, enabling a second gateway
    /// flips the no-`--model` default to the FIRST authed model in catalog
    /// order, not the user's saved choice.
    #[test]
    fn settings_default_model_honored_for_models_json_provider() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "beta-gw": {
      "baseUrl": "https://beta.example.com",
      "api": "anthropic-messages",
      "apiKey": "beta-secret",
      "models": [ { "id": "beta-model" } ]
    },
    "alpha-gw": {
      "baseUrl": "https://alpha.example.com",
      "api": "anthropic-messages",
      "apiKey": "alpha-secret",
      "models": [ { "id": "alpha-model" } ]
    }
  }
}"#,
        )
        .unwrap();
        // Saved default points at the ALPHA gateway's model even though
        // "beta-gw" is declared first and would win first-authed without the
        // settings arm, matching upstream's Object.entries order.
        std::fs::write(
            config::settings_path().unwrap(),
            r#"{"defaultProvider":"alpha-gw","defaultModel":"alpha-model"}"#,
        )
        .unwrap();
        let r = resolve(None, None, None, None, None).unwrap();
        assert_eq!(r.model.id, "alpha-model");
        // An unknown provider id falls through to first-authed (beta-gw).
        std::fs::write(
            config::settings_path().unwrap(),
            r#"{"defaultProvider":"not-a-provider","defaultModel":"beta-model"}"#,
        )
        .unwrap();
        let r = resolve(None, None, None, None, None).unwrap();
        assert_eq!(r.model.id, "beta-model");
    }

    #[test]
    fn models_json_fallback_preserves_provider_and_model_declaration_order() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "routeryo-copy": {
      "api": "openai-completions",
      "baseUrl": "https://router.example.com/v1",
      "apiKey": "router-key",
      "models": [
        { "id": "gpt-5.6-sol" },
        { "id": "gpt-5.6-terra" }
      ]
    },
    "alpha-gw": {
      "api": "openai-completions",
      "baseUrl": "https://alpha.example.com/v1",
      "apiKey": "alpha-key",
      "models": [ { "id": "alpha-model" } ]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, None, None, None, None).unwrap();
        assert_eq!(resolved.model.provider, "routeryo-copy");
        assert_eq!(resolved.model.id, "gpt-5.6-sol");
    }

    #[test]
    fn native_known_provider_default_beats_first_model_in_array() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "deepseek": {
      "api": "openai-completions",
      "baseUrl": "https://api.deepseek.com",
      "apiKey": "deepseek-key",
      "models": [
        { "id": "deepseek-chat" },
        { "id": "deepseek-v4-pro" }
      ]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, None, None, None, None).unwrap();
        assert_eq!(resolved.model.provider, "deepseek");
        assert_eq!(resolved.model.id, "deepseek-v4-pro");
    }

    /// The `/model` selector catalog (`available_catalog`) is auth-filtered —
    /// it must NOT offer built-in claude-* models that carry no auth headers in
    /// a gateway-only setup (selecting one would fail at request time with
    /// "No API key for provider: anthropic"). Mirrors pi's
    /// `getAvailableSnapshot` filter (`available = all.filter(m =>
    /// configuredProviders.has(m.provider))`): only the gateway model is
    /// loadable, so only it appears in the selector / Ctrl+P cycle.
    #[test]
    fn available_catalog_filters_to_authed_models_in_gateway_only_setup() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "api": "anthropic-messages",
      "apiKey": "gw-secret",
      "models": [
        { "id": "custom-claude", "contextWindow": 200000, "maxTokens": 8192 }
      ]
    }
  }
}"#,
        )
        .unwrap();
        let r = resolve(None, None, None, None, None).unwrap();
        // A bare models.json key is carried by this provider, not model headers.
        assert!(r.has_provider_key);
        let catalog = available_catalog(&r);
        // Exactly one loadable model: the gateway one. The 7 built-in Anthropic
        // models are filtered out.
        let ids: Vec<&str> = catalog.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["custom-claude"],
            "selector must only list authed models"
        );
        // The runtime provider and selector share the same provider-isolated
        // catalog, so models from another identity cannot be routed through it.
        assert_eq!(r.provider.models().len(), catalog.len());
    }

    /// On the x-api-key path (`--api-key`/auth.json/`ANTHROPIC_API_KEY`), the
    /// provider's default key attaches to EVERY model out-of-band — so the
    /// catalog filter keeps the full list (all models are loadable).
    #[test]
    fn available_catalog_keeps_all_models_on_provider_key_path() {
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_API_KEY_ENV, "k");
        let r = resolve(None, None, None, None, None).unwrap();
        assert!(r.has_provider_key);
        let catalog = available_catalog(&r);
        assert_eq!(catalog.len(), r.provider.models().len());
        assert!(catalog.iter().any(|m| m.id == DEFAULT_MODEL_ID));
    }

    #[test]
    fn unconfigured_resolved_is_a_usable_placeholder() {
        let resolved = unconfigured_resolved(Some("dark".into()));
        assert_eq!(resolved.model.id, UNCONFIGURED_MODEL_ID);
        assert_eq!(resolved.model.provider, UNCONFIGURED_PROVIDER_ID);
        assert!(is_unconfigured(&resolved.model));
        assert_eq!(resolved.theme.as_deref(), Some("dark"));
        // No models means the /model selector has nothing to offer, and the
        // placeholder never collides with a real provider's catalog.
        assert!(authenticated_catalog(&resolved).is_empty());
        assert!(resolved.providers.iter().all(|p| p.models().is_empty()));
        assert_eq!(resolved.provider.id(), UNCONFIGURED_PROVIDER_ID);
    }

    #[test]
    fn is_unconfigured_rejects_real_models() {
        let real = Model::new(
            "claude-opus-5",
            "Opus",
            kiqi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        assert!(!is_unconfigured(&real));
        // An id match with a different provider is not the placeholder.
        let lookalike = Model::new(
            UNCONFIGURED_MODEL_ID,
            "x",
            kiqi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        assert!(!is_unconfigured(&lookalike));
    }

    #[tokio::test]
    async fn unconfigured_provider_answers_with_configuration_guidance() {
        use kiqi_ai::Context;
        let resolved = unconfigured_resolved(None);
        let opts = SimpleStreamOptions::default();
        let mut stream = resolved
            .provider
            .stream_simple(&resolved.model, &Context::default(), &opts)
            .await;
        let mut saw_error = None;
        while let Some(event) = stream.next().await {
            if let kiqi_ai::AssistantMessageEvent::Error { error, .. } = event {
                saw_error = Some(error.error_message.clone().unwrap_or_default());
                break;
            }
        }
        let message = saw_error.expect("placeholder must yield an Error event");
        assert!(message.contains("ki configure"), "{message}");
        assert!(message.contains("No provider is configured"), "{message}");
    }

    // ---- Granted extension provider catalog + auth predicate ----

    /// Install a fake wasm component plus a `provider` grant under `root`, the
    /// shape `ki extension grant <id> provider` writes. The bytes are never
    /// instantiated here (no host is built); only the catalog path is exercised.
    #[cfg(feature = "extension-wasm")]
    fn install_extension_provider(root: &Path, id: &str, models: &[(&str, &str)]) {
        use kiqi_host::grants::{
            Grants, ProviderEndpoint, ProviderGrant, ProviderModel, ProviderProfile,
            PROVIDER_MAX_HANDLES_DEFAULT, PROVIDER_MAX_RESPONSE_BYTES_DEFAULT,
            PROVIDER_TIMEOUT_DEFAULT_SECONDS,
        };

        let dir = root.join("extensions").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("extension.wasm"), b"not-a-real-component").unwrap();
        let grant = ProviderGrant {
            providers: vec![ProviderProfile {
                id: id.to_string(),
                display_name: String::new(),
                models: models
                    .iter()
                    .map(|(model_id, api)| ProviderModel {
                        id: (*model_id).to_string(),
                        api: (*api).to_string(),
                        base_url: "https://api.example.test/v1".to_string(),
                        reasoning: false,
                        input: vec!["text".to_string()],
                    })
                    .collect(),
                endpoint: ProviderEndpoint {
                    scheme: "https".to_string(),
                    host: "api.example.test".to_string(),
                    path_prefix: "/v1".to_string(),
                    methods: vec!["POST".to_string()],
                },
                credential: None,
                auth: None,
                timeout_seconds: PROVIDER_TIMEOUT_DEFAULT_SECONDS,
                max_response_bytes: PROVIDER_MAX_RESPONSE_BYTES_DEFAULT,
                max_handles: PROVIDER_MAX_HANDLES_DEFAULT,
            }],
        };
        let mut grants = Grants::default();
        grants.grant_provider(grant).unwrap();
        Grants::save_for_at(root, id, &grants).unwrap();
    }

    /// Install the real conformance guest under `root/extensions/conformance` with
    /// a provider grant covering `models`. Unlike [`install_extension_provider`]
    /// these bytes are a loadable component, so a test may drive a request.
    #[cfg(feature = "extension-wasm")]
    fn install_real_conformance_provider(root: &Path, models: &[&str]) -> PathBuf {
        use kiqi_host::grants::{
            Grants, ProviderEndpoint, ProviderGrant, ProviderModel, ProviderProfile,
            PROVIDER_MAX_HANDLES_DEFAULT, PROVIDER_MAX_RESPONSE_BYTES_DEFAULT,
        };

        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let built =
            crate::extension_cli::build_guest(&workspace.join("extensions/wasm/kiqi-conformance"))
                .expect("build the conformance guest");
        let dir = root.join("extensions/conformance");
        std::fs::create_dir_all(&dir).unwrap();
        let wasm = dir.join("extension.wasm");
        std::fs::copy(&built, &wasm).expect("install extension.wasm");
        let grant = ProviderGrant {
            providers: vec![ProviderProfile {
                id: "conformance".to_string(),
                display_name: "Conformance Provider".to_string(),
                models: models
                    .iter()
                    .map(|id| ProviderModel {
                        id: (*id).to_string(),
                        api: "openai-completions".to_string(),
                        base_url: "https://api.example.test/v1".to_string(),
                        reasoning: true,
                        input: vec!["text".to_string()],
                    })
                    .collect(),
                endpoint: ProviderEndpoint {
                    scheme: "https".to_string(),
                    host: "api.example.test".to_string(),
                    path_prefix: "/v1".to_string(),
                    methods: vec!["POST".to_string()],
                },
                credential: None,
                auth: None,
                timeout_seconds: 30,
                max_response_bytes: PROVIDER_MAX_RESPONSE_BYTES_DEFAULT,
                max_handles: PROVIDER_MAX_HANDLES_DEFAULT,
            }],
        };
        let mut grants = Grants::default();
        grants.grant_provider(grant).expect("valid provider grant");
        Grants::save_for_at(root, "conformance", &grants).unwrap();
        wasm
    }

    /// Install a component directory with no capability grant at all.
    #[cfg(feature = "extension-wasm")]
    fn install_extension_without_grants(root: &Path, id: &str) {
        use kiqi_host::grants::Grants;
        let dir = root.join("extensions").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("extension.wasm"), b"not-a-real-component").unwrap();
        Grants::save_for_at(root, id, &Grants::default()).unwrap();
    }

    #[cfg(feature = "extension-wasm")]
    #[test]
    fn granted_extension_model_is_selectable_and_visible() {
        let env = TestEnv::new();
        let root = env._tmp.path();
        install_extension_provider(root, "acme", &[("acme-1", "openai-completions")]);

        // No built-in key exists: the grant alone authorises the model.
        let resolved = resolve_with_settings(
            None,
            Some("acme/acme-1"),
            None,
            None,
            None,
            settings::Settings::default(),
            Some(root),
        )
        .unwrap();
        assert_eq!(resolved.model.id, "acme-1");
        assert_eq!(resolved.model.provider, "acme");
        assert_eq!(resolved.provider.id(), "acme");
        assert!(!resolved.has_provider_key);
        assert!(resolved.granted_extension_providers.contains("acme"));
        assert!(available_catalog(&resolved)
            .iter()
            .any(|model| model.id == "acme-1"));
        assert!(authenticated_catalog(&resolved)
            .iter()
            .any(|model| model.provider == "acme"));

        // A built-in credential (here `--api-key`) must never be folded onto a
        // extension model: the host-held credential is injected only by
        // `provider-http` for a matching request.
        let keyed = resolve_with_settings(
            None,
            Some("acme/acme-1"),
            None,
            Some("cli-secret"),
            None,
            settings::Settings::default(),
            Some(root),
        )
        .unwrap();
        assert!(
            keyed
                .model
                .headers
                .as_ref()
                .map_or(true, |headers| headers.is_empty()),
            "extension model must not carry the CLI key: {:?}",
            keyed.model.headers
        );

        // With no `--model` and no other auth, the granted model is the only
        // authenticated one, so default selection picks it.
        let default = resolve_with_settings(
            None,
            None,
            None,
            None,
            None,
            settings::Settings::default(),
            Some(root),
        )
        .unwrap();
        assert_eq!(default.model.provider, "acme");
        assert_eq!(default.model.id, "acme-1");

        // `--provider` accepts the extension id too.
        let by_provider = resolve_with_settings(
            Some("acme"),
            None,
            None,
            None,
            None,
            settings::Settings::default(),
            Some(root),
        )
        .unwrap();
        assert_eq!(by_provider.model.provider, "acme");
    }

    /// `/model` is built from `resolved.providers`, so a reload must keep the
    /// granted extension adapter in that list. Adapters are per-resolve and lazy:
    /// resolving compiles nothing, and a second resolve never reuses the first
    /// adapter's host.
    #[cfg(feature = "extension-wasm")]
    #[tokio::test]
    async fn granted_extension_adapters_survive_reload_and_compile_lazily() {
        let _lock = crate::wasm_provider::HOST_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let env = TestEnv::new();
        let root = env._tmp.path();
        install_real_conformance_provider(root, &["conformance-stream"]);
        crate::wasm_provider::reset_hosts_built();

        let resolve = || {
            resolve_with_settings(
                None,
                Some("conformance/conformance-stream"),
                None,
                None,
                None,
                settings::Settings::default(),
                Some(root),
            )
            .unwrap()
        };
        let first = resolve();
        let second = resolve();
        assert_eq!(
            crate::wasm_provider::hosts_built(),
            0,
            "resolve must not compile a provider guest"
        );
        assert!(
            !Arc::ptr_eq(&first.provider, &second.provider),
            "each resolve owns its own adapter"
        );
        for resolved in [&first, &second] {
            assert!(
                authenticated_catalog(resolved).iter().any(
                    |model| model.provider == "conformance" && model.id == "conformance-stream"
                ),
                "the /model selector must see the granted model after a reload"
            );
        }

        let opts = SimpleStreamOptions::default();
        let mut stream = first
            .provider
            .stream_simple(&first.model, &Context::default(), &opts)
            .await;
        while stream.next().await.is_some() {}
        assert_eq!(
            crate::wasm_provider::hosts_built(),
            1,
            "the first request compiles its own host"
        );

        // A reload after a successful request still compiles nothing.
        let third = resolve();
        assert_eq!(crate::wasm_provider::hosts_built(), 1);
        assert!(authenticated_catalog(&third)
            .iter()
            .any(|model| model.id == "conformance-stream"));

        // The adapter from the second resolve had no host; its first request
        // builds a fresh one instead of borrowing the first adapter's.
        let mut stream = second
            .provider
            .stream_simple(&second.model, &Context::default(), &opts)
            .await;
        while stream.next().await.is_some() {}
        assert_eq!(
            crate::wasm_provider::hosts_built(),
            2,
            "a second resolve must not reuse the first adapter's host"
        );
    }

    #[cfg(feature = "extension-wasm")]
    #[test]
    fn ungranted_extension_model_is_filtered_out_exactly_as_today() {
        let env = TestEnv::new();
        let root = env._tmp.path();
        install_extension_without_grants(root, "acme");

        assert!(granted_extension_models(root).is_empty());
        let err = resolve_with_settings(
            None,
            Some("acme-1"),
            None,
            None,
            None,
            settings::Settings::default(),
            Some(root),
        )
        .expect_err("an ungranted extension model must not enter the catalog");
        assert!(matches!(err, ResolveError::NoMatch { .. }), "{err:?}");

        // Without any auth the gate is unchanged.
        let err = resolve_with_settings(
            None,
            None,
            None,
            None,
            None,
            settings::Settings::default(),
            Some(root),
        )
        .expect_err("no credential means no runnable model");
        assert!(matches!(err, ResolveError::NoApiKey { .. }), "{err:?}");
    }

    #[cfg(feature = "extension-wasm")]
    #[test]
    fn extension_provider_id_collisions_are_refused() {
        let env = TestEnv::new();
        let root = env._tmp.path();
        install_extension_provider(root, "anthropic", &[("shadow", "anthropic-messages")]);
        let err = resolve_with_settings(
            None,
            None,
            None,
            None,
            None,
            settings::Settings::default(),
            Some(root),
        )
        .expect_err("an extension must not shadow a built-in provider id");
        assert!(
            matches!(
                err,
                ResolveError::ProviderIdCollision { ref id, .. } if id == "anthropic"
            ),
            "{err:?}"
        );

        // Same rule for a models.json provider id.
        std::fs::remove_dir_all(root.join("extensions").join("anthropic")).unwrap();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "gateway-secret",
      "models": [{"id":"gateway-1"}]
    }
  }
}"#,
        )
        .unwrap();
        install_extension_provider(root, "gateway", &[("shadow", "anthropic-messages")]);
        let err = resolve_with_settings(
            None,
            None,
            None,
            None,
            None,
            settings::Settings::default(),
            Some(root),
        )
        .expect_err("an extension must not shadow a models.json provider id");
        assert!(
            matches!(
                err,
                ResolveError::ProviderIdCollision { ref id, .. } if id == "gateway"
            ),
            "{err:?}"
        );
    }

    #[cfg(feature = "extension-wasm")]
    #[test]
    fn catalog_merge_order_is_builtin_then_models_json_then_extensions() {
        let env = TestEnv::new();
        let root = env._tmp.path();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "gateway-secret",
      "models": [{"id":"gateway-1"}]
    }
  }
}"#,
        )
        .unwrap();
        install_extension_provider(root, "acme", &[("acme-1", "anthropic-messages")]);

        let cfg = config::load_models_config().unwrap();
        let granted = granted_extension_models(root);
        let catalog = catalog_with_granted_extensions(&cfg, &granted).unwrap();

        assert_eq!(catalog[0].provider, "anthropic");
        let gateway = catalog
            .iter()
            .position(|model| model.provider == "gateway")
            .expect("models.json provider is present");
        let extension = catalog
            .iter()
            .position(|model| model.provider == "acme")
            .expect("granted extension provider is present");
        assert!(gateway < extension, "models.json must precede extensions");
        assert_eq!(extension, catalog.len() - 1, "extensions are appended last");
    }
}
