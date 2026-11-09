//! Hold-to-talk capture that streams PCM to xAI and publishes the live line.
//!
//! If the websocket cannot be opened — or fails mid-utterance — the same
//! recording is sent to the batch `POST /v1/stt` endpoint after the mic stops.
//! If batch also fails, a non-empty live partial is kept instead of dropping
//! the utterance.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::io::audio::{self, LivePcm};
use crate::io::stt::SttClient;
use crate::io::stt_stream::{
    PartialAssembler, StreamPoll, SttStream, SttStreamEvent, streaming_rate_supported,
};

/// Hard cap for one listen: capture, stream finalize, and batch fallback.
const LISTEN_HARD_CAP: Duration = Duration::from_secs(90);
/// Budget for opening the streaming socket (connect + ready handshake).
const STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(15);
/// Budget for the server to finalize the utterance after `audio.done`.
const STREAM_FINALIZE_TIMEOUT: Duration = Duration::from_secs(12);

/// Earliest of the stage limit and the overall listen deadline.
fn stage_deadline(now: Instant, hard_deadline: Instant, limit: Duration) -> Instant {
    hard_deadline.min(now + limit)
}

/// Where the final transcript should come from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TranscriptPlan {
    /// Streaming produced a healthy live result; use it as-is.
    Live,
    /// Prefer batch transcription. `fallback` holds a non-empty live partial
    /// to rescue when the batch call fails.
    Batch { fallback: Option<String> },
}

/// Decide between the live streaming transcript and batch transcription.
fn plan_transcript(live: &str, stream_failed: bool) -> TranscriptPlan {
    let live = live.trim();
    if !stream_failed && !live.is_empty() {
        return TranscriptPlan::Live;
    }
    TranscriptPlan::Batch {
        fallback: (!live.is_empty()).then(|| live.to_string()),
    }
}

/// Keep the live partial when batch transcription fails, if one exists.
fn choose_after_batch<E>(
    batch: std::result::Result<String, E>,
    fallback: Option<String>,
) -> std::result::Result<String, E> {
    match batch {
        Ok(text) => Ok(text),
        Err(err) => match fallback {
            Some(text) if !text.is_empty() => Ok(text),
            _ => Err(err),
        },
    }
}

/// In-progress listen. The partial line updates while [`Self::finish`] is pending.
pub struct ListenHandle {
    stop: Arc<AtomicBool>,
    partial: Arc<Mutex<String>>,
    task: JoinHandle<Result<String>>,
}

