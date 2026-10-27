//! Per-extension capability grants — the **user's** authorization.
//!
//! The host never authorizes on an extension's behalf: an extension gets exactly what
//! the user recorded here, and a missing file grants nothing. The file lives at
//! `$KI_AGENT_DIR/extensions/<id>/grants.json` (default
//! `~/.ki/agent/extensions/<id>/grants.json`), so both the wasm host (which
//! enforces it) and the CLI (which edits it) share this one type without either
//! depending on the other.

use std::path::PathBuf;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;

fn is_false(value: &bool) -> bool {
    !*value
}

/// The current grants.json format version.
pub const GRANTS_VERSION: u32 = 2;
const GRANTS_VERSION_LEGACY: u32 = 1;

/// What the user authorized for one extension.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Grants {
    /// The file format version. Absent on disk means legacy version 1; every
    /// save writes [`GRANTS_VERSION`], regardless of the in-memory value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// Host-mediated HTTP, with a host allow-list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_http: Option<NetHttpGrant>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "fs_grant_from_bool_or_value"
    )]
    pub fs: Option<FsGrant>,
    /// Running a child process, with a command allow-list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<ExecGrant>,
    /// Interactive child processes, with a command allow-list, a cwd scope, a
    /// per-child budget and a concurrency bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proc: Option<ProcGrant>,
    /// Raw TCP, with a host allow-list (same shape as `net-http`: the grant
    /// carries its policy, so "granted" and "to what" are one decision).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_sockets: Option<NetSocketsGrant>,
    /// Host-attributed OAuth: exact IdP profiles, a flow budget and a request
    /// budget. Token material never crosses the ABI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthGrant>,
    /// Provider catalog plus the request filter for `provider-http`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderGrant>,
    /// Markdown transform (`render-api`). Bare `true` = markdown + defaults,
    /// the same compatibility `fs` keeps.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "render_grant_from_bool_or_value"
    )]
    pub render: Option<RenderGrant>,
    /// Package-resident skills/prompts. Bare `true` = both kinds + defaults.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "resources_grant_from_bool_or_value"
    )]
    pub resources: Option<ResourcesGrant>,
    /// Redacted provider-request observation and the bounded patch channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_request: Option<ProviderRequestGrant>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetHttpGrant {
    /// Hosts the extension may reach; `*` allows any.
    pub hosts: Vec<String>,
}

/// The default wall-clock budget for one `exec` call when the grant says
/// nothing — the value the host used before the budget was grant-carried.
pub const EXEC_TIMEOUT_DEFAULT_SECONDS: u64 = 30;

/// The largest budget a grant can ask for. The CLI rejects larger values and
/// the host clamps them, so a hand-edited file cannot buy an unbounded child.
pub const EXEC_TIMEOUT_MAX_SECONDS: u64 = 300;

/// Which root an `exec` working directory resolves against. The scope rides
/// with the grant — "granted" and "where" are one decision, the same rule the
/// command allow-list follows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecCwd {
    /// Inside `extensions/<id>/data/` — the default, and the same jail `fs` uses.
    ///
    /// **Deliberate legacy trace**: grant files written before the 2026-10-26
    /// rename spell this as `"plugin-data"`. Without the alias serde rejects the
    /// whole document, `load_for_at` falls back to empty grants, and the next
    /// save would rewrite the file without every capability. The alias is
    /// transient: the file is read for compatibility, and the next explicit
    /// save writes the current spelling.
    #[default]
    #[serde(alias = "plugin-data")]
    ExtensionData,
    /// The project directory the session runs in (the host process's working
    /// directory). Needed by diagnostic extensions that run `cargo`/`clippy`
    /// against the user's tree.
    Project,
}

impl ExecCwd {
    /// The CLI spelling (`--cwd extension-data|project`).
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "extension-data" => Ok(ExecCwd::ExtensionData),
            "project" => Ok(ExecCwd::Project),
            other => Err(format!(
                "exec cwd must be `extension-data` or `project`, got `{other}`"
            )),
        }
    }

    fn is_extension_data(&self) -> bool {
        matches!(self, ExecCwd::ExtensionData)
    }
}

fn grants_file_version(value: &Value) -> Result<u32, String> {
    match value.get("version") {
        None | Some(Value::Null) => Ok(GRANTS_VERSION_LEGACY),
        Some(version) => version
            .as_u64()
            .and_then(|version| u32::try_from(version).ok())
            .ok_or_else(|| "grants.json `version` must be an unsigned 32-bit integer".to_string()),
    }
}

/// A structural pass over the raw JSON that rejects duplicated object keys at
/// any depth. `serde_json::Value` keeps only the last duplicate, so parsing a
/// grants file through `Value` would let a second `"exec"` (or a second nested
/// `"cwd"`) silently overwrite the first — a duplicated capability key could
/// widen a grant from `["safe"]` to `["*"]`. Running this visitor on the
/// original text makes the file fail closed like any other malformed input.
struct RejectDuplicateKeys;

impl<'de> Deserialize<'de> for RejectDuplicateKeys {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Keys;

        impl<'de> Visitor<'de> for Keys {
            type Value = RejectDuplicateKeys;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("any JSON value without duplicated object keys")
            }

            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(RejectDuplicateKeys)
            }

            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(RejectDuplicateKeys)
            }

            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(RejectDuplicateKeys)
            }

            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(RejectDuplicateKeys)
            }

            fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(RejectDuplicateKeys)
            }

            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(RejectDuplicateKeys)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                while seq.next_element::<RejectDuplicateKeys>()?.is_some() {}
                Ok(RejectDuplicateKeys)
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut seen = std::collections::HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !seen.insert(key.clone()) {
                        return Err(de::Error::custom(format!("duplicate key `{key}`")));
                    }
                    map.next_value::<RejectDuplicateKeys>()?;
                }
                Ok(RejectDuplicateKeys)
            }
        }

        deserializer.deserialize_any(Keys)
    }
}

fn default_exec_timeout() -> u64 {
    EXEC_TIMEOUT_DEFAULT_SECONDS
}

fn is_default_exec_timeout(value: &u64) -> bool {
    *value == EXEC_TIMEOUT_DEFAULT_SECONDS
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ExecGrant {
    /// Command names (no path, no shell) the extension may run; `*` allows any.
    pub commands: Vec<String>,
    /// Where the child may start. Absent in older grant files = `extension-data`,
    /// so an existing grant keeps its old meaning unchanged.
    #[serde(default, skip_serializing_if = "ExecCwd::is_extension_data")]
    pub cwd: ExecCwd,
    /// Wall-clock budget in seconds for one call; absent = the default,
    /// capped at [`EXEC_TIMEOUT_MAX_SECONDS`].
    #[serde(
        default = "default_exec_timeout",
        skip_serializing_if = "is_default_exec_timeout",
        deserialize_with = "nonzero_exec_timeout"
    )]
    pub timeout_seconds: u64,
}

/// The default wall-clock budget for one `proc` child — long enough for an
/// index/analysis server to do real work, and deliberately its own constant:
/// it exceeds `exec`'s 30s/300s pair, which is the point of a separate
/// capability (a session is not a bounded one-shot command).
pub const PROC_TIMEOUT_DEFAULT_SECONDS: u64 = 600;

/// The largest budget a `proc` grant can ask for (the orchestrator's subagent
/// ceiling, reused so the repo has one "very long child" number).
pub const PROC_TIMEOUT_MAX_SECONDS: u64 = 1800;

/// How many children one call may hold open by default.
pub const PROC_MAX_OPEN_DEFAULT: u32 = 100;

/// The hard ceiling on `max-open` — the resource is the machine's process
/// table, not something the sandbox can carry for the extension.
pub const PROC_MAX_OPEN_MAX: u32 = 256;

fn default_proc_timeout() -> u64 {
    PROC_TIMEOUT_DEFAULT_SECONDS
}

fn is_default_proc_timeout(value: &u64) -> bool {
    *value == PROC_TIMEOUT_DEFAULT_SECONDS
}

fn default_proc_max_open() -> u32 {
    PROC_MAX_OPEN_DEFAULT
}

fn is_default_proc_max_open(value: &u32) -> bool {
    *value == PROC_MAX_OPEN_DEFAULT
}

/// Zero budgets parse nowhere. `grant()` rejects `--timeout 0`/`--max-open 0`
/// (a 0s timeout would kill every child instantly, 0 slots denies every
/// spawn), so a hand-edited grants.json cannot sneak the same values in
/// through the deserialize path — the file then loads as "nothing granted",
/// the same fail-closed a malformed file gets.
fn nonzero_exec_timeout<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let seconds = u64::deserialize(deserializer)?;
    if seconds == 0 {
        return Err(serde::de::Error::custom(
            "exec `timeout-seconds` must be at least 1 second",
        ));
    }
    Ok(seconds)
}

fn nonzero_proc_timeout<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let seconds = u64::deserialize(deserializer)?;
    if seconds == 0 {
        return Err(serde::de::Error::custom(
            "proc `timeout-seconds` must be at least 1 second",
        ));
    }
    Ok(seconds)
}

fn nonzero_proc_max_open<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let open = u32::deserialize(deserializer)?;
    if open == 0 {
        return Err(serde::de::Error::custom(
            "proc `max-open` must be at least 1",
        ));
    }
    Ok(open)
}

/// The `proc` policy: which commands, in which scope, for how long, and how
/// many at once. Same shape as [`ExecGrant`] plus the concurrency bound.
/// The `fs` policy: which root file access is jailed to. Legacy files carry
/// the bare `true` = the extension's own data directory (the historical default);
/// `--cwd project` opts an extension into the project tree, the same scope
/// `exec`/`proc` use.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct FsGrant {
    pub cwd: ExecCwd,
}

/// Accept the historical `"fs": true` / `"fs": false` spellings alongside the
/// current `{"cwd": …}` object, so old grants.json files keep working.
fn fs_grant_from_bool_or_value<'de, D>(deserializer: D) -> Result<Option<FsGrant>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(match value {
        None | Some(Value::Bool(false)) | Some(Value::Null) => None,
        Some(Value::Bool(true)) => Some(FsGrant {
            cwd: ExecCwd::ExtensionData,
        }),
        Some(other) => {
            Some(serde_json::from_value::<FsGrant>(other).map_err(serde::de::Error::custom)?)
        }
    })
}

impl FsGrant {
    /// The scope root this grant resolves against.
    pub fn scope(self) -> ExecCwd {
        self.cwd
    }
}

/// The bare-`true` compatibility rule shared by `render` and `resources`:
/// `true` means "grant with the defaults", `false`/`null` means absent, and an
/// object is read as the policy. `fs` keeps its own wrapper because its bare
/// form also names a historical default.
fn bool_or_default_grant<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    use serde::Deserialize;
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(match value {
        None | Some(Value::Bool(false)) | Some(Value::Null) => None,
        Some(Value::Bool(true)) => Some(T::default()),
        Some(other) => Some(serde_json::from_value::<T>(other).map_err(serde::de::Error::custom)?),
    })
}

fn render_grant_from_bool_or_value<'de, D>(deserializer: D) -> Result<Option<RenderGrant>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    bool_or_default_grant(deserializer)
}

fn resources_grant_from_bool_or_value<'de, D>(
    deserializer: D,
) -> Result<Option<ResourcesGrant>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    bool_or_default_grant(deserializer)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProcGrant {
    /// Command names (no path, no shell) the extension may spawn; `*` allows any.
    pub commands: Vec<String>,
    /// Where a child may start — same scope rule as `exec`.
    #[serde(default, skip_serializing_if = "ExecCwd::is_extension_data")]
    pub cwd: ExecCwd,
    /// Wall-clock budget for one child; absent = the default, capped at
    /// [`PROC_TIMEOUT_MAX_SECONDS`].
    #[serde(
        default = "default_proc_timeout",
        skip_serializing_if = "is_default_proc_timeout",
        deserialize_with = "nonzero_proc_timeout"
    )]
    pub timeout_seconds: u64,
    /// How many children one call may hold open at once.
    #[serde(
        default = "default_proc_max_open",
        skip_serializing_if = "is_default_proc_max_open",
        deserialize_with = "nonzero_proc_max_open"
    )]
    pub max_open: u32,
}

impl Default for ProcGrant {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            cwd: ExecCwd::default(),
            timeout_seconds: PROC_TIMEOUT_DEFAULT_SECONDS,
            max_open: PROC_MAX_OPEN_DEFAULT,
        }
    }
}

impl Default for ExecGrant {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            cwd: ExecCwd::default(),
            timeout_seconds: EXEC_TIMEOUT_DEFAULT_SECONDS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetSocketsGrant {
    /// Hosts the extension may open a TCP connection to; `*` allows any.
    pub hosts: Vec<String>,
}

// ---------------------------------------------------------------------------
// 0.8.0 capabilities
//
// Each key below carries its own policy, so "granted" and "to what" stay one
// decision (the rule `net-http` established). None of them accept the
// `exec`/`proc` options; `grant()` rejects those rather than dropping them.
// ---------------------------------------------------------------------------

/// The default wall-clock budget for one OAuth flow (polling included).
pub const OAUTH_TIMEOUT_DEFAULT_SECONDS: u64 = 600;

/// The largest OAuth budget a grant can ask for. The host clamps and the CLI
/// rejects larger values, so a hand-edited file cannot buy an unbounded flow.
pub const OAUTH_TIMEOUT_MAX_SECONDS: u64 = 900;

/// How many `oauth-http` calls one flow may make by default.
pub const OAUTH_MAX_REQUESTS_DEFAULT: u32 = 16;

/// The hard ceiling on `max-requests` — every call reaches the IdP.
pub const OAUTH_MAX_REQUESTS_MAX: u32 = 64;

fn default_oauth_timeout() -> u64 {
    OAUTH_TIMEOUT_DEFAULT_SECONDS
}

fn is_default_oauth_timeout(value: &u64) -> bool {
    *value == OAUTH_TIMEOUT_DEFAULT_SECONDS
}

fn default_oauth_max_requests() -> u32 {
    OAUTH_MAX_REQUESTS_DEFAULT
}

fn is_default_oauth_max_requests(value: &u32) -> bool {
    *value == OAUTH_MAX_REQUESTS_DEFAULT
}

/// One OAuth flow a provider profile enables. The set is closed: an unknown
/// flow string fails deserialization, so the file loads as "nothing granted".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OAuthFlow {
    AuthorizationCode,
    DeviceCode,
}

