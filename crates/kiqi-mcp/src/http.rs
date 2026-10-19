//! Repo-owned `StreamableHttpClient` implementation over reqwest 0.12.
//!
//! rmcp's streamable-HTTP worker (session adoption, `MCP-Protocol-Version`
//! injection, the standalone GET SSE stream, 404 re-initialization, DELETE on
//! exit) is transport-generic: it drives any [`StreamableHttpClient`]. Its own
//! reqwest client is gated behind `transport-streamable-http-client-reqwest`,
//! which pins reqwest 0.13 — a second major next to ki's 0.12. This module
//! supplies the reqwest face instead, ported from rmcp's implementation with
//! two deliberate differences:
//!
//! * every response is capped at [`MAX_LINE_BYTES`] (the stdio per-frame cap),
//!   because rmcp's HTTP read path has no message-size limit at all;
//! * redirects are followed only within one origin, so a configured server
//!   cannot bounce ki's credentials to another host.
//!
//! The reqwest client itself comes from [`kiqi_ai::http`], so MCP HTTP honors
//! the same proxy resolution and pooled-connection idle-timeout policy as the
//! model providers.

use std::borrow::Cow;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::stream::{BoxStream, Stream, StreamExt};
use http::header::WWW_AUTHENTICATE;
use http::{HeaderName, HeaderValue};
use reqwest::header::ACCEPT;
use rmcp::model::{ClientJsonRpcMessage, JsonRpcMessage, ServerJsonRpcMessage};
use rmcp::transport::common::http_header::{
    EVENT_STREAM_MIME_TYPE, HEADER_LAST_EVENT_ID, HEADER_MCP_PROTOCOL_VERSION, HEADER_SESSION_ID,
    JSON_MIME_TYPE,
};
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, InsufficientScopeError, SseError, StreamableHttpClient, StreamableHttpError,
    StreamableHttpPostResponse,
};
use sse_stream::{Sse, SseStream};

use crate::manager::MAX_LINE_BYTES;

type HttpError = StreamableHttpError<reqwest::Error>;

/// The reqwest-backed client handed to `StreamableHttpClientTransport::with_client`.
#[derive(Clone)]
pub(crate) struct KiHttpClient {
    client: reqwest::Client,
    max_response_bytes: usize,
    /// Bound on a POST/DELETE's whole exchange (connect, headers and body).
    /// rmcp's worker processes one client message inline, so a request that
    /// hangs without this would wedge the worker — and therefore every later
    /// call on this server — for the rest of the session. The long-lived GET
    /// stream deliberately has no such bound.
    request_timeout: Option<Duration>,
    /// The session id this connection has most recently seen.
    last_session_id: Arc<Mutex<Option<String>>>,
    /// Set when that id changes: the server expired the session and rmcp
    /// transparently re-initialized. Per-session MCP state (resource
    /// subscriptions) did not survive, so the manager must restore it.
    session_rotated: Arc<AtomicBool>,
}

