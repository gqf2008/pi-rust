//! `kiqi-webfetch` ported to the wasm ABI (0.5.0).
//!
//! The compile-time extension builds a `reqwest` client and fetches directly. A wasm
//! guest has no sockets of its own, so the request goes through the host's
//! `net-http` capability and the *user's* allow-list decides where it may go —
//! which is also why the post-redirect re-check moved to the host (it refuses a
//! hop to a host that is not allow-listed).
//!
//! What stays in the guest is everything that is pure: the URL policy
//! (`validate`) and the HTML-to-text reduction (`strip_html`), so the extension keeps
//! its own opinion about what is fetchable instead of relying only on the host.
//!
//! Two differences worth stating rather than hiding:
//!
//! * **No DNS check.** The compile-time version resolves the hostname and refuses
//!   one that lands on loopback/RFC1918, on top of the literal-IP check. A guest
//!   cannot resolve, so only the literal check survives; the allow-list is what
//!   constrains a hostname here.
//! * **No final-URL check.** The host answers with status/headers/body, not the
//!   URL it ended on, so there is nothing to re-validate after a redirect.

use kiqi_wasm_bindings::bindings::exports::kiqi::extension::extension_api::Guest;
use kiqi_wasm_bindings::bindings::kiqi::extension::{net_http, types};
use serde_json::{json, Value};

const ID: &str = "kiqi-webfetch";
const TOOL: &str = "webfetch";
const DESCRIPTION: &str = "Fetch bounded readable text from a public HTTP(S) URL.";
const SCHEMA: &str = r#"{"type":"object","properties":{"url":{"type":"string"},"maxChars":{"type":"integer","minimum":256,"maximum":50000}},"required":["url"]}"#;
/// The compile-time extension's own body cap, kept so the two ports agree.
const MAX_BODY_BYTES: usize = 1_048_576;

struct Webfetch;

impl Guest for Webfetch {
    /// No selector/editor/prompt flow exists in this extension yet; a reply is
    /// refused rather than silently dropped.
    fn continue_command(
        _name: String,
        _flow_id: String,
        _reply: types::CommandReply,
    ) -> Result<types::CommandResult, types::ExtensionError> {
        Err(types::ExtensionError::Failed(
            "this extension declares no command continuations".to_string(),
        ))
    }

    fn id() -> String {
        ID.to_string()
    }

    fn shutdown() {}

    fn list_tools() -> Vec<types::ToolSpec> {
        vec![types::ToolSpec {
            name: TOOL.to_string(),
            description: DESCRIPTION.to_string(),
            parameters_schema: SCHEMA.to_string(),
        }]
    }

    fn call_tool(
        name: String,
        arguments: String,
    ) -> Result<types::ToolResult, types::ExtensionError> {
        if name != TOOL {
            return Err(types::ExtensionError::UnknownTool(name));
        }
        let text = fetch(&arguments)?;
        Ok(types::ToolResult {
            blocks: vec![types::ContentBlock::Text(text)],
            is_error: false,
            details: "null".to_string(),
        })
    }

    fn list_commands() -> Vec<types::CommandSpec> {
        Vec::new()
    }

    fn run_command(
        _name: String,
        _arguments: String,
    ) -> Result<types::CommandResult, types::ExtensionError> {
        Err(types::ExtensionError::UnknownTool(_name))
    }

    fn list_flags() -> Vec<types::FlagSpec> {
        Vec::new()
    }

    fn list_shortcuts() -> Vec<types::ShortcutSpec> {
        Vec::new()
    }

    fn on_event(_event: types::Event) -> types::EventAction {
        types::EventAction::Continue
    }

    fn before_agent_start(_prompt: String) -> Option<String> {
        None
    }

    fn list_probe_points() -> Vec<String> {
        Vec::new()
    }

    fn probe(_point: String, _payload: String) -> types::Verdict {
        types::Verdict::Continue
    }
}

