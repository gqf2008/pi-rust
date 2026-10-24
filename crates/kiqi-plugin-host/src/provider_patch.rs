//! Validation and clamping for a `before-provider-request` patch.
//!
//! A wasm guest may answer the provider gate with `event-action.patch(json)`.
//! The patch is untrusted: it is the only path by which a sandboxed plugin can
//! change the live request options, so the host validates it against a fixed
//! table before anything is applied. Any violation rejects the WHOLE patch —
//! partial application is not intended — and the request proceeds unpatched.
//!
//! The accepted envelope is:
//!
//! ```json
//! {
//!   "headers":    { "<name>": "<value>" | null },
//!   "metadata":   { "<key>":  "<value>" | null },
//!   "timeoutMs":  1000,
//!   "maxTokens":  4096,
//!   "temperature": 0.7
//! }
//! ```
//!
//! `null` deletes a map entry (the same inner-`None` semantics the native
//! [`kiqi_ai::SimpleStreamOptionsPatch`] uses). Header names are matched
//! case-insensitively against [`DENIED_HEADERS`] for **insertion and deletion**:
//! a plugin cannot shadow a credential header by removing and re-adding it.

use serde_json::Value;

/// Largest accepted serialized patch. A patch is a small override record, not a
/// second request body.
pub const MAX_ENVELOPE_BYTES: usize = 32 * 1024;

pub const MAX_HEADER_ENTRIES: usize = 32;
pub const MAX_METADATA_ENTRIES: usize = 64;
pub const MAX_KEY_BYTES: usize = 128;
pub const MAX_MAP_VALUE_BYTES: usize = 2048;
pub const MAX_HEADERS_BYTES: usize = 16 * 1024;
pub const MAX_METADATA_BYTES: usize = 16 * 1024;
pub const MAX_METADATA_DEPTH: usize = 4;

pub const TIMEOUT_MS_MIN: u64 = 1_000;
pub const TIMEOUT_MS_MAX: u64 = 900_000;
pub const MAX_TOKENS_MIN: u64 = 1;
pub const MAX_TOKENS_MAX: u64 = 262_144;
pub const TEMPERATURE_MIN: f64 = 0.0;
pub const TEMPERATURE_MAX: f64 = 2.0;

/// Whether `name` is a protected header: the credential/framing names a guest
/// may neither patch nor see the value of. The comparison is
/// case-insensitive, so `Authorization` and `authorization` are the same
/// header.
pub fn is_denied_header(name: &str) -> bool {
    DENIED_HEADERS.contains(&name.to_ascii_lowercase().as_str())
}

/// Header names a patch may never touch and an observer never sees the value
/// of. Matched case-insensitively, and enforced for both insertion and
/// deletion.
pub const DENIED_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "x-goog-api-key",
    "api-key",
    "x-auth-token",
    "cf-aig-authorization",
    "cookie",
    "proxy-authorization",
    "host",
    "content-length",
    "transfer-encoding",
    "content-type",
];

/// Keys the live options carry but a patch may not change.
pub const NON_PATCHABLE_KEYS: &[&str] =
    &["sessionId", "cacheRetention", "maxRetries", "maxRetryDelay"];

