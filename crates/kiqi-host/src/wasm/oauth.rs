//! Host-attributed OAuth plumbing.
//!
//! The `oauth-http` ABI import is deliberately thin for the guest: it names a
//! granted provider id and a small form, and gets back a status enum. Everything
//! token-bearing happens here — the grant supplies the endpoints and client id,
//! the host performs the POST, parses the standard OAuth response itself and
//! hands the token to an injected [`PluginCredentialSink`]. No token value is
//! ever returned to the guest (or logged).
//!
//! Device authorization is **host-side too**: [`request_device_authorization`]
//! and [`poll_device_token`] are called by `ki auth`, never by a guest, so the
//! device code never enters the sandbox. They share [`OAuthRequestBudget`] with
//! the guest-facing exchange, so a flow is bounded by the grant's wall-clock
//! budget and request count either way.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::grants::{
    OAuthFlow, OAuthGrant, OAuthProviderProfile, OAUTH_MAX_REQUESTS_DEFAULT,
    OAUTH_TIMEOUT_DEFAULT_SECONDS,
};

/// The only form keys a guest may send to `oauth-http.token-request`.
///
/// `client_id` is not here on purpose: it comes from the grant, never the
/// guest. `client_secret` is refused explicitly (see [`parse_token_form`]).
pub const TOKEN_FORM_KEYS: &[&str] = &[
    "grant_type",
    "code",
    "code_verifier",
    "redirect_uri",
    "scope",
    "device_code",
];

/// The largest token/device response the host will read. Real responses are a
/// few hundred bytes; the cap keeps a hostile IdP from making the host allocate.
pub const MAX_OAUTH_RESPONSE_BYTES: usize = 64 * 1024;

/// A token pair the host parsed and will store. It never leaves the host: the
/// guest sees only [`crate::wasm::kiqi::plugin::types::oauth_credential_status`].
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthTokens {
    pub access: String,
    pub refresh: String,
    /// Unix epoch seconds; `0` means no known expiry.
    pub expires: i64,
}

impl fmt::Debug for OAuthTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never let a `{:?}` in a diagnostic print token material.
        f.debug_struct("OAuthTokens")
            .field("access", &"<redacted>")
            .field("refresh", &"<redacted>")
            .field("expires", &self.expires)
            .finish()
    }
}

/// The credential values the host read back for revocation.
#[derive(Clone, PartialEq, Eq)]
pub struct StoredOAuthToken {
    pub access: String,
    pub refresh: String,
    pub expires: i64,
    /// Digest of the provider profile that issued this credential. Legacy
    /// entries have no binding and must not be sent to a current grant's IdP.
    pub profile_fingerprint: Option<String>,
}

impl fmt::Debug for StoredOAuthToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredOAuthToken")
            .field("access", &"<redacted>")
            .field("refresh", &"<redacted>")
            .field("expires", &self.expires)
            .field("profile_fingerprint", &self.profile_fingerprint)
            .finish()
    }
}

/// Bind a stored credential to the IdP profile that issued it.
///
/// Paths, scopes and flow preferences are deliberately excluded: changing
/// those does not change where a live token would be sent. Every endpoint host
/// (including an optional revocation host), the provider id and the client id
/// are included, so re-granting another IdP cannot replay the old credential.
pub fn oauth_profile_fingerprint(profile: &OAuthProviderProfile) -> Result<String, String> {
    fn origin(endpoint: &str) -> Result<String, String> {
        Ok(granted_endpoint(endpoint)?.origin().ascii_serialization())
    }

    let endpoints = serde_json::json!({
        "authorization": origin(&profile.authorization_endpoint)?,
        "token": origin(&profile.token_endpoint)?,
        "device-authorization": profile.device_authorization_endpoint.as_deref().map(origin).transpose()?,
        "revocation": profile.revocation_endpoint.as_deref().map(origin).transpose()?,
    });
    let material = serde_json::to_vec(&serde_json::json!({
        "provider-id": profile.id,
        "client-id": profile.client_id,
        "endpoint-hosts": endpoints,
    }))
    .map_err(|error| format!("could not fingerprint OAuth profile: {error}"))?;
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(&material);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Refuse before a stored token can leave the process under a different grant.
pub fn ensure_oauth_profile_matches(
    profile: &OAuthProviderProfile,
    token: &StoredOAuthToken,
) -> Result<(), String> {
    let actual = token.profile_fingerprint.as_deref().ok_or_else(|| {
        "stored OAuth credential has no issuing-profile binding; refusing to send it to the current endpoint"
            .to_string()
    })?;
    let expected = oauth_profile_fingerprint(profile)?;
    if actual != expected {
        return Err(
            "stored OAuth credential is bound to a different issuing profile; refusing to send it to the current endpoint"
                .to_string(),
        );
    }
    Ok(())
}

/// Where a plugin's own OAuth credential goes.
///
/// This is the **only** way a guest's token reaches disk. The host crate cannot
/// depend on `kiqi-cli` (the dependency runs the other way), so the CLI installs
/// a sink when it constructs the instance; the default is absent, which makes
/// an exchange return `failed` rather than writing somewhere it should not.
///
/// Implementations must namespace the key as
/// `plugin:<install-id>:<provider-id>` and must never replace a non-plugin
/// `auth.json` entry.
pub trait PluginCredentialSink: fmt::Debug + Send + Sync {
    fn store_plugin_oauth(
        &self,
        install_id: &str,
        provider_id: &str,
        access: &str,
        refresh: &str,
        expires: i64,
        profile_fingerprint: &str,
    ) -> Result<(), String>;

    fn read_plugin_oauth(
        &self,
        install_id: &str,
        provider_id: &str,
    ) -> Result<Option<StoredOAuthToken>, String>;

    fn delete_plugin_oauth(&self, install_id: &str, provider_id: &str) -> Result<bool, String>;
}

/// One HTTP response from a token/device/revocation endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    /// `Location` for a 3xx. Redirects are never followed; the header is kept so
    /// the host can say *which* host was refused.
    pub location: Option<String>,
    pub body: String,
}

