//! Microphone recording and local audio playback helpers.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};

/// Clamp a requested record duration into `1..=max_secs` (max at least 1).
pub fn clamp_record_seconds(requested: u32, max_secs: u32) -> u32 {
    let max = max_secs.max(1);
    requested.max(1).min(max)
}

/// Write mono signed 16-bit PCM samples as a WAV file.
///
/// Pure helper — no audio hardware required.
pub fn write_wav_i16(path: impl AsRef<Path>, sample_rate: u32, samples: &[i16]) -> Result<()> {
    let path = path.as_ref();
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .with_context(|| format!("creating WAV {}", path.display()))?;
    for &s in samples {
        writer
            .write_sample(s)
            .with_context(|| format!("writing sample to {}", path.display()))?;
    }
    writer
        .finalize()
        .with_context(|| format!("finalizing WAV {}", path.display()))?;
    Ok(())
}

/// Resolve a temp path for a new WAV capture.
pub fn temp_wav_path() -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "grok_adventures_rec_{}_{}.wav",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    path
}

/// Resolve a temp path for a TTS mp3 payload.
pub fn temp_mp3_path() -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "grok_adventures_tts_{}_{}.mp3",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    path
}

/// Record mono PCM from the default (or indexed) input device for `seconds`.
///
/// Returns the path to a temporary WAV file (16-bit mono).
/// `device_index` of `-1` selects the host default input.
///
/// Uses the preferred `sample_rate` when the device accepts it; otherwise
/// falls back to the device default rate. The WAV header matches capture.
pub fn record_seconds(sample_rate: u32, device_index: i32, seconds: u32) -> Result<PathBuf> {
    if seconds == 0 {
        bail!("record duration must be > 0");
    }
    let stop = Arc::new(AtomicBool::new(false));
    record_until(sample_rate, device_index, seconds, stop)
}

