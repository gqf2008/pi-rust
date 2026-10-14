//! A representative subset of `rpi-langfuse`, on the wasm ABI.
//!
//! It builds one turn trace from the event stream and flushes it to Langfuse's
//! OTLP endpoint through `net-http`. This is the "high-frequency events +
//! outgoing HTTP" reference the plan asks for; the three management tools and
//! the full usage/cost model stay in the compile-time plugin.
//!
//! Configuration arrives through the `langfuse_config` tool, not the process
//! environment: the guest is a sandbox and should not be handed the host's env.

use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use kiqi_plugin_guest::bindings::exports::kiqi::plugin::plugin_api::Guest;
use kiqi_plugin_guest::bindings::kiqi::plugin::{log, net_http, types};
use serde_json::{json, Map, Value};

const CONFIG_TOOL: &str = "langfuse_config";
const OTEL_PATH: &str = "/api/public/otel/v1/traces";
const ROOT_NAME: &str = "Conversational Turn";
const GENERATION_NAME: &str = "LLM Call";
const TRACE_PREFIX_FALLBACK: &str = "kiqi";
const SDK_NAME: &str = "rpi-langfuse";

#[derive(Clone, Default)]
struct Config {
    base_url: String,
    public_key: String,
    secret_key: String,
}

impl Config {
    fn is_valid(&self) -> bool {
        !self.public_key.is_empty() && !self.secret_key.is_empty()
    }
}

#[derive(Clone)]
struct Span {
    id: String,
    trace_id: String,
    parent_id: Option<String>,
    name: String,
    kind: String,
    trace_name: String,
    session_id: Option<String>,
    start_ms: u128,
    end_ms: Option<u128>,
    input: Option<Value>,
    output: Option<Value>,
    model: Option<String>,
    usage: Option<Value>,
}

#[derive(Default)]
struct State {
    config: Config,
    trace_id: String,
    trace_name: String,
    session_id: Option<String>,
    root_id: String,
    generation: Option<usize>,
    spans: Vec<Span>,
    last_text: Option<String>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Lowercase hex id, `bytes` bytes. Splitmix64 over a counter and the clock;
/// OTLP wants 8-byte span ids and 16-byte trace ids.
fn hex_id(bytes: usize) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut x = (now_ms() as u64) ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut out = String::with_capacity(bytes * 2);
    while out.len() < bytes * 2 {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.push_str(&format!("{z:016x}"));
    }
    out.truncate(bytes * 2);
    if out.bytes().all(|b| b == b'0') {
        out.pop();
        out.push('1');
    }
    out
}

fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18 & 63) as usize] as char);
        out.push(TABLE[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn extract_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Event handlers
// ---------------------------------------------------------------------------

fn on_session_start() {
    let config = state().lock().unwrap().config.clone();
    *state().lock().unwrap() = State {
        config,
        ..Default::default()
    };
}

fn on_before_agent_start(payload: &str) {
    let data: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    let mut state = state().lock().unwrap();
    if !state.config.is_valid() {
        return;
    }
    let host = data
        .get("host")
        .and_then(|host| host.get("name"))
        .and_then(Value::as_str)
        .unwrap_or(TRACE_PREFIX_FALLBACK);
    let trace_name = format!("{host} Turn");
    let trace_id = hex_id(16);
    let root_id = hex_id(8);
    state.spans.push(Span {
        id: root_id.clone(),
        trace_id: trace_id.clone(),
        parent_id: None,
        name: ROOT_NAME.to_string(),
        kind: "span".to_string(),
        trace_name: trace_name.clone(),
        session_id: data
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string),
        start_ms: now_ms(),
        end_ms: None,
        input: Some(json!({
            "role": "user",
            "content": data.get("prompt").and_then(Value::as_str).unwrap_or(""),
        })),
        output: None,
        model: None,
        usage: None,
    });
    state.trace_id = trace_id;
    state.trace_name = trace_name;
    state.session_id = data
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string);
    state.root_id = root_id;
    state.generation = None;
    state.last_text = None;
}