/// The HTTP seam, injectable so the bounded/policy behaviour is testable
/// without a network or TLS.
pub trait OAuthTransport: fmt::Debug + Send + Sync {
    fn post_form(
        &self,
        url: &str,
        form: &[(String, String)],
        timeout: Duration,
    ) -> Result<HttpResponse, String>;
}

/// The production transport: one blocking POST, no redirect following.
#[derive(Debug, Default, Clone)]
pub struct HttpOAuthTransport;

impl OAuthTransport for HttpOAuthTransport {
    fn post_form(
        &self,
        url: &str,
        form: &[(String, String)],
        timeout: Duration,
    ) -> Result<HttpResponse, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            // A redirect is a second request to a host the grant did not name.
            // Refuse it outright; the caller reports the 3xx.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| format!("oauth client: {error}"))?;
        let response = client
            .post(url)
            .form(form)
            .send()
            .map_err(|error| format!("oauth request failed: {error}"))?;
        let status = response.status().as_u16();
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = {
            use std::io::Read;
            let mut bytes = Vec::new();
            response
                .take(MAX_OAUTH_RESPONSE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| format!("oauth response: {error}"))?;
            if bytes.len() > MAX_OAUTH_RESPONSE_BYTES {
                return Err(format!(
                    "oauth response exceeds {MAX_OAUTH_RESPONSE_BYTES} bytes"
                ));
            }
            String::from_utf8_lossy(&bytes).into_owned()
        };
        Ok(HttpResponse {
            status,
            location,
            body,
        })
    }
}

/// One flow's request/deadline budget, derived from the grant.
///
/// The clock starts at the first charge, so loading a plugin does not consume
/// the budget before the user begins a login. Enforcement is host-side because
/// epoch/fuel cannot interrupt a guest blocked in a host import.
#[derive(Debug, Clone)]
pub struct OAuthRequestBudget {
    timeout: Duration,
    max_requests: u32,
    started: Option<Instant>,
    used: u32,
}

impl OAuthRequestBudget {
    pub fn for_grant(grant: &OAuthGrant) -> Self {
        Self::new(grant.timeout_seconds, grant.max_requests)
    }

    pub fn new(timeout_seconds: u64, max_requests: u32) -> Self {
        Self {
            timeout: Duration::from_secs(timeout_seconds.max(1)),
            max_requests: max_requests.max(1),
            started: None,
            used: 0,
        }
    }

    /// Account for one outbound request, or explain why it may not happen.
    pub fn charge(&mut self) -> Result<(), String> {
        let started = *self.started.get_or_insert_with(Instant::now);
        if started.elapsed() >= self.timeout {
            return Err(format!(
                "oauth flow deadline of {}s exceeded",
                self.timeout.as_secs()
            ));
        }
        if self.used >= self.max_requests {
            return Err(format!(
                "oauth request budget exhausted ({} requests)",
                self.max_requests
            ));
        }
        self.used += 1;
        Ok(())
    }

    /// A per-request timeout that never outlives the flow deadline.
    pub fn request_timeout(&self) -> Duration {
        let started = self.started.unwrap_or_else(Instant::now);
        let left = self.timeout.saturating_sub(started.elapsed());
        left.max(Duration::from_secs(1))
    }

    /// Start a new flow: the previous exchange completed (or was revoked), so
    /// its counters must not make a later re-login impossible.
    pub fn reset(&mut self) {
        self.started = None;
        self.used = 0;
    }

    pub fn used(&self) -> u32 {
        self.used
    }

    pub fn max_requests(&self) -> u32 {
        self.max_requests
    }
}

impl Default for OAuthRequestBudget {
    fn default() -> Self {
        Self::new(OAUTH_TIMEOUT_DEFAULT_SECONDS, OAUTH_MAX_REQUESTS_DEFAULT)
    }
}

