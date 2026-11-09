//! xAI text-to-speech client (`POST /v1/tts`).

use std::fmt;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::io::audio::{ActivePlayback, play_mp3_bytes, play_mp3_bytes_cancellable};
use crate::util::http::{api_client, read_body_bounded, read_text_bounded};
use crate::util::redact::redact_secrets;

const DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Hard cap on one TTS audio body (20 MiB).
const MAX_TTS_AUDIO_BYTES: usize = 20 * 1024 * 1024;
/// Hard cap on a TTS error body (64 KiB).
const MAX_TTS_ERROR_BODY_BYTES: usize = 64 * 1024;
/// xAI TTS text length limit (chars).
pub const MAX_TTS_CHARS: usize = 15_000;

/// Strip common markdown markers so TTS does not read asterisks aloud.
pub fn strip_markdown_for_tts(text: &str) -> String {
    let mut final_out = String::with_capacity(text.len());
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            final_out.push('\n');
        }
        final_out.push_str(&strip_line_markdown(line));
    }
    // Preserve a trailing newline only if the original had content ending that way
    // and we produced something.
    final_out
}

fn strip_line_markdown(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' | '`' => {
                while chars.peek() == Some(&'*') || chars.peek() == Some(&'`') {
                    chars.next();
                }
            }
            '_' => {
                // Drop underscore emphasis markers.
            }
            '#' if out.is_empty() => {
                while chars.peek() == Some(&'#') {
                    chars.next();
                }
                if chars.peek() == Some(&' ') {
                    chars.next();
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// Truncate text to the TTS API limit, preferring a word boundary.
pub fn truncate_tts_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_chars).collect();
    if let Some(idx) = truncated.rfind(|c: char| c.is_whitespace()) {
        truncated[..idx].trim_end().to_string()
    } else {
        truncated
    }
}

/// Split text into sentences for pipeline TTS.
///
/// Breaks on `. `, `! `, `? `, and newlines. Punctuation stays with the
/// preceding sentence. Empty segments are dropped.
pub fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();

    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\n' || c == '\r' {
            let trimmed = current.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
            current.clear();
            // Skip paired \r\n
            if c == '\r' && i + 1 < chars.len() && chars[i + 1] == '\n' {
                i += 1;
            }
            i += 1;
            continue;
        }

        current.push(c);

        // Sentence end: `.` / `!` / `?` followed by whitespace or end of text.
        if matches!(c, '.' | '!' | '?') {
            let at_end = i + 1 >= chars.len();
            let next_ws = i + 1 < chars.len() && chars[i + 1].is_whitespace();
            if at_end || next_ws {
                let trimmed = current.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
                current.clear();
                // Consume the following space (keep newlines for the branch above).
                if next_ws && chars[i + 1] == ' ' {
                    i += 1;
                }
            }
        }
        i += 1;
    }

    let trimmed = current.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
    }
    out
}

/// Build the JSON body for a TTS request (pure, testable).
pub fn build_tts_request_body(text: &str, voice_id: &str, language: &str) -> Value {
    let cleaned = strip_markdown_for_tts(text);
    let text = truncate_tts_text(&cleaned, MAX_TTS_CHARS);
    json!({
        "text": text,
        "voice_id": voice_id,
        "language": language,
        "output_format": {
            "codec": "mp3",
            "sample_rate": 24000,
            "bit_rate": 128000
        }
    })
}

/// HTTP client for xAI TTS.
#[derive(Clone)]
pub struct TtsClient {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl fmt::Debug for TtsClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TtsClient")
            .field("client", &self.client)
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .finish()
    }
}