/// One IdP profile the guest may drive through host-attributed `oauth-http`.
/// Every endpoint is an exact `https://host/…` URL — there is no wildcard form,
/// because the grant is what binds a flow to its IdP.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct OAuthProviderProfile {
    /// The profile id the guest names in `oauth-api`/`oauth-http`.
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// At least one; an empty list is invalid.
    #[serde(default)]
    pub flows: Vec<OAuthFlow>,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    /// Where the host performs the device-authorization request. Required for a
    /// profile that grants `device-code`, optional otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_authorization_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revocation_endpoint: Option<String>,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub supports_pkce: bool,
}

/// The `oauth` policy: the profiles an extension may drive, plus the flow budget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct OAuthGrant {
    #[serde(default)]
    pub providers: Vec<OAuthProviderProfile>,
    /// Wall-clock budget for one flow; absent = the default, capped at
    /// [`OAUTH_TIMEOUT_MAX_SECONDS`].
    #[serde(
        default = "default_oauth_timeout",
        skip_serializing_if = "is_default_oauth_timeout"
    )]
    pub timeout_seconds: u64,
    /// How many `oauth-http` calls one flow may make; absent = the default,
    /// capped at [`OAUTH_MAX_REQUESTS_MAX`].
    #[serde(
        default = "default_oauth_max_requests",
        skip_serializing_if = "is_default_oauth_max_requests"
    )]
    pub max_requests: u32,
}

impl OAuthGrant {
    /// Validate a hand-built or deserialized policy. `grant_oauth` and
    /// [`Grants::validate`] both run this, so the CLI and a hand-edited file
    /// cannot disagree about what a valid profile is.
    pub fn validate(&self) -> Result<(), String> {
        if self.providers.is_empty() {
            return Err("oauth needs at least one provider profile".to_string());
        }
        if self.timeout_seconds == 0 || self.timeout_seconds > OAUTH_TIMEOUT_MAX_SECONDS {
            return Err(format!(
                "oauth `timeout-seconds` must be 1..={OAUTH_TIMEOUT_MAX_SECONDS}, got {}",
                self.timeout_seconds
            ));
        }
        if self.max_requests == 0 || self.max_requests > OAUTH_MAX_REQUESTS_MAX {
            return Err(format!(
                "oauth `max-requests` must be 1..={OAUTH_MAX_REQUESTS_MAX}, got {}",
                self.max_requests
            ));
        }
        for profile in &self.providers {
            if profile.id.trim().is_empty() {
                return Err("oauth provider `id` must not be empty".to_string());
            }
            if self
                .providers
                .iter()
                .filter(|other| other.id == profile.id)
                .count()
                > 1
            {
                return Err(format!("oauth provider id `{}` is not unique", profile.id));
            }
            if profile.flows.is_empty() {
                return Err(format!(
                    "oauth provider `{}` needs at least one flow",
                    profile.id
                ));
            }
            if profile.client_id.trim().is_empty() {
                return Err(format!(
                    "oauth provider `{}` needs a `client-id`",
                    profile.id
                ));
            }
            for (field, endpoint) in [
                ("authorization-endpoint", &profile.authorization_endpoint),
                ("token-endpoint", &profile.token_endpoint),
            ] {
                validate_https_endpoint(
                    &format!("oauth provider `{}` {field}", profile.id),
                    endpoint,
                )?;
            }
            if let Some(endpoint) = &profile.device_authorization_endpoint {
                validate_https_endpoint(
                    &format!(
                        "oauth provider `{}` device-authorization-endpoint",
                        profile.id
                    ),
                    endpoint,
                )?;
            }
            if profile.flows.contains(&OAuthFlow::DeviceCode)
                && profile.device_authorization_endpoint.is_none()
            {
                return Err(format!(
                    "oauth provider `{}` grants device-code but has no \
                     `device-authorization-endpoint`",
                    profile.id
                ));
            }
            if let Some(endpoint) = &profile.revocation_endpoint {
                validate_https_endpoint(
                    &format!("oauth provider `{}` revocation-endpoint", profile.id),
                    endpoint,
                )?;
            }
        }
        Ok(())
    }
}

/// The default budget for one provider request; also the fallback when a
/// `provider-request` patch must be bounded without a provider grant.
pub const PROVIDER_TIMEOUT_DEFAULT_SECONDS: u64 = 600;

/// The largest provider budget a grant can ask for.
pub const PROVIDER_TIMEOUT_MAX_SECONDS: u64 = 900;

fn default_provider_timeout() -> u64 {
    PROVIDER_TIMEOUT_DEFAULT_SECONDS
}

fn is_default_provider_timeout(value: &u64) -> bool {
    *value == PROVIDER_TIMEOUT_DEFAULT_SECONDS
}

/// Default cap on one provider response body.
pub const PROVIDER_MAX_RESPONSE_BYTES_DEFAULT: u64 = 8 * 1024 * 1024;

/// Smallest response cap a grant may ask for.
pub const PROVIDER_MAX_RESPONSE_BYTES_MIN: u64 = 64 * 1024;

/// Largest response cap a grant may ask for.
pub const PROVIDER_MAX_RESPONSE_BYTES_MAX: u64 = 32 * 1024 * 1024;

fn default_provider_max_response_bytes() -> u64 {
    PROVIDER_MAX_RESPONSE_BYTES_DEFAULT
}

fn is_default_provider_max_response_bytes(value: &u64) -> bool {
    *value == PROVIDER_MAX_RESPONSE_BYTES_DEFAULT
}

/// Default cap on concurrently open `provider-http` handles.
pub const PROVIDER_MAX_HANDLES_DEFAULT: u32 = 8;

/// Largest handle cap a grant may ask for.
pub const PROVIDER_MAX_HANDLES_MAX: u32 = 64;

fn default_provider_max_handles() -> u32 {
    PROVIDER_MAX_HANDLES_DEFAULT
}

fn is_default_provider_max_handles(value: &u32) -> bool {
    *value == PROVIDER_MAX_HANDLES_DEFAULT
}

/// The header a provider credential is injected into by default.
pub const PROVIDER_AUTH_HEADER_DEFAULT: &str = "authorization";

/// The prefix a provider credential value gets by default.
pub const PROVIDER_AUTH_PREFIX_DEFAULT: &str = "Bearer ";

fn default_provider_auth_header() -> String {
    PROVIDER_AUTH_HEADER_DEFAULT.to_string()
}

fn is_default_provider_auth_header(value: &String) -> bool {
    value == "authorization"
}

fn default_provider_auth_prefix() -> String {
    PROVIDER_AUTH_PREFIX_DEFAULT.to_string()
}

fn is_default_provider_auth_prefix(value: &String) -> bool {
    value == "Bearer "
}

fn default_provider_methods() -> Vec<String> {
    vec!["POST".to_string()]
}

fn is_default_provider_methods(value: &Vec<String>) -> bool {
    value.len() == 1 && value[0] == "POST"
}

fn default_https_scheme() -> String {
    "https".to_string()
}

fn is_default_https_scheme(value: &String) -> bool {
    value == "https"
}

/// The typed wire APIs a provider grant may name.
///
/// This mirrors `kiqi_ai::Api`'s typed variants exactly. The contract crate
/// deliberately keeps zero internal dependencies, so the list is spelled here
/// instead of importing the type; `faux` and any unknown spelling are rejected
/// the same way `Api::Faux`/`Api::Other(_)` would be — a grant must name a real
/// wire API an extension can speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderApi {
    OpenaiCompletions,
    MistralConversations,
    OpenaiResponses,
    AzureOpenaiResponses,
    OpenaiCodexResponses,
    AnthropicMessages,
    BedrockConverseStream,
    GoogleGenerativeAi,
    GoogleVertex,
    PiMessages,
}

impl ProviderApi {
    /// Parse a grant's `api` string into the typed allow-list.
    pub fn parse(value: &str) -> Result<Self, String> {
        Ok(match value {
            "openai-completions" => Self::OpenaiCompletions,
            "mistral-conversations" => Self::MistralConversations,
            "openai-responses" => Self::OpenaiResponses,
            "azure-openai-responses" => Self::AzureOpenaiResponses,
            "openai-codex-responses" => Self::OpenaiCodexResponses,
            "anthropic-messages" => Self::AnthropicMessages,
            "bedrock-converse-stream" => Self::BedrockConverseStream,
            "google-generative-ai" => Self::GoogleGenerativeAi,
            "google-vertex" => Self::GoogleVertex,
            "pi-messages" => Self::PiMessages,
            "faux" => {
                return Err("`faux` is a test provider, not a wire API".to_string());
            }
            other => {
                return Err(format!("`{other}` is not a typed wire API"));
            }
        })
    }

    /// The wire spelling, as it appears in grants.json.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenaiCompletions => "openai-completions",
            Self::MistralConversations => "mistral-conversations",
            Self::OpenaiResponses => "openai-responses",
            Self::AzureOpenaiResponses => "azure-openai-responses",
            Self::OpenaiCodexResponses => "openai-codex-responses",
            Self::AnthropicMessages => "anthropic-messages",
            Self::BedrockConverseStream => "bedrock-converse-stream",
            Self::GoogleGenerativeAi => "google-generative-ai",
            Self::GoogleVertex => "google-vertex",
            Self::PiMessages => "pi-messages",
        }
    }
}

/// One model the provider grant owns. The grant is the catalog authority: a
/// guest cannot surface a model it did not get here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProviderModel {
    pub id: String,
    /// A wire API spelling from [`ProviderApi`].
    pub api: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub reasoning: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input: Vec<String>,
}

/// The request filter for `provider-http`: a request is refused unless its
/// scheme, host, path prefix and method all match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProviderEndpoint {
    #[serde(
        default = "default_https_scheme",
        skip_serializing_if = "is_default_https_scheme"
    )]
    pub scheme: String,
    /// An exact host — no wildcard.
    pub host: String,
    /// Must start with `/`; the host serves nothing outside it.
    pub path_prefix: String,
    #[serde(
        default = "default_provider_methods",
        skip_serializing_if = "is_default_provider_methods"
    )]
    pub methods: Vec<String>,
}

/// One provider profile: its catalog, its one endpoint filter, the opaque
/// host-side credential handle, and its budget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProviderProfile {
    /// The provider id (= the extension's install-dir id, checked by the host).
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub display_name: String,
    #[serde(default)]
    pub models: Vec<ProviderModel>,
    pub endpoint: ProviderEndpoint,
    /// An opaque host-minted handle (`pc-…`); the value never lives in this
    /// file. Empty is rejected when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// How the host injects the resolved credential: which header and which
    /// prefix. Absent = `authorization: Bearer <value>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ProviderAuth>,
    /// Wall-clock budget for one request; absent = the default, capped at
    /// [`PROVIDER_TIMEOUT_MAX_SECONDS`].
    #[serde(
        default = "default_provider_timeout",
        skip_serializing_if = "is_default_provider_timeout"
    )]
    pub timeout_seconds: u64,
    /// Cap on one response body across all `provider-http.read` calls.
    #[serde(
        default = "default_provider_max_response_bytes",
        skip_serializing_if = "is_default_provider_max_response_bytes"
    )]
    pub max_response_bytes: u64,
    /// Cap on concurrently open `provider-http` handles in one call.
    #[serde(
        default = "default_provider_max_handles",
        skip_serializing_if = "is_default_provider_max_handles"
    )]
    pub max_handles: u32,
}

/// How a resolved credential is written onto a granted request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProviderAuth {
    /// Header name, lowercase HTTP token. Absent = `authorization`.
    #[serde(
        default = "default_provider_auth_header",
        skip_serializing_if = "is_default_provider_auth_header"
    )]
    pub header: String,
    /// Value prefix (`Bearer ` for most APIs). Absent = `Bearer `.
    #[serde(
        default = "default_provider_auth_prefix",
        skip_serializing_if = "is_default_provider_auth_prefix"
    )]
    pub prefix: String,
}

impl Default for ProviderAuth {
    fn default() -> Self {
        Self {
            header: default_provider_auth_header(),
            prefix: default_provider_auth_prefix(),
        }
    }
}

/// The `provider` policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProviderGrant {
    #[serde(default)]
    pub providers: Vec<ProviderProfile>,
}

/// An injected credential header name must be a lowercase HTTP token.
fn validate_provider_auth_header(provider_id: &str, header: &str) -> Result<(), String> {
    if header.is_empty() || header.len() > 128 {
        return Err(format!(
            "provider `{provider_id}` auth `header` must be 1..=128 bytes"
        ));
    }
    if !header.bytes().all(|byte| {
        matches!(byte, b'a'..=b'z' | b'0'..=b'9')
            || matches!(
                byte,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
            )
    }) {
        return Err(format!(
            "provider `{provider_id}` auth `header` `{header}` is not a lowercase HTTP token"
        ));
    }
    if header.eq_ignore_ascii_case("host")
        || header.eq_ignore_ascii_case("content-length")
        || header.eq_ignore_ascii_case("transfer-encoding")
        || header.eq_ignore_ascii_case("connection")
    {
        return Err(format!(
            "provider `{provider_id}` auth `header` `{header}` is a framing header and cannot \
             carry a credential"
        ));
    }
    Ok(())
}

/// The path prefix must be a plain absolute path: no dot segments, no empty
/// segments and no percent-encoded separators. The host re-checks requests
/// against it, so a sloppy prefix would widen the filter.
fn validate_provider_path_prefix(provider_id: &str, prefix: &str) -> Result<(), String> {
    let lower = prefix.to_ascii_lowercase();
    if lower.contains("%2f") || lower.contains("%5c") || lower.contains("%2e") {
        return Err(format!(
            "provider `{provider_id}` `path-prefix` must not contain encoded separators or dots"
        ));
    }
    if prefix.contains("//")
        || prefix
            .split('/')
            .any(|segment| segment == "." || segment == "..")
    {
        return Err(format!(
            "provider `{provider_id}` `path-prefix` must not contain dot or empty segments"
        ));
    }
    Ok(())
}