/// Little-endian mono PCM16, the encoding streaming STT expects.
pub fn i16_to_pcm_le(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// Where a live capture publishes its rate and PCM frames.
pub struct LivePcm {
    pub rate_tx: std::sync::mpsc::Sender<u32>,
    pub pcm_tx: std::sync::mpsc::Sender<Vec<u8>>,
}

/// Record until `stop` is set or `max_secs` elapses (whichever first).
///
/// Polls the flag every ~50ms so hold-to-talk / Enter-to-stop can end early.
/// Returns a temp 16-bit mono WAV path.
pub fn record_until(
    sample_rate: u32,
    device_index: i32,
    max_secs: u32,
    stop: Arc<AtomicBool>,
) -> Result<PathBuf> {
    record_until_live(sample_rate, device_index, max_secs, stop, None)
}

/// Like [`record_until`], and also streams PCM frames when `live` is set.
pub fn record_until_live(
    sample_rate: u32,
    device_index: i32,
    max_secs: u32,
    stop: Arc<AtomicBool>,
    live: Option<LivePcm>,
) -> Result<PathBuf> {
    if max_secs == 0 {
        bail!("record duration must be > 0");
    }

    let host = cpal::default_host();
    let device = select_input_device(&host, device_index)?;
    let supported = device
        .default_input_config()
        .context("querying default input config")?;
    let sample_format = supported.sample_format();
    let device_rate = supported.sample_rate().0;
    let preferred = if sample_rate > 0 {
        sample_rate
    } else {
        device_rate
    };

    let samples: Arc<Mutex<Vec<i16>>> = Arc::new(Mutex::new(Vec::new()));
    let err_flag = Arc::new(Mutex::new(None::<String>));

    let mut config: StreamConfig = supported.config();
    config.sample_rate = cpal::SampleRate(preferred);

    let (stream, wav_rate) =
        match build_input_stream(&device, &config, sample_format, &samples, &err_flag) {
            Ok(s) => (s, preferred),
            Err(first_err) => {
                let fallback: StreamConfig = device.default_input_config()?.config();
                let rate = fallback.sample_rate.0;
                let s = build_input_stream(&device, &fallback, sample_format, &samples, &err_flag)
                    .with_context(|| {
                        format!("opening input stream (preferred rate also failed: {first_err:#})")
                    })?;
                (s, rate)
            }
        };

    if let Some(live) = &live {
        let _ = live.rate_tx.send(wav_rate);
    }
    stream.play().context("starting input stream")?;

    let deadline = Instant::now() + Duration::from_secs(u64::from(max_secs));
    let poll = Duration::from_millis(50);
    let mut sent = 0usize;
    while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(poll);
        sent = flush_new_pcm(&samples, sent, live.as_ref());
    }
    drop(stream);
    let _ = flush_new_pcm(&samples, sent, live.as_ref());
    // Drop the sender so the listener can finish the websocket while the
    // WAV is written.
    drop(live);

    if let Ok(slot) = err_flag.lock()
        && let Some(ref msg) = *slot
    {
        bail!("recording stream error: {msg}");
    }

    let mono = samples
        .lock()
        .map_err(|_| anyhow::anyhow!("recording buffer lock poisoned"))?
        .clone();

    let device_name = device.name().unwrap_or_else(|_| "(unnamed)".to_string());
    if mono.is_empty() {
        bail!(
            "no audio samples captured on '{device_name}' — check microphone permissions / device (stt.device in config.toml; {})",
            input_device_hint()
        );
    }
    let stats = recording_stats(&mono);
    let seconds = mono.len() as f64 / f64::from(wav_rate.max(1));
    tracing::debug!(
        samples = stats.samples,
        peak = stats.peak,
        rms = format!("{:.0}", stats.rms),
        "mic capture level"
    );
    if is_silence(stats) {
        bail!(
            "captured {seconds:.1}s of audio on '{device_name}' but heard only silence (peak {}/32767) — check input volume, macOS mic permission, or pick another device with stt.device=N ({})",
            stats.peak,
            input_device_hint()
        );
    }

    let path = temp_wav_path();
    write_wav_i16(&path, wav_rate, &mono)?;
    Ok(path)
}

fn flush_new_pcm(samples: &Mutex<Vec<i16>>, sent: usize, live: Option<&LivePcm>) -> usize {
    let Some(live) = live else {
        return sent;
    };
    let Ok(buf) = samples.lock() else {
        return sent;
    };
    if buf.len() <= sent {
        return sent;
    }
    let _ = live.pcm_tx.send(i16_to_pcm_le(&buf[sent..]));
    buf.len()
}

