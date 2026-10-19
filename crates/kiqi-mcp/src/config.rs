//! `mcpServers` config for ki's host-level MCP client.
//!
//! The on-disk shape is reused verbatim from the retired `kiqi-mcp-adapter`
//! (`rpi-package/packages/kiqi-mcp-adapter/src/config.rs`) so users' existing
//! files port unchanged: an object keyed by server name, each value either a
//! stdio server (discriminated by `command`) or an HTTP server (by `url`).
//!
//! Two deliberate departures from the adapter:
//! * the adapter's `exposure` field is **not** modeled — it was parsed and then
//!   never enforced (dead config). A ported file that still carries it parses
//!   fine: unknown keys are ignored, only the *discriminator* is validated.
//! * validation is explicit and per entry: one malformed server is dropped with
//!   a reported issue instead of poisoning the whole block.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;

/// `timeoutSeconds` default (adapter parity).
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// `timeoutSeconds` lower clamp (adapter parity).
pub const MIN_TIMEOUT_SECS: u64 = 1;
/// `timeoutSeconds` upper clamp (adapter parity).
pub const MAX_TIMEOUT_SECS: u64 = 300;

/// `signals.minIntervalSeconds` default: how often an `auto` server may wake
/// the session. New opt-in behavior; no prior value to preserve.
pub const DEFAULT_MIN_INTERVAL_SECS: u64 = 30;
/// `signals.maxRunsPerHour` default.
pub const DEFAULT_MAX_RUNS_PER_HOUR: u32 = 6;
/// `signals.maxConsecutiveRuns` default: a signal-triggered chain stops here.
pub const DEFAULT_MAX_CONSECUTIVE_RUNS: u32 = 2;
const MIN_SIGNAL_INTERVAL_SECS: u64 = 1;
const MAX_SIGNAL_INTERVAL_SECS: u64 = 3600;
const MIN_SIGNAL_RUNS_PER_HOUR: u32 = 1;
const MAX_SIGNAL_RUNS_PER_HOUR: u32 = 60;
const MIN_SIGNAL_CONSECUTIVE_RUNS: u32 = 1;
const MAX_SIGNAL_CONSECUTIVE_RUNS: u32 = 20;

/// What a resource-update signal may do to the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WakeMode {
    /// Record the notice in the transcript; never start a run (today's
    /// behavior when nothing is configured).
    #[default]
    Off,
    /// Record + surface the notice to the user's client; never start a run.
    Notify,
    /// Record + wake the session with a synthetic trigger, subject to the
    /// host's budget. The agent decides what to do after waking.
    Auto,
}

/// Host-side wake policy for one server's resource signals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalsConfig {
    pub wake: WakeMode,
    pub min_interval: Duration,
    pub max_runs_per_hour: u32,
    pub max_consecutive_runs: u32,
}

impl Default for SignalsConfig {
    fn default() -> Self {
        Self {
            // No subscription, no signals: the default stays today's behavior.
            wake: WakeMode::Off,
            min_interval: Duration::from_secs(DEFAULT_MIN_INTERVAL_SECS),
            max_runs_per_hour: DEFAULT_MAX_RUNS_PER_HOUR,
            max_consecutive_runs: DEFAULT_MAX_CONSECUTIVE_RUNS,
        }
    }
}

/// Top-level `signals` defaults (global settings, optionally overridden by a
/// trusted project). Every field is optional: an absent field falls through to
/// the per-server value / the `subscribe`-derived default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignalsDefaults {
    pub wake: Option<WakeMode>,
    pub min_interval: Option<Duration>,
    pub max_runs_per_hour: Option<u32>,
    pub max_consecutive_runs: Option<u32>,
}

/// Field-wise merge: the project block wins over the global one.
pub fn merge_signals_defaults(
    global: SignalsDefaults,
    project: SignalsDefaults,
) -> SignalsDefaults {
    SignalsDefaults {
        wake: project.wake.or(global.wake),
        min_interval: project.min_interval.or(global.min_interval),
        max_runs_per_hour: project.max_runs_per_hour.or(global.max_runs_per_hour),
        max_consecutive_runs: project.max_consecutive_runs.or(global.max_consecutive_runs),
    }
}

