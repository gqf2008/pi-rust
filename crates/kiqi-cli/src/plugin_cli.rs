//! `ki plugin` — inspect and edit the user's per-plugin capability grants.
//!
//! The grant store is the user's authorization (see
//! [`kiqi_plugin::grants`]); the host enforces it and this command is how the
//! user sets it. Nothing here is wasm-specific: the same files govern a wasm
//! plugin's `HostCaps`.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use kiqi_plugin::grants::{
    agent_dir, grants_path, installed_plugins, plugin_dir_at, ExecCwd, GrantOptions, Grants,
    HeadersMode, MessagesMode, OAuthFlow, OAuthGrant, OAuthProviderProfile, ObserveMode,
    ProviderEndpoint, ProviderGrant, ProviderModel, ProviderProfile, ProviderRequestGrant,
    RenderGrant, ResourcesGrant, OAUTH_MAX_REQUESTS_DEFAULT, OAUTH_TIMEOUT_DEFAULT_SECONDS,
    RENDER_KIND_MARKDOWN, RESOURCES_KIND_PROMPTS, RESOURCES_KIND_SKILLS,
};

/// Entry for `ki plugin [sub]`; `args` is the slice after `plugin`.
pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("list") => list(),
        Some("show") => show(args.get(1).map(String::as_str)),
        Some("grant") => grant(&args[1..]),
        Some("revoke") => revoke(&args[1..]),
        Some("install") => install(&args[1..]),
        Some("uninstall") => uninstall(&args[1..]),
        None | Some("--help") | Some("-h") | Some("help") => {
            print_help();
            0
        }
        Some(other) => {
            eprintln!("error: unknown `ki plugin` subcommand `{other}`");
            print_help();
            2
        }
    }
}

fn list() -> i32 {
    let plugins = installed_plugins();
    if plugins.is_empty() {
        println!("no plugins installed");
        return 0;
    }
    for id in plugins {
        let granted = Grants::load_for(&id).granted();
        if granted.is_empty() {
            println!("{id}: (nothing granted)");
        } else {
            println!("{id}: {}", granted.join(", "));
        }
    }
    0
}

fn show(id: Option<&str>) -> i32 {
    let Some(id) = id else {
        eprintln!("error: `ki plugin show <id>` needs a plugin id");
        return 2;
    };
    let grants = Grants::load_for(id);
    match grants_path(id) {
        Some(path) => println!("{id}  ({})", path.display()),
        None => println!("{id}"),
    }
    match serde_json::to_string_pretty(&grants) {
        Ok(text) => println!("{text}"),
        Err(error) => eprintln!("error: {error}"),
    }
    // The resolved policy, in words: endpoint hosts, model ids and kinds are
    // what the user is actually agreeing to. A credential is shown only as its
    // handle (plus the display name/fingerprint recorded at grant time) — the
    // value lives outside grants.json and is never printed.
    let credentials = agent_dir()
        .map(|root| read_credentials_at(&root, id))
        .unwrap_or_default();
    let disclosure = policy_disclosure(&grants, &credentials);
    if !disclosure.is_empty() {
        println!("\n{disclosure}");
    }
    if grants.oauth.is_some() {
        let store = crate::config::read_auth().unwrap_or_default();
        let text = oauth_credentials_disclosure(id, &grants, &store);
        if !text.is_empty() {
            println!("\n{text}");
        }
    }
    print_digest(id);
    // Declarations, not grants: what the plugin claims it contributes. This is
    // the consumer for `flags`/`shortcuts`, whose runtime paths are
    // `SessionRead::cli_flag` and an `Input` event respectively — the
    // declaration itself is only ever read here.
    print_declared(id);
    0
}

/// Print the component's recorded digest and whether the file still matches.
/// Compiled-in plugins have no `plugin.wasm`, so nothing is printed for them.
fn print_digest(id: &str) {
    let Some(dir) = kiqi_plugin::grants::plugin_dir(id) else {
        return;
    };
    let wasm = dir.join("plugin.wasm");
    if wasm.is_file() {
        println!("\n{}", digest_report(&wasm));
    }
}

/// One line of digest state; split from printing so it can be asserted on.
fn digest_report(wasm: &Path) -> String {
    match digest_status(wasm) {
        Ok(DigestStatus::Ok { recorded }) => format!("digest: {recorded} (ok)"),
        Ok(DigestStatus::Mismatch { recorded, actual }) => {
            format!("digest: {recorded} (mismatch; file is {actual})")
        }
        Ok(DigestStatus::Missing) => "digest: missing sidecar (unverified)".to_string(),
        Err(error) => format!("digest: cannot verify ({error})"),
    }
}

// ---------------------------------------------------------------------------
// Resolved policy disclosure (and the credential-handle sidecar)
// ---------------------------------------------------------------------------

/// The handle→metadata sidecar that lets `grants.json` name a credential
/// without ever holding its value. Lives at `plugins/<id>/credentials.json`.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
struct CredentialRecord {
    /// The user-facing source (`auth-json:<provider-id>`), never a secret.
    source: String,
    /// The auth-store provider id the value came from.
    provider: String,
    /// A display name resolved at grant time (never the value).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    display: String,
    /// SHA-256 of the value, so a changed credential can be detected later.
    fingerprint: String,
}

type CredentialSidecar = std::collections::BTreeMap<String, CredentialRecord>;

const CREDENTIALS_FILE: &str = "credentials.json";

fn credentials_path_at(root: &Path, plugin_id: &str) -> PathBuf {
    plugin_dir_at(root, plugin_id).join(CREDENTIALS_FILE)
}

fn read_credentials_at(root: &Path, plugin_id: &str) -> CredentialSidecar {
    std::fs::read_to_string(credentials_path_at(root, plugin_id))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_credentials_at(
    root: &Path,
    plugin_id: &str,
    sidecar: &CredentialSidecar,
) -> Result<(), String> {
    let path = credentials_path_at(root, plugin_id);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("create {}: {error}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(sidecar).map_err(|error| error.to_string())?;
    std::fs::write(&path, text).map_err(|error| format!("write {}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("chmod {}: {error}", path.display()))?;
    }
    Ok(())
}

/// Resolve `--credential-from auth-json:<provider-id>` into the opaque handle
/// `grants.json` stores.
///
/// The value is read, fingerprinted and dropped: the handle and the non-secret
/// metadata (source, display name, fingerprint) go to `credentials.json`, and
/// `show` can later say whether the credential changed without ever printing
/// it. `--credential-from` deliberately has no form that accepts a raw secret
/// on the command line.
fn mint_credential_handle(root: &Path, plugin_id: &str, source: &str) -> Result<String, String> {
    let provider_id = source
        .strip_prefix("auth-json:")
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            format!("--credential-from expects `auth-json:<provider-id>`, got `{source}`")
        })?;
    let path = root.join("auth.json");
    let text = std::fs::read_to_string(&path).map_err(|error| {
        format!(
            "read {} (has `{provider_id}` been logged in?): {error}",
            path.display()
        )
    })?;
    let store: crate::config::AuthStore =
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    let value = match store.get(provider_id) {
        Some(crate::config::Credential::ApiKey { key: Some(key), .. }) => key.as_str(),
        Some(crate::config::Credential::Oauth { access, .. }) => access.as_str(),
        _ => {
            return Err(format!(
                "auth.json has no usable credential for `{provider_id}`"
            ))
        }
    };
    let fingerprint = sha256_hex(value.as_bytes());
    let handle = format!("pc-{}", &fingerprint[..16]);
    let mut sidecar = read_credentials_at(root, plugin_id);
    sidecar.insert(
        handle.clone(),
        CredentialRecord {
            source: source.to_string(),
            provider: provider_id.to_string(),
            display: provider_id.to_string(),
            fingerprint,
        },
    );
    write_credentials_at(root, plugin_id, &sidecar)?;
    Ok(handle)
}

fn display_name(recorded: &str, fallback: &str) -> String {
    if recorded.is_empty() {
        fallback.to_string()
    } else {
        recorded.to_string()
    }
}

fn observe_name(mode: ObserveMode) -> &'static str {
    match mode {
        ObserveMode::None => "none",
        ObserveMode::Requests => "requests",
    }
}

fn headers_name(mode: HeadersMode) -> &'static str {
    match mode {
        HeadersMode::Names => "names",
        HeadersMode::Values => "values",
    }
}

fn messages_name(mode: MessagesMode) -> &'static str {
    match mode {
        MessagesMode::None => "none",
        MessagesMode::Full => "full",
    }
}

/// The resolved policy in words. Endpoint hosts, model ids, kinds and budgets
/// are the thing the user is agreeing to; a credential appears only as its
/// handle (with display name, source and fingerprint), never its value.
fn policy_disclosure(grants: &Grants, credentials: &CredentialSidecar) -> String {
    let mut out = String::new();
    if let Some(oauth) = &grants.oauth {
        out.push_str("oauth policy:\n");
        for profile in &oauth.providers {
            let flows = profile
                .flows
                .iter()
                .map(|flow| match flow {
                    OAuthFlow::AuthorizationCode => "authorization-code",
                    OAuthFlow::DeviceCode => "device-code",
                })
                .collect::<Vec<_>>()
                .join(", ");
            let scopes = if profile.scopes.is_empty() {
                "(none)".to_string()
            } else {
                profile.scopes.join(" ")
            };
            out.push_str(&format!(
                "  {} ({}) token: {}; authorization: {}; device: {}; revocation: {}; \
                 flows: {}; scopes: {}; pkce: {}\n",
                profile.id,
                display_name(&profile.display_name, &profile.id),
                profile.token_endpoint,
                profile.authorization_endpoint,
                profile
                    .device_authorization_endpoint
                    .as_deref()
                    .unwrap_or("(none)"),
                profile.revocation_endpoint.as_deref().unwrap_or("(none)"),
                flows,
                scopes,
                if profile.supports_pkce { "yes" } else { "no" },
            ));
        }
        out.push_str(&format!(
            "  budget: {}s, {} requests\n",
            oauth.timeout_seconds, oauth.max_requests
        ));
    }
    if let Some(provider) = &grants.provider {
        out.push_str("provider policy:\n");
        for profile in &provider.providers {
            let models = profile
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let credential = match &profile.credential {
                Some(handle) => match credentials.get(handle) {
                    Some(record) => format!(
                        "{handle} ({} · {} · fingerprint {})",
                        display_name(&record.display, &record.provider),
                        record.source,
                        &record.fingerprint[..record.fingerprint.len().min(16)]
                    ),
                    None => format!("{handle} (no sidecar record)"),
                },
                None => "(none)".to_string(),
            };
            out.push_str(&format!(
                "  {} ({}) endpoint: {}://{}{} methods: {}; models: {}; credential: {}\n",
                profile.id,
                display_name(&profile.display_name, &profile.id),
                profile.endpoint.scheme,
                profile.endpoint.host,
                profile.endpoint.path_prefix,
                profile.endpoint.methods.join(","),
                if models.is_empty() {
                    "(none)".to_string()
                } else {
                    models
                },
                credential,
            ));
            out.push_str(&format!("  budget: {}s\n", profile.timeout_seconds));
        }
    }
    if let Some(render) = &grants.render {
        out.push_str(&format!(
            "render policy: kinds: {}; timeout: {}ms; max output: {} bytes\n",
            render.kinds.join(","),
            render.timeout_ms,
            render.max_output_bytes
        ));
    }
    if let Some(resources) = &grants.resources {
        out.push_str(&format!(
            "resources policy: kinds: {}; max items: {}; max bytes: {}\n",
            resources.kinds.join(","),
            resources.max_items,
            resources.max_bytes
        ));
    }
    if let Some(request) = &grants.provider_request {
        out.push_str(&format!(
            "provider-request policy: observe: {}; headers: {}; messages: {}; patch: {}; \
             patch timeout: {}ms\n",
            observe_name(request.observe),
            headers_name(request.headers),
            messages_name(request.messages),
            if request.patch { "enabled" } else { "disabled" },
            request.patch_timeout_ms
        ));
    }
    out
}

/// The stored OAuth credential handles for a plugin's granted providers.
///
/// Only the namespaced key and its expiry are disclosed; the value stays in
/// `auth.json` and is never printed, cached or passed to a guest.
fn oauth_credentials_disclosure(
    install_id: &str,
    grants: &Grants,
    store: &crate::config::AuthStore,
) -> String {
    let Some(oauth) = &grants.oauth else {
        return String::new();
    };
    let mut out = String::from("oauth credentials:\n");
    for profile in &oauth.providers {
        let key = crate::config::plugin_credential_key(install_id, &profile.id);
        let state = match store.get(&key) {
            Some(crate::config::Credential::Oauth { expires, .. }) => {
                if *expires == 0 {
                    "stored (no known expiry)".to_string()
                } else {
                    format!("stored (expires {expires})")
                }
            }
            Some(_) => "refused: a non-OAuth entry occupies this key".to_string(),
            None => "absent".to_string(),
        };
        out.push_str(&format!("  {key}: {state}\n"));
    }
    out
}

/// List a plugin's declared surfaces, or say why nothing could be listed.
fn print_declared(id: &str) {
    let host = crate::plugins::compiled_plugin_hosts()
        .into_iter()
        .find(|host| host.id() == id);
    match host {
        Some(host) => println!("\n{}", declared_surfaces(host.plugin())),
        None => eprintln!(
            "note: no compiled or wasm plugin with id `{id}` is installed, so its declared \
             surfaces cannot be listed"
        ),
    }
}

/// Render the surfaces a plugin declares. Split from the printing so it can be
/// asserted on.
fn declared_surfaces(plugin: &dyn kiqi_plugin::Plugin) -> String {
    let mut out = String::new();
    let mut section = |label: &str, items: Vec<String>| {
        if items.is_empty() {
            return;
        }
        out.push_str(&format!("{label}:\n"));
        for item in items {
            out.push_str(&format!("  {item}\n"));
        }
    };
    section(
        "tools",
        plugin
            .tools()
            .iter()
            .map(|spec| spec.name.clone())
            .collect(),
    );
    section(
        "commands",
        plugin
            .commands()
            .iter()
            .map(|spec| {
                format!(
                    "/{} \u{2014} {}",
                    spec.name.trim_start_matches('/'),
                    spec.description
                )
            })
            .collect(),
    );
    section(
        "flags",
        plugin
            .flags()
            .iter()
            .map(|spec| format!("--{} \u{2014} {}", spec.name, spec.description))
            .collect(),
    );
    section(
        "shortcuts",
        plugin
            .shortcuts()
            .iter()
            .map(|spec| format!("{} \u{2014} {}", spec.key, spec.description))
            .collect(),
    );
    section("probe points", plugin.probe_points());
    out
}

