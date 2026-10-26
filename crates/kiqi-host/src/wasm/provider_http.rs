//! `provider-http`: grant-scoped streaming HTTP for a provider request.
//!
//! This is deliberately **not** a general HTTP client. A request is refused
//! unless its scheme, exact host, path prefix and method all match the one
//! endpoint record in the `provider` grant. On a full match the host injects
//! the credential it resolved from its own sidecar; the guest never names a
//! secret, and a request that fails any check never opens a connection.
//!
//! The endpoint record is a request filter, not merely a credential filter:
//! `open` parses and checks the URL itself, and refuses encoded separators,
//! dot segments and duplicate slashes rather than trying to normalize a path a
//! server may interpret differently. Redirects are never followed (a 3xx is a
//! refusal), the body is capped by the grant and the handles live in a
//! per-call table that is reaped at every entry point and on unload.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::grants::ProviderProfile;
use serde_json::Value;

/// One already-checked request handed to a transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderHttpRequest {
    pub url: String,
    pub method: String,
    /// Sanitized headers, with the host-held credential already injected.
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    /// Wall-clock budget for the whole exchange.
    pub timeout: std::time::Duration,
    /// Per-read budget; a transport that cannot express it may apply it to the
    /// whole request instead (the grant's total budget still bounds the call).
    pub read_timeout: std::time::Duration,
}

/// A streaming HTTP response, as the host's handle table sees it.
pub trait ProviderHttpStream: Send {
    /// HTTP status. A 3xx is refused by `open`; the guest never sees it.
    fn status(&self) -> u16;
    /// Read at most `max` bytes; `None` is EOF. Errors are host-visible but
    /// never contain a credential.
    fn read(&mut self, max: usize) -> Result<Option<Vec<u8>>, String>;
}

/// The HTTP seam. The default implementation is real TLS reqwest; tests inject
/// a fake to assert policy and streaming without a network.
pub trait ProviderHttpTransport: Send + Sync {
    fn open(&self, request: ProviderHttpRequest) -> Result<Box<dyn ProviderHttpStream>, String>;
}

/// A resolved, still-secret credential plus how to place it on the wire.
#[derive(Clone)]
pub struct ResolvedCredential {
    pub header: String,
    pub prefix: String,
    pub value: String,
}

impl std::fmt::Debug for ResolvedCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ResolvedCredential(<redacted>)")
    }
}

/// The provider-call state one `provider-api.request` installs on its store.
/// `None` outside that call, which is what makes every `provider-http` entry
/// point deny.
pub(crate) struct ProviderCallState {
    pub profile: ProviderProfile,
    pub credential: Option<ResolvedCredential>,
    pub transport: Arc<dyn ProviderHttpTransport>,
    pub cancel: Option<Arc<dyn crate::CancelProbe>>,
}

/// Headers a guest may never supply. Auth/framing headers are host-owned: the
/// credential comes from the sidecar, and the framing is what the transport
/// negotiated.
pub const STRIPPED_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "authorization",
    "x-api-key",
    "x-goog-api-key",
    "proxy-authorization",
    "cookie",
    "api-key",
    "x-auth-token",
    "cf-aig-authorization",
];

/// One `read` never returns more than this, whatever the guest asks for.
pub const PROVIDER_HTTP_READ_CHUNK_MAX: usize = 1024 * 1024;

/// The longest header value the host forwards.
const PROVIDER_HTTP_MAX_HEADER_VALUE: usize = 8192;

/// The largest request body the host will send on a provider's behalf.
pub const PROVIDER_HTTP_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Why a `provider-http` call failed.
///
/// `Denied` is the authorization answer (not a provider call, no grant, or the
/// request does not match the endpoint record); `Failed` is a granted
/// operation that did not complete (transport error, read error, body cap).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderHttpError {
    Denied(String),
    Failed(String),
}

impl ProviderHttpError {
    fn denied(message: impl Into<String>) -> Self {
        Self::Denied(message.into())
    }

    fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }

    pub(crate) fn into_capability(self) -> crate::wasm::kiqi::plugin::types::CapabilityError {
        use crate::wasm::kiqi::plugin::types::CapabilityError;
        match self {
            Self::Denied(message) => CapabilityError::Denied(message),
            Self::Failed(message) => CapabilityError::Failed(message),
        }
    }
}