/// Parse the top-level `signals` block. Invalid fields are reported and
/// ignored; one bad field must not drop the whole block (and must not take the
/// MCP servers down with it).
pub fn parse_signals_defaults(raw: Option<&serde_json::Value>) -> (SignalsDefaults, Vec<String>) {
    let mut out = SignalsDefaults::default();
    let mut issues = Vec::new();
    let Some(value) = raw else {
        return (out, issues);
    };
    if !value.is_object() {
        issues.push("`signals` must be a JSON object".to_string());
        return (out, issues);
    }
    let raw: RawSignals = match serde_json::from_value(value.clone()) {
        Ok(raw) => raw,
        Err(error) => {
            issues.push(format!("`signals`: invalid definition: {error}"));
            return (out, issues);
        }
    };
    apply_signals_fields(raw, &mut out, &mut issues);
    (out, issues)
}

/// Where a configured server's transport goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    Stdio(StdioTransport),
    /// An HTTP server, served over the MCP Streamable HTTP transport
    /// (`src/http.rs` supplies the reqwest client; rmcp owns the protocol).
    Http(HttpTransport),
}

/// Stdio server definition — the external process ki spawns and talks to over
/// newline-delimited JSON-RPC on its stdin/stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdioTransport {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
}

/// HTTP server definition served over the MCP Streamable HTTP transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpTransport {
    pub url: String,
    pub headers: BTreeMap<String, String>,
}

/// A validated server entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub name: String,
    pub transport: Transport,
    pub enabled: bool,
    pub timeout: Duration,
    /// Resource uris ki subscribes to right after the handshake. Durable,
    /// config-driven watches; the `{server}__watch_resource` tool can add
    /// session-scoped watches at runtime. The restore-on-reconnect machinery
    /// keeps configured watches alive for the session.
    pub subscribe: Vec<String>,
    /// Resource uris whose updates are read eagerly at the boundary instead of
    /// delivering the light (server, uri) notice. Opt-in per server: the
    /// product semantic stays "notice, not content" unless the user lists the
    /// uri here.
    pub auto_read: Vec<String>,
    /// What this server's resource-update signals may do. Default is derived
    /// at parse time: an explicit `subscribe` list is the user's opt-in to be
    /// told, so it defaults to [`WakeMode::Auto`]; no subscription means
    /// [`WakeMode::Off`] (today's behavior).
    pub signals: SignalsConfig,
}

/// The raw entry as it appears in `mcpServers.<name>`. Every field is optional
/// so a partially-specified entry produces a specific validation message rather
/// than a serde error, and unknown keys (e.g. a legacy `exposure`) are ignored.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawServer {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    #[serde(default)]
    subscribe: Option<Vec<String>>,
    #[serde(default)]
    auto_read: Option<Vec<String>>,
    #[serde(default)]
    signals: Option<RawSignals>,
}

/// `signals` block. Unknown keys are ignored like the rest of the entry; the
/// numbers are clamped so a typo cannot set the budget to zero or unbounded.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSignals {
    #[serde(default)]
    wake: Option<String>,
    #[serde(default)]
    min_interval_seconds: Option<u64>,
    #[serde(default)]
    max_runs_per_hour: Option<u32>,
    #[serde(default)]
    max_consecutive_runs: Option<u32>,
}

/// A server name must be non-empty ASCII `[A-Za-z0-9_-]` WITHOUT the tool-name
/// separator — it is not only an identifier but the namespace half of every
/// tool it contributes (`{name}__{tool}`, parsed by splitting on the FIRST
/// `__`), so a name containing `__` itself would make the expanded names
/// ambiguous: server `a__b` contributing tool `t` would parse as server `a`.
pub fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(crate::manager::TOOL_NAME_SEPARATOR)
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Validate an HTTP server URL: `http`/`https`, a host, and NO embedded
/// credentials. reqwest turns `user:password@` into a Basic Authorization
/// header on every request (a credential channel ki does not document), and
/// the URL is logged — so it is refused here, at every entry point, before it
/// can be printed.
pub fn validate_http_url(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|error| format!("invalid `url`: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("`url` must use http or https".to_string());
    }
    if parsed.host_str().is_none() {
        return Err("`url` has no host".to_string());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("`url` must not embed credentials (user:password@host)".to_string());
    }
    Ok(())
}