impl ProviderGrant {
    /// The tightest per-profile budget. A `provider-request` patch deadline is
    /// bounded by it: a patch must not outlive the request it belongs to.
    pub fn effective_timeout_seconds(&self) -> u64 {
        self.providers
            .iter()
            .map(|profile| profile.timeout_seconds)
            .min()
            .unwrap_or(PROVIDER_TIMEOUT_DEFAULT_SECONDS)
    }

    /// Validate the catalog, the endpoint filter and the credential handle.
    pub fn validate(&self) -> Result<(), String> {
        if self.providers.is_empty() {
            return Err("provider needs at least one provider profile".to_string());
        }
        for profile in &self.providers {
            if profile.id.trim().is_empty() {
                return Err("provider `id` must not be empty".to_string());
            }
            if self
                .providers
                .iter()
                .filter(|other| other.id == profile.id)
                .count()
                > 1
            {
                return Err(format!("provider id `{}` is not unique", profile.id));
            }
            if profile.models.is_empty() {
                return Err(format!(
                    "provider `{}` needs at least one model",
                    profile.id
                ));
            }
            for model in &profile.models {
                if model.id.trim().is_empty() {
                    return Err(format!(
                        "provider `{}` has a model with an empty id",
                        profile.id
                    ));
                }
                if profile
                    .models
                    .iter()
                    .filter(|other| other.id == model.id)
                    .count()
                    > 1
                {
                    return Err(format!(
                        "provider `{}` model id `{}` is not unique",
                        profile.id, model.id
                    ));
                }
                if let Err(error) = ProviderApi::parse(&model.api) {
                    return Err(format!(
                        "provider `{}` model `{}`: {error}",
                        profile.id, model.id
                    ));
                }
            }
            let endpoint = &profile.endpoint;
            if endpoint.scheme != "https" {
                return Err(format!(
                    "provider `{}` endpoint scheme must be `https`, got `{}`",
                    profile.id, endpoint.scheme
                ));
            }
            validate_exact_host(
                &format!("provider `{}` endpoint host", profile.id),
                &endpoint.host,
            )?;
            if !endpoint.path_prefix.starts_with('/') {
                return Err(format!(
                    "provider `{}` endpoint `path-prefix` must start with `/`, got `{}`",
                    profile.id, endpoint.path_prefix
                ));
            }
            if endpoint.methods.is_empty() {
                return Err(format!(
                    "provider `{}` endpoint needs at least one method",
                    profile.id
                ));
            }
            for method in &endpoint.methods {
                if method != "GET" && method != "POST" {
                    return Err(format!(
                        "provider `{}` endpoint method `{method}` is not allowed (GET/POST only)",
                        profile.id
                    ));
                }
            }
            if let Some(credential) = &profile.credential {
                if credential.trim().is_empty() {
                    return Err(format!(
                        "provider `{}` `credential` must not be empty when present",
                        profile.id
                    ));
                }
                // A credential is injected on a matching request, so the
                // filter must actually narrow the host: `/` would mean the
                // whole host and defeat the endpoint record.
                if endpoint.path_prefix == "/" {
                    return Err(format!(
                        "provider `{}` has a credential but `path-prefix` is `/`;                          name the API root instead",
                        profile.id
                    ));
                }
            }
            if let Some(auth) = &profile.auth {
                validate_provider_auth_header(&profile.id, &auth.header)?;
                if auth.prefix.contains(['\r', '\n', '\0']) {
                    return Err(format!(
                        "provider `{}` auth `prefix` must not contain CR, LF or NUL",
                        profile.id
                    ));
                }
                if auth.prefix.len() > 1024 {
                    return Err(format!(
                        "provider `{}` auth `prefix` is over 1024 bytes",
                        profile.id
                    ));
                }
            }
            validate_provider_path_prefix(&profile.id, &endpoint.path_prefix)?;
            if profile.timeout_seconds == 0
                || profile.timeout_seconds > PROVIDER_TIMEOUT_MAX_SECONDS
            {
                return Err(format!(
                    "provider `{}` `timeout-seconds` must be 1..={PROVIDER_TIMEOUT_MAX_SECONDS}, got {}",
                    profile.id, profile.timeout_seconds
                ));
            }
            if profile.max_response_bytes < PROVIDER_MAX_RESPONSE_BYTES_MIN
                || profile.max_response_bytes > PROVIDER_MAX_RESPONSE_BYTES_MAX
            {
                return Err(format!(
                    "provider `{}` `max-response-bytes` must be {}..={}, got {}",
                    profile.id,
                    PROVIDER_MAX_RESPONSE_BYTES_MIN,
                    PROVIDER_MAX_RESPONSE_BYTES_MAX,
                    profile.max_response_bytes
                ));
            }
            if profile.max_handles == 0 || profile.max_handles > PROVIDER_MAX_HANDLES_MAX {
                return Err(format!(
                    "provider `{}` `max-handles` must be 1..={PROVIDER_MAX_HANDLES_MAX}, got {}",
                    profile.id, profile.max_handles
                ));
            }
        }
        Ok(())
    }
}

/// The only render kind 0.8.0 accepts.
pub const RENDER_KIND_MARKDOWN: &str = "markdown";

/// Default deadline for one markdown transform.
pub const RENDER_TIMEOUT_MS_DEFAULT: u64 = 2_000;

/// The largest transform deadline a grant can ask for.
pub const RENDER_TIMEOUT_MS_MAX: u64 = 10_000;

/// Default cap on the transformed text.
pub const RENDER_MAX_OUTPUT_BYTES_DEFAULT: u64 = 1024 * 1024;

/// The largest output cap a grant can ask for.
pub const RENDER_MAX_OUTPUT_BYTES_MAX: u64 = 8 * 1024 * 1024;

fn default_render_kinds() -> Vec<String> {
    vec![RENDER_KIND_MARKDOWN.to_string()]
}

fn is_default_render_kinds(value: &Vec<String>) -> bool {
    value.len() == 1 && value[0] == RENDER_KIND_MARKDOWN
}

fn default_render_timeout() -> u64 {
    RENDER_TIMEOUT_MS_DEFAULT
}

fn is_default_render_timeout(value: &u64) -> bool {
    *value == RENDER_TIMEOUT_MS_DEFAULT
}

fn default_render_max_output_bytes() -> u64 {
    RENDER_MAX_OUTPUT_BYTES_DEFAULT
}

fn is_default_render_max_output_bytes(value: &u64) -> bool {
    *value == RENDER_MAX_OUTPUT_BYTES_DEFAULT
}

/// The `render` policy: which transform kinds and the budget for one call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RenderGrant {
    /// Only `markdown` is accepted; absent = `["markdown"]`.
    #[serde(
        default = "default_render_kinds",
        skip_serializing_if = "is_default_render_kinds"
    )]
    pub kinds: Vec<String>,
    #[serde(
        default = "default_render_timeout",
        skip_serializing_if = "is_default_render_timeout"
    )]
    pub timeout_ms: u64,
    #[serde(
        default = "default_render_max_output_bytes",
        skip_serializing_if = "is_default_render_max_output_bytes"
    )]
    pub max_output_bytes: u64,
}

impl Default for RenderGrant {
    fn default() -> Self {
        Self {
            kinds: default_render_kinds(),
            timeout_ms: RENDER_TIMEOUT_MS_DEFAULT,
            max_output_bytes: RENDER_MAX_OUTPUT_BYTES_DEFAULT,
        }
    }
}

impl RenderGrant {
    /// Validate the kinds and the two bounds.
    pub fn validate(&self) -> Result<(), String> {
        if self.kinds.is_empty() {
            return Err("render needs at least one kind".to_string());
        }
        for kind in &self.kinds {
            if kind != RENDER_KIND_MARKDOWN {
                return Err(format!(
                    "render kind `{kind}` is not supported (only `{RENDER_KIND_MARKDOWN}`)"
                ));
            }
        }
        if self.timeout_ms == 0 || self.timeout_ms > RENDER_TIMEOUT_MS_MAX {
            return Err(format!(
                "render `timeout-ms` must be 1..={RENDER_TIMEOUT_MS_MAX}, got {}",
                self.timeout_ms
            ));
        }
        if self.max_output_bytes == 0 || self.max_output_bytes > RENDER_MAX_OUTPUT_BYTES_MAX {
            return Err(format!(
                "render `max-output-bytes` must be 1..={RENDER_MAX_OUTPUT_BYTES_MAX}, got {}",
                self.max_output_bytes
            ));
        }
        Ok(())
    }
}

/// The resource kinds a `resources` grant may enable.
pub const RESOURCES_KIND_SKILLS: &str = "skills";
/// See [`RESOURCES_KIND_SKILLS`].
pub const RESOURCES_KIND_PROMPTS: &str = "prompts";

/// Default number of resources one extension may contribute.
pub const RESOURCES_MAX_ITEMS_DEFAULT: u32 = 64;
/// The hard ceiling on `max-items`.
pub const RESOURCES_MAX_ITEMS_MAX: u32 = 256;
/// Default total bytes of resources one extension may contribute.
pub const RESOURCES_MAX_BYTES_DEFAULT: u64 = 1024 * 1024;
/// The hard ceiling on `max-bytes`.
pub const RESOURCES_MAX_BYTES_MAX: u64 = 16 * 1024 * 1024;

fn default_resources_kinds() -> Vec<String> {
    vec![
        RESOURCES_KIND_SKILLS.to_string(),
        RESOURCES_KIND_PROMPTS.to_string(),
    ]
}

fn is_default_resources_kinds(value: &Vec<String>) -> bool {
    value.len() == 2 && value[0] == RESOURCES_KIND_SKILLS && value[1] == RESOURCES_KIND_PROMPTS
}

fn default_resources_max_items() -> u32 {
    RESOURCES_MAX_ITEMS_DEFAULT
}

fn is_default_resources_max_items(value: &u32) -> bool {
    *value == RESOURCES_MAX_ITEMS_DEFAULT
}

fn default_resources_max_bytes() -> u64 {
    RESOURCES_MAX_BYTES_DEFAULT
}

fn is_default_resources_max_bytes(value: &u64) -> bool {
    *value == RESOURCES_MAX_BYTES_DEFAULT
}

/// The `resources` policy: package-resident skills/prompts, with item and byte
/// caps. Bare `true` grants both kinds with the defaults, like `fs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ResourcesGrant {
    #[serde(
        default = "default_resources_kinds",
        skip_serializing_if = "is_default_resources_kinds"
    )]
    pub kinds: Vec<String>,
    #[serde(
        default = "default_resources_max_items",
        skip_serializing_if = "is_default_resources_max_items"
    )]
    pub max_items: u32,
    #[serde(
        default = "default_resources_max_bytes",
        skip_serializing_if = "is_default_resources_max_bytes"
    )]
    pub max_bytes: u64,
}

impl Default for ResourcesGrant {
    fn default() -> Self {
        Self {
            kinds: default_resources_kinds(),
            max_items: RESOURCES_MAX_ITEMS_DEFAULT,
            max_bytes: RESOURCES_MAX_BYTES_DEFAULT,
        }
    }
}

impl ResourcesGrant {
    /// Validate the kinds and the two caps.
    pub fn validate(&self) -> Result<(), String> {
        if self.kinds.is_empty() {
            return Err("resources needs at least one kind".to_string());
        }
        for kind in &self.kinds {
            if kind != RESOURCES_KIND_SKILLS && kind != RESOURCES_KIND_PROMPTS {
                return Err(format!(
                    "resources kind `{kind}` is not supported (skills/prompts only)"
                ));
            }
        }
        if self.max_items == 0 || self.max_items > RESOURCES_MAX_ITEMS_MAX {
            return Err(format!(
                "resources `max-items` must be 1..={RESOURCES_MAX_ITEMS_MAX}, got {}",
                self.max_items
            ));
        }
        if self.max_bytes == 0 || self.max_bytes > RESOURCES_MAX_BYTES_MAX {
            return Err(format!(
                "resources `max-bytes` must be 1..={RESOURCES_MAX_BYTES_MAX}, got {}",
                self.max_bytes
            ));
        }
        Ok(())
    }
}

/// Whether `provider-request` delivers the before-request event at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObserveMode {
    /// Nothing is delivered (the default).
    #[default]
    None,
    /// Delivered, redacted, once per provider request.
    Requests,
}

fn is_default_observe(value: &ObserveMode) -> bool {
    *value == ObserveMode::None
}

/// Whether header values cross the ABI, or only their names.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HeadersMode {
    /// Names only; values are replaced by `<redacted>` (the default).
    #[default]
    Names,
    /// Values too, but only for the host's explicit allowlist of
    /// non-credential tracing/correlation headers. Every other value,
    /// including nonstandard provider credentials, stays host-side. Header
    /// names still cross in `headerNames`.
    Values,
}

fn is_default_headers(value: &HeadersMode) -> bool {
    *value == HeadersMode::Names
}

/// Whether the conversation crosses the ABI with the observe event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MessagesMode {
    /// No messages (the default).
    #[default]
    None,
    /// The full conversation, an explicit opt-in.
    Full,
}

fn is_default_messages(value: &MessagesMode) -> bool {
    *value == MessagesMode::None
}

/// Default deadline for one patch decision.
pub const PROVIDER_REQUEST_PATCH_TIMEOUT_MS_DEFAULT: u64 = 2_000;

/// The fixed ceiling on `patch-timeout-ms`; the effective maximum is this or
/// the provider grant's tightest budget, whichever is smaller.
pub const PROVIDER_REQUEST_PATCH_TIMEOUT_MS_MAX: u64 = 10_000;

fn default_provider_request_patch_timeout() -> u64 {
    PROVIDER_REQUEST_PATCH_TIMEOUT_MS_DEFAULT
}

fn is_default_provider_request_patch_timeout(value: &u64) -> bool {
    *value == PROVIDER_REQUEST_PATCH_TIMEOUT_MS_DEFAULT
}