/// Validate one patch.
///
/// `provider_budget_ms` is the tightest provider grant budget, when a provider
/// grant exists: a patched timeout must not outlive the request it belongs to.
/// Returns the canonical patch on success and a one-line reason on rejection.
pub fn validate_provider_patch(
    raw: &Value,
    provider_budget_ms: Option<u64>,
) -> Result<Value, String> {
    let object = raw
        .as_object()
        .ok_or_else(|| "patch must be a JSON object".to_string())?;
    let serialized =
        serde_json::to_vec(raw).map_err(|error| format!("serialize patch: {error}"))?;
    if serialized.len() > MAX_ENVELOPE_BYTES {
        return Err(format!(
            "patch is {} bytes, over the {MAX_ENVELOPE_BYTES}-byte envelope",
            serialized.len()
        ));
    }

    for key in object.keys() {
        match key.as_str() {
            "headers" | "metadata" | "timeoutMs" | "maxTokens" | "temperature" => {}
            other if NON_PATCHABLE_KEYS.contains(&other) => {
                return Err(format!("`{other}` is not patchable"));
            }
            other => return Err(format!("unknown patch key `{other}`")),
        }
    }

    if let Some(headers) = object.get("headers") {
        validate_map(
            "headers",
            headers,
            MAX_HEADER_ENTRIES,
            MAX_HEADERS_BYTES,
            true,
        )?;
    }
    if let Some(metadata) = object.get("metadata") {
        validate_map(
            "metadata",
            metadata,
            MAX_METADATA_ENTRIES,
            MAX_METADATA_BYTES,
            false,
        )?;
    }

    if let Some(timeout) = object.get("timeoutMs") {
        let timeout = timeout
            .as_u64()
            .ok_or_else(|| "`timeoutMs` must be a non-negative integer".to_string())?;
        if !(TIMEOUT_MS_MIN..=TIMEOUT_MS_MAX).contains(&timeout) {
            return Err(format!(
                "`timeoutMs` must be {TIMEOUT_MS_MIN}..={TIMEOUT_MS_MAX}, got {timeout}"
            ));
        }
        if let Some(budget) = provider_budget_ms {
            if timeout > budget {
                return Err(format!(
                    "`timeoutMs` {timeout} exceeds the provider budget of {budget} ms"
                ));
            }
        }
    }

    if let Some(tokens) = object.get("maxTokens") {
        let tokens = tokens
            .as_u64()
            .ok_or_else(|| "`maxTokens` must be a non-negative integer".to_string())?;
        if !(MAX_TOKENS_MIN..=MAX_TOKENS_MAX).contains(&tokens) {
            return Err(format!(
                "`maxTokens` must be {MAX_TOKENS_MIN}..={MAX_TOKENS_MAX}, got {tokens}"
            ));
        }
    }

    if let Some(temperature) = object.get("temperature") {
        let temperature = temperature
            .as_f64()
            .ok_or_else(|| "`temperature` must be a number".to_string())?;
        if !temperature.is_finite() || !(TEMPERATURE_MIN..=TEMPERATURE_MAX).contains(&temperature) {
            return Err(format!(
                "`temperature` must be finite and {TEMPERATURE_MIN}..={TEMPERATURE_MAX}"
            ));
        }
    }

    Ok(raw.clone())
}

fn validate_map(
    field: &str,
    value: &Value,
    max_entries: usize,
    max_bytes: usize,
    deny_headers: bool,
) -> Result<(), String> {
    let map = value
        .as_object()
        .ok_or_else(|| format!("`{field}` must be an object"))?;
    if map.len() > max_entries {
        return Err(format!(
            "`{field}` has {} entries, over the {max_entries}-entry cap",
            map.len()
        ));
    }
    for (key, entry) in map {
        if key.is_empty() {
            return Err(format!("`{field}` has an empty key"));
        }
        if key.len() > MAX_KEY_BYTES {
            return Err(format!(
                "`{field}` key `{key}` is {} bytes, over the {MAX_KEY_BYTES}-byte cap",
                key.len()
            ));
        }
        if deny_headers && is_denied_header(key) {
            return Err(format!(
                "`{field}` may not change the protected header `{key}` (insert or delete)"
            ));
        }
        match entry {
            Value::Null => {}
            Value::String(text) => {
                if text.len() > MAX_MAP_VALUE_BYTES {
                    return Err(format!(
                        "`{field}.{key}` is {} bytes, over the {MAX_MAP_VALUE_BYTES}-byte cap",
                        text.len()
                    ));
                }
            }
            other => {
                let depth = json_depth(other);
                if depth > MAX_METADATA_DEPTH {
                    return Err(format!(
                        "`{field}.{key}` nests {depth} levels, over the {MAX_METADATA_DEPTH} cap"
                    ));
                }
                return Err(format!(
                    "`{field}.{key}` must be a string or null, got {other}"
                ));
            }
        }
    }
    let serialized =
        serde_json::to_vec(value).map_err(|error| format!("serialize `{field}`: {error}"))?;
    if serialized.len() > max_bytes {
        return Err(format!(
            "`{field}` is {} bytes, over the {max_bytes}-byte cap",
            serialized.len()
        ));
    }
    Ok(())
}