/// Parse and validate the whole `mcpServers` block.
///
/// Returns the accepted servers in declaration order plus one human-readable
/// issue per rejected entry. The caller is expected to print every issue to
/// stderr (loud, never silent) — nothing is dropped without a reason.
pub fn parse_servers(raw: Option<&serde_json::Value>) -> (Vec<ServerConfig>, Vec<String>) {
    parse_servers_with_defaults(raw, &SignalsDefaults::default())
}

/// Same as [`parse_servers`], with top-level `signals` defaults applied to
/// every entry (per-server fields still win).
pub fn parse_servers_with_defaults(
    raw: Option<&serde_json::Value>,
    defaults: &SignalsDefaults,
) -> (Vec<ServerConfig>, Vec<String>) {
    let mut out = Vec::new();
    let mut issues = Vec::new();
    let Some(value) = raw else {
        return (out, issues);
    };
    let Some(map) = value.as_object() else {
        issues.push("mcpServers must be a JSON object of server definitions".to_string());
        return (out, issues);
    };
    for (name, entry) in map {
        match parse_one(name, entry, defaults) {
            Ok(config) => out.push(config),
            Err(reason) => issues.push(format!("mcpServers.{name}: {reason}")),
        }
    }
    (out, issues)
}

fn parse_one(
    name: &str,
    entry: &serde_json::Value,
    defaults: &SignalsDefaults,
) -> Result<ServerConfig, String> {
    if !valid_server_name(name) {
        return Err(format!(
            "invalid server name (allowed: [A-Za-z0-9_-], non-empty, no {:?})",
            crate::manager::TOOL_NAME_SEPARATOR
        ));
    }
    let raw: RawServer = serde_json::from_value(entry.clone())
        .map_err(|error| format!("invalid server definition: {error}"))?;

    // The discriminator: exactly one of `command`/`url`. Both or neither is a
    // contradictory definition, not a shape we can guess at.
    let has_command = raw.command.is_some();
    let has_url = raw.url.is_some();
    if has_command == has_url {
        return Err("must define exactly one of `command` (stdio) or `url` (http)".to_string());
    }

    let timeout = Duration::from_secs(
        raw.timeout_seconds
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS),
    );

    let transport = if let Some(command) = raw.command {
        let command = command.trim();
        if command.is_empty() {
            return Err("`command` is empty".to_string());
        }
        Transport::Stdio(StdioTransport {
            command: command.to_string(),
            args: raw.args.unwrap_or_default(),
            env: raw.env.unwrap_or_default(),
            cwd: raw.cwd,
        })
    } else {
        let url = raw.url.unwrap_or_default();
        let url = url.trim();
        if url.is_empty() {
            return Err("`url` is empty".to_string());
        }
        validate_http_url(url)?;
        Transport::Http(HttpTransport {
            url: url.to_string(),
            headers: raw.headers.unwrap_or_default(),
        })
    };

    // `subscribe` uris are wire values sent verbatim in `resources/subscribe`;
    // an empty string would put a malformed request on the wire.
    let mut subscribe = Vec::new();
    if let Some(uris) = raw.subscribe {
        for uri in uris {
            let uri = uri.trim();
            if uri.is_empty() {
                return Err("`subscribe` contains an empty uri".to_string());
            }
            subscribe.push(uri.to_string());
        }
    }
    // Same rule for `autoRead`: a uri is matched verbatim against a
    // notification's uri before a `resources/read` goes on the wire.
    let mut auto_read = Vec::new();
    if let Some(uris) = raw.auto_read {
        for uri in uris {
            let uri = uri.trim();
            if uri.is_empty() {
                return Err("`autoRead` contains an empty uri".to_string());
            }
            if !auto_read.contains(&uri.to_string()) {
                auto_read.push(uri.to_string());
            }
        }
    }

    // Wake policy: subscribing is the explicit "tell me" opt-in, so a
    // subscription defaults to `auto`; an explicit `signals.wake` overrides.
    let signals = parse_signals(raw.signals, !subscribe.is_empty(), defaults)?;

    Ok(ServerConfig {
        name: name.to_string(),
        transport,
        enabled: raw.enabled.unwrap_or(true),
        timeout,
        subscribe,
        auto_read,
        signals,
    })
}