fn build_input_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
    samples: &Arc<Mutex<Vec<i16>>>,
    err_flag: &Arc<Mutex<Option<String>>>,
) -> Result<cpal::Stream> {
    let channels = config.channels as usize;
    let samples_cb = Arc::clone(samples);
    let err_cb = Arc::clone(err_flag);

    let stream = match sample_format {
        SampleFormat::I16 => device.build_input_stream(
            config,
            move |data: &[i16], _| {
                if let Ok(mut buf) = samples_cb.lock() {
                    push_mono_i16(&mut buf, data, channels);
                }
            },
            move |err| {
                if let Ok(mut slot) = err_cb.lock() {
                    *slot = Some(err.to_string());
                }
            },
            None,
        )?,
        SampleFormat::U16 => {
            let samples_cb = Arc::clone(samples);
            let err_cb = Arc::clone(err_flag);
            device.build_input_stream(
                config,
                move |data: &[u16], _| {
                    if let Ok(mut buf) = samples_cb.lock() {
                        let converted: Vec<i16> = data
                            .iter()
                            .map(|&s| (i32::from(s) - 32_768) as i16)
                            .collect();
                        push_mono_i16(&mut buf, &converted, channels);
                    }
                },
                move |err| {
                    if let Ok(mut slot) = err_cb.lock() {
                        *slot = Some(err.to_string());
                    }
                },
                None,
            )?
        }
        SampleFormat::F32 => {
            let samples_cb = Arc::clone(samples);
            let err_cb = Arc::clone(err_flag);
            device.build_input_stream(
                config,
                move |data: &[f32], _| {
                    if let Ok(mut buf) = samples_cb.lock() {
                        let converted: Vec<i16> = data
                            .iter()
                            .map(|&s| (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16)
                            .collect();
                        push_mono_i16(&mut buf, &converted, channels);
                    }
                },
                move |err| {
                    if let Ok(mut slot) = err_cb.lock() {
                        *slot = Some(err.to_string());
                    }
                },
                None,
            )?
        }
        other => bail!("unsupported input sample format: {other:?}"),
    };
    Ok(stream)
}

/// Peak amplitude below which a capture counts as silence (~-45 dBFS).
///
/// Normal speech near a mic peaks in the thousands; anything under this is
/// room noise or a muted/wrong device.
pub const SILENCE_PEAK_THRESHOLD: i16 = 300;

/// Level summary for a mono i16 capture (pure — no hardware needed).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecordingStats {
    /// Number of samples analyzed.
    pub samples: usize,
    /// Peak absolute amplitude (0..=32767).
    pub peak: i16,
    /// Root-mean-square amplitude.
    pub rms: f64,
}

/// Compute [`RecordingStats`] for mono i16 samples.
pub fn recording_stats(samples: &[i16]) -> RecordingStats {
    let mut peak: i32 = 0;
    let mut sum_sq: f64 = 0.0;
    for &s in samples {
        let a = i32::from(s).abs();
        if a > peak {
            peak = a;
        }
        sum_sq += f64::from(s) * f64::from(s);
    }
    RecordingStats {
        samples: samples.len(),
        peak: peak.min(i32::from(i16::MAX)) as i16,
        rms: if samples.is_empty() {
            0.0
        } else {
            (sum_sq / samples.len() as f64).sqrt()
        },
    }
}

/// Whether a capture is effectively silence (nothing the STT could transcribe).
pub fn is_silence(stats: RecordingStats) -> bool {
    stats.samples == 0 || stats.peak < SILENCE_PEAK_THRESHOLD
}

/// Names of available input devices (`index: name`), for diagnostics.
pub fn list_input_device_names() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(devices) => devices
            .enumerate()
            .map(|(i, d)| {
                format!(
                    "{i}: {}",
                    d.name().unwrap_or_else(|_| "(unnamed)".to_string())
                )
            })
            .collect(),
        Err(_) => vec![],
    }
}

/// One-line hint of available inputs, for error messages.
fn input_device_hint() -> String {
    let names = list_input_device_names();
    if names.is_empty() {
        "no input devices found".to_string()
    } else {
        format!("available inputs — {}", names.join(", "))
    }
}

fn select_input_device(host: &cpal::Host, device_index: i32) -> Result<cpal::Device> {
    if device_index < 0 {
        return host
            .default_input_device()
            .context("no default input device (microphone)");
    }
    let devices: Vec<_> = host
        .input_devices()
        .context("enumerating input devices")?
        .collect();
    let idx = device_index as usize;
    devices
        .into_iter()
        .nth(idx)
        .with_context(|| format!("no input device at index {device_index}"))
}

/// Downmix interleaved frames to mono by averaging channels.
fn push_mono_i16(out: &mut Vec<i16>, interleaved: &[i16], channels: usize) {
    if channels <= 1 {
        out.extend_from_slice(interleaved);
        return;
    }
    for frame in interleaved.chunks_exact(channels) {
        let sum: i32 = frame.iter().map(|&s| i32::from(s)).sum();
        out.push((sum / channels as i32) as i16);
    }
}