/// Nesting depth of a JSON value (a scalar is 1, an array of scalars 2, …).
fn json_depth(value: &Value) -> usize {
    match value {
        Value::Array(items) => 1 + items.iter().map(json_depth).max().unwrap_or(0),
        Value::Object(map) => 1 + map.values().map(json_depth).max().unwrap_or(0),
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_well_formed_patch_passes() {
        let raw = json!({
            "headers": { "x-trace": "abc", "x-remove": null },
            "metadata": { "user_id": "u-1" },
            "timeoutMs": 5_000,
            "maxTokens": 4096,
            "temperature": 0.7,
        });
        let validated = validate_provider_patch(&raw, Some(600_000)).expect("valid");
        assert_eq!(validated, raw);
    }

    #[test]
    fn protected_headers_are_rejected_on_insert_and_delete() {
        for value in [json!("secret"), Value::Null] {
            let raw = json!({ "headers": { "Authorization": value } });
            let error = validate_provider_patch(&raw, None).expect_err("denied header");
            assert!(
                error.to_ascii_lowercase().contains("authorization"),
                "{error}"
            );

            let raw = json!({ "headers": { "X-API-KEY": value } });
            assert!(validate_provider_patch(&raw, None).is_err());
        }
    }

    #[test]
    fn any_bad_key_rejects_the_whole_patch() {
        // A valid field next to a bad one must not be applied alone.
        let raw = json!({ "maxTokens": 128, "sessionId": "other" });
        let error = validate_provider_patch(&raw, None).expect_err("non-patchable");
        assert!(error.contains("sessionId"), "{error}");

        let raw = json!({ "maxTokens": 128, "headers": { "Host": "evil" } });
        assert!(validate_provider_patch(&raw, None).is_err());

        let raw = json!({ "unknown": true });
        assert!(validate_provider_patch(&raw, None).is_err());
    }

    #[test]
    fn number_clamps_are_enforced() {
        assert!(validate_provider_patch(&json!({ "timeoutMs": 999 }), None).is_err());
        assert!(validate_provider_patch(&json!({ "timeoutMs": 900_001 }), None).is_err());
        assert!(validate_provider_patch(&json!({ "timeoutMs": 2_000 }), Some(1_000)).is_err());
        assert!(validate_provider_patch(&json!({ "maxTokens": 0 }), None).is_err());
        assert!(validate_provider_patch(&json!({ "maxTokens": 262_145 }), None).is_err());
        assert!(validate_provider_patch(&json!({ "temperature": 2.1 }), None).is_err());
        assert!(validate_provider_patch(&json!({ "temperature": -0.1 }), None).is_err());
    }

    #[test]
    fn map_caps_and_envelope_are_enforced() {
        let mut headers = serde_json::Map::new();
        for index in 0..(MAX_HEADER_ENTRIES + 1) {
            headers.insert(format!("x-{index}"), json!("v"));
        }
        assert!(validate_provider_patch(&json!({ "headers": headers }), None).is_err());

        let long = "x".repeat(MAX_MAP_VALUE_BYTES + 1);
        assert!(validate_provider_patch(&json!({ "headers": { "x-a": long } }), None).is_err());

        let envelope = "x".repeat(MAX_ENVELOPE_BYTES);
        assert!(validate_provider_patch(&json!({ "headers": { "x-a": envelope } }), None).is_err());

        // Nested values are not representable in the live options.
        assert!(
            validate_provider_patch(&json!({ "metadata": { "nested": { "a": "b" } } }), None)
                .is_err()
        );
    }

    #[test]
    fn a_non_object_patch_is_rejected() {
        assert!(validate_provider_patch(&json!("not a patch"), None).is_err());
        assert!(validate_provider_patch(&Value::Null, None).is_err());
    }
}