/// A plugin id names a directory under `plugins/`, so it must be exactly one
/// plain path component. Without this, `PathBuf::join` makes an absolute id
/// replace the whole path (`--id /home/me/notes` writes there) and `..` walks
/// out of the store (`ki plugin uninstall ..` renames the agent directory).
fn validate_id(id: &str) -> Result<(), String> {
    let plain = !id.is_empty()
        && id != "."
        && id != ".."
        && !id.contains('/')
        && !id.contains('\\')
        // The uninstall marker is reserved: the loader skips any directory
        // carrying it, so an id that contained it would install a plugin that
        // can never load (the two ends must agree).
        && !kiqi_plugin::grants::is_uninstalled_dir_name(id)
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'));
    if plain {
        Ok(())
    } else {
        Err(format!(
            "invalid plugin id `{id}`: use one plain name (letters, digits, `.`, `-`, `_`, `+`)"
        ))
    }
}

/// The capabilities a component declares, or an error when this build has no
/// wasm tier.
///
/// `kiqi-plugin-host` is an optional dependency (the `plugin-wasm` feature,
/// which `Cargo.toml` documents as "off only for builds that want the leaner
/// binary without the wasm tier"). Calling it ungated made that documented
/// build fail to compile, so both entry points go through these shims.
#[cfg(feature = "plugin-wasm")]
fn declared_capabilities(wasm: &Path) -> Result<Vec<String>, String> {
    kiqi_plugin_host::declared_capabilities(wasm)
}

#[cfg(not(feature = "plugin-wasm"))]
fn declared_capabilities(_wasm: &Path) -> Result<Vec<String>, String> {
    Err("this build has no wasm plugin tier (`plugin-wasm` is disabled)".to_string())
}

/// [`declared_capabilities`] plus the package conventions.
///
/// `resources` has no component interface: a package declares it by shipping
/// `resources.json` beside `plugin.wasm`. The installer must see it in the
/// consent prompt and the "not granted" summary, so the manifest is scanned
/// here (the component scan alone can never report it).
fn declared_capabilities_in(wasm: &Path, package_dir: &Path) -> Result<Vec<String>, String> {
    let mut caps = declared_capabilities(wasm)?;
    if crate::plugins::has_package_resources(package_dir)
        && !caps.iter().any(|cap| cap == "resources")
    {
        caps.push("resources".to_string());
        caps.sort();
    }
    Ok(caps)
}

#[cfg(feature = "plugin-wasm")]
fn capability_is_enforced(name: &str) -> bool {
    kiqi_plugin_host::capability_is_enforced(name)
}

#[cfg(not(feature = "plugin-wasm"))]
fn capability_is_enforced(_name: &str) -> bool {
    false
}

fn grant(args: &[String]) -> i32 {
    grant_into(args, None)
}

/// [`grant`] against an explicit agent root (the test seam).
fn grant_into(args: &[String], root: Option<&Path>) -> i32 {
    let parsed = match parse_target(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let Some(root) = resolve_root(root) else {
        eprintln!("error: cannot resolve the agent directory");
        return 1;
    };
    let mut grants = Grants::load_for_at(&root, &parsed.id);
    // Grant REPLACES the capability's whole policy. When an exec grant with a
    // non-default scope or budget is replaced without those flags, say what
    // was reset rather than letting it silently revert.
    let old_exec = if parsed.capability == "exec" {
        grants.exec.clone()
    } else {
        None
    };
    let given_options = parsed.options.clone();
    if let Err(error) = apply_grant(&mut grants, &parsed, &root) {
        eprintln!("error: {error}");
        return 2;
    }
    if let Err(error) = Grants::save_for_at(&root, &parsed.id, &grants) {
        eprintln!("error: {error}");
        return 1;
    }
    println!("granted `{}` to {}", parsed.capability, parsed.id);
    if let Some(warning) = credential_capability_warning(&grants, &parsed.capability, &parsed.id) {
        eprintln!("{warning}");
    }
    if let (Some(old), Some(new)) = (old_exec.as_ref(), grants.exec.as_ref()) {
        if let Some(note) = exec_reset_note(old, &given_options, new) {
            eprintln!("{note}");
        }
    }
    0
}

/// The residual O17 risk made visible: a plugin that already holds exec, fs or
/// raw sockets can read `auth.json` (and every `plugin:*` token) directly, so
/// granting it `oauth`/`provider` does not actually keep the credential out of
/// its reach. The grant still goes through — this is a warning, not a refusal —
/// and names the cleanup commands.
fn credential_capability_warning(grants: &Grants, capability: &str, id: &str) -> Option<String> {
    if capability != "oauth" && capability != "provider" {
        return None;
    }
    let mut held = Vec::new();
    if grants.exec.is_some() {
        held.push("exec");
    }
    if grants.fs.is_some() {
        held.push("fs");
    }
    if grants.net_sockets.is_some() {
        held.push("net-sockets");
    }
    if held.is_empty() {
        return None;
    }
    Some(format!(
        "warning: `{id}` already holds {}; that can read auth.json and every stored plugin \
         token, so granting `{capability}` does not keep the credential out of its reach \
         (revoke it with `ki plugin revoke {id} exec|fs|net-sockets`; clear a stored token \
         with `ki auth logout --provider <provider>`)",
        held.join("/"),
    ))
}

/// Apply one parsed grant. The five policy-shaped keys build their structured
/// policy here; every legacy key keeps the value-list form.
fn apply_grant(grants: &mut Grants, target: &Target, root: &Path) -> Result<(), String> {
    target.flags.validate_for(&target.capability)?;
    if !target.policy.is_empty() {
        return Err(format!(
            "`{}` takes no --host/--command; set its own policy flags instead",
            target.capability
        ));
    }
    match target.capability.as_str() {
        "oauth" => {
            reject_cwd_and_max_open(target)?;
            grants.grant_oauth(build_oauth(target)?)
        }
        "provider" => {
            reject_cwd_and_max_open(target)?;
            grants.grant_provider(build_provider(target, root)?)
        }
        "render" => {
            reject_all_grant_options(target)?;
            grants.grant_render(build_render(target))
        }
        "resources" => {
            reject_all_grant_options(target)?;
            grants.grant_resources(build_resources(target))
        }
        "provider-request" => {
            reject_all_grant_options(target)?;
            grants.grant_provider_request(build_provider_request(target)?)
        }
        _ => grants.grant(
            &target.capability,
            target.policy.clone(),
            target.options.clone(),
        ),
    }
}

/// `--cwd`/`--max-open` are exec/proc knobs. `--timeout` is meaningful for
/// `oauth`/`provider` (their flow/request budget), so it stays.
fn reject_cwd_and_max_open(target: &Target) -> Result<(), String> {
    if target.options.cwd.is_some() || target.options.max_open.is_some() {
        return Err(format!(
            "`{}` takes no --cwd/--max-open (set its own policy fields instead)",
            target.capability
        ));
    }
    Ok(())
}

/// `render`/`resources`/`provider-request` carry their own budgets; none of the
/// exec-era options apply.
fn reject_all_grant_options(target: &Target) -> Result<(), String> {
    if target.options.cwd.is_some()
        || target.options.timeout_seconds.is_some()
        || target.options.max_open.is_some()
    {
        return Err(format!(
            "`{}` takes no --cwd/--timeout/--max-open (set its own policy fields instead)",
            target.capability
        ));
    }
    Ok(())
}

fn build_oauth(target: &Target) -> Result<OAuthGrant, String> {
    let flags = &target.flags;
    if flags.oauth_profiles.is_empty() {
        return Err("`oauth` needs --provider <id> --profile <file.json>".to_string());
    }
    if flags.oauth_providers.len() != flags.oauth_profiles.len() {
        return Err("each --profile needs a matching --provider <id>".to_string());
    }
    let mut providers = Vec::new();
    for (provider_id, path) in flags.oauth_providers.iter().zip(&flags.oauth_profiles) {
        let text =
            std::fs::read_to_string(path).map_err(|error| format!("read {path}: {error}"))?;
        let mut profile: OAuthProviderProfile =
            serde_json::from_str(&text).map_err(|error| format!("{path}: {error}"))?;
        if profile.id.is_empty() {
            profile.id = provider_id.clone();
        } else if profile.id != *provider_id {
            return Err(format!(
                "{path} names provider `{}`, but --provider said `{provider_id}`",
                profile.id
            ));
        }
        providers.push(profile);
    }
    Ok(OAuthGrant {
        providers,
        timeout_seconds: target
            .options
            .timeout_seconds
            .unwrap_or(OAUTH_TIMEOUT_DEFAULT_SECONDS),
        max_requests: flags.max_requests.unwrap_or(OAUTH_MAX_REQUESTS_DEFAULT),
    })
}

fn build_provider(target: &Target, root: &Path) -> Result<ProviderGrant, String> {
    let flags = &target.flags;
    if flags.model_json.is_empty() {
        return Err("`provider` needs --model-json <file.json>".to_string());
    }
    let endpoint_text = flags
        .endpoint
        .as_deref()
        .ok_or_else(|| "`provider` needs --endpoint <scheme://host/prefix>".to_string())?;
    let endpoint = parse_provider_endpoint(endpoint_text, &flags.methods)?;
    let mut models = Vec::new();
    for path in &flags.model_json {
        models.extend(read_provider_models(path)?);
    }
    let credential = match &flags.credential_from {
        Some(source) => Some(mint_credential_handle(root, &target.id, source)?),
        None => None,
    };
    Ok(ProviderGrant {
        providers: vec![ProviderProfile {
            id: target.id.clone(),
            display_name: String::new(),
            models,
            endpoint,
            credential,
            auth: None,
            timeout_seconds: target
                .options
                .timeout_seconds
                .unwrap_or(kiqi_plugin::grants::PROVIDER_TIMEOUT_DEFAULT_SECONDS),
            max_response_bytes: kiqi_plugin::grants::PROVIDER_MAX_RESPONSE_BYTES_DEFAULT,
            max_handles: kiqi_plugin::grants::PROVIDER_MAX_HANDLES_DEFAULT,
        }],
    })
}

/// One `--model-json` file. A bare model object, an array of them, or an
/// object with a `models` array are all accepted, because the catalog file the
/// user already has may be any of the three.
fn read_provider_models(path: &str) -> Result<Vec<ProviderModel>, String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("read {path}: {error}"))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| format!("{path}: {error}"))?;
    let items = match value {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Object(mut map) => match map.remove("models") {
            Some(serde_json::Value::Array(items)) => items,
            Some(other) => vec![other],
            None => vec![serde_json::Value::Object(map)],
        },
        other => vec![other],
    };
    items
        .into_iter()
        .map(|item| serde_json::from_value(item).map_err(|error| format!("{path}: {error}")))
        .collect()
}

/// `--endpoint <scheme://host/path-prefix>`. The host is exact (no wildcard);
/// the grants model re-checks scheme/host/path and the methods.
fn parse_provider_endpoint(value: &str, methods: &[String]) -> Result<ProviderEndpoint, String> {
    let (scheme, rest) = value
        .split_once("://")
        .ok_or_else(|| format!("--endpoint needs `scheme://host/prefix`, got `{value}`"))?;
    if scheme.is_empty() || rest.is_empty() {
        return Err(format!(
            "--endpoint needs `scheme://host/prefix`, got `{value}`"
        ));
    }
    if rest.contains('@') {
        return Err("--endpoint must not carry userinfo".to_string());
    }
    let (host, path_prefix) = match rest.split_once('/') {
        Some((host, path)) => (host.to_string(), format!("/{path}")),
        None => (rest.to_string(), "/".to_string()),
    };
    if host.is_empty() {
        return Err(format!("--endpoint needs a host, got `{value}`"));
    }
    Ok(ProviderEndpoint {
        scheme: scheme.to_string(),
        host,
        path_prefix,
        methods: if methods.is_empty() {
            vec!["POST".to_string()]
        } else {
            methods.to_vec()
        },
    })
}

fn build_render(target: &Target) -> RenderGrant {
    let kinds = if target.flags.kinds.is_empty() {
        vec![RENDER_KIND_MARKDOWN.to_string()]
    } else {
        target.flags.kinds.clone()
    };
    RenderGrant {
        kinds,
        ..RenderGrant::default()
    }
}

fn build_resources(target: &Target) -> ResourcesGrant {
    let kinds = if target.flags.kinds.is_empty() {
        vec![
            RESOURCES_KIND_SKILLS.to_string(),
            RESOURCES_KIND_PROMPTS.to_string(),
        ]
    } else {
        target.flags.kinds.clone()
    };
    ResourcesGrant {
        kinds,
        ..ResourcesGrant::default()
    }
}

fn build_provider_request(target: &Target) -> Result<ProviderRequestGrant, String> {
    let flags = &target.flags;
    let headers = match flags.headers.as_deref() {
        None | Some("names") => HeadersMode::Names,
        Some("values") => HeadersMode::Values,
        Some(other) => {
            return Err(format!(
                "--headers must be `names` or `values`, got `{other}`"
            ))
        }
    };
    let messages = match flags.messages.as_deref() {
        None | Some("none") => MessagesMode::None,
        Some("full") => MessagesMode::Full,
        Some(other) => {
            return Err(format!(
                "--messages must be `none` or `full`, got `{other}`"
            ))
        }
    };
    Ok(ProviderRequestGrant {
        observe: if flags.observe {
            ObserveMode::Requests
        } else {
            ObserveMode::None
        },
        headers,
        messages,
        patch: flags.patch,
        patch_timeout_ms: flags
            .patch_timeout_ms
            .unwrap_or(kiqi_plugin::grants::PROVIDER_REQUEST_PATCH_TIMEOUT_MS_DEFAULT),
    })
}

