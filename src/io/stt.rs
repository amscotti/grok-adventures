//! xAI speech-to-text client (`POST /v1/stt`).

use std::fmt;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::DEFAULT_STT_MODEL;
use crate::util::http::{api_client, read_text_bounded};
use crate::util::redact::redact_secrets;

const DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Hard cap on one STT response body (2 MiB).
const MAX_STT_BODY_BYTES: usize = 2 * 1024 * 1024;

/// JSON response from `/v1/stt`.
#[derive(Debug, Clone, Deserialize)]
pub struct SttResponse {
    pub text: String,
    #[serde(default)]
    pub duration: Option<f64>,
}

/// HTTP client for xAI STT.
#[derive(Clone)]
pub struct SttClient {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
    language: String,
    streaming: bool,
}

impl fmt::Debug for SttClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SttClient")
            .field("client", &self.client)
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("language", &self.language)
            .field("streaming", &self.streaming)
            .finish()
    }
}

impl SttClient {
    /// Model defaults to `XAI_STT_MODEL` or [`DEFAULT_STT_MODEL`]
    /// (`grok-voice-transcribe-2.0`). Language defaults to `en`.
    pub fn new(api_key: impl Into<String>) -> Self {
        let model = std::env::var("XAI_STT_MODEL")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_STT_MODEL.to_string());
        Self {
            client: api_client(REQUEST_TIMEOUT),
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            model,
            language: "en".to_string(),
            streaming: true,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        let model = model.into();
        let model = model.trim();
        if !model.is_empty() {
            self.model = model.to_string();
        }
        self
    }

    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        let language = language.into();
        let language = language.trim();
        if !language.is_empty() {
            self.language = language.to_ascii_lowercase();
        }
        self
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn language(&self) -> &str {
        &self.language
    }

    pub fn streaming(&self) -> bool {
        self.streaming
    }

    pub(crate) fn api_key(&self) -> &str {
        &self.api_key
    }

    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    /// When false, listen captures a WAV and uses the batch endpoint only.
    pub fn with_streaming(mut self, streaming: bool) -> Self {
        self.streaming = streaming;
        self
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Transcribe a local WAV (or other supported audio) file.
    pub async fn transcribe_wav(&self, path: &Path) -> Result<String> {
        if !path.exists() {
            bail!("audio file not found: {}", path.display());
        }

        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("audio.wav")
            .to_string();

        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;

        // Text fields first, file LAST. The API ignores fields that follow
        // the file on a streamable upload.
        let file_part = reqwest::multipart::Part::bytes(bytes)
            .file_name(file_name)
            .mime_str("audio/wav")
            .context("setting audio mime type")?;

        let mut form = reqwest::multipart::Form::new();
        for (key, value) in stt_text_fields(&self.model, &self.language) {
            form = form.text(key, value);
        }
        let form = form.part("file", file_part);

        let url = format!("{}/stt", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .multipart(form)
            .send()
            .await
            .context("sending STT request")?;

        let status = resp.status();
        let body = read_text_bounded(resp, MAX_STT_BODY_BYTES, "STT")
            .await
            .context("reading STT response body")?;
        if !status.is_success() {
            bail!("STT API error {status}: {}", redact_secrets(&body));
        }

        let parsed: SttResponse = serde_json::from_str(&body)
            .with_context(|| format!("parsing STT JSON: {}", redact_secrets(&body)))?;
        Ok(parsed.text.trim().to_string())
    }
}

/// Multipart text fields for `/v1/stt`, in the order they must precede `file`.
fn stt_text_fields(model: &str, language: &str) -> Vec<(&'static str, String)> {
    vec![
        ("model", model.to_string()),
        ("language", language.to_string()),
        ("format", "true".to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stt_response_deserializes() {
        let raw = r#"{"text":"hello world","duration":1.25}"#;
        let r: SttResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(r.text, "hello world");
        assert_eq!(r.duration, Some(1.25));
    }

    #[test]
    fn stt_text_fields_pin_the_transcription_model() {
        let fields = stt_text_fields("grok-voice-transcribe-2.0", "en");
        assert_eq!(fields[0], ("model", "grok-voice-transcribe-2.0".into()));
        assert_eq!(fields[1].0, "language");
        assert_eq!(fields[2], ("format", "true".into()));
        assert!(fields.iter().all(|(key, _)| *key != "file"));
    }

    #[test]
    fn stt_client_model_override() {
        let client = SttClient::new("key").with_model("grok-voice-transcribe-1.0");
        assert_eq!(client.model(), "grok-voice-transcribe-1.0");
    }

    #[test]
    fn stt_response_text_only() {
        let raw = r#"{"text":"hi"}"#;
        let r: SttResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(r.text, "hi");
        assert!(r.duration.is_none());
    }

    #[test]
    fn debug_never_prints_the_api_key() {
        const FAKE_KEY: &str = "xai-stt-debug-SECRET-9988776655";
        let client = SttClient::new(FAKE_KEY);
        let out = format!("{client:?}");
        assert!(!out.contains(FAKE_KEY), "debug leaked key: {out}");
        assert!(out.contains("<redacted>"));
    }

    fn temp_wav() -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"fake wav bytes").unwrap();
        file
    }

    #[tokio::test]
    async fn stt_error_body_is_redacted() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const FAKE_KEY: &str = "xai-stt-error-SECRET-13572468";
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/stt"))
            .respond_with(
                ResponseTemplate::new(401).set_body_string(format!("bad token Bearer {FAKE_KEY}")),
            )
            .mount(&server)
            .await;

        let client = SttClient::new("test-key").with_base_url(server.uri());
        let wav = temp_wav();
        let err = client
            .transcribe_wav(wav.path())
            .await
            .expect_err("401 should fail");
        let msg = format!("{err:#}");
        assert!(!msg.contains(FAKE_KEY), "error leaked key: {msg}");
        assert!(msg.contains("[REDACTED]"), "error was not redacted: {msg}");
    }

    #[tokio::test]
    async fn stt_body_is_capped() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = "x".repeat(MAX_STT_BODY_BYTES + 1);
        Mock::given(method("POST"))
            .and(path("/stt"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let client = SttClient::new("test-key").with_base_url(server.uri());
        let wav = temp_wav();
        let err = client
            .transcribe_wav(wav.path())
            .await
            .expect_err("oversized body should fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("too large") || msg.contains("exceeds"),
            "unexpected error: {msg}"
        );
    }
}
