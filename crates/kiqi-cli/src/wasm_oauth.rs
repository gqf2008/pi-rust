//! `ki auth` for wasm OAuth providers: discovery and the host-side flow driver.
//!
//! The ABI splits the work so that **no token crosses the WASM boundary**:
//!
//! * the guest is the flow controller (`oauth-api.action`), but it only ever
//!   sees a PKCE `code_challenge` and returns an interaction/status;
//! * the host owns the IdP endpoints (from the grant), performs every HTTP
//!   request, parses the standard token response and writes the credential to
//!   `extension:<install-id>:<provider-id>`;
//! * device authorization and polling are entirely host-side, so the device
//!   code never enters the sandbox;
//! * `ki auth refresh` and `ki auth logout` never call a guest at all.
//!
//! Deny-by-default is enforced in two places: [`find_provider`] only sees a
//! provider a `grants.json` names, and [`kiqi_host::wasm::WasmExtension::oauth_action`]
//! refuses before touching the guest when the grant is absent.

use std::path::{Path, PathBuf};

use kiqi_host::grants::{
    extension_dir_at, installed_extensions_at, Grants, OAuthFlow, OAuthGrant, OAuthProviderProfile,
};
use kiqi_host::wasm::{
    ensure_oauth_profile_matches, exchange_authorization_code, oauth_profile_fingerprint,
    poll_device_token, refresh_stored_token, request_device_authorization, revoke_stored_token,
    OAuthRequestBudget, OAuthTransport, StoredOAuthToken,
};
use kiqi_host::{OAuthAction, OAuthResponse};

use crate::config::Credential;

/// The redirect URI the authorization-code flow uses.
///
/// The grant profile deliberately carries no redirect URI (it is the client's
/// registration detail, not the IdP's address), and `ki auth` has no loopback
/// callback, so the flow is out-of-band: the user pastes the code back. The
/// same value goes into both the authorization URL and the token exchange.
pub const OOB_REDIRECT_URI: &str = "urn:ietf:wg:oauth:2.0:oob";

/// One installed extension that the grants let drive `provider_id`.
#[derive(Debug, Clone)]
pub struct FoundProvider {
    pub install_id: String,
    pub profile: OAuthProviderProfile,
    /// The whole `oauth` policy: it carries the flow budget the driver needs.
    pub grant: OAuthGrant,
    /// The extension's full authorization, read once at discovery. Loading from
    /// this (rather than re-reading the file) keeps the bytes and the grants
    /// the same snapshot.
    pub grants: Grants,
    pub wasm: PathBuf,
}

/// The guest side of one flow step. Implemented by the wasm wrapper and by the
/// tests' fakes (so the driver is testable without a component).
pub trait OAuthGuest {
    fn action(&self, provider_id: &str, action: OAuthAction) -> Result<OAuthResponse, String>;
}

impl OAuthGuest for kiqi_host::wasm::WasmExtension {
    fn action(&self, provider_id: &str, action: OAuthAction) -> Result<OAuthResponse, String> {
        <kiqi_host::wasm::WasmExtension as kiqi_host::Extension>::oauth_action(
            self,
            provider_id,
            action,
        )
        .map_err(|error| error.to_string())
    }
}

/// Where the driver shows instructions and asks for one pasted code.
pub trait OAuthUi {
    fn say(&mut self, text: &str);
    fn ask(&mut self, prompt: &str) -> Result<String, String>;
}

/// Discover which installed extension a grant lets drive `provider_id`.
///
/// Two rules matter:
/// * a provider granted by more than one extension is ambiguous, so it is an
///   error with the candidate list rather than a silent first match;
/// * an extension whose digest sidecar disagrees is refused before its bytes run.
pub fn find_provider(provider_id: &str) -> Result<Option<FoundProvider>, String> {
    let root = kiqi_host::grants::agent_dir()
        .ok_or_else(|| "cannot resolve the agent directory".to_string())?;
    find_provider_at(&root, provider_id)
}