/// The request JSON `open` accepts.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenRequest {
    url: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    headers: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    body: Option<String>,
}

/// Parse and authorize one `open` request against the grant.
///
/// `credential` is `Some` only after the sidecar fingerprint check, so an
/// injected header always corresponds to the credential the user granted.
pub(crate) fn checked_request(
    profile: &ProviderProfile,
    credential: Option<&ResolvedCredential>,
    raw: &str,
) -> Result<ProviderHttpRequest, ProviderHttpError> {
    let request: OpenRequest = serde_json::from_str(raw).map_err(|error| {
        ProviderHttpError::failed(format!("invalid provider-http request: {error}"))
    })?;
    // Check the *raw* path before parsing: the URL parser removes dot
    // segments, which would let `/v1/a/../admin` look like `/v1/admin`.
    check_raw_path(raw_path_of(&request.url))?;
    let url = reqwest::Url::parse(&request.url)
        .map_err(|error| ProviderHttpError::failed(format!("invalid url: {error}")))?;

    if url.scheme() != "https" {
        return Err(ProviderHttpError::denied("provider-http only speaks https"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ProviderHttpError::denied(
            "provider-http refuses a URL with userinfo",
        ));
    }
    if url.fragment().is_some() {
        return Err(ProviderHttpError::denied(
            "provider-http refuses a URL with a fragment",
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| ProviderHttpError::denied("provider-http URL has no host"))?;
    if !host.eq_ignore_ascii_case(&profile.endpoint.host) {
        return Err(ProviderHttpError::denied(format!(
            "provider-http host `{host}` is not the granted `{}`",
            profile.endpoint.host
        )));
    }
    if !matches!(url.port(), None | Some(443)) {
        return Err(ProviderHttpError::denied(
            "provider-http refuses a non-default port",
        ));
    }

    let path = url.path();
    check_raw_path(path)?;
    if !path_matches_prefix(&profile.endpoint.path_prefix, path) {
        return Err(ProviderHttpError::denied(format!(
            "provider-http path `{path}` is outside the granted `{}`",
            profile.endpoint.path_prefix
        )));
    }

    let method = request
        .method
        .as_deref()
        .unwrap_or("POST")
        .to_ascii_uppercase();
    if !profile
        .endpoint
        .methods
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(&method))
    {
        return Err(ProviderHttpError::denied(format!(
            "provider-http method `{method}` is not granted"
        )));
    }

    if request
        .body
        .as_ref()
        .is_some_and(|body| body.len() > PROVIDER_HTTP_MAX_BODY_BYTES)
    {
        return Err(ProviderHttpError::denied(format!(
            "provider-http request body is over {PROVIDER_HTTP_MAX_BODY_BYTES} bytes"
        )));
    }
    let headers = sanitize_headers(request.headers.as_ref(), credential)?;
    let timeout = std::time::Duration::from_secs(
        profile
            .timeout_seconds
            .clamp(1, crate::grants::PROVIDER_TIMEOUT_MAX_SECONDS),
    );

    Ok(ProviderHttpRequest {
        url: url.to_string(),
        method,
        headers,
        body: request.body.map(String::into_bytes),
        timeout,
        // A read shares the grant's wall clock; the transport applies it as a
        // per-read budget so one stalled chunk cannot pin the call.
        read_timeout: timeout,
    })
}

/// The path exactly as the guest wrote it, before any URL normalization.
fn raw_path_of(raw: &str) -> &str {
    let rest = raw.split_once("://").map(|(_, rest)| rest).unwrap_or(raw);
    match rest.find('/') {
        Some(index) => {
            let path = &rest[index..];
            path.split(['?', '#']).next().unwrap_or(path)
        }
        None => "/",
    }
}

/// The raw path must not carry an encoded separator, an encoded dot, a dot
/// segment or an empty segment. Normalizing those is exactly where a filter and
/// an origin server disagree, so the host refuses them.
fn check_raw_path(path: &str) -> Result<(), ProviderHttpError> {
    let lower = path.to_ascii_lowercase();
    if lower.contains("%2f") || lower.contains("%5c") || lower.contains("%2e") {
        return Err(ProviderHttpError::denied(
            "provider-http refuses an encoded separator or dot in the path",
        ));
    }
    if path.contains("//") {
        return Err(ProviderHttpError::denied(
            "provider-http refuses an empty path segment",
        ));
    }
    if path
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return Err(ProviderHttpError::denied(
            "provider-http refuses a dot segment in the path",
        ));
    }
    Ok(())
}

/// `path == prefix || path starts with prefix + "/"`. This is what stops
/// `/v1` from also authorizing `/v10`.
pub(crate) fn path_matches_prefix(prefix: &str, path: &str) -> bool {
    path == prefix || (path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/'))
}

/// Copy guest headers, drop the host-owned names, then inject the credential.
/// A guest value for the credential header is dropped, never merged.
pub(crate) fn sanitize_headers(
    headers: Option<&serde_json::Map<String, Value>>,
    credential: Option<&ResolvedCredential>,
) -> Result<Vec<(String, String)>, ProviderHttpError> {
    let credential_header = credential.map(|credential| credential.header.as_str());
    let mut out: Vec<(String, String)> = Vec::new();
    if let Some(headers) = headers {
        if headers.len() > 64 {
            return Err(ProviderHttpError::denied(
                "provider-http refuses more than 64 headers",
            ));
        }
        for (name, value) in headers {
            if !is_http_token(name) {
                return Err(ProviderHttpError::denied(format!(
                    "provider-http header name `{name}` is not a token"
                )));
            }
            let Some(value) = value.as_str() else {
                return Err(ProviderHttpError::failed(format!(
                    "provider-http header `{name}` must be a string"
                )));
            };
            if value.len() > PROVIDER_HTTP_MAX_HEADER_VALUE {
                return Err(ProviderHttpError::denied(format!(
                    "provider-http header `{name}` is over {PROVIDER_HTTP_MAX_HEADER_VALUE} bytes"
                )));
            }
            if value.contains(['\r', '\n', '\0']) {
                return Err(ProviderHttpError::denied(format!(
                    "provider-http header `{name}` contains CR, LF or NUL"
                )));
            }
            let lower = name.to_ascii_lowercase();
            if STRIPPED_HEADERS.contains(&lower.as_str()) {
                continue;
            }
            if credential_header.is_some_and(|header| header.eq_ignore_ascii_case(name)) {
                continue;
            }
            out.push((lower, value.to_string()));
        }
    }
    if let Some(credential) = credential {
        out.push((
            credential.header.clone(),
            format!("{}{}", credential.prefix, credential.value),
        ));
    }
    Ok(out)
}

fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9')
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
        })
}

