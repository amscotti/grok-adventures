//! Interactive game loop and autopilot harness.
//!
//! Tools first, markers as safety net, UI as a pure presentation leaf.
//! Slash commands live in [`crate::game::commands`] and are re-exported here.
//! Presentation helpers return `Vec<Block>` so minimal (print) and fullscreen
//! (scrollback) share the same paths; DM speech (TTS) hangs off the same turn
//! pipeline.

use std::io::{self, BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::dm::{ChatHistory, DmClient, ToolCallRecord, TurnResult, UsageTotals};
use crate::game::delta::{self, StateSnapshot};
use crate::game::markers;
use crate::game::player_agent::{self, PlayerAgent, PlayerStyle};
use crate::game::save::SaveGame;
use crate::game::state::GameState;
use crate::io::audio::{self, ActivePlayback, clamp_record_seconds};
use crate::io::listen::ListenHandle;
use crate::io::stt::SttClient;
use crate::io::tts::TtsClient;
use crate::ui::UiMode;
use crate::ui::blocks::{self, Block, SystemKind, ToolCallView};
use crate::ui::status;
use crate::ui::theme;
use crate::util::clipboard;

// Re-exported so the TUI and tests can keep using `crate::game::loop_::…`.
pub use crate::game::commands::{
    MetaResult, SessionFlags, handle_meta_command, handle_state_query, handle_state_query_blocks,
    health_condition, is_local_query, parse_command,
};

/// Inputs for one player → DM action presentation (minimal path).
struct ActionTurn<'a> {
    input: &'a str,
    spoken: bool,
    tone: &'a str,
    dm_voice: &'a str,
    enable_tts: bool,
    tools_expanded: bool,
    history: &'a mut ChatHistory,
    playback: &'a ActivePlayback,
}

/// Stable id for xAI `x-grok-conv-id` prompt-cache routing (one per session).
fn session_conv_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("ga-{}-{nanos}", std::process::id())
}

/// Build a DM client from config, with session-scoped prompt-cache routing.
pub fn make_dm_client(config: &Config) -> DmClient {
    DmClient::with_model(&config.xai.api_key, &config.xai.model)
        .with_reasoning_effort(&config.xai.reasoning_effort)
        .with_conv_id(session_conv_id())
}

pub(crate) fn stt_client(config: &Config) -> SttClient {
    SttClient::new(config.xai.api_key.clone())
        .with_model(&config.stt.model)
        .with_language(&config.stt.language)
        .with_streaming(config.stt.streaming)
}

/// Orchestrates turns: input → DM tools → markers → UI.
pub struct GameLoop;

impl GameLoop {
    /// Interactive session: keyboard always; optional STT via `talk` / `/talk`;
    /// optional DM speech (TTS).
    /// Dispatches to fullscreen TUI when `config.ui.mode == Fullscreen`.
    pub async fn run_interactive(
        state: &mut GameState,
        config: &Config,
        resume: bool,
    ) -> Result<()> {
        if config.ui.mode == UiMode::Fullscreen {
            return crate::ui::fullscreen::run_fullscreen_interactive(state, config, resume).await;
        }
        run_minimal_interactive(state, config, resume).await
    }

    /// AI-vs-AI style stress test.
    ///
    /// By default drives a Grok player agent from state + last DM beat.
    /// Pass `scripted: true` (CLI `--scripted-autopilot`) to force the fixed
    /// action rotation — free/offline-friendly relative to a second model.
    /// Autopilot: no STT/TTS. Always minimal UI.
    pub async fn run_autopilot(
        state: &mut GameState,
        config: &Config,
        turns: u32,
        scripted: bool,
    ) -> Result<()> {
        Self::run_autopilot_with_style(state, config, turns, scripted, PlayerStyle::from_env())
            .await
    }

    /// Autopilot with an explicit player personality (CLI / env).
    pub async fn run_autopilot_with_style(
        state: &mut GameState,
        config: &Config,
        turns: u32,
        scripted: bool,
        player_style: PlayerStyle,
    ) -> Result<()> {
        let client = make_dm_client(config);
        let mut history = ChatHistory::new(config.game.history_turns);
        let player_agent = if scripted {
            None
        } else {
            Some(
                PlayerAgent::with_model(config.xai.api_key.clone(), &config.xai.player_model)
                    .with_style(player_style),
            )
        };
        let player_model = player_agent
            .as_ref()
            .map(|a| a.model().to_string())
            .unwrap_or_else(|| "(scripted)".to_string());
        let tools_expanded = true;
        let tone = config.game.tone.as_str();

        let mode_label = if scripted {
            "scripted exploration".to_string()
        } else {
            format!("player {player_model}, style {}", player_style.as_str())
        };
        blocks::print_session_header(
            "Grok Adventures",
            &format!(
                "autopilot  ·  dm {} · {}  ·  {mode_label}",
                client.model(),
                client.reasoning_effort()
            ),
            &format!("{turns} turns"),
        );

        let session = start_session(state, &client, false, &true, tone, None, &mut history).await?;
        let mut last_dm = session.text.clone();

        let mut total_tool_calls = session.tool_calls.len() as u32;
        let mut total_usage = session.usage;
        let mut violations: Vec<(u32, String)> = Vec::new();
        let mut completed = 0u32;
        let mut agent_fallbacks = 0u32;

        for turn in 1..=turns {
            if !state.alive() {
                blocks::print_block(
                    &Block::System {
                        text: format!("Death on autopilot turn {turn}"),
                        kind: SystemKind::Error,
                    },
                    tools_expanded,
                );
                break;
            }

            let (action, used_fallback) = if let Some(ref agent) = player_agent {
                agent.next_action(state, &last_dm, turn).await
            } else {
                (player_agent::scripted_action(turn), false)
            };
            if used_fallback {
                agent_fallbacks += 1;
            }

            let source = if scripted {
                "scripted"
            } else if used_fallback {
                "fallback"
            } else {
                "agent"
            };
            println!(
                "{}",
                theme::dim(&format!(
                    "[Autopilot turn {turn}/{turns} · {source}] {action}"
                ))
            );

            blocks::print_block(
                &Block::Player {
                    text: action.clone(),
                    spoken: false,
                },
                tools_expanded,
            );

            let before = StateSnapshot::from(&*state);
            let result = match run_dm_turn(&client, state, &action, tone, &mut history).await {
                Ok(r) => r,
                Err(e) => {
                    blocks::print_block(
                        &Block::System {
                            text: format!("API error on turn {turn}: {e:#}"),
                            kind: SystemKind::Error,
                        },
                        tools_expanded,
                    );
                    break;
                }
            };

            last_dm = result.text.clone();
            present_turn(state, &result, &before, tools_expanded);
            total_tool_calls += result.tool_calls.len() as u32;
            total_usage.add(&result.usage);
            completed = turn;

            for v in state.invariant_violations() {
                violations.push((turn, v.clone()));
                blocks::print_block(
                    &Block::System {
                        text: format!("INVARIANT (turn {turn}): {v}"),
                        kind: SystemKind::Error,
                    },
                    tools_expanded,
                );
            }
        }

        print_autopilot_report(
            state,
            completed,
            total_tool_calls,
            &total_usage,
            &violations,
            client.model(),
            AutopilotDriverInfo {
                scripted,
                player_model: &player_model,
                agent_fallbacks,
            },
        );

        Ok(())
    }
}

