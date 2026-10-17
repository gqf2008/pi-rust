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
    /// Resource uris ki subscribes to right after the handshake. There is no
    /// model verb for subscribing (batch-2 stance): subscriptions are
    /// host/config-driven, and the restore-on-reconnect machinery keeps them
    /// alive for the session.
    pub subscribe: Vec<String>,
    /// Resource uris whose updates are read eagerly at the boundary instead of
    /// delivering the light (server, uri) notice. Opt-in per server: the
    /// product semantic stays "notice, not content" unless the user lists the
    /// uri here.
    pub auto_read: Vec<String>,
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
        match parse_one(name, entry) {
            Ok(config) => out.push(config),
            Err(reason) => issues.push(format!("mcpServers.{name}: {reason}")),
        }
    }
    (out, issues)
}

fn parse_one(name: &str, entry: &serde_json::Value) -> Result<ServerConfig, String> {
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

    Ok(ServerConfig {
        name: name.to_string(),
        transport,
        enabled: raw.enabled.unwrap_or(true),
        timeout,
        subscribe,
        auto_read,
    })
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
