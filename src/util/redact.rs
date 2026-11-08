//! Secret redaction for provider error bodies and debug output.
//!
//! Provider errors sometimes echo request fragments. Those bodies are shown
//! in the UI and may be included in tracing, so scrub anything that looks
//! like an API token before it leaves the HTTP layer.

use std::sync::LazyLock;

use regex::Regex;

/// `xai-...` API keys.
static XAI_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bxai-[A-Za-z0-9._-]{4,}").expect("xai token regex"));

/// OpenAI-style `sk-...` API keys.
static SK_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bsk-[A-Za-z0-9._-]{4,}").expect("sk token regex"));

/// `Authorization: Bearer <token>` headers.
static BEARER_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]+").expect("bearer token regex")
});

/// `api_key=...`, `"api_key": "..."`, `x-api-key: ...` values.
static API_KEY_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(\b(?:x[-_]?api[-_]?key|api[-_]?key)\b\s*["']?\s*[:=]\s*["']?)([^"'\s,}&]+)"#)
        .expect("api key value regex")
});

/// Replace likely secrets in `input` with `[REDACTED]`.
///
/// Matches `xai-...` and `sk-...` tokens, `Bearer <token>` values, and
/// obvious `api_key` assignments (including JSON `"api_key":"..."`).
pub fn redact_secrets(input: &str) -> String {
    let out = XAI_TOKEN.replace_all(input, "[REDACTED]");
    let out = SK_TOKEN.replace_all(&out, "[REDACTED]");
    let out = BEARER_TOKEN.replace_all(&out, "Bearer [REDACTED]");
    API_KEY_VALUE
        .replace_all(&out, "${1}[REDACTED]")
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn redacts_xai_and_sk_tokens() {
        let out = redact_secrets("bad key xai-not-a-real-key-12345 and sk-fake_98765 end");
        assert!(!out.contains("xai-not-a-real-key-12345"));
        assert!(!out.contains("sk-fake_98765"));
        assert_eq!(out.matches("[REDACTED]").count(), 2);
        assert!(out.contains("bad key"));
    }

    #[test]
    fn redacts_bearer_tokens() {
        let out = redact_secrets("Authorization: Bearer abc.def-123_456");
        assert_eq!(out, "Authorization: Bearer [REDACTED]");
    }

    #[test]
    fn redacts_api_key_assignments() {
        let cases = [
            ("api_key=fake-value-123", "api_key=[REDACTED]"),
            ("API_KEY: fake-value-123", "API_KEY: [REDACTED]"),
            (
                r#"{"api_key":"fake-value-123"}"#,
                r#"{"api_key":"[REDACTED]"}"#,
            ),
            ("x-api-key: fake-value-123", "x-api-key: [REDACTED]"),
        ];
        for (input, expected) in cases {
            assert_eq!(redact_secrets(input), expected, "input: {input}");
        }
    }

    #[test]
    fn leaves_ordinary_text_and_unicode_alone() {
        let text = "The dragon says: héllo 龍 — no secrets here.";
        assert_eq!(redact_secrets(text), text);
    }

    #[test]
    fn handles_multiple_patterns_in_one_body() {
        let out = redact_secrets(
            "key=xai-abcdef123456 Authorization: Bearer token.abc-123 \
             {\"api_key\":\"sk-abcdef123456\"}",
        );
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("xai-abcdef123456"));
        assert!(!out.contains("token.abc-123"));
        assert!(!out.contains("sk-abcdef123456"));
    }
}