fn on_before_provider_request(payload: &str) {
    let data: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    let mut state = state().lock().unwrap();
    if !state.config.is_valid() || state.root_id.is_empty() {
        return;
    }
    let span = Span {
        id: hex_id(8),
        trace_id: state.trace_id.clone(),
        parent_id: Some(state.root_id.clone()),
        name: GENERATION_NAME.to_string(),
        kind: "generation".to_string(),
        trace_name: state.trace_name.clone(),
        session_id: state.session_id.clone(),
        start_ms: now_ms(),
        end_ms: None,
        input: None,
        output: None,
        model: data
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string),
        usage: None,
    };
    state.spans.push(span);
    state.generation = Some(state.spans.len() - 1);
}

fn on_message_end(payload: &str) {
    let data: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    let role = data
        .get("role")
        .or_else(|| data.get("kind"))
        .and_then(Value::as_str);
    if role != Some("assistant") {
        return;
    }
    let text = extract_text(data.get("content").unwrap_or(&Value::Null));
    let usage = data.get("usage").cloned();
    let mut state = state().lock().unwrap();
    if !state.config.is_valid() {
        return;
    }
    if let Some(index) = state.generation.take() {
        state.spans[index].end_ms = Some(now_ms());
        state.spans[index].output = if text.is_empty() {
            None
        } else {
            Some(json!({ "role": "assistant", "content": text }))
        };
        state.spans[index].usage = usage;
        state.last_text = Some(text);
    }
}

fn on_session_shutdown() {
    {
        let mut state = state().lock().unwrap();
        if !state.config.is_valid() {
            return;
        }
        if let Some(index) = state.generation.take() {
            state.spans[index].end_ms = Some(now_ms());
        }
        let root_id = state.root_id.clone();
        let last_text = state.last_text.clone();
        if let Some(root) = state.spans.iter_mut().find(|span| span.id == root_id) {
            root.end_ms = Some(now_ms());
            root.output = last_text.map(|text| json!({ "role": "assistant", "content": text }));
        }
    }
    flush();
}

// ---------------------------------------------------------------------------
// OTLP flush over net-http
// ---------------------------------------------------------------------------

fn flush() {
    let (config, spans) = {
        let state = state().lock().unwrap();
        (state.config.clone(), state.spans.clone())
    };
    if !config.is_valid() || spans.is_empty() {
        return;
    }
    let payload = otlp_payload(&spans);
    let url = format!("{}{}", config.base_url.trim_end_matches('/'), OTEL_PATH);
    let authorization = format!(
        "Basic {}",
        base64(format!("{}:{}", config.public_key, config.secret_key).as_bytes())
    );
    let request = json!({
        "url": url,
        "method": "POST",
        "headers": {
            "content-type": "application/json",
            "authorization": authorization,
            "x-langfuse-ingestion-version": "4",
        },
        "body": payload.to_string(),
    })
    .to_string();
    match net_http::fetch(&request) {
        Ok(_) => log::write(types::LogLevel::Info, "langfuse: flushed span(s)"),
        Err(error) => log::write(
            types::LogLevel::Warn,
            &format!("langfuse: flush rejected: {error:?}"),
        ),
    }
}

fn attr_string(key: &str, value: &str) -> Value {
    json!({ "key": key, "value": { "stringValue": value } })
}

fn span_to_otlp(span: &Span) -> Value {
    let mut attributes = vec![
        attr_string("langfuse.trace.name", &span.trace_name),
        attr_string("langfuse.observation.type", &span.kind),
    ];
    if let Some(session) = &span.session_id {
        attributes.push(attr_string("langfuse.session.id", session));
    }
    if let Some(model) = &span.model {
        attributes.push(attr_string("langfuse.observation.model.name", model));
    }
    if let Some(output) = &span.output {
        attributes.push(attr_string(
            "langfuse.observation.output",
            &output.to_string(),
        ));
    }
    if let Some(input) = &span.input {
        attributes.push(attr_string(
            "langfuse.observation.input",
            &input.to_string(),
        ));
    }

    let mut out = Map::new();
    out.insert("traceId".into(), json!(span.trace_id));
    out.insert("spanId".into(), json!(span.id));
    if let Some(parent) = &span.parent_id {
        out.insert("parentSpanId".into(), json!(parent));
    }
    out.insert("name".into(), json!(span.name));
    out.insert("kind".into(), json!(1));
    out.insert(
        "startTimeUnixNano".into(),
        json!((span.start_ms * 1_000_000).to_string()),
    );
    if let Some(end) = span.end_ms {
        out.insert(
            "endTimeUnixNano".into(),
            json!((end * 1_000_000).to_string()),
        );
    }
    out.insert("attributes".into(), Value::Array(attributes));
    Value::Object(out)
}