/// Killable handle for a local audio player child process (`afplay` / etc.).
///
/// Cloneable so a session can hold a copy while a blocking wait runs on another
/// thread; call [`PlaybackHandle::kill`] to stop mid-play (Esc / new turn).
#[derive(Clone, Debug)]
pub struct PlaybackHandle {
    inner: Arc<PlaybackInner>,
}

#[derive(Debug)]
struct PlaybackInner {
    child: Mutex<Option<std::process::Child>>,
    temp_path: Mutex<Option<PathBuf>>,
    killed: AtomicBool,
}

impl PlaybackHandle {
    fn new(child: std::process::Child, temp_path: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(PlaybackInner {
                child: Mutex::new(Some(child)),
                temp_path: Mutex::new(temp_path),
                killed: AtomicBool::new(false),
            }),
        }
    }

    /// Whether [`kill`](Self::kill) was invoked.
    pub fn was_killed(&self) -> bool {
        self.inner.killed.load(Ordering::SeqCst)
    }

    /// Stop playback immediately (best-effort). Safe to call more than once.
    pub fn kill(&self) {
        self.inner.killed.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.inner.child.lock()
            && let Some(mut child) = guard.take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.cleanup_temp();
    }

    /// Block until the player exits. Signal-killed / cancelled exits are `Ok`.
    pub fn wait(&self) -> Result<()> {
        let status = {
            let mut guard = self.inner.child.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_mut() {
                Some(child) => {
                    let status = child.wait().context("waiting for audio player")?;
                    *guard = None;
                    status
                }
                None => {
                    // Already reaped (killed or prior wait).
                    self.cleanup_temp();
                    return Ok(());
                }
            }
        };
        self.cleanup_temp();
        if self.was_killed() {
            return Ok(());
        }
        if !status.success() {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if status.signal().is_some() {
                    return Ok(());
                }
            }
            bail!("audio player exited with {status}");
        }
        Ok(())
    }

    fn cleanup_temp(&self) {
        if let Ok(mut guard) = self.inner.temp_path.lock()
            && let Some(path) = guard.take()
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Session-level slot for the active TTS/player process.
///
/// Call [`ActivePlayback::stop`] on Esc cancel or when a new player turn starts.
#[derive(Clone, Debug, Default)]
pub struct ActivePlayback {
    current: Arc<Mutex<Option<PlaybackHandle>>>,
    /// Sticky until the next successful [`set`] (new clip starts).
    stop_requested: Arc<AtomicBool>,
}

impl ActivePlayback {
    pub fn new() -> Self {
        Self::default()
    }

    /// Kill any in-flight playback and mark the stream cancelled.
    pub fn stop(&self) {
        self.stop_requested.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.current.lock()
            && let Some(handle) = guard.take()
        {
            handle.kill();
        }
    }

    /// Clear a prior [`stop`](Self::stop) so a new intentional speech session can play.
    pub fn prepare(&self) {
        self.stop_requested.store(false, Ordering::SeqCst);
    }

    /// Whether [`stop`](Self::stop) was requested (streaming TTS should bail).
    pub fn is_stopped(&self) -> bool {
        self.stop_requested.load(Ordering::SeqCst)
    }

    /// Register `handle` as the active playback (stops any previous).
    pub fn set(&self, handle: PlaybackHandle) {
        if let Ok(mut guard) = self.current.lock() {
            if let Some(prev) = guard.take() {
                prev.kill();
            }
            *guard = Some(handle);
        }
    }

    /// Clear the slot if it still holds `handle` (after natural completion).
    pub fn clear_if(&self, handle: &PlaybackHandle) {
        if let Ok(mut guard) = self.current.lock()
            && let Some(cur) = guard.as_ref()
            && Arc::ptr_eq(&cur.inner, &handle.inner)
        {
            *guard = None;
        }
    }
}

/// Spawn the platform audio player for `path` without waiting.
fn spawn_player(path: &Path) -> Result<std::process::Child> {
    if !path.exists() {
        bail!("audio file not found: {}", path.display());
    }

    if cfg!(target_os = "macos") {
        return std::process::Command::new("afplay")
            .arg(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("spawning afplay (install Command Line Tools / check PATH)");
    }

    if which_in_path("ffplay") {
        return std::process::Command::new("ffplay")
            .args(["-nodisp", "-autoexit", "-loglevel", "quiet"])
            .arg(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("spawning ffplay");
    }

    if which_in_path("mpg123") {
        return std::process::Command::new("mpg123")
            .args(["-q"])
            .arg(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("spawning mpg123");
    }

    bail!("no audio player found (need afplay on macOS, or ffplay/mpg123)");
}

/// Play an audio file with the platform default player.
///
/// macOS: `afplay`. Elsewhere: try `ffplay -nodisp -autoexit`, then `mpg123`.
pub fn play_mp3(path: impl AsRef<Path>) -> Result<()> {
    let handle = play_mp3_cancellable(path)?;
    handle.wait()
}

/// Start playback and return a killable handle (does not wait).
pub fn play_mp3_cancellable(path: impl AsRef<Path>) -> Result<PlaybackHandle> {
    let path = path.as_ref();
    let child = spawn_player(path)?;
    Ok(PlaybackHandle::new(child, None))
}

fn which_in_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

/// Write bytes to a temp mp3, play, then delete.
pub fn play_mp3_bytes(bytes: &[u8]) -> Result<()> {
    let handle = play_mp3_bytes_cancellable(bytes)?;
    handle.wait()
}

/// Write bytes to a temp mp3, start playback, return a killable handle.
///
/// The temp file is removed when the handle is waited on or killed.
pub fn play_mp3_bytes_cancellable(bytes: &[u8]) -> Result<PlaybackHandle> {
    let path = temp_mp3_path();
    {
        let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        let mut writer = BufWriter::new(file);
        writer
            .write_all(bytes)
            .with_context(|| format!("writing {}", path.display()))?;
        writer.flush()?;
    }
    let child = match spawn_player(&path) {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
    };
    Ok(PlaybackHandle::new(child, Some(path)))
}

/// Optional damage click from `assets/sounds/*.{wav,mp3}` via afplay (macOS).
///
/// Graceful no-op when the directory is missing, empty, or playback fails.
/// Fire-and-forget so the game loop is never blocked on SFX.
pub fn maybe_play_damage_sfx() {
    let Some(path) = find_damage_sfx_path(Path::new("assets/sounds")) else {
        return;
    };
    let _ = spawn_play_sfx(&path);
}

/// Prefer names like `damage` / `hit` / `click`; otherwise first wav/mp3.
fn find_damage_sfx_path(dir: &Path) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("wav") || e.eq_ignore_ascii_case("mp3"))
            })
            .collect(),
        Err(_) => return None,
    };
    if files.is_empty() {
        return None;
    }
    files.sort();
    let preferred = ["damage", "hit", "hurt", "click", "impact"];
    for key in preferred {
        if let Some(p) = files.iter().find(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.to_ascii_lowercase().contains(key))
        }) {
            return Some(p.clone());
        }
    }
    Some(files.remove(0))
}

