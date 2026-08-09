use openfang_types::runtime_admission::ResultStorageMode;
use openfang_types::tool::ToolResult;
use sha2::{Digest, Sha256};

/// Persistable, privacy-safe representation of one tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptPayload {
    pub sha256: String,
    pub storage_mode: ResultStorageMode,
    pub envelope: Option<String>,
}

/// Hash compact canonical JSON with recursively sorted object keys.
pub fn canonical_json_sha256(value: &serde_json::Value) -> Result<String, String> {
    let canonical = canonicalize_json(value.clone());
    serde_json::to_vec(&canonical)
        .map(|bytes| sha256_hex(&bytes))
        .map_err(|_| "canonical_json_serialization_failed".into())
}

/// Build a receipt payload without retaining a non-JSON or oversized result.
pub fn receipt_payload(
    result: &ToolResult,
    receipt_max_bytes: usize,
) -> Result<ReceiptPayload, String> {
    if !(1..=65_536).contains(&receipt_max_bytes) {
        return Err("receipt_max_bytes_invalid".into());
    }

    let sha256 = sha256_hex(result.content.as_bytes());
    if result.content.len() > receipt_max_bytes {
        return Ok(digest_only(sha256));
    }
    let parsed = match serde_json::from_str::<serde_json::Value>(&result.content) {
        Ok(value) => value,
        Err(_) => return Ok(digest_only(sha256)),
    };
    let redacted = match redact_value(parsed) {
        Some(value) => value,
        None => return Ok(digest_only(sha256)),
    };
    let envelope = match serde_json::to_string(&redacted) {
        Ok(envelope) if envelope.len() <= receipt_max_bytes => envelope,
        _ => return Ok(digest_only(sha256)),
    };
    Ok(ReceiptPayload {
        sha256,
        storage_mode: ResultStorageMode::RedactedEnvelope,
        envelope: Some(envelope),
    })
}

fn digest_only(sha256: String) -> ReceiptPayload {
    ReceiptPayload {
        sha256,
        storage_mode: ResultStorageMode::DigestOnly,
        envelope: None,
    }
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn redact_value(value: serde_json::Value) -> Option<serde_json::Value> {
    match value {
        serde_json::Value::Array(items) => Some(serde_json::Value::Array(
            items
                .into_iter()
                .map(redact_value)
                .collect::<Option<Vec<_>>>()?,
        )),
        serde_json::Value::Object(fields) => {
            let mut ordered = std::collections::BTreeMap::new();
            for (key, value) in fields {
                let redacted = if is_secret_key(&key) {
                    serde_json::Value::String("[REDACTED]".into())
                } else {
                    redact_value(value)?
                };
                ordered.insert(key, redacted);
            }
            Some(serde_json::Value::Object(ordered.into_iter().collect()))
        }
        serde_json::Value::String(value) => redact_string(value),
        value => Some(value),
    }
}

fn canonicalize_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(canonicalize_json).collect())
        }
        serde_json::Value::Object(fields) => {
            let mut ordered = std::collections::BTreeMap::new();
            for (key, value) in fields {
                ordered.insert(key, canonicalize_json(value));
            }
            serde_json::Value::Object(ordered.into_iter().collect())
        }
        value => value,
    }
}

fn is_secret_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect();
    matches!(
        normalized.as_str(),
        "authorization"
            | "token"
            | "accesstoken"
            | "refreshtoken"
            | "apitoken"
            | "apikey"
            | "xapikey"
            | "password"
            | "passwd"
            | "secret"
            | "clientsecret"
            | "credential"
            | "credentials"
            | "privatekey"
            | "cookie"
            | "setcookie"
            | "session"
            | "sessionid"
            | "sessionidentifier"
    ) || normalized.starts_with("session")
}