impl TtsClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: api_client(REQUEST_TIMEOUT),
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Synthesize speech; returns raw audio bytes (mp3 by default).
    pub async fn synthesize(&self, text: &str, voice_id: &str) -> Result<Vec<u8>> {
        let body = build_tts_request_body(text, voice_id, "en");
        let text_field = body.get("text").and_then(|v| v.as_str()).unwrap_or("");
        if text_field.trim().is_empty() {
            bail!("TTS text is empty after cleanup");
        }

        let url = format!("{}/tts", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("sending TTS request")?;

        let status = resp.status();
        if !status.is_success() {
            let err_body = read_text_bounded(resp, MAX_TTS_ERROR_BODY_BYTES, "TTS error").await?;
            bail!("TTS API error {status}: {}", redact_secrets(&err_body));
        }

        let bytes = read_body_bounded(resp, MAX_TTS_AUDIO_BYTES, "TTS audio")
            .await
            .context("reading TTS audio body")?;
        if bytes.is_empty() {
            bail!("TTS API returned empty audio body");
        }
        Ok(bytes)
    }

    /// Synthesize and play via local player (single request).
    pub async fn speak(&self, text: &str, voice_id: &str) -> Result<()> {
        self.speak_with_playback(text, voice_id, None).await
    }

    /// Like [`speak`](Self::speak), optionally registering a killable handle on
    /// `playback` so Esc / a new turn can stop `afplay`.
    pub async fn speak_with_playback(
        &self,
        text: &str,
        voice_id: &str,
        playback: Option<&ActivePlayback>,
    ) -> Result<()> {
        if let Some(p) = playback {
            p.prepare();
        }
        let audio = self.synthesize(text, voice_id).await?;
        play_audio_bytes(&audio, playback).await
    }

    /// Pipeline TTS: synthesize sentence N+1 while playing sentence N.
    ///
    /// xAI `/v1/tts` is request/response; this overlaps playback with the next
    /// synthesis via a small mpsc queue and `spawn_blocking` for `afplay`.
    pub async fn speak_streaming(&self, text: &str, voice_id: &str) -> Result<()> {
        self.speak_streaming_with_playback(text, voice_id, None)
            .await
    }

    /// Like [`speak_streaming`](Self::speak_streaming) with cancellable playback.
    pub async fn speak_streaming_with_playback(
        &self,
        text: &str,
        voice_id: &str,
        playback: Option<&ActivePlayback>,
    ) -> Result<()> {
        if let Some(p) = playback {
            p.prepare();
        }
        let cleaned = strip_markdown_for_tts(text);
        let cleaned = truncate_tts_text(&cleaned, MAX_TTS_CHARS);
        if cleaned.trim().is_empty() {
            return Ok(());
        }

        let sentences = split_sentences(&cleaned);
        if sentences.is_empty() {
            return Ok(());
        }
        if sentences.len() == 1 {
            // prepare already called; avoid double prepare by going direct.
            let audio = self.synthesize(&sentences[0], voice_id).await?;
            return play_audio_bytes(&audio, playback).await;
        }

        // Capacity 2: one playing / one buffered while the next synth runs.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
        let client = self.clone();
        let voice = voice_id.to_string();

        let synth_task = tokio::spawn(async move {
            let mut first_err: Option<anyhow::Error> = None;
            for sentence in sentences {
                match client.synthesize(&sentence, &voice).await {
                    Ok(bytes) => {
                        if tx.send(bytes).await.is_err() {
                            break; // consumer dropped
                        }
                    }
                    Err(e) => {
                        first_err = Some(e);
                        break;
                    }
                }
            }
            drop(tx);
            match first_err {
                Some(e) => Err(e),
                None => Ok(()),
            }
        });

        while let Some(audio) = rx.recv().await {
            if playback.is_some_and(|p| p.is_stopped()) {
                drop(rx);
                break;
            }
            play_audio_bytes(&audio, playback).await?;
            if playback.is_some_and(|p| p.is_stopped()) {
                drop(rx);
                break;
            }
        }

        // Best-effort: abort remaining synthesis if we stopped early.
        synth_task.abort();
        let _ = synth_task.await;
        Ok(())
    }

    /// Speak but skip empty text. Uses streaming for multi-sentence input.
    pub async fn speak_soft(&self, text: &str, voice_id: &str) -> Result<()> {
        self.speak_soft_with_playback(text, voice_id, None).await
    }

    /// Like [`speak_soft`](Self::speak_soft) with cancellable playback.
    pub async fn speak_soft_with_playback(
        &self,
        text: &str,
        voice_id: &str,
        playback: Option<&ActivePlayback>,
    ) -> Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        let cleaned = strip_markdown_for_tts(text);
        let sentences = split_sentences(&cleaned);
        if sentences.len() > 1 {
            self.speak_streaming_with_playback(text, voice_id, playback)
                .await
        } else {
            self.speak_with_playback(text, voice_id, playback).await
        }
    }
}