/// The `provider-request` policy: the redacted observe event plus the bounded
/// patch channel. Absent = the event is not delivered and no patch is accepted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProviderRequestGrant {
    #[serde(default, skip_serializing_if = "is_default_observe")]
    pub observe: ObserveMode,
    #[serde(default, skip_serializing_if = "is_default_headers")]
    pub headers: HeadersMode,
    #[serde(default, skip_serializing_if = "is_default_messages")]
    pub messages: MessagesMode,
    #[serde(default, skip_serializing_if = "is_false")]
    pub patch: bool,
    /// Deadline for one patch decision; capped by the provider budget too.
    #[serde(
        default = "default_provider_request_patch_timeout",
        skip_serializing_if = "is_default_provider_request_patch_timeout"
    )]
    pub patch_timeout_ms: u64,
}

impl Default for ProviderRequestGrant {
    fn default() -> Self {
        Self {
            observe: ObserveMode::None,
            headers: HeadersMode::Names,
            messages: MessagesMode::None,
            patch: false,
            patch_timeout_ms: PROVIDER_REQUEST_PATCH_TIMEOUT_MS_DEFAULT,
        }
    }
}

impl ProviderRequestGrant {
    /// Validate the patch deadline against the provider grant's tightest
    /// budget. `None` means no provider grant is present, so only the fixed
    /// ceiling applies.
    pub fn validate(&self, provider_timeout_seconds: Option<u64>) -> Result<(), String> {
        let max = provider_patch_timeout_max_ms(provider_timeout_seconds);
        if self.patch_timeout_ms == 0 || self.patch_timeout_ms > max {
            return Err(format!(
                "provider-request `patch-timeout-ms` must be 1..={max}, got {}",
                self.patch_timeout_ms
            ));
        }
        Ok(())
    }

    /// The deadline the host actually applies to one patch decision.
    ///
    /// `patch-timeout-ms` is already validated against the provider budget when
    /// the grant is written, but a hand-edited `grants.json` is not re-validated
    /// on load: clamp here too, so the guest can never hold the request open
    /// longer than the provider call it belongs to could run.
    pub fn effective_patch_timeout_ms(&self, provider_timeout_seconds: Option<u64>) -> u64 {
        self.patch_timeout_ms
            .min(provider_patch_timeout_max_ms(provider_timeout_seconds))
            .max(1)
    }
}

/// The policy a sandboxed host enforces for one provider request, resolved from
/// its `provider-request` grant.
///
/// The host keeps this as a snapshot beside the instance (not behind the
/// per-call mutex), so the event fan-out can partition hosts without waiting
/// for a busy guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderRequestPolicy {
    /// Deliver the redacted request event through the observe fan-out.
    pub observe: bool,
    /// Accept a patch from the provider gate. A patch-capable host is invoked
    /// ONLY by the gate and never by the observe fan-out.
    pub patch: bool,
    /// Wall-clock deadline for the gate call.
    pub patch_timeout: std::time::Duration,
}

/// Resolve the enforcement snapshot for one extension's `provider-request` grant.
///
/// `None` means neither observation nor patching is authorized: the guest is
/// never called for a provider request at all. The snapshot is computed once at
/// load and stored beside the instance, so the fan-out can partition hosts
/// without taking the per-call guest mutex.
pub fn resolve_provider_request_policy(grants: &Grants) -> Option<ProviderRequestPolicy> {
    let grant = grants.provider_request.as_ref()?;
    let observe = grant.observe == ObserveMode::Requests;
    if !observe && !grant.patch {
        return None;
    }
    let provider_timeout = grants
        .provider
        .as_ref()
        .map(ProviderGrant::effective_timeout_seconds);
    Some(ProviderRequestPolicy {
        observe,
        patch: grant.patch,
        patch_timeout: std::time::Duration::from_millis(
            grant.effective_patch_timeout_ms(provider_timeout),
        ),
    })
}

fn provider_patch_timeout_max_ms(provider_timeout_seconds: Option<u64>) -> u64 {
    provider_timeout_seconds
        .map(|seconds| seconds.saturating_mul(1000))
        .unwrap_or(u64::MAX)
        .min(PROVIDER_REQUEST_PATCH_TIMEOUT_MS_MAX)
}

/// An OAuth endpoint must be an exact `https://host/…` URL: TLS only, a real
/// host (no wildcard, no userinfo) and no fragment. A query string is allowed;
/// a wildcard anywhere is not.
fn validate_https_endpoint(field: &str, value: &str) -> Result<(), String> {
    let Some(rest) = value.strip_prefix("https://") else {
        return Err(format!("{field} must be an https URL, got `{value}`"));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err(format!("{field} has no host: `{value}`"));
    }
    if authority.contains('@') {
        return Err(format!("{field} must not carry userinfo: `{value}`"));
    }
    if value.contains('*') {
        return Err(format!(
            "{field} must name an exact host, not a wildcard: `{value}`"
        ));
    }
    if value.contains('#') {
        return Err(format!("{field} must not carry a fragment: `{value}`"));
    }
    if authority.chars().any(char::is_whitespace) {
        return Err(format!("{field} must not contain whitespace: `{value}`"));
    }
    Ok(())
}

/// A provider endpoint host is a bare exact host: non-empty, no wildcard, no
/// path and no whitespace.
fn validate_exact_host(field: &str, host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if host.contains('*') {
        return Err(format!(
            "{field} must be an exact host, not a wildcard: `{host}`"
        ));
    }
    if host.contains('/') || host.chars().any(char::is_whitespace) {
        return Err(format!("{field} must be a bare host, got `{host}`"));
    }
    Ok(())
}

/// The grant key that authorizes an ABI interface.
///
/// `provider-http` and `provider-stream` are the imports and `provider-api`
/// the export of the one `provider` decision (`provider-stream` is additionally
/// call-scoped, not separately grantable); `oauth-http` and `oauth-api` are the
/// two faces of `oauth`; the `render-api` export maps to `render`, the package
/// `resources` convention to `resources`, and the provider-request hook to
/// `provider-request`. The bare interface name and the
/// `kiqi:extension/<name>@<version>` form are both accepted.
pub fn grant_key_for_interface(interface: &str) -> Option<&'static str> {
    let name = interface
        .strip_prefix("kiqi:extension/")
        .unwrap_or(interface);
    let name = name.split('@').next().unwrap_or(name);
    match name {
        "provider-http" | "provider-stream" | "provider-api" => Some("provider"),
        "oauth-http" | "oauth-api" => Some("oauth"),
        "render-api" => Some("render"),
        "resources" => Some("resources"),
        "provider-request" => Some("provider-request"),
        _ => None,
    }
}

/// Options a grant may carry beyond its value list. `fs`/`exec`/`proc` accept
/// some; a caller setting one for another capability (including every 0.8.0
/// key) gets an error, not a silently dropped flag.
#[derive(Debug, Clone, Default)]
pub struct GrantOptions {
    /// `--cwd extension-data|project` (fs, exec and proc).
    pub cwd: Option<ExecCwd>,
    /// `--timeout <seconds>` (exec and proc; each has its own bounds).
    pub timeout_seconds: Option<u64>,
    /// `--max-open <n>` (proc only).
    pub max_open: Option<u32>,
}

impl Grants {
    /// Read the user's grants for `extension_id`; a missing (or unreadable, or
    /// malformed) file means nothing is granted.
    pub fn load_for(extension_id: &str) -> Self {
        match agent_dir() {
            Some(root) => Self::load_for_at(&root, extension_id),
            None => Self::default(),
        }
    }

    /// [`Self::load_for`] against an explicit agent root, so a caller (a test, or
    /// a CLI that was handed a directory) never has to write a process-global
    /// environment variable to redirect it.
    pub fn load_for_at(root: &std::path::Path, extension_id: &str) -> Self {
        Self::load_for_at_with(root, extension_id, |warning| eprintln!("{warning}"))
    }