fn redact_string(value: String) -> Option<serde_json::Value> {
    let lower = value.to_ascii_lowercase();
    if lower.contains("bearer ")
        || lower.contains("basic ")
        || lower.contains("-----begin private key-----")
        || lower.contains("-----begin rsa private key-----")
        || lower.contains("-----begin ec private key-----")
    {
        return Some(serde_json::Value::String("[REDACTED]".into()));
    }

    if let Some(scheme_end) = lower.find("://") {
        let scheme = &lower[..scheme_end];
        let rest = &value[scheme_end + 3..];
        if scheme.is_empty()
            || !scheme
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "+-.".contains(character))
            || rest.is_empty()
        {
            return None;
        }
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        if rest[..authority_end].contains('@') {
            return Some(serde_json::Value::String("[REDACTED]".into()));
        }
        if let Some(query_start) = rest.find('?') {
            let query_end = rest[query_start + 1..]
                .find('#')
                .map(|offset| query_start + 1 + offset)
                .unwrap_or(rest.len());
            for parameter in rest[query_start + 1..query_end].split(['&', ';']) {
                let key = parameter.split_once('=').map_or(parameter, |(key, _)| key);
                if key.contains('%') {
                    return None;
                }
                if is_secret_key(key) {
                    return Some(serde_json::Value::String("[REDACTED]".into()));
                }
            }
        }
    }

    Some(serde_json::Value::String(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::runtime_admission::ResultStorageMode;
    use openfang_types::tool::ToolResult;

    #[test]
    fn receipt_redaction_uses_safe_compact_json_and_original_digest() {
        let result = ToolResult {
            tool_use_id: "tool-1".into(),
            content: r#"{"token":"top-secret","nested":{"PASSWORD":"also-secret"},"items":[{"Api_Key":"secret","safe":1}]}"#.into(),
            is_error: false,
        };

        let payload = receipt_payload(&result, 1_024).expect("valid receipt payload");
        assert_eq!(payload.storage_mode, ResultStorageMode::RedactedEnvelope);
        assert_eq!(
            payload.envelope.as_deref(),
            Some(
                r#"{"items":[{"Api_Key":"[REDACTED]","safe":1}],"nested":{"PASSWORD":"[REDACTED]"},"token":"[REDACTED]"}"#
            )
        );
        assert!(!payload
            .envelope
            .as_deref()
            .unwrap_or_default()
            .contains("secret"));
        assert_eq!(payload.sha256, sha256_hex(result.content.as_bytes()));
    }

    #[test]
    fn receipt_payload_is_digest_only_for_non_json_oversize_and_zero_limit() {
        let non_json = ToolResult {
            tool_use_id: "tool-1".into(),
            content: "not json".into(),
            is_error: false,
        };
        let payload = receipt_payload(&non_json, 64).expect("digest-only payload");
        assert_eq!(payload.storage_mode, ResultStorageMode::DigestOnly);
        assert!(payload.envelope.is_none());

        let oversize = ToolResult {
            tool_use_id: "tool-2".into(),
            content: r#"{"safe":"this is too large"}"#.into(),
            is_error: false,
        };
        let payload = receipt_payload(&oversize, 1).expect("digest-only payload");
        assert_eq!(payload.storage_mode, ResultStorageMode::DigestOnly);
        assert!(payload.envelope.is_none());

        assert!(receipt_payload(&oversize, 0).is_err());
    }

    #[test]
    fn receipt_redacts_separator_variants_and_suspicious_string_values() {
        let result = ToolResult {
            tool_use_id: "tool-sensitive".into(),
            content: serde_json::json!({
                "Access-Token": "alpha",
                "refresh token": "beta",
                "API.Token": "gamma",
                "X_API_KEY": "delta",
                "PassWd": "epsilon",
                "client-secret": "zeta",
                "Private Key": "eta",
                "Set-Cookie": "theta",
                "session_identifier": "iota",
                "nested": [{
                    "safe": "Grüße 東京",
                    "auth_header": "Bearer synthetic-token",
                    "basic_header": "Basic c3ludGhldGlj",
                    "pem": "-----BEGIN PRIVATE KEY-----\nsynthetic\n-----END PRIVATE KEY-----",
                    "userinfo_url": "https://synthetic-user:synthetic-pass@example.invalid/path",
                    "query_url": "https://example.invalid/?access_token=synthetic"
                }]
            })
            .to_string(),
            is_error: false,
        };

        let payload = receipt_payload(&result, 4_096).expect("payload");
        let envelope = payload.envelope.expect("redacted envelope");
        assert_eq!(payload.storage_mode, ResultStorageMode::RedactedEnvelope);
        assert!(envelope.contains("Grüße 東京"));
        for secret in [
            "alpha",
            "beta",
            "gamma",
            "delta",
            "epsilon",
            "zeta",
            "eta",
            "theta",
            "iota",
            "synthetic-token",
            "c3ludGhldGlj",
            "synthetic-user",
            "access_token=synthetic",
        ] {
            assert!(
                !envelope.contains(secret),
                "retained secret fragment: {secret}"
            );
        }
    }

    #[test]
    fn raw_content_larger_than_bound_is_digest_only_before_redaction() {
        let result = ToolResult {
            tool_use_id: "tool-oversize".into(),
            content: serde_json::json!({
                "token": "x".repeat(2_048),
                "safe": "small after redaction"
            })
            .to_string(),
            is_error: false,
        };

        let payload = receipt_payload(&result, 128).expect("payload");
        assert_eq!(payload.storage_mode, ResultStorageMode::DigestOnly);
        assert!(payload.envelope.is_none());
        assert_eq!(payload.sha256, sha256_hex(result.content.as_bytes()));
    }

    #[test]
    fn uncertain_encoded_url_credential_key_falls_back_to_digest_only() {
        let result = ToolResult {
            tool_use_id: "tool-url".into(),
            content: serde_json::json!({
                "url": "https://example.invalid/?access%5Ftoken=synthetic"
            })
            .to_string(),
            is_error: false,
        };

        let payload = receipt_payload(&result, 1_024).expect("payload");
        assert_eq!(payload.storage_mode, ResultStorageMode::DigestOnly);
        assert!(payload.envelope.is_none());
    }

    #[test]
    fn canonical_json_hash_sorts_objects_but_preserves_array_order() {
        let left = serde_json::json!({"b": 2, "a": {"y": 1, "x": 0}, "items": [1, 2]});
        let reordered = serde_json::json!({"items": [1, 2], "a": {"x": 0, "y": 1}, "b": 2});
        let different_array = serde_json::json!({"items": [2, 1], "a": {"x": 0, "y": 1}, "b": 2});

        assert_eq!(
            canonical_json_sha256(&left).expect("left"),
            canonical_json_sha256(&reordered).expect("reordered")
        );
        assert_ne!(
            canonical_json_sha256(&left).expect("left"),
            canonical_json_sha256(&different_array).expect("different")
        );
    }
}
