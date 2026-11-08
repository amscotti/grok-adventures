//! Streaming speech-to-text over `wss://api.x.ai/v1/stt`.
//!
//! Interim results show up while the speaker is still talking. Chunk-final
//! pieces are locked and stitched; a speech-final (or `transcript.done`)
//! replaces that preview with the server's full utterance.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use url::Url;

/// Rates the streaming API accepts for raw PCM.
pub fn streaming_rate_supported(rate: u32) -> bool {
    matches!(rate, 8000 | 16_000 | 22_050 | 24_000 | 44_100 | 48_000)
}

/// Build the WebSocket URL for one streaming session.
pub fn stt_ws_url(http_base: &str, model: &str, language: &str, sample_rate: u32) -> Result<Url> {
    let endpoint = ws_endpoint(http_base);
    Url::parse_with_params(
        &endpoint,
        &[
            ("model", model),
            ("sample_rate", &sample_rate.to_string()),
            ("encoding", "pcm"),
            ("interim_results", "true"),
            ("language", language),
        ],
    )
    .with_context(|| format!("building STT websocket URL from {endpoint}"))
}

fn ws_endpoint(http_base: &str) -> String {
    let base = http_base.trim().trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}/v1/stt")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}/v1/stt")
    } else if base.starts_with("wss://") || base.starts_with("ws://") {
        format!("{base}/v1/stt")
    } else {
        "wss://api.x.ai/v1/stt".to_string()
    }
}

/// Stitches streaming partials into the line shown while the user is speaking.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialAssembler {
    /// Locked chunk-final text. Later interims append after this.
    locked: String,
    latest: String,
}

impl PartialAssembler {
    pub fn text(&self) -> &str {
        &self.latest
    }

    /// Fold one `transcript.partial` into the live line.
    pub fn push(&mut self, text: &str, is_final: bool, speech_final: bool) -> String {
        let text = text.trim();
        if text.is_empty() {
            return self.latest.clone();
        }
        if speech_final {
            self.locked.clear();
            self.latest = text.to_string();
            return self.latest.clone();
        }
        if is_final {
            if !self.locked.is_empty() {
                self.locked.push(' ');
            }
            self.locked.push_str(text);
            self.latest = self.locked.clone();
            return self.latest.clone();
        }
        self.latest = if self.locked.is_empty() {
            text.to_string()
        } else {
            format!("{} {text}", self.locked)
        };
        self.latest.clone()
    }