fn otlp_payload(spans: &[Span]) -> Value {
    // The root is exported last: Langfuse freezes a trace header when its root
    // arrives, so children must be in flight first.
    let mut ordered: Vec<&Span> = spans.iter().filter(|s| s.parent_id.is_some()).collect();
    ordered.extend(spans.iter().filter(|s| s.parent_id.is_none()));
    json!({
        "resourceSpans": [{
            "resource": { "attributes": [attr_string("service.name", TRACE_PREFIX_FALLBACK)] },
            "scopeSpans": [{
                "scope": { "name": SDK_NAME, "version": "0.0.0" },
                "spans": ordered.iter().map(|s| span_to_otlp(s)).collect::<Vec<_>>(),
            }],
        }],
    })
}

// ---------------------------------------------------------------------------
// Plugin surface
// ---------------------------------------------------------------------------

const CONFIG_SCHEMA: &str = r#"{"type":"object","properties":{"baseUrl":{"type":"string"},"publicKey":{"type":"string"},"secretKey":{"type":"string"}},"required":["baseUrl","publicKey","secretKey"]}"#;

fn configure(arguments: &str) -> types::ToolResult {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let get = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let config = Config {
        base_url: get("baseUrl"),
        public_key: get("publicKey"),
        secret_key: get("secretKey"),
    };
    let valid = config.is_valid();
    state().lock().unwrap().config = config;
    types::ToolResult {
        blocks: vec![types::ContentBlock::Text(if valid {
            "langfuse: configured".to_string()
        } else {
            "langfuse: baseUrl, publicKey and secretKey are required".to_string()
        })],
        is_error: !valid,
        details: "null".to_string(),
    }
}

struct LangfusePlugin;

impl Guest for LangfusePlugin {
    fn id() -> String {
        "rpi-langfuse".to_string()
    }

    fn shutdown() {
        on_session_shutdown();
    }

    fn list_tools() -> Vec<types::ToolSpec> {
        vec![types::ToolSpec {
            name: CONFIG_TOOL.to_string(),
            description: "Point this langfuse plugin at an endpoint (representative subset)"
                .to_string(),
            parameters_schema: CONFIG_SCHEMA.to_string(),
        }]
    }

    fn call_tool(name: String, arguments: String) -> Result<types::ToolResult, types::PluginError> {
        if name == CONFIG_TOOL {
            return Ok(configure(&arguments));
        }
        Err(types::PluginError::UnknownTool(name))
    }

    fn list_commands() -> Vec<types::CommandSpec> {
        Vec::new()
    }

    fn run_command(_name: String, _arguments: String) -> Result<String, types::PluginError> {
        Err(types::PluginError::Failed("no commands".to_string()))
    }

    fn list_flags() -> Vec<types::FlagSpec> {
        Vec::new()
    }

    fn list_shortcuts() -> Vec<types::ShortcutSpec> {
        Vec::new()
    }

    fn on_event(event: types::Event) -> types::EventAction {
        match event {
            types::Event::SessionStart => on_session_start(),
            types::Event::BeforeAgentStart(payload) => on_before_agent_start(&payload),
            types::Event::BeforeProviderRequest(payload) => on_before_provider_request(&payload),
            types::Event::MessageEnd(payload) => on_message_end(&payload),
            types::Event::SessionShutdown => on_session_shutdown(),
            _ => {}
        }
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

kiqi_plugin_guest::bindings::export!(LangfusePlugin with_types_in kiqi_plugin_guest::bindings);