impl KiHttpClient {
    /// Build the client for one configured endpoint. The URL is validated here
    /// (the config layer only guarantees it is non-empty) so a typo fails at
    /// connect with a message naming the server. `idle_timeout_ms` is ki's
    /// `httpIdleTimeout` setting, shared with the model providers.
    pub(crate) fn new(
        url: &str,
        idle_timeout_ms: Option<i64>,
        request_timeout: Option<Duration>,
        session_rotated: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let parsed = reqwest::Url::parse(url)
            .map_err(|error| format!("invalid MCP http url {url:?}: {error}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(format!("MCP http url {url:?} must use http or https"));
        }
        if parsed.host_str().is_none() {
            return Err(format!("MCP http url {url:?} has no host"));
        }
        // A URL with userinfo would be turned into a Basic Authorization
        // header by reqwest on every request — a credential channel ki does
        // not document — and the raw URL is logged. Refuse it (the message
        // must NOT echo the URL: it carries the password).
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(
                "MCP http url must not embed credentials (user:password@host is not allowed)"
                    .to_string(),
            );
        }
        // A loopback endpoint is by definition local: an ambient
        // HTTP(S)_PROXY/ALL_PROXY must not route ki's MCP connection through a
        // proxy (which also makes the offline e2e tests proxy-env hermetic).
        let loopback = parsed.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host.ends_with(".localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        let no_proxy;
        let env = if loopback {
            no_proxy = HashMap::from([("no_proxy".to_string(), "*".to_string())]);
            Some(&no_proxy)
        } else {
            None
        };
        let builder = kiqi_ai::http::client_builder_for(url, idle_timeout_ms, env)?;
        // reqwest ALSO auto-detects system proxies on its own, following its
        // own NO_PROXY rules (which do not cover loopback). `no_proxy()`
        // disables that fallback for a local endpoint.
        let builder = if loopback {
            builder.no_proxy()
        } else {
            builder
        };
        let client = builder
            .redirect(reqwest::redirect::Policy::custom(same_origin_redirect))
            .build()
            .map_err(|error| format!("could not build the MCP http client: {error}"))?;
        Ok(Self {
            client,
            max_response_bytes: MAX_LINE_BYTES,
            request_timeout,
            last_session_id: Arc::new(Mutex::new(None)),
            session_rotated,
        })
    }

    /// Record the session id a response advertised, flagging a change. A
    /// transparent re-init is the only way the id can change under us, and the
    /// new server session carries none of the old subscriptions.
    fn observe_session(&self, session_id: &str) {
        let mut last = self.last_session_id.lock().unwrap();
        match last.as_deref() {
            Some(previous) if previous != session_id => {
                *last = Some(session_id.to_string());
                self.session_rotated.store(true, Ordering::Release);
            }
            Some(_) => {}
            None => *last = Some(session_id.to_string()),
        }
    }

    async fn send_post(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        let mut request = self
            .client
            .post(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "));
        if let Some(timeout) = self.request_timeout {
            request = request.timeout(timeout);
        }
        if let Some(auth_header) = auth_header {
            // rmcp's config carries the RAW bearer token, without the prefix.
            request = request.bearer_auth(auth_header);
        }
        request = apply_custom_headers(request, custom_headers)?;
        let session_was_attached = session_id.is_some();
        if let Some(session_id) = session_id {
            request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        }
        let response = request
            .json(&message)
            .send()
            .await
            .map_err(|error| send_error(error, self.request_timeout))?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(header) = response.headers().get(WWW_AUTHENTICATE) {
                let header = header.to_str().map_err(|_| invalid_www_authenticate())?;
                return Err(StreamableHttpError::AuthRequired(AuthRequiredError::new(
                    header.to_string(),
                )));
            }
        }
        if response.status() == reqwest::StatusCode::FORBIDDEN {
            if let Some(header) = response.headers().get(WWW_AUTHENTICATE) {
                let header = header.to_str().map_err(|_| invalid_www_authenticate())?;
                return Err(StreamableHttpError::InsufficientScope(
                    InsufficientScopeError::new(header.to_string(), extract_scope(header)),
                ));
            }
        }
        let status = response.status();
        if matches!(
            status,
            reqwest::StatusCode::ACCEPTED | reqwest::StatusCode::NO_CONTENT
        ) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        // A 404 on a request that carried a session is the spec's session-loss
        // signal; the worker turns it into one re-init + replay.
        if status == reqwest::StatusCode::NOT_FOUND && session_was_attached {
            return Err(StreamableHttpError::SessionExpired);
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).to_string());
        let content_length = response.content_length();
        let session_id = response
            .headers()
            .get(HEADER_SESSION_ID)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        if let Some(session_id) = session_id.as_deref() {
            self.observe_session(session_id);
        }
        // The spec requires 202 for notifications/responses, but some servers
        // send an empty 200; treat both as accepted.
        if status.is_success()
            && content_length == Some(0)
            && matches!(
                message,
                ClientJsonRpcMessage::Notification(_)
                    | ClientJsonRpcMessage::Response(_)
                    | ClientJsonRpcMessage::Error(_)
            )
        {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if !status.is_success() {
            let body = read_body_capped(response, self.max_response_bytes).await?;
            let body = String::from_utf8_lossy(&body);
            // A non-success response may still carry a valid JSON-RPC error,
            // which must surface as McpError rather than a transport failure.
            let is_json = content_type
                .as_deref()
                .is_some_and(|ct| ct.as_bytes().starts_with(JSON_MIME_TYPE.as_bytes()));
            if is_json {
                if let Some(message) = parse_json_rpc_error(&body) {
                    return Ok(StreamableHttpPostResponse::Json(message, session_id));
                }
            }
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                format!("HTTP {status}: {body}"),
            )));
        }
        match content_type.as_deref() {
            Some(ct) if ct.as_bytes().starts_with(EVENT_STREAM_MIME_TYPE.as_bytes()) => {
                // A POST reply is bounded as a whole: the worker closes its
                // stream after the response message anyway.
                let stream = self.sse_stream(
                    response.bytes_stream(),
                    CapMode::Total(self.max_response_bytes),
                );
                Ok(StreamableHttpPostResponse::Sse(stream, session_id))
            }
            Some(ct) if ct.as_bytes().starts_with(JSON_MIME_TYPE.as_bytes()) => {
                let body = read_body_capped(response, self.max_response_bytes).await?;
                match serde_json::from_slice::<ServerJsonRpcMessage>(&body) {
                    Ok(message) => Ok(StreamableHttpPostResponse::Json(message, session_id)),
                    Err(error) => {
                        eprintln!(
                            "[ki mcp] http: could not parse a JSON response ({error}); treating it as accepted"
                        );
                        Ok(StreamableHttpPostResponse::Accepted)
                    }
                }
            }
            _ => Err(StreamableHttpError::UnexpectedContentType(content_type)),
        }
    }

    async fn send_get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, HttpError> {
        let mut request = self
            .client
            .get(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "))
            .header(HEADER_SESSION_ID, session_id.as_ref());
        if let Some(last_event_id) = last_event_id {
            request = request.header(HEADER_LAST_EVENT_ID, last_event_id);
        }
        if let Some(auth_header) = auth_header {
            request = request.bearer_auth(auth_header);
        }
        request = apply_custom_headers(request, custom_headers)?;
        let response = request
            .send()
            .await
            .map_err(|error| send_error(error, self.request_timeout))?;
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        let response = response.error_for_status().map_err(client_error)?;
        match response.headers().get(reqwest::header::CONTENT_TYPE) {
            Some(ct) => {
                if !ct.as_bytes().starts_with(EVENT_STREAM_MIME_TYPE.as_bytes())
                    && !ct.as_bytes().starts_with(JSON_MIME_TYPE.as_bytes())
                {
                    return Err(StreamableHttpError::UnexpectedContentType(Some(
                        String::from_utf8_lossy(ct.as_bytes()).to_string(),
                    )));
                }
            }
            None => return Err(StreamableHttpError::UnexpectedContentType(None)),
        }
        // The standalone subscription stream is long-lived by design, so its
        // total byte count cannot be capped like a response. The bytes between
        // two event separators are capped instead (the stdio per-frame bound).
        Ok(self.sse_stream(
            response.bytes_stream(),
            CapMode::PerEvent(self.max_response_bytes),
        ))
    }

    async fn send_delete(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), HttpError> {
        let mut request = self.client.delete(uri.as_ref());
        if let Some(timeout) = self.request_timeout {
            request = request.timeout(timeout);
        }
        if let Some(auth_header) = auth_header {
            request = request.bearer_auth(auth_header);
        }
        request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        request = apply_custom_headers(request, custom_headers)?;
        let response = request
            .send()
            .await
            .map_err(|error| send_error(error, self.request_timeout))?;
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Ok(());
        }
        response.error_for_status().map_err(client_error)?;
        Ok(())
    }

    /// Wrap a byte stream as SSE, enforcing the size cap. `CapMode::Total`
    /// bounds a whole POST reply; `CapMode::PerEvent` bounds the bytes between
    /// two event separators, which is the long-lived standalone GET stream's
    /// analogue of stdio's per-frame cap.
    fn sse_stream<S>(&self, body: S, mode: CapMode) -> BoxStream<'static, Result<Sse, SseError>>
    where
        S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    {
        let cap = self.max_response_bytes;
        let capped = CappedBytes {
            inner: Box::pin(body),
            mode,
            seen: 0,
            line_empty: true,
            skip_lf: false,
        };
        SseStream::from_bytes_stream(capped)
            .map(move |item| match item {
                Ok(event) if event.data.as_ref().is_some_and(|data| data.len() > cap) => {
                    Err(SseError::Body(Box::new(CapViolation::new(cap))))
                }
                other => other,
            })
            .boxed()
    }
}