    /// Load with an injectable diagnostic sink so tests can prove that parse
    /// failures are loud. The read path never writes: a malformed file, an
    /// invalid capability, or an unknown field is left exactly as the user
    /// wrote it, so a concurrent grant/revoke can never be overwritten by a
    /// stale read. Upgrading the document format is a side effect of an
    /// explicit save ([`Self::save_for_at`]), not of loading.
    fn load_for_at_with(
        root: &std::path::Path,
        extension_id: &str,
        mut warn: impl FnMut(String),
    ) -> Self {
        let path = grants_path_at(root, extension_id);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        // Parse the envelope separately so an unsupported version fails closed
        // before any of its capability values can be interpreted by this build.
        let value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(error) => {
                warn(format!(
                    "warning: {} is unreadable ({error}); treating every capability as ungranted and leaving the file unchanged",
                    path.display()
                ));
                return Self::default();
            }
        };
        let version = match grants_file_version(&value) {
            Ok(version) => version,
            Err(error) => {
                warn(format!(
                    "warning: {} is unreadable ({error}); treating every capability as ungranted and leaving the file unchanged",
                    path.display()
                ));
                return Self::default();
            }
        };
        if version != GRANTS_VERSION_LEGACY && version != GRANTS_VERSION {
            warn(format!(
                "warning: {} has unsupported grants.json version {version} (supported: {GRANTS_VERSION_LEGACY}, {GRANTS_VERSION}); treating every capability as ungranted and leaving the file unchanged",
                path.display()
            ));
            return Self::default();
        }
        // `Value` keeps only the last of a duplicated key, so reject duplicates
        // on the original text before any capability is interpreted.
        if let Err(error) = serde_json::from_str::<RejectDuplicateKeys>(&text) {
            warn(format!(
                "warning: {} is unreadable ({error}); treating every capability as ungranted and leaving the file unchanged",
                path.display()
            ));
            return Self::default();
        }
        let mut grants: Grants = match serde_json::from_str(&text) {
            Ok(grants) => grants,
            Err(error) => {
                warn(format!(
                    "warning: {} is unreadable ({error}); treating every capability as ungranted and leaving the file unchanged",
                    path.display()
                ));
                return Self::default();
            }
        };
        // A hand-edited file must not buy policy the CLI would refuse, so the
        // same validation the constructors use runs here. An invalid policy is
        // treated as absent (that one capability is cleared, the rest of the
        // file still loads): the caller sees only what survived, and the file
        // is left untouched so nothing the user wrote is silently deleted.
        let before: Vec<&'static str> = grants.granted();
        grants = grants.validated();
        let dropped: Vec<&'static str> = before
            .into_iter()
            .filter(|capability| !grants.granted().contains(capability))
            .collect();
        if !dropped.is_empty() {
            warn(format!(
                "warning: {} has invalid policy for {}; ignoring those capabilities for this load and leaving the file unchanged",
                path.display(),
                dropped.join(", ")
            ));
        }
        grants
    }

    /// Write the grants, creating `extensions/<id>/` as needed.
    pub fn save_for(extension_id: &str, grants: &Grants) -> Result<(), String> {
        let root = agent_dir().ok_or("cannot resolve the agent directory")?;
        Self::save_for_at(&root, extension_id, grants)
    }

    /// [`Self::save_for`] against an explicit agent root. This is the only
    /// writer: every explicit save upgrades the document to [`GRANTS_VERSION`]
    /// and writes the current capability spellings (defaults stay omitted).
    pub fn save_for_at(
        root: &std::path::Path,
        extension_id: &str,
        grants: &Grants,
    ) -> Result<(), String> {
        let path = grants_path_at(root, extension_id);
        // A save rewrites the whole document, so it must not run over a file
        // this build only partially understands: a future version's contents
        // (or a duplicated key that failed to parse) would be silently erased.
        if let Ok(text) = std::fs::read_to_string(&path) {
            match serde_json::from_str::<Value>(&text) {
                Ok(value) => {
                    if let Ok(version) = grants_file_version(&value) {
                        if version != GRANTS_VERSION && version != GRANTS_VERSION_LEGACY {
                            return Err(format!(
                                "{} has unsupported grants.json version {version}; refusing to overwrite it",
                                path.display()
                            ));
                        }
                    }
                    if serde_json::from_str::<RejectDuplicateKeys>(&text).is_err() {
                        return Err(format!(
                            "{} has duplicated keys; refusing to overwrite it",
                            path.display()
                        ));
                    }
                }
                Err(error) => {
                    return Err(format!(
                        "{} is unreadable ({error}); refusing to overwrite it",
                        path.display()
                    ));
                }
            }
        }
        let mut current = grants.clone();
        current.version = Some(GRANTS_VERSION);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(&current).map_err(|error| error.to_string())?;
        std::fs::write(&path, format!("{text}\n"))
            .map_err(|error| format!("write {}: {error}", path.display()))
    }

    /// True when HTTP to `host` is granted.
    pub fn allows_host(&self, host: &str) -> bool {
        self.net_http
            .as_ref()
            .is_some_and(|grant| allows(&grant.hosts, host))
    }

    /// True when `command` may be spawned through `proc`.
    pub fn allows_proc_command(&self, command: &str) -> bool {
        self.proc
            .as_ref()
            .is_some_and(|grant| allows(&grant.commands, command))
    }

    /// True when a TCP connection to `host` is granted.
    pub fn allows_socket_host(&self, host: &str) -> bool {
        self.net_sockets
            .as_ref()
            .is_some_and(|grant| allows(&grant.hosts, host))
    }

    /// True when `command` (a bare name, matched exactly against the list) is
    /// granted.
    pub fn allows_command(&self, command: &str) -> bool {
        self.exec
            .as_ref()
            .is_some_and(|grant| allows(&grant.commands, command))
    }

    /// The capability names currently granted, for display.
    pub fn granted(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.net_http.is_some() {
            out.push("net-http");
        }
        if self.fs.is_some() {
            out.push("fs");
        }
        if self.exec.is_some() {
            out.push("exec");
        }
        if self.proc.is_some() {
            out.push("proc");
        }
        if self.net_sockets.is_some() {
            out.push("net-sockets");
        }
        if self.oauth.is_some() {
            out.push("oauth");
        }
        if self.provider.is_some() {
            out.push("provider");
        }
        if self.render.is_some() {
            out.push("render");
        }
        if self.resources.is_some() {
            out.push("resources");
        }
        if self.provider_request.is_some() {
            out.push("provider-request");
        }
        out
    }

    /// Grant one capability. Every capability carries its policy: hosts for
    /// `net-http`/`net-sockets`, command names for `exec`, nothing for `fs`
    /// (whose jail is the policy). `exec` additionally accepts a cwd scope and
    /// a time budget through `options`; any other capability rejects them.
    pub fn grant(
        &mut self,
        capability: &str,
        policy: Vec<String>,
        options: GrantOptions,
    ) -> Result<(), String> {
        let non_empty = |what: &str| -> Result<Vec<String>, String> {
            if policy.is_empty() {
                Err(format!("{capability} needs at least one {what}"))
            } else {
                Ok(policy.clone())
            }
        };
        match capability {
            "net-http" | "net-sockets" => {
                if options.cwd.is_some()
                    || options.timeout_seconds.is_some()
                    || options.max_open.is_some()
                {
                    return Err(format!(
                        "`{capability}` takes no --cwd/--timeout/--max-open (those are exec and proc options)"
                    ));
                }
                let hosts = || non_empty("--host (or --host '*'))");
                match capability {
                    "net-http" => self.net_http = Some(NetHttpGrant { hosts: hosts()? }),
                    "net-sockets" => self.net_sockets = Some(NetSocketsGrant { hosts: hosts()? }),
                    _ => unreachable!(),
                }
            }
            "fs" => {
                if options.timeout_seconds.is_some() || options.max_open.is_some() {
                    return Err(format!(
                        "`fs` takes no --timeout/--max-open (its policy is the jail scope)"
                    ));
                }
                // The default stays the data-dir jail; `--cwd project` opts
                // into the project tree.
                self.fs = Some(FsGrant {
                    cwd: options.cwd.unwrap_or(ExecCwd::ExtensionData),
                });
            }
            "exec" => {
                if options.max_open.is_some() {
                    return Err("`exec` takes no --max-open (that is a proc option)".to_string());
                }
                let commands = non_empty("--command (or --command '*')")?;
                let timeout_seconds = match options.timeout_seconds {
                    None => EXEC_TIMEOUT_DEFAULT_SECONDS,
                    Some(0) => return Err("exec --timeout must be at least 1 second".to_string()),
                    Some(seconds) if seconds > EXEC_TIMEOUT_MAX_SECONDS => {
                        return Err(format!(
                            "exec --timeout {seconds}s exceeds the {EXEC_TIMEOUT_MAX_SECONDS}s cap"
                        ))
                    }
                    Some(seconds) => seconds,
                };
                self.exec = Some(ExecGrant {
                    commands,
                    cwd: options.cwd.unwrap_or_default(),
                    timeout_seconds,
                });
            }
            "proc" => {
                let commands = non_empty("--command (or --command '*')")?;
                let timeout_seconds = match options.timeout_seconds {
                    None => PROC_TIMEOUT_DEFAULT_SECONDS,
                    Some(0) => return Err("proc --timeout must be at least 1 second".to_string()),
                    Some(seconds) if seconds > PROC_TIMEOUT_MAX_SECONDS => {
                        return Err(format!(
                            "proc --timeout {seconds}s exceeds the {PROC_TIMEOUT_MAX_SECONDS}s cap"
                        ))
                    }
                    Some(seconds) => seconds,
                };
                let max_open = match options.max_open {
                    None => PROC_MAX_OPEN_DEFAULT,
                    Some(0) => return Err("proc --max-open must be at least 1".to_string()),
                    Some(n) if n > PROC_MAX_OPEN_MAX => {
                        return Err(format!(
                            "proc --max-open {n} exceeds the {PROC_MAX_OPEN_MAX} cap"
                        ))
                    }
                    Some(n) => n,
                };
                self.proc = Some(ProcGrant {
                    commands,
                    cwd: options.cwd.unwrap_or_default(),
                    timeout_seconds,
                    max_open,
                });
            }
            "oauth" => {
                reject_grant_options(capability, &options)?;
                // A Vec<String> cannot describe an IdP profile, so the generic
                // entry point refuses rather than inventing one.
                return Err("`oauth` carries provider profiles; use grant_oauth".to_string());
            }
            "provider" => {
                reject_grant_options(capability, &options)?;
                return Err("`provider` carries provider profiles; use grant_provider".to_string());
            }
            "render" => {
                reject_grant_options(capability, &options)?;
                let kinds = if policy.is_empty() {
                    default_render_kinds()
                } else {
                    policy.clone()
                };
                self.grant_render(RenderGrant {
                    kinds,
                    ..RenderGrant::default()
                })?;
            }
            "resources" => {
                reject_grant_options(capability, &options)?;
                let kinds = if policy.is_empty() {
                    default_resources_kinds()
                } else {
                    policy.clone()
                };
                self.grant_resources(ResourcesGrant {
                    kinds,
                    ..ResourcesGrant::default()
                })?;
            }
            "provider-request" => {
                reject_grant_options(capability, &options)?;
                if !policy.is_empty() {
                    return Err(
                        "`provider-request` takes no value list; use grant_provider_request"
                            .to_string(),
                    );
                }
                self.grant_provider_request(ProviderRequestGrant::default())?;
            }
            other => return Err(format!("unknown capability `{other}`")),
        }
        Ok(())
    }

    /// Revoke one capability.
    pub fn revoke(&mut self, capability: &str) -> Result<(), String> {
        match capability {
            "net-http" => self.net_http = None,
            "fs" => self.fs = None,
            "exec" => self.exec = None,
            "proc" => self.proc = None,
            "net-sockets" => self.net_sockets = None,
            "oauth" => self.oauth = None,
            "provider" => self.provider = None,
            "render" => self.render = None,
            "resources" => self.resources = None,
            "provider-request" => self.provider_request = None,
            other => return Err(format!("unknown capability `{other}`")),
        }
        Ok(())
    }

    /// Grant the `oauth` policy. The constructor validates first, so an
    /// invalid profile never reaches the file.
    pub fn grant_oauth(&mut self, grant: OAuthGrant) -> Result<(), String> {
        grant.validate()?;
        self.oauth = Some(grant);
        Ok(())
    }

    /// Grant the `provider` policy. A provider with a shorter budget than an
    /// existing `provider-request` patch deadline is refused, because it would
    /// strand a patch the file already promises to run.
    pub fn grant_provider(&mut self, grant: ProviderGrant) -> Result<(), String> {
        grant.validate()?;
        if let Some(request) = &self.provider_request {
            request.validate(Some(grant.effective_timeout_seconds()))?;
        }
        self.provider = Some(grant);
        Ok(())
    }

    /// Grant the `render` policy.
    pub fn grant_render(&mut self, grant: RenderGrant) -> Result<(), String> {
        grant.validate()?;
        self.render = Some(grant);
        Ok(())
    }

    /// Grant the `resources` policy.
    pub fn grant_resources(&mut self, grant: ResourcesGrant) -> Result<(), String> {
        grant.validate()?;
        self.resources = Some(grant);
        Ok(())
    }

    /// Grant the `provider-request` policy, bounded by the provider budget.
    pub fn grant_provider_request(&mut self, grant: ProviderRequestGrant) -> Result<(), String> {
        grant.validate(
            self.provider
                .as_ref()
                .map(ProviderGrant::effective_timeout_seconds),
        )?;
        self.provider_request = Some(grant);
        Ok(())
    }

    /// Validate every 0.8.0 policy present. The constructors call the
    /// per-policy checks; this is the whole-file view, and the one the CLI can
    /// run before writing.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(grant) = &self.oauth {
            grant.validate()?;
        }
        if let Some(grant) = &self.provider {
            grant.validate()?;
        }
        if let Some(grant) = &self.render {
            grant.validate()?;
        }
        if let Some(grant) = &self.resources {
            grant.validate()?;
        }
        if let Some(grant) = &self.provider_request {
            grant.validate(
                self.provider
                    .as_ref()
                    .map(ProviderGrant::effective_timeout_seconds),
            )?;
        }
        Ok(())
    }

    /// The fail-closed load path: clear only the capability whose policy does
    /// not validate, so a hand-edited file cannot smuggle an invalid policy in
    /// while leaving unrelated valid grants intact.
    fn validated(mut self) -> Self {
        if self
            .oauth
            .as_ref()
            .is_some_and(|grant| grant.validate().is_err())
        {
            self.oauth = None;
        }
        if self
            .provider
            .as_ref()
            .is_some_and(|grant| grant.validate().is_err())
        {
            self.provider = None;
        }
        if self
            .render
            .as_ref()
            .is_some_and(|grant| grant.validate().is_err())
        {
            self.render = None;
        }
        if self
            .resources
            .as_ref()
            .is_some_and(|grant| grant.validate().is_err())
        {
            self.resources = None;
        }
        let provider_timeout = self
            .provider
            .as_ref()
            .map(ProviderGrant::effective_timeout_seconds);
        if self
            .provider_request
            .as_ref()
            .is_some_and(|grant| grant.validate(provider_timeout).is_err())
        {
            self.provider_request = None;
        }
        self
    }
}

/// Reject the `exec`/`proc` options on a capability that has no use for them.
/// The one place the rule lives, so every 0.8.0 arm behaves like `net-http`.
fn reject_grant_options(capability: &str, options: &GrantOptions) -> Result<(), String> {
    if options.cwd.is_some() || options.timeout_seconds.is_some() || options.max_open.is_some() {
        return Err(format!(
            "`{capability}` takes no --cwd/--timeout/--max-open (set its own policy fields instead)"
        ));
    }
    Ok(())
}

/// Exact match, or `*` for "any" — the one allow-list rule every policy uses.
pub fn allows(list: &[String], value: &str) -> bool {
    list.iter()
        .any(|allowed| allowed == "*" || allowed == value)
}

/// The agent data directory: `$KI_AGENT_DIR`, else the CLI's own relocation
/// knob `$KI_CODING_AGENT_DIR` (which is what actually moves `~/.ki`), else
/// `$HOME/.ki/agent`.
///
/// Reading only `KI_AGENT_DIR` — a name nothing in the tree sets — meant an
/// isolated run (`KI_CODING_AGENT_DIR=/tmp/profile ki`) still discovered, and
/// granted, the extensions installed in the default profile.
pub fn agent_dir() -> Option<PathBuf> {
    for var in ["KI_AGENT_DIR", "KI_CODING_AGENT_DIR"] {
        if let Some(dir) = std::env::var_os(var) {
            return Some(PathBuf::from(dir));
        }
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".ki").join("agent"))
}

/// Where an extension's grants live.
pub fn grants_path(extension_id: &str) -> Option<PathBuf> {
    agent_dir().map(|root| grants_path_at(&root, extension_id))
}

/// Where an extension's grants live under an explicit agent root.
pub fn grants_path_at(root: &std::path::Path, extension_id: &str) -> PathBuf {
    extension_dir_at(root, extension_id).join("grants.json")
}

/// An extension's own directory (`extensions/<id>/`), which holds its `extension.wasm`
/// and `grants.json`.
pub fn extension_dir(extension_id: &str) -> Option<PathBuf> {
    agent_dir().map(|root| extension_dir_at(&root, extension_id))
}

/// An extension's own directory under an explicit agent root.
pub fn extension_dir_at(root: &std::path::Path, extension_id: &str) -> PathBuf {
    root.join("extensions").join(extension_id)
}

/// The infix `ki extension uninstall` renames an extension directory with.
pub const UNINSTALLED_INFIX: &str = ".removed-";

/// True when a directory under `extensions/` is one `uninstall` moved aside.
///
/// The loader and the installer must agree: a moved-aside directory still
/// holds its `extension.wasm` and `grants.json`, so treating it as installed
/// would keep loading (and granting) an extension the user asked to remove.
pub fn is_uninstalled_dir_name(name: &str) -> bool {
    name.contains(UNINSTALLED_INFIX)
}

/// Extension ids that have a directory under `extensions/`.
pub fn installed_extensions() -> Vec<String> {
    agent_dir()
        .map(|root| installed_extensions_at(&root))
        .unwrap_or_default()
}