async fn play_audio_bytes(audio: &[u8], playback: Option<&ActivePlayback>) -> Result<()> {
    if playback.is_some_and(|p| p.is_stopped()) {
        return Ok(());
    }
    let audio = audio.to_vec();
    match playback {
        Some(slot) => {
            let slot = slot.clone();
            tokio::task::spawn_blocking(move || {
                if slot.is_stopped() {
                    return Ok(());
                }
                let handle = play_mp3_bytes_cancellable(&audio)?;
                slot.set(handle.clone());
                let result = handle.wait();
                slot.clear_if(&handle);
                result
            })
            .await
            .context("TTS playback task join")??;
            Ok(())
        }
        None => {
            tokio::task::spawn_blocking(move || play_mp3_bytes(&audio))
                .await
                .context("TTS playback task join")??;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn strip_bold_and_italic() {
        assert_eq!(
            strip_markdown_for_tts("You find a **sword** and *shield*."),
            "You find a sword and shield."
        );
    }

    #[test]
    fn strip_code_and_headings() {
        let out = strip_markdown_for_tts("## Hall\nTake the `key`.");
        assert!(out.contains("Hall"));
        assert!(out.contains("Take the key."));
        assert!(!out.contains('`'));
        assert!(!out.contains('#'));
    }

    #[test]
    fn truncate_respects_limit() {
        let long = "word ".repeat(20);
        let t = truncate_tts_text(&long, 20);
        assert!(t.chars().count() <= 20);
        assert!(!t.ends_with(' '));
    }

    #[test]
    fn truncate_noop_when_short() {
        assert_eq!(truncate_tts_text("hi", 100), "hi");
    }

    #[test]
    fn request_body_shape() {
        let body = build_tts_request_body("Hello **world**", "zagan", "en");
        assert_eq!(body["voice_id"], "zagan");
        assert_eq!(body["language"], "en");
        assert_eq!(body["text"], "Hello world");
        assert_eq!(body["output_format"]["codec"], "mp3");
        assert_eq!(body["output_format"]["sample_rate"], 24000);
        assert_eq!(body["output_format"]["bit_rate"], 128000);
    }

    #[test]
    fn request_body_truncates_long_text() {
        let long = "a".repeat(MAX_TTS_CHARS + 500);
        let body = build_tts_request_body(&long, "orion", "en");
        let text = body["text"].as_str().unwrap();
        assert!(text.chars().count() <= MAX_TTS_CHARS);
    }

    #[test]
    fn split_sentences_on_punctuation_and_newlines() {
        let parts = split_sentences("Hello there. How are you? Fine!\nNext line.");
        assert_eq!(
            parts,
            vec!["Hello there.", "How are you?", "Fine!", "Next line.",]
        );
    }

    #[test]
    fn split_sentences_single_and_empty() {
        assert_eq!(split_sentences("Just one"), vec!["Just one"]);
        assert!(split_sentences("   ").is_empty());
        assert!(split_sentences("").is_empty());
    }

    #[test]
    fn split_sentences_keeps_abbreviation_dots_without_space() {
        // "Dr.Strange" stays one segment; "Dr. " would split.
        let parts = split_sentences("See Dr.Strange now. Go.");
        assert_eq!(parts, vec!["See Dr.Strange now.", "Go."]);
    }

    #[test]
    fn debug_never_prints_the_api_key() {
        const FAKE_KEY: &str = "xai-tts-debug-SECRET-1122334455";
        let client = TtsClient::new(FAKE_KEY);
        let out = format!("{client:?}");
        assert!(!out.contains(FAKE_KEY), "debug leaked key: {out}");
        assert!(out.contains("<redacted>"));
    }

    #[tokio::test]
    async fn tts_error_body_is_redacted() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const FAKE_KEY: &str = "xai-tts-error-SECRET-5566778899";
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tts"))
            .respond_with(
                ResponseTemplate::new(401).set_body_string(format!("bad token api_key={FAKE_KEY}")),
            )
            .mount(&server)
            .await;

        let client = TtsClient::new("test-key").with_base_url(server.uri());
        let err = client
            .synthesize("hello", "voice")
            .await
            .expect_err("401 should fail");
        let msg = format!("{err:#}");
        assert!(!msg.contains(FAKE_KEY), "error leaked key: {msg}");
        assert!(msg.contains("[REDACTED]"), "error was not redacted: {msg}");
    }

    #[tokio::test]
    async fn tts_error_body_is_capped() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = "e".repeat(MAX_TTS_ERROR_BODY_BYTES + 1);
        Mock::given(method("POST"))
            .and(path("/tts"))
            .respond_with(ResponseTemplate::new(500).set_body_string(body))
            .mount(&server)
            .await;

        let client = TtsClient::new("test-key").with_base_url(server.uri());
        let err = client
            .synthesize("hello", "voice")
            .await
            .expect_err("oversized error body should fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("too large") || msg.contains("exceeds"),
            "unexpected error: {msg}"
        );
    }
}