impl StreamableHttpClient for KiHttpClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        self.send_post(uri, message, session_id, auth_header, custom_headers)
            .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), HttpError> {
        self.send_delete(uri, session_id, auth_header, custom_headers)
            .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, HttpError> {
        self.send_get_stream(uri, session_id, last_event_id, auth_header, custom_headers)
            .await
    }
}

fn client_error(error: reqwest::Error) -> HttpError {
    StreamableHttpError::Client(error)
}

/// Map a wire failure to the error the caller sees, naming the deadline when the
/// failure was one.
///
/// reqwest's `Display` for an elapsed `.timeout()` is "error sending request for
/// url (...)" — the cause ("operation timed out") only shows up in the source
/// chain, which rmcp flattens away. Two deadlines race here: this per-request
/// wire timeout and rmcp's `PeerRequestOptions::with_timeout`, both set from the
/// same `timeoutSeconds`. Whichever fires first reports, so a bare reqwest error
/// made the surfaced message depend on which layer won. Spelling the timeout out
/// keeps the two paths saying the same thing.
fn client_timeout_error(timeout: Duration) -> HttpError {
    StreamableHttpError::Io(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("request timeout after {timeout:?}"),
    ))
}

/// [`client_error`], unless the wire gave up on our own deadline.
fn send_error(error: reqwest::Error, timeout: Option<Duration>) -> HttpError {
    match timeout {
        Some(timeout) if error.is_timeout() => client_timeout_error(timeout),
        _ => client_error(error),
    }
}