/// Parse and police the guest-supplied `form` JSON object.
///
/// Returns the fields in a host-controlled order. Refusals are policy, not
/// transport failures: `client_secret`, `client_id` and every unknown key are
/// rejected, and `grant_type` is limited to the two grants a plugin may use.
pub fn parse_token_form(form: &str) -> Result<Vec<(String, String)>, String> {
    let value: serde_json::Value = serde_json::from_str(form)
        .map_err(|_| "`oauth-http` form must be a JSON object of string values".to_string())?;
    let serde_json::Value::Object(fields) = value else {
        return Err("`oauth-http` form must be a JSON object of string values".to_string());
    };
    let mut out = Vec::new();
    let mut grant_type: Option<String> = None;
    let mut has_device_code = false;
    for (key, value) in fields {
        if key == "client_secret" {
            return Err(
                "`oauth-http` form must not carry `client_secret`; the host authenticates with \
                 the grant's client id"
                    .to_string(),
            );
        }
        if key == "client_id" {
            return Err(
                "`oauth-http` form must not carry `client_id`; the host takes it from the grant"
                    .to_string(),
            );
        }
        if !TOKEN_FORM_KEYS.contains(&key.as_str()) {
            return Err(format!("`oauth-http` form key `{key}` is not allowed"));
        }
        let Some(text) = value.as_str() else {
            return Err(format!("`oauth-http` form key `{key}` must be a string"));
        };
        if key == "grant_type" {
            grant_type = Some(text.to_string());
        }
        if key == "code_verifier" {
            validate_code_verifier(text)?;
        }
        has_device_code |= key == "device_code";
        out.push((key, text.to_string()));
    }

    let grant_type = match grant_type {
        Some(value) => value,
        // Infer the only sensible grant: a device code means device polling,
        // everything else is the authorization-code exchange.
        None if has_device_code => "urn:ietf:params:oauth:grant-type:device_code".to_string(),
        None => "authorization_code".to_string(),
    };
    if grant_type != "authorization_code"
        && grant_type != "urn:ietf:params:oauth:grant-type:device_code"
    {
        return Err(format!(
            "`oauth-http` grant_type `{grant_type}` is not allowed"
        ));
    }
    // Replace (or add) the single normalized grant_type field.
    out.retain(|(key, _)| key != "grant_type");
    out.insert(0, ("grant_type".to_string(), grant_type));
    Ok(out)
}

/// Enforce the grant on an already-normalized token form.
///
/// The guest may narrow `scope`, but every requested scope must be granted.
/// An omitted scope uses the profile's set rather than an IdP default. Nothing
/// outside the granted flow/scope reaches the transport or consumes budget.
pub fn enforce_token_form_grant(
    profile: &OAuthProviderProfile,
    mut form: Vec<(String, String)>,
) -> Result<Vec<(String, String)>, String> {
    let grant_type = form
        .iter()
        .find(|(key, _)| key == "grant_type")
        .map(|(_, value)| value.as_str())
        .ok_or_else(|| "`oauth-http` normalized form has no grant_type".to_string())?;
    let (flow, flow_name) = match grant_type {
        "authorization_code" => (OAuthFlow::AuthorizationCode, "authorization-code"),
        "urn:ietf:params:oauth:grant-type:device_code" => (OAuthFlow::DeviceCode, "device-code"),
        _ => return Err("`oauth-http` normalized grant_type is not allowed".to_string()),
    };
    if !profile.flows.contains(&flow) {
        return Err(format!(
            "oauth provider `{}` does not grant the {flow_name} flow",
            profile.id
        ));
    }

    let requested_scopes = form
        .iter()
        .find(|(key, _)| key == "scope")
        .map(|(_, value)| {
            value
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        });
    if let Some(scopes) = &requested_scopes {
        if scopes.iter().any(|scope| !profile.scopes.contains(scope)) {
            // Do not echo guest-controlled scope text into the error channel.
            return Err(format!(
                "oauth provider `{}` does not grant the requested scope",
                profile.id
            ));
        }
    }

    form.retain(|(key, _)| key != "scope");
    let scopes = requested_scopes.unwrap_or_else(|| profile.scopes.clone());
    if !scopes.is_empty() {
        form.push(("scope".to_string(), scopes.join(" ")));
    }
    Ok(form)
}

/// RFC 7636: 43-128 characters from the unreserved set.
fn validate_code_verifier(value: &str) -> Result<(), String> {
    if !(43..=128).contains(&value.len()) {
        return Err("`oauth-http` code_verifier must be 43-128 characters (RFC 7636)".to_string());
    }
    if !value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-._~".contains(character))
    {
        return Err("`oauth-http` code_verifier carries a non-unreserved character".to_string());
    }
    Ok(())
}

/// Parse the standard OAuth token response.
///
/// Errors never echo the body: a failed response may contain a token or an
/// attacker-chosen string. Only a whitelisted `error` code is repeated.
pub fn parse_token_response(body: &str, now: i64) -> Result<OAuthTokens, String> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|_| "token endpoint response is not valid JSON".to_string())?;
    let serde_json::Value::Object(fields) = value else {
        return Err("token endpoint response is not a JSON object".to_string());
    };
    if let Some(error) = fields.get("error").and_then(serde_json::Value::as_str) {
        return Err(safe_oauth_error(error));
    }
    let access = fields
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "token endpoint response carries no access_token".to_string())?
        .to_string();
    let refresh = fields
        .get("refresh_token")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let expires = match fields.get("expires_in") {
        None | Some(serde_json::Value::Null) => 0,
        Some(serde_json::Value::Number(number)) => {
            let seconds = number
                .as_i64()
                .ok_or_else(|| "token endpoint `expires_in` is not an integer".to_string())?;
            if seconds < 0 {
                return Err("token endpoint `expires_in` is negative".to_string());
            }
            now.saturating_add(seconds)
        }
        Some(serde_json::Value::String(text)) => {
            let seconds: i64 = text
                .parse()
                .map_err(|_| "token endpoint `expires_in` is not an integer".to_string())?;
            if seconds < 0 {
                return Err("token endpoint `expires_in` is negative".to_string());
            }
            now.saturating_add(seconds)
        }
        Some(_) => return Err("token endpoint `expires_in` is not an integer".to_string()),
    };
    Ok(OAuthTokens {
        access,
        refresh,
        expires,
    })
}

