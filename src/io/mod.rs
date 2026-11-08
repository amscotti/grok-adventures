//! Voice I/O: microphone capture (STT) and speech output (TTS).

pub mod audio;
pub mod listen;
pub mod stt;
pub mod stt_stream;
pub mod tts;

pub use audio::{
    ActivePlayback, PlaybackHandle, clamp_record_seconds, is_silence, list_input_device_names,
    play_mp3, play_mp3_bytes, play_mp3_bytes_cancellable, play_mp3_cancellable, record_seconds,
    record_until, recording_stats, write_wav_i16,
};
pub use listen::ListenHandle;
pub use stt::SttClient;
pub use tts::{
    TtsClient, build_tts_request_body, split_sentences, strip_markdown_for_tts, truncate_tts_text,
};
