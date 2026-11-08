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
use crate::dm::{
    ChatHistory, DmClient, ToolCallRecord, TurnResult, UsageTotals, is_turn_cancelled,
};
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
use crate::ui::minimal_cancel::EscWatcher;
use crate::ui::sanitize;
use crate::ui::status;
use crate::ui::theme;
use crate::util::clipboard;

// Re-exported so the TUI and tests can keep using `crate::game::loop_::…`.
pub use crate::game::commands::{
    MetaResult, SessionFlags, SessionUsage, ensure_alive, format_session_usage,
    handle_meta_command, handle_state_query, handle_state_query_blocks, health_condition,
    is_local_query, parse_command, session_usage_block,
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

/// Result of one minimal player action after the DM wait, including the
/// cancellation paths that `TurnResult` alone cannot represent.
enum ActionOutcome {
    /// The DM turn committed; bill it and record the narration.
    Completed(TurnResult),
    /// Esc cancelled the turn. State was restored to the pre-turn snapshot;
    /// nothing may be billed or recorded.
    Cancelled,
    /// Ctrl+C cancelled the turn and asked the session to exit so the
    /// existing autosave-on-clean-exit path runs.
    Quit,
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
        Self::run_autopilot_with_style(
            state,
            config,
            turns,
            scripted,
            false,
            PlayerStyle::from_env(),
        )
        .await
    }

    /// Autopilot with an explicit player personality (CLI / env).
    pub async fn run_autopilot_with_style(
        state: &mut GameState,
        config: &Config,
        turns: u32,
        scripted: bool,
        resume: bool,
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

        let session =
            start_session(state, &client, resume, &true, tone, None, &mut history).await?;
        status::print_status(state);
        println!();
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
                theme::dim(&autopilot_turn_line(turn, turns, source, &action))
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
            status::print_status(state);
            println!();
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
    let mut usage = SessionUsage::default();
    usage.record_turn(&opening.usage);
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
            playback.stop();
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
            if let Some(block) = ensure_alive(state) {
                blocks::print_block(&block, flags.tools_expanded);
                continue;
            }
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
                    Ok(outcome) => {
                        if apply_action_outcome(
                            outcome,
                            &mut usage,
                            &mut last_dm_text,
                            &mut action_history,
                            &action,
                        ) {
                            break;
                        }
                    }
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
                playback.stop();
                break;
            }
            (MetaResult::Handled, blocks) => {
                print_blocks(&blocks, flags.tools_expanded);
                continue;
            }
            (MetaResult::Loaded, blocks) => {
                playback.stop();
                print_blocks(&blocks, flags.tools_expanded);
                // `/load` starts a new campaign: drop old DM/UI context so the
                // next turn cannot see the previous campaign's conversation.
                history = ChatHistory::new(config.game.history_turns);
                seed_history(&mut history, state);
                action_history.clear();
                editor = crate::ui::line_editor::LineEditor::new();
                last_dm_text = state.last_narration.clone().unwrap_or_default();
                continue;
            }
            (MetaResult::Usage, _) => {
                // Local only: never a DM call. Session totals survive /load.
                blocks::print_block(&session_usage_block(&usage), flags.tools_expanded);
                continue;
            }
            (MetaResult::Talk { seconds }, blocks) => {
                print_blocks(&blocks, flags.tools_expanded);
                if let Some(block) = ensure_alive(state) {
                    blocks::print_block(&block, flags.tools_expanded);
                    continue;
                }
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
                            Ok(outcome) => {
                                if apply_action_outcome(
                                    outcome,
                                    &mut usage,
                                    &mut last_dm_text,
                                    &mut action_history,
                                    &action,
                                ) {
                                    break;
                                }
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
            Ok(outcome) => {
                if apply_action_outcome(
                    outcome,
                    &mut usage,
                    &mut last_dm_text,
                    &mut action_history,
                    input,
                ) {
                    break;
                }
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
    playback.stop();
    if state.alive()
        && state.has_progress()
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

/// Session bookkeeping for one minimal action outcome.
///
/// `Completed` bills usage, records the narration, and records the action for
/// `/again`; `Cancelled` does nothing (never billed or recorded); `Quit` does
/// nothing and asks the input loop to exit. Returns `true` when the loop
/// should break.
fn apply_action_outcome(
    outcome: ActionOutcome,
    usage: &mut SessionUsage,
    last_dm_text: &mut String,
    action_history: &mut Vec<String>,
    recorded_action: &str,
) -> bool {
    match outcome {
        ActionOutcome::Completed(result) => {
            usage.record_turn(&result.usage);
            push_action_history(action_history, recorded_action);
            *last_dm_text = result.text;
            false
        }
        ActionOutcome::Cancelled => false,
        ActionOutcome::Quit => true,
    }
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
        println!();
        // Resume is a local recap: never speak it.
        return Ok(result);
    }

    print!("{}", theme::dim("… setting the scene"));
    let _ = io::stdout().flush();
    let (blocks, result) = start_session_collect(state, client, false, tone, history).await?;
    print!("\r{}\r", " ".repeat(40));
    let _ = io::stdout().flush();
    print_blocks(&blocks, *tools_expanded);
    println!();

    // Speak the opening in the background so the prompt is immediately free.
    if let Some((tts_client, true, voice, playback)) = tts {
        spawn_speak_dm(tts_client, &result.text, voice, playback);
    }
    Ok(result)
}

/// Speak DM narration off the critical path (minimal UI).
///
/// The next turn's [`ActivePlayback::stop`] cuts a lingering clip; errors are
/// logged through tracing instead of blocking or printing.
pub(crate) fn spawn_speak_dm(
    tts: &TtsClient,
    text: &str,
    voice_id: &str,
    playback: &ActivePlayback,
) {
    if text.trim().is_empty() {
        return;
    }
    let tts = tts.clone();
    let text = text.to_string();
    let voice_id = voice_id.to_string();
    let playback = playback.clone();
    tokio::spawn(async move {
        if let Some(Block::System { text, .. }) =
            maybe_speak_dm(&tts, &text, &voice_id, true, Some(&playback)).await
        {
            tracing::warn!(target: "grok_adventures::tts", "{text}");
        }
    });
}

/// Seed a chat history from the state's persisted exchanges, oldest first.
///
/// Purely local — never calls the DM. [`ChatHistory::push`] applies the
/// configured `history_turns` and character caps, so a save may hold more
/// context than the current session asks for.
pub(crate) fn seed_history(history: &mut ChatHistory, state: &GameState) {
    for exchange in &state.recent_exchanges {
        history.push(&exchange.player, &exchange.dm);
    }
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
        seed_history(history, state);
        let narration = state
            .resume_narration()
            .unwrap_or_else(|| "The adventure continues from where you left off.".to_string());
        let blocks = vec![
            Block::System {
                text: format!("Resuming adventure for {}…", state.character_name),
                kind: SystemKind::Info,
            },
            Block::System {
                text: "Previously…".into(),
                kind: SystemKind::Info,
            },
            Block::Dm {
                text: narration.clone(),
            },
        ];
        let result = TurnResult {
            text: narration,
            tool_calls: vec![],
            mutating_tool_ran: false,
            usage: UsageTotals::default(),
        };
        return Ok((blocks, result));
    }

    let before = StateSnapshot::from(&*state);
    let result = run_dm_turn(client, state, &opening_action(state), tone, history).await?;
    Ok((turn_blocks_with_deltas(&result, &before, state), result))
}

/// Opening instruction for a fresh session: an inciting incident, a grounded
/// room, a goal and threat, and a first meaningful choice.
fn opening_action(state: &GameState) -> String {
    let class = state.class.as_deref().unwrap_or("none - no training/kit");
    format!(
        "Begin the adventure for {name} ({class}). Open on a specific inciting incident with immediate pressure — not a generic wake-up; danger, a summons, a theft, or a pursuit all work. \
         Call move_to_room(room_name=...) first to set the opening location, then describe it so state and prose agree. \
         Establish a clear objective and an escalating threat or clock, and register both with set_objective(objective=..., threat=...). Introduce 2-3 named NPCs with wants and secrets. \
         Do not narrate {name}'s words, thoughts, feelings, or decisions. End at a decision point with 2-3 concrete options that carry different risks.",
        name = state.character_name,
        class = class
    )
}

/// Print player action, run DM turn, present, then speak (TTS).
///
/// On a terminal, an [`EscWatcher`] owns the keyboard for the duration of the
/// DM wait: `Esc` cancels (state is restored to the pre-turn snapshot) and
/// `Ctrl+C` cancels and asks the session to quit. Pipes and non-terminals run
/// the original non-cancellable path.
async fn play_player_action(
    state: &mut GameState,
    client: &DmClient,
    tts: &TtsClient,
    turn: ActionTurn<'_>,
) -> Result<ActionOutcome> {
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

    let before = StateSnapshot::from(&*state);
    // Defense in depth: the DM client commits transactionally, but restoring
    // this snapshot guarantees a cancelled turn leaves no partial mutation.
    let pre_turn = state.clone();

    let mut watcher = EscWatcher::start(turn.playback);
    let thinking = if watcher.is_some() {
        "… thinking — Esc cancels"
    } else {
        "… thinking"
    };
    print!("{}", theme::dim(thinking));
    let _ = io::stdout().flush();

    let result = match watcher.as_ref() {
        Some(watcher) => {
            run_dm_turn_cancellable(
                client,
                state,
                turn.input,
                turn.tone,
                Some(watcher.cancel_token()),
                turn.history,
            )
            .await
        }
        None => run_dm_turn(client, state, turn.input, turn.tone, turn.history).await,
    };

    // Stop and join the poll thread (restoring cooked mode) on every path
    // before touching stdout with multi-line output. Read `quit` after the
    // join so a Ctrl+C processed just before shutdown is never lost.
    if let Some(watcher) = watcher.as_mut() {
        watcher.stop();
    }
    let quit = watcher.as_ref().is_some_and(EscWatcher::quit_requested);
    print!("\r{}\r", " ".repeat(40));
    let _ = io::stdout().flush();

    match result {
        Ok(result) => {
            // Ctrl+C can race the final token. The turn already committed, so
            // present it, but honor the quit request (autosave runs after the
            // loop breaks). Do not start new speech on the way out.
            present_turn(state, &result, &before, turn.tools_expanded);
            if quit {
                turn.playback.stop();
                return Ok(ActionOutcome::Quit);
            }
            // Speak the narration in the background so the next prompt is immediate.
            if turn.enable_tts {
                spawn_speak_dm(tts, &result.text, turn.dm_voice, turn.playback);
            }
            Ok(ActionOutcome::Completed(result))
        }
        Err(e) if is_turn_cancelled(&e) => {
            state.replace_with(pre_turn);
            turn.playback.stop();
            if quit {
                Ok(ActionOutcome::Quit)
            } else {
                blocks::print_block(
                    &Block::System {
                        text: "Turn cancelled.".into(),
                        kind: SystemKind::Info,
                    },
                    turn.tools_expanded,
                );
                Ok(ActionOutcome::Cancelled)
            }
        }
        Err(e) => {
            // API errors propagate as before; always stop any residual speech.
            turn.playback.stop();
            Err(e)
        }
    }
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

    println!(
        "{}",
        theme::player(&format!(
            "› {}",
            sanitize::sanitize_terminal_line(&transcript)
        ))
    );

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
    print!(
        "\r{}{}\x1b[K",
        theme::player("› "),
        theme::dim(&live_line_text(text, width))
    );
    let _ = io::stdout().flush();
}

/// Sanitized, width-limited payload for the live STT line (without chrome).
///
/// Untrusted partial transcripts are filtered *before* truncation so escape
/// payloads neither reach the terminal nor eat into the visible width.
fn live_line_text(text: &str, width: usize) -> String {
    let room = width.saturating_sub(2);
    let clean = sanitize::sanitize_terminal_line(text);
    if clean.is_empty() {
        "…".to_string()
    } else {
        let count = clean.chars().count();
        if count <= room {
            clean
        } else {
            clean.chars().skip(count - room).collect()
        }
    }
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
/// The underlying [`DmClient::query_cancellable`] is transactional: tool
/// mutations are applied to a working copy and committed only after final
/// narration arrives, so cancelled/failed turns leave `state` untouched.
///
/// On success, appends the player action + final DM text to `history` (no-op
/// when history is disabled) and records the exchange in `state` for saves and
/// resume. Cancelled turns do not update history or state.
pub async fn run_dm_turn_cancellable(
    client: &DmClient,
    state: &mut GameState,
    player_action: &str,
    tone: &str,
    cancel: Option<CancellationToken>,
    history: &mut ChatHistory,
) -> Result<TurnResult> {
    // Defense in depth: the UI gates actions, but no DM turn may start while
    // the character is dead.
    if !state.alive() {
        anyhow::bail!(
            "{} has fallen — /resurrect or /load to continue",
            state.character_name
        );
    }
    let system = build_system_prompt_with_tone(state, tone);
    let mut result = client
        .query_cancellable(&system, player_action, state, cancel, history)
        .await?;

    // Marker safety-net only when no mutating tool ran.
    result.text = markers::process_if_fallback(state, &result.text, result.mutating_tool_ran);
    state.set_last_narration(&result.text);
    state.record_exchange(player_action, &result.text);

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

/// Presentation blocks for a completed DM turn.
///
/// Composition puts the story first: narration, then dice, then tools. State
/// deltas (when present) are inserted before the tools by
/// [`turn_blocks_with_deltas`].
pub fn turn_blocks(result: &TurnResult) -> Vec<Block> {
    let mut out = Vec::new();
    if !result.text.trim().is_empty() {
        out.push(Block::Dm {
            text: result.text.clone(),
        });
    }

    if !result.tool_calls.is_empty() {
        for tc in &result.tool_calls {
            if tc.name == "roll_dice" {
                out.push(Block::Dice {
                    text: tc.result.clone(),
                });
            }
        }

        let calls: Vec<ToolCallView> = result.tool_calls.iter().map(tool_call_view).collect();
        out.push(Block::Tools { calls });
    }
    out
}

/// DM narration → dice → one aggregated State block → tools.
///
/// Deltas are newline-joined into a single block so a turn with several
/// changes does not repeat a "State" header per line and push the narration
/// off an 80×24 screen.
pub fn turn_blocks_with_deltas(
    result: &TurnResult,
    before: &StateSnapshot,
    after: &GameState,
) -> Vec<Block> {
    let mut out = turn_blocks(result);
    let deltas = delta::describe_deltas(before, after);
    if !deltas.is_empty() {
        let state = Block::State {
            text: deltas.join("\n"),
        };
        // Tools stay last so mechanics never precede the narration.
        let tools_at = out
            .iter()
            .position(|b| matches!(b, Block::Tools { .. }))
            .unwrap_or(out.len());
        out.insert(tools_at, state);
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
    format!(
        r#"You are the Dungeon Master for a concrete, ongoing fantasy adventure — not a generic narrator. Run one campaign with a place, a problem, people, and a clock. The world reacts; it does not wait for the player.

{tone_block}

## Campaign Brief
- Premise: open on a specific inciting incident with immediate pressure. "Waking up" is only one option; a summons, a theft, a pursuit, or a dying messenger work just as well. End the opening beat on a meaningful choice.
- Ground the scene: call move_to_room(room_name=...) to establish the opening location before describing it. State and prose must agree.
- Goal and threat: register a clear objective and an escalating threat or clock with set_objective(objective=..., threat=...) in the first scene, and call it again whenever either materially changes. Choices leave consequences: NPC attitudes shift, rooms change, resources are spent, and the past is called back. Never reset the world between turns.
- Cast: introduce 2-3 named NPCs early. Give each a want, a fear or secret, and a distinct voice. Register anyone who travels with the player via add_companion on first appearance; lock important names and facts with remember. get_companions and get_journal are canon; never rename or contradict them.
- Structure: frame scenes, cut on meaningful transitions, and let consequences drive the next scene. Build toward a climax and a real epilogue or ending. Do not stall in an endless middle; when the clock runs out, the world changes or the adventure ends.
- Player agency (hard rule): never narrate the player's words, thoughts, feelings, or decisions. Narrate the world and its NPCs only. Stop at a decision point and offer 2-3 concrete options with different risks (fight, sneak, negotiate, sacrifice). Options are invitations, not a limit; the player may do anything.
- Out of character: answer rules, meta, and out-of-character questions briefly and out of character. If the player refuses content or asks for a break, respect it immediately: offer a content break or fade-to-black, then continue only with consent.
- Prose: 2-3 tight paragraphs. Vivid sensory detail and concrete nouns, not purple prose. No repetition, no recaps beyond a line, no meta commentary. Put NPC dialogue in quotes with speaker names.

## Tools First — ALWAYS USE TOOLS FOR GAME ACTIONS
State changes happen only through tools. For every action: call the tools first, wait for results, then narrate the actual outcome. Narration alone never moves the player, grants an item, changes HP, or decides a roll.
- **roll_dice** — all uncertain checks, saves, attacks, and damage. Pass ability="DEX" style (STR, DEX, CON, INT, WIS, or CHA) whenever a score applies; add modifier= for proficiency, magic, or cover. Never invent a result.
- **modify_hp** — establish damage through an attack, save, or roll; establish healing through a potion, rest, or spell (negative = damage, positive = healing). No arbitrary damage. A short rest heals but costs time and risk; a long rest needs safety and can be interrupted.
- **move_to_room** — every relocation. Name the destination; narrate the new place only after the move lands.
- **add_to_inventory / remove_from_inventory / get_inventory / inspect_item** — items exist only after add_to_inventory. Remove items when used or lost; inspect_item before describing an item's contents; get_inventory before naming what the player carries.
- **get_character_stats / get_current_room** — read HP, stats, and location before asserting them.
- **add_companion / update_companion / remove_companion / get_companions** — register named travelers on first appearance, update only to correct a description, and remove them when they depart or die; use the exact name from the companion list forever.
- **remember / get_journal** — record names, quests, lore, passwords, and promises the moment they matter. The journal is the single source of truth.
- **set_objective** — the authoritative quest goal and escalating threat/clock. Call it at the opening and whenever the goal or threat materially changes. Call set_objective(complete=true) only when the objective is truly resolved, then narrate the epilogue. The Objective and Threat lines in state are canon.
- **start_combat / advance_turn / end_combat** — ordered participants including the player; one advance_turn per combat turn; end_combat when the fight is over.

### Tool contract
- FIRST call the tool(s); WAIT for results; THEN narrate those exact results.
- ❌ "You move north" without move_to_room. ❌ "You find a sword" without add_to_inventory. ❌ "You take 5 damage" without modify_hp. ❌ "You roll 15" without roll_dice. ❌ Inventing a companion's name or contradicting a remembered fact.
- ✅ move_to_room → describe the new location. ✅ add_to_inventory(item="Iron Sword", description="Sharp blade") → narrate finding it. ✅ modify_hp(amount=-5) → narrate the wound. ✅ roll_dice(sides=20, ability="DEX") → use the real total.
- Fallback markers ([ROLL:...], [DAMAGE:...], [MOVE:...], etc.) are a safety net for narration that missed a tool. If a mutating tool already ran, do not also emit a marker for that change. Never double-apply; tools win over markers.

## Rules & Checks
- Ruleset: dnd5e-lite. Supported: d20 + ability modifier checks, standard dice (d4–d100), HP and death at 0, inventory, initiative order, and short/long rest homebrew. Not supported: AC, proficiency, levels, spell slots, advantage/disadvantage state, death saves, or enemy stat blocks. Do not imply they exist; resolve those situations with the dice and tools that do.
- Class features are narrative permissions, not extra rules: each is usable roughly once per scene (Fighter's Second Wind: once per short rest). The Class feature line in state is canon. Apply healing or damage through modify_hp and checks or attacks through roll_dice; never invent extra uses, spell slots, or free damage.
- Roll only when the outcome is uncertain and failure matters. A competent hero simply succeeds at routine tasks; impossible actions fail with a visible path to change that.
- DC bands: easy 10, medium 15, hard 20, very hard 25. Roll 1d20 + the relevant ability. Use roll_dice with ability="DEX" (or STR/CON/INT/WIS/CHA) whenever a stat applies; never roll a stat-less 1d20 for a stat-driven action.
- Damage follows fiction: only after an attack, save, or roll establishes it, and always through modify_hp. Enemies can be hurt too, but track that in narration or combat, not by changing the player's HP.
- Failure should bite: raise the threat clock, cost time or resources, damage gear, close a door, or shift an NPC's trust. Do not soften every failure into a near-success.
- End when the ending is earned: let the climax resolve the objective and the threat, call set_objective(complete=true), then narrate the epilogue. Do not loop forever.

## Response Format
- 2-3 paragraphs of prose, tools first.
- End at a decision point with 2-3 concrete options with different risks; a plain question is fine only when options would feel false.
- If HP reaches 0, narrate the dramatic defeat and stop.
- Never contradict the Current Game State below.
"#,
        tone_block = tone_block(tone)
    )
}

/// Tone-specific opening, cast, stakes, and prose guidance.
fn tone_block(tone: &str) -> &'static str {
    match tone {
        "dark_fantasy" => {
            r#"### Tone: Dark Fantasy
- Opening: a grim omen, an old sin, or a spreading rot surfaces in a wet, ruined place; safety is already gone.
- NPCs: desperate, secretive, morally compromised. Kindness costs something; no one is purely good.
- Stakes: corruption, memory, and survival. Victories leave scars, and death is real.
- Prose: cold, sensory, restrained. Dread lives between the lines; use gore sparingly for shock."#
        }
        "comedic" => {
            r#"### Tone: Comedic Fantasy
- Opening: a petty disaster or absurd complication with real stakes underneath.
- NPCs: eccentric, earnest, and self-important. Comic timing, not cruelty; let them be people, not punchlines.
- Stakes: keep them real so the jokes land. Consequences can be ridiculous, but they still stick.
- Prose: dry wit, brisk dialogue, surprising comparisons. Never wink at the player or break immersion."#
        }
        "custom" => {
            r#"### Tone: Custom
- Follow the player's lead on genre, register, and content. If they have not stated a preference, default to heroic fantasy with clear stakes.
- Match their vocabulary and pacing; never override the tone they ask for.
- Structure, rules, and player-agency limits still apply unchanged."#
        }
        _ => {
            r#"### Tone: Heroic Fantasy
- Opening: a call to protect someone or something worth saving, with danger already at the gate.
- NPCs: brave and flawed, loyal and stubborn. Villains have a reason and a line they will not cross.
- Stakes: hope, freedom, and sacrifice. Heroism is a choice with a price, never invincibility.
- Prose: bright, kinetic, clear. Wonder and momentum, with room for quiet courage."#
        }
    }
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

/// Single-line progress line printed before each autopilot turn. The action
/// can come from the player model, so strip terminal controls and flatten
/// line breaks before styling.
fn autopilot_turn_line(turn: u32, turns: u32, source: &str, action: &str) -> String {
    format!(
        "[Autopilot turn {turn}/{turns} · {source}] {}",
        sanitize::sanitize_terminal_line(action)
    )
}

/// Single-line report field (room/model/violation names are untrusted).
fn report_field(value: &str) -> String {
    sanitize::sanitize_terminal_line(value)
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
    println!("Final location: {}", report_field(&state.current_room));
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
        println!("Player model: {}", report_field(driver.player_model));
        println!("Agent API failures / fallbacks: {}", driver.agent_fallbacks);
    }
    println!();
    println!("{}", theme::dm("═══ Token Usage ═══"));
    println!("DM model: {}", report_field(dm_model));
    println!("API requests: {}", usage.requests);
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
            println!("  - turn {turn}: {}", report_field(v));
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
    use crate::game::state::Exchange;
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
    fn action_outcome_records_only_completed_turns() {
        let mut usage = SessionUsage::default();
        let mut last_dm = String::new();
        let mut actions = Vec::new();

        let completed = ActionOutcome::Completed(TurnResult {
            text: "The door creaks open.".into(),
            tool_calls: vec![],
            mutating_tool_ran: false,
            usage: UsageTotals {
                prompt_tokens: 10,
                completion_tokens: 4,
                total_tokens: 14,
                requests: 1,
            },
        });
        let quit = apply_action_outcome(
            completed,
            &mut usage,
            &mut last_dm,
            &mut actions,
            "open the door",
        );
        assert!(!quit);
        assert_eq!(usage.turns, 1);
        assert_eq!(usage.requests, 1);
        assert_eq!(usage.total_tokens, 14);
        assert_eq!(last_dm, "The door creaks open.");
        assert_eq!(actions, vec!["open the door".to_string()]);

        // Cancelled: no billing, no history, keep looping.
        assert!(!apply_action_outcome(
            ActionOutcome::Cancelled,
            &mut usage,
            &mut last_dm,
            &mut actions,
            "ignored",
        ));
        assert_eq!(usage.turns, 1);
        assert_eq!(usage.total_tokens, 14);
        assert_eq!(last_dm, "The door creaks open.");
        assert_eq!(actions, vec!["open the door".to_string()]);

        // Quit: still no billing or recording, but the loop must exit.
        assert!(apply_action_outcome(
            ActionOutcome::Quit,
            &mut usage,
            &mut last_dm,
            &mut actions,
            "ignored",
        ));
        assert_eq!(usage.turns, 1);
        assert_eq!(last_dm, "The door creaks open.");
        assert_eq!(actions, vec!["open the door".to_string()]);
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

    #[test]
    fn live_line_sanitizes_before_truncating() {
        let hostile = "hello\x1b]52;c;cGF5bG9hZA==\x07world";
        let line = live_line_text(hostile, 80);
        assert_eq!(line, "helloworld");
        assert!(!line.contains('\u{1b}'));
        assert!(!line.contains('\u{7}'));
        assert!(!line.contains("52;"));

        // Width accounting uses the sanitized text, so a hidden OSC payload
        // does not push visible characters past the truncation window.
        assert_eq!(live_line_text("\x1b[2J\x1b]0;pwned\x07abcdef", 8), "abcdef");
        assert_eq!(live_line_text("0123456789", 8), "456789");

        // A payload that sanitizes to nothing shows the idle marker, not junk.
        assert_eq!(live_line_text("\x1b]52;c;AAAA\x07", 80), "…");
    }

    #[test]
    fn live_line_strips_bidi_and_flattens_breaks() {
        let line = live_line_text("safe\u{202e}evil\nspoof", 80);
        assert_eq!(line, "safeevil spoof");
        assert!(!line.contains('\u{202e}'));
    }

    #[test]
    fn autopilot_turn_line_sanitizes_model_action() {
        let line = autopilot_turn_line(2, 5, "agent", "open door\x1b]52;c;AAAA\x07");
        assert_eq!(line, "[Autopilot turn 2/5 · agent] open door");
        assert!(!line.contains('\u{1b}'));
        assert!(!line.contains('\u{7}'));
    }

    #[test]
    fn report_fields_strip_terminal_controls() {
        let hostile = "Crypt \x1b]52;c;cGF5bG9hZA==\x07\u{202e}evil\nspoof";
        let safe = report_field(hostile);
        assert_eq!(safe, "Crypt evil spoof");
        assert!(!safe.contains('\u{1b}'));
        assert!(!safe.contains('\u{7}'));
        assert!(!safe.contains('\u{202e}'));
        assert!(!safe.contains('\n'));
        assert!(!safe.contains("52;"));
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
        let (r, blocks) = handle_meta_command(&mut state, "/scene", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert!(
            blocks.iter().any(
                |b| matches!(b, Block::System { text, .. } if text.contains("Unknown command"))
            )
        );
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
        state.set_last_narration("The old gate closed.");
        let mut flags = test_flags();
        let (r, _) =
            handle_meta_command(&mut state, &format!("/save {path_str}"), &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);

        let mut state2 = GameState::new("Other");
        let (r, _) =
            handle_meta_command(&mut state2, &format!("/load {path_str}"), &mut flags).unwrap();
        assert_eq!(r, MetaResult::Loaded);
        assert_eq!(state2.character_name, "Hero");
        assert_eq!(state2.hp, 11);
        assert_eq!(
            state2.last_narration.as_deref(),
            Some("The old gate closed.")
        );
    }

    #[tokio::test]
    async fn resume_seeds_history_from_saved_exchanges_without_calling_the_dm() {
        let mut state = GameState::new("Aria");
        state.set_last_narration("The gate groans open.");
        state.log("DM: stale truncated summary");
        state.record_exchange("light the torch", "The torch catches.");
        state.record_exchange("step inside", "The gate groans open.");
        // Unreachable on purpose: resume must be purely local.
        let client = DmClient::with_model("", "test-model").with_base_url("http://127.0.0.1:1");
        let mut history = ChatHistory::new(8);

        let (blocks, result) =
            start_session_collect(&mut state, &client, true, "heroic", &mut history)
                .await
                .unwrap();
        assert_eq!(result.text, "The gate groans open.");
        assert!(result.tool_calls.is_empty());
        assert_eq!(history.len(), 2, "resume seeds saved exchanges");
        let messages = history.api_messages();
        assert_eq!(messages[0]["content"], "Player action: light the torch");
        assert_eq!(messages[1]["content"], "The torch catches.");
        assert_eq!(messages[2]["content"], "Player action: step inside");
        assert_eq!(messages[3]["content"], "The gate groans open.");
        assert!(
            blocks
                .iter()
                .any(|b| matches!(b, Block::System { text, .. } if text.contains("Previously")))
        );
        assert!(
            blocks
                .iter()
                .any(|b| matches!(b, Block::Dm { text } if text == "The gate groans open."))
        );
    }

    #[tokio::test]
    async fn resume_falls_back_to_latest_dm_log_line_without_prefix() {
        let mut state = GameState::new("Aria");
        state.last_narration = None;
        state.log("Player: look");
        state.log("DM: The bridge is out.");
        state.log("Player: wait");
        let client = DmClient::with_model("", "test-model").with_base_url("http://127.0.0.1:1");
        let mut history = ChatHistory::new(2);

        let (blocks, result) =
            start_session_collect(&mut state, &client, true, "heroic", &mut history)
                .await
                .unwrap();
        assert_eq!(result.text, "The bridge is out.");
        assert!(history.is_empty(), "no saved exchanges to seed");
        assert!(
            blocks
                .iter()
                .any(|b| matches!(b, Block::Dm { text } if !text.starts_with("DM: ")))
        );
    }

    #[test]
    fn seed_history_preserves_order_and_applies_configured_cap() {
        use crate::game::state::MAX_EXCHANGES;

        let mut state = GameState::new("Aria");
        for i in 0..MAX_EXCHANGES {
            state.record_exchange(&format!("action {i}"), &format!("reply {i}"));
        }

        let mut full = ChatHistory::new(MAX_EXCHANGES as u32);
        seed_history(&mut full, &state);
        assert_eq!(full.len(), MAX_EXCHANGES);
        let messages = full.api_messages();
        assert_eq!(messages[0]["content"], "Player action: action 0");
        assert_eq!(
            messages[messages.len() - 1]["content"],
            format!("reply {}", MAX_EXCHANGES - 1)
        );

        // A smaller configured window keeps only the newest exchanges.
        let mut small = ChatHistory::new(2);
        seed_history(&mut small, &state);
        assert_eq!(small.len(), 2);
        let messages = small.api_messages();
        assert_eq!(
            messages[0]["content"],
            format!("Player action: action {}", MAX_EXCHANGES - 2)
        );
        assert_eq!(
            messages[3]["content"],
            format!("reply {}", MAX_EXCHANGES - 1)
        );
    }

    #[tokio::test]
    async fn opening_and_player_turns_record_exchanges_for_resume() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "The gate groans open."
                    }
                }]
            })))
            .mount(&server)
            .await;
        let client = DmClient::with_model("key", "grok-test").with_base_url(server.uri());

        let mut state = GameState::new("Aria");
        let mut history = ChatHistory::new(8);
        let (_blocks, opening) =
            start_session_collect(&mut state, &client, false, "heroic", &mut history)
                .await
                .unwrap();
        assert_eq!(state.recent_exchanges.len(), 1, "opening is recorded");
        assert!(
            state.recent_exchanges[0]
                .player
                .starts_with("Begin the adventure for Aria")
        );
        assert_eq!(state.recent_exchanges[0].dm, opening.text);

        let turn = run_dm_turn(&client, &mut state, "open the door", "heroic", &mut history)
            .await
            .unwrap();
        assert_eq!(state.recent_exchanges.len(), 2);
        assert_eq!(
            state.recent_exchanges[1],
            Exchange {
                player: "open the door".into(),
                dm: turn.text.clone(),
            }
        );
        assert_eq!(history.len(), 2);

        // Resume from a fresh history: seeded locally, never a DM call.
        let resume_client =
            DmClient::with_model("key", "grok-test").with_base_url("http://127.0.0.1:1");
        let mut resumed = ChatHistory::new(8);
        start_session_collect(&mut state, &resume_client, true, "heroic", &mut resumed)
            .await
            .unwrap();
        assert_eq!(resumed.len(), 2);
        assert_eq!(
            resumed.api_messages()[2]["content"],
            "Player action: open the door"
        );
    }

    #[tokio::test]
    async fn failed_or_cancelled_turns_do_not_record_exchanges() {
        // Port 1 is unreachable: the request fails before any turn commits.
        let client = DmClient::with_model("key", "grok-test").with_base_url("http://127.0.0.1:1");
        let mut state = GameState::new("Aria");
        let mut history = ChatHistory::new(8);

        let err =
            run_dm_turn_cancellable(&client, &mut state, "look", "heroic", None, &mut history)
                .await
                .unwrap_err();
        assert!(!format!("{err:#}").is_empty());
        assert!(state.recent_exchanges.is_empty());
        assert!(state.last_narration.is_none());
        assert!(history.is_empty());

        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = run_dm_turn_cancellable(
            &client,
            &mut state,
            "look",
            "heroic",
            Some(cancel),
            &mut history,
        )
        .await
        .unwrap_err();
        assert!(crate::dm::is_turn_cancelled(&err));
        assert!(state.recent_exchanges.is_empty());
        assert!(state.last_narration.is_none());
        assert!(history.is_empty());
    }

    #[tokio::test]
    async fn dead_character_cannot_start_a_dm_turn() {
        let mut state = GameState::new("Fallen");
        state.hp = 0;
        let client = DmClient::with_model("", "test-model");
        let mut history = ChatHistory::new(2);

        let err =
            run_dm_turn_cancellable(&client, &mut state, "look", "heroic", None, &mut history)
                .await
                .unwrap_err();
        assert!(format!("{err:#}").contains("has fallen"));
        assert!(history.is_empty());
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
    fn prompt_enforces_player_agency_and_options() {
        let prompt = static_dm_instructions("heroic");
        assert!(
            prompt.contains("never narrate the player's words, thoughts, feelings, or decisions")
        );
        assert!(prompt.contains("Stop at a decision point"));
        assert!(prompt.contains("2-3 concrete options with different risks"));
        assert!(prompt.contains("Options are invitations, not a limit"));
    }

    #[test]
    fn prompt_handles_ooc_and_refusal() {
        let prompt = static_dm_instructions("heroic");
        let lower = prompt.to_lowercase();
        assert!(lower.contains("out of character"));
        assert!(lower.contains("out-of-character questions"));
        assert!(prompt.contains("If the player refuses content or asks for a break"));
        assert!(prompt.contains("content break or fade-to-black"));
    }

    #[test]
    fn prompt_gives_dc_and_ability_guidance() {
        let prompt = static_dm_instructions("heroic");
        assert!(prompt.contains("DC bands: easy 10, medium 15, hard 20, very hard 25"));
        assert!(prompt.contains("Roll only when the outcome is uncertain and failure matters"));
        assert!(prompt.contains("never roll a stat-less 1d20"));
        assert!(prompt.contains("roll_dice(sides=20, ability=\"DEX\")"));
        assert!(prompt.contains("whenever a score applies"));
    }

    #[test]
    fn prompt_keeps_damage_and_marker_contract() {
        let prompt = static_dm_instructions("heroic");
        assert!(prompt.contains("only after an attack, save, or roll establishes it"));
        assert!(prompt.contains("always through modify_hp"));
        assert!(prompt.contains("No arbitrary damage"));
        assert!(prompt.contains("short rest heals but costs time and risk"));
        assert!(prompt.contains("long rest needs safety and can be interrupted"));
        assert!(prompt.contains("Never double-apply; tools win over markers"));
        assert!(prompt.contains("Fallback markers ([ROLL:...], [DAMAGE:...], [MOVE:...]"));
    }

    #[test]
    fn prompt_retains_every_tool() {
        let prompt = static_dm_instructions("heroic");
        for name in [
            "roll_dice",
            "add_to_inventory",
            "remove_from_inventory",
            "modify_hp",
            "move_to_room",
            "get_inventory",
            "inspect_item",
            "get_character_stats",
            "get_current_room",
            "add_companion",
            "update_companion",
            "remove_companion",
            "get_companions",
            "remember",
            "get_journal",
            "start_combat",
            "advance_turn",
            "end_combat",
            "set_objective",
        ] {
            assert!(prompt.contains(name), "prompt dropped tool {name}");
        }
    }

    #[test]
    fn prompt_requires_objective_tool_and_completion_flow() {
        let prompt = static_dm_instructions("heroic");
        assert!(prompt.contains("set_objective(objective=..."));
        assert!(prompt.contains("whenever the goal or threat materially changes"));
        assert!(prompt.contains("set_objective(complete=true)"));
        assert!(prompt.contains("narrate the epilogue"));
        assert!(prompt.contains("Objective and Threat lines in state are canon"));
    }

    #[test]
    fn prompt_states_dnd5e_lite_and_class_feature_rules() {
        let prompt = static_dm_instructions("heroic");
        assert!(prompt.contains("Ruleset: dnd5e-lite"));
        assert!(prompt.contains("Not supported: AC, proficiency, levels, spell slots"));
        assert!(prompt.contains("Class features are narrative permissions"));
        assert!(prompt.contains("Class feature line in state is canon"));
        assert!(prompt.contains("never invent extra uses, spell slots, or free damage"));
    }

    #[test]
    fn tones_are_distinct_blocks() {
        let dark = static_dm_instructions("dark_fantasy");
        let heroic = static_dm_instructions("heroic");
        let comedic = static_dm_instructions("comedic");
        let custom = static_dm_instructions("custom");

        assert!(dark.contains("Dread lives between the lines"));
        assert!(heroic.contains("Heroism is a choice with a price"));
        assert!(comedic.contains("Comic timing, not cruelty"));
        assert!(custom.contains("Follow the player's lead"));
        assert!(custom.contains("default to heroic fantasy with clear stakes"));

        assert_ne!(dark, heroic);
        assert_ne!(heroic, comedic);
        assert_ne!(comedic, custom);
        assert_ne!(dark, comedic);
    }

    #[test]
    fn opening_action_requires_inciting_incident_and_first_choice() {
        let mut state = GameState::new("Mira");
        state.class = Some("Rogue".into());
        let opening = opening_action(&state);
        assert!(opening.contains("Mira"));
        assert!(opening.contains("Rogue"));
        assert!(opening.contains("inciting incident"));
        assert!(opening.contains("move_to_room(room_name=...)"));
        assert!(opening.contains("threat or clock"));
        assert!(opening.contains("set_objective(objective=..."));
        assert!(opening.contains("decision point"));
        assert!(opening.contains("2-3 concrete options"));
        assert!(opening.contains("Do not narrate Mira's words"));
        assert!(!opening.contains("wakes up"));
    }

    #[test]
    fn opening_action_is_classless_aware() {
        let state = GameState::new("Nomad");
        let opening = opening_action(&state);
        assert!(opening.contains("Nomad"));
        assert!(opening.contains("none - no training/kit"));
    }

    #[test]
    fn system_prompt_always_shows_class() {
        let classless = GameState::new("Thorin");
        let prompt = build_system_prompt(&classless);
        assert!(prompt.contains("Class: none — no training/kit"));

        let mut classed = GameState::new("Thorin");
        classed.class = Some("Wizard".into());
        let prompt = build_system_prompt(&classed);
        assert!(prompt.contains("Class: Wizard"));
        assert!(!prompt.contains("Class: none"));
    }

    #[test]
    fn static_prompt_stays_compact() {
        for tone in ["dark_fantasy", "heroic", "comedic", "custom"] {
            let len = static_dm_instructions(tone).chars().count();
            assert!(len < 8000, "{tone} prompt is {len} chars; target <8000");
        }
    }

    #[test]
    fn state_query_handler_returns_true() {
        let state = GameState::new("Hero");
        assert!(handle_state_query(&state, "where am i", false));
        assert!(!handle_state_query(&state, "open the chest", false));
    }

    #[test]
    fn turn_blocks_put_narration_before_mechanics() {
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
        assert!(matches!(blocks.first(), Some(Block::Dm { .. })));
        assert!(blocks.iter().any(|b| matches!(b, Block::Dice { .. })));
        assert!(matches!(blocks.last(), Some(Block::Tools { .. })));
        let dm_at = blocks
            .iter()
            .position(|b| matches!(b, Block::Dm { .. }))
            .unwrap();
        let dice_at = blocks
            .iter()
            .position(|b| matches!(b, Block::Dice { .. }))
            .unwrap();
        let tools_at = blocks
            .iter()
            .position(|b| matches!(b, Block::Tools { .. }))
            .unwrap();
        assert!(dm_at < dice_at && dice_at < tools_at, "{blocks:?}");
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
    fn turn_blocks_with_deltas_aggregates_state_before_tools() {
        let mut state = GameState::new("Hero");
        let before = StateSnapshot::from(&state);
        state.modify_hp(-2);
        state.add_item("Iron Dagger", "blade", 1);
        let result = TurnResult {
            text: "Ouch.".into(),
            tool_calls: vec![ToolCallRecord {
                name: "modify_hp".into(),
                arguments: "{}".into(),
                result: "Applied -2 HP".into(),
            }],
            mutating_tool_ran: true,
            usage: UsageTotals::default(),
        };
        let blocks = turn_blocks_with_deltas(&result, &before, &state);
        assert!(matches!(blocks.first(), Some(Block::Dm { .. })));

        let state_lines: Vec<_> = blocks
            .iter()
            .filter_map(|b| match b {
                Block::State { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(state_lines.len(), 1, "one aggregated State block");
        assert!(state_lines[0].contains("💔") && state_lines[0].contains("HP"));
        assert!(state_lines[0].contains("Iron Dagger"));
        assert!(state_lines[0].contains('\n'), "deltas are newline-joined");

        let state_at = blocks
            .iter()
            .position(|b| matches!(b, Block::State { .. }))
            .unwrap();
        let tools_at = blocks
            .iter()
            .position(|b| matches!(b, Block::Tools { .. }))
            .unwrap();
        assert!(state_at < tools_at, "State before Tools: {blocks:?}");
        assert!(matches!(blocks.last(), Some(Block::Tools { .. })));
    }
}