fn parse_signals(
    raw: Option<RawSignals>,
    subscribed: bool,
    defaults: &SignalsDefaults,
) -> Result<SignalsConfig, String> {
    let mut config = SignalsConfig {
        wake: if subscribed {
            WakeMode::Auto
        } else {
            WakeMode::Off
        },
        ..SignalsConfig::default()
    };
    // Precedence: built-in/`subscribe` default < top-level defaults <
    // per-server explicit fields.
    if let Some(wake) = defaults.wake {
        config.wake = wake;
    }
    if let Some(interval) = defaults.min_interval {
        config.min_interval = interval;
    }
    if let Some(runs) = defaults.max_runs_per_hour {
        config.max_runs_per_hour = runs;
    }
    if let Some(runs) = defaults.max_consecutive_runs {
        config.max_consecutive_runs = runs;
    }
    let Some(raw) = raw else {
        return Ok(config);
    };
    let mut issues = Vec::new();
    let mut explicit = SignalsDefaults::default();
    apply_signals_fields(raw, &mut explicit, &mut issues);
    if let Some(issue) = issues.into_iter().next() {
        return Err(issue);
    }
    if let Some(wake) = explicit.wake {
        config.wake = wake;
    }
    if let Some(interval) = explicit.min_interval {
        config.min_interval = interval;
    }
    if let Some(runs) = explicit.max_runs_per_hour {
        config.max_runs_per_hour = runs;
    }
    if let Some(runs) = explicit.max_consecutive_runs {
        config.max_consecutive_runs = runs;
    }
    Ok(config)
}

/// Apply the fields present in a `signals` object to `out`. Shared by the
/// top-level defaults (issues are collected) and the per-server block (an
/// invalid wake rejects that server entry).
fn apply_signals_fields(raw: RawSignals, out: &mut SignalsDefaults, issues: &mut Vec<String>) {
    if let Some(wake) = raw.wake {
        let normalized = wake.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "off" => out.wake = Some(WakeMode::Off),
            "notify" => out.wake = Some(WakeMode::Notify),
            "auto" => out.wake = Some(WakeMode::Auto),
            other => issues.push(format!(
                "`signals.wake` must be one of off/notify/auto (got {other:?})"
            )),
        }
    }
    if let Some(seconds) = raw.min_interval_seconds {
        out.min_interval = Some(Duration::from_secs(
            seconds.clamp(MIN_SIGNAL_INTERVAL_SECS, MAX_SIGNAL_INTERVAL_SECS),
        ));
    }
    if let Some(runs) = raw.max_runs_per_hour {
        out.max_runs_per_hour =
            Some(runs.clamp(MIN_SIGNAL_RUNS_PER_HOUR, MAX_SIGNAL_RUNS_PER_HOUR));
    }
    if let Some(runs) = raw.max_consecutive_runs {
        out.max_consecutive_runs =
            Some(runs.clamp(MIN_SIGNAL_CONSECUTIVE_RUNS, MAX_SIGNAL_CONSECUTIVE_RUNS));
    }
}

