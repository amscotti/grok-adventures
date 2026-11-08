//! CLI entry for Grok Adventures.

use anyhow::{Context, Result, bail};
use clap::Parser;
use grok_adventures::config::Config;
use grok_adventures::game::loop_::GameLoop;
use grok_adventures::game::player_agent::PlayerStyle;
use grok_adventures::game::save::SaveGame;
use grok_adventures::game::state::{ClassKit, GameState};
use grok_adventures::ui::UiMode;
use grok_adventures::ui::creation;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "grok-adventures",
    version,
    about = "CLI dungeon master — Grok narrates, Rust keeps score"
)]
struct Cli {
    /// Character name for a new adventure
    #[arg(long, short = 'n')]
    name: Option<String>,

    /// Character class for a new adventure: fighter|rogue|wizard|cleric|none
    /// (skips the interactive class picker)
    #[arg(long, value_name = "CLASS")]
    class: Option<String>,

    /// Load a save slot name or path
    #[arg(long)]
    load: Option<String>,

    /// Force minimal (linear) UI
    #[arg(long)]
    minimal: bool,

    /// Force fullscreen ratatui TUI (sticky status, scrollback, prompt)
    #[arg(long)]
    fullscreen: bool,

    /// Run AI player stress test
    #[arg(long)]
    autopilot: bool,

    /// Max autopilot turns
    #[arg(long, default_value_t = 20)]
    turns: u32,

    /// Force fixed action rotation instead of the Grok player agent
    /// (also: GROK_ADVENTURES_SCRIPTED_AUTOPILOT=1)
    #[arg(long)]
    scripted_autopilot: bool,

    /// Autopilot player personality: cautious | balanced | reckless
    /// (also: GROK_ADVENTURES_PLAYER_STYLE)
    #[arg(long, value_name = "STYLE")]
    player_style: Option<String>,

    /// Disable TTS for this session
    #[arg(long)]
    no_tts: bool,

    /// Disable STT for this session
    #[arg(long)]
    no_stt: bool,

    /// Path to config.toml
    #[arg(long, default_value = "config.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env if present (local XAI_API_KEY); ignore missing file.
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let mut config = Config::load(&cli.config).context("loading config")?;

    if cli.no_tts {
        config.voice.enable_tts = false;
    }
    if cli.no_stt {
        config.voice.enable_stt = false;
    }
    if cli.minimal {
        config.ui.mode = UiMode::Minimal;
    } else if cli.fullscreen {
        config.ui.mode = UiMode::Fullscreen;
    }

    if config.xai.api_key.is_empty() {
        bail!("XAI_API_KEY is not set. Put it in the environment or .env (see config.toml).");
    }

    let (mut state, resume) = if let Some(ref load_arg) = cli.load {
        let path = SaveGame::resolve_path(Some(load_arg));
        let loaded = SaveGame::load(&path).with_context(|| format!("loading save {path}"))?;
        (loaded, true)
    } else {
        // Character creation runs before either UI mode on a real terminal.
        // Scripts stay non-interactive: --name/--class, autopilot, or a
        // piped stdin never prompt (and never consume piped actions).
        let interactive = !cli.autopilot && creation::stdin_interactive();
        if interactive {
            creation::print_creation_header();
        }
        let name = resolve_character_name(&cli, interactive)?;
        let mut state = GameState::new(name);
        if let Some(kit) = resolve_character_class(&cli, interactive)? {
            match state.apply_class_kit(kit.key) {
                Ok(msg) => println!("{msg}"),
                Err(e) => eprintln!("Class kit skipped: {e}"),
            }
        }
        if interactive {
            creation::print_character_summary(&state);
        }
        (state, false)
    };

    if cli.autopilot {
        let scripted = cli.scripted_autopilot || scripted_autopilot_env();
        let style = resolve_player_style(&cli)?;
        GameLoop::run_autopilot_with_style(&mut state, &config, cli.turns, scripted, style).await?;
    } else {
        GameLoop::run_interactive(&mut state, &config, resume).await?;
    }

    Ok(())
}

/// `GROK_ADVENTURES_SCRIPTED_AUTOPILOT=1|true|yes` forces scripted autopilot.
fn scripted_autopilot_env() -> bool {
    matches!(
        std::env::var("GROK_ADVENTURES_SCRIPTED_AUTOPILOT").as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("TRUE") | Ok("YES")
    )
}

/// CLI `--player-style` overrides env `GROK_ADVENTURES_PLAYER_STYLE` (default balanced).
fn resolve_player_style(cli: &Cli) -> Result<PlayerStyle> {
    if let Some(ref s) = cli.player_style {
        return PlayerStyle::parse(s).ok_or_else(|| {
            anyhow::anyhow!("invalid --player-style '{s}' (expected cautious|balanced|reckless)")
        });
    }
    Ok(PlayerStyle::from_env())
}

/// Resolve character name: `--name` wins; interactive sessions prompt;
/// everything else (autopilot, pipes) keeps the default `"Adventurer"`.
fn resolve_character_name(cli: &Cli, interactive: bool) -> Result<String> {
    if let Some(ref name) = cli.name {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    if interactive {
        return creation::prompt_character_name();
    }
    Ok("Adventurer".to_string())
}

/// Resolve character class: `--class` wins (invalid values fail fast);
/// interactive sessions get the highlight picker; others start classless.
fn resolve_character_class(cli: &Cli, interactive: bool) -> Result<Option<&'static ClassKit>> {
    if let Some(ref value) = cli.class {
        return creation::parse_class_flag(value)
            .map_err(anyhow::Error::msg)
            .context("parsing --class");
    }
    if interactive {
        return creation::prompt_character_class();
    }
    Ok(None)
}
