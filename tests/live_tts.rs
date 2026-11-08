//! Live xAI TTS/STT smoke tests. Ignored by default — requires XAI_API_KEY.
//!
//! Run with:
//!   XAI_API_KEY=... cargo test --test live_tts -- --ignored --nocapture

use std::io::Write;

use grok_adventures::io::audio::{play_mp3_bytes, write_wav_i16};
use grok_adventures::io::stt::SttClient;
use grok_adventures::io::tts::TtsClient;

#[tokio::test]
#[ignore = "billed live API; set XAI_API_KEY and run with --ignored"]
async fn tts_synthesize_returns_audio() {
    let api_key = std::env::var("XAI_API_KEY").expect("XAI_API_KEY must be set for live test");
    assert!(!api_key.is_empty());

    let client = TtsClient::new(api_key);
    let audio = client
        .synthesize("Hello from Grok Adventures TTS smoke test.", "zagan")
        .await
        .expect("TTS synthesize");

    assert!(
        audio.len() > 100,
        "expected non-trivial audio bytes, got {}",
        audio.len()
    );
    eprintln!("TTS bytes: {}", audio.len());

    // Optional playback — ignore failure if no afplay in CI.
    if let Err(e) = play_mp3_bytes(&audio) {
        eprintln!("playback skipped/failed (ok for CI): {e:#}");
    }
}

#[tokio::test]
#[ignore = "billed live API; set XAI_API_KEY and run with --ignored"]
async fn stt_transcribe_silence_wav() {
    let api_key = std::env::var("XAI_API_KEY").expect("XAI_API_KEY must be set for live test");
    assert!(!api_key.is_empty());

    // 0.5s of near-silence at 16 kHz mono.
    let samples = vec![0i16; 8_000];
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("silence.wav");
    write_wav_i16(&path, 16_000, &samples).unwrap();

    let client = SttClient::new(api_key);
    let text = client
        .transcribe_wav(&path)
        .await
        .expect("STT should accept a valid WAV even if empty speech");
    eprintln!("STT transcript (may be empty): {text:?}");
    // No assertion on content — silence may yield empty string.
    let _ = text;
}

#[tokio::test]
#[ignore = "billed live API + writes request body fixture; set XAI_API_KEY"]
async fn tts_request_body_matches_contract() {
    // Pure contract check lives in unit tests; this writes a sample for debugging.
    let body = grok_adventures::io::tts::build_tts_request_body("Hi", "zagan", "en");
    let mut f = tempfile::NamedTempFile::new().unwrap();
    writeln!(f, "{body}").unwrap();
    assert_eq!(body["voice_id"], "zagan");
}