/// Minimal (linear stdout) interactive loop — default; full TTS/STT support.
async fn run_minimal_interactive(
    state: &mut GameState,
    config: &Config,
    resume: bool,
) -> Result<()> {
    let client = make_dm_client(config);
    let mut history = ChatHistory::new(config.game.history_turns);
    let playback = ActivePlayback::new();
    let tts = TtsClient::new(config.xai.api_key.clone());
    let stt = stt_client(config);
    let mut flags = SessionFlags {
        tools_expanded: config.ui.tools_expanded || debug_enabled(),
        enable_tts: config.voice.enable_tts,
        enable_stt: config.voice.enable_stt,
        stt_edit_before_send: config.voice.stt_edit_before_send,
    };
    let tone = config.game.tone.as_str();
    let dm_voice = config.voice.dm_voice.as_str();
    let max_record_secs = config.stt.max_record_seconds.max(1);
    let sample_rate = config.stt.sample_rate;
    let device = config.stt.device;
    // Ring of previous player actions for `/again` (DM actions only; the
    // line editor keeps its own ↑/↓ recall of everything typed).
    let mut action_history: Vec<String> = Vec::new();
    let mut editor = crate::ui::line_editor::LineEditor::new();
    let interactive_prompt =
        crate::ui::creation::stdin_interactive() && crate::ui::creation::stdout_interactive();

    let mut detail = format!("model {} · {}", client.model(), client.reasoning_effort());
    if flags.enable_tts {
        detail.push_str(&format!("    voice {dm_voice}"));
    }
    if flags.enable_stt {
        detail.push_str(&format!("    stt {}", config.stt.model));
    }
    let hint = if flags.enable_stt {
        format!("Type an action.  /help for commands.  talk to speak (max {max_record_secs}s).")
    } else {
        "Type an action.  /help for commands.".to_string()
    };
    blocks::print_session_header("Grok Adventures", &detail, &hint);

    let opening = start_session(
        state,
        &client,
        resume,
        &flags.tools_expanded,
        tone,
        Some((&tts, flags.enable_tts, dm_voice, &playback)),
        &mut history,
    )
    .await?;
    let mut last_dm_text = opening.text.clone();

    let mut death_announced = false;
    loop {
        if !state.alive() {
            if !death_announced {
                blocks::print_rule();
                blocks::print_block(
                    &Block::System {
                        text: format!("Game Over — {} has fallen!", state.character_name),
                        kind: SystemKind::Error,
                    },
                    flags.tools_expanded,
                );
                blocks::print_block(
                    &Block::System {
                        text: "Type /resurrect to continue testing (1 HP), or /quit to end.".into(),
                        kind: SystemKind::Info,
                    },
                    flags.tools_expanded,
                );
                death_announced = true;
            }
        } else {
            death_announced = false;
        }

        status::print_status(state);

        // TTY: raw-mode editor (arrows/history/autocomplete). Pipes keep
        // cooked read_line so scripted sessions behave exactly as before.
        let line = if interactive_prompt {
            editor.read_line("")?
        } else {
            print!("{}", theme::player("› "));
            let _ = io::stdout().flush();
            let mut buf = String::new();
            let n = {
                let stdin = io::stdin();
                let mut stdin_lock = stdin.lock();
                stdin_lock
                    .read_line(&mut buf)
                    .context("reading player input")?
            };
            if n == 0 {
                println!();
                None
            } else {
                Some(buf)
            }
        };
        let Some(line) = line else {
            // EOF
            blocks::print_block(
                &Block::System {
                    text: format!("Farewell, {}!", state.character_name),
                    kind: SystemKind::Info,
                },
                flags.tools_expanded,
            );
            break;
        };

        let input = line.trim();
        if input.is_empty() {
            if flags.enable_stt {
                println!(
                    "{}",
                    theme::dim("Tip: type an action, or `talk` to speak (typing always works).")
                );
            }
            continue;
        }

        // Minimal-local: /copy last DM, /again last action (bare word or slash only).
        let (cmd, _) = parse_command(input);
        let is_slash = input.trim_start().starts_with('/');
        let bare_one_word = input.split_whitespace().count() == 1;
        if (cmd == "copy" || cmd == "again") && (is_slash || bare_one_word) {
            if cmd == "copy" {
                if last_dm_text.trim().is_empty() {
                    blocks::print_block(
                        &Block::System {
                            text: "Nothing to copy yet — wait for DM narration.".into(),
                            kind: SystemKind::Info,
                        },
                        flags.tools_expanded,
                    );
                } else {
                    match clipboard::copy_text(&last_dm_text) {
                        Ok(msg) => blocks::print_block(
                            &Block::System {
                                text: msg,
                                kind: SystemKind::Success,
                            },
                            flags.tools_expanded,
                        ),
                        Err(e) => blocks::print_block(
                            &Block::System {
                                text: format!("Copy failed: {e:#}"),
                                kind: SystemKind::Error,
                            },
                            flags.tools_expanded,
                        ),
                    }
                }
                continue;
            }
            // again
            let prev = action_history.last().cloned();
            if let Some(action) = prev {
                match play_player_action(
                    state,
                    &client,
                    &tts,
                    ActionTurn {
                        input: &action,
                        spoken: false,
                        tone,
                        dm_voice,
                        enable_tts: flags.enable_tts,
                        tools_expanded: flags.tools_expanded,
                        history: &mut history,
                        playback: &playback,
                    },
                )
                .await
                {
                    Ok(dm) => last_dm_text = dm,
                    Err(e) => blocks::print_block(
                        &Block::System {
                            text: format!("The mystical connection wavers... ({e:#})"),
                            kind: SystemKind::Error,
                        },
                        flags.tools_expanded,
                    ),
                }
                continue;
            }
            blocks::print_block(
                &Block::System {
                    text: "No previous action to repeat. Type something first.".into(),
                    kind: SystemKind::Info,
                },
                flags.tools_expanded,
            );
            continue;
        }

        match handle_meta_command(state, input, &mut flags)? {
            (MetaResult::Quit, blocks) => {
                print_blocks(&blocks, flags.tools_expanded);
                break;
            }
            (MetaResult::Handled, blocks) => {
                print_blocks(&blocks, flags.tools_expanded);
                continue;
            }
            (MetaResult::Talk { seconds }, blocks) => {
                print_blocks(&blocks, flags.tools_expanded);
                if !flags.enable_stt {
                    blocks::print_block(
                        &Block::System {
                            text: "STT is off. Enable with /voice.".into(),
                            kind: SystemKind::Info,
                        },
                        flags.tools_expanded,
                    );
                    continue;
                }
                let max_secs =
                    clamp_record_seconds(seconds.unwrap_or(max_record_secs), max_record_secs);
                match capture_spoken_action(
                    &stt,
                    sample_rate,
                    device,
                    max_secs,
                    flags.stt_edit_before_send,
                    flags.tools_expanded,
                )
                .await
                {
                    Ok(Some(action)) => {
                        match play_player_action(
                            state,
                            &client,
                            &tts,
                            ActionTurn {
                                input: &action,
                                spoken: true,
                                tone,
                                dm_voice,
                                enable_tts: flags.enable_tts,
                                tools_expanded: flags.tools_expanded,
                                history: &mut history,
                                playback: &playback,
                            },
                        )
                        .await
                        {
                            Ok(dm) => {
                                push_action_history(&mut action_history, &action);
                                last_dm_text = dm;
                            }
                            Err(e) => blocks::print_block(
                                &Block::System {
                                    text: format!("The mystical connection wavers... ({e:#})"),
                                    kind: SystemKind::Error,
                                },
                                flags.tools_expanded,
                            ),
                        }
                    }
                    Ok(None) => {
                        // cancelled / empty transcript
                    }
                    Err(e) => {
                        blocks::print_block(
                            &Block::System {
                                text: format!("Voice capture failed: {e:#}"),
                                kind: SystemKind::Error,
                            },
                            flags.tools_expanded,
                        );
                    }
                }
                continue;
            }
            (MetaResult::NotMeta, _) => {}
        }

        // While dead, only meta commands (handled above) are allowed.
        if !state.alive() {
            blocks::print_block(
                &Block::System {
                    text: "You have fallen. Type /resurrect to continue testing, or /quit to end."
                        .into(),
                    kind: SystemKind::Info,
                },
                flags.tools_expanded,
            );
            continue;
        }

        if handle_state_query(state, input, flags.tools_expanded) {
            continue;
        }

        match play_player_action(
            state,
            &client,
            &tts,
            ActionTurn {
                input,
                spoken: false,
                tone,
                dm_voice,
                enable_tts: flags.enable_tts,
                tools_expanded: flags.tools_expanded,
                history: &mut history,
                playback: &playback,
            },
        )
        .await
        {
            Ok(dm) => {
                push_action_history(&mut action_history, input);
                last_dm_text = dm;
            }
            Err(e) => blocks::print_block(
                &Block::System {
                    text: format!("The mystical connection wavers... ({e:#})"),
                    kind: SystemKind::Error,
                },
                flags.tools_expanded,
            ),
        }
    }

    // Autosave on clean exit
    if state.alive()
        && let Err(e) = SaveGame::save_slot(state, Some("autosave"))
    {
        blocks::print_block(
            &Block::System {
                text: format!("Autosave failed: {e:#}"),
                kind: SystemKind::Error,
            },
            flags.tools_expanded,
        );
    }

    Ok(())
}