fn invalid_www_authenticate() -> HttpError {
    StreamableHttpError::UnexpectedServerResponse(Cow::from(
        "invalid www-authenticate header value",
    ))
}

fn cap_violation(cap: usize) -> HttpError {
    StreamableHttpError::UnexpectedServerResponse(Cow::Owned(format!(
        "MCP HTTP response exceeds the {cap}-byte cap; closing"
    )))
}

fn parse_json_rpc_error(body: &str) -> Option<ServerJsonRpcMessage> {
    match serde_json::from_str::<ServerJsonRpcMessage>(body) {
        Ok(message @ JsonRpcMessage::Error(_)) => Some(message),
        _ => None,
    }
}

/// Reserved headers a config file may not override. `MCP-Protocol-Version` is
/// reserved in the spec but allowed through: the worker injects the negotiated
/// value after initialization.
fn is_reserved_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    if name.eq_ignore_ascii_case(HEADER_MCP_PROTOCOL_VERSION) {
        return false;
    }
    ["accept", HEADER_SESSION_ID, HEADER_LAST_EVENT_ID]
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

fn apply_custom_headers(
    mut builder: reqwest::RequestBuilder,
    custom_headers: HashMap<HeaderName, HeaderValue>,
) -> Result<reqwest::RequestBuilder, HttpError> {
    for (name, value) in custom_headers {
        if is_reserved_header(&name) {
            return Err(StreamableHttpError::ReservedHeaderConflict(
                name.to_string(),
            ));
        }
        builder = builder.header(name, value);
    }
    Ok(builder)
}