/// The note to print when re-granting `exec` silently resets a non-default
/// scope/budget: grant REPLACES the whole policy, so an unstated `--cwd` /
/// `--timeout` reverts to the defaults — visible beats silent.
fn exec_reset_note(
    old: &kiqi_plugin::grants::ExecGrant,
    options: &GrantOptions,
    new: &kiqi_plugin::grants::ExecGrant,
) -> Option<String> {
    let reset = (old.cwd != new.cwd && options.cwd.is_none())
        || (old.timeout_seconds != new.timeout_seconds && options.timeout_seconds.is_none());
    if !reset {
        return None;
    }
    Some(format!(
        "note: the previous exec policy (cwd: {}, timeout: {}s) was reset to the \
         defaults; pass --cwd/--timeout to keep it",
        match old.cwd {
            ExecCwd::PluginData => "plugin-data",
            ExecCwd::Project => "project",
        },
        old.timeout_seconds,
    ))
}

fn revoke(args: &[String]) -> i32 {
    revoke_into(args, None)
}

/// [`revoke`] against an explicit agent root (the test seam).
fn revoke_into(args: &[String], root: Option<&Path>) -> i32 {
    let parsed = match parse_target(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    if !parsed.policy.is_empty()
        || parsed.options.cwd.is_some()
        || parsed.options.timeout_seconds.is_some()
        || !parsed.flags.is_empty()
    {
        eprintln!("error: `revoke` takes no flags (it clears the whole capability)");
        return 2;
    }
    let Some(root) = resolve_root(root) else {
        eprintln!("error: cannot resolve the agent directory");
        return 1;
    };
    let mut grants = Grants::load_for_at(&root, &parsed.id);
    // Revoking `oauth` must also take the tokens away. Clearing the grant alone
    // leaves a live refresh token in auth.json that no command can reach any
    // more (`ki auth logout` needs a grant to find the provider). Attempt the
    // IdP revocation while the profile is still known, then delete the
    // namespaced entry whether or not the endpoint answered.
    if parsed.capability == "oauth" {
        #[cfg(feature = "plugin-wasm")]
        let report = revoke_oauth_credentials(&root, &parsed.id, grants.oauth.as_ref(), None);
        #[cfg(not(feature = "plugin-wasm"))]
        let report = revoke_oauth_credentials(&root, &parsed.id, grants.oauth.as_ref());
        for provider in &report.removed {
            println!("removed stored OAuth credential for `{provider}`");
        }
        for failure in &report.failures {
            eprintln!(
                "warning: could not revoke OAuth credential ({failure}); \
                 the local credential was removed anyway"
            );
        }
    }
    if let Err(error) = grants.revoke(&parsed.capability) {
        eprintln!("error: {error}");
        return 2;
    }
    if let Err(error) = Grants::save_for_at(&root, &parsed.id, &grants) {
        eprintln!("error: {error}");
        return 1;
    }
    println!("revoked `{}` from {}", parsed.capability, parsed.id);
    0
}

/// What one `revoke oauth` did to the stored credentials.
#[derive(Default)]
struct OAuthRevocation {
    /// Provider ids whose namespaced entry was deleted.
    removed: Vec<String>,
    /// Remote/local failures, already worded for the user.
    failures: Vec<String>,
}

/// Delete the namespaced credential(s) named by an `oauth` grant, attempting
/// host-side revocation for every provider that still has a stored token.
///
/// The local delete is unconditional: a failing IdP endpoint must not leave a
/// token on disk that no CLI command can reach once the grant is gone.
#[cfg(feature = "plugin-wasm")]
fn revoke_oauth_credentials(
    root: &Path,
    install_id: &str,
    grant: Option<&OAuthGrant>,
    transport: Option<&dyn kiqi_plugin_host::OAuthTransport>,
) -> OAuthRevocation {
    let default;
    let transport = match transport {
        Some(transport) => transport,
        None => {
            default = kiqi_plugin_host::default_transport();
            &*default
        }
    };
    revoke_oauth_credentials_with(root, install_id, grant, transport)
}

/// [`revoke_oauth_credentials`] against an injected transport (the test seam).
#[cfg(feature = "plugin-wasm")]
fn revoke_oauth_credentials_with(
    root: &Path,
    install_id: &str,
    grant: Option<&OAuthGrant>,
    transport: &dyn kiqi_plugin_host::OAuthTransport,
) -> OAuthRevocation {
    let mut report = OAuthRevocation::default();
    let Some(grant) = grant else {
        return report;
    };
    for profile in &grant.providers {
        let stored = match crate::config::read_plugin_oauth_token_at(root, install_id, &profile.id)
        {
            Ok(Some(crate::config::Credential::Oauth {
                access,
                refresh,
                expires,
                profile_fingerprint,
            })) => Some(kiqi_plugin_host::StoredOAuthToken {
                access,
                refresh,
                expires,
                profile_fingerprint,
            }),
            Ok(_) => None,
            Err(error) => {
                report
                    .failures
                    .push(format!("read the credential for `{}`: {error}", profile.id));
                None
            }
        };
        if let Some(token) = &stored {
            if profile.revocation_endpoint.is_some() {
                let mut budget = kiqi_plugin_host::OAuthRequestBudget::for_grant(grant);
                if let Err(error) =
                    kiqi_plugin_host::revoke_stored_token(profile, token, &mut budget, transport)
                {
                    report
                        .failures
                        .push(format!("`{}` at the IdP: {error}", profile.id));
                }
            }
        }
        match crate::config::delete_plugin_oauth_token_at(root, install_id, &profile.id) {
            Ok(true) => report.removed.push(profile.id.clone()),
            Ok(false) => {}
            Err(error) => report.failures.push(format!(
                "remove the local credential for `{}`: {error}",
                profile.id
            )),
        }
    }
    report
}

/// Without the wasm tier there is no host transport; the tokens still must go.
#[cfg(not(feature = "plugin-wasm"))]
fn revoke_oauth_credentials(
    root: &Path,
    install_id: &str,
    grant: Option<&OAuthGrant>,
) -> OAuthRevocation {
    let mut report = OAuthRevocation::default();
    let Some(grant) = grant else {
        return report;
    };
    for profile in &grant.providers {
        match crate::config::delete_plugin_oauth_token_at(root, install_id, &profile.id) {
            Ok(true) => report.removed.push(profile.id.clone()),
            Ok(false) => {}
            Err(error) => report.failures.push(format!(
                "remove the local credential for `{}`: {error}",
                profile.id
            )),
        }
    }
    report
}

/// Flags for the five policy-shaped keys. Kept apart from the value-list
/// `policy`/`options` pair so an exec-era flag can never leak into a profile.
#[derive(Default, Clone)]
struct StructuredFlags {
    oauth_providers: Vec<String>,
    oauth_profiles: Vec<String>,
    model_json: Vec<String>,
    endpoint: Option<String>,
    methods: Vec<String>,
    credential_from: Option<String>,
    kinds: Vec<String>,
    observe: bool,
    headers: Option<String>,
    messages: Option<String>,
    patch: bool,
    patch_timeout_ms: Option<u64>,
    max_requests: Option<u32>,
}

impl StructuredFlags {
    fn is_empty(&self) -> bool {
        self.oauth_providers.is_empty()
            && self.oauth_profiles.is_empty()
            && self.model_json.is_empty()
            && self.endpoint.is_none()
            && self.methods.is_empty()
            && self.credential_from.is_none()
            && self.kinds.is_empty()
            && !self.observe
            && self.headers.is_none()
            && self.messages.is_none()
            && !self.patch
            && self.patch_timeout_ms.is_none()
            && self.max_requests.is_none()
    }

    /// Reject a flag that belongs to a different capability, so `--kind` on
    /// `oauth` is an error instead of a silently ignored option.
    fn validate_for(&self, capability: &str) -> Result<(), String> {
        let oauth_used = !self.oauth_providers.is_empty()
            || !self.oauth_profiles.is_empty()
            || self.max_requests.is_some();
        let provider_used = !self.model_json.is_empty()
            || self.endpoint.is_some()
            || !self.methods.is_empty()
            || self.credential_from.is_some();
        let kind_used = !self.kinds.is_empty();
        let request_used = self.observe
            || self.headers.is_some()
            || self.messages.is_some()
            || self.patch
            || self.patch_timeout_ms.is_some();
        let ok = match capability {
            "oauth" => !provider_used && !kind_used && !request_used,
            "provider" => !oauth_used && !kind_used && !request_used,
            "render" | "resources" => !oauth_used && !provider_used && !request_used,
            "provider-request" => !oauth_used && !provider_used && !kind_used,
            _ => !oauth_used && !provider_used && !kind_used && !request_used,
        };
        if ok {
            Ok(())
        } else {
            Err(format!(
                "one or more flags do not apply to `{capability}`; see `ki plugin --help`"
            ))
        }
    }
}

struct Target {
    id: String,
    capability: String,
    /// The capability's policy: `--host` values for `net-http`/`net-sockets`,
    /// `--command` values for `exec` (empty for `fs`, whose jail is the policy).
    policy: Vec<String>,
    /// Exec-only options (`--cwd`, `--timeout`); other capabilities reject them.
    options: GrantOptions,
    /// Policy flags for `oauth`/`provider`/`render`/`resources`/`provider-request`.
    flags: StructuredFlags,
}

/// The `--timeout` ceiling for a capability: `proc` runs long jobs by design,
/// so it carries its own, longer cap than `exec`; any other capability falls
/// back to `exec`'s (the grants model rejects `--timeout` on those outright,
/// so the fallback only shapes the message).
fn timeout_max_for(capability: &str) -> u64 {
    if capability == "proc" {
        kiqi_plugin::grants::PROC_TIMEOUT_MAX_SECONDS
    } else {
        kiqi_plugin::grants::EXEC_TIMEOUT_MAX_SECONDS
    }
}

/// Parse a `--timeout` value's shared lower bound (zero is rejected for every
/// capability). The capability-specific ceiling is checked by the caller once
/// the capability is known — the flag may precede it — keeping a bad timeout a
/// CLI error: the grants model re-checks, but `install` downgrades grant
/// errors to a note, so it must fail before it ever gets there.
fn parse_timeout(value: &str) -> Result<u64, String> {
    let seconds = value
        .parse::<u64>()
        .map_err(|_| format!("--timeout needs seconds, got `{value}`"))?;
    if seconds == 0 {
        return Err(format!(
            "--timeout must be at least 1 second, got {seconds}"
        ));
    }
    Ok(seconds)
}

/// `<id> <capability> [flags]...`, shared by grant and revoke.
fn parse_target(args: &[String]) -> Result<Target, i32> {
    let mut id = None;
    let mut capability = None;
    let mut policy = Vec::new();
    let mut options = GrantOptions::default();
    let mut flags = StructuredFlags::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--host" | "--command" => match iter.next() {
                Some(value) => policy.push(value.clone()),
                None => {
                    eprintln!("error: {arg} needs a value");
                    return Err(2);
                }
            },
            "--cwd" => match iter.next() {
                Some(value) => match ExecCwd::parse(value) {
                    Ok(cwd) => options.cwd = Some(cwd),
                    Err(error) => {
                        eprintln!("error: {error}");
                        return Err(2);
                    }
                },
                None => {
                    eprintln!("error: --cwd needs a value");
                    return Err(2);
                }
            },
            "--timeout" => match iter.next() {
                Some(value) => match parse_timeout(value) {
                    Ok(seconds) => options.timeout_seconds = Some(seconds),
                    Err(error) => {
                        eprintln!("error: {error}");
                        return Err(2);
                    }
                },
                None => {
                    eprintln!("error: --timeout needs a value");
                    return Err(2);
                }
            },
            "--max-open" => match iter.next() {
                Some(value) => match value.parse::<u32>() {
                    Ok(n) => options.max_open = Some(n),
                    Err(_) => {
                        eprintln!("error: --max-open needs a number, got `{value}`");
                        return Err(2);
                    }
                },
                None => {
                    eprintln!("error: --max-open needs a value");
                    return Err(2);
                }
            },
            "--provider" => match iter.next() {
                Some(value) => flags.oauth_providers.push(value.clone()),
                None => {
                    eprintln!("error: --provider needs a provider id");
                    return Err(2);
                }
            },
            "--profile" => match iter.next() {
                Some(value) => flags.oauth_profiles.push(value.clone()),
                None => {
                    eprintln!("error: --profile needs a JSON file");
                    return Err(2);
                }
            },
            "--model-json" => match iter.next() {
                Some(value) => flags.model_json.push(value.clone()),
                None => {
                    eprintln!("error: --model-json needs a JSON file");
                    return Err(2);
                }
            },
            "--endpoint" => match iter.next() {
                Some(value) => flags.endpoint = Some(value.clone()),
                None => {
                    eprintln!("error: --endpoint needs a value");
                    return Err(2);
                }
            },
            "--method" => match iter.next() {
                Some(value) => flags.methods.push(value.to_ascii_uppercase()),
                None => {
                    eprintln!("error: --method needs GET or POST");
                    return Err(2);
                }
            },
            "--credential-from" => match iter.next() {
                Some(value) => flags.credential_from = Some(value.clone()),
                None => {
                    eprintln!("error: --credential-from needs `auth-json:<provider-id>`");
                    return Err(2);
                }
            },
            "--kind" => match iter.next() {
                Some(value) => flags.kinds.push(value.clone()),
                None => {
                    eprintln!("error: --kind needs a value");
                    return Err(2);
                }
            },
            "--observe" => flags.observe = true,
            "--patch" => flags.patch = true,
            "--headers" => match iter.next() {
                Some(value) => flags.headers = Some(value.clone()),
                None => {
                    eprintln!("error: --headers needs `names` or `values`");
                    return Err(2);
                }
            },
            "--messages" => match iter.next() {
                Some(value) => flags.messages = Some(value.clone()),
                None => {
                    eprintln!("error: --messages needs `none` or `full`");
                    return Err(2);
                }
            },
            "--patch-timeout-ms" => match iter.next() {
                Some(value) => match value.parse::<u64>() {
                    Ok(n) => flags.patch_timeout_ms = Some(n),
                    Err(_) => {
                        eprintln!("error: --patch-timeout-ms needs a number, got `{value}`");
                        return Err(2);
                    }
                },
                None => {
                    eprintln!("error: --patch-timeout-ms needs a value");
                    return Err(2);
                }
            },
            "--max-requests" => match iter.next() {
                Some(value) => match value.parse::<u32>() {
                    Ok(n) => flags.max_requests = Some(n),
                    Err(_) => {
                        eprintln!("error: --max-requests needs a number, got `{value}`");
                        return Err(2);
                    }
                },
                None => {
                    eprintln!("error: --max-requests needs a value");
                    return Err(2);
                }
            },
            other if other.starts_with("--") => {
                eprintln!("error: unknown flag `{other}`");
                return Err(2);
            }
            other if id.is_none() => id = Some(other.to_string()),
            other if capability.is_none() => capability = Some(other.to_string()),
            other => {
                eprintln!("error: unexpected argument `{other}`");
                return Err(2);
            }
        }
    }
    let (Some(id), Some(capability)) = (id, capability) else {
        eprintln!(
            "error: expected `ki plugin <grant|revoke> <id> <capability> [flags]...` \
             (see `ki plugin --help`)"
        );
        return Err(2);
    };
    // The capability is known only now (it may follow --timeout), so the
    // budget's ceiling is enforced here, on the same layer as a bad --cwd.
    if let Some(seconds) = options.timeout_seconds {
        if matches!(capability.as_str(), "exec" | "proc") {
            let max = timeout_max_for(&capability);
            if seconds > max {
                eprintln!("error: {capability} --timeout must be 1..={max} seconds, got {seconds}");
                return Err(2);
            }
        }
    }
    // Enumerated values are checked here so a bad --headers/--messages is a
    // usage error rather than a silently defaulted policy.
    if let Some(value) = &flags.headers {
        if value != "names" && value != "values" {
            eprintln!("error: --headers must be `names` or `values`, got `{value}`");
            return Err(2);
        }
    }
    if let Some(value) = &flags.messages {
        if value != "none" && value != "full" {
            eprintln!("error: --messages must be `none` or `full`, got `{value}`");
            return Err(2);
        }
    }
    if let Err(error) = validate_id(&id) {
        eprintln!("error: {error}");
        return Err(2);
    }
    Ok(Target {
        id,
        capability,
        policy,
        options,
        flags,
    })
}