impl ListenHandle {
    /// Start the microphone and, when streaming is enabled, the websocket.
    pub fn start(stt: SttClient, sample_rate: u32, device: i32, max_secs: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let partial = Arc::new(Mutex::new(String::new()));
        let stop_task = Arc::clone(&stop);
        let partial_task = Arc::clone(&partial);
        let task = tokio::spawn(async move {
            run_listen(
                stt,
                sample_rate,
                device,
                max_secs.max(1),
                stop_task,
                partial_task,
            )
            .await
        });
        Self {
            stop,
            partial,
            task,
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Latest live line. Empty until the first partial arrives.
    pub fn partial(&self) -> String {
        self.partial
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default()
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    pub async fn finish(self) -> Result<String> {
        self.stop.store(true, Ordering::SeqCst);
        self.task.await.context("listen task join")?
    }
}

async fn run_listen(
    stt: SttClient,
    sample_rate: u32,
    device: i32,
    max_secs: u32,
    stop: Arc<AtomicBool>,
    partial: Arc<Mutex<String>>,
) -> Result<String> {
    let hard_deadline = Instant::now() + LISTEN_HARD_CAP;
    let (rate_tx, rate_rx) = mpsc::channel();
    let (pcm_tx, pcm_rx) = mpsc::channel();
    let stop_rec = Arc::clone(&stop);
    let mut rec = tokio::task::spawn_blocking(move || {
        audio::record_until_live(
            sample_rate,
            device,
            max_secs,
            stop_rec,
            Some(LivePcm { rate_tx, pcm_tx }),
        )
    });

    let mut rate_rx = rate_rx;
    let mut pcm_rx = pcm_rx;
    let rate = wait_rate(&mut rate_rx, &rec).await;
    let mut stream = open_stream(&stt, rate, hard_deadline).await;
    let mut assembler = PartialAssembler::default();
    let mut stream_failed = false;

    loop {
        let forward = forward_pcm(&mut pcm_rx, &mut stream, hard_deadline).await;
        let mut failed = matches!(forward, Forward::SendFailed);
        if let Some(active) = &mut stream
            && matches!(pump_events(active, &mut assembler, &partial), Pump::Failed)
        {
            failed = true;
        }
        if failed {
            stream_failed = true;
            tracing::warn!("streaming STT failed mid-utterance; using batch fallback");
            stream = None;
        }
        if matches!(forward, Forward::MicDone) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    if let Some(mut active) = stream.take() {
        active.finish_audio();
        if !wait_for_done(&mut active, &mut assembler, &partial, hard_deadline).await {
            stream_failed = true;
        }
    }

    let path = match tokio::time::timeout_at(hard_deadline, &mut rec).await {
        Ok(joined) => joined.context("recording task join")??,
        Err(_) => {
            tracing::warn!(
                cap_secs = LISTEN_HARD_CAP.as_secs(),
                "listen exceeded hard cap; stopping capture"
            );
            stop.store(true, Ordering::SeqCst);
            // A spawn_blocking recorder cannot be aborted; it polls `stop`
            // every 50ms and then writes the WAV, so await it for cleanup.
            (&mut rec).await.context("recording task join")??
        }
    };
    let live = assembler.text().trim().to_string();
    match plan_transcript(&live, stream_failed) {
        TranscriptPlan::Live => {
            let _ = std::fs::remove_file(&path);
            publish(&partial, &live);
            Ok(live)
        }
        TranscriptPlan::Batch { fallback } => {
            let batch = batch_transcribe(&stt, &path, hard_deadline).await;
            if let (Err(err), Some(_)) = (&batch, &fallback) {
                tracing::warn!(error = %err, "batch STT failed; keeping live partial");
            }
            let chosen = choose_after_batch(batch, fallback);
            // The recording is only obsolete once the final source is known.
            let _ = std::fs::remove_file(&path);
            let text = chosen.map_err(|err| err.context("batch STT transcription failed"))?;
            publish(&partial, &text);
            Ok(text)
        }
    }
}

enum Forward {
    Open,
    MicDone,
    SendFailed,
}

async fn forward_pcm(
    pcm_rx: &mut mpsc::Receiver<Vec<u8>>,
    stream: &mut Option<SttStream>,
    hard_deadline: Instant,
) -> Forward {
    loop {
        if Instant::now() >= hard_deadline {
            tracing::warn!(
                cap_secs = LISTEN_HARD_CAP.as_secs(),
                "listen exceeded hard cap while forwarding audio"
            );
            return Forward::SendFailed;
        }
        match pcm_rx.try_recv() {
            Ok(chunk) => {
                if let Some(active) = stream.as_ref()
                    && let Err(err) = active.send_pcm(chunk).await
                {
                    tracing::warn!(error = %err, "streaming STT send failed");
                    return Forward::SendFailed;
                }
            }
            Err(TryRecvError::Empty) => return Forward::Open,
            Err(TryRecvError::Disconnected) => return Forward::MicDone,
        }
    }
}

enum Pump {
    Continue,
    Finished,
    Failed,
}

fn pump_events(
    stream: &mut SttStream,
    assembler: &mut PartialAssembler,
    partial: &Mutex<String>,
) -> Pump {
    loop {
        match stream.poll() {
            StreamPoll::Event(SttStreamEvent::Ready) => {}
            StreamPoll::Event(SttStreamEvent::Partial {
                text,
                is_final,
                speech_final,
            }) => {
                let shown = assembler.push(&text, is_final, speech_final);
                publish(partial, &shown);
            }
            StreamPoll::Event(SttStreamEvent::Done { text }) => {
                let shown = assembler.finish(&text);
                publish(partial, &shown);
                return Pump::Finished;
            }
            StreamPoll::Event(SttStreamEvent::Error { message }) => {
                tracing::warn!(%message, "streaming STT error");
                return Pump::Failed;
            }
            StreamPoll::Idle => return Pump::Continue,
            StreamPoll::Closed => return Pump::Failed,
        }
    }
}

/// Wait for the server's final transcript.
///
/// Returns `true` when the stream delivered `Done`, `false` when finalization
/// failed or hit its bounded deadline (caller then prefers batch).
async fn wait_for_done(
    stream: &mut SttStream,
    assembler: &mut PartialAssembler,
    partial: &Mutex<String>,
    hard_deadline: Instant,
) -> bool {
    let deadline = stage_deadline(Instant::now(), hard_deadline, STREAM_FINALIZE_TIMEOUT);
    let finalize = async {
        loop {
            match pump_events(stream, assembler, partial) {
                Pump::Continue => tokio::time::sleep(Duration::from_millis(30)).await,
                Pump::Finished => return true,
                Pump::Failed => return false,
            }
        }
    };
    match tokio::time::timeout_at(deadline, finalize).await {
        Ok(true) => true,
        Ok(false) => {
            tracing::warn!("streaming STT failed during finalization; using batch fallback");
            false
        }
        Err(_) => {
            tracing::warn!(
                timeout_secs = STREAM_FINALIZE_TIMEOUT.as_secs(),
                "streaming STT finalization timed out; using batch fallback"
            );
            false
        }
    }
}

/// Transcribe the recorded WAV, bounded by the overall listen deadline.
///
/// Cleanup is left to the caller so the recording survives until the final
/// transcript source has been selected.
async fn batch_transcribe(stt: &SttClient, path: &Path, hard_deadline: Instant) -> Result<String> {
    match tokio::time::timeout_at(hard_deadline, stt.transcribe_wav(path)).await {
        Ok(Ok(text)) => Ok(text.trim().to_string()),
        Ok(Err(err)) => Err(err),
        Err(_) => {
            tracing::warn!(
                cap_secs = LISTEN_HARD_CAP.as_secs(),
                "listen exceeded hard cap during batch transcription"
            );
            Err(anyhow::anyhow!(
                "listen exceeded {}s hard cap during batch transcription",
                LISTEN_HARD_CAP.as_secs()
            ))
        }
    }
}

async fn open_stream(
    stt: &SttClient,
    rate: Option<u32>,
    hard_deadline: Instant,
) -> Option<SttStream> {
    if !stt.streaming() {
        return None;
    }
    let rate = rate?;
    if !streaming_rate_supported(rate) {
        tracing::warn!(
            rate,
            "mic rate is not supported by streaming STT; using batch"
        );
        return None;
    }
    let deadline = stage_deadline(Instant::now(), hard_deadline, STREAM_OPEN_TIMEOUT);
    match tokio::time::timeout_at(
        deadline,
        SttStream::connect(
            stt.api_key(),
            stt.base_url(),
            stt.model(),
            stt.language(),
            rate,
        ),
    )
    .await
    {
        Ok(Ok(stream)) => Some(stream),
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "streaming STT unavailable; using batch");
            None
        }
        Err(_) => {
            tracing::warn!(
                timeout_secs = STREAM_OPEN_TIMEOUT.as_secs(),
                "streaming STT open timed out; using batch"
            );
            None
        }
    }
}

fn publish(partial: &Mutex<String>, text: &str) {
    if let Ok(mut slot) = partial.lock() {
        *slot = text.to_string();
    }
}

async fn wait_rate(rx: &mut mpsc::Receiver<u32>, rec: &JoinHandle<Result<PathBuf>>) -> Option<u32> {
    let start = tokio::time::Instant::now();
    loop {
        match rx.try_recv() {
            Ok(rate) => return Some(rate),
            Err(TryRecvError::Disconnected) => return None,
            Err(TryRecvError::Empty) => {
                if start.elapsed() > Duration::from_secs(8) || rec.is_finished() {
                    return rx.try_recv().ok();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_result_is_used_when_stream_is_healthy() {
        assert_eq!(plan_transcript("hello there", false), TranscriptPlan::Live);
    }

    #[test]
    fn stream_failure_prefers_batch_but_keeps_partial() {
        assert_eq!(
            plan_transcript(" hello there ", true),
            TranscriptPlan::Batch {
                fallback: Some("hello there".to_string())
            }
        );
    }

    #[test]
    fn empty_partial_uses_batch_without_fallback() {
        assert_eq!(
            plan_transcript("", false),
            TranscriptPlan::Batch { fallback: None }
        );
        assert_eq!(
            plan_transcript("   ", true),
            TranscriptPlan::Batch { fallback: None }
        );
    }

    #[test]
    fn batch_failure_falls_back_to_live_partial() {
        let chosen = choose_after_batch::<&str>(Err("network"), Some("live text".into()));
        assert_eq!(chosen, Ok("live text".to_string()));
    }

    #[test]
    fn batch_failure_without_partial_surfaces_the_error() {
        let chosen = choose_after_batch::<&str>(Err("network"), None);
        assert_eq!(chosen, Err("network"));
    }

    #[test]
    fn stage_deadline_never_exceeds_hard_cap() {
        let now = Instant::now();
        let hard = now + Duration::from_secs(5);

        assert_eq!(
            stage_deadline(now, hard, Duration::from_secs(15)),
            hard,
            "stage limit must be clipped to the overall hard cap"
        );
        assert_eq!(
            stage_deadline(now, hard, Duration::from_secs(2)),
            now + Duration::from_secs(2)
        );
    }
}