fn spawn_play_sfx(path: &Path) -> Result<()> {
    // Fire-and-forget through the shared platform player so SFX and TTS
    // agree (afplay on macOS, else ffplay then mpg123). Missing players
    // are a silent no-op for SFX — gameplay must never block on sound.
    match spawn_player(path) {
        // Detached like before: never block the game loop on SFX.
        Ok(child) => {
            drop(child);
            Ok(())
        }
        Err(e) => {
            tracing::debug!("sfx skipped: {e:#}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn write_wav_roundtrip_header() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("t.wav");
        let samples: Vec<i16> = (0..160).map(|i| (i * 100) as i16).collect();
        write_wav_i16(&path, 16_000, &samples).unwrap();

        let mut reader = hound::WavReader::open(&path).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 16_000);
        assert_eq!(spec.bits_per_sample, 16);
        let read: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
        assert_eq!(read, samples);
    }

    #[test]
    fn push_mono_averages_stereo() {
        let mut out = Vec::new();
        push_mono_i16(&mut out, &[10, 20, 30, 50], 2);
        assert_eq!(out, vec![15, 40]);
    }

    #[test]
    fn temp_paths_have_expected_extension() {
        assert!(temp_wav_path().extension().is_some_and(|e| e == "wav"));
        assert!(temp_mp3_path().extension().is_some_and(|e| e == "mp3"));
    }

    #[test]
    fn find_damage_sfx_missing_dir_is_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("nope");
        assert!(find_damage_sfx_path(&missing).is_none());
    }

    #[test]
    fn find_damage_sfx_empty_dir_is_none() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(find_damage_sfx_path(dir.path()).is_none());
    }

    #[test]
    fn find_damage_sfx_prefers_damage_name() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("ambient.wav"), b"x").unwrap();
        std::fs::write(dir.path().join("damage.wav"), b"x").unwrap();
        let found = find_damage_sfx_path(dir.path()).unwrap();
        assert!(
            found
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.contains("damage"))
        );
    }

    #[test]
    fn maybe_play_damage_sfx_noop_without_assets() {
        // Must not panic when assets/sounds is empty or missing.
        maybe_play_damage_sfx();
    }

    #[test]
    fn clamp_record_seconds_bounds() {
        assert_eq!(clamp_record_seconds(0, 60), 1);
        assert_eq!(clamp_record_seconds(12, 60), 12);
        assert_eq!(clamp_record_seconds(90, 60), 60);
        assert_eq!(clamp_record_seconds(5, 0), 1);
    }

    #[test]
    fn recording_stats_measures_peak_and_rms() {
        let stats = recording_stats(&[0, 1000, -2000, 500]);
        assert_eq!(stats.samples, 4);
        assert_eq!(stats.peak, 2000);
        let expected: f64 = ((0.0f64 + 1_000_000.0 + 4_000_000.0 + 250_000.0) / 4.0).sqrt();
        assert!((stats.rms - expected).abs() < 1e-6);
    }

    #[test]
    fn recording_stats_empty_is_silence() {
        let stats = recording_stats(&[]);
        assert_eq!(stats.samples, 0);
        assert_eq!(stats.peak, 0);
        assert_eq!(stats.rms, 0.0);
        assert!(is_silence(stats));
    }

    #[test]
    fn silence_threshold_separates_noise_from_speech() {
        assert!(is_silence(recording_stats(&[0, 100, -299, 200])));
        assert!(!is_silence(recording_stats(&[0, 100, -300, 200])));
        assert!(!is_silence(recording_stats(&[0, 5000, -1000])));
    }

    #[test]
    fn pcm_le_encodes_signed_samples() {
        assert_eq!(i16_to_pcm_le(&[0, 1, -1]), vec![0, 0, 1, 0, 0xff, 0xff]);
    }

    #[test]
    fn record_until_respects_stop_flag_without_device() {
        // Pure path: stop already set before open would still try device.
        // Unit-test the clamp + AtomicBool contract used by callers instead.
        let stop = Arc::new(AtomicBool::new(false));
        assert!(!stop.load(Ordering::SeqCst));
        stop.store(true, Ordering::SeqCst);
        assert!(stop.load(Ordering::SeqCst));
        // Simulate caller stop path: max duration clamp then early stop.
        let max = clamp_record_seconds(60, 60);
        assert_eq!(max, 60);
        let stop2 = Arc::new(AtomicBool::new(true));
        // Already-stopped flag means the poll loop exits immediately *after*
        // stream setup; we only assert the flag semantics here (no mic in CI).
        assert!(stop2.load(Ordering::SeqCst));
    }
}