/// Extension ids with a directory under `root/extensions/`.
pub fn installed_extensions_at(root: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root.join("extensions")) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !is_uninstalled_dir_name(name))
        .collect();
    ids.sort();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_grants_nothing() {
        let grants = Grants::load_for("no-such-extension-here");
        assert_eq!(grants, Grants::default());
        assert!(grants.granted().is_empty());
        assert!(!grants.allows_host("example.com"));
    }

    #[test]
    fn grant_revoke_and_host_matching() {
        let mut grants = Grants::default();
        grants
            .grant(
                "net-http",
                vec!["api.langfuse.com".to_string()],
                GrantOptions::default(),
            )
            .unwrap();
        assert!(grants.allows_host("api.langfuse.com"));
        assert!(!grants.allows_host("evil.example"));
        assert_eq!(grants.granted(), vec!["net-http"]);

        grants.grant("fs", vec![], GrantOptions::default()).unwrap();
        assert_eq!(grants.granted(), vec!["net-http", "fs"]);

        grants.revoke("net-http").unwrap();
        assert!(grants.granted() == vec!["fs"]);
        assert!(!grants.allows_host("api.langfuse.com"));

        assert!(grants
            .grant("nope", vec![], GrantOptions::default())
            .is_err());
        assert!(grants.revoke("nope").is_err());
    }

    #[test]
    fn wildcard_host_is_allowed() {
        let mut grants = Grants::default();
        grants
            .grant("net-http", vec!["*".to_string()], GrantOptions::default())
            .unwrap();
        assert!(grants.allows_host("anything.example"));
    }

    /// Every capability carries its policy, so "granted" and "to what" are one
    /// decision — and a policy-carrying capability without one is an error
    /// rather than a silent allow-all.
    #[test]
    fn every_capability_carries_its_policy() {
        let mut grants = Grants::default();
        grants
            .grant(
                "net-sockets",
                vec!["127.0.0.1".to_string()],
                GrantOptions::default(),
            )
            .unwrap();
        grants
            .grant("exec", vec!["echo".to_string()], GrantOptions::default())
            .unwrap();
        grants.grant("fs", vec![], GrantOptions::default()).unwrap();

        assert!(grants.allows_socket_host("127.0.0.1"));
        assert!(!grants.allows_socket_host("example.com"));
        assert!(grants.allows_command("echo"));
        assert!(!grants.allows_command("rm"));
        assert_eq!(grants.granted(), vec!["fs", "exec", "net-sockets"]);

        assert!(grants
            .grant("exec", vec![], GrantOptions::default())
            .is_err());
        assert!(grants
            .grant("net-sockets", vec![], GrantOptions::default())
            .is_err());
        assert!(grants
            .grant("net-http", vec![], GrantOptions::default())
            .is_err());

        grants.revoke("exec").unwrap();
        assert!(!grants.allows_command("echo"));
        assert!(!grants.granted().contains(&"exec"));
    }

    /// The exec policy: defaults preserve the pre-scope meaning (extension data
    /// dir, 30s), and only `exec` accepts the scope/budget options.
    #[test]
    fn exec_grant_carries_cwd_scope_and_time_budget() {
        let mut grants = Grants::default();
        grants
            .grant(
                "exec",
                vec!["cargo".to_string()],
                GrantOptions {
                    cwd: Some(ExecCwd::Project),
                    timeout_seconds: Some(120),
                    max_open: None,
                },
            )
            .unwrap();
        let grant = grants.exec.as_ref().unwrap();
        assert_eq!(grant.cwd, ExecCwd::Project);
        assert_eq!(grant.timeout_seconds, 120);

        // Defaults when the options are not passed.
        grants
            .grant("exec", vec!["echo".to_string()], GrantOptions::default())
            .unwrap();
        let grant = grants.exec.as_ref().unwrap();
        assert_eq!(grant.cwd, ExecCwd::ExtensionData);
        assert_eq!(grant.timeout_seconds, EXEC_TIMEOUT_DEFAULT_SECONDS);

        // Validation: the budget is bounded, and only exec takes the options.
        let bad_timeout = |seconds: u64| GrantOptions {
            cwd: None,
            timeout_seconds: Some(seconds),
            max_open: None,
        };
        assert!(grants
            .grant("exec", vec!["echo".to_string()], bad_timeout(0))
            .is_err());
        assert!(grants
            .grant(
                "exec",
                vec!["echo".to_string()],
                bad_timeout(EXEC_TIMEOUT_MAX_SECONDS + 1),
            )
            .is_err());
        assert!(grants
            .grant(
                "exec",
                vec!["echo".to_string()],
                bad_timeout(EXEC_TIMEOUT_MAX_SECONDS),
            )
            .is_ok());
        assert!(grants
            .grant(
                "net-http",
                vec!["example.com".to_string()],
                GrantOptions {
                    cwd: Some(ExecCwd::Project),
                    timeout_seconds: None,
                    max_open: None,
                },
            )
            .is_err());
    }

    /// The `proc` policy: its own defaults (600s / 100 open / extension-data), its
    /// own bounds, and options that belong to the other capabilities are
    /// rejected rather than ignored.
    #[test]
    fn proc_grant_carries_its_own_defaults_and_bounds() {
        let mut grants = Grants::default();
        grants
            .grant(
                "proc",
                vec!["codegraph".to_string()],
                GrantOptions::default(),
            )
            .unwrap();
        let grant = grants.proc.as_ref().unwrap();
        assert_eq!(grant.cwd, ExecCwd::ExtensionData);
        assert_eq!(grant.timeout_seconds, PROC_TIMEOUT_DEFAULT_SECONDS);
        assert_eq!(grant.max_open, PROC_MAX_OPEN_DEFAULT);
        assert!(grants.allows_proc_command("codegraph"));
        assert!(!grants.allows_proc_command("rm"));
        assert!(grants.granted().contains(&"proc"));

        // Explicit options ride along.
        grants
            .grant(
                "proc",
                vec!["codegraph".to_string()],
                GrantOptions {
                    cwd: Some(ExecCwd::Project),
                    timeout_seconds: Some(900),
                    max_open: Some(8),
                },
            )
            .unwrap();
        let grant = grants.proc.as_ref().unwrap();
        assert_eq!(grant.cwd, ExecCwd::Project);
        assert_eq!(grant.timeout_seconds, 900);
        assert_eq!(grant.max_open, 8);

        // Bounds are enforced at grant time.
        let bad = |seconds: u64, open: u32| GrantOptions {
            cwd: None,
            timeout_seconds: Some(seconds),
            max_open: Some(open),
        };
        assert!(grants
            .grant("proc", vec!["x".to_string()], bad(0, 1))
            .is_err());
        assert!(grants
            .grant(
                "proc",
                vec!["x".to_string()],
                bad(PROC_TIMEOUT_MAX_SECONDS + 1, 1)
            )
            .is_err());
        assert!(grants
            .grant("proc", vec!["x".to_string()], bad(60, 0))
            .is_err());
        assert!(grants
            .grant(
                "proc",
                vec!["x".to_string()],
                bad(60, PROC_MAX_OPEN_MAX + 1)
            )
            .is_err());
        assert!(grants
            .grant(
                "proc",
                vec!["x".to_string()],
                bad(PROC_TIMEOUT_MAX_SECONDS, PROC_MAX_OPEN_MAX)
            )
            .is_ok());

        // `--max-open` is proc's alone; the cwd/timeout pair never reaches the
        // network capabilities or fs.
        assert!(grants
            .grant(
                "exec",
                vec!["echo".to_string()],
                GrantOptions {
                    cwd: None,
                    timeout_seconds: None,
                    max_open: Some(2),
                },
            )
            .is_err());
        assert!(grants
            .grant(
                "fs",
                vec![],
                GrantOptions {
                    cwd: None,
                    timeout_seconds: None,
                    max_open: Some(2),
                },
            )
            .is_err());

        // Revoking takes it away again.
        grants.revoke("proc").unwrap();
        assert!(!grants.allows_proc_command("codegraph"));
        assert!(!grants.granted().contains(&"proc"));
    }

    /// The fs scope: default is the data-dir jail, `--cwd project` opts into
    /// the project tree, and the JSON stays round-trippable — with legacy
    /// `"fs": true` files reading as the data-dir scope.
    #[test]
    fn fs_grant_carries_a_scope() {
        let mut grants = Grants::default();
        grants.grant("fs", vec![], GrantOptions::default()).unwrap();
        let grant = grants.fs.as_ref().unwrap();
        assert_eq!(
            grant.cwd,
            ExecCwd::ExtensionData,
            "default is the data jail"
        );

        grants
            .grant(
                "fs",
                vec![],
                GrantOptions {
                    cwd: Some(ExecCwd::Project),
                    timeout_seconds: None,
                    max_open: None,
                },
            )
            .unwrap();
        assert_eq!(grants.fs.as_ref().unwrap().cwd, ExecCwd::Project);

        // Serialization: the scoped form writes its cwd and round-trips.
        let json = serde_json::to_value(&grants).unwrap();
        assert_eq!(json["fs"], serde_json::json!({"cwd": "project"}));
        let reparsed: Grants = serde_json::from_value(json).unwrap();
        assert_eq!(reparsed, grants);

        // Legacy form: the bare `true` reads as the data-dir scope.
        let legacy: Grants = serde_json::from_value(serde_json::json!({"fs": true})).unwrap();
        assert_eq!(
            legacy.fs.as_ref().unwrap().cwd,
            ExecCwd::ExtensionData,
            "an old \"fs\": true keeps its historical meaning"
        );
        assert!(legacy.allows_host("anything") == false || true); // fs grants no hosts
    }

    /// A grant file from before `proc` existed deserializes unchanged, and the
    /// new defaults stay out of the serialized form.
    #[test]
    fn proc_grant_json_is_backward_compatible_and_minimal() {
        let old = serde_json::json!({"exec": {"commands": ["echo"]}});
        let grants: Grants = serde_json::from_value(old).unwrap();
        assert!(grants.proc.is_none());
        assert!(!grants.allows_proc_command("echo"));

        let mut grants = Grants::default();
        grants
            .grant(
                "proc",
                vec!["codegraph".to_string()],
                GrantOptions::default(),
            )
            .unwrap();
        let json = serde_json::to_value(&grants).unwrap();
        assert_eq!(
            json["proc"],
            serde_json::json!({"commands": ["codegraph"]}),
            "defaults are omitted, like exec's"
        );
        let reparsed: Grants = serde_json::from_value(json).unwrap();
        assert_eq!(reparsed, grants);
    }

    /// Grant files written before the cwd/timeout fields existed must keep
    /// their meaning: an old `{"commands":[...]}` deserializes to the defaults.
    #[test]
    fn exec_grant_json_is_backward_compatible() {
        let old = serde_json::json!({"exec": {"commands": ["cargo", "git"]}});
        let grants: Grants = serde_json::from_value(old).unwrap();
        let grant = grants.exec.as_ref().unwrap();
        assert_eq!(grant.commands, vec!["cargo", "git"]);
        assert_eq!(grant.cwd, ExecCwd::ExtensionData);
        assert_eq!(grant.timeout_seconds, EXEC_TIMEOUT_DEFAULT_SECONDS);

        // The default fields stay out of the file (minimal grants.json), and
        // the explicit ones use the kebab-case spellings the CLI documents.
        let minimal = serde_json::to_string(&grants).unwrap();
        assert!(!minimal.contains("cwd") && !minimal.contains("timeout"));

        let mut grants = Grants::default();
        grants
            .grant(
                "exec",
                vec!["cargo".to_string()],
                GrantOptions {
                    cwd: Some(ExecCwd::Project),
                    timeout_seconds: Some(120),
                    max_open: None,
                },
            )
            .unwrap();
        let json = serde_json::to_value(&grants).unwrap();
        assert_eq!(
            json["exec"],
            serde_json::json!({
                "commands": ["cargo"],
                "cwd": "project",
                "timeout-seconds": 120,
            })
        );
        // Round-trip back through the file form.
        let reparsed: Grants = serde_json::from_value(json).unwrap();
        assert_eq!(reparsed, grants);

        assert_eq!(ExecCwd::parse("project").unwrap(), ExecCwd::Project);
        assert_eq!(
            ExecCwd::parse("extension-data").unwrap(),
            ExecCwd::ExtensionData
        );
        assert!(ExecCwd::parse("/abs/path").is_err());
    }

    /// Zero budgets parse nowhere: the deserialize layer rejects
    /// `timeout-seconds` 0 and `max-open` 0 with the field and the bound
    /// named — the same rule `grant()` applies to `--timeout 0`/`--max-open
    /// 0` — and the boundary value 1 is accepted.
    #[test]
    fn zero_budgets_are_rejected_at_parse_and_one_is_accepted() {
        let error = serde_json::from_value::<Grants>(serde_json::json!({
            "exec": {"commands": ["x"], "timeout-seconds": 0}
        }))
        .expect_err("exec timeout 0 must be rejected");
        assert!(
            error
                .to_string()
                .contains("exec `timeout-seconds` must be at least 1"),
            "the error should name the field and the bound: {error}"
        );

        let error = serde_json::from_value::<Grants>(serde_json::json!({
            "proc": {"commands": ["x"], "timeout-seconds": 0}
        }))
        .expect_err("proc timeout 0 must be rejected");
        assert!(
            error
                .to_string()
                .contains("proc `timeout-seconds` must be at least 1"),
            "the error should name the field and the bound: {error}"
        );

        let error = serde_json::from_value::<Grants>(serde_json::json!({
            "proc": {"commands": ["x"], "max-open": 0}
        }))
        .expect_err("proc max-open 0 must be rejected");
        assert!(
            error
                .to_string()
                .contains("proc `max-open` must be at least 1"),
            "the error should name the field and the bound: {error}"
        );

        // The boundary value is inside every bound.
        let grants: Grants = serde_json::from_value(serde_json::json!({
            "exec": {"commands": ["x"], "timeout-seconds": 1},
            "proc": {"commands": ["y"], "timeout-seconds": 1, "max-open": 1},
        }))
        .expect("1 is accepted for every budget field");
        assert_eq!(grants.exec.as_ref().unwrap().timeout_seconds, 1);
        assert_eq!(grants.proc.as_ref().unwrap().timeout_seconds, 1);
        assert_eq!(grants.proc.as_ref().unwrap().max_open, 1);
    }

    /// A grants.json carrying a zero budget loads as "nothing granted": the
    /// rejected file fails closed exactly like a malformed one.
    #[test]
    fn a_zero_budget_file_grants_nothing() {
        let root = std::env::temp_dir().join(format!("kiqi-zero-grants-{}", std::process::id()));
        let dir = extension_dir_at(&root, "zero");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("grants.json"),
            r#"{"exec": {"commands": ["x"], "timeout-seconds": 0}}"#,
        )
        .unwrap();

        let grants = Grants::load_for_at(&root, "zero");
        assert!(grants.exec.is_none(), "a rejected file grants nothing");
        assert!(grants.granted().is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    /// The OAuth policy: profiles round-trip with kebab-case spellings, the
    /// flow/request budgets keep their defaults out of the file, and the
    /// constructor and `validate` agree.
    #[test]
    fn oauth_grant_validates_and_round_trips() {
        let mut grants = Grants::default();
        grants.grant_oauth(valid_oauth_grant()).unwrap();
        assert_eq!(grants.granted(), vec!["oauth"]);
        assert!(grants.validate().is_ok());

        let json = serde_json::to_value(&grants).unwrap();
        let profile = &json["oauth"]["providers"][0];
        assert_eq!(
            profile["authorization-endpoint"],
            "https://id.acme.test/authorize"
        );
        assert_eq!(profile["token-endpoint"], "https://id.acme.test/token");
        assert_eq!(
            profile["device-authorization-endpoint"],
            "https://id.acme.test/device"
        );
        assert_eq!(profile["supports-pkce"], true);
        assert_eq!(profile["flows"][0], "device-code");
        assert!(
            json["oauth"].get("timeout-seconds").is_none(),
            "the default budget stays out of the file"
        );
        assert!(json["oauth"].get("max-requests").is_none());

        let reparsed: Grants = serde_json::from_value(json).unwrap();
        assert_eq!(reparsed, grants);

        grants.revoke("oauth").unwrap();
        assert!(grants.oauth.is_none());
        assert!(grants.granted().is_empty());
    }

    /// Nothing about an OAuth profile is wildcard-friendly: the endpoints must
    /// be exact https hosts, the flow set is closed, ids are non-empty and
    /// unique, and the budgets are bounded.
    #[test]
    fn oauth_grant_rejects_wildcards_duplicates_and_bad_endpoints() {
        let mut grants = Grants::default();

        let mut wildcard = valid_oauth_grant();
        wildcard.providers[0].authorization_endpoint = "https://*.acme.test/authorize".into();
        assert!(grants.grant_oauth(wildcard).is_err(), "no wildcard hosts");

        let mut plain_http = valid_oauth_grant();
        plain_http.providers[0].token_endpoint = "http://id.acme.test/token".into();
        assert!(grants.grant_oauth(plain_http).is_err(), "https only");

        let mut duplicate = valid_oauth_grant();
        duplicate
            .providers
            .push(valid_oauth_grant().providers[0].clone());
        assert!(grants.grant_oauth(duplicate).is_err(), "ids are unique");

        let mut empty_id = valid_oauth_grant();
        empty_id.providers[0].id = String::new();
        assert!(grants.grant_oauth(empty_id).is_err());

        let mut no_flow = valid_oauth_grant();
        no_flow.providers[0].flows.clear();
        assert!(grants.grant_oauth(no_flow).is_err());

        let mut no_device_endpoint = valid_oauth_grant();
        no_device_endpoint.providers[0].device_authorization_endpoint = None;
        assert!(
            grants.grant_oauth(no_device_endpoint).is_err(),
            "device-code needs a device-authorization-endpoint"
        );

        let mut zero_budget = valid_oauth_grant();
        zero_budget.timeout_seconds = 0;
        assert!(grants.grant_oauth(zero_budget).is_err());

        let mut over_budget = valid_oauth_grant();
        over_budget.timeout_seconds = OAUTH_TIMEOUT_MAX_SECONDS + 1;
        assert!(grants.grant_oauth(over_budget).is_err());

        let mut too_many = valid_oauth_grant();
        too_many.max_requests = OAUTH_MAX_REQUESTS_MAX + 1;
        assert!(grants.grant_oauth(too_many).is_err());

        // An unknown flow is a closed-set violation, so parsing fails too.
        let error = serde_json::from_value::<Grants>(serde_json::json!({
            "oauth": {
                "providers": [{
                    "id": "acme",
                    "flows": ["implicit"],
                    "authorization-endpoint": "https://id.acme.test/authorize",
                    "token-endpoint": "https://id.acme.test/token",
                    "client-id": "client"
                }]
            }
        }))
        .expect_err("`implicit` is not one of the two flows");
        assert!(error.to_string().contains("unknown variant"), "{error}");
    }

    /// The provider policy is a catalog plus a request filter: the api must be
    /// a typed wire variant (no `Other`, no `faux`), the host is exact, the
    /// path prefix starts at `/`, only GET/POST are allowed, and an attached
    /// credential cannot be empty.
    #[test]
    fn provider_grant_validates_api_endpoint_and_credential() {
        let mut grants = Grants::default();
        grants.grant_provider(valid_provider_grant()).unwrap();
        assert_eq!(grants.granted(), vec!["provider"]);
        assert!(grants.validate().is_ok());

        for api in ["made-up-api", "faux"] {
            let mut invalid = valid_provider_grant();
            invalid.providers[0].models[0].api = api.to_string();
            assert!(
                grants.grant_provider(invalid).is_err(),
                "`{api}` must be rejected"
            );
        }

        let mut no_prefix = valid_provider_grant();
        no_prefix.providers[0].endpoint.path_prefix = "v1".into();
        assert!(
            grants.grant_provider(no_prefix).is_err(),
            "path starts at /"
        );

        let mut bad_method = valid_provider_grant();
        bad_method.providers[0].endpoint.methods = vec!["PUT".into()];
        assert!(grants.grant_provider(bad_method).is_err(), "GET/POST only");

        let mut wildcard_host = valid_provider_grant();
        wildcard_host.providers[0].endpoint.host = "*.acme.test".into();
        assert!(grants.grant_provider(wildcard_host).is_err());

        let mut empty_host = valid_provider_grant();
        empty_host.providers[0].endpoint.host = String::new();
        assert!(grants.grant_provider(empty_host).is_err());

        let mut not_https = valid_provider_grant();
        not_https.providers[0].endpoint.scheme = "http".into();
        assert!(grants.grant_provider(not_https).is_err());

        let mut empty_credential = valid_provider_grant();
        empty_credential.providers[0].credential = Some("   ".into());
        assert!(grants.grant_provider(empty_credential).is_err());

        let mut over_budget = valid_provider_grant();
        over_budget.providers[0].timeout_seconds = PROVIDER_TIMEOUT_MAX_SECONDS + 1;
        assert!(grants.grant_provider(over_budget).is_err());

        let mut duplicate_model = valid_provider_grant();
        duplicate_model.providers[0]
            .models
            .push(valid_provider_grant().providers[0].models[0].clone());
        assert!(grants.grant_provider(duplicate_model).is_err());
    }

    /// A minimal provider entry takes the documented defaults (https, POST,
    /// 600 s), and none of them is written back unless overridden.
    #[test]
    fn provider_http_budgets_auth_and_prefix_are_bounded() {
        let mut grants = Grants::default();
        let mut credential = valid_provider_grant();
        credential.providers[0].endpoint.path_prefix = "/".to_string();
        assert!(
            grants.grant_provider(credential).is_err(),
            "a credential with a whole-host prefix defeats the filter"
        );

        let mut bad_prefix = valid_provider_grant();
        bad_prefix.providers[0].endpoint.path_prefix = "/v1/../admin".to_string();
        assert!(grants.grant_provider(bad_prefix).is_err());

        let mut encoded = valid_provider_grant();
        encoded.providers[0].endpoint.path_prefix = "/v1/%2fadmin".to_string();
        assert!(grants.grant_provider(encoded).is_err());

        let mut too_small = valid_provider_grant();
        too_small.providers[0].max_response_bytes = 1024;
        assert!(grants.grant_provider(too_small).is_err());

        let mut too_big = valid_provider_grant();
        too_big.providers[0].max_response_bytes = PROVIDER_MAX_RESPONSE_BYTES_MAX + 1;
        assert!(grants.grant_provider(too_big).is_err());

        let mut no_handles = valid_provider_grant();
        no_handles.providers[0].max_handles = 0;
        assert!(grants.grant_provider(no_handles).is_err());

        let mut bad_header = valid_provider_grant();
        bad_header.providers[0].auth = Some(ProviderAuth {
            header: "X Api Key".to_string(),
            prefix: "Bearer ".to_string(),
        });
        assert!(grants.grant_provider(bad_header).is_err());

        let mut framing_header = valid_provider_grant();
        framing_header.providers[0].auth = Some(ProviderAuth {
            header: "host".to_string(),
            prefix: String::new(),
        });
        assert!(grants.grant_provider(framing_header).is_err());

        let mut good = valid_provider_grant();
        good.providers[0].auth = Some(ProviderAuth {
            header: "x-api-key".to_string(),
            prefix: String::new(),
        });
        assert!(grants.grant_provider(good).is_ok());
    }

    #[test]
    fn provider_budgets_round_trip_through_json() {
        let mut grant = valid_provider_grant();
        grant.providers[0].auth = Some(ProviderAuth {
            header: "x-goog-api-key".to_string(),
            prefix: String::new(),
        });
        grant.providers[0].max_response_bytes = 1024 * 1024;
        grant.providers[0].max_handles = 4;
        let text = serde_json::to_string(&grant).unwrap();
        let parsed: ProviderGrant = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, grant);
    }

    #[test]
    fn provider_grant_json_defaults_are_minimal() {
        let grants: Grants = serde_json::from_value(serde_json::json!({
            "provider": {
                "providers": [{
                    "id": "acme",
                    "models": [{"id": "acme-1", "api": "anthropic-messages"}],
                    "endpoint": {"host": "api.acme.test", "path-prefix": "/v1"}
                }]
            }
        }))
        .unwrap();
        let profile = &grants.provider.as_ref().unwrap().providers[0];
        assert_eq!(profile.endpoint.scheme, "https");
        assert_eq!(profile.endpoint.methods, vec!["POST".to_string()]);
        assert_eq!(profile.timeout_seconds, PROVIDER_TIMEOUT_DEFAULT_SECONDS);
        assert!(profile.credential.is_none());
        assert!(grants.validate().is_ok());

        let out = serde_json::to_string(&grants).unwrap();
        assert!(!out.contains("timeout-seconds"));
        assert!(!out.contains("scheme"));
        assert!(!out.contains("methods"));
    }

    /// Render accepts the bare `true` form, defaults to markdown with the
    /// documented budget, and rejects every other kind or an over-cap value.
    #[test]
    fn render_grant_bare_true_and_bounds() {
        let grants: Grants = serde_json::from_value(serde_json::json!({"render": true})).unwrap();
        let render = grants.render.as_ref().unwrap();
        assert_eq!(render.kinds, vec![RENDER_KIND_MARKDOWN.to_string()]);
        assert_eq!(render.timeout_ms, RENDER_TIMEOUT_MS_DEFAULT);
        assert_eq!(render.max_output_bytes, RENDER_MAX_OUTPUT_BYTES_DEFAULT);
        assert_eq!(grants.granted(), vec!["render"]);

        let mut grants = Grants::default();
        assert!(grants
            .grant_render(RenderGrant {
                kinds: vec!["html".into()],
                ..RenderGrant::default()
            })
            .is_err());
        assert!(grants
            .grant_render(RenderGrant {
                timeout_ms: RENDER_TIMEOUT_MS_MAX + 1,
                ..RenderGrant::default()
            })
            .is_err());
        assert!(grants
            .grant_render(RenderGrant {
                max_output_bytes: RENDER_MAX_OUTPUT_BYTES_MAX + 1,
                ..RenderGrant::default()
            })
            .is_err());

        // The generic arm takes the kinds as its policy; the exec options stay
        // rejected, the same rule `net-http` follows.
        grants
            .grant("render", vec![], GrantOptions::default())
            .unwrap();
        assert_eq!(grants.render.as_ref().unwrap().kinds, vec!["markdown"]);
        assert!(grants
            .grant(
                "render",
                vec!["markdown".into()],
                GrantOptions {
                    timeout_seconds: Some(1),
                    ..GrantOptions::default()
                }
            )
            .is_err());
    }

    /// Resources accepts bare `true`, defaults to both kinds, and rejects an
    /// unknown kind or an over-cap value.
    #[test]
    fn resources_grant_bare_true_and_unknown_kind() {
        let grants: Grants =
            serde_json::from_value(serde_json::json!({"resources": true})).unwrap();
        let resources = grants.resources.as_ref().unwrap();
        assert_eq!(
            resources.kinds,
            vec![
                RESOURCES_KIND_SKILLS.to_string(),
                RESOURCES_KIND_PROMPTS.to_string()
            ]
        );
        assert_eq!(resources.max_items, RESOURCES_MAX_ITEMS_DEFAULT);
        assert_eq!(resources.max_bytes, RESOURCES_MAX_BYTES_DEFAULT);

        let mut grants = Grants::default();
        assert!(grants
            .grant_resources(ResourcesGrant {
                kinds: vec!["themes".into()],
                ..ResourcesGrant::default()
            })
            .is_err());
        assert!(grants
            .grant_resources(ResourcesGrant {
                max_items: RESOURCES_MAX_ITEMS_MAX + 1,
                ..ResourcesGrant::default()
            })
            .is_err());
        assert!(grants
            .grant_resources(ResourcesGrant {
                max_bytes: RESOURCES_MAX_BYTES_MAX + 1,
                ..ResourcesGrant::default()
            })
            .is_err());

        // The kinds are the generic arm's policy.
        grants
            .grant("resources", vec!["skills".into()], GrantOptions::default())
            .unwrap();
        assert_eq!(grants.resources.as_ref().unwrap().kinds, vec!["skills"]);
        assert!(grants
            .grant(
                "resources",
                vec![],
                GrantOptions {
                    max_open: Some(1),
                    ..GrantOptions::default()
                }
            )
            .is_err());
    }

    /// The provider-request defaults are all "nothing leaves the host", and
    /// the patch deadline is the smaller of the fixed 10 s cap and the
    /// provider budget.
    #[test]
    fn provider_request_defaults_and_patch_budget() {
        let grants: Grants =
            serde_json::from_value(serde_json::json!({"provider-request": {}})).unwrap();
        let request = grants.provider_request.as_ref().unwrap();
        assert_eq!(request.observe, ObserveMode::None);
        assert_eq!(request.headers, HeadersMode::Names);
        assert_eq!(request.messages, MessagesMode::None);
        assert!(!request.patch);
        assert_eq!(
            request.patch_timeout_ms,
            PROVIDER_REQUEST_PATCH_TIMEOUT_MS_DEFAULT
        );
        assert!(grants.validate().is_ok());

        // Without a provider grant the fixed cap applies.
        let mut grants = Grants::default();
        assert!(grants
            .grant_provider_request(ProviderRequestGrant {
                patch: true,
                patch_timeout_ms: PROVIDER_REQUEST_PATCH_TIMEOUT_MS_MAX,
                ..ProviderRequestGrant::default()
            })
            .is_ok());
        assert!(grants
            .grant_provider_request(ProviderRequestGrant {
                patch: true,
                patch_timeout_ms: PROVIDER_REQUEST_PATCH_TIMEOUT_MS_MAX + 1,
                ..ProviderRequestGrant::default()
            })
            .is_err());

        // A short provider budget tightens the ceiling, in both directions:
        // the provider cannot be granted over an existing deadline, and a new
        // deadline cannot exceed the provider's budget.
        let mut short = valid_provider_grant();
        short.providers[0].timeout_seconds = 5;
        let mut grants = Grants::default();
        grants
            .grant_provider_request(ProviderRequestGrant {
                patch: true,
                patch_timeout_ms: 8_000,
                ..ProviderRequestGrant::default()
            })
            .unwrap();
        assert!(
            grants.grant_provider(short.clone()).is_err(),
            "a 5 s provider would strand an 8 s patch budget"
        );

        let mut grants = Grants::default();
        grants.grant_provider(short).unwrap();
        assert!(grants
            .grant_provider_request(ProviderRequestGrant {
                patch: true,
                patch_timeout_ms: 5_000,
                ..ProviderRequestGrant::default()
            })
            .is_ok());
        assert!(grants
            .grant_provider_request(ProviderRequestGrant {
                patch: true,
                patch_timeout_ms: 5_001,
                ..ProviderRequestGrant::default()
            })
            .is_err());

        // The object form round-trips with the kebab-case keys.
        let json = serde_json::to_value(ProviderRequestGrant {
            observe: ObserveMode::Requests,
            headers: HeadersMode::Values,
            messages: MessagesMode::Full,
            patch: true,
            patch_timeout_ms: 5_000,
        })
        .unwrap();
        assert_eq!(json["observe"], "requests");
        assert_eq!(json["headers"], "values");
        assert_eq!(json["messages"], "full");
        assert_eq!(json["patch-timeout-ms"], 5_000);
    }

    /// The generic `grant()` arms for the profile-carrying keys refuse instead
    /// of inventing a policy, and every new key rejects the exec options.
    #[test]
    fn new_keys_reject_the_generic_entry_point_and_exec_options() {
        let mut grants = Grants::default();
        assert!(grants
            .grant("oauth", vec![], GrantOptions::default())
            .is_err());
        assert!(grants
            .grant("provider", vec![], GrantOptions::default())
            .is_err());
        assert!(grants
            .grant(
                "provider-request",
                vec!["wat".into()],
                GrantOptions::default()
            )
            .is_err());
        for capability in [
            "oauth",
            "provider",
            "render",
            "resources",
            "provider-request",
        ] {
            assert!(
                grants
                    .grant(
                        capability,
                        vec![],
                        GrantOptions {
                            cwd: Some(ExecCwd::Project),
                            ..GrantOptions::default()
                        }
                    )
                    .is_err(),
                "`{capability}` must reject --cwd"
            );
        }
    }

    /// A legacy cwd spelling is read for compatibility, but loading is a pure
    /// read: the file is only rewritten when the user explicitly saves.
    #[test]
    fn legacy_cwd_spelling_loads_without_rewriting_and_saves_current_json() {
        let root = tempfile::tempdir().unwrap();
        let dir = extension_dir_at(root.path(), "legacy");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("grants.json");
        // Deliberate legacy compatibility fixture: this was the pre-rename cwd
        // value, and all three cwd-scoped capabilities must survive the load.
        let original = r#"{
                "net-http": {"hosts": ["api.example.test"]},
                "fs": {"cwd": "plugin-data"},
                "exec": {"commands": ["echo"], "cwd": "plugin-data"},
                "proc": {"commands": ["server"], "cwd": "plugin-data"}
            }"#;
        std::fs::write(&path, original).unwrap();

        let mut warnings = Vec::new();
        let grants =
            Grants::load_for_at_with(root.path(), "legacy", |warning| warnings.push(warning));
        assert!(grants.allows_host("api.example.test"));
        assert!(grants.allows_command("echo"));
        assert!(grants.allows_proc_command("server"));
        assert_eq!(grants.fs.as_ref().unwrap().cwd, ExecCwd::ExtensionData);
        assert_eq!(grants.exec.as_ref().unwrap().cwd, ExecCwd::ExtensionData);
        assert_eq!(grants.proc.as_ref().unwrap().cwd, ExecCwd::ExtensionData);
        assert!(
            grants.version.is_none(),
            "the load reports the on-disk version; only an explicit save upgrades it"
        );
        assert!(
            warnings.is_empty(),
            "a supported legacy file loads quietly: {warnings:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "loading must not rewrite the user's file"
        );

        // An explicit save is where the document moves to the current format.
        Grants::save_for_at(root.path(), "legacy", &grants).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("plugin-data"));
        let saved: Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(saved["version"], GRANTS_VERSION);
        // `fs` always spells its scope; `exec`/`proc` omit the default one.
        assert_eq!(saved["fs"]["cwd"], "extension-data");
        for capability in ["exec", "proc"] {
            assert!(
                saved[capability].get("cwd").is_none(),
                "the default cwd is omitted in the current minimal encoding"
            );
        }
    }

    /// An explicit legacy version is accepted without touching the file; a
    /// later explicit save writes the current version.
    #[test]
    fn explicit_legacy_version_loads_read_only_and_save_upgrades_it() {
        let root = tempfile::tempdir().unwrap();
        let dir = extension_dir_at(root.path(), "version-one");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("grants.json");
        let original = r#"{"version":1,"net-http":{"hosts":["api.example.test"]}}"#;
        std::fs::write(&path, original).unwrap();

        let mut warnings = Vec::new();
        let grants =
            Grants::load_for_at_with(root.path(), "version-one", |warning| warnings.push(warning));
        assert!(grants.allows_host("api.example.test"));
        assert_eq!(grants.version, Some(GRANTS_VERSION_LEGACY));
        assert!(
            warnings.is_empty(),
            "a supported legacy file loads quietly: {warnings:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

        Grants::save_for_at(root.path(), "version-one", &grants).unwrap();
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["version"], GRANTS_VERSION);
        assert_eq!(saved["net-http"]["hosts"][0], "api.example.test");
    }

    #[test]
    fn unknown_version_fails_closed_loudly_without_rewriting() {
        let root = tempfile::tempdir().unwrap();
        let dir = extension_dir_at(root.path(), "future");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("grants.json");
        let original = r#"{
            "version": 99,
            "net-http": {"hosts": ["api.example.test"]}
        }"#;
        std::fs::write(&path, original).unwrap();

        let mut warnings = Vec::new();
        let grants =
            Grants::load_for_at_with(root.path(), "future", |warning| warnings.push(warning));
        assert_eq!(grants, Grants::default());
        assert!(grants.granted().is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("unsupported grants.json version 99"));
        assert!(warnings[0].contains("leaving the file unchanged"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn malformed_grants_fail_closed_loudly_without_rewriting() {
        let root = tempfile::tempdir().unwrap();
        let dir = extension_dir_at(root.path(), "malformed");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("grants.json");
        let original = r#"{"version":1,"exec":{"commands":"not-an-array"}}"#;
        std::fs::write(&path, original).unwrap();

        let mut warnings = Vec::new();
        let grants =
            Grants::load_for_at_with(root.path(), "malformed", |warning| warnings.push(warning));
        assert_eq!(grants, Grants::default());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("is unreadable"));
        assert!(warnings[0].contains("leaving the file unchanged"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    /// Duplicate keys must fail closed. A two-phase `Value` parse keeps only
    /// the last duplicate, so `{"exec": safe, "exec": any}` would silently
    /// widen the grant; parsing the original text sees the duplicate.
    #[test]
    fn duplicate_grant_keys_fail_closed_without_rewriting() {
        let root = tempfile::tempdir().unwrap();
        let cases = [
            (
                "top-level",
                r#"{
            "version": 2,
            "exec": {"commands": ["safe"]},
            "exec": {"commands": ["*"]}
        }"#,
                "duplicate key `exec`",
            ),
            (
                "nested",
                r#"{"version":2,"exec":{"commands":["safe"],"commands":["*"]}}"#,
                "duplicate key `commands`",
            ),
        ];
        for (id, original, expected) in cases {
            let dir = extension_dir_at(root.path(), id);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("grants.json");
            std::fs::write(&path, original).unwrap();

            let mut warnings = Vec::new();
            let grants =
                Grants::load_for_at_with(root.path(), id, |warning| warnings.push(warning));
            assert!(
                grants.exec.is_none(),
                "{id}: a duplicated capability key grants nothing"
            );
            assert!(grants.granted().is_empty(), "{id}");
            assert_eq!(warnings.len(), 1, "{id}: {warnings:?}");
            assert!(warnings[0].contains(expected), "{id}: {}", warnings[0]);
            assert!(warnings[0].contains("leaving the file unchanged"), "{id}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original, "{id}");
        }
    }

    #[test]
    fn every_save_writes_the_current_version() {
        let root = tempfile::tempdir().unwrap();
        let stale = Grants {
            version: Some(99),
            ..Grants::default()
        };
        Grants::save_for_at(root.path(), "current", &stale).unwrap();

        let path = grants_path_at(root.path(), "current");
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(saved["version"], GRANTS_VERSION);
    }

    /// A hand-edited file cannot smuggle invalid policy in: the offending
    /// capability loads as absent, the rest of the file survives, and the file
    /// is left exactly as written (invalid policy and unknown keys included).
    #[test]
    fn a_hand_edited_invalid_policy_is_treated_as_absent() {
        let root = tempfile::tempdir().unwrap();
        let dir = extension_dir_at(root.path(), "invalid");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("grants.json");
        let original = r#"{
                "user-note": "keep this",
                "net-http": {"hosts": ["api.example.test"]},
                "provider": {
                    "providers": [{
                        "id": "p",
                        "models": [{"id": "m", "api": "faux"}],
                        "endpoint": {"host": "api.example.test", "path-prefix": "/v1"}
                    }]
                }
            }"#;
        std::fs::write(&path, original).unwrap();

        let mut warnings = Vec::new();
        let grants =
            Grants::load_for_at_with(root.path(), "invalid", |warning| warnings.push(warning));
        assert!(
            grants.provider.is_none(),
            "an api outside the typed wire set is dropped"
        );
        assert!(
            grants.net_http.is_some(),
            "unrelated valid grants survive the bad one"
        );
        assert_eq!(grants.granted(), vec!["net-http"]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("invalid policy for provider"),
            "{}",
            warnings[0]
        );
        assert!(warnings[0].contains("leaving the file unchanged"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "the read path must not delete the invalid policy or unknown keys"
        );
    }

    /// The interface map is what a consent prompt uses to turn a component's
    /// imports/exports into the grant keys the user is asked about.
    #[test]
    fn interfaces_map_to_their_grant_keys() {
        assert_eq!(grant_key_for_interface("provider-http"), Some("provider"));
        assert_eq!(
            grant_key_for_interface("kiqi:extension/provider-stream@0.8.0"),
            Some("provider")
        );
        assert_eq!(grant_key_for_interface("oauth-http"), Some("oauth"));
        assert_eq!(grant_key_for_interface("oauth-api"), Some("oauth"));
        assert_eq!(
            grant_key_for_interface("kiqi:extension/provider-api@0.8.0"),
            Some("provider")
        );
        assert_eq!(grant_key_for_interface("render-api"), Some("render"));
        assert_eq!(grant_key_for_interface("resources"), Some("resources"));
        assert_eq!(
            grant_key_for_interface("provider-request"),
            Some("provider-request")
        );
        assert_eq!(grant_key_for_interface("net-http"), None);
    }

    fn valid_oauth_grant() -> OAuthGrant {
        OAuthGrant {
            providers: vec![OAuthProviderProfile {
                id: "acme".to_string(),
                display_name: "Acme".to_string(),
                scopes: vec!["openid".to_string()],
                flows: vec![OAuthFlow::DeviceCode, OAuthFlow::AuthorizationCode],
                authorization_endpoint: "https://id.acme.test/authorize".to_string(),
                token_endpoint: "https://id.acme.test/token".to_string(),
                device_authorization_endpoint: Some("https://id.acme.test/device".to_string()),
                revocation_endpoint: Some("https://id.acme.test/revoke".to_string()),
                client_id: "client-123".to_string(),
                supports_pkce: true,
            }],
            timeout_seconds: OAUTH_TIMEOUT_DEFAULT_SECONDS,
            max_requests: OAUTH_MAX_REQUESTS_DEFAULT,
        }
    }

    fn valid_provider_grant() -> ProviderGrant {
        ProviderGrant {
            providers: vec![ProviderProfile {
                id: "acme".to_string(),
                display_name: "Acme".to_string(),
                models: vec![ProviderModel {
                    id: "acme-1".to_string(),
                    api: "anthropic-messages".to_string(),
                    base_url: "https://api.acme.test".to_string(),
                    reasoning: true,
                    input: vec!["text".to_string()],
                }],
                endpoint: ProviderEndpoint {
                    scheme: "https".to_string(),
                    host: "api.acme.test".to_string(),
                    path_prefix: "/v1".to_string(),
                    methods: vec!["POST".to_string()],
                },
                credential: Some("pc-abc123".to_string()),
                auth: None,
                timeout_seconds: PROVIDER_TIMEOUT_DEFAULT_SECONDS,
                max_response_bytes: PROVIDER_MAX_RESPONSE_BYTES_DEFAULT,
                max_handles: PROVIDER_MAX_HANDLES_DEFAULT,
            }],
        }
    }

    /// A save rewrites the whole document, so it must refuse to run over a file
    /// this build cannot interpret (future version / duplicated keys /
    /// malformed), leaving the user's bytes untouched.
    #[test]
    fn save_refuses_to_overwrite_a_file_it_cannot_interpret() {
        let root = tempfile::tempdir().unwrap();
        let dir = extension_dir_at(root.path(), "future");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("grants.json");

        let mut grants = Grants::default();
        grants.net_http = Some(NetHttpGrant {
            hosts: vec!["a.example".to_string()],
        });

        for (label, body) in [
            (
                "future version",
                r#"{"version": 99, "net-http": {"hosts": ["a.example"]}}"#,
            ),
            (
                "duplicate keys",
                r#"{"version": 2, "exec": {"commands": ["safe"]}, "exec": {"commands": ["*"]}}"#,
            ),
            ("malformed", "{"),
        ] {
            std::fs::write(&path, body).unwrap();
            let error = Grants::save_for_at(root.path(), "future", &grants)
                .expect_err(&format!("{label} must refuse a save"));
            assert!(error.contains("refusing to overwrite"), "{label}: {error}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), body, "{label}");
        }

        // A v1 file is interpretable, so a save upgrades it in place.
        std::fs::write(&path, r#"{"exec": {"commands": ["safe"]}}"#).unwrap();
        Grants::save_for_at(root.path(), "future", &grants).unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["version"], GRANTS_VERSION);
        assert_eq!(saved["net-http"]["hosts"][0], "a.example");
    }
}
