//! `kiqi-websearch` ported to the wasm ABI (0.7.0).
//!
//! The compile-time plugin builds a `reqwest` client and talks to two keyless
//! HTTPS endpoints directly — the DuckDuckGo Instant Answer API (primary) and
//! Wikipedia's search API (fallback when DDG yields fewer than `limit` hits). A
//! wasm guest has no sockets, so both GETs go through the host's `net-http`
//! capability, and the *user's* allow-list decides which of the two it may
//! reach. The host forwards the request headers (including the `user-agent` this
//! plugin sets) and hands back `{status, headers, body}`, capping the body at
//! 8 MiB.
//!
//! What stays in the guest is everything that is pure: argument validation, the
//! hit extraction/filtering, and the result envelope. The host only transports.
//!
//! Two differences from the compile-time plugin worth stating rather than
//! hiding:
//!
//! * **No DNS/IP checks.** The compile-time plugin does not do any either — for
//!   this plugin the request goes straight out with no host policy of its own,
//!   so the host's allow-list is the *only* gate here, exactly as the webfetch
//!   port notes. If the allow-list is `*`, both endpoints are reachable
//!   wherever they resolve.
//! * **Result shape follows the trait face.** The two compile-time faces
//!   disagree: `src/kiqi.rs` (the `kiqi_plugin::Plugin` trait — the actual kiqi
//!   contract) puts the raw result JSON in the block text and leaves `details`
//!   null, while `src/lib.rs` (the cdylib face) renders a human summary into the
//!   text and stashes the JSON in `details`. This port follows the **trait**
//!   face, so the block text is the JSON envelope and `details` is `null`.

use kiqi_plugin_guest::bindings::exports::kiqi::plugin::plugin_api::Guest;
use kiqi_plugin_guest::bindings::kiqi::plugin::{net_http, types};
use serde_json::{json, Value};

const ID: &str = "kiqi-websearch";
const TOOL: &str = "websearch";
const DESCRIPTION: &str = "Search public web references with DuckDuckGo.";
const SCHEMA: &str = r#"{"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":20}},"required":["query"]}"#;

/// Set by the compile-time client; the host forwards it unchanged.
const USER_AGENT: &str = "kiqi-websearch/0.1";
const DDG_ENDPOINT: &str = "https://api.duckduckgo.com/";
const WIKI_ENDPOINT: &str = "https://en.wikipedia.org/w/api.php";
/// The compile-time fallback always asks Wikipedia for up to eight hits.
const WIKI_LIMIT: &str = "8";

struct Websearch;

impl Guest for Websearch {
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

    fn call_tool(name: String, arguments: String) -> Result<types::ToolResult, types::PluginError> {
        if name != TOOL {
            return Err(types::PluginError::UnknownTool(name));
        }
        let text = search(&arguments)?;
        Ok(types::ToolResult {
            blocks: vec![types::ContentBlock::Text(text)],
            is_error: false,
            details: "null".to_string(),
        })
    }

    fn list_commands() -> Vec<types::CommandSpec> {
        Vec::new()
    }

    fn run_command(_name: String, _arguments: String) -> Result<String, types::PluginError> {
        Err(types::PluginError::UnknownTool(_name))
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

/// Run one search and return the result envelope as JSON — the trait face's
/// shape.
///
/// The validation is pure and runs first, so a missing/empty query never
/// reaches the network. The DuckDuckGo call is the primary source and, as in the
/// compile-time plugin, its failure is fatal; the Wikipedia fallback is
/// best-effort and only contributes `"wikipedia"` to `failed` when it does not
/// answer.
fn search(arguments: &str) -> Result<String, types::PluginError> {
    let params: Value = serde_json::from_str(arguments)
        .map_err(|error| types::PluginError::InvalidArguments(error.to_string()))?;
    let query = params
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| types::PluginError::InvalidArguments("query is required".to_string()))?
        .trim()
        .to_string();
    if query.is_empty() {
        return Err(types::PluginError::InvalidArguments(
            "query must not be empty".to_string(),
        ));
    }
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .clamp(1, 20) as usize;

    let ddg_url = format!(
        "{}?{}",
        DDG_ENDPOINT,
        form(&[
            ("q", &query),
            ("format", "json"),
            ("no_html", "1"),
            ("skip_disambig", "1"),
        ])
    );
    let body = get(&ddg_url)?;
    let value: Value = serde_json::from_str(&body)
        .map_err(|error| types::PluginError::Failed(format!("invalid search response: {error}")))?;

    let mut hits: Vec<Value> = Vec::new();
    let mut failed: Vec<&str> = Vec::new();

    if let Some(text) = value
        .get("AbstractText")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        hits.push(json!({
            "title": value.get("Heading").and_then(Value::as_str).unwrap_or(&query),
            "url": value.get("AbstractURL").and_then(Value::as_str).unwrap_or(""),
            "snippet": text,
            "source": "duckduckgo",
        }));
    }
    collect(
        &value.get("RelatedTopics").cloned().unwrap_or(Value::Null),
        &mut hits,
        limit,
    );