fn print_help() {
    println!(
        "\
ki plugin — manage per-plugin capability grants

USAGE:
    ki insmod <path.wasm> [flags]        (same as `ki plugin install`)
    ki insmod --build <guest-crate-dir>  build a guest for wasm32-wasip2 first
    ki rmmod <id>                        (same as `ki plugin uninstall`)
    ki plugin list
    ki plugin show <id>
    ki plugin install <path.wasm> [--id <id>] [--yes] [--host <host>]... [--command <cmd>]...
                                  [--cwd <plugin-data|project>] [--timeout <seconds>]
    ki plugin uninstall <id>
    ki plugin grant <id> <capability> [flags]...
    ki plugin revoke <id> <capability>

CAPABILITIES:
    net-http      host-mediated HTTP; --host <host> (repeatable, `*` allows any)
    fs            file access; --cwd plugin-data|project (default plugin-data:
                  the plugin's own data directory, or the project tree)
    exec          run a child process; --command <name> (repeatable, `*` allows any),
                  --cwd plugin-data|project (default plugin-data: where the child
                  may start — the plugin's data directory or the project
                  directory), --timeout <seconds> (default 30, at most 300)
    proc          talk to a child process for the length of one tool call
                  (spawn/write/read-line/wait/kill); same --command/--cwd, plus
                  --timeout <seconds> (default 600, at most 1800) and
                  --max-open <n> (default 100)
    net-sockets   raw TCP; --host <host> (repeatable, `*` allows any)

    0.8.0 capabilities (never auto-granted by `--yes`):
    oauth         drive an IdP; --provider <id> --profile <file.json>
                  (repeatable pairs), optional --timeout <seconds>,
                  --max-requests <n>
    provider      serve a model catalog; --model-json <file.json> (repeatable),
                  --endpoint <scheme://host/prefix>, optional --method GET|POST,
                  --credential-from auth-json:<provider-id>, --timeout <seconds>
    render        markdown transform; --kind <kind> (only `markdown` today)
    resources     package skills/prompts; --kind skills|prompts (repeatable)
    provider-request  observe/patch a provider request; --observe, --headers
                  names|values, --messages none|full, --patch,
                  --patch-timeout-ms <n>

`install` copies the component to `plugins/<id>/plugin.wasm`, shows the
capabilities it declares, and grants only what the user approves (`--yes` for a
non-interactive run; without a terminal and without `--yes`, nothing is
granted). oauth/provider/render/resources/provider-request need an explicit
`ki plugin grant` with their policy. `uninstall` moves the directory aside
instead of deleting it.

`show` prints the resolved policy: endpoint hosts, model ids, kinds and budgets,
and a credential only as its handle (with display name/source/fingerprint),
never its value.

A missing grants file grants nothing; the host never authorizes for a plugin."
    );
}

/// The agent root for this invocation: an explicit one (a test, or a caller
/// handed a directory) or the environment's. Resolving it here — rather than
/// reading `KI_AGENT_DIR` deep inside — is what keeps tests from mutating the
/// process environment.
fn resolve_root(root: Option<&Path>) -> Option<PathBuf> {
    root.map(Path::to_path_buf).or_else(agent_dir)
}

/// Capabilities `--yes` never auto-grants.
///
/// Their policy is not derivable from an import name: oauth needs exact IdP
/// profiles, provider needs a catalog, an endpoint and a credential handle,
/// render/resources need kinds and budgets, and provider-request needs the
/// observe/patch switches. A blanket yes would write an empty or wrong policy,
/// so each one requires an explicit
/// `ki plugin grant <id> <capability> ...`. `--yes` behaviour for
/// `net-http`/`fs`/`exec`/`proc`/`net-sockets` is unchanged.
const AUTO_GRANT_EXEMPT: &[&str] = &[
    "oauth",
    "provider",
    "render",
    "resources",
    "provider-request",
];

/// Whether to grant the capabilities an install newly declares.
///
/// Split out because the interactive branch is invisible to CI (no terminal):
/// the decision is the part that matters, and it is testable on its own.
/// Anything short of an explicit `y`/`Y`/`yes` — an empty line, `n`, EOF — is a
/// no, so the default answer is to grant nothing.
fn approve_new_capabilities(yes: bool, interactive: bool, answer: Option<&str>) -> bool {
    if yes {
        return true;
    }
    if !interactive {
        return false;
    }
    matches!(answer.map(str::trim), Some("y" | "Y" | "yes"))
}

/// `ki insmod <path.wasm> [flags]` — the kernel-flavoured spelling of
/// [`install`] (the **wasm** tier; `ki install <crate>` is the cdylib one, so
/// the two hardly-ever-confused verbs get their own name).
pub fn insmod(args: &[String]) -> i32 {
    insmod_into(args, None)
}

/// [`insmod`] against an explicit agent root (the test seam).
fn insmod_into(args: &[String], root: Option<&Path>) -> i32 {
    install_into(args, root)
}

/// `ki rmmod <id>` — the kernel-flavoured spelling of [`uninstall`]. Moves the
/// module's directory aside, exactly as `ki plugin uninstall` does.
pub fn rmmod(args: &[String]) -> i32 {
    rmmod_into(args, None)
}

/// [`rmmod`] against an explicit agent root (the test seam).
fn rmmod_into(args: &[String], root: Option<&Path>) -> i32 {
    uninstall_into(args, root)
}

/// `ki plugin install <path.wasm> [--id ID] [--yes] [--host H]... [--command C]... [--cwd S] [--timeout N]`
fn install(args: &[String]) -> i32 {
    install_into(args, None)
}

/// Build a wasm guest crate and return the component it produced.
///
/// The artifact path is read from cargo's own JSON output rather than guessed
/// from the crate name (a `[lib] name = ...` or a workspace target directory
/// would break the guess). This is the "install a guest" half of the wasm
/// tier: with the plugins that have a guest no longer compiled in, getting one
/// must be one command, not a remembered target path.
///
/// `pub(crate)` so the wasm tests in [`crate::plugins`] build their fixtures the
/// same way: a hard-coded `plugins/wasm/target/...` silently breaks on a
/// machine whose cargo config points `build.target-dir` elsewhere.
/// Serialises `build_guest` inside one test binary.
///
/// Several tests build the *same* guest (four do for `kiqi-conformance-guest`), and
/// they run concurrently under the default `--test-threads`. They share one
/// `cargo --target-dir`, so while one `cargo build` rewrites the artifact another
/// test can be told "already fresh" and open the file mid-write — observed as
/// `failed to read from …/kiqi_conformance_guest.wasm`. The same shape appears across
/// processes when two `cargo test -p kiqi-cli` runs share a target dir.
///
/// The mutex makes the in-binary case deterministic; the retry below covers the
/// cross-process window. Both keep the shared target dir, so `cargo`'s incremental
/// cache still applies — a per-process `--target-dir` would rebuild every guest.
static BUILD_GUEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn build_guest(dir: &Path) -> Result<PathBuf, String> {
    // A poisoned lock means an earlier build panicked; that says nothing about the
    // filesystem, so carry on rather than propagating the panic.
    let _guard = BUILD_GUEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let manifest = dir.join("Cargo.toml");
    if !manifest.is_file() {
        return Err(format!("{} has no Cargo.toml", dir.display()));
    }
    let output = std::process::Command::new(env!("CARGO"))
        .args(["build", "--manifest-path"])
        .arg(&manifest)
        .args([
            "--target",
            "wasm32-wasip2",
            "--release",
            "--message-format=json",
        ])
        .output()
        .map_err(|error| format!("run cargo for {}: {error}", dir.display()))?;
    if !output.status.success() {
        return Err(format!(
            "building {} failed; is the `wasm32-wasip2` target installed?\n{}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    // `cargo build --message-format=json` emits one JSON object per line; the
    // component is the compiler-artifact whose filenames end in `.wasm`.
    let mut artifact = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if message.get("reason").and_then(serde_json::Value::as_str) != Some("compiler-artifact") {
            continue;
        }
        if let Some(found) = message
            .get("filenames")
            .and_then(serde_json::Value::as_array)
            .and_then(|names| {
                names
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .find(|name| name.ends_with(".wasm"))
            })
        {
            artifact = Some(PathBuf::from(found));
        }
    }
    let artifact = artifact.ok_or_else(|| {
        format!(
            "{} built no .wasm component (is it a guest crate: crate-type cdylib, wasm32-wasip2?)",
            dir.display()
        )
    })?;

    // Another process sharing this target dir may have been mid-write when cargo
    // reported the artifact fresh, so the bytes on disk can be a truncated component.
    // Checking the 4-byte module header (`\0asm`) catches that; a plain length check
    // would not, because a half-written file is non-empty.
    if !has_wasm_header(&artifact) {
        let _ = std::process::Command::new(env!("CARGO"))
            .args(["build", "--manifest-path"])
            .arg(&manifest)
            .args(["--target", "wasm32-wasip2", "--release"])
            .output();
    }
    Ok(artifact)
}

/// Whether `path` starts with the WebAssembly module magic (`\0asm`).
///
/// A component that is still being written by another process reads as empty or as a
/// prefix, and loading it fails with a bare "failed to read"; this predicate lets
/// `build_guest` tell "complete artifact" from "torn write".
fn has_wasm_header(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).is_ok() && magic == *b"\0asm"
}

/// Name suffix of the digest sidecar written next to a component.
pub(crate) const DIGEST_SUFFIX: &str = ".sha256";

/// Where a component's digest sidecar lives: `plugin.wasm.sha256` beside
/// `plugin.wasm`.
pub(crate) fn digest_sidecar_path(wasm: &Path) -> PathBuf {
    let mut name = wasm.file_name().unwrap_or_default().to_os_string();
    name.push(DIGEST_SUFFIX);
    wasm.with_file_name(name)
}

/// Lowercase hex SHA-256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn file_sha256_hex(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(sha256_hex(&bytes))
}

/// Record `wasm`'s current digest in its sidecar and return the hex.
pub(crate) fn write_digest_sidecar(wasm: &Path) -> Result<String, String> {
    let hex = file_sha256_hex(wasm)?;
    let path = digest_sidecar_path(wasm);
    std::fs::write(&path, format!("{hex}\n"))
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    Ok(hex)
}

/// The digest recorded in the sidecar, or `None` when there is no sidecar.
fn read_digest_sidecar(wasm: &Path) -> Result<Option<String>, String> {
    let path = digest_sidecar_path(wasm);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some(
            text.lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

/// How a component compares to its recorded digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DigestStatus {
    Ok { recorded: String },
    Mismatch { recorded: String, actual: String },
    Missing,
}

/// Verify a component against its sidecar.
///
/// This is corruption/integrity checking, not anti-tamper: whoever can rewrite
/// `plugin.wasm` can rewrite the sidecar beside it. Detached signatures
/// (provenance) need a distribution channel to be worth anything, so they are
/// deliberately not pretended here.
pub(crate) fn digest_status(wasm: &Path) -> Result<DigestStatus, String> {
    let Some(recorded) = read_digest_sidecar(wasm)? else {
        return Ok(DigestStatus::Missing);
    };
    let actual = file_sha256_hex(wasm)?;
    if recorded == actual {
        Ok(DigestStatus::Ok { recorded })
    } else {
        Ok(DigestStatus::Mismatch { recorded, actual })
    }
}

/// [`install`] against an explicit agent root (`None` = the environment's).
///
/// Taking the root as a parameter is why no test has to write `KI_AGENT_DIR`
/// into the process environment, which is unsafe while other threads read it.
fn install_into(args: &[String], root: Option<&Path>) -> i32 {
    let mut wasm: Option<String> = None;
    let mut id: Option<String> = None;
    let mut yes = false;
    let mut hosts: Vec<String> = Vec::new();
    let mut commands: Vec<String> = Vec::new();
    let mut exec_options = GrantOptions::default();
    let mut build_dir: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--id" => match iter.next() {
                Some(value) => id = Some(value.clone()),
                None => {
                    eprintln!("error: --id needs a value");
                    return 2;
                }
            },
            "--yes" | "-y" => yes = true,
            "--host" => match iter.next() {
                Some(value) => hosts.push(value.clone()),
                None => {
                    eprintln!("error: --host needs a value");
                    return 2;
                }
            },
            "--command" => match iter.next() {
                Some(value) => commands.push(value.clone()),
                None => {
                    eprintln!("error: --command needs a value");
                    return 2;
                }
            },
            "--build" => match iter.next() {
                Some(value) => build_dir = Some(value.clone()),
                None => {
                    eprintln!("error: --build needs a directory");
                    return 2;
                }
            },
            "--cwd" => match iter.next() {
                Some(value) => match ExecCwd::parse(value) {
                    Ok(cwd) => exec_options.cwd = Some(cwd),
                    Err(error) => {
                        eprintln!("error: {error}");
                        return 2;
                    }
                },
                None => {
                    eprintln!("error: --cwd needs a value");
                    return 2;
                }
            },
            "--timeout" => match iter.next() {
                Some(value) => match parse_timeout(value) {
                    Ok(seconds) => exec_options.timeout_seconds = Some(seconds),
                    Err(error) => {
                        eprintln!("error: {error}");
                        return 2;
                    }
                },
                None => {
                    eprintln!("error: --timeout needs a value");
                    return 2;
                }
            },
            "--max-open" => match iter.next() {
                Some(value) => match value.parse::<u32>() {
                    Ok(n) => exec_options.max_open = Some(n),
                    Err(_) => {
                        eprintln!("error: --max-open needs a number, got `{value}`");
                        return 2;
                    }
                },
                None => {
                    eprintln!("error: --max-open needs a value");
                    return 2;
                }
            },
            other if other.starts_with("--") => {
                eprintln!("error: unknown flag `{other}`");
                return 2;
            }
            other if wasm.is_none() => wasm = Some(other.to_string()),
            other => {
                eprintln!("error: unexpected argument `{other}`");
                return 2;
            }
        }
    }
    // `--build <dir>` builds a guest crate and installs what it produced; the
    // positional `.wasm` path is the alternative.
    let built;
    let wasm: &Path = match (&build_dir, &wasm) {
        (Some(dir), _) => match build_guest(Path::new(dir)) {
            Ok(path) => {
                built = path;
                &built
            }
            Err(error) => {
                eprintln!("error: {error}");
                return 1;
            }
        },
        (None, Some(path)) => Path::new(path),
        (None, None) => {
            eprintln!(
                "error: expected `ki insmod <path.wasm> [--id <id>] [--yes] [--host <host>]... [--command <cmd>]... [--cwd <scope>] [--timeout <seconds>]`\n       or `ki insmod --build <guest-crate-dir> [...]`"
            );
            return 2;
        }
    };
    if !wasm.is_file() {
        eprintln!("error: {} is not a file", wasm.display());
        return 2;
    }
    // A package's resources live beside the crate's component, not beside the
    // artifact cargo reports: `--build` passes the guest crate dir so its
    // `resources.json`/`resources/` is what gets installed.
    let resource_source: PathBuf = match &build_dir {
        Some(dir) => PathBuf::from(dir),
        None => wasm
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf(),
    };
    // Validate the manifest, kinds, caps and every file before anything is
    // copied, so a rejected package installs nothing.
    let prepared_resources = match crate::plugins::prepare_package_resources(&resource_source) {
        Ok(prepared) => prepared,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };

    let id = id.unwrap_or_else(|| {
        wasm.file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("plugin")
            .to_string()
    });
    if let Err(error) = validate_id(&id) {
        eprintln!("error: {error}");
        return 2;
    }
    let declared = match declared_capabilities_in(wasm, &resource_source) {
        Ok(caps) => caps,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    let exec_options_given = exec_options.cwd.is_some()
        || exec_options.timeout_seconds.is_some()
        || exec_options.max_open.is_some();
    if exec_options_given && !declared.iter().any(|cap| cap == "exec" || cap == "proc") {
        eprintln!(
            "error: --cwd/--timeout/--max-open apply to `exec` and `proc`, which this component \
             declares neither of"
        );
        return 2;
    }
    // `--timeout` lands on every declared exec/proc, so the tightest of their
    // ceilings is what must hold (proc allows more than exec). A bad value is
    // a hard error here, never the note `grant()`'s re-check would become.
    if let Some(seconds) = exec_options.timeout_seconds {
        let max = declared
            .iter()
            .filter_map(|cap| match cap.as_str() {
                "exec" => Some(kiqi_plugin::grants::EXEC_TIMEOUT_MAX_SECONDS),
                "proc" => Some(kiqi_plugin::grants::PROC_TIMEOUT_MAX_SECONDS),
                _ => None,
            })
            .min()
            .unwrap_or(kiqi_plugin::grants::EXEC_TIMEOUT_MAX_SECONDS);
        if seconds > max {
            eprintln!("error: --timeout must be 1..={max} seconds, got {seconds}");
            return 2;
        }
    }

    let Some(root) = resolve_root(root) else {
        eprintln!("error: cannot resolve the agent directory");
        return 1;
    };
    let dir = plugin_dir_at(&root, &id);
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!("error: create {}: {error}", dir.display());
        return 1;
    }
    let target = dir.join("plugin.wasm");
    if let Err(error) = std::fs::copy(wasm, &target) {
        eprintln!("error: install {}: {error}", target.display());
        return 1;
    }
    // Record the digest of the bytes actually written (the copy), not of the
    // source file, so the sidecar describes what the loader will read.
    match write_digest_sidecar(&target) {
        Ok(hex) => eprintln!("digest: {hex}"),
        Err(error) => {
            eprintln!("error: record digest for {}: {error}", target.display());
            return 1;
        }
    }

    // Copy the verified resource tree beside plugin.wasm and record one
    // digest per file. The manifest itself is copied and digested too.
    if let Some(prepared) = &prepared_resources {
        if let Err(error) = crate::plugins::write_package_resources(&dir, prepared) {
            eprintln!("error: install resources for {}: {error}", dir.display());
            return 1;
        }
    }

    // Install-time provider discovery: bounded, grant-less, and advisory. The
    // GRANT stays the catalog authority; a guest that cannot answer is a
    // diagnostic, not an install failure (`--yes`/fail-closed rules still
    // decide what is granted).
    #[cfg(feature = "plugin-wasm")]
    if declared
        .iter()
        .any(|cap| cap == "oauth" || cap == "provider")
    {
        let discovery = kiqi_plugin_host::discover_providers(&target);
        for diagnostic in &discovery.diagnostics {
            eprintln!("note: {diagnostic}");
        }
        if !discovery.providers.is_empty() {
            let listed = discovery
                .providers
                .iter()
                .map(|provider| format!("{} ({})", provider.id, provider.interface))
                .collect::<Vec<_>>()
                .join(", ");
            println!("providers disclosed by the guest: {listed}");
        }
    }

    // Only capabilities this plugin declares that are not already granted need a
    // decision, so an update that does not grow the set never re-prompts.
    //
    // `resources` is treated exactly like the other declared capabilities: the
    // package's `resources.json` is a declaration, never an authorization, so
    // install must not write a grant for it. It is in `AUTO_GRANT_EXEMPT`, which
    // means a fresh install asks for an explicit `ki plugin grant`. Because an
    // existing policy is left untouched, a reinstall can neither widen a
    // narrowed grant nor resurrect one the user revoked.
    let mut grants = Grants::load_for_at(&root, &id);
    let mut grants_changed = false;
    let already = grants.granted();
    let new_caps: Vec<&String> = declared
        .iter()
        .filter(|cap| !already.contains(&cap.as_str()))
        .collect();
    // `--yes` covers only capabilities whose policy an import name fully
    // describes. The five policy-shaped keys are never auto-granted: they need
    // profiles/endpoints/credentials/kinds that no default can invent.
    let (auto_caps, explicit_caps): (Vec<&String>, Vec<&String>) = new_caps
        .iter()
        .partition(|cap| !AUTO_GRANT_EXEMPT.contains(&cap.as_str()));
    let mut granted_now: Vec<String> = Vec::new();
    if !auto_caps.is_empty() {
        let interactive = std::io::stdin().is_terminal();
        if !yes && interactive {
            print!(
                "grant {} to `{id}`? [y/N] ",
                auto_caps
                    .iter()
                    .map(|cap| cap.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        let answer = if !yes && interactive {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).ok().map(|_| line)
        } else {
            None
        };
        let approve = approve_new_capabilities(yes, interactive, answer.as_deref());
        if approve {
            for cap in &auto_caps {
                // Each capability carries its own policy: hosts for the network
                // capabilities, command names (plus --cwd/--timeout) for exec.
                let (policy, options) = if cap.as_str() == "exec" || cap.as_str() == "proc" {
                    (commands.clone(), exec_options.clone())
                } else {
                    (hosts.clone(), GrantOptions::default())
                };
                match grants.grant(cap, policy, options) {
                    Ok(()) => granted_now.push(cap.to_string()),
                    Err(error) => eprintln!("note: {cap}: {error}"),
                }
            }
            if !granted_now.is_empty() {
                grants_changed = true;
            }
        }
    }
    if grants_changed {
        if let Err(error) = Grants::save_for_at(&root, &id, &grants) {
            eprintln!("error: {error}");
            return 1;
        }
    }
    if !explicit_caps.is_empty() {
        eprintln!(
            "note: {} need an explicit `ki plugin grant {id} <capability> ...` — \
             --yes does not grant them",
            explicit_caps
                .iter()
                .map(|cap| cap.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // `--cwd`/`--timeout` only reach a *newly granted* exec; when exec was
    // already granted, install leaves it alone and the flags would vanish
    // silently. Say so (a component that declares no exec is a hard error,
    // checked before the copy above).
    if exec_options_given
        && already.iter().any(|cap| *cap == "exec")
        && !granted_now.iter().any(|cap| cap == "exec")
    {
        eprintln!(
            "note: --cwd/--timeout were not applied — `exec` was already granted; \
             use `ki plugin grant {id} exec ...` to change its policy"
        );
    }

    println!("installed `{id}` -> {}", target.display());
    if !declared.is_empty() {
        println!("declared capabilities: {}", declared.join(", "));
    }
    // A declared capability this build cannot enforce would otherwise look
    // granted while every call comes back denied. Say so at install time.
    let unenforceable: Vec<&str> = declared
        .iter()
        .map(String::as_str)
        .filter(|capability| !capability_is_enforced(capability))
        .collect();
    if !unenforceable.is_empty() {
        println!(
            "note: this build cannot enforce {} — the component still loads, but those calls \
             return `failed` (not `denied`)",
            unenforceable.join(", ")
        );
    }
    if !granted_now.is_empty() {
        println!("granted: {}", granted_now.join(", "));
    }
    let granted = grants.granted();
    let missing: Vec<&str> = declared
        .iter()
        .map(String::as_str)
        .filter(|cap| !granted.contains(cap))
        .collect();
    if !missing.is_empty() {
        println!(
            "not granted: {} — use `ki plugin grant {id} <capability>`",
            missing.join(", ")
        );
    }
    0
}

/// `ki plugin uninstall <id>` — moves the plugin directory aside rather than
/// deleting it, so the files stay recoverable.
fn uninstall(args: &[String]) -> i32 {
    uninstall_into(args, None)
}

/// [`uninstall`] against an explicit agent root.
fn uninstall_into(args: &[String], root: Option<&Path>) -> i32 {
    let Some(id) = args.first().map(String::as_str) else {
        eprintln!("error: `ki plugin uninstall <id>` needs a plugin id");
        return 2;
    };
    if let Err(error) = validate_id(id) {
        eprintln!("error: {error}");
        return 2;
    }
    let Some(root) = resolve_root(root) else {
        eprintln!("error: cannot resolve the agent directory");
        return 1;
    };
    let dir = plugin_dir_at(&root, id);
    if !dir.is_dir() {
        eprintln!("error: `{id}` is not installed");
        return 1;
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let backup = dir.with_file_name(format!("{id}.removed-{timestamp}"));
    if let Err(error) = std::fs::rename(&dir, &backup) {
        eprintln!("error: move {}: {error}", dir.display());
        return 1;
    }
    // The directory is gone, so nothing can use or re-discover these tokens;
    // leaving them in auth.json would strand live credentials with no CLI path
    // to remove them (the grant that named their provider is gone too).
    let removed = match crate::config::delete_plugin_credentials_at(&root, id) {
        Ok(count) => count,
        Err(error) => {
            eprintln!("error: remove stored credentials for `{id}`: {error}");
            return 1;
        }
    };
    println!("uninstalled `{id}` (moved to {})", backup.display());
    if removed > 0 {
        println!("removed {removed} stored credential(s)");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_without_a_capability_is_a_usage_error() {
        assert_eq!(run(&["grant".into(), "some-plugin".into()]), 2);
    }

    #[test]
    fn unknown_subcommand_is_a_usage_error() {
        assert_eq!(run(&["nope".into()]), 2);
    }

    #[test]
    fn host_flag_without_a_value_is_a_usage_error() {
        let args = vec![
            "p".to_string(),
            "net-http".to_string(),
            "--host".to_string(),
        ];
        assert!(parse_target(&args).is_err());
    }

    /// The exec options parse; bad values and unknown flags are usage errors.
    #[test]
    fn grant_parses_exec_cwd_and_timeout() {
        let args = vec![
            "p".to_string(),
            "exec".to_string(),
            "--command".to_string(),
            "cargo".to_string(),
            "--cwd".to_string(),
            "project".to_string(),
            "--timeout".to_string(),
            "120".to_string(),
        ];
        let target = parse_target(&args).expect("parse");
        assert_eq!(target.policy, vec!["cargo".to_string()]);
        assert_eq!(target.options.cwd, Some(ExecCwd::Project));
        assert_eq!(target.options.timeout_seconds, Some(120));

        for bad in [
            vec!["p".to_string(), "exec".to_string(), "--cwd".to_string()],
            vec![
                "p".to_string(),
                "exec".to_string(),
                "--cwd".to_string(),
                "elsewhere".to_string(),
            ],
            vec![
                "p".to_string(),
                "exec".to_string(),
                "--timeout".to_string(),
                "soon".to_string(),
            ],
            // The budget's bounds are a parse error, on the same layer as a
            // bad --cwd value.
            vec![
                "p".to_string(),
                "exec".to_string(),
                "--timeout".to_string(),
                "0".to_string(),
            ],
            vec![
                "p".to_string(),
                "exec".to_string(),
                "--timeout".to_string(),
                "301".to_string(),
            ],
        ] {
            assert!(parse_target(&bad).is_err(), "{bad:?}");
        }
    }

    /// The `--timeout` ceiling is per-capability: `proc` accepts its own, longer
    /// budget (default 600, at most 1800) while `exec` keeps its 300s cap.
    #[test]
    fn proc_and_exec_timeouts_use_their_own_caps() {
        let proc_args = |seconds: &str| {
            vec![
                "p".to_string(),
                "proc".to_string(),
                "--command".to_string(),
                "sh".to_string(),
                "--timeout".to_string(),
                seconds.to_string(),
            ]
        };
        let target = parse_target(&proc_args("600")).expect("600 is proc's default budget");
        assert_eq!(target.options.timeout_seconds, Some(600));
        assert!(
            parse_target(&proc_args("1800")).is_ok(),
            "proc's cap is inclusive"
        );
        assert!(parse_target(&proc_args("1801")).is_err(), "past proc's cap");
        assert!(
            parse_target(&proc_args("0")).is_err(),
            "zero is rejected everywhere"
        );

        let exec_args = vec![
            "p".to_string(),
            "exec".to_string(),
            "--timeout".to_string(),
            "301".to_string(),
        ];
        assert!(parse_target(&exec_args).is_err(), "exec keeps its 300s cap");
    }

    /// Re-granting exec without the flags resets a previously granted
    /// scope/budget — that must produce a note, and only when the flags were
    /// actually unstated.
    #[test]
    fn regrant_reset_is_announced() {
        use kiqi_plugin::grants::ExecGrant;

        let old = ExecGrant {
            commands: vec!["cargo".to_string()],
            cwd: ExecCwd::Project,
            timeout_seconds: 120,
        };
        let fresh = ExecGrant {
            commands: vec!["git".to_string()],
            ..ExecGrant::default()
        };
        let note = exec_reset_note(&old, &GrantOptions::default(), &fresh)
            .expect("a silent reset must be announced");
        assert!(note.contains("project") && note.contains("120"));

        // Restated explicitly: no note.
        let kept = GrantOptions {
            cwd: Some(ExecCwd::Project),
            timeout_seconds: Some(120),
            max_open: None,
        };
        assert!(exec_reset_note(&old, &kept, &old.clone()).is_none());
        // Nothing non-default to lose: no note.
        let plain_old = ExecGrant {
            commands: vec!["echo".to_string()],
            ..ExecGrant::default()
        };
        assert!(exec_reset_note(&plain_old, &GrantOptions::default(), &fresh).is_none());
    }

    /// The fs scope rides the same `--cwd` flag: default stays the data jail,
    /// `project` opts into the project tree, and the JSON round-trips.
    #[test]
    fn grant_fs_with_cwd_scope() {
        let args = vec![
            "p".to_string(),
            "fs".to_string(),
            "--cwd".to_string(),
            "project".to_string(),
        ];
        let target = parse_target(&args).expect("parse");
        assert_eq!(target.options.cwd, Some(ExecCwd::Project));

        let mut grants = Grants::default();
        grants
            .grant("fs", vec![], target.options)
            .expect("fs takes --cwd");
        assert_eq!(
            serde_json::to_value(&grants).unwrap()["fs"],
            serde_json::json!({"cwd": "project"})
        );
    }

    /// `revoke` clears a whole capability; flags would be silently ignored, so
    /// they are a usage error instead.
    #[test]
    fn revoke_rejects_flags() {
        assert_eq!(
            run(&[
                "revoke".into(),
                "p".into(),
                "exec".into(),
                "--cwd".into(),
                "project".into(),
            ]),
            2
        );
        assert_eq!(
            run(&[
                "revoke".into(),
                "p".into(),
                "net-http".into(),
                "--host".into(),
                "x".into()
            ]),
            2
        );
    }

    /// `--cwd`/`--timeout` on a component that does not declare `exec` are a
    /// hard error before anything is copied — never silently dropped.
    #[cfg(feature = "plugin-wasm")]
    #[test]
    fn install_rejects_exec_flags_for_a_component_without_exec() {
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let wasm = build_guest(&workspace.join("plugins/wasm/kiqi-webfetch-guest"))
            .expect("build the webfetch guest");

        let agent = tempfile::tempdir().unwrap();
        let code = install_into(
            &[
                wasm.display().to_string(),
                "--id".into(),
                "wf".into(),
                "--yes".into(),
                "--host".into(),
                "example.com".into(),
                "--cwd".into(),
                "project".into(),
            ],
            Some(agent.path()),
        );
        assert_eq!(code, 2, "--cwd without a declared exec is an error");
        assert!(
            !agent.path().join("plugins/wf/plugin.wasm").exists(),
            "nothing is installed when the flags are rejected"
        );
    }

    /// Installing copies the component, grants `--yes` only what the component
    /// declares, and uninstalling moves the directory aside.
    ///
    /// Runs against an explicit root rather than by writing `KI_AGENT_DIR`:
    /// mutating the process environment is unsafe while other test threads read
    /// it, and a lock only serializes writers, not readers.
    #[cfg(feature = "plugin-wasm")]
    #[test]
    fn install_and_uninstall_round_trip() {
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let wasm = build_guest(&workspace.join("plugins/wasm/kiqi-conformance-guest"))
            .expect("build the conformance guest");

        let agent = tempfile::tempdir().unwrap();
        let root = agent.path();

        let code = install_into(
            &[
                wasm.display().to_string(),
                "--id".into(),
                "conformance".into(),
                "--yes".into(),
                "--host".into(),
                "127.0.0.1".into(),
                "--command".into(),
                "echo".into(),
                "--cwd".into(),
                "project".into(),
                "--timeout".into(),
                "120".into(),
            ],
            Some(root),
        );
        assert_eq!(code, 0);
        let installed = root.join("plugins/conformance/plugin.wasm");
        assert!(installed.is_file());
        // The sidecar records the digest of the bytes actually installed.
        let sidecar = std::fs::read_to_string(digest_sidecar_path(&installed))
            .expect("install writes plugin.wasm.sha256");
        let expected: String = Sha256::digest(std::fs::read(&installed).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(sidecar.trim(), expected, "one lowercase-hex line");
        let grants = Grants::load_for_at(root, "conformance");
        assert_eq!(
            grants.granted(),
            vec!["net-http", "fs", "exec", "proc", "net-sockets"]
        );
        assert!(grants.allows_host("127.0.0.1"));
        assert!(grants.allows_socket_host("127.0.0.1"));
        assert!(grants.allows_command("echo"));
        let exec = grants.exec.as_ref().expect("exec was granted");
        assert_eq!(exec.cwd, kiqi_plugin::grants::ExecCwd::Project);
        assert_eq!(exec.timeout_seconds, 120);

        let code = uninstall_into(&["conformance".into()], Some(root));
        assert_eq!(code, 0);
        assert!(!root.join("plugins/conformance").exists());
        let moved = std::fs::read_dir(root.join("plugins"))
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .into_string()
                    .map(|name| name.starts_with("conformance.removed-"))
                    .unwrap_or(false)
            });
        assert!(moved, "uninstall keeps the files as a backup");
    }

    /// `ki insmod`/`ki rmmod` are the kernel-flavoured spellings: they must
    /// install and uninstall exactly as `ki plugin install`/`uninstall` do, with
    /// the same flags and the same move-aside semantics.
    #[cfg(feature = "plugin-wasm")]
    #[test]
    fn insmod_and_rmmod_round_trip() {
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let wasm = build_guest(&workspace.join("plugins/wasm/kiqi-conformance-guest"))
            .expect("build the conformance guest");

        let agent = tempfile::tempdir().unwrap();
        let root = agent.path();

        let code = insmod_into(
            &[
                wasm.display().to_string(),
                "--id".into(),
                "mod".into(),
                "--yes".into(),
                "--host".into(),
                "127.0.0.1".into(),
                "--command".into(),
                "echo".into(),
                "--cwd".into(),
                "project".into(),
                "--timeout".into(),
                "120".into(),
            ],
            Some(root),
        );
        assert_eq!(code, 0);
        assert!(root.join("plugins/mod/plugin.wasm").is_file());
        let grants = Grants::load_for_at(root, "mod");
        assert!(grants.allows_command("echo"));
        let exec = grants.exec.as_ref().expect("exec granted");
        assert_eq!(exec.cwd, ExecCwd::Project);
        assert_eq!(exec.timeout_seconds, 120);

        let code = rmmod_into(&["mod".into()], Some(root));
        assert_eq!(code, 0);
        assert!(!root.join("plugins/mod").exists());
        let moved = std::fs::read_dir(root.join("plugins"))
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .into_string()
                    .map(|name| name.starts_with("mod.removed-"))
                    .unwrap_or(false)
            });
        assert!(
            moved,
            "rmmod moves the module aside rather than deleting it"
        );
    }

    /// `--build <guest-dir>` compiles the guest and installs the component
    /// cargo reported — the one-command path back for a plugin whose compiled
    /// version is no longer in the default build.
    #[cfg(feature = "plugin-wasm")]
    #[test]
    fn insmod_builds_a_guest_and_installs_it() {
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let guest = workspace.join("plugins/wasm/kiqi-conformance-guest");
        let agent = tempfile::tempdir().unwrap();
        let root = agent.path();

        let code = insmod_into(
            &[
                "--build".into(),
                guest.display().to_string(),
                "--id".into(),
                "built".into(),
                "--yes".into(),
                "--host".into(),
                "127.0.0.1".into(),
                "--command".into(),
                "echo".into(),
            ],
            Some(root),
        );
        assert_eq!(code, 0, "--build compiles the guest and installs it");
        assert!(
            root.join("plugins/built/plugin.wasm").is_file(),
            "the component cargo built landed in the plugin store"
        );
        assert!(Grants::load_for_at(root, "built").allows_command("echo"));

        // A directory that is not a guest crate is an error, not a silent no-op.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            insmod_into(
                &[
                    "--build".into(),
                    empty.path().display().to_string(),
                    "--id".into(),
                    "nope".into(),
                ],
                Some(root),
            ),
            1
        );
    }

    /// The interactive answer path — unreachable in CI, which has no terminal —
    /// decided on its own. Only an explicit yes grants; the default is no.
    #[test]
    fn install_grants_only_on_an_explicit_yes() {
        assert!(approve_new_capabilities(true, false, None), "--yes");
        assert!(
            !approve_new_capabilities(false, false, None),
            "headless without --yes"
        );
        for answer in ["y\n", "Y\n", "yes\n", "  y  "] {
            assert!(
                approve_new_capabilities(false, true, Some(answer)),
                "{answer:?}"
            );
        }
        for answer in ["\n", "n\n", "no\n", "nah\n"]
            .into_iter()
            .map(Some)
            .chain([None])
        {
            assert!(!approve_new_capabilities(false, true, answer), "{answer:?}");
        }
    }

    /// The sidecar is the exact bytes' SHA-256, lowercase hex, one line.
    #[test]
    fn digest_sidecar_records_sha256_of_the_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let wasm = dir.path().join("plugin.wasm");
        let bytes = b"\0asm not really a component";
        std::fs::write(&wasm, bytes).unwrap();

        let recorded = write_digest_sidecar(&wasm).unwrap();
        let expected: String = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(recorded, expected);
        let text = std::fs::read_to_string(digest_sidecar_path(&wasm)).unwrap();
        assert_eq!(text.trim(), expected);
        assert_eq!(text.lines().count(), 1, "one line, no extras");
        assert_eq!(digest_status(&wasm).unwrap(), DigestStatus::Ok { recorded });
    }

    /// A changed byte is a mismatch; a file with no sidecar is `Missing`.
    #[test]
    fn digest_status_detects_mismatch_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let wasm = dir.path().join("plugin.wasm");
        std::fs::write(&wasm, b"component").unwrap();
        assert_eq!(digest_status(&wasm).unwrap(), DigestStatus::Missing);

        let recorded = write_digest_sidecar(&wasm).unwrap();
        std::fs::write(&wasm, b"tampered").unwrap();
        assert_eq!(
            digest_status(&wasm).unwrap(),
            DigestStatus::Mismatch {
                recorded,
                actual: sha256_hex(b"tampered"),
            }
        );

        std::fs::remove_file(digest_sidecar_path(&wasm)).unwrap();
        assert_eq!(digest_status(&wasm).unwrap(), DigestStatus::Missing);
    }

    /// `ki plugin show` states which of the three cases applies, and names both
    /// digests when they disagree.
    #[test]
    fn digest_report_states_ok_mismatch_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let wasm = dir.path().join("plugin.wasm");
        std::fs::write(&wasm, b"component").unwrap();
        assert!(digest_report(&wasm).contains("missing sidecar"));

        let recorded = write_digest_sidecar(&wasm).unwrap();
        let ok = digest_report(&wasm);
        assert!(ok.contains(&recorded) && ok.contains("ok"), "{ok}");

        std::fs::write(&wasm, b"tampered").unwrap();
        let bad = digest_report(&wasm);
        assert!(bad.contains("mismatch"), "{bad}");
        assert!(bad.contains(&recorded), "{bad} names the recorded digest");
        assert!(
            bad.contains(&sha256_hex(b"tampered")),
            "{bad} names the actual digest"
        );
    }

    /// `ki plugin show` is the consumer for the `flags`/`shortcuts`
    /// declarations: they have no runtime path of their own (a flag's value is
    /// read through `cli_flag`, a key arrives as an `Input` event), so this is
    /// where they become observable.
    #[cfg(feature = "plugin-wasm")]
    #[test]
    fn declared_surfaces_include_the_guests_flags_and_shortcuts() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let wasm = build_guest(&root.join("plugins/wasm/kiqi-conformance-guest"))
            .expect("build the conformance guest");
        let plugin = kiqi_plugin_host::WasmPlugin::load_with_grants(
            "conformance",
            &wasm,
            kiqi_plugin::grants::Grants::default(),
        )
        .expect("load the conformance guest");

        let text = declared_surfaces(&plugin);
        for expected in [
            "commands:",
            "flags:",
            "--conformance-flag",
            "shortcuts:",
            "f8",
            "probe points:",
            "before-tool-call",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
    }
    // -----------------------------------------------------------------
    // 0.8.0 capabilities: scanning, explicit grants, consent, disclosure
    // -----------------------------------------------------------------

    fn push_section(out: &mut Vec<u8>, id: u8, contents: &[u8]) {
        out.push(id);
        push_leb(out, contents.len() as u32);
        out.extend_from_slice(contents);
    }

    fn push_leb(out: &mut Vec<u8>, mut value: u32) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
    }

    fn push_name(out: &mut Vec<u8>, name: &str) {
        out.push(0x00);
        push_leb(out, name.len() as u32);
        out.extend_from_slice(name.as_bytes());
    }

    /// A minimal, deliberately non-instantiable component carrying the given
    /// import/export names. The scanners read names; install must still work
    /// when discovery fails closed.
    fn synthetic_component(imports: &[&str], exports: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"\0asm");
        out.extend_from_slice(&[0x0d, 0x00, 0x01, 0x00]);
        if !imports.is_empty() {
            let mut body = Vec::new();
            push_leb(&mut body, imports.len() as u32);
            for name in imports {
                push_name(&mut body, name);
                body.push(0x05);
                push_leb(&mut body, 0);
            }
            push_section(&mut out, 10, &body);
        }
        if !exports.is_empty() {
            let mut body = Vec::new();
            push_leb(&mut body, exports.len() as u32);
            for name in exports {
                push_name(&mut body, name);
                body.push(0x05);
                push_leb(&mut body, 0);
                body.push(0x00);
            }
            push_section(&mut out, 11, &body);
        }
        out
    }

    fn write_provider_fixtures(root: &Path, dir: &Path) -> (PathBuf, PathBuf) {
        let model = dir.join("model.json");
        std::fs::write(
            &model,
            r#"{"id":"claude-sonnet","api":"anthropic-messages","base-url":"https://api.example.com"}"#,
        )
        .unwrap();
        let profile = dir.join("oauth.json");
        std::fs::write(
            &profile,
            r#"{
  "id": "acme",
  "display-name": "Acme",
  "scopes": ["read"],
  "flows": ["authorization-code", "device-code"],
  "authorization-endpoint": "https://auth.example.com/authorize",
  "token-endpoint": "https://auth.example.com/token",
  "device-authorization-endpoint": "https://auth.example.com/device",
  "client-id": "client-123",
  "supports-pkce": true
}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("auth.json"),
            r#"{"anthropic":{"type":"api_key","key":"sk-live-super-secret"}}"#,
        )
        .unwrap();
        (model, profile)
    }

    /// The five policy-shaped keys are exempt from `--yes`; the old five are
    /// not. A `--yes` install of a component exporting `oauth-api` and
    /// importing `provider-http` therefore declares both and grants neither.
    #[cfg(feature = "plugin-wasm")]
    #[test]
    fn install_yes_never_auto_grants_the_policy_shaped_keys() {
        for capability in [
            "oauth",
            "provider",
            "render",
            "resources",
            "provider-request",
        ] {
            assert!(
                AUTO_GRANT_EXEMPT.contains(&capability),
                "{capability} must require an explicit grant"
            );
        }

        let source = tempfile::tempdir().unwrap();
        let wasm = source.path().join("plugin.wasm");
        std::fs::write(
            &wasm,
            synthetic_component(
                &["kiqi:plugin/provider-http@0.8.0"],
                &["kiqi:plugin/oauth-api@0.8.0"],
            ),
        )
        .unwrap();
        let agent = tempfile::tempdir().unwrap();
        let code = install_into(
            &[
                wasm.display().to_string(),
                "--id".into(),
                "prov".into(),
                "--yes".into(),
            ],
            Some(agent.path()),
        );
        assert_eq!(code, 0, "discovery failure must not fail the install");
        assert!(
            Grants::load_for_at(agent.path(), "prov")
                .granted()
                .is_empty(),
            "--yes must not grant oauth/provider/render/resources/provider-request"
        );
    }

    /// Each 0.8.0 key has a policy-shaped spelling; the resolved policy lands
    /// in grants.json (and the credential only as a handle).
    #[test]
    fn grant_spellings_build_the_policy_shaped_keys() {
        let agent = tempfile::tempdir().unwrap();
        let fixtures = tempfile::tempdir().unwrap();
        let root = agent.path();
        let (model, profile) = write_provider_fixtures(root, fixtures.path());

        let steps: Vec<Vec<String>> = vec![
            vec![
                "p".into(),
                "oauth".into(),
                "--provider".into(),
                "acme".into(),
                "--profile".into(),
                profile.display().to_string(),
            ],
            vec![
                "p".into(),
                "provider".into(),
                "--model-json".into(),
                model.display().to_string(),
                "--endpoint".into(),
                "https://api.example.com/v1".into(),
                "--credential-from".into(),
                "auth-json:anthropic".into(),
                "--timeout".into(),
                "300".into(),
            ],
            vec![
                "p".into(),
                "render".into(),
                "--kind".into(),
                "markdown".into(),
            ],
            vec![
                "p".into(),
                "resources".into(),
                "--kind".into(),
                "skills".into(),
            ],
            vec![
                "p".into(),
                "provider-request".into(),
                "--observe".into(),
                "--headers".into(),
                "names".into(),
                "--messages".into(),
                "none".into(),
                "--patch".into(),
                "--patch-timeout-ms".into(),
                "2000".into(),
            ],
        ];
        for (index, step) in steps.iter().enumerate() {
            assert_eq!(
                grant_into(step, Some(root)),
                0,
                "step {index} should grant:\n{}",
                step.join(" ")
            );
        }

        let grants = Grants::load_for_at(root, "p");
        assert_eq!(
            grants.granted(),
            vec![
                "oauth",
                "provider",
                "render",
                "resources",
                "provider-request"
            ]
        );
        let provider = grants.provider.as_ref().expect("provider grant");
        assert_eq!(provider.providers[0].id, "p");
        assert_eq!(provider.providers[0].endpoint.host, "api.example.com");
        assert_eq!(provider.providers[0].endpoint.path_prefix, "/v1");
        assert_eq!(provider.providers[0].timeout_seconds, 300);
        let handle = provider.providers[0]
            .credential
            .as_ref()
            .expect("a handle was minted");
        assert!(handle.starts_with("pc-"));

        let oauth = grants.oauth.as_ref().expect("oauth grant");
        assert_eq!(oauth.providers[0].id, "acme");
        assert!(oauth.providers[0].supports_pkce);
        assert_eq!(
            grants.render.as_ref().unwrap().kinds,
            vec!["markdown".to_string()]
        );
        assert_eq!(
            grants.resources.as_ref().unwrap().kinds,
            vec!["skills".to_string()]
        );
        let request = grants.provider_request.as_ref().expect("request grant");
        assert_eq!(request.observe, ObserveMode::Requests);
        assert!(request.patch);
    }

    /// A mismatched `--provider` / profile id is a usage error, not a silently
    /// renamed profile.
    #[test]
    fn grant_oauth_rejects_a_profile_id_mismatch() {
        let agent = tempfile::tempdir().unwrap();
        let fixtures = tempfile::tempdir().unwrap();
        let (_, profile) = write_provider_fixtures(agent.path(), fixtures.path());
        let code = grant_into(
            &[
                "p".into(),
                "oauth".into(),
                "--provider".into(),
                "other".into(),
                "--profile".into(),
                profile.display().to_string(),
            ],
            Some(agent.path()),
        );
        assert_eq!(code, 2);
    }

    /// `--credential-from` stores a handle plus a fingerprint, never the value.
    #[test]
    fn credential_handle_sidecar_never_contains_the_value() {
        let agent = tempfile::tempdir().unwrap();
        let fixtures = tempfile::tempdir().unwrap();
        let _ = write_provider_fixtures(agent.path(), fixtures.path());
        let handle = mint_credential_handle(agent.path(), "p", "auth-json:anthropic").unwrap();
        assert!(handle.starts_with("pc-"));
        let sidecar = std::fs::read_to_string(credentials_path_at(agent.path(), "p")).unwrap();
        assert!(sidecar.contains(&handle));
        assert!(!sidecar.contains("sk-live-super-secret"));
    }

    /// `show`'s policy section names the credential handle and its display
    /// name/endpoint, and never the value.
    #[test]
    fn show_discloses_the_handle_but_never_the_value() {
        let request = ProviderRequestGrant {
            observe: ObserveMode::Requests,
            ..ProviderRequestGrant::default()
        };
        let mut grants = Grants::default();
        grants
            .grant_provider(ProviderGrant {
                providers: vec![ProviderProfile {
                    id: "p".to_string(),
                    display_name: "P".to_string(),
                    models: vec![ProviderModel {
                        id: "claude-sonnet".to_string(),
                        api: "anthropic-messages".to_string(),
                        base_url: "https://api.example.com".to_string(),
                        reasoning: false,
                        input: Vec::new(),
                    }],
                    endpoint: ProviderEndpoint {
                        scheme: "https".to_string(),
                        host: "api.example.com".to_string(),
                        path_prefix: "/v1".to_string(),
                        methods: vec!["POST".to_string()],
                    },
                    credential: Some("pc-0123456789abcdef".to_string()),
                    auth: None,
                    timeout_seconds: 600,
                    max_response_bytes: kiqi_plugin::grants::PROVIDER_MAX_RESPONSE_BYTES_DEFAULT,
                    max_handles: kiqi_plugin::grants::PROVIDER_MAX_HANDLES_DEFAULT,
                }],
            })
            .unwrap();
        grants.grant_provider_request(request).unwrap();

        let mut credentials = CredentialSidecar::default();
        credentials.insert(
            "pc-0123456789abcdef".to_string(),
            CredentialRecord {
                source: "auth-json:anthropic".to_string(),
                provider: "anthropic".to_string(),
                display: "Anthropic".to_string(),
                fingerprint: "deadbeefdeadbeefdeadbeefdeadbeef".to_string(),
            },
        );

        let text = policy_disclosure(&grants, &credentials);
        for expected in [
            "provider policy:",
            "api.example.com",
            "/v1",
            "claude-sonnet",
            "pc-0123456789abcdef",
            "Anthropic",
            "deadbeefdeadbeef",
            "provider-request policy:",
            "observe: requests",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
        assert!(
            !text.contains("sk-live-super-secret"),
            "the credential value must never be printed:\n{text}"
        );
    }
    /// Write a minimal package that declares `kinds` and ships one skill.
    fn write_resources_package(dir: &std::path::Path, kinds: &str) {
        std::fs::write(dir.join("plugin.wasm"), synthetic_component(&[], &[])).unwrap();
        std::fs::create_dir_all(dir.join("resources/skills/demo")).unwrap();
        std::fs::write(
            dir.join("resources.json"),
            format!(r#"{{"kinds":{kinds},"max-items":4,"max-bytes":4096}}"#),
        )
        .unwrap();
        std::fs::write(
            dir.join("resources/skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: demo\n---\nbody\n",
        )
        .unwrap();
    }

    fn install_resources_package(
        source: &std::path::Path,
        root: &std::path::Path,
        id: &str,
    ) -> i32 {
        install_into(
            &[
                source.join("plugin.wasm").display().to_string(),
                "--id".into(),
                id.into(),
                "--yes".into(),
            ],
            Some(root),
        )
    }

    /// The package manifest is a declaration, not an authorization: a
    /// non-interactive `--yes` install copies the tree but records no
    /// `resources` grant. `resources` is in `AUTO_GRANT_EXEMPT`, so only an
    /// explicit `ki plugin grant` authorizes it.
    #[cfg(feature = "plugin-wasm")]
    #[test]
    fn install_without_consent_leaves_no_resources_grant() {
        let source = tempfile::tempdir().unwrap();
        write_resources_package(source.path(), r#"["skills"]"#);
        let agent = tempfile::tempdir().unwrap();
        let root = agent.path();

        assert_eq!(install_resources_package(source.path(), root, "res"), 0);

        let package = root.join("plugins/res");
        assert!(package.join("resources/skills/demo/SKILL.md").is_file());
        assert!(package.join("resources.json").is_file());
        let digests = std::fs::read_to_string(package.join("resources.sha256")).unwrap();
        assert_eq!(
            digests.lines().count(),
            2,
            "manifest + one resource: {digests}"
        );

        let grants = Grants::load_for_at(root, "res");
        assert!(
            grants.resources.is_none(),
            "install must not write a grant the user did not approve: {:?}",
            grants.resources
        );
    }

    /// An explicit grant is the authorization; only then does the verified
    /// tree resolve into the session.
    #[cfg(feature = "plugin-wasm")]
    #[test]
    fn explicit_resources_grant_resolves_the_installed_tree() {
        let source = tempfile::tempdir().unwrap();
        write_resources_package(source.path(), r#"["skills"]"#);
        let agent = tempfile::tempdir().unwrap();
        let root = agent.path();

        assert_eq!(install_resources_package(source.path(), root, "res"), 0);
        assert_eq!(
            grant_into(
                &[
                    "res".into(),
                    "resources".into(),
                    "--kind".into(),
                    "skills".into()
                ],
                Some(root)
            ),
            0
        );

        let package = root.join("plugins/res");
        let grants = Grants::load_for_at(root, "res");
        let resources = grants.resources.as_ref().expect("resources granted");
        assert_eq!(resources.kinds, vec!["skills"]);
        // The explicit grant carries the CLI's budgets, not the package's
        // declared ones: the manifest is not the authorization.
        assert_eq!(
            resources.max_items,
            kiqi_plugin::grants::RESOURCES_MAX_ITEMS_DEFAULT
        );
        assert_eq!(
            resources.max_bytes,
            kiqi_plugin::grants::RESOURCES_MAX_BYTES_DEFAULT
        );

        let resolved = crate::plugins::resolve_package_resources(&package, resources)
            .expect("the install output resolves");
        assert_eq!(resolved.skills, vec![package.join("resources/skills")]);
        assert!(resolved.prompts.is_empty(), "only skills was granted");

        // Integrity: tampering after install drops the whole package.
        std::fs::write(package.join("resources/skills/demo/SKILL.md"), "tampered").unwrap();
        assert!(crate::plugins::resolve_package_resources(&package, resources).is_err());
    }

    /// Revoking a resources grant and reinstalling the same package must not
    /// resurrect it: the manifest is declaration only, never consent.
    #[cfg(feature = "plugin-wasm")]
    #[test]
    fn reinstall_after_revoke_keeps_resources_revoked() {
        let source = tempfile::tempdir().unwrap();
        write_resources_package(source.path(), r#"["skills"]"#);
        let agent = tempfile::tempdir().unwrap();
        let root = agent.path();

        assert_eq!(install_resources_package(source.path(), root, "res"), 0);
        assert_eq!(
            grant_into(
                &[
                    "res".into(),
                    "resources".into(),
                    "--kind".into(),
                    "skills".into()
                ],
                Some(root)
            ),
            0
        );
        assert!(Grants::load_for_at(root, "res").resources.is_some());
        assert_eq!(
            revoke_into(&["res".into(), "resources".into()], Some(root)),
            0
        );
        assert!(Grants::load_for_at(root, "res").resources.is_none());

        assert_eq!(install_resources_package(source.path(), root, "res"), 0);
        assert!(
            Grants::load_for_at(root, "res").resources.is_none(),
            "a reinstall must not resurrect a revoked grant"
        );
    }

    /// A reinstall whose manifest grows the declared kinds must not widen a
    /// grant the user narrowed: the existing policy is left untouched.
    #[cfg(feature = "plugin-wasm")]
    #[test]
    fn reinstall_does_not_widen_a_narrowed_resources_grant() {
        let source = tempfile::tempdir().unwrap();
        write_resources_package(source.path(), r#"["skills"]"#);
        let agent = tempfile::tempdir().unwrap();
        let root = agent.path();

        assert_eq!(install_resources_package(source.path(), root, "res"), 0);
        assert_eq!(
            grant_into(
                &[
                    "res".into(),
                    "resources".into(),
                    "--kind".into(),
                    "skills".into()
                ],
                Some(root)
            ),
            0
        );

        // The package now declares both kinds; reinstall must leave the
        // narrowed `skills`-only grant exactly as the user set it.
        write_resources_package(source.path(), r#"["skills","prompts"]"#);
        assert_eq!(install_resources_package(source.path(), root, "res"), 0);
        let grants = Grants::load_for_at(root, "res");
        assert_eq!(
            grants.resources.as_ref().expect("still granted").kinds,
            vec!["skills"],
            "a reinstall must not widen the user's grant"
        );
    }

    /// A transport that never reaches the IdP, for the "revocation failed"
    /// branch.
    #[cfg(feature = "plugin-wasm")]
    #[derive(Debug)]
    struct FailingTransport;

    #[cfg(feature = "plugin-wasm")]
    fn stored_profile_fingerprint(root: &Path, install_id: &str, provider_id: &str) -> String {
        let grants = Grants::load_for_at(root, install_id);
        let profile = grants
            .oauth
            .as_ref()
            .and_then(|grant| {
                grant
                    .providers
                    .iter()
                    .find(|profile| profile.id == provider_id)
            })
            .expect("granted OAuth profile");
        kiqi_plugin_host::oauth_profile_fingerprint(profile).unwrap()
    }

    #[cfg(feature = "plugin-wasm")]
    impl kiqi_plugin_host::OAuthTransport for FailingTransport {
        fn post_form(
            &self,
            _url: &str,
            _form: &[(String, String)],
            _timeout: std::time::Duration,
        ) -> Result<kiqi_plugin_host::HttpResponse, String> {
            Err("connection refused".to_string())
        }
    }

    /// `revoke oauth` clears the grant *and* the namespaced token; a bare
    /// provider credential is untouched. (A profile without a revocation
    /// endpoint makes no network call.)
    #[cfg(feature = "plugin-wasm")]
    #[test]
    fn revoke_oauth_deletes_the_namespaced_credential() {
        let agent = tempfile::tempdir().unwrap();
        let fixtures = tempfile::tempdir().unwrap();
        let root = agent.path();
        let (_, profile) = write_provider_fixtures(root, fixtures.path());
        assert_eq!(
            grant_into(
                &[
                    "acme".into(),
                    "oauth".into(),
                    "--provider".into(),
                    "acme".into(),
                    "--profile".into(),
                    profile.display().to_string(),
                ],
                Some(root)
            ),
            0
        );

        let mut store = crate::config::read_auth_at(root).unwrap();
        store.insert(
            crate::config::plugin_credential_key("acme", "acme"),
            crate::config::Credential::Oauth {
                access: "at-secret".to_string(),
                refresh: "rt-secret".to_string(),
                expires: 123,
                profile_fingerprint: Some(stored_profile_fingerprint(root, "acme", "acme")),
            },
        );
        crate::config::write_auth_at(root, &store).unwrap();

        assert_eq!(revoke_into(&["acme".into(), "oauth".into()], Some(root)), 0);

        let grants = Grants::load_for_at(root, "acme");
        assert!(grants.oauth.is_none(), "the grant is cleared");
        let after = crate::config::read_auth_at(root).unwrap();
        assert!(
            after
                .get(&crate::config::plugin_credential_key("acme", "acme"))
                .is_none(),
            "the namespaced token must be gone: {after:?}"
        );
        assert!(
            after.contains_key("anthropic"),
            "a bare provider credential is never touched: {after:?}"
        );
    }

    #[test]
    fn revoke_oauth_preserves_a_non_oauth_namespaced_credential() {
        let agent = tempfile::tempdir().unwrap();
        let fixtures = tempfile::tempdir().unwrap();
        let root = agent.path();
        let (_, profile) = write_provider_fixtures(root, fixtures.path());
        assert_eq!(
            grant_into(
                &[
                    "acme".into(),
                    "oauth".into(),
                    "--provider".into(),
                    "acme".into(),
                    "--profile".into(),
                    profile.display().to_string(),
                ],
                Some(root)
            ),
            0
        );
        let key = crate::config::plugin_credential_key("acme", "acme");
        let mut store = crate::config::read_auth_at(root).unwrap();
        store.insert(
            key.clone(),
            crate::config::Credential::ApiKey {
                key: Some("sk-user".to_string()),
                env: None,
            },
        );
        crate::config::write_auth_at(root, &store).unwrap();

        assert_eq!(revoke_into(&["acme".into(), "oauth".into()], Some(root)), 0);
        assert!(Grants::load_for_at(root, "acme").oauth.is_none());
        assert!(matches!(
            crate::config::read_auth_at(root).unwrap().get(&key),
            Some(crate::config::Credential::ApiKey { key, .. })
                if key.as_deref() == Some("sk-user")
        ));
    }

    /// A failed IdP revocation still removes the local credential, and the
    /// failure is reported rather than swallowed.
    #[cfg(feature = "plugin-wasm")]
    #[test]
    fn revoke_oauth_failure_still_removes_the_local_credential() {
        let agent = tempfile::tempdir().unwrap();
        let root = agent.path();
        let fixtures = tempfile::tempdir().unwrap();
        let profile = fixtures.path().join("oauth.json");
        std::fs::write(
            &profile,
            r#"{
  "id": "acme",
  "scopes": ["read"],
  "flows": ["authorization-code"],
  "authorization-endpoint": "https://auth.example.com/authorize",
  "token-endpoint": "https://auth.example.com/token",
  "revocation-endpoint": "https://auth.example.com/revoke",
  "client-id": "client-123"
}"#,
        )
        .unwrap();
        assert_eq!(
            grant_into(
                &[
                    "acme".into(),
                    "oauth".into(),
                    "--provider".into(),
                    "acme".into(),
                    "--profile".into(),
                    profile.display().to_string(),
                ],
                Some(root)
            ),
            0
        );

        let mut store = crate::config::AuthStore::new();
        store.insert(
            crate::config::plugin_credential_key("acme", "acme"),
            crate::config::Credential::Oauth {
                access: "at-secret".to_string(),
                refresh: "rt-secret".to_string(),
                expires: 123,
                profile_fingerprint: Some(stored_profile_fingerprint(root, "acme", "acme")),
            },
        );
        crate::config::write_auth_at(root, &store).unwrap();

        let grants = Grants::load_for_at(root, "acme");
        let report =
            revoke_oauth_credentials_with(root, "acme", grants.oauth.as_ref(), &FailingTransport);
        assert_eq!(report.removed, vec!["acme".to_string()]);
        assert!(
            report
                .failures
                .iter()
                .any(|failure| failure.contains("IdP")),
            "the failure must be reported: {:?}",
            report.failures
        );
        let after = crate::config::read_auth_at(root).unwrap();
        assert!(
            after
                .get(&crate::config::plugin_credential_key("acme", "acme"))
                .is_none(),
            "the local credential is deleted despite the failure: {after:?}"
        );
    }

    /// `uninstall` deletes every `plugin:<install>:` OAuth credential and
    /// nothing else, so no live token is stranded once its grant is gone.
    #[test]
    fn uninstall_removes_every_namespaced_oauth_credential() {
        let agent = tempfile::tempdir().unwrap();
        let root = agent.path();
        std::fs::create_dir_all(root.join("plugins/foo")).unwrap();
        assert!(root.join("plugins/foo").is_dir());

        let mut store = crate::config::AuthStore::new();
        for (key, access) in [
            ("plugin:foo:acme", "at-acme"),
            ("plugin:foo:other", "at-other"),
            ("plugin:bar:acme", "at-other-plugin"),
            ("anthropic", "sk-live"),
        ] {
            store.insert(
                key.to_string(),
                crate::config::Credential::Oauth {
                    access: access.to_string(),
                    refresh: "rt".to_string(),
                    expires: 1,
                    profile_fingerprint: None,
                },
            );
        }
        crate::config::write_auth_at(root, &store).unwrap();

        assert_eq!(uninstall_into(&["foo".into()], Some(root)), 0);

        let after = crate::config::read_auth_at(root).unwrap();
        assert!(!after.contains_key("plugin:foo:acme"), "{after:?}");
        assert!(!after.contains_key("plugin:foo:other"), "{after:?}");
        assert!(after.contains_key("plugin:bar:acme"), "{after:?}");
        assert!(after.contains_key("anthropic"), "{after:?}");
    }

    #[test]
    fn uninstall_preserves_a_non_oauth_namespaced_credential() {
        let agent = tempfile::tempdir().unwrap();
        let root = agent.path();
        std::fs::create_dir_all(root.join("plugins/foo")).unwrap();
        let key = crate::config::plugin_credential_key("foo", "acme");
        let mut store = crate::config::AuthStore::new();
        store.insert(
            key.clone(),
            crate::config::Credential::ApiKey {
                key: Some("sk-user".to_string()),
                env: None,
            },
        );
        crate::config::write_auth_at(root, &store).unwrap();

        assert_eq!(uninstall_into(&["foo".into()], Some(root)), 0);
        assert!(matches!(
            crate::config::read_auth_at(root).unwrap().get(&key),
            Some(crate::config::Credential::ApiKey { key, .. })
                if key.as_deref() == Some("sk-user")
        ));
    }

    /// Granting `oauth`/`provider` to a plugin that already holds exec (or fs,
    /// or raw sockets) warns that those can read the credential store; other
    /// capabilities and a clean plugin produce no warning.
    #[test]
    fn credential_grants_warn_when_the_plugin_can_read_auth_json() {
        let mut grants = Grants::default();
        assert!(
            credential_capability_warning(&grants, "oauth", "p").is_none(),
            "nothing held: no warning"
        );
        grants
            .grant("exec", vec!["cat".to_string()], GrantOptions::default())
            .unwrap();
        let warning = credential_capability_warning(&grants, "oauth", "p").expect("warning");
        for expected in ["p", "exec", "auth.json", "oauth", "ki plugin revoke"] {
            assert!(
                warning.contains(expected),
                "missing {expected:?}: {warning}"
            );
        }
        assert!(
            credential_capability_warning(&grants, "fs", "p").is_none(),
            "a non-credential capability is not this warning's subject"
        );
        assert!(credential_capability_warning(&grants, "provider", "p")
            .expect("provider warns too")
            .contains("provider"));
    }

    /// An unknown resource kind aborts the install before the component is
    /// copied, so nothing half-installed is left behind.
    #[test]
    fn install_rejects_unknown_resource_kinds_before_copying() {
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("plugin.wasm"), b"\0asm").unwrap();
        std::fs::write(
            source.path().join("resources.json"),
            r#"{"kinds":["themes"]}"#,
        )
        .unwrap();
        std::fs::create_dir_all(source.path().join("resources/themes")).unwrap();

        let agent = tempfile::tempdir().unwrap();
        let root = agent.path();
        let code = install_into(
            &[
                source.path().join("plugin.wasm").display().to_string(),
                "--id".into(),
                "bad".into(),
            ],
            Some(root),
        );
        assert_eq!(code, 1);
        assert!(
            !root.join("plugins/bad/plugin.wasm").exists(),
            "a rejected resource package installs nothing"
        );
    }

    /// `show` discloses the oauth profile (endpoints, flows, scopes, budget)
    /// and the namespaced credential handle — never a token value.
    #[test]
    fn show_discloses_plugin_oauth_credentials_without_the_value() {
        let mut grants = Grants::default();
        grants
            .grant_oauth(OAuthGrant {
                providers: vec![OAuthProviderProfile {
                    id: "acme".to_string(),
                    display_name: "Acme".to_string(),
                    scopes: vec!["read".to_string(), "write".to_string()],
                    flows: vec![OAuthFlow::AuthorizationCode, OAuthFlow::DeviceCode],
                    authorization_endpoint: "https://auth.example.com/authorize".to_string(),
                    token_endpoint: "https://auth.example.com/token".to_string(),
                    device_authorization_endpoint: Some(
                        "https://auth.example.com/device".to_string(),
                    ),
                    revocation_endpoint: Some("https://auth.example.com/revoke".to_string()),
                    client_id: "client-123".to_string(),
                    supports_pkce: true,
                }],
                timeout_seconds: 45,
                max_requests: 7,
            })
            .unwrap();

        let policy = policy_disclosure(&grants, &CredentialSidecar::default());
        for expected in [
            "oauth policy:",
            "acme",
            "token: https://auth.example.com/token",
            "authorization: https://auth.example.com/authorize",
            "device: https://auth.example.com/device",
            "revocation: https://auth.example.com/revoke",
            "flows: authorization-code, device-code",
            "scopes: read write",
            "budget: 45s, 7 requests",
        ] {
            assert!(policy.contains(expected), "missing {expected}: {policy}");
        }

        let mut store = crate::config::AuthStore::new();
        store.insert(
            crate::config::plugin_credential_key("acme", "acme"),
            crate::config::Credential::Oauth {
                access: "at-secret".to_string(),
                refresh: "rt-secret".to_string(),
                expires: 123,
                profile_fingerprint: None,
            },
        );
        store.insert(
            crate::config::plugin_credential_key("acme", "blocked"),
            crate::config::Credential::ApiKey {
                key: Some("sk-secret".to_string()),
                env: None,
            },
        );
        // Only `acme` is granted, so the blocked key is not in this disclosure.
        let text = oauth_credentials_disclosure("acme", &grants, &store);
        assert!(text.contains("plugin:acme:acme"), "{text}");
        assert!(text.contains("stored (expires 123)"), "{text}");
        assert!(!text.contains("at-secret"), "{text}");
        assert!(!text.contains("rt-secret"), "{text}");
        assert!(!text.contains("sk-secret"), "{text}");
    }
}