/// [`find_provider`] against an explicit agent root (the test seam).
pub fn find_provider_at(root: &Path, provider_id: &str) -> Result<Option<FoundProvider>, String> {
    let mut found = Vec::new();
    for install_id in installed_extensions_at(root) {
        let grants = Grants::load_for_at(root, &install_id);
        let Some(oauth) = grants.oauth.clone() else {
            continue;
        };
        let Some(profile) = oauth
            .providers
            .iter()
            .find(|profile| profile.id == provider_id)
            .cloned()
        else {
            continue;
        };
        let wasm = extension_dir_at(root, &install_id).join("extension.wasm");
        if !wasm.is_file() {
            continue;
        }
        // The same gate the session loader uses: a mismatched sidecar means the
        // bytes on disk are not the ones the user authorized.
        if let Err(error) = crate::extensions::digest_gate(&wasm) {
            return Err(format!("extension `{install_id}`: {error}"));
        }
        found.push(FoundProvider {
            install_id,
            profile,
            grant: oauth,
            grants,
            wasm,
        });
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => Err(format!(
            "oauth provider `{provider_id}` is granted by more than one extension: {}",
            found
                .iter()
                .map(|entry| entry.install_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Load the extension a [`FoundProvider`] names, with the namespaced credential
/// sink installed (the guest-facing `oauth-http` path has one route to disk).
pub fn load_guest(found: &FoundProvider) -> Result<kiqi_host::wasm::WasmExtension, String> {
    let options = kiqi_host::wasm::Options {
        credential_sink: Some(std::sync::Arc::new(crate::extensions::ConfigCredentialSink)),
        ..Default::default()
    };
    kiqi_host::wasm::WasmExtension::load_with_options(
        &found.install_id,
        &found.wasm,
        found.grants.clone(),
        options,
    )
    .map_err(|error| error.to_string())
}

/// Run `ki auth login` against a wasm provider.
///
/// The guest is called for `begin` (deny-by-default happens inside the host
/// wrapper); everything after that is host-attributed. The returned value is
/// the stored credential's expiry (`0` = no known expiry).
pub fn login(
    install_id: &str,
    profile: &OAuthProviderProfile,
    grant: &OAuthGrant,
    guest: &dyn OAuthGuest,
    transport: &dyn OAuthTransport,
    now: i64,
    ui: &mut dyn OAuthUi,
) -> Result<i64, String> {
    let flow = select_flow(profile)?;
    let mut budget = OAuthRequestBudget::for_grant(grant);
    let (challenge, verifier) = match flow {
        OAuthFlow::AuthorizationCode => {
            let verifier = generate_pkce_verifier();
            let challenge = pkce_challenge(&verifier);
            (challenge, Some(verifier))
        }
        OAuthFlow::DeviceCode => (String::new(), None),
    };

    match guest.action(&profile.id, OAuthAction::Begin(challenge.clone()))? {
        OAuthResponse::Interaction(interaction) => {
            // Provider-specific guidance from the guest is fine; its `url` is
            // explicitly ignored. The host builds every URL.
            if !interaction.title.is_empty() {
                ui.say(&interaction.title);
            }
            for instruction in &interaction.instructions {
                ui.say(instruction);
            }
        }
        OAuthResponse::Denied(message) | OAuthResponse::Error(message) => return Err(message),
        // A guest that says it already stored a credential is trusted only for
        // the status it may report; it has no way to write one without the
        // host's sink.
        OAuthResponse::Stored(status) => return Ok(status.expires_at),
        OAuthResponse::AuthorizationPending | OAuthResponse::SlowDown => {}
    }

    let tokens = match flow {
        OAuthFlow::DeviceCode => {
            let authorization = request_device_authorization(profile, &mut budget, transport, now)?;
            ui.say(&format!(
                "Open {} and enter code {}",
                authorization.verification_uri, authorization.user_code
            ));
            poll_device_token(profile, &authorization, &mut budget, transport, now)?
        }
        OAuthFlow::AuthorizationCode => {
            let url = build_authorization_url(profile, &challenge, verifier.is_some())?;
            ui.say("Open this URL in a browser:");
            ui.say(&url);
            let code = ui.ask("Paste the authorization code: ")?;
            let code = code.trim();
            if code.is_empty() {
                return Err("empty authorization code; nothing saved".to_string());
            }
            exchange_authorization_code(
                profile,
                code,
                verifier.as_deref(),
                OOB_REDIRECT_URI,
                &mut budget,
                transport,
                now,
            )?
        }
    };

    let refresh = if tokens.refresh.is_empty() {
        String::new()
    } else {
        tokens.refresh.clone()
    };
    crate::config::upsert_extension_oauth_token(
        install_id,
        &profile.id,
        tokens.access,
        refresh,
        tokens.expires,
        oauth_profile_fingerprint(profile)?,
    )
    .map_err(|error| format!("could not save OAuth credentials: {error}"))?;
    Ok(tokens.expires)
}

/// Run `ki auth refresh` against a wasm provider.
///
/// **Host-only**: the stored refresh token goes from `auth.json` straight to the
/// IdP; no guest is instantiated or called. A response without a
/// `refresh_token` keeps the existing one.
pub fn refresh(
    install_id: &str,
    profile: &OAuthProviderProfile,
    grant: &OAuthGrant,
    transport: &dyn OAuthTransport,
    now: i64,
) -> Result<i64, String> {
    let stored = read_stored(install_id, &profile.id)?
        .ok_or_else(|| format!("no stored OAuth credentials for `{}`", profile.id))?;
    ensure_oauth_profile_matches(profile, &stored)?;
    let mut budget = OAuthRequestBudget::for_grant(grant);
    let tokens = refresh_stored_token(profile, &stored.refresh, &mut budget, transport, now)?;
    let refresh = if tokens.refresh.is_empty() {
        stored.refresh
    } else {
        tokens.refresh
    };
    crate::config::upsert_extension_oauth_token(
        install_id,
        &profile.id,
        tokens.access,
        refresh,
        tokens.expires,
        oauth_profile_fingerprint(profile)?,
    )
    .map_err(|error| format!("could not save refreshed OAuth credentials: {error}"))?;
    Ok(tokens.expires)
}

/// Run `ki auth logout` against a wasm provider.
///
/// **Host-only**: the token is read from the namespaced key and revoked at the
/// IdP; only then is that key deleted. A failed revocation keeps the local
/// credential (the same discipline the cdylib path uses), and a bare
/// `auth.json` entry is never touched. If the grant's issuing profile changed,
/// remote revocation is refused; only an explicit `--force-local` confirmation
/// may delete the local entry, and that path makes no request.
pub fn logout(
    install_id: &str,
    profile: &OAuthProviderProfile,
    grant: &OAuthGrant,
    transport: &dyn OAuthTransport,
    mismatched_profile_local_delete_confirmed: bool,
) -> Result<bool, String> {
    let Some(stored) = read_stored(install_id, &profile.id)? else {
        return Ok(false);
    };
    if let Err(error) = ensure_oauth_profile_matches(profile, &stored) {
        if !mismatched_profile_local_delete_confirmed {
            return Err(format!(
                "{error}; local credentials were kept; rerun `ki auth logout --provider {} --force-local` to delete locally without remote revocation",
                profile.id
            ));
        }
        return crate::config::delete_extension_oauth_token(install_id, &profile.id)
            .map_err(|delete_error| format!("could not remove credentials: {delete_error}"));
    }
    if profile.revocation_endpoint.is_some() {
        let mut budget = OAuthRequestBudget::for_grant(grant);
        revoke_stored_token(profile, &stored, &mut budget, transport)
            .map_err(|error| format!("revoke failed; local credentials were kept: {error}"))?;
    }
    crate::config::delete_extension_oauth_token(install_id, &profile.id)
        .map_err(|error| format!("could not remove credentials: {error}"))
}

/// Read the namespaced credential for one extension provider.
fn read_stored(install_id: &str, provider_id: &str) -> Result<Option<StoredOAuthToken>, String> {
    match crate::config::read_extension_oauth_token(install_id, provider_id)
        .map_err(|error| format!("could not read credentials: {error}"))?
    {
        Some(Credential::Oauth {
            access,
            refresh,
            expires,
            profile_fingerprint,
        }) => Ok(Some(StoredOAuthToken {
            access,
            refresh,
            expires,
            profile_fingerprint,
        })),
        Some(_) => Err(format!(
            "the entry at `extension:{install_id}:{provider_id}` is not an OAuth credential"
        )),
        None => Ok(None),
    }
}

fn select_flow(profile: &OAuthProviderProfile) -> Result<OAuthFlow, String> {
    // Device-code needs no browser callback, so it is the better default when
    // the profile grants both.
    if profile.flows.contains(&OAuthFlow::DeviceCode) {
        Ok(OAuthFlow::DeviceCode)
    } else if profile.flows.contains(&OAuthFlow::AuthorizationCode) {
        Ok(OAuthFlow::AuthorizationCode)
    } else {
        Err(format!(
            "oauth provider `{}` grants no flow this host can drive",
            profile.id
        ))
    }
}

fn generate_pkce_verifier() -> String {
    use base64::Engine as _;
    // 32 random bytes → exactly 43 base64url characters, the RFC 7636 minimum.
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn pkce_challenge(verifier: &str) -> String {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn build_authorization_url(
    profile: &OAuthProviderProfile,
    challenge: &str,
    use_pkce: bool,
) -> Result<String, String> {
    let mut url = reqwest::Url::parse(&profile.authorization_endpoint)
        .map_err(|error| format!("invalid authorization endpoint: {error}"))?;
    if url.scheme() != "https" {
        return Err(format!(
            "authorization endpoint must be https, got `{}`",
            profile.authorization_endpoint
        ));
    }
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", &profile.client_id);
        query.append_pair("redirect_uri", OOB_REDIRECT_URI);
        if !profile.scopes.is_empty() {
            query.append_pair("scope", &profile.scopes.join(" "));
        }
        if use_pkce {
            query.append_pair("code_challenge", challenge);
            query.append_pair("code_challenge_method", "S256");
        }
    }
    Ok(url.to_string())
}

/// Unix epoch seconds, saturating at 0 for a clock before 1970.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    // -----------------------------------------------------------------------
    // A fake IdP: a real local HTTP server plus a transport that routes the
    // grant's (https) URLs to it. Production still refuses a non-https grant;
    // the tests construct the grant in memory, bypassing that check only.
    // -----------------------------------------------------------------------

    #[derive(Debug, Clone)]
    struct Recorded {
        path: String,
        body: String,
    }

    struct ScriptedServer {
        addr: SocketAddr,
        requests: Arc<Mutex<Vec<Recorded>>>,
        _handle: std::thread::JoinHandle<()>,
    }

    fn encode(value: &str) -> String {
        let mut out = String::new();
        for byte in value.bytes() {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                out.push(byte as char);
            } else {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
        out
    }

    fn scripted_server(script: Vec<(u16, &'static str)>) -> ScriptedServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake IdP");
        let addr = listener.local_addr().expect("local addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let handle = std::thread::spawn(move || {
            for (status, body) in script {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let Ok(clone) = stream.try_clone() else {
                    return;
                };
                let mut reader = BufReader::new(clone);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    return;
                }
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = value.trim().parse().unwrap_or(0);
                    }
                }
                let mut bytes = vec![0u8; content_length];
                if content_length > 0 {
                    let _ = reader.read_exact(&mut bytes);
                }
                recorded.lock().unwrap().push(Recorded {
                    path,
                    body: String::from_utf8_lossy(&bytes).into_owned(),
                });
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        ScriptedServer {
            addr,
            requests,
            _handle: handle,
        }
    }

    /// Route the grant's URLs to the local server. Only the path matters: the
    /// grant's `https` host is a policy identity, not a routing hint.
    #[derive(Debug)]
    struct LocalTransport {
        addr: SocketAddr,
    }

    impl OAuthTransport for LocalTransport {
        fn post_form(
            &self,
            url: &str,
            form: &[(String, String)],
            _timeout: std::time::Duration,
        ) -> Result<kiqi_host::wasm::HttpResponse, String> {
            let parsed = reqwest::Url::parse(url).map_err(|error| error.to_string())?;
            let path = parsed.path().to_string();
            let body = form
                .iter()
                .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
                .collect::<Vec<_>>()
                .join("&");
            let mut stream = TcpStream::connect(self.addr).map_err(|error| error.to_string())?;
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .ok();
            write!(
                stream,
                "POST {path} HTTP/1.1\r\nHost: {}\r\n\
                 Content-Type: application/x-www-form-urlencoded\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                self.addr,
                body.len()
            )
            .map_err(|error| error.to_string())?;
            let mut response = Vec::new();
            stream
                .read_to_end(&mut response)
                .map_err(|error| error.to_string())?;
            let text = String::from_utf8_lossy(&response);
            let (head, body) = text
                .split_once("\r\n\r\n")
                .ok_or_else(|| "malformed HTTP response".to_string())?;
            let status = head
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|code| code.parse().ok())
                .ok_or_else(|| "missing HTTP status".to_string())?;
            let location = head.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("location")
                    .then(|| value.trim().to_string())
            });
            Ok(kiqi_host::wasm::HttpResponse {
                status,
                location,
                body: body.to_string(),
            })
        }
    }

    // -----------------------------------------------------------------------
    // Fakes for the guest and the terminal
    // -----------------------------------------------------------------------

    #[derive(Default)]
    struct FakeGuest {
        calls: Mutex<Vec<String>>,
        begin: Mutex<Option<OAuthResponse>>,
    }

    impl FakeGuest {
        fn returning(begin: OAuthResponse) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                begin: Mutex::new(Some(begin)),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl OAuthGuest for FakeGuest {
        fn action(&self, provider_id: &str, action: OAuthAction) -> Result<OAuthResponse, String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{provider_id}:{action:?}"));
            match action {
                OAuthAction::Begin(_) => self
                    .begin
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or_else(|| "no scripted begin response".to_string()),
                _ => Ok(OAuthResponse::Error("unexpected action".to_string())),
            }
        }
    }

    #[derive(Default)]
    struct FakeUi {
        out: Vec<String>,
        answer: Option<String>,
    }

    impl OAuthUi for FakeUi {
        fn say(&mut self, text: &str) {
            self.out.push(text.to_string());
        }

        fn ask(&mut self, prompt: &str) -> Result<String, String> {
            self.out.push(format!("ASK:{prompt}"));
            self.answer
                .clone()
                .ok_or_else(|| "no scripted answer".to_string())
        }
    }

    struct TempAgent {
        _guard: crate::env_lock::EnvGuard,
        _tmp: tempfile::TempDir,
        prev: Option<std::ffi::OsString>,
    }

    impl TempAgent {
        fn new() -> Self {
            let guard = crate::config::test_support::env_lock().lock();
            let prev = std::env::var_os(crate::config::CONFIG_DIR_ENV);
            let tmp = tempfile::TempDir::new().expect("tempdir");
            std::env::set_var(crate::config::CONFIG_DIR_ENV, tmp.path());
            Self {
                _guard: guard,
                _tmp: tmp,
                prev,
            }
        }
    }

    impl Drop for TempAgent {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
            }
        }
    }

    fn profile(port: u16, flows: Vec<OAuthFlow>) -> OAuthProviderProfile {
        OAuthProviderProfile {
            id: "acme".to_string(),
            display_name: "Acme".to_string(),
            scopes: vec!["openid".to_string()],
            flows,
            authorization_endpoint: format!("https://127.0.0.1:{port}/authorize"),
            token_endpoint: format!("https://127.0.0.1:{port}/token"),
            device_authorization_endpoint: Some(format!("https://127.0.0.1:{port}/device")),
            revocation_endpoint: Some(format!("https://127.0.0.1:{port}/revoke")),
            client_id: "client-123".to_string(),
            supports_pkce: true,
        }
    }

    fn grant(profile: OAuthProviderProfile) -> OAuthGrant {
        OAuthGrant {
            providers: vec![profile],
            timeout_seconds: 30,
            max_requests: 10,
        }
    }

    fn stored(install_id: &str, provider_id: &str) -> Option<(String, String, i64)> {
        match crate::config::read_extension_oauth_token(install_id, provider_id).unwrap() {
            Some(Credential::Oauth {
                access,
                refresh,
                expires,
                ..
            }) => Some((access, refresh, expires)),
            _ => None,
        }
    }

    fn bare_key(provider_id: &str) -> Option<Credential> {
        crate::config::read_auth()
            .unwrap()
            .get(provider_id)
            .cloned()
    }

    // -----------------------------------------------------------------------
    // Login
    // -----------------------------------------------------------------------

    #[test]
    fn device_login_stores_the_namespaced_token_and_ignores_the_guest_url() {
        let _agent = TempAgent::new();
        // A bare credential the extension must never clobber.
        crate::config::upsert_credential(
            "anthropic",
            Credential::ApiKey {
                key: Some("sk-bare".to_string()),
                env: None,
            },
        )
        .unwrap();

        let server = scripted_server(vec![
            (
                200,
                r#"{"device_code":"dev","user_code":"USER-1","verification_uri":"https://id.acme.test/activate","interval":1,"expires_in":600}"#,
            ),
            (
                200,
                r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":3600}"#,
            ),
        ]);
        let profile = profile(server.addr.port(), vec![OAuthFlow::DeviceCode]);
        let grant = grant(profile.clone());
        let guest = FakeGuest::returning(OAuthResponse::Interaction(kiqi_host::OAuthInteraction {
            title: "Conformance".to_string(),
            url: Some("https://guest.example/never".to_string()),
            instructions: vec!["the host builds the URL".to_string()],
            user_code: None,
        }));
        let transport = LocalTransport { addr: server.addr };
        let mut ui = FakeUi::default();

        let expires = login(
            "install-1",
            &profile,
            &grant,
            &guest,
            &transport,
            1_000,
            &mut ui,
        )
        .expect("device login");
        assert_eq!(expires, 4_600);
        assert_eq!(
            stored("install-1", "acme"),
            Some(("at-1".to_string(), "rt-1".to_string(), 4_600))
        );
        assert_eq!(
            guest.calls(),
            vec!["acme:Begin(\"\")".to_string()],
            "the guest sees begin only"
        );

        // The host-parsed verification URI/code are shown; the guest's URL is not.
        let shown = ui.out.join("\n");
        assert!(shown.contains("https://id.acme.test/activate"), "{shown}");
        assert!(shown.contains("USER-1"), "{shown}");
        assert!(!shown.contains("guest.example"), "{shown}");

        // The bare credential is untouched.
        assert!(matches!(
            bare_key("anthropic"),
            Some(Credential::ApiKey { key: Some(key), .. }) if key == "sk-bare"
        ));
        assert!(bare_key("acme").is_none(), "never a bare provider id");
    }

    #[test]
    fn authorization_code_login_builds_the_url_and_exchanges_the_code() {
        let _agent = TempAgent::new();
        let server = scripted_server(vec![(
            200,
            r#"{"access_token":"at-code","refresh_token":"rt-code","expires_in":120}"#,
        )]);
        let profile = profile(server.addr.port(), vec![OAuthFlow::AuthorizationCode]);
        let grant = grant(profile.clone());
        let guest = FakeGuest::returning(OAuthResponse::Interaction(kiqi_host::OAuthInteraction {
            title: String::new(),
            url: Some("https://guest.example/never".to_string()),
            instructions: Vec::new(),
            user_code: None,
        }));
        let transport = LocalTransport { addr: server.addr };
        let mut ui = FakeUi {
            answer: Some("the-code".to_string()),
            ..FakeUi::default()
        };

        let expires = login(
            "install-1",
            &profile,
            &grant,
            &guest,
            &transport,
            10,
            &mut ui,
        )
        .expect("code login");
        assert_eq!(expires, 130);
        assert_eq!(
            stored("install-1", "acme"),
            Some(("at-code".to_string(), "rt-code".to_string(), 130))
        );

        // The URL is host-built: client id, redirect uri and PKCE challenge.
        let shown = ui.out.join("\n");
        assert!(shown.contains("client_id=client-123"), "{shown}");
        assert!(shown.contains("code_challenge="), "{shown}");
        assert!(shown.contains("code_challenge_method=S256"), "{shown}");
        assert!(
            shown.contains("redirect_uri=urn%3Aietf%3Awg%3Aoauth%3A2.0%3Aoob"),
            "{shown}"
        );
        assert!(!shown.contains("guest.example"), "{shown}");

        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/token");
        for expected in [
            "grant_type=authorization_code",
            "code=the-code",
            "code_verifier=",
            "redirect_uri=urn%3Aietf%3Awg%3Aoauth%3A2.0%3Aoob",
            "client_id=client-123",
        ] {
            assert!(
                requests[0].body.contains(expected),
                "{:?}",
                requests[0].body
            );
        }
    }

    // -----------------------------------------------------------------------
    // Refresh / logout (host-only: no guest argument exists)
    // -----------------------------------------------------------------------

    #[test]
    fn refresh_is_host_only_and_preserves_an_absent_refresh_token() {
        let _agent = TempAgent::new();
        let server = scripted_server(vec![(200, r#"{"access_token":"at-2","expires_in":60}"#)]);
        let profile = profile(server.addr.port(), vec![OAuthFlow::DeviceCode]);
        let grant = grant(profile.clone());
        let transport = LocalTransport { addr: server.addr };
        crate::config::upsert_extension_oauth_token(
            "install-1",
            "acme",
            "at-1".to_string(),
            "rt-1".to_string(),
            0,
            oauth_profile_fingerprint(&profile).unwrap(),
        )
        .unwrap();

        let expires = refresh("install-1", &profile, &grant, &transport, 100).expect("refresh");
        assert_eq!(expires, 160);
        assert_eq!(
            stored("install-1", "acme"),
            Some(("at-2".to_string(), "rt-1".to_string(), 160)),
            "the old refresh token is kept"
        );

        let requests = server.requests.lock().unwrap();
        assert!(requests[0].body.contains("grant_type=refresh_token"));
        assert!(requests[0].body.contains("refresh_token=rt-1"));
        assert!(requests[0].body.contains("client_id=client-123"));
    }

    #[test]
    fn refresh_refuses_a_changed_profile_without_sending_the_old_token() {
        let _agent = TempAgent::new();
        let original = profile(1, vec![OAuthFlow::DeviceCode]);
        crate::config::upsert_extension_oauth_token(
            "install-1",
            "acme",
            "at-secret".to_string(),
            "rt-secret".to_string(),
            0,
            oauth_profile_fingerprint(&original).unwrap(),
        )
        .unwrap();
        let server = scripted_server(vec![(200, r#"{"access_token":"at-new"}"#)]);
        let changed = profile(server.addr.port(), vec![OAuthFlow::DeviceCode]);
        let grant = grant(changed.clone());
        let transport = LocalTransport { addr: server.addr };

        let error = refresh("install-1", &changed, &grant, &transport, 100).unwrap_err();
        assert!(error.contains("different issuing profile"), "{error}");
        assert!(!error.contains("at-secret"));
        assert!(!error.contains("rt-secret"));
        assert!(server.requests.lock().unwrap().is_empty());
        assert_eq!(
            stored("install-1", "acme"),
            Some(("at-secret".to_string(), "rt-secret".to_string(), 0))
        );
    }

    #[test]
    fn logout_profile_mismatch_requires_explicit_local_delete_confirmation() {
        let _agent = TempAgent::new();
        let original = profile(1, vec![OAuthFlow::DeviceCode]);
        crate::config::upsert_extension_oauth_token(
            "install-1",
            "acme",
            "at-secret".to_string(),
            "rt-secret".to_string(),
            0,
            oauth_profile_fingerprint(&original).unwrap(),
        )
        .unwrap();
        let server = scripted_server(vec![(200, "")]);
        let changed = profile(server.addr.port(), vec![OAuthFlow::DeviceCode]);
        let grant = grant(changed.clone());
        let transport = LocalTransport { addr: server.addr };

        let error = logout("install-1", &changed, &grant, &transport, false).unwrap_err();
        assert!(error.contains("--force-local"), "{error}");
        assert!(!error.contains("at-secret"));
        assert!(!error.contains("rt-secret"));
        assert!(server.requests.lock().unwrap().is_empty());
        assert!(stored("install-1", "acme").is_some());

        assert!(logout("install-1", &changed, &grant, &transport, true).unwrap());
        assert!(stored("install-1", "acme").is_none());
        assert!(
            server.requests.lock().unwrap().is_empty(),
            "the confirmed local path must not replay the token either"
        );
    }

    #[test]
    fn logout_revokes_then_deletes_only_the_namespaced_key() {
        let _agent = TempAgent::new();
        crate::config::upsert_credential(
            "anthropic",
            Credential::ApiKey {
                key: Some("sk-bare".to_string()),
                env: None,
            },
        )
        .unwrap();
        let server = scripted_server(vec![(200, "")]);
        let profile = profile(server.addr.port(), vec![OAuthFlow::DeviceCode]);
        let grant = grant(profile.clone());
        let transport = LocalTransport { addr: server.addr };
        crate::config::upsert_extension_oauth_token(
            "install-1",
            "acme",
            "at-1".to_string(),
            "rt-1".to_string(),
            0,
            oauth_profile_fingerprint(&profile).unwrap(),
        )
        .unwrap();

        assert!(logout("install-1", &profile, &grant, &transport, false).expect("logout"));
        assert!(stored("install-1", "acme").is_none());
        assert!(matches!(
            bare_key("anthropic"),
            Some(Credential::ApiKey { key: Some(key), .. }) if key == "sk-bare"
        ));

        let requests = server.requests.lock().unwrap();
        assert_eq!(requests[0].path, "/revoke");
        assert!(
            requests[0].body.contains("token=rt-1"),
            "{:?}",
            requests[0].body
        );
        assert!(requests[0].body.contains("token_type_hint=refresh_token"));
    }

    #[test]
    fn logout_without_a_stored_token_is_a_noop() {
        let _agent = TempAgent::new();
        let profile = profile(1, vec![OAuthFlow::DeviceCode]);
        let grant = grant(profile.clone());
        // No request must be made: a server that never accepts is fine.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let transport = LocalTransport {
            addr: listener.local_addr().unwrap(),
        };
        assert!(!logout("install-1", &profile, &grant, &transport, false).unwrap());
    }

    // -----------------------------------------------------------------------
    // Re-login and isolation
    // -----------------------------------------------------------------------

    #[test]
    fn relogin_overwrites_the_namespaced_key_and_leaves_a_bare_key_alone() {
        let _agent = TempAgent::new();
        crate::config::upsert_credential(
            "anthropic",
            Credential::ApiKey {
                key: Some("sk-bare".to_string()),
                env: None,
            },
        )
        .unwrap();

        let first = scripted_server(vec![(
            200,
            r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":60}"#,
        )]);
        let first_profile = profile(first.addr.port(), vec![OAuthFlow::AuthorizationCode]);
        let first_grant = grant(first_profile.clone());
        let guest = FakeGuest::returning(OAuthResponse::Interaction(kiqi_host::OAuthInteraction {
            title: String::new(),
            url: None,
            instructions: Vec::new(),
            user_code: None,
        }));
        login(
            "install-1",
            &first_profile,
            &first_grant,
            &guest,
            &LocalTransport { addr: first.addr },
            0,
            &mut FakeUi {
                answer: Some("code-1".to_string()),
                ..FakeUi::default()
            },
        )
        .expect("first login");

        let second = scripted_server(vec![(
            200,
            r#"{"access_token":"at-2","refresh_token":"rt-2","expires_in":60}"#,
        )]);
        let second_profile = profile(second.addr.port(), vec![OAuthFlow::AuthorizationCode]);
        let second_grant = grant(second_profile.clone());
        login(
            "install-1",
            &second_profile,
            &second_grant,
            &guest,
            &LocalTransport { addr: second.addr },
            0,
            &mut FakeUi {
                answer: Some("code-2".to_string()),
                ..FakeUi::default()
            },
        )
        .expect("re-login");

        assert_eq!(
            stored("install-1", "acme"),
            Some(("at-2".to_string(), "rt-2".to_string(), 60))
        );
        assert!(matches!(
            bare_key("anthropic"),
            Some(Credential::ApiKey { key: Some(key), .. }) if key == "sk-bare"
        ));
    }

    // -----------------------------------------------------------------------
    // Discovery
    // -----------------------------------------------------------------------

    fn grants_file(root: &Path, install_id: &str, provider_id: &str) {
        let dir = extension_dir_at(root, install_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("extension.wasm"), b"\0asm").unwrap();
        let mut grants = Grants::default();
        grants
            .grant_oauth(OAuthGrant {
                providers: vec![OAuthProviderProfile {
                    id: provider_id.to_string(),
                    display_name: "Acme".to_string(),
                    scopes: Vec::new(),
                    flows: vec![OAuthFlow::DeviceCode],
                    authorization_endpoint: "https://id.acme.test/authorize".to_string(),
                    token_endpoint: "https://id.acme.test/token".to_string(),
                    device_authorization_endpoint: Some("https://id.acme.test/device".to_string()),
                    revocation_endpoint: None,
                    client_id: "client-123".to_string(),
                    supports_pkce: true,
                }],
                timeout_seconds: 30,
                max_requests: 5,
            })
            .unwrap();
        Grants::save_for_at(root, install_id, &grants).unwrap();
    }

    #[test]
    fn find_provider_reads_the_grant_and_ignores_ungranted_extensions() {
        let root = tempfile::tempdir().unwrap();
        grants_file(root.path(), "with-oauth", "acme");
        // An extension directory with a wasm file but no oauth grant.
        let other = extension_dir_at(root.path(), "no-oauth");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("extension.wasm"), b"\0asm").unwrap();

        let found = find_provider_at(root.path(), "acme")
            .expect("discovery")
            .expect("found");
        assert_eq!(found.install_id, "with-oauth");
        assert_eq!(found.profile.id, "acme");
        assert_eq!(found.grant.providers[0].id, "acme");

        assert!(find_provider_at(root.path(), "missing").unwrap().is_none());
    }

    #[test]
    fn find_provider_refuses_two_extensions_granting_the_same_id() {
        let root = tempfile::tempdir().unwrap();
        grants_file(root.path(), "one", "acme");
        grants_file(root.path(), "two", "acme");
        let error = find_provider_at(root.path(), "acme").unwrap_err();
        assert!(error.contains("more than one extension"), "{error}");
        assert!(error.contains("one"), "{error}");
        assert!(error.contains("two"), "{error}");
    }
}
