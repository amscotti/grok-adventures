//! Configuration loaded from `config.toml` + environment.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::ui::UiMode;

/// Dungeon-master model. Latest Grok with tool calling (docs.x.ai, Sept 2026).
pub const DEFAULT_DM_MODEL: &str = "grok-4.7";
/// Autopilot player model. Same generation as the DM; the player request
/// pins reasoning effort to `low` so a one-line action stays cheap.
pub const DEFAULT_PLAYER_MODEL: &str = "grok-4.7";
/// Reasoning effort for DM turns.
///
/// The API default is `high`, which spends a lot of tokens thinking before
/// every tool hop. `low` is the documented setting for latency-sensitive
/// tool calling, which is what a turn of this game is.
pub const DEFAULT_REASONING_EFFORT: &str = "low";
/// Batch speech-to-text model. Latest xAI transcription model.
pub const DEFAULT_STT_MODEL: &str = "grok-voice-transcribe-2.0";

/// Top-level application configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub xai: XaiConfig,
    pub ui: UiConfig,
    pub voice: VoiceConfig,
    pub game: GameConfig,
    pub tts: TtsConfig,
    pub stt: SttConfig,
}

impl Config {
    /// Load configuration from `path`. Missing files use defaults; missing
    /// fields within a present file also use defaults. Prefer non-empty
    /// `XAI_API_KEY` env over the file value.
    pub fn load(path: impl AsRef<Path>) -> Result<Config> {
        let path = path.as_ref();
        let mut config = if path.exists() {
            let raw = fs::read_to_string(path)
                .with_context(|| format!("reading config from {}", path.display()))?;
            toml::from_str(&raw)
                .with_context(|| format!("parsing config from {}", path.display()))?
        } else {
            Config::default()
        };

        config.normalize_blanks();
        config.apply_env();
        config.canonicalize();
        Ok(config)
    }

    /// Fill blank model fields so an empty string in toml does not disable
    /// the built-in defaults.
    fn normalize_blanks(&mut self) {
        fill_blank(&mut self.xai.model, DEFAULT_DM_MODEL);
        fill_blank(&mut self.xai.player_model, DEFAULT_PLAYER_MODEL);
        fill_blank(&mut self.xai.reasoning_effort, DEFAULT_REASONING_EFFORT);
        fill_blank(&mut self.stt.model, DEFAULT_STT_MODEL);
        fill_blank(&mut self.stt.language, "en");
    }

    /// Environment wins over the file. Empty values are ignored.
    fn apply_env(&mut self) {
        env_override(&mut self.xai.api_key, "XAI_API_KEY");
        env_override(&mut self.xai.model, "XAI_MODEL");
        env_override(&mut self.xai.player_model, "XAI_PLAYER_MODEL");
        env_override(&mut self.xai.reasoning_effort, "XAI_REASONING_EFFORT");
        env_override(&mut self.stt.model, "XAI_STT_MODEL");
    }

    fn canonicalize(&mut self) {
        self.xai.model = self.xai.model.trim().to_string();
        self.xai.player_model = self.xai.player_model.trim().to_string();
        self.xai.reasoning_effort = self.xai.reasoning_effort.trim().to_ascii_lowercase();
        self.stt.model = self.stt.model.trim().to_string();
        self.stt.language = self.stt.language.trim().to_ascii_lowercase();
        self.normalize_blanks();
    }
}

fn fill_blank(slot: &mut String, fallback: &str) {
    if slot.trim().is_empty() {
        *slot = fallback.to_string();
    }
}

fn env_override(slot: &mut String, key: &str) {
    if let Ok(value) = std::env::var(key) {
        let value = value.trim();
        if !value.is_empty() {
            *slot = value.to_string();
        }
    }
}

/// xAI API credentials and model selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct XaiConfig {
    pub api_key: String,
    /// Chat-completions model for the dungeon master.
    pub model: String,
    /// Chat-completions model for the autopilot player agent.
    pub player_model: String,
    /// `low` | `medium` | `high` | `xhigh`. Sent as `reasoning_effort`.
    pub reasoning_effort: String,
}

impl Default for XaiConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: DEFAULT_DM_MODEL.to_string(),
            player_model: DEFAULT_PLAYER_MODEL.to_string(),
            reasoning_effort: DEFAULT_REASONING_EFFORT.to_string(),
        }
    }
}

/// UI presentation options.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub mode: UiMode,
    pub theme: String,
    pub compact: bool,
    pub tools_expanded: bool,
    pub show_shortcuts: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            mode: UiMode::Minimal,
            theme: "groknight".to_string(),
            compact: false,
            tools_expanded: false,
            show_shortcuts: true,
        }
    }
}