/// `scope=` out of a `WWW-Authenticate` header, quoted or bare (rmcp's helper
/// is crate-private).
fn extract_scope(header: &str) -> Option<String> {
    let lowercase = header.to_ascii_lowercase();
    let key = "scope=";
    let start = lowercase.find(key)? + key.len();
    let value = &header[start..];
    if let Some(stripped) = value.strip_prefix('"') {
        return stripped.find('"').map(|end| stripped[..end].to_string());
    }
    let end = value
        .find(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .unwrap_or(value.len());
    (end > 0).then(|| value[..end].to_string())
}

fn same_origin(a: &reqwest::Url, b: &reqwest::Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Same-origin redirects only: a configured server must not be able to move
/// ki's requests (and their headers) to another host.
fn same_origin_redirect(attempt: reqwest::redirect::Attempt<'_>) -> reqwest::redirect::Action {
    const MAX_REDIRECTS: usize = 10;
    if attempt.previous().len() >= MAX_REDIRECTS {
        return attempt.error("too many redirects");
    }
    match attempt.previous().last() {
        Some(previous) if !same_origin(previous, attempt.url()) => attempt.stop(),
        _ => attempt.follow(),
    }
}

async fn read_body_capped(response: reqwest::Response, cap: usize) -> Result<Bytes, HttpError> {
    // Content-Length lets an oversized response die before a byte is read.
    if response
        .content_length()
        .is_some_and(|len| len > cap as u64)
    {
        return Err(cap_violation(cap));
    }
    let mut stream = Box::pin(response.bytes_stream());
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(client_error)?;
        if body.len().saturating_add(chunk.len()) > cap {
            return Err(cap_violation(cap));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(body))
}

/// One cap violation, surfaced through `sse_stream::Error::Body`.
#[derive(Debug)]
struct CapViolation {
    message: String,
}

impl CapViolation {
    fn new(cap: usize) -> Self {
        Self {
            message: format!("MCP HTTP response exceeds the {cap}-byte cap; closing"),
        }
    }
}

impl std::fmt::Display for CapViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CapViolation {}

/// How [`CappedBytes`] applies the byte budget.
#[derive(Clone, Copy)]
enum CapMode {
    /// The whole stream (a POST reply).
    Total(usize),
    /// Bytes since the last blank line (one SSE event on the GET stream).
    PerEvent(usize),
}

/// Byte-stream adapter: enforce the size cap, and turn a transport read error
/// into something `sse_stream` can carry.
struct CappedBytes<S> {
    inner: Pin<Box<S>>,
    mode: CapMode,
    seen: usize,
    /// The current line has no characters yet (a blank line ends an SSE
    /// event and starts a fresh per-event budget).
    line_empty: bool,
    /// The previous byte was `\r`: a following `\n` is its CRLF pair, not a
    /// second line terminator.
    skip_lf: bool,
}

impl<S> Stream for CappedBytes<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>>,
{
    type Item = Result<Bytes, CapViolation>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    match this.mode {
                        CapMode::Total(cap) => {
                            this.seen = this.seen.saturating_add(chunk.len());
                            if this.seen > cap {
                                return Poll::Ready(Some(Err(CapViolation::new(cap))));
                            }
                        }
                        CapMode::PerEvent(cap) => {
                            for &byte in chunk.iter() {
                                // SSE treats CR, LF and CRLF as line
                                // terminators (sse_stream parses exactly that
                                // way), and an empty line ends the event. A
                                // CR-only server (`data: ...\r\r`) must reset
                                // the budget just like `\n\n` does.
                                if byte == b'\r' {
                                    if this.line_empty {
                                        this.seen = 0;
                                    }
                                    this.line_empty = true;
                                    this.skip_lf = true;
                                } else if byte == b'\n' {
                                    if this.skip_lf {
                                        this.skip_lf = false;
                                    } else {
                                        if this.line_empty {
                                            this.seen = 0;
                                        }
                                        this.line_empty = true;
                                    }
                                } else {
                                    this.skip_lf = false;
                                    this.line_empty = false;
                                }
                                this.seen = this.seen.saturating_add(1);
                                if this.seen > cap {
                                    return Poll::Ready(Some(Err(CapViolation::new(cap))));
                                }
                            }
                        }
                    }
                    if !chunk.is_empty() {
                        return Poll::Ready(Some(Ok(chunk)));
                    }
                }
                Poll::Ready(Some(Err(error))) => {
                    return Poll::Ready(Some(Err(CapViolation {
                        message: format!("MCP HTTP body read failed: {error}"),
                    })))
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_headers_are_rejected_but_protocol_version_passes() {
        assert!(is_reserved_header(&HeaderName::from_static("accept")));
        assert!(is_reserved_header(&HeaderName::from_static(
            "mcp-session-id"
        )));
        assert!(is_reserved_header(&HeaderName::from_static(
            "last-event-id"
        )));
        assert!(!is_reserved_header(&HeaderName::from_static(
            "mcp-protocol-version"
        )));
        assert!(!is_reserved_header(&HeaderName::from_static(
            "authorization"
        )));
    }

    /// A per-request timeout must surface as a *timeout*.
    ///
    /// reqwest's own `Display` is "error sending request for url (...)" and the
    /// cause only lives in the source chain, which rmcp flattens. rmcp's
    /// `PeerRequestOptions::with_timeout` uses the same `timeoutSeconds`, so the
    /// two deadlines race and the surfaced text used to depend on which fired
    /// first — the http e2e asserted on "timeout" and flaked when the wire won.
    #[tokio::test]
    async fn a_wire_timeout_is_reported_as_a_timeout() {
        // Accept the connection and never answer: the wire timeout has to fire.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (_stream, _peer) = listener.accept().await.unwrap();
            // Hold the socket open without writing a response.
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let timeout = Duration::from_millis(150);
        let error = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/mcp"))
            .timeout(timeout)
            .json(&serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }))
            .send()
            .await
            .expect_err("the peer never answers");
        assert!(error.is_timeout(), "not a timeout: {error:?}");

        let mapped = send_error(error, Some(timeout));
        let text = mapped.to_string();
        assert!(
            text.contains("timeout"),
            "the caller must see a timeout, got {text:?}"
        );
        assert!(text.contains("150ms"), "got {text:?}");

        server.abort();
    }

    #[test]
    fn scope_parsing_handles_quoted_and_bare_forms() {
        assert_eq!(
            extract_scope(r#"Bearer error="insufficient_scope", scope="files:read files:write""#),
            Some("files:read files:write".to_string())
        );
        assert_eq!(
            extract_scope("Bearer scope=read:data, error=\"insufficient_scope\""),
            Some("read:data".to_string())
        );
        assert_eq!(extract_scope("Bearer error=\"invalid_token\""), None);
    }

    #[tokio::test]
    async fn capped_bytes_stops_a_stream_past_the_cap() {
        let chunks: Vec<Result<Bytes, reqwest::Error>> = vec![
            Ok(Bytes::from_static(b"1234")),
            Ok(Bytes::from_static(b"5678")),
        ];
        let mut stream = CappedBytes {
            inner: Box::pin(futures::stream::iter(chunks)),
            mode: CapMode::Total(6),
            seen: 0,
            line_empty: true,
            skip_lf: false,
        };
        assert_eq!(stream.next().await.unwrap().unwrap(), "1234");
        let error = stream.next().await.unwrap().unwrap_err();
        assert!(error.to_string().contains("6-byte cap"), "{error}");
    }

    #[tokio::test]
    async fn capped_bytes_per_event_resets_at_a_blank_line() {
        // Two 9-byte events: over a 10-byte total, but each is within the
        // per-event budget because the blank line resets it.
        let chunks: Vec<Result<Bytes, reqwest::Error>> =
            vec![Ok(Bytes::from_static(b"data: 1\n\ndata: 2\n\n"))];
        let mut stream = CappedBytes {
            inner: Box::pin(futures::stream::iter(chunks)),
            mode: CapMode::PerEvent(10),
            seen: 0,
            line_empty: true,
            skip_lf: false,
        };
        assert!(stream.next().await.unwrap().is_ok());
        assert!(stream.next().await.is_none());

        // A CRLF blank line resets too, even when a chunk boundary splits it.
        let chunks: Vec<Result<Bytes, reqwest::Error>> = vec![
            Ok(Bytes::from_static(b"data: 1\r\n\r")),
            Ok(Bytes::from_static(b"\ndata: 2\r\n\r\n")),
        ];
        let mut stream = CappedBytes {
            inner: Box::pin(futures::stream::iter(chunks)),
            mode: CapMode::PerEvent(12),
            seen: 0,
            line_empty: true,
            skip_lf: false,
        };
        assert!(stream.next().await.unwrap().is_ok());

        // A single event past the budget is rejected.
        let chunks: Vec<Result<Bytes, reqwest::Error>> =
            vec![Ok(Bytes::from_static(b"data: 1234567890123\n\n"))];
        let mut stream = CappedBytes {
            inner: Box::pin(futures::stream::iter(chunks)),
            mode: CapMode::PerEvent(10),
            seen: 0,
            line_empty: true,
            skip_lf: false,
        };
        assert!(stream.next().await.unwrap().is_err());
    }

    #[tokio::test]
    async fn capped_bytes_per_event_resets_at_a_bare_cr_blank_line() {
        // The SSE spec allows CR-only framing, and sse_stream accepts it. The
        // per-event budget must reset on `\r\r` just like `\n\n`, or the
        // long-lived GET stream dies after 16 MiB of perfectly valid events.
        let chunks: Vec<Result<Bytes, reqwest::Error>> = vec![
            Ok(Bytes::from_static(b"data: 1\r\r")),
            Ok(Bytes::from_static(b"data: 2\r\r")),
        ];
        let mut stream = CappedBytes {
            inner: Box::pin(futures::stream::iter(chunks)),
            mode: CapMode::PerEvent(10),
            seen: 0,
            line_empty: true,
            skip_lf: false,
        };
        assert!(stream.next().await.unwrap().is_ok());
        assert!(stream.next().await.unwrap().is_ok());
    }
}