/// Fetch one URL through the host and reduce it to bounded readable text.
fn fetch(arguments: &str) -> Result<String, types::ExtensionError> {
    let params: Value = serde_json::from_str(arguments)
        .map_err(|error| types::ExtensionError::InvalidArguments(error.to_string()))?;
    let url = params
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| types::ExtensionError::InvalidArguments("url is required".to_string()))?;
    let url = validate(url).map_err(types::ExtensionError::InvalidArguments)?;
    let max = params
        .get("maxChars")
        .and_then(Value::as_u64)
        .unwrap_or(20480)
        .clamp(256, 50000) as usize;

    let request = json!({ "url": url }).to_string();
    let response = net_http::fetch(&request).map_err(|error| {
        types::ExtensionError::Failed(format!("web fetch failed: {}", reason(error)))
    })?;
    let response: Value = serde_json::from_str(&response).map_err(|error| {
        types::ExtensionError::Failed(format!("bad capability response: {error}"))
    })?;
    let status = response.get("status").and_then(Value::as_u64).unwrap_or(0);
    let headers = response.get("headers").cloned().unwrap_or(Value::Null);
    let content_type = header(&headers, "content-type").unwrap_or("").to_string();
    let body = response.get("body").and_then(Value::as_str).unwrap_or("");

    if body.len() > MAX_BODY_BYTES {
        return Err(types::ExtensionError::Failed(format!(
            "response exceeded {} bytes",
            MAX_BODY_BYTES
        )));
    }
    if !(content_type.is_empty()
        || content_type.contains("text/")
        || content_type.contains("json")
        || content_type.contains("xml")
        || content_type.contains("html"))
    {
        return Ok(json!({
            "url": url,
            "status": status,
            "contentType": content_type,
            "skipped": "non-text response",
        })
        .to_string());
    }

    let text = if content_type.contains("html") {
        strip_html(body)
    } else {
        body.to_string()
    };
    let clipped: String = text.chars().take(max).collect();
    Ok(json!({
        "url": url,
        "status": status,
        "contentType": content_type,
        "text": clipped,
        "truncated": text.chars().count() > max,
    })
    .to_string())
}

/// The capability error as a displayable reason.
fn reason(error: types::CapabilityError) -> String {
    match error {
        types::CapabilityError::Denied(reason) => format!("denied: {reason}"),
        types::CapabilityError::Failed(reason) => reason,
    }
}

/// The header named `name`, case-insensitively (HTTP header names are not
/// case-sensitive and the host forwards whatever the server sent).
fn header<'a>(headers: &'a Value, name: &str) -> Option<&'a str> {
    headers
        .as_object()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .and_then(|(_, value)| value.as_str())
}

/// The extension's own URL policy.
///
/// Kept from the compile-time extension so the guest does not depend solely on the
/// host's allow-list. The DNS-resolution branch is gone (a guest cannot resolve);
/// see the module docs.
fn validate(input: &str) -> Result<String, String> {
    let lower = input.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err("only http and https URLs are allowed".to_string());
    }
    let after_scheme = input.split_once("://").map(|(_, rest)| rest).unwrap_or("");
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.contains('@') {
        return Err("embedded credentials are not allowed".to_string());
    }
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default().to_string()
    } else {
        authority.split(':').next().unwrap_or_default().to_string()
    };
    if host.is_empty() {
        return Err("URL has no host".to_string());
    }
    let host_lower = host.to_ascii_lowercase();
    if host_lower == "localhost" || host_lower.ends_with(".localhost") {
        return Err("local hosts are not allowed".to_string());
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if blocked_ip(ip) {
            return Err("private or local IPs are not allowed".to_string());
        }
    }
    Ok(input.to_string())
}

fn blocked_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.octets()[0] == 0
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                // See the native face: a v4-mapped literal names an IPv4
                // address, so judge it as one.
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| blocked_ip(std::net::IpAddr::V4(v4)))
        }
    }
}

/// Same reduction as the compile-time extension: drop the tag soup and collapse
/// whitespace.
fn strip_html(s: &str) -> String {
    let mut out = s.replace('\r', " ");
    for tag in [
        "script", "style", "nav", "footer", "header", "aside", "noscript",
    ] {
        let start = format!("<{tag}");
        while let Some(a) = out.to_ascii_lowercase().find(&start) {
            if let Some(b) = out[a..].find(&format!("</{tag}>")) {
                out.replace_range(a..a + b + tag.len() + 3, " ");
            } else {
                break;
            }
        }
    }
    out = out.replace("><", ">\n<");
    let mut result = String::new();
    let mut inside = false;
    for c in out.chars() {
        match c {
            '<' => inside = true,
            '>' => inside = false,
            c if !inside => result.push(c),
            _ => {}
        }
    }
    result.split_whitespace().collect::<Vec<_>>().join(" ")
}

kiqi_wasm_bindings::bindings::export!(Webfetch with_types_in kiqi_wasm_bindings::bindings);

// The 0.8.0 contribution exports are mandatory world exports; deny them all.
kiqi_wasm_bindings::deny_contribution_exports!(Webfetch);