/// Voice I/O (STT / TTS) toggles and voice ids.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    pub enable_stt: bool,
    pub stt_edit_before_send: bool,
    pub enable_tts: bool,
    pub dm_voice: String,
    pub narrator_voice: String,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enable_stt: true,
            stt_edit_before_send: true,
            enable_tts: true,
            dm_voice: "zagan".to_string(),
            narrator_voice: "orion".to_string(),
        }
    }
}

/// Game tone / ruleset.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GameConfig {
    pub tone: String,
    pub ruleset: String,
    /// Prior DM exchanges kept as chat messages (user + assistant final text).
    /// `0` = no multi-turn history (state + log only, legacy behavior).
    pub history_turns: u32,
}

impl Default for GameConfig {
    fn default() -> Self {
        Self {
            tone: "heroic".to_string(),
            ruleset: "dnd5e".to_string(),
            history_turns: 8,
        }
    }
}

/// TTS output format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TtsConfig {
    pub format: String,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            format: "mp3".to_string(),
        }
    }
}

/// STT capture device settings and transcription model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SttConfig {
    pub device: i32,
    pub sample_rate: u32,
    /// Default push-to-talk / fixed record length (seconds).
    pub record_seconds: u32,
    /// Hard cap for hold-to-talk / Enter-to-stop recording (seconds).
    pub max_record_seconds: u32,
    /// `POST /v1/stt` model id.
    pub model: String,
    /// Language code passed with `format=true` (inverse text normalization).
    pub language: String,
    /// Stream partial transcripts over the websocket while recording.
    /// When false, audio is uploaded in one batch after the mic stops.
    pub streaming: bool,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            device: -1,
            sample_rate: 16_000,
            record_seconds: 12,
            max_record_seconds: 60,
            model: DEFAULT_STT_MODEL.to_string(),
            language: "en".to_string(),
            streaming: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn load_missing_file_uses_defaults() {
        let path = std::env::temp_dir().join(format!(
            "grok_adventures_missing_config_{}.toml",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let cfg = Config::load(&path).expect("defaults");
        assert_eq!(cfg.ui.theme, "groknight");
        assert_eq!(cfg.game.history_turns, 8);
        assert_eq!(cfg.stt.sample_rate, 16_000);
        assert_eq!(cfg.stt.record_seconds, 12);
        assert_eq!(cfg.stt.max_record_seconds, 60);
        assert!(!cfg.stt.model.is_empty());
        assert!(!cfg.xai.model.is_empty());
    }

    #[test]
    fn defaults_pin_current_xai_models() {
        let cfg = Config::default();
        assert_eq!(cfg.xai.model, "grok-4.7");
        assert_eq!(cfg.xai.player_model, "grok-4.7");
        assert_eq!(cfg.xai.reasoning_effort, "low");
        assert_eq!(cfg.stt.model, "grok-voice-transcribe-2.0");
        assert_eq!(cfg.stt.language, "en");
        assert!(cfg.stt.streaming);
    }

    #[test]
    fn blank_model_fields_fall_back_to_defaults() {
        let mut cfg = Config::default();
        cfg.xai.model.clear();
        cfg.xai.player_model = "  ".into();
        cfg.xai.reasoning_effort.clear();
        cfg.stt.model.clear();
        cfg.stt.language.clear();
        cfg.normalize_blanks();
        cfg.canonicalize();
        assert_eq!(cfg.xai.model, DEFAULT_DM_MODEL);
        assert_eq!(cfg.xai.player_model, DEFAULT_PLAYER_MODEL);
        assert_eq!(cfg.xai.reasoning_effort, DEFAULT_REASONING_EFFORT);
        assert_eq!(cfg.stt.model, DEFAULT_STT_MODEL);
        assert_eq!(cfg.stt.language, "en");
    }

    #[test]
    fn load_partial_toml_fills_defaults() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
[ui]
mode = "fullscreen"
theme = "grokday"

[game]
tone = "comedic"
"#
        )
        .unwrap();

        let cfg = Config::load(file.path()).expect("parse");
        assert_eq!(cfg.ui.mode, UiMode::Fullscreen);
        assert_eq!(cfg.ui.theme, "grokday");
        assert!(!cfg.ui.compact);
        assert_eq!(cfg.game.tone, "comedic");
        assert_eq!(cfg.game.ruleset, "dnd5e");
        assert_eq!(cfg.voice.dm_voice, "zagan");
        assert_eq!(cfg.tts.format, "mp3");
    }

    #[test]
    fn load_matches_project_config_toml_shape() {
        // Project root config.toml (when present) should parse cleanly.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.toml");
        if root.exists() {
            let cfg = Config::load(&root).expect("project config");
            assert!(matches!(cfg.ui.mode, UiMode::Minimal | UiMode::Fullscreen));
            assert!(!cfg.game.ruleset.is_empty());
        }
    }
}