/// Minimal-mode `/again` history shares [`crate::ui::prompt::push_history_entry`]
/// semantics (and cap) with fullscreen so the two UIs cannot drift.
fn push_action_history(history: &mut Vec<String>, action: &str) {
    crate::ui::prompt::push_history_entry(history, action);
}

async fn start_session(
    state: &mut GameState,
    client: &DmClient,
    resume: bool,
    tools_expanded: &bool,
    tone: &str,
    tts: Option<(&TtsClient, bool, &str, &ActivePlayback)>,
    history: &mut ChatHistory,
) -> Result<TurnResult> {
    if resume {
        let (blocks, result) = start_session_collect(state, client, true, tone, history).await?;
        print_blocks(&blocks, *tools_expanded);
        status::print_status(state);
        println!();
        if let Some((tts_client, enabled, voice, playback)) = tts
            && enabled
            && let Some(err) =
                maybe_speak_dm(tts_client, &result.text, voice, false, Some(playback)).await
        {
            blocks::print_block(&err, *tools_expanded);
        }
        return Ok(result);
    }

    print!("{}", theme::dim("… setting the scene"));
    let _ = io::stdout().flush();
    let (blocks, result) = start_session_collect(state, client, false, tone, history).await?;
    print!("\r{}\r", " ".repeat(40));
    let _ = io::stdout().flush();
    print_blocks(&blocks, *tools_expanded);
    status::print_status(state);
    println!();

    // Speak the opening text as soon as it is ready.
    if let Some((tts_client, true, voice, playback)) = tts {
        print!("{}", theme::dim("… speaking"));
        let _ = io::stdout().flush();
        if let Some(err) =
            maybe_speak_dm(tts_client, &result.text, voice, true, Some(playback)).await
        {
            print!("\r{}\r", " ".repeat(48));
            let _ = io::stdout().flush();
            blocks::print_block(&err, *tools_expanded);
        } else {
            print!("\r{}\r", " ".repeat(48));
            let _ = io::stdout().flush();
        }
    }
    Ok(result)
}