/// Resolve the grant's opaque credential handle through the host's own
/// sidecar. The value is re-read at use and re-fingerprinted: a credential that
/// changed since the grant is refused rather than silently used.
pub(crate) fn resolve_credential(
    profile: &ProviderProfile,
    plugin_dir: &Path,
    agent_root: Option<&Path>,
) -> Result<Option<ResolvedCredential>, ProviderHttpError> {
    let Some(handle) = profile.credential.as_deref() else {
        return Ok(None);
    };
    let sidecar_path = plugin_dir.join("credentials.json");
    let sidecar_text = std::fs::read_to_string(&sidecar_path).map_err(|error| {
        ProviderHttpError::failed(format!("read {}: {error}", sidecar_path.display()))
    })?;
    let sidecar: Value = serde_json::from_str(&sidecar_text).map_err(|error| {
        ProviderHttpError::failed(format!("{}: {error}", sidecar_path.display()))
    })?;
    let record = sidecar.get(handle).ok_or_else(|| {
        ProviderHttpError::denied(format!(
            "credential handle `{handle}` is not in {}",
            sidecar_path.display()
        ))
    })?;
    let source = record
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let provider_id = source
        .strip_prefix("auth-json:")
        .filter(|id| !id.is_empty())
        .or_else(|| record.get("provider").and_then(Value::as_str))
        .ok_or_else(|| {
            ProviderHttpError::denied(format!(
                "credential `{handle}` does not name an auth-json provider"
            ))
        })?;
    let recorded = record
        .get("fingerprint")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let root = agent_root
        .map(Path::to_path_buf)
        .or_else(|| {
            plugin_dir
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
        })
        .ok_or_else(|| ProviderHttpError::failed("cannot locate the agent directory"))?;
    let auth_path = root.join("auth.json");
    let auth_text = std::fs::read_to_string(&auth_path).map_err(|error| {
        ProviderHttpError::failed(format!("read {}: {error}", auth_path.display()))
    })?;
    let auth: Value = serde_json::from_str(&auth_text)
        .map_err(|error| ProviderHttpError::failed(format!("{}: {error}", auth_path.display())))?;
    let entry = auth.get(provider_id).ok_or_else(|| {
        ProviderHttpError::denied(format!("auth.json has no credential for `{provider_id}`"))
    })?;
    let value = match entry.get("type").and_then(Value::as_str) {
        Some("api_key") => entry.get("key").and_then(Value::as_str),
        Some("oauth") => entry.get("access").and_then(Value::as_str),
        _ => None,
    }
    .ok_or_else(|| {
        ProviderHttpError::denied(format!(
            "auth.json has no usable credential for `{provider_id}`"
        ))
    })?;
    if sha256_hex(value.as_bytes()) != recorded.to_ascii_lowercase() {
        return Err(ProviderHttpError::denied(
            "credential changed since the grant; re-run `ki plugin grant --credential-from`",
        ));
    }

    let (header, prefix) = match &profile.auth {
        Some(auth) => (auth.header.clone(), auth.prefix.clone()),
        None => (
            "authorization".to_string(),
            crate::grants::PROVIDER_AUTH_PREFIX_DEFAULT.to_string(),
        ),
    };
    Ok(Some(ResolvedCredential {
        header,
        prefix,
        value: value.to_string(),
    }))
}