/// The OAuth `error` code a response carries, if any.
pub fn oauth_error_code(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
}

/// Known OAuth error codes are safe to repeat. Anything else is summarized, so
/// a hostile IdP cannot smuggle a token (or arbitrary text) into a diagnostic.
fn safe_oauth_error(code: &str) -> String {
    const SAFE: &[&str] = &[
        "access_denied",
        "authorization_pending",
        "expired_token",
        "invalid_client",
        "invalid_grant",
        "invalid_request",
        "invalid_scope",
        "slow_down",
        "temporarily_unavailable",
        "unauthorized_client",
        "unsupported_grant_type",
        "unsupported_response_type",
    ];
    if SAFE.contains(&code) {
        format!("token endpoint returned `{code}`")
    } else {
        "token endpoint returned an OAuth error".to_string()
    }
}

/// Accept exactly the grant's endpoint, re-checked at call time.
///
/// The grant validator already enforced this at write time, but a hand-edited
/// `grants.json` or a future code path must not be able to point a token request
/// anywhere else. Only `https`, a real host, no userinfo and no fragment pass.
pub fn granted_endpoint(url: &str) -> Result<reqwest::Url, String> {
    let parsed = reqwest::Url::parse(url).map_err(|error| format!("invalid endpoint: {error}"))?;
    if parsed.scheme() != "https" {
        return Err(format!("oauth endpoint must be https, got `{url}`"));
    }
    if parsed.host_str().unwrap_or_default().is_empty() {
        return Err(format!("oauth endpoint has no host: `{url}`"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(format!("oauth endpoint must not carry userinfo: `{url}`"));
    }
    if parsed.fragment().is_some() {
        return Err(format!("oauth endpoint must not carry a fragment: `{url}`"));
    }
    Ok(parsed)
}

/// A parsed device-authorization response. Kept host-side: neither the device
/// code nor the interval is sent back into the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    /// Seconds between polls; `5` when the IdP does not say.
    pub interval: u64,
    /// Unix epoch seconds. `0` means no known expiry.
    pub expires_at: i64,
}

/// Start the device flow: one host-side POST to the grant's
/// `device-authorization-endpoint`.
pub fn request_device_authorization(
    profile: &OAuthProviderProfile,
    budget: &mut OAuthRequestBudget,
    transport: &dyn OAuthTransport,
    now: i64,
) -> Result<DeviceAuthorization, String> {
    if !profile.flows.contains(&OAuthFlow::DeviceCode) {
        return Err(format!(
            "oauth provider `{}` does not grant the device-code flow",
            profile.id
        ));
    }
    let endpoint = profile
        .device_authorization_endpoint
        .as_deref()
        .ok_or_else(|| {
            format!(
                "oauth provider `{}` has no device-authorization-endpoint",
                profile.id
            )
        })?;
    let url = granted_endpoint(endpoint)?;
    budget.charge()?;
    let mut form = vec![("client_id".to_string(), profile.client_id.clone())];
    if !profile.scopes.is_empty() {
        form.push(("scope".to_string(), profile.scopes.join(" ")));
    }
    let response = transport.post_form(url.as_str(), &form, budget.request_timeout())?;
    if !(200..300).contains(&response.status) {
        return Err(format!(
            "device authorization returned HTTP {}",
            response.status
        ));
    }
    let value: serde_json::Value = serde_json::from_str(&response.body)
        .map_err(|_| "device authorization response is not valid JSON".to_string())?;
    let field = |name: &str| {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let device_code = field("device_code")
        .ok_or_else(|| "device authorization response carries no device_code".to_string())?;
    let user_code = field("user_code")
        .ok_or_else(|| "device authorization response carries no user_code".to_string())?;
    let verification_uri = field("verification_uri")
        .ok_or_else(|| "device authorization response carries no verification_uri".to_string())?;
    let interval = value
        .get("interval")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(5)
        .clamp(1, 60);
    let expires_in = value
        .get("expires_in")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(900)
        .max(0);
    Ok(DeviceAuthorization {
        device_code,
        user_code,
        verification_uri,
        interval,
        expires_at: now.saturating_add(expires_in),
    })
}

/// Poll the token endpoint until the device flow completes, slows down or the
/// grant budget runs out.
///
/// Bounded three ways: the response's `expires_at`, the grant's wall clock and
/// the grant's `max-requests`. `slow_down` grows the interval by 5 seconds, like
/// the RFC; the interval is itself clamped so a hostile response cannot park the
/// host for minutes.
pub fn poll_device_token(
    profile: &OAuthProviderProfile,
    authorization: &DeviceAuthorization,
    budget: &mut OAuthRequestBudget,
    transport: &dyn OAuthTransport,
    now: i64,
) -> Result<OAuthTokens, String> {
    let url = granted_endpoint(&profile.token_endpoint)?;
    let mut interval = authorization.interval.clamp(1, 60);
    loop {
        if authorization.expires_at > 0
            && now.saturating_add(elapsed_seconds(budget)) > authorization.expires_at
        {
            return Err("device authorization expired".to_string());
        }
        budget.charge()?;
        let form = vec![
            (
                "grant_type".to_string(),
                "urn:ietf:params:oauth:grant-type:device_code".to_string(),
            ),
            ("device_code".to_string(), authorization.device_code.clone()),
            ("client_id".to_string(), profile.client_id.clone()),
        ];
        let response = transport.post_form(url.as_str(), &form, budget.request_timeout())?;
        if (200..300).contains(&response.status) {
            return parse_token_response(
                &response.body,
                now.saturating_add(elapsed_seconds(budget)),
            );
        }
        match oauth_error_code(&response.body).as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => {
                interval = (interval + 5).min(60);
            }
            Some(code) => return Err(safe_oauth_error(code)),
            None => {
                return Err(format!(
                    "token endpoint returned HTTP {} while polling",
                    response.status
                ))
            }
        }
        std::thread::sleep(Duration::from_secs(interval));
    }
}

fn elapsed_seconds(budget: &OAuthRequestBudget) -> i64 {
    budget
        .started
        .map(|started| started.elapsed().as_secs() as i64)
        .unwrap_or(0)
}

/// A shared transport for callers that do not inject one.
pub fn default_transport() -> Arc<dyn OAuthTransport> {
    Arc::new(HttpOAuthTransport)
}

/// Exchange an authorization code the user pasted back.
///
/// **Host-only.** `ki auth login` calls this for the authorization-code flow;
/// it is not reachable from a guest. The grant supplies the client id and token
/// endpoint, the caller supplies only the code and (when the profile supports
/// PKCE) the verifier. The host parses the standard response and returns the
/// token pair; nothing token-bearing is ever handed back to a guest.
pub fn exchange_authorization_code(
    profile: &OAuthProviderProfile,
    code: &str,
    code_verifier: Option<&str>,
    redirect_uri: &str,
    budget: &mut OAuthRequestBudget,
    transport: &dyn OAuthTransport,
    now: i64,
) -> Result<OAuthTokens, String> {
    let url = granted_endpoint(&profile.token_endpoint)?;
    let mut form: Vec<(String, String)> = vec![
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("code".to_string(), code.to_string()),
    ];
    if let Some(verifier) = code_verifier {
        validate_code_verifier(verifier)?;
        form.push(("code_verifier".to_string(), verifier.to_string()));
    }
    if !redirect_uri.is_empty() {
        form.push(("redirect_uri".to_string(), redirect_uri.to_string()));
    }
    if !profile.scopes.is_empty() {
        form.push(("scope".to_string(), profile.scopes.join(" ")));
    }
    // The client id is the grant's, never the caller's.
    form.push(("client_id".to_string(), profile.client_id.clone()));
    budget.charge()?;
    let response = transport.post_form(url.as_str(), &form, budget.request_timeout())?;
    finish_token_response(&response, now)
}

/// Refresh a stored token at the grant's token endpoint.
///
/// **Host-only**, like [`exchange_authorization_code`]: `ki auth refresh` never
/// hands the refresh token to a guest. A response that omits `refresh_token`
/// leaves the caller's existing refresh value in place (the caller decides
/// that; this function returns an empty string for the missing field).
pub fn refresh_stored_token(
    profile: &OAuthProviderProfile,
    refresh: &str,
    budget: &mut OAuthRequestBudget,
    transport: &dyn OAuthTransport,
    now: i64,
) -> Result<OAuthTokens, String> {
    if refresh.is_empty() {
        return Err(format!(
            "oauth provider `{}` has no stored refresh token",
            profile.id
        ));
    }
    let url = granted_endpoint(&profile.token_endpoint)?;
    let mut form: Vec<(String, String)> = vec![
        ("grant_type".to_string(), "refresh_token".to_string()),
        ("refresh_token".to_string(), refresh.to_string()),
    ];
    if !profile.scopes.is_empty() {
        form.push(("scope".to_string(), profile.scopes.join(" ")));
    }
    form.push(("client_id".to_string(), profile.client_id.clone()));
    budget.charge()?;
    let response = transport.post_form(url.as_str(), &form, budget.request_timeout())?;
    finish_token_response(&response, now)
}

/// Revoke the token the host stored for a granted provider.
///
/// **Host-only**: `ki auth logout` calls this with the value it read from the
/// namespaced store; a guest can only ask for the same operation through
/// `oauth-http.revoke`, which never sees the token either. A profile without a
/// `revocation-endpoint` cannot revoke, and says so rather than silently
/// pretending the token is dead at the IdP.
pub fn revoke_stored_token(
    profile: &OAuthProviderProfile,
    token: &StoredOAuthToken,
    budget: &mut OAuthRequestBudget,
    transport: &dyn OAuthTransport,
) -> Result<(), String> {
    ensure_oauth_profile_matches(profile, token)?;
    let endpoint_text = profile
        .revocation_endpoint
        .as_deref()
        .ok_or_else(|| format!("oauth provider `{}` has no revocation-endpoint", profile.id))?;
    let endpoint = granted_endpoint(endpoint_text)?;
    let (hint, value) = if token.refresh.is_empty() {
        ("access_token", token.access.clone())
    } else {
        ("refresh_token", token.refresh.clone())
    };
    let form: Vec<(String, String)> = vec![
        ("token".to_string(), value),
        ("token_type_hint".to_string(), hint.to_string()),
        ("client_id".to_string(), profile.client_id.clone()),
    ];
    budget.charge()?;
    let response = transport.post_form(endpoint.as_str(), &form, budget.request_timeout())?;
    if !(200..300).contains(&response.status) {
        return Err(format!(
            "revocation endpoint returned HTTP {}",
            response.status
        ));
    }
    Ok(())
}

/// The shared half of the three host-side endpoints: validate the status, parse
/// the standard response, and never echo the body on failure.
fn finish_token_response(response: &HttpResponse, now: i64) -> Result<OAuthTokens, String> {
    if !(200..300).contains(&response.status) {
        if (300..400).contains(&response.status) {
            let host = response
                .location
                .as_deref()
                .and_then(|location| reqwest::Url::parse(location).ok())
                .and_then(|url| url.host_str().map(str::to_string));
            return Err(match host {
                Some(host) => format!(
                    "token endpoint redirected to `{host}`; redirects to another host are refused"
                ),
                None => "token endpoint returned a redirect; redirects are refused".to_string(),
            });
        }
        if let Some(code) = oauth_error_code(&response.body) {
            return Err(safe_oauth_error(&code));
        }
        return Err(format!("token endpoint returned HTTP {}", response.status));
    }
    parse_token_response(&response.body, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_allow_list_refuses_the_secret_and_unknown_keys() {
        let error = parse_token_form(r#"{"client_secret":"shh","code":"c"}"#).unwrap_err();
        assert!(error.contains("client_secret"), "{error}");

        let error = parse_token_form(r#"{"client_id":"mine","code":"c"}"#).unwrap_err();
        assert!(error.contains("client_id"), "{error}");

        let error = parse_token_form(r#"{"token":"stolen"}"#).unwrap_err();
        assert!(error.contains("not allowed"), "{error}");

        let form = parse_token_form(
            r#"{"grant_type":"authorization_code","code":"abc","code_verifier":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","redirect_uri":"https://app.test/cb","scope":"openid"}"#,
        )
        .unwrap();
        assert_eq!(form[0].0, "grant_type");
        assert!(form.iter().any(|(key, _)| key == "code_verifier"));
    }

    #[test]
    fn form_defaults_the_grant_type_and_refuses_others() {
        let form = parse_token_form(r#"{"code":"abc"}"#).unwrap();
        assert_eq!(
            form[0],
            ("grant_type".to_string(), "authorization_code".to_string())
        );

        let form = parse_token_form(r#"{"device_code":"dev"}"#).unwrap();
        assert_eq!(
            form[0],
            (
                "grant_type".to_string(),
                "urn:ietf:params:oauth:grant-type:device_code".to_string()
            )
        );

        let error = parse_token_form(r#"{"grant_type":"client_credentials"}"#).unwrap_err();
        assert!(error.contains("not allowed"), "{error}");
    }

    #[test]
    fn token_response_parses_the_standard_shape_and_never_echoes_the_body() {
        let tokens = parse_token_response(
            r#"{"access_token":"at","refresh_token":"rt","expires_in":3600}"#,
            100,
        )
        .unwrap();
        assert_eq!(tokens.access, "at");
        assert_eq!(tokens.refresh, "rt");
        assert_eq!(tokens.expires, 3700);
        // Debug is redacted.
        let debug = format!("{tokens:?}");
        assert!(!debug.contains("at"), "{debug}");
        assert!(!debug.contains("rt"), "{debug}");

        let error = parse_token_response(r#"{"error":"invalid_grant"}"#, 0).unwrap_err();
        assert!(error.contains("invalid_grant"), "{error}");

        // An unknown error code cannot smuggle text back to the guest.
        let error = parse_token_response(r#"{"error":"access_token=at-secret"}"#, 0).unwrap_err();
        assert!(!error.contains("at-secret"), "{error}");
    }

    #[test]
    fn endpoint_discipline_requires_exact_https() {
        assert!(granted_endpoint("https://id.acme.test/token").is_ok());
        assert!(granted_endpoint("http://id.acme.test/token").is_err());
        assert!(granted_endpoint("https://user@id.acme.test/token").is_err());
        assert!(granted_endpoint("https://id.acme.test/token#frag").is_err());
    }

    #[derive(Debug, Default)]
    struct PendingTransport {
        requests: std::sync::Mutex<u32>,
    }

    impl OAuthTransport for PendingTransport {
        fn post_form(
            &self,
            _url: &str,
            _form: &[(String, String)],
            _timeout: Duration,
        ) -> Result<HttpResponse, String> {
            *self.requests.lock().unwrap() += 1;
            Ok(HttpResponse {
                status: 400,
                location: None,
                body: r#"{"error":"authorization_pending"}"#.to_string(),
            })
        }
    }

    fn device_profile() -> OAuthProviderProfile {
        OAuthProviderProfile {
            id: "acme".to_string(),
            display_name: "Acme".to_string(),
            scopes: vec!["openid".to_string(), "profile".to_string()],
            flows: vec![OAuthFlow::DeviceCode, OAuthFlow::AuthorizationCode],
            authorization_endpoint: "https://id.acme.test/authorize".to_string(),
            token_endpoint: "https://id.acme.test/token".to_string(),
            device_authorization_endpoint: Some("https://id.acme.test/device".to_string()),
            revocation_endpoint: None,
            client_id: "client-123".to_string(),
            supports_pkce: true,
        }
    }

    #[test]
    fn device_authorization_uses_the_grant_and_host_budget() {
        #[derive(Debug, Default)]
        struct DeviceTransport {
            requests: std::sync::Mutex<Vec<Vec<(String, String)>>>,
        }
        impl OAuthTransport for DeviceTransport {
            fn post_form(
                &self,
                _url: &str,
                form: &[(String, String)],
                _timeout: Duration,
            ) -> Result<HttpResponse, String> {
                self.requests.lock().unwrap().push(form.to_vec());
                Ok(HttpResponse {
                    status: 200,
                    location: None,
                    body: r#"{"device_code":"dev","user_code":"USER-1","verification_uri":"https://id.acme.test/activate","interval":7,"expires_in":300}"#
                        .to_string(),
                })
            }
        }
        let transport = DeviceTransport::default();
        let mut budget = OAuthRequestBudget::new(60, 5);
        let authorization =
            request_device_authorization(&device_profile(), &mut budget, &transport, 1_000)
                .unwrap();
        assert_eq!(authorization.device_code, "dev");
        assert_eq!(authorization.user_code, "USER-1");
        assert_eq!(authorization.interval, 7);
        assert_eq!(authorization.expires_at, 1_300);
        assert_eq!(budget.used(), 1);

        // The client id comes from the grant; the guest never supplies it.
        let requests = transport.requests.lock().unwrap();
        assert!(requests[0]
            .iter()
            .any(|(key, value)| key == "client_id" && value == "client-123"));
        assert!(requests[0]
            .iter()
            .any(|(key, value)| key == "scope" && value == "openid profile"));
    }

    #[test]
    fn device_polling_stops_at_the_grant_request_budget() {
        let transport = PendingTransport::default();
        let authorization = DeviceAuthorization {
            device_code: "dev".to_string(),
            user_code: "USER-1".to_string(),
            verification_uri: "https://id.acme.test/activate".to_string(),
            // No sleep between polls: the bound under test is the request count.
            interval: 0,
            expires_at: 0,
        };
        let mut budget = OAuthRequestBudget::new(60, 2);
        let error = poll_device_token(
            &device_profile(),
            &authorization,
            &mut budget,
            &transport,
            0,
        )
        .unwrap_err();
        assert!(error.contains("budget exhausted"), "{error}");
        assert_eq!(*transport.requests.lock().unwrap(), 2);
        assert_eq!(budget.used(), 2);
    }

    #[test]
    fn device_authorization_refuses_a_profile_without_the_device_flow() {
        let mut profile = device_profile();
        profile.flows = vec![OAuthFlow::AuthorizationCode];
        let transport = PendingTransport::default();
        let mut budget = OAuthRequestBudget::new(60, 5);
        let error = request_device_authorization(&profile, &mut budget, &transport, 0).unwrap_err();
        assert!(error.contains("device-code"), "{error}");
    }

    #[derive(Debug, Default)]
    struct QueueTransport {
        responses: std::sync::Mutex<Vec<HttpResponse>>,
        requests: std::sync::Mutex<Vec<(String, Vec<(String, String)>)>>,
    }

    impl QueueTransport {
        fn with(responses: Vec<HttpResponse>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
                requests: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl OAuthTransport for QueueTransport {
        fn post_form(
            &self,
            url: &str,
            form: &[(String, String)],
            _timeout: Duration,
        ) -> Result<HttpResponse, String> {
            self.requests
                .lock()
                .unwrap()
                .push((url.to_string(), form.to_vec()));
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Err("no scripted response".to_string());
            }
            Ok(responses.remove(0))
        }
    }

    fn token_response(body: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            location: None,
            body: body.to_string(),
        }
    }

    #[test]
    fn authorization_code_exchange_uses_the_grant_and_parses_the_token() {
        let transport = QueueTransport::with(vec![token_response(
            r#"{"access_token":"at","refresh_token":"rt","expires_in":60}"#,
        )]);
        let mut budget = OAuthRequestBudget::new(60, 4);
        let tokens = exchange_authorization_code(
            &device_profile(),
            "the-code",
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "urn:ietf:wg:oauth:2.0:oob",
            &mut budget,
            &transport,
            10,
        )
        .unwrap();
        assert_eq!(tokens.access, "at");
        assert_eq!(tokens.refresh, "rt");
        assert_eq!(tokens.expires, 70);
        let requests = transport.requests.lock().unwrap();
        let form = &requests[0].1;
        assert!(form
            .iter()
            .any(|(key, value)| key == "client_id" && value == "client-123"));
        assert!(form
            .iter()
            .any(|(key, value)| key == "redirect_uri" && value == "urn:ietf:wg:oauth:2.0:oob"));
        assert!(form.iter().any(|(key, _)| key == "code_verifier"));
    }

    #[test]
    fn refresh_preserves_an_absent_refresh_token_for_the_caller() {
        let transport = QueueTransport::with(vec![token_response(
            r#"{"access_token":"at-2","expires_in":30}"#,
        )]);
        let mut budget = OAuthRequestBudget::new(60, 4);
        let tokens =
            refresh_stored_token(&device_profile(), "rt", &mut budget, &transport, 100).unwrap();
        assert_eq!(tokens.access, "at-2");
        assert!(tokens.refresh.is_empty(), "the caller keeps the old one");
        assert_eq!(tokens.expires, 130);
    }

    #[test]
    fn refresh_refuses_an_empty_refresh_token_without_a_request() {
        let transport = QueueTransport::default();
        let mut budget = OAuthRequestBudget::new(60, 4);
        let error =
            refresh_stored_token(&device_profile(), "", &mut budget, &transport, 0).unwrap_err();
        assert!(error.contains("no stored refresh token"), "{error}");
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn profile_fingerprint_binds_provider_client_and_endpoint_hosts() {
        let original = device_profile();
        let fingerprint = oauth_profile_fingerprint(&original).unwrap();
        assert_eq!(fingerprint, oauth_profile_fingerprint(&original).unwrap());

        let mut changed = original.clone();
        changed.id = "other".to_string();
        assert_ne!(fingerprint, oauth_profile_fingerprint(&changed).unwrap());
        let mut changed = original.clone();
        changed.client_id = "other-client".to_string();
        assert_ne!(fingerprint, oauth_profile_fingerprint(&changed).unwrap());
        let mut changed = original.clone();
        changed.token_endpoint = "https://id.other.test/token".to_string();
        assert_ne!(fingerprint, oauth_profile_fingerprint(&changed).unwrap());
        let mut changed = original;
        changed.token_endpoint = "https://id.acme.test/another-token-path".to_string();
        assert_eq!(
            fingerprint,
            oauth_profile_fingerprint(&changed).unwrap(),
            "a path-only grant edit does not change the token's issuer"
        );
    }

    #[test]
    fn revoke_prefers_the_refresh_token_and_requires_a_2xx() {
        let mut profile = device_profile();
        profile.revocation_endpoint = Some("https://id.acme.test/revoke".to_string());
        let token = StoredOAuthToken {
            access: "at".to_string(),
            refresh: "rt".to_string(),
            expires: 0,
            profile_fingerprint: Some(oauth_profile_fingerprint(&profile).unwrap()),
        };
        let transport = QueueTransport::with(vec![HttpResponse {
            status: 200,
            location: None,
            body: String::new(),
        }]);
        let mut budget = OAuthRequestBudget::new(60, 4);
        revoke_stored_token(&profile, &token, &mut budget, &transport).unwrap();
        let requests = transport.requests.lock().unwrap();
        assert!(requests[0]
            .1
            .iter()
            .any(|(key, value)| key == "token" && value == "rt"));
        assert!(requests[0]
            .1
            .iter()
            .any(|(key, value)| key == "token_type_hint" && value == "refresh_token"));

        let transport = QueueTransport::with(vec![HttpResponse {
            status: 500,
            location: None,
            body: r#"{"error":"invalid_token"}"#.to_string(),
        }]);
        let mut budget = OAuthRequestBudget::new(60, 4);
        let error = revoke_stored_token(&profile, &token, &mut budget, &transport).unwrap_err();
        assert!(error.contains("HTTP 500"), "{error}");
    }

    #[test]
    fn revoke_refuses_a_changed_profile_before_the_network() {
        let original = device_profile();
        let mut changed = original.clone();
        changed.token_endpoint = "https://id.other.test/token".to_string();
        changed.revocation_endpoint = Some("https://id.other.test/revoke".to_string());
        let token = StoredOAuthToken {
            access: "at-secret".to_string(),
            refresh: "rt-secret".to_string(),
            expires: 0,
            profile_fingerprint: Some(oauth_profile_fingerprint(&original).unwrap()),
        };
        let transport = QueueTransport::default();
        let mut budget = OAuthRequestBudget::new(60, 4);
        let error = revoke_stored_token(&changed, &token, &mut budget, &transport).unwrap_err();
        assert!(error.contains("different issuing profile"), "{error}");
        assert!(!error.contains("at-secret"));
        assert!(!error.contains("rt-secret"));
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn revoke_without_a_revocation_endpoint_says_so() {
        let token = StoredOAuthToken {
            access: "at".to_string(),
            refresh: String::new(),
            expires: 0,
            profile_fingerprint: Some(oauth_profile_fingerprint(&device_profile()).unwrap()),
        };
        let transport = QueueTransport::default();
        let mut budget = OAuthRequestBudget::new(60, 4);
        let error =
            revoke_stored_token(&device_profile(), &token, &mut budget, &transport).unwrap_err();
        assert!(error.contains("no revocation-endpoint"), "{error}");
    }
}