/// Collect opening/resume blocks without printing (for fullscreen TUI).
pub async fn start_session_collect(
    state: &mut GameState,
    client: &DmClient,
    resume: bool,
    tone: &str,
    history: &mut ChatHistory,
) -> Result<(Vec<Block>, TurnResult)> {
    if resume {
        let mut blocks = vec![Block::System {
            text: format!("Resuming adventure for {}…", state.character_name),
            kind: SystemKind::Info,
        }];
        let last = state
            .recent_log(1)
            .last()
            .cloned()
            .unwrap_or_else(|| "The adventure continues from where you left off.".into());
        blocks.push(Block::Dm { text: last.clone() });
        let result = TurnResult {
            text: last,
            tool_calls: vec![],
            mutating_tool_ran: false,
            usage: UsageTotals::default(),
        };
        return Ok((blocks, result));
    }

    let opening_action = format!(
        "Begin the adventure. Describe where {} wakes up and set the scene.",
        state.character_name
    );
    let before = StateSnapshot::from(&*state);
    let result = run_dm_turn(client, state, &opening_action, tone, history).await?;
    Ok((turn_blocks_with_deltas(&result, &before, state), result))
}

/// Print player action, run DM turn, present, then speak (TTS).
/// Returns DM narration text on success.
async fn play_player_action(
    state: &mut GameState,
    client: &DmClient,
    tts: &TtsClient,
    turn: ActionTurn<'_>,
) -> Result<String> {
    // New turn stops any leftover TTS from the previous beat.
    turn.playback.stop();

    blocks::print_rule();
    blocks::print_block(
        &Block::Player {
            text: turn.input.to_string(),
            spoken: turn.spoken,
        },
        turn.tools_expanded,
    );

    print!("{}", theme::dim("… thinking"));
    let _ = io::stdout().flush();

    let before = StateSnapshot::from(&*state);
    let result = run_dm_turn(client, state, turn.input, turn.tone, turn.history).await?;
    print!("\r{}\r", " ".repeat(40));
    let _ = io::stdout().flush();

    present_turn(state, &result, &before, turn.tools_expanded);

    // Speak the narration as soon as it exists.
    if turn.enable_tts {
        print!("{}", theme::dim("… speaking"));
        let _ = io::stdout().flush();
        if let Some(err) = maybe_speak_dm(
            tts,
            &result.text,
            turn.dm_voice,
            true, // progress already printed above
            Some(turn.playback),
        )
        .await
        {
            print!("\r{}\r", " ".repeat(48));
            let _ = io::stdout().flush();
            blocks::print_block(&err, turn.tools_expanded);
        } else {
            print!("\r{}\r", " ".repeat(48));
            let _ = io::stdout().flush();
        }
    }
    Ok(result.text)
}

/// Speak DM text. When `quiet`, suppress progress prints (for fullscreen TUI).
/// Returns an optional error System block.
///
/// When `playback` is provided, the afplay child is registered so it can be
/// killed via [`ActivePlayback::stop`] (Esc cancel / next turn).
pub async fn maybe_speak_dm(
    tts: &TtsClient,
    text: &str,
    voice_id: &str,
    quiet: bool,
    playback: Option<&ActivePlayback>,
) -> Option<Block> {
    if text.trim().is_empty() {
        return None;
    }
    if !quiet {
        print!("{}", theme::dim("… speaking"));
        let _ = io::stdout().flush();
    }
    match tts.speak_soft_with_playback(text, voice_id, playback).await {
        Ok(()) => {
            if !quiet {
                print!("\r{}\r", " ".repeat(40));
                let _ = io::stdout().flush();
            }
            None
        }
        Err(e) => {
            if !quiet {
                print!("\r{}\r", " ".repeat(40));
                let _ = io::stdout().flush();
            }
            Some(Block::System {
                text: format!("TTS unavailable: {e:#}"),
                kind: SystemKind::Info,
            })
        }
    }
}