/// Fingerprint a credential value the way the grant sidecar records it
/// (SHA-256 hex). Exposed so the CLI and tests compute the same string without
/// duplicating the rule.
pub fn fingerprint_credential(value: &str) -> String {
    sha256_hex(value.as_bytes())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The per-call handle table.
///
/// One table lives on one store. `begin_call` bumps the call id and drops every
/// handle the previous call left behind; `reap` drops them at the current
/// call's exit. A handle can therefore never outlive the call that opened it.
#[derive(Default)]
pub(crate) struct ProviderHttpTable {
    call_id: u64,
    next: u32,
    handles: HashMap<u32, OpenHttp>,
}

struct OpenHttp {
    call_id: u64,
    stream: Box<dyn ProviderHttpStream>,
    read_bytes: u64,
}

impl ProviderHttpTable {
    /// Start a provider call. Returns the new call id.
    pub(crate) fn begin_call(&mut self) -> u64 {
        self.handles.clear();
        self.next = 1;
        self.call_id = self.call_id.wrapping_add(1);
        self.call_id
    }

    /// Drop every open stream. Called at entry and exit of every provider-call
    /// entry point, and on unload.
    pub(crate) fn reap(&mut self) {
        self.handles.clear();
        self.next = 1;
    }

    pub(crate) fn open(
        &mut self,
        stream: Box<dyn ProviderHttpStream>,
        max_handles: u32,
    ) -> Result<u32, ProviderHttpError> {
        if self.handles.len() as u32 >= max_handles.max(1) {
            return Err(ProviderHttpError::denied(format!(
                "provider-http already has {} open handles (cap {max_handles})",
                self.handles.len()
            )));
        }
        let handle = self.next;
        self.next = self.next.saturating_add(1).max(1);
        self.handles.insert(
            handle,
            OpenHttp {
                call_id: self.call_id,
                stream,
                read_bytes: 0,
            },
        );
        Ok(handle)
    }

    pub(crate) fn read(
        &mut self,
        handle: u32,
        max_bytes: u64,
        max_response_bytes: u64,
        cancel: Option<&dyn crate::CancelProbe>,
    ) -> Result<Option<Vec<u8>>, ProviderHttpError> {
        if cancel.is_some_and(|probe| probe.is_cancelled()) {
            return Err(ProviderHttpError::failed("provider-http read cancelled"));
        }
        let open = self.handles.get_mut(&handle).ok_or_else(|| {
            ProviderHttpError::failed(format!("no open provider-http handle {handle}"))
        })?;
        if open.call_id != self.call_id {
            return Err(ProviderHttpError::failed(format!(
                "provider-http handle {handle} belongs to an earlier call"
            )));
        }
        let remaining = max_response_bytes.saturating_sub(open.read_bytes);
        if remaining == 0 {
            return Err(ProviderHttpError::failed(format!(
                "provider-http response exceeds {max_response_bytes} bytes"
            )));
        }
        // Read one byte past the remaining budget so "exactly full" and "too
        // big" stay distinguishable.
        let limit = max_bytes
            .min(PROVIDER_HTTP_READ_CHUNK_MAX as u64)
            .min(remaining.saturating_add(1)) as usize;
        match open.stream.read(limit).map_err(ProviderHttpError::failed)? {
            None => Ok(None),
            Some(chunk) => {
                if chunk.len() as u64 > remaining {
                    return Err(ProviderHttpError::failed(format!(
                        "provider-http response exceeds {max_response_bytes} bytes"
                    )));
                }
                open.read_bytes += chunk.len() as u64;
                Ok(Some(chunk))
            }
        }
    }

    pub(crate) fn close(&mut self, handle: u32) -> Result<(), ProviderHttpError> {
        // Idempotent: an unknown handle is already closed.
        self.handles.remove(&handle);
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        self.handles.len()
    }
}

/// The real transport: blocking reqwest over rustls, with redirects disabled
/// and the grant's wall clock applied to the exchange.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReqwestTransport;

impl ProviderHttpTransport for ReqwestTransport {
    fn open(&self, request: ProviderHttpRequest) -> Result<Box<dyn ProviderHttpStream>, String> {
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(request.timeout)
            .build()
            .map_err(|error| format!("provider-http client: {error}"))?;
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|error| format!("provider-http method: {error}"))?;
        let mut builder = client.request(method, &request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = request.body {
            builder = builder.body(body);
        }
        let response = builder
            .send()
            .map_err(|error| format!("provider-http request: {error}"))?;
        let status = response.status().as_u16();
        Ok(Box::new(ReqwestStream {
            response: Some(response),
            status,
            read_timeout: request.read_timeout,
        }))
    }
}

struct ReqwestStream {
    response: Option<reqwest::blocking::Response>,
    status: u16,
    read_timeout: std::time::Duration,
}

impl ProviderHttpStream for ReqwestStream {
    fn status(&self) -> u16 {
        self.status
    }

    fn read(&mut self, max: usize) -> Result<Option<Vec<u8>>, String> {
        use std::io::Read;
        let Some(mut response) = self.response.take() else {
            return Err("provider-http stream is closed after an earlier read".to_string());
        };
        // A blocking `Response::read` cannot be interrupted in place, so the
        // read runs on its own thread and the caller waits for the grant's
        // per-read budget. On timeout the response is dropped (closing the
        // socket); the worker may linger only until the kernel read returns.
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("kiqi-provider-http-read".to_string())
            .spawn(move || {
                let mut buffer = vec![0u8; max.max(1)];
                let result = match response.read(&mut buffer) {
                    Ok(0) => Ok(None),
                    Ok(read) => {
                        buffer.truncate(read);
                        Ok(Some(buffer))
                    }
                    Err(error) => Err(format!("provider-http read: {error}")),
                };
                let _ = tx.send((result, response));
            });
        if let Err(error) = spawned {
            return Err(format!("provider-http read thread: {error}"));
        }
        match rx.recv_timeout(self.read_timeout) {
            Ok((result, response)) => {
                self.response = Some(response);
                result
            }
            Err(_) => Err(format!(
                "provider-http read exceeded {}s",
                self.read_timeout.as_secs()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grants::{
        ProviderEndpoint, ProviderModel, PROVIDER_MAX_HANDLES_DEFAULT,
        PROVIDER_MAX_RESPONSE_BYTES_DEFAULT,
    };

    fn profile() -> ProviderProfile {
        ProviderProfile {
            id: "acme".to_string(),
            display_name: "Acme".to_string(),
            models: vec![ProviderModel {
                id: "acme-1".to_string(),
                api: "openai-completions".to_string(),
                base_url: "https://api.acme.test/v1".to_string(),
                reasoning: false,
                input: vec!["text".to_string()],
            }],
            endpoint: ProviderEndpoint {
                scheme: "https".to_string(),
                host: "api.acme.test".to_string(),
                path_prefix: "/v1".to_string(),
                methods: vec!["POST".to_string()],
            },
            credential: None,
            auth: None,
            timeout_seconds: 30,
            max_response_bytes: PROVIDER_MAX_RESPONSE_BYTES_DEFAULT,
            max_handles: PROVIDER_MAX_HANDLES_DEFAULT,
        }
    }

    fn request_json(url: &str) -> String {
        serde_json::json!({"url": url, "method": "POST"}).to_string()
    }

    #[test]
    fn prefix_matching_is_segment_aware() {
        assert!(path_matches_prefix("/v1", "/v1"));
        assert!(path_matches_prefix("/v1", "/v1/messages"));
        assert!(!path_matches_prefix("/v1", "/v10"));
        assert!(!path_matches_prefix("/v1", "/v2"));
        assert!(!path_matches_prefix("/v1", "/"));
    }

    #[test]
    fn the_endpoint_record_is_a_request_filter_not_only_a_credential_filter() {
        let profile = profile();
        let allowed = checked_request(
            &profile,
            None,
            &request_json("https://api.acme.test/v1/messages?q=1"),
        )
        .expect("granted request");
        assert_eq!(allowed.method, "POST");

        for url in [
            "http://api.acme.test/v1/messages",
            "https://evil.test/v1/messages",
            "https://api.acme.test/v10/messages",
            "https://api.acme.test/v2/messages",
            "https://user:pass@api.acme.test/v1/messages",
            "https://api.acme.test:8443/v1/messages",
            "https://api.acme.test/v1/messages#frag",
            "https://api.acme.test/v1/%2e%2e/admin",
            "https://api.acme.test/v1/%2Fadmin",
            "https://api.acme.test/v1//admin",
            "https://api.acme.test/v1/a/../admin",
        ] {
            assert!(
                checked_request(&profile, None, &request_json(url)).is_err(),
                "{url} must be refused"
            );
        }
    }

    #[test]
    fn a_method_outside_the_grant_is_refused() {
        let profile = profile();
        let request = serde_json::json!({
            "url": "https://api.acme.test/v1/messages",
            "method": "GET",
        })
        .to_string();
        assert!(matches!(
            checked_request(&profile, None, &request),
            Err(ProviderHttpError::Denied(_))
        ));
    }

    #[test]
    fn host_owned_headers_are_stripped_and_the_credential_is_injected() {
        let profile = profile();
        let credential = ResolvedCredential {
            header: "authorization".to_string(),
            prefix: "Bearer ".to_string(),
            value: "sk-secret".to_string(),
        };
        let request = serde_json::json!({
            "url": "https://api.acme.test/v1/messages",
            "method": "POST",
            "headers": {
                "authorization": "guest-secret",
                "x-api-key": "guest-key",
                "x-goog-api-key": "guest-key",
                "proxy-authorization": "guest-proxy",
                "host": "evil.test",
                "content-length": "999",
                "transfer-encoding": "chunked",
                "connection": "close",
                "cookie": "session=1",
                "x-test": "kept",
            },
            "body": "hello",
        })
        .to_string();
        let checked = checked_request(&profile, Some(&credential), &request).expect("checked");
        let names: Vec<&str> = checked
            .headers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(names, vec!["x-test", "authorization"]);
        assert_eq!(checked.headers[0].1, "kept");
        assert_eq!(checked.headers[1].1, "Bearer sk-secret");
        assert_eq!(checked.body.as_deref(), Some(b"hello".as_slice()));
    }

    #[test]
    fn a_header_with_crlf_or_a_non_token_name_is_refused() {
        let profile = profile();
        let injected = serde_json::json!({
            "url": "https://api.acme.test/v1/messages",
            "headers": {"x-test": "a\r\nb"},
        })
        .to_string();
        assert!(checked_request(&profile, None, &injected).is_err());
        let bad_name = serde_json::json!({
            "url": "https://api.acme.test/v1/messages",
            "headers": {"x test": "value"},
        })
        .to_string();
        assert!(checked_request(&profile, None, &bad_name).is_err());
    }

    #[test]
    fn the_table_reaps_at_the_call_boundary() {
        struct NoopStream;
        impl ProviderHttpStream for NoopStream {
            fn status(&self) -> u16 {
                200
            }
            fn read(&mut self, _max: usize) -> Result<Option<Vec<u8>>, String> {
                Ok(None)
            }
        }
        let mut table = ProviderHttpTable::default();
        table.begin_call();
        let handle = table.open(Box::new(NoopStream), 2).expect("open");
        assert_eq!(table.len(), 1);
        table.reap();
        assert_eq!(table.len(), 0);
        assert!(table.read(handle, 1, 1024, None).is_err());
    }
}