    /// Prefer the `transcript.done` text when the server sends one.
    pub fn finish(&mut self, done: &str) -> String {
        let done = done.trim();
        if !done.is_empty() {
            self.locked.clear();
            self.latest = done.to_string();
        }
        self.latest.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SttStreamEvent {
    Ready,
    Partial {
        text: String,
        is_final: bool,
        speech_final: bool,
    },
    Done {
        text: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug)]
pub enum StreamPoll {
    Event(SttStreamEvent),
    Idle,
    Closed,
}

/// One streaming recognition session. Drop aborts the socket tasks.
pub struct SttStream {
    audio_tx: Option<mpsc::Sender<Vec<u8>>>,
    event_rx: mpsc::Receiver<SttStreamEvent>,
    writer: JoinHandle<()>,
    reader: JoinHandle<()>,
}

impl SttStream {
    pub async fn connect(
        api_key: &str,
        http_base: &str,
        model: &str,
        language: &str,
        sample_rate: u32,
    ) -> Result<Self> {
        install_crypto();
        let url = stt_ws_url(http_base, model, language, sample_rate)?;
        let mut request = url
            .as_str()
            .into_client_request()
            .context("building STT websocket request")?;
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {api_key}")
                .parse()
                .context("STT auth header")?,
        );

        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(15),
            tokio_tungstenite::connect_async(request),
        )
        .await
        .context("STT websocket connect timed out")?
        .context("STT websocket connect")?;

        let (mut ws_write, mut ws_read) = ws.split();
        let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(64);
        let (event_tx, event_rx) = mpsc::channel::<SttStreamEvent>(64);

        let writer = tokio::spawn(async move {
            while let Some(chunk) = audio_rx.recv().await {
                if ws_write.send(Message::Binary(chunk.into())).await.is_err() {
                    return;
                }
            }
            let _ = ws_write
                .send(Message::Text(r#"{"type":"audio.done"}"#.into()))
                .await;
        });

        let reader = tokio::spawn(async move {
            loop {
                match ws_read.next().await {
                    Some(Ok(Message::Text(text))) => {
                        let event = parse_server_event(&text);
                        if let Some(event) = event
                            && event_tx.send(event).await.is_err()
                        {
                            break;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(err)) => {
                        if !is_benign_disconnect(&err) {
                            let _ = event_tx
                                .send(SttStreamEvent::Error {
                                    message: err.to_string(),
                                })
                                .await;
                        }
                        break;
                    }
                    None => break,
                }
            }
        });

        let mut session = Self {
            audio_tx: Some(audio_tx),
            event_rx,
            writer,
            reader,
        };
        match tokio::time::timeout(Duration::from_secs(10), session.event_rx.recv()).await {
            Ok(Some(SttStreamEvent::Ready)) => Ok(session),
            Ok(Some(SttStreamEvent::Error { message })) => {
                bail!("streaming STT: {message}");
            }
            Ok(Some(_)) => bail!("streaming STT sent a transcript before it was ready"),
            Ok(None) => bail!("streaming STT closed before it was ready"),
            Err(_) => bail!("timed out waiting for streaming STT to be ready"),
        }
    }

    pub async fn send_pcm(&self, bytes: Vec<u8>) -> Result<()> {
        let Some(tx) = &self.audio_tx else {
            bail!("streaming STT audio channel is closed");
        };
        tx.send(bytes)
            .await
            .context("streaming STT audio channel closed")?;
        Ok(())
    }

    pub fn poll(&mut self) -> StreamPoll {
        match self.event_rx.try_recv() {
            Ok(event) => StreamPoll::Event(event),
            Err(mpsc::error::TryRecvError::Empty) => StreamPoll::Idle,
            Err(mpsc::error::TryRecvError::Disconnected) => StreamPoll::Closed,
        }
    }

    /// Stop sending audio. The writer then sends `audio.done`.
    pub fn finish_audio(&mut self) {
        self.audio_tx.take();
    }
}

impl Drop for SttStream {
    fn drop(&mut self) {
        self.writer.abort();
        self.reader.abort();
    }
}

fn install_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn is_benign_disconnect(err: &impl std::fmt::Display) -> bool {
    let text = err.to_string().to_ascii_lowercase();
    text.contains("closed") || text.contains("reset without")
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerEvent {
    #[serde(rename = "transcript.created")]
    Created {},
    #[serde(rename = "transcript.partial")]
    Partial {
        #[serde(default)]
        text: String,
        #[serde(default)]
        is_final: bool,
        #[serde(default)]
        speech_final: bool,
    },
    #[serde(rename = "transcript.done")]
    Done {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        message: String,
    },
    #[serde(other)]
    Unknown,
}

fn parse_server_event(text: &str) -> Option<SttStreamEvent> {
    match serde_json::from_str::<ServerEvent>(text) {
        Ok(ServerEvent::Created {}) => Some(SttStreamEvent::Ready),
        Ok(ServerEvent::Partial {
            text,
            is_final,
            speech_final,
        }) => Some(SttStreamEvent::Partial {
            text,
            is_final,
            speech_final,
        }),
        Ok(ServerEvent::Done { text }) => Some(SttStreamEvent::Done { text }),
        Ok(ServerEvent::Error { message }) => Some(SttStreamEvent::Error { message }),
        Ok(ServerEvent::Unknown) => None,
        Err(err) => Some(SttStreamEvent::Error {
            message: format!("parse error: {err}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_pins_model_and_interim_results() {
        let url = stt_ws_url(
            "https://api.x.ai/v1",
            "grok-voice-transcribe-2.0",
            "en",
            16_000,
        )
        .unwrap();
        let s = url.to_string();
        assert!(s.starts_with("wss://api.x.ai/v1/stt?"), "{s}");
        assert!(s.contains("model=grok-voice-transcribe-2.0"), "{s}");
        assert!(s.contains("interim_results=true"), "{s}");
        assert!(s.contains("encoding=pcm"), "{s}");
        assert!(s.contains("sample_rate=16000"), "{s}");
        assert!(s.contains("language=en"), "{s}");
    }

    #[test]
    fn http_base_becomes_ws() {
        let url = stt_ws_url("http://127.0.0.1:9/v1", "m", "en", 16_000).unwrap();
        assert!(url.as_str().starts_with("ws://127.0.0.1:9/v1/stt?"));
    }

    #[test]
    fn assembler_stitches_chunks_then_prefers_the_full_utterance() {
        let mut asm = PartialAssembler::default();
        assert_eq!(asm.push("open", false, false), "open");
        assert_eq!(asm.push("open the", false, false), "open the");
        assert_eq!(asm.push("open the door", true, false), "open the door");
        assert_eq!(asm.push("and", false, false), "open the door and");
        assert_eq!(
            asm.push("open the door and look", true, true),
            "open the door and look"
        );
        assert_eq!(
            asm.finish("open the door and look around"),
            "open the door and look around"
        );
    }

    #[test]
    fn parse_partial_and_done() {
        let ev = parse_server_event(
            r#"{"type":"transcript.partial","text":"hello","is_final":false,"speech_final":false}"#,
        );
        assert_eq!(
            ev,
            Some(SttStreamEvent::Partial {
                text: "hello".into(),
                is_final: false,
                speech_final: false,
            })
        );
        let ev = parse_server_event(r#"{"type":"transcript.done","text":"hello there"}"#);
        assert_eq!(
            ev,
            Some(SttStreamEvent::Done {
                text: "hello there".into()
            })
        );
        assert_eq!(
            parse_server_event(r#"{"type":"transcript.created"}"#),
            Some(SttStreamEvent::Ready)
        );
    }
}