/// Record mic until Enter (or max), showing the transcript as it arrives.
///
/// When `edit_before_send` is false, the final line is the action.
pub async fn capture_spoken_action(
    stt: &SttClient,
    sample_rate: u32,
    device: i32,
    max_secs: u32,
    edit_before_send: bool,
    tools_expanded: bool,
) -> Result<Option<String>> {
    let max_secs = max_secs.max(1);
    println!(
        "{}",
        theme::dim(&format!("Listening · Enter to stop · {max_secs}s"))
    );
    let handle = ListenHandle::start(stt.clone(), sample_rate, device, max_secs);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_for_keys = Arc::clone(&stop_flag);
    let keys = tokio::task::spawn_blocking(move || wait_enter_or_stop(&stop_for_keys));

    let mut shown = String::new();
    loop {
        let partial = handle.partial();
        if partial != shown {
            paint_live_line(&partial);
            shown = partial;
        } else if shown.is_empty() {
            paint_live_line("");
        }
        if stop_flag.load(Ordering::SeqCst) || handle.is_finished() {
            handle.stop();
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    let transcript = match handle.finish().await {
        Ok(text) => text,
        Err(err) => {
            clear_live_line();
            println!();
            stop_flag.store(true, Ordering::SeqCst);
            let _ = keys.await;
            return Err(err);
        }
    };
    stop_flag.store(true, Ordering::SeqCst);
    let _ = keys.await;
    clear_live_line();
    if transcript.is_empty() {
        println!();
        blocks::print_block(
            &Block::System {
                text: "Heard nothing — try again or type your action.".into(),
                kind: SystemKind::Info,
            },
            tools_expanded,
        );
        return Ok(None);
    }

    println!("{}", theme::player(&format!("› {transcript}")));

    if edit_before_send {
        let interactive =
            crate::ui::creation::stdin_interactive() && crate::ui::creation::stdout_interactive();
        if interactive {
            // Prefill the editor so arrows edit the transcript in place.
            println!(
                "{}",
                theme::dim("Edit the line, Enter to send, Ctrl+C to discard.")
            );
            let mut editor = crate::ui::line_editor::LineEditor::new();
            return match editor.read_line(&transcript)? {
                Some(line) if !line.trim().is_empty() => Ok(Some(line.trim().to_string())),
                _ => Ok(None),
            };
        }
        println!(
            "{}",
            theme::dim("Edit the line, or press Enter to send it.")
        );
        print!("{}", theme::player("› "));
        let _ = io::stdout().flush();
        let mut line = String::new();
        {
            let stdin = io::stdin();
            let mut lock = stdin.lock();
            lock.read_line(&mut line).context("reading STT edit")?;
        }
        let edited = line.trim();
        if edited.is_empty() {
            Ok(Some(transcript))
        } else {
            Ok(Some(edited.to_string()))
        }
    } else {
        Ok(Some(transcript))
    }
}

fn paint_live_line(text: &str) {
    let width = crossterm::terminal::size()
        .ok()
        .filter(|(cols, _)| *cols > 0)
        .map_or(80, |(cols, _)| cols as usize);
    let room = width.saturating_sub(2);
    let shown: String = if text.is_empty() {
        "…".to_string()
    } else {
        let count = text.chars().count();
        if count <= room {
            text.to_string()
        } else {
            text.chars().skip(count - room).collect()
        }
    };
    print!("\r{}{}\x1b[K", theme::player("› "), theme::dim(&shown));
    let _ = io::stdout().flush();
}

fn clear_live_line() {
    print!("\r\x1b[K");
    let _ = io::stdout().flush();
}

/// Record until `max_secs` elapses, streaming partials when the socket is up.
pub async fn capture_spoken_action_quiet(
    stt: &SttClient,
    sample_rate: u32,
    device: i32,
    max_secs: u32,
) -> Result<Option<String>> {
    let handle = ListenHandle::start(stt.clone(), sample_rate, device, max_secs.max(1));
    let transcript = handle.finish().await?;
    if transcript.is_empty() {
        Ok(None)
    } else {
        Ok(Some(transcript))
    }
}

/// Poll for Enter/Esc in raw mode until `stop` is set (also set by caller on max).
fn wait_enter_or_stop(stop: &AtomicBool) {
    let raw_ok = enable_raw_mode().is_ok();
    while !stop.load(Ordering::SeqCst) {
        match event::poll(Duration::from_millis(50)) {
            Ok(true) => {
                if let Ok(Event::Key(key)) = event::read()
                    && key.kind == KeyEventKind::Press
                    && matches!(key.code, KeyCode::Enter | KeyCode::Esc)
                {
                    stop.store(true, Ordering::SeqCst);
                    break;
                }
            }
            Ok(false) => {}
            Err(_) => break,
        }
    }
    if raw_ok {
        let _ = disable_raw_mode();
    }
}

/// Run one player action through the DM tool loop + marker fallback + log.
pub async fn run_dm_turn(
    client: &DmClient,
    state: &mut GameState,
    player_action: &str,
    tone: &str,
    history: &mut ChatHistory,
) -> Result<TurnResult> {
    run_dm_turn_cancellable(client, state, player_action, tone, None, history).await
}

/// Like [`run_dm_turn`], but abort the xAI loop when `cancel` is cancelled.
///
/// Callers that need clean cancel should snapshot `GameState` before calling
/// and restore it if [`crate::dm::is_turn_cancelled`] reports cancel — tools
/// already applied mid-turn would otherwise stick.
///
/// On success, appends the player action + final DM text to `history` (no-op
/// when history is disabled). Cancelled turns do not update history.
pub async fn run_dm_turn_cancellable(
    client: &DmClient,
    state: &mut GameState,
    player_action: &str,
    tone: &str,
    cancel: Option<CancellationToken>,
    history: &mut ChatHistory,
) -> Result<TurnResult> {
    let system = build_system_prompt_with_tone(state, tone);
    let mut result = client
        .query_cancellable(&system, player_action, state, cancel, history)
        .await?;

    // Marker safety-net only when no mutating tool ran.
    result.text = markers::process_if_fallback(state, &result.text, result.mutating_tool_ran);

    // Log the exchange (compact DM summary).
    state.log(format!("Player: {player_action}"));
    let summary = result.text.lines().next().unwrap_or(result.text.as_str());
    let summary = if summary.chars().count() > 80 {
        let t: String = summary.chars().take(80).collect();
        format!("{t}...")
    } else {
        summary.to_string()
    };
    state.log(format!("DM: {summary}"));

    // Multi-turn continuity: keep final narration (tool hops stay ephemeral).
    history.push(player_action, &result.text);

    Ok(result)
}

/// Presentation blocks for a completed DM turn (tools, dice, narration).
pub fn turn_blocks(result: &TurnResult) -> Vec<Block> {
    let mut out = Vec::new();
    if !result.tool_calls.is_empty() {
        let calls: Vec<ToolCallView> = result.tool_calls.iter().map(tool_call_view).collect();
        out.push(Block::Tools { calls });

        for tc in &result.tool_calls {
            if tc.name == "roll_dice" {
                out.push(Block::Dice {
                    text: tc.result.clone(),
                });
            }
        }
    }

    if !result.text.trim().is_empty() {
        out.push(Block::Dm {
            text: result.text.clone(),
        });
    }
    out
}

/// Tools + DM blocks, then State delta toasts for HP / room / inventory changes.
pub fn turn_blocks_with_deltas(
    result: &TurnResult,
    before: &StateSnapshot,
    after: &GameState,
) -> Vec<Block> {
    let mut out = turn_blocks(result);
    for line in delta::describe_deltas(before, after) {
        out.push(Block::State { text: line });
    }
    out
}

fn present_turn(
    state: &GameState,
    result: &TurnResult,
    before: &StateSnapshot,
    tools_expanded: bool,
) {
    if delta::took_damage(before, state) {
        audio::maybe_play_damage_sfx();
    }
    print_blocks(
        &turn_blocks_with_deltas(result, before, state),
        tools_expanded,
    );
    status::print_status(state);
    println!();
}

fn print_blocks(blocks: &[Block], tools_expanded: bool) {
    for b in blocks {
        blocks::print_block(b, tools_expanded);
    }
}

fn tool_call_view(tc: &ToolCallRecord) -> ToolCallView {
    ToolCallView {
        name: tc.name.clone(),
        result_summary: tc.result.chars().filter(|c| *c != '\n').collect(),
    }
}

/// Static DM instructions plus the configured tone.
pub fn static_dm_instructions(tone: &str) -> String {
    let tone_line = match tone {
        "dark_fantasy" => "Tone: dark fantasy — ominous, gothic, high stakes.",
        "comedic" => "Tone: comedic — witty, light, playful without breaking immersion.",
        "custom" => "Tone: follow the player's lead.",
        _ => "Tone: heroic fantasy — adventurous, vivid, fair.",
    };

    format!(
        r#"You are a Dungeon Master for a text-based fantasy adventure game. Your role is to:
- Narrate an exciting, atmospheric adventure
- Describe environments vividly but concisely
- Control NPCs and monsters with distinct personalities
- Adjudicate player actions fairly using dice rolls when outcomes are uncertain

{tone_line}

## CRITICAL RULE: ALWAYS USE TOOLS FOR GAME ACTIONS!

**IMPORTANT**: You have tools that control the game state. NARRATING actions WITHOUT calling tools does NOT change the game state! The game ONLY updates when you call tools.

### Available Tools (YOU MUST USE THESE):
- **roll_dice**: Roll dice for checks/combat - USE THIS for ALL uncertain outcomes!
- **add_to_inventory**: Add items - items DON'T EXIST until you call this!
- **remove_from_inventory**: Remove items when used/lost
- **modify_hp**: Change HP (negative=damage, positive=healing)
- **move_to_room**: MOVE player to new locations - USE THIS when traveling!
- **get_inventory**: Check what player is carrying
- **inspect_item**: Check details of a SPECIFIC item - USE THIS when player asks about an item!
- **get_character_stats**: Check player stats/HP
- **get_current_room**: Check current location
- **add_companion**: Register a named NPC traveling with the player - CALL THIS the first time a named companion appears!
- **update_companion**: Rename or re-describe an existing companion
- **remove_companion**: Remove a companion when it departs/dies
- **get_companions**: Check who is traveling with the player
- **remember**: Record a fact (names, quests, lore, passwords) so it stays consistent forever
- **get_journal**: Recall remembered facts
- **start_combat**: Begin initiative combat with an ordered list of participants (include the player)
- **advance_turn**: Move to the next participant in initiative order
- **end_combat**: Clear combat state when the fight ends

### MANDATORY: Call Tools BEFORE Narrating!

**FOR EVERY ACTION:**
1. **FIRST**: Call the appropriate tool(s)
2. **WAIT**: You will receive tool results
3. **THEN**: Narrate based on the actual tool results

**NEVER do this:**
- ❌ "You move north" (without calling move_to_room) - Player stays in old location!
- ❌ "You find a sword" (without calling add_to_inventory) - Player can't use it!
- ❌ "You take 5 damage" (without calling modify_hp) - HP doesn't change!
- ❌ "Rolling dice... you get 15" (without calling roll_dice) - Not a real roll!
- ❌ Inventing or changing a companion's name (use the EXACT name from the Companions list!)
- ❌ Contradicting a fact in the Remembered facts (journal) list

**ALWAYS do this:**
- ✅ Call move_to_room(room_name="North Corridor") → Narrate the new location
- ✅ Call add_to_inventory(item="Iron Sword", description="Sharp blade") → Narrate finding it
- ✅ Call modify_hp(amount=-5) → Narrate the injury
- ✅ Call roll_dice(sides=20, count=1) → Use the ACTUAL result in your narration
- ✅ Call inspect_item(item_name="Leather Pouch") BEFORE describing what's inside an item
- ✅ Call add_companion(name="Skritchnik", description="imp familiar") the FIRST time a named NPC joins the player
- ✅ Call remember(fact="Your patron is Zarathra, a demon") when an important name/fact is established
- ✅ Call start_combat(participants=["Hero","Goblin"]) when a fight begins, advance_turn each turn, end_combat when done

## NAME & FACT CONSISTENCY (CRITICAL)
- Once a companion is named, ALWAYS refer to it by that EXACT name. Never invent a new name for an existing companion.
- If a named entity (patron, villain, deity, quest-giver) is NOT in the lists yet and is important, call `remember` to lock in its name/identity on the turn it appears.
- Treat the journal as the single source of truth for lore you've already established.

## Response Format
- Keep responses to 2-3 paragraphs
- Call tools FIRST for any state-changing action
- Always end with a question/prompt for player action
- If HP reaches 0, narrate the dramatic defeat
- Check inventory with get_inventory before mentioning items
- NEVER contradict the Current Game State below
"#
    )
}

/// Build full system prompt: static + authoritative state + recent log.
pub fn build_system_prompt(state: &GameState) -> String {
    build_system_prompt_with_tone(state, "heroic")
}

/// Like [`build_system_prompt`] but with a tone from config.
pub fn build_system_prompt_with_tone(state: &GameState, tone: &str) -> String {
    let recent = state.recent_log(5);
    let log_section = if recent.is_empty() {
        "The adventure has just begun...".to_string()
    } else {
        recent.join("\n")
    };

    format!(
        "{}\n## Current Game State (AUTHORITATIVE - DO NOT CONTRADICT)\n{}\n## Recent Events\n{}\n",
        static_dm_instructions(tone),
        state.to_context_string(),
        log_section
    )
}

/// Autopilot driver mode stats for the end-of-run report.
struct AutopilotDriverInfo<'a> {
    scripted: bool,
    player_model: &'a str,
    agent_fallbacks: u32,
}

fn print_autopilot_report(
    state: &GameState,
    turns: u32,
    total_tool_calls: u32,
    usage: &UsageTotals,
    violations: &[(u32, String)],
    dm_model: &str,
    driver: AutopilotDriverInfo<'_>,
) {
    blocks::print_rule();
    println!("{}", theme::dm("═══ Autopilot Stress-Test Report ═══"));
    println!("Turns played: {turns}");
    println!("Total tool calls: {total_tool_calls}");
    let avg = if turns > 0 {
        total_tool_calls as f64 / turns as f64
    } else {
        0.0
    };
    println!("Avg tool calls/turn: {avg:.2}");
    println!("Final HP: {}/{}", state.hp, state.max_hp);
    println!("Final location: {}", state.current_room);
    println!("Items collected: {}", state.inventory.len());
    println!("Companions: {}", state.companions.len());
    println!("Rooms visited: {}", state.visited_rooms.len());
    println!("Journal entries: {}", state.journal.len());
    println!("Invariant violations: {}", violations.len());
    println!();
    println!("{}", theme::dm("═══ Player Driver ═══"));
    if driver.scripted {
        println!("Mode: scripted (fixed action rotation)");
        println!("Player model: (none)");
    } else {
        println!("Mode: AI player agent");
        println!("Player model: {}", driver.player_model);
        println!("Agent API failures / fallbacks: {}", driver.agent_fallbacks);
    }
    println!();
    println!("{}", theme::dm("═══ Token Usage ═══"));
    println!("DM model: {dm_model}");
    println!("Prompt tokens: {}", usage.prompt_tokens);
    println!("Completion tokens: {}", usage.completion_tokens);
    println!("Total tokens: {}", usage.total_tokens);
    if turns > 0 {
        let avg_tok = (usage.prompt_tokens + usage.completion_tokens) as f64 / turns as f64;
        println!("Avg tokens/turn: {avg_tok:.0}");
    }
    println!();
    if violations.is_empty() {
        blocks::print_block(
            &Block::System {
                text: "All state invariants held — no consistency bugs detected.".into(),
                kind: SystemKind::Success,
            },
            true,
        );
    } else {
        blocks::print_block(
            &Block::System {
                text: format!("{} invariant violation(s) found:", violations.len()),
                kind: SystemKind::Error,
            },
            true,
        );
        for (turn, v) in violations {
            println!("  - turn {turn}: {v}");
        }
    }
}

fn debug_enabled() -> bool {
    matches!(
        std::env::var("GROK_ADVENTURES_DEBUG").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parse_command_splits_args() {
        assert_eq!(
            parse_command("save hero1"),
            ("save".into(), Some("hero1".into()))
        );
        assert_eq!(parse_command("/quit"), ("quit".into(), None));
        assert_eq!(parse_command("  HELP  "), ("help".into(), None));
        assert_eq!(
            parse_command("/load ./run.json"),
            ("load".into(), Some("./run.json".into()))
        );
    }

    #[test]
    fn local_query_detection() {
        assert!(is_local_query("what's in my inventory?"));
        assert!(is_local_query("check inventory"));
        assert!(is_local_query("where am I"));
        assert!(is_local_query("how's my health"));
        assert!(is_local_query("check my hp"));
        assert!(!is_local_query("where is this goblin from?"));
        assert!(!is_local_query("attack the goblin"));
        assert!(!is_local_query("search the room"));
    }

    #[test]
    fn health_condition_thresholds() {
        assert_eq!(health_condition(20, 20), "in perfect health");
        assert_eq!(health_condition(18, 20), "slightly wounded");
        assert_eq!(health_condition(12, 20), "wounded");
        assert_eq!(health_condition(8, 20), "badly wounded");
        assert_eq!(health_condition(2, 20), "near death");
        assert_eq!(health_condition(0, 20), "dead");
    }

    fn test_flags() -> SessionFlags {
        SessionFlags {
            tools_expanded: false,
            enable_tts: true,
            enable_stt: true,
            stt_edit_before_send: true,
        }
    }

    #[test]
    fn meta_quit_and_not_meta() {
        let mut state = GameState::new("Hero");
        let mut flags = test_flags();
        let (r, blocks) = handle_meta_command(&mut state, "quit", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Quit);
        assert!(!blocks.is_empty());
        let (r, _) = handle_meta_command(&mut state, "search the room", &mut flags).unwrap();
        assert_eq!(r, MetaResult::NotMeta);
        let (r, blocks) = handle_meta_command(&mut state, "/help", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert!(!blocks.is_empty());
    }

    #[test]
    fn meta_tts_voice_talk_toggles() {
        let mut state = GameState::new("Hero");
        let mut flags = test_flags();
        assert!(flags.enable_tts);
        let (r, _) = handle_meta_command(&mut state, "/tts", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert!(!flags.enable_tts);

        assert!(flags.enable_stt);
        let (r, _) = handle_meta_command(&mut state, "/voice", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert!(!flags.enable_stt);

        let (r, _) = handle_meta_command(&mut state, "talk", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Talk { seconds: None });
        let (r, _) = handle_meta_command(&mut state, "/talk 8", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Talk { seconds: Some(8) });
    }

    #[test]
    fn scene_command_is_gone() {
        // `/scene` was removed with the image pipeline: unknown slash command.
        let mut state = GameState::new("Hero");
        let mut flags = test_flags();
        let (r, _) = handle_meta_command(&mut state, "/scene", &mut flags).unwrap();
        assert_eq!(r, MetaResult::NotMeta);
        // Bare "scene" is natural language, not meta.
        let (r, _) = handle_meta_command(&mut state, "scene", &mut flags).unwrap();
        assert_eq!(r, MetaResult::NotMeta);
    }

    #[test]
    fn meta_rest_longrest_and_resurrect() {
        let mut state = GameState::new("Hero");
        let mut flags = test_flags();
        state.hp = 4;
        let (r, blocks) = handle_meta_command(&mut state, "/rest", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert_eq!(state.hp, 4 + state.short_rest_heal_amount().min(20 - 4));
        // recompute: max_hp 20, heal 5, from 4 → 9
        assert_eq!(state.hp, 9);
        assert!(matches!(blocks.first(), Some(Block::State { .. })));

        state.hp = 2;
        let (r, _) = handle_meta_command(&mut state, "long rest", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert_eq!(state.hp, state.max_hp);

        state.hp = 5;
        let (r, _) = handle_meta_command(&mut state, "/longrest", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert_eq!(state.hp, state.max_hp);

        state.hp = 0;
        let (r, blocks) = handle_meta_command(&mut state, "/resurrect", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert_eq!(state.hp, 1);
        assert!(matches!(blocks.first(), Some(Block::State { .. })));

        // Natural language "rest by the fire" should NOT be meta.
        let (r, _) = handle_meta_command(&mut state, "rest by the fire", &mut flags).unwrap();
        assert_eq!(r, MetaResult::NotMeta);
    }

    #[test]
    fn meta_save_load_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("t.json");
        let path_str = path.to_str().unwrap().to_string();

        let mut state = GameState::new("Hero");
        state.hp = 11;
        let mut flags = test_flags();
        let (r, _) =
            handle_meta_command(&mut state, &format!("save {path_str}"), &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);

        let mut state2 = GameState::new("Other");
        let (r, _) =
            handle_meta_command(&mut state2, &format!("load {path_str}"), &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert_eq!(state2.character_name, "Hero");
        assert_eq!(state2.hp, 11);
    }

    #[test]
    fn system_prompt_includes_state() {
        let mut state = GameState::new("Thorin");
        state.current_room = "Hall".into();
        let prompt = build_system_prompt(&state);
        assert!(prompt.contains("ALWAYS USE TOOLS"));
        assert!(prompt.contains("Thorin"));
        assert!(prompt.contains("Hall"));
        assert!(prompt.contains("AUTHORITATIVE"));
    }

    #[test]
    fn state_query_handler_returns_true() {
        let state = GameState::new("Hero");
        assert!(handle_state_query(&state, "where am i", false));
        assert!(!handle_state_query(&state, "open the chest", false));
    }

    #[test]
    fn turn_blocks_include_dm_and_tools() {
        let result = TurnResult {
            text: "You see a door.".into(),
            tool_calls: vec![ToolCallRecord {
                name: "roll_dice".into(),
                arguments: "{}".into(),
                result: "Rolled 15".into(),
            }],
            mutating_tool_ran: false,
            usage: UsageTotals::default(),
        };
        let blocks = turn_blocks(&result);
        assert!(blocks.iter().any(|b| matches!(b, Block::Tools { .. })));
        assert!(blocks.iter().any(|b| matches!(b, Block::Dice { .. })));
        assert!(blocks.iter().any(|b| matches!(b, Block::Dm { .. })));
    }

    #[tokio::test]
    async fn speak_dm_skips_empty_text() {
        let tts = TtsClient::new("test-key");
        let err = maybe_speak_dm(&tts, "   ", "zagan", true, None).await;
        assert!(
            err.is_none(),
            "empty narration speaks nothing, errors nothing"
        );
    }

    #[tokio::test]
    async fn speak_dm_failure_becomes_info_block() {
        // Unreachable TTS host: synthesize fails fast, no player spawned.
        let tts = TtsClient::new("test-key").with_base_url("http://127.0.0.1:1");
        let err = maybe_speak_dm(&tts, "Hello there.", "zagan", true, None).await;
        match err {
            Some(Block::System { text, .. }) => assert!(text.contains("TTS unavailable")),
            other => panic!("expected TTS info block, got {other:?}"),
        }
    }

    #[test]
    fn turn_blocks_with_deltas_appends_state_toasts() {
        let mut state = GameState::new("Hero");
        let before = StateSnapshot::from(&state);
        state.modify_hp(-2);
        state.add_item("Iron Dagger", "blade", 1);
        let result = TurnResult {
            text: "Ouch.".into(),
            tool_calls: vec![],
            mutating_tool_ran: true,
            usage: UsageTotals::default(),
        };
        let blocks = turn_blocks_with_deltas(&result, &before, &state);
        assert!(blocks.iter().any(|b| matches!(b, Block::Dm { .. })));
        let state_lines: Vec<_> = blocks
            .iter()
            .filter_map(|b| match b {
                Block::State { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            state_lines
                .iter()
                .any(|t| t.contains("💔") && t.contains("HP"))
        );
        assert!(state_lines.iter().any(|t| t.contains("Iron Dagger")));
    }
}
