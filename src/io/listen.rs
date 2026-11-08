//! Hold-to-talk capture that streams PCM to xAI and publishes the live line.
//!
//! If the websocket cannot be opened, the same recording is sent to the batch
//! `POST /v1/stt` endpoint after the mic stops.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::task::JoinHandle;

use crate::io::audio::{self, LivePcm};
use crate::io::stt::SttClient;
use crate::io::stt_stream::{
    PartialAssembler, StreamPoll, SttStream, SttStreamEvent, streaming_rate_supported,
};

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
    let (rate_tx, rate_rx) = mpsc::channel();
    let (pcm_tx, pcm_rx) = mpsc::channel();
    let stop_rec = Arc::clone(&stop);
    let rec = tokio::task::spawn_blocking(move || {
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
    let mut stream = open_stream(&stt, rate).await;
    let mut assembler = PartialAssembler::default();

    loop {
        let forward = forward_pcm(&mut pcm_rx, &mut stream).await;
        let mut failed = matches!(forward, Forward::SendFailed);
        if let Some(active) = &mut stream
            && matches!(pump_events(active, &mut assembler, &partial), Pump::Failed)
        {
            failed = true;
        }
        if failed {
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
        wait_for_done(&mut active, &mut assembler, &partial).await;
    }

    let path = rec.await.context("recording task join")??;
    let live = assembler.text().trim().to_string();
    if !live.is_empty() {
        let _ = std::fs::remove_file(&path);
        publish(&partial, &live);
        return Ok(live);
    }
    batch_transcribe(&stt, &path, &partial).await
}

enum Forward {
    Open,
    MicDone,
    SendFailed,
}

async fn forward_pcm(
    pcm_rx: &mut mpsc::Receiver<Vec<u8>>,
    stream: &mut Option<SttStream>,
) -> Forward {
    loop {
        match pcm_rx.try_recv() {
            Ok(chunk) => {
                if let Some(active) = stream.as_ref()
                    && active.send_pcm(chunk).await.is_err()
                {
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

async fn wait_for_done(
    stream: &mut SttStream,
    assembler: &mut PartialAssembler,
    partial: &Mutex<String>,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
    loop {
        if tokio::time::Instant::now() >= deadline {
            return;
        }
        match pump_events(stream, assembler, partial) {
            Pump::Continue => tokio::time::sleep(Duration::from_millis(30)).await,
            Pump::Finished | Pump::Failed => return,
        }
    }
}

async fn batch_transcribe(
    stt: &SttClient,
    path: &PathBuf,
    partial: &Mutex<String>,
) -> Result<String> {
    let batch = match stt.transcribe_wav(path).await {
        Ok(text) => text,
        Err(err) => {
            let _ = std::fs::remove_file(path);
            return Err(err);
        }
    };
    let _ = std::fs::remove_file(path);
    let batch = batch.trim().to_string();
    publish(partial, &batch);
    Ok(batch)
}

async fn open_stream(stt: &SttClient, rate: Option<u32>) -> Option<SttStream> {
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
    match SttStream::connect(
        stt.api_key(),
        stt.base_url(),
        stt.model(),
        stt.language(),
        rate,
    )
    .await
    {
        Ok(stream) => Some(stream),
        Err(err) => {
            tracing::warn!(error = %err, "streaming STT unavailable; using batch");
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