    // DDG's Instant Answer endpoint is intentionally narrow. Wikipedia gives
    // useful coverage for ordinary research queries when DDG returns few hits.
    if hits.len() < limit {
        let wiki_url = format!(
            "{}?{}",
            WIKI_ENDPOINT,
            form(&[
                ("action", "query"),
                ("list", "search"),
                ("srsearch", &query),
                ("srlimit", WIKI_LIMIT),
                ("format", "json"),
                ("origin", "*"),
            ])
        );
        match get(&wiki_url) {
            Ok(body) => {
                if let Ok(wiki) = serde_json::from_str::<Value>(&body) {
                    if let Some(items) = wiki
                        .get("query")
                        .and_then(|query| query.get("search"))
                        .and_then(Value::as_array)
                    {
                        for item in items {
                            if hits.len() >= limit {
                                break;
                            }
                            let title = item.get("title").and_then(Value::as_str).unwrap_or(&query);
                            let pageid = item.get("pageid").and_then(Value::as_u64).unwrap_or(0);
                            let snippet = item
                                .get("snippet")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .replace("<span class=\"searchmatch\">", "")
                                .replace("</span>", "");
                            hits.push(json!({
                                "title": title,
                                "url": format!("https://en.wikipedia.org/?curid={pageid}"),
                                "snippet": snippet,
                                "source": "wikipedia",
                            }));
                        }
                    }
                }
            }
            // A denied or failed Wikipedia call is not fatal — the compile-time
            // plugin records it and returns whatever DDG found.
            Err(_) => failed.push("wikipedia"),
        }
    }

    Ok(json!({
        "query": query,
        "count": hits.len(),
        "results": hits,
        "failed": failed,
    })
    .to_string())
}

/// Walk DDG's `RelatedTopics` tree (it nests one level under interior topics)
/// and collect `Text`/`FirstURL` entries until `limit` is reached.
fn collect(value: &Value, hits: &mut Vec<Value>, limit: usize) {
    if hits.len() >= limit {
        return;
    }
    if let Some(array) = value.as_array() {
        for entry in array {
            if hits.len() >= limit {
                return;
            }
            if let Some(text) = entry.get("Text").and_then(Value::as_str) {
                hits.push(json!({
                    "title": text,
                    "url": entry.get("FirstURL").and_then(Value::as_str).unwrap_or(""),
                    "snippet": text,
                    "source": "duckduckgo",
                }));
            }
            collect(
                &entry.get("Topics").cloned().unwrap_or(Value::Null),
                hits,
                limit,
            );
        }
    }
}

/// One GET through the host's `net-http` capability, returning the body.
///
/// The host answers with `{status, headers, body}`; the status is kept only to
/// tell a real answer from an error page, as the compile-time plugin does with
/// `response.status().is_success()`.
fn get(url: &str) -> Result<String, types::PluginError> {
    let request = json!({
        "url": url,
        "method": "GET",
        "headers": {
            "user-agent": USER_AGENT,
            "accept": "application/json",
        },
    })
    .to_string();
    let response = net_http::fetch(&request).map_err(capability)?;
    let response: Value = serde_json::from_str(&response)
        .map_err(|error| types::PluginError::Failed(format!("bad capability response: {error}")))?;
    let status = response.get("status").and_then(Value::as_u64).unwrap_or(0);
    if !(200..300).contains(&status) {
        return Err(types::PluginError::Failed(format!(
            "web search failed: HTTP {status}"
        )));
    }
    Ok(response
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string())
}

/// Map a host capability error onto the plugin error, keeping the ABI's
/// `denied` / `failed` distinction rather than collapsing it.
fn capability(error: types::CapabilityError) -> types::PluginError {
    match error {
        types::CapabilityError::Denied(reason) => types::PluginError::Denied(reason),
        types::CapabilityError::Failed(reason) => {
            types::PluginError::Failed(format!("web search failed: {reason}"))
        }
    }
}

/// `application/x-www-form-urlencoded`, since a guest cannot link `url` and the
/// two endpoints both take query parameters in the URL.
fn form(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encode everything outside the unreserved set (RFC 3986 §2.3).
fn encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

kiqi_plugin_guest::bindings::export!(Websearch with_types_in kiqi_plugin_guest::bindings);