/// Merge the global block with the project block. **Project wins**: a project
/// entry replaces a same-named global entry in place; new project names are
/// appended after the global ones.
///
/// A project's `mcpServers` block is honored only when `project_trusted` — a
/// project-declared server spawns a process and inherits the allowlisted
/// environment on its author's say-so, exactly the class of action project
/// trust gates everywhere else. Untrusted project entries are ignored with a
/// reported issue.
pub fn merge_servers(
    global: Vec<ServerConfig>,
    project: Vec<ServerConfig>,
    project_trusted: bool,
) -> (Vec<ServerConfig>, Vec<String>) {
    let mut issues = Vec::new();
    let mut merged = global;
    if project_trusted {
        for incoming in project {
            match merged.iter_mut().find(|s| s.name == incoming.name) {
                Some(slot) => *slot = incoming,
                None => merged.push(incoming),
            }
        }
    } else if !project.is_empty() {
        issues.push(
            "project mcpServers ignored: project is not trusted (use --approve or /trust)"
                .to_string(),
        );
    }
    (merged, issues)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: serde_json::Value) -> (Vec<ServerConfig>, Vec<String>) {
        parse_servers(Some(&value))
    }

    #[test]
    fn parses_stdio_and_http_entries_with_defaults() {
        let (servers, issues) = parse(json!({
            "local": { "command": "srv", "args": ["--mcp"] },
            "remote": { "url": "https://example.test/mcp", "headers": { "Authorization": "Bearer x" } }
        }));
        assert!(issues.is_empty(), "unexpected issues: {issues:?}");
        assert_eq!(servers.len(), 2);

        let local = &servers[0];
        assert_eq!(local.name, "local");
        assert!(local.enabled);
        assert_eq!(local.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        match &local.transport {
            Transport::Stdio(s) => {
                assert_eq!(s.command, "srv");
                assert_eq!(s.args, vec!["--mcp".to_string()]);
                assert!(s.env.is_empty());
                assert_eq!(s.cwd, None);
            }
            other => panic!("expected stdio, got {other:?}"),
        }

        match &servers[1].transport {
            Transport::Http(h) => {
                assert_eq!(h.url, "https://example.test/mcp");
                assert_eq!(h.headers.len(), 1);
            }
            other => panic!("expected http, got {other:?}"),
        }
    }

    #[test]
    fn unknown_legacy_keys_are_ignored_so_ported_files_load() {
        // The retired adapter's `exposure` field: parsed there, never enforced.
        // A user porting their file must not have the whole entry rejected.
        let (servers, issues) = parse(json!({
            "codegraph": { "command": "codegraph", "exposure": "always", "enabled": true }
        }));
        assert!(issues.is_empty(), "unexpected issues: {issues:?}");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "codegraph");
    }

    #[test]
    fn missing_name_and_blank_command_are_rejected() {
        let (servers, issues) = parse(json!({
            "": { "command": "srv" },
            "bad name": { "command": "srv" },
            "blank": { "command": "   " },
            "good": { "command": "srv" }
        }));
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "good");
        assert_eq!(issues.len(), 3, "issues: {issues:?}");
        assert!(issues.iter().any(|i| i.contains("invalid server name")));
        assert!(issues.iter().any(|i| i.contains("`command` is empty")));
    }

    #[test]
    fn empty_url_is_rejected() {
        let (servers, issues) = parse(json!({ "remote": { "url": "   " } }));
        assert!(servers.is_empty());
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("`url` is empty"));
    }

    #[test]
    fn a_url_with_embedded_credentials_is_rejected() {
        // reqwest would turn `alice:s3cret@` into Basic auth and the URL is
        // logged, so the entry is refused instead — and the issue must not
        // echo the password.
        let (servers, issues) = parse(json!({
            "remote": { "url": "https://alice:s3cret@example.test/mcp" }
        }));
        assert!(servers.is_empty());
        assert_eq!(issues.len(), 1, "issues: {issues:?}");
        assert!(
            issues[0].contains("must not embed credentials"),
            "issues: {issues:?}"
        );
        assert!(!issues[0].contains("s3cret"), "issues: {issues:?}");
    }

    #[test]
    fn a_non_http_url_is_rejected() {
        let (servers, issues) = parse(json!({ "remote": { "url": "ws://example.test/mcp" } }));
        assert!(servers.is_empty());
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("must use http or https"),
            "issues: {issues:?}"
        );
    }

    #[test]
    fn server_names_containing_the_tool_separator_are_rejected() {
        // `a__b` would make owns_tool_name parse `a__b__t` as server `a` (the
        // namespace half is matched by splitting on the FIRST `__`) — the
        // separator must never appear inside the namespace itself.
        let (servers, issues) = parse(json!({
            "a__b": { "command": "srv" },
            "a_": { "command": "srv" },
            "docs": { "command": "srv" }
        }));
        assert_eq!(servers.len(), 2, "single underscores stay legal");
        assert_eq!(issues.len(), 1, "issues: {issues:?}");
        assert!(
            issues[0].contains("invalid server name"),
            "issues: {issues:?}"
        );
        assert!(issues[0].contains("\"__\""), "issues: {issues:?}");
    }

    #[test]
    fn contradictory_discriminator_is_rejected() {
        let (servers, issues) = parse(json!({
            "both": { "command": "srv", "url": "https://x" },
            "neither": { "args": ["--mcp"] }
        }));
        assert!(servers.is_empty());
        assert_eq!(issues.len(), 2, "issues: {issues:?}");
        assert!(issues.iter().all(|i| i.contains("exactly one of")));
    }

    #[test]
    fn timeout_is_clamped_to_1_300_seconds() {
        let (servers, issues) = parse(json!({
            "low": { "command": "srv", "timeoutSeconds": 0 },
            "high": { "command": "srv", "timeoutSeconds": 100_000 },
            "mid": { "command": "srv", "timeoutSeconds": 42 }
        }));
        assert!(issues.is_empty(), "unexpected issues: {issues:?}");
        let by_name = |n: &str| servers.iter().find(|s| s.name == n).unwrap();
        assert_eq!(
            by_name("low").timeout,
            Duration::from_secs(MIN_TIMEOUT_SECS)
        );
        assert_eq!(
            by_name("high").timeout,
            Duration::from_secs(MAX_TIMEOUT_SECS)
        );
        assert_eq!(by_name("mid").timeout, Duration::from_secs(42));
    }

    #[test]
    fn non_object_block_is_reported() {
        let (servers, issues) = parse(json!(["not", "an", "object"]));
        assert!(servers.is_empty());
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("must be a JSON object"));
    }

    #[test]
    fn absent_block_yields_nothing() {
        let (servers, issues) = parse_servers(None);
        assert!(servers.is_empty());
        assert!(issues.is_empty());
    }

    #[test]
    fn subscription_defaults_wake_to_auto() {
        let (servers, issues) = parse(json!({
            "docs": { "command": "srv", "subscribe": ["file:///a.md"] }
        }));
        assert!(issues.is_empty(), "issues: {issues:?}");
        assert_eq!(servers[0].signals.wake, WakeMode::Auto);
        // The budget defaults are the documented initial values.
        assert_eq!(
            servers[0].signals.min_interval,
            Duration::from_secs(DEFAULT_MIN_INTERVAL_SECS)
        );
        assert_eq!(
            servers[0].signals.max_runs_per_hour,
            DEFAULT_MAX_RUNS_PER_HOUR
        );
        assert_eq!(
            servers[0].signals.max_consecutive_runs,
            DEFAULT_MAX_CONSECUTIVE_RUNS
        );
    }

    #[test]
    fn explicit_wake_off_overrides_subscription() {
        let (servers, issues) = parse(json!({
            "docs": {
                "command": "srv",
                "subscribe": ["file:///a.md"],
                "signals": { "wake": "off" }
            }
        }));
        assert!(issues.is_empty(), "issues: {issues:?}");
        assert_eq!(servers[0].signals.wake, WakeMode::Off);
    }

    #[test]
    fn absent_subscription_defaults_wake_to_off() {
        let (servers, issues) = parse(json!({
            "docs": { "command": "srv" },
            "tools": { "command": "srv", "signals": { "wake": "auto" } }
        }));
        assert!(issues.is_empty(), "issues: {issues:?}");
        let by_name = |n: &str| servers.iter().find(|s| s.name == n).unwrap();
        assert_eq!(by_name("docs").signals.wake, WakeMode::Off);
        // An explicit auto without a subscription is harmless: no notification
        // can arrive, so the mode never fires.
        assert_eq!(by_name("tools").signals.wake, WakeMode::Auto);
    }

    #[test]
    fn invalid_wake_mode_rejects_the_entry() {
        let (servers, issues) = parse(json!({
            "docs": { "command": "srv", "signals": { "wake": "sometimes" } }
        }));
        assert!(servers.is_empty());
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("off/notify/auto"), "issues: {issues:?}");
    }

    #[test]
    fn signal_numbers_are_clamped() {
        let (servers, issues) = parse(json!({
            "low": {
                "command": "srv",
                "signals": {
                    "minIntervalSeconds": 0,
                    "maxRunsPerHour": 0,
                    "maxConsecutiveRuns": 0
                }
            },
            "high": {
                "command": "srv",
                "signals": {
                    "minIntervalSeconds": 999_999,
                    "maxRunsPerHour": 99_999,
                    "maxConsecutiveRuns": 99_999
                }
            }
        }));
        assert!(issues.is_empty(), "issues: {issues:?}");
        let by_name = |n: &str| servers.iter().find(|s| s.name == n).unwrap();
        assert_eq!(
            by_name("low").signals.min_interval,
            Duration::from_secs(MIN_SIGNAL_INTERVAL_SECS)
        );
        assert_eq!(
            by_name("low").signals.max_runs_per_hour,
            MIN_SIGNAL_RUNS_PER_HOUR
        );
        assert_eq!(
            by_name("low").signals.max_consecutive_runs,
            MIN_SIGNAL_CONSECUTIVE_RUNS
        );
        assert_eq!(
            by_name("high").signals.min_interval,
            Duration::from_secs(MAX_SIGNAL_INTERVAL_SECS)
        );
        assert_eq!(
            by_name("high").signals.max_runs_per_hour,
            MAX_SIGNAL_RUNS_PER_HOUR
        );
        assert_eq!(
            by_name("high").signals.max_consecutive_runs,
            MAX_SIGNAL_CONSECUTIVE_RUNS
        );
    }

    #[test]
    fn global_defaults_override_the_subscribe_default() {
        let (defaults, issues) = parse_signals_defaults(Some(&json!({ "wake": "off" })));
        assert!(issues.is_empty(), "issues: {issues:?}");
        let (servers, issues) = parse_servers_with_defaults(
            Some(&json!({
                "docs": { "command": "srv", "subscribe": ["file:///a.md"] }
            })),
            &defaults,
        );
        assert!(issues.is_empty(), "issues: {issues:?}");
        assert_eq!(servers[0].signals.wake, WakeMode::Off);
    }

    #[test]
    fn per_server_fields_beat_global_defaults() {
        let (defaults, issues) = parse_signals_defaults(Some(&json!({
            "wake": "off",
            "minIntervalSeconds": 5,
            "maxRunsPerHour": 1,
            "maxConsecutiveRuns": 1
        })));
        assert!(issues.is_empty(), "issues: {issues:?}");
        let (servers, issues) = parse_servers_with_defaults(
            Some(&json!({
                "docs": {
                    "command": "srv",
                    "subscribe": ["file:///a.md"],
                    "signals": { "wake": "auto", "minIntervalSeconds": 90 }
                }
            })),
            &defaults,
        );
        assert!(issues.is_empty(), "issues: {issues:?}");
        let signals = &servers[0].signals;
        assert_eq!(signals.wake, WakeMode::Auto);
        assert_eq!(signals.min_interval, Duration::from_secs(90));
        // Untouched fields still come from the global block.
        assert_eq!(signals.max_runs_per_hour, 1);
        assert_eq!(signals.max_consecutive_runs, 1);
    }

    #[test]
    fn project_defaults_merge_field_wise() {
        let (global, _) = parse_signals_defaults(Some(&json!({
            "wake": "off",
            "minIntervalSeconds": 10,
            "maxRunsPerHour": 3
        })));
        let (project, _) = parse_signals_defaults(Some(&json!({
            "wake": "auto",
            "maxConsecutiveRuns": 1
        })));
        let merged = merge_signals_defaults(global, project);
        assert_eq!(merged.wake, Some(WakeMode::Auto));
        assert_eq!(merged.min_interval, Some(Duration::from_secs(10)));
        assert_eq!(merged.max_runs_per_hour, Some(3));
        assert_eq!(merged.max_consecutive_runs, Some(1));
    }

    #[test]
    fn invalid_global_wake_is_reported_and_ignored() {
        let (defaults, issues) = parse_signals_defaults(Some(&json!({
            "wake": "sometimes",
            "minIntervalSeconds": 12
        })));
        assert_eq!(defaults.wake, None);
        assert_eq!(defaults.min_interval, Some(Duration::from_secs(12)));
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("off/notify/auto"), "issues: {issues:?}");
    }

    fn stdio(name: &str) -> ServerConfig {
        ServerConfig {
            name: name.to_string(),
            transport: Transport::Stdio(StdioTransport {
                command: format!("{name}-bin"),
                args: Vec::new(),
                env: BTreeMap::new(),
                cwd: None,
            }),
            enabled: true,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            subscribe: Vec::new(),
            auto_read: Vec::new(),
            signals: SignalsConfig::default(),
        }
    }

    #[test]
    fn project_override_wins_and_new_project_servers_append() {
        let (merged, issues) = merge_servers(
            vec![stdio("shared"), stdio("global-only")],
            vec![stdio("shared"), stdio("project-only")],
            true,
        );
        assert!(issues.is_empty(), "unexpected issues: {issues:?}");
        let names: Vec<_> = merged.iter().map(|s| s.name.as_str()).collect();
        // Same-name project entry replaces in place; new names append.
        assert_eq!(names, vec!["shared", "global-only", "project-only"]);
        match &merged[0].transport {
            Transport::Stdio(s) => assert_eq!(s.command, "shared-bin"),
            other => panic!("expected stdio, got {other:?}"),
        }
    }

    #[test]
    fn untrusted_project_servers_are_ignored() {
        let (merged, issues) = merge_servers(vec![stdio("global")], vec![stdio("project")], false);
        let names: Vec<_> = merged.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["global"]);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("not trusted"));
    }

    #[test]
    fn untrusted_project_without_servers_is_silent() {
        let (merged, issues) = merge_servers(vec![stdio("global")], Vec::new(), false);
        assert_eq!(merged.len(), 1);
        assert!(issues.is_empty());
    }

    #[test]
    fn subscribe_uris_are_kept_verbatim_and_empty_ones_reject_the_entry() {
        let (servers, issues) = parse(json!({
            "docs": { "command": "srv", "subscribe": ["file:///a.md", "  file:///b.md  "] },
            "bad": { "command": "srv", "subscribe": ["   "] }
        }));
        assert_eq!(servers.len(), 1);
        assert_eq!(
            servers[0].subscribe,
            vec!["file:///a.md".to_string(), "file:///b.md".to_string()]
        );
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("`subscribe` contains an empty uri"));
    }

    #[test]
    fn absent_subscribe_field_yields_an_empty_list() {
        let (servers, issues) = parse(json!({ "plain": { "command": "srv" } }));
        assert!(issues.is_empty());
        assert!(servers[0].subscribe.is_empty());
        assert!(servers[0].auto_read.is_empty());
    }

    #[test]
    fn auto_read_uris_are_trimmed_deduped_and_empty_ones_reject_the_entry() {
        let (servers, issues) = parse(json!({
            "docs": {
                "command": "srv",
                "autoRead": ["file:///a.md", "  file:///a.md  ", "file:///b.md"]
            },
            "bad": { "command": "srv", "autoRead": ["   "] }
        }));
        assert_eq!(servers.len(), 1);
        assert_eq!(
            servers[0].auto_read,
            vec!["file:///a.md".to_string(), "file:///b.md".to_string()]
        );
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("`autoRead` contains an empty uri"));
    }
}
