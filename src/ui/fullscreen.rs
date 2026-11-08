//! Fullscreen alt-screen interactive loop (ratatui + crossterm).
//!
//! Shares turn/meta helpers with the minimal loop. DM speech (TTS) plays after
//! turns. Hold **Ctrl+Space** (or **F2**) to talk: words appear in the
//! composer while you speak. Release commits the line into the prompt when
//! edit-before-send is on.

use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::config::Config;
use crate::dm::{ChatHistory, DmClient, is_turn_cancelled};
use crate::game::delta::{self, StateSnapshot};
use crate::game::loop_::{
    MetaResult, SessionFlags, handle_meta_command, handle_state_query_blocks, make_dm_client,
    maybe_speak_dm, run_dm_turn_cancellable, start_session_collect, stt_client,
    turn_blocks_with_deltas,
};
use crate::game::save::SaveGame;
use crate::game::state::GameState;
use crate::io::audio::{self, ActivePlayback, clamp_record_seconds};
use crate::io::listen::ListenHandle;
use crate::io::stt::SttClient;
use crate::io::tts::TtsClient;
use crate::ui::app::FullscreenApp;
use crate::ui::blocks::{Block, SystemKind};
use crate::ui::status;
use tokio_util::sync::CancellationToken;

/// In-flight hold-to-talk capture (Ctrl+Space or F2).
struct PttCapture {
    listen: ListenHandle,
    /// When recording started (for the elapsed-time activity readout).
    started: std::time::Instant,
    /// Max record seconds (for the activity readout).
    max_secs: u32,
}

/// RAII guard: leave alt-screen and disable raw mode on drop (including panic unwind).
struct TerminalGuard {
    stdout: Stdout,
    active: bool,
    /// Whether we pushed keyboard enhancement (for push-to-talk press/release).
    kb_enhanced: bool,
    mouse_enabled: bool,
}

impl TerminalGuard {
    fn enter() -> Result<(Terminal<CrosstermBackend<Stdout>>, Self)> {
        enable_raw_mode().context("enable raw mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
        // Prefer press/release reports so hold-to-talk works (Ghostty/Kitty/etc.).
        let kb_enhanced = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
        )
        .is_ok();
        // Mouse wheel → scrollback.
        let mouse_enabled = execute!(stdout, EnableMouseCapture).is_ok();
        let backend = CrosstermBackend::new(io::stdout());
        let terminal = Terminal::new(backend).context("create terminal")?;
        Ok((
            terminal,
            Self {
                stdout,
                active: true,
                kb_enhanced,
                mouse_enabled,
            },
        ))
    }

    fn restore(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        if self.mouse_enabled {
            let _ = execute!(self.stdout, DisableMouseCapture);
        }
        if self.kb_enhanced {
            let _ = execute!(self.stdout, PopKeyboardEnhancementFlags);
        }
        let _ = disable_raw_mode();
        let _ = execute!(self.stdout, LeaveAlternateScreen);
        // Show cursor again.
        let _ = execute!(self.stdout, crossterm::cursor::Show);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Run the interactive adventure in a fullscreen ratatui alt-screen.
pub async fn run_fullscreen_interactive(
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
    let record_secs_default = config.stt.record_seconds.max(1);
    let max_record_secs = config.stt.max_record_seconds.max(1);
    let sample_rate = config.stt.sample_rate;
    let device = config.stt.device;

    let (mut terminal, mut guard) = TerminalGuard::enter()?;
    // Cursor is positioned by FullscreenApp::draw when the prompt is focused.
    let _ = execute!(io::stdout(), crossterm::cursor::Show);

    let mut app = FullscreenApp::with_options(
        config.ui.theme.clone(),
        flags.tools_expanded,
        config.ui.show_shortcuts,
        config.ui.compact,
    );
    app.status_line = status::format_status_plain(state);
    let mut ptt: Option<PttCapture> = None;

    let mut hello = format!(
        "Grok Adventures  ·  {} · {}",
        client.model(),
        client.reasoning_effort()
    );
    if flags.enable_stt {
        hello.push_str("  ·  hold ctrl+space to talk");
    }
    app.push_block(Block::System {
        text: hello,
        kind: SystemKind::Info,
    });
    app.push_block(Block::System {
        text: crate::ui::creation::summary_line(state),
        kind: SystemKind::Info,
    });

    // Opening / resume
    app.activity = Some("Setting the scene…".into());
    redraw(&mut terminal, &app)?;
    let opening = start_session_collect(state, &client, resume, tone, &mut history).await;
    app.activity = None;
    match opening {
        Ok((blocks, result)) => {
            app.push_blocks(blocks);
            app.tools_expanded = flags.tools_expanded;
            app.status_line = status::format_status_plain(state);
            // Speak the opening text as soon as it is ready.
            if flags.enable_tts {
                app.activity = Some("Speaking…".into());
                redraw(&mut terminal, &app)?;
                if let Some(err) =
                    maybe_speak_dm(&tts, &result.text, dm_voice, true, Some(&playback)).await
                {
                    app.push_block(err);
                }
                app.activity = None;
            }
        }
        Err(e) => {
            app.push_block(Block::System {
                text: format!("Failed to start session: {e:#}"),
                kind: SystemKind::Error,
            });
        }
    }
    redraw(&mut terminal, &app)?;

    let mut death_announced = false;
    loop {
        if app.should_quit {
            break;
        }
        if !state.alive() {
            if !death_announced {
                app.push_block(Block::System {
                    text: format!("Game Over — {} has fallen!", state.character_name),
                    kind: SystemKind::Error,
                });
                app.push_block(Block::System {
                    text: "Type /resurrect to continue testing (1 HP), or /quit to end.".into(),
                    kind: SystemKind::Info,
                });
                death_announced = true;
                app.status_line = status::format_status_plain(state);
            }
        } else {
            death_announced = false;
        }

        app.sync_completions();
        redraw(&mut terminal, &app)?;

        // Poll input with timeout so we can redraw if needed.
        if event::poll(Duration::from_millis(100)).context("poll events")? {
            match event::read().context("read event")? {
                Event::Key(key) => {
                    handle_key_with_ptt(
                        &mut app,
                        key,
                        &mut ptt,
                        &stt,
                        flags.enable_stt,
                        sample_rate,
                        device,
                        max_record_secs,
                    );
                }
                Event::Mouse(me) => match me.kind {
                    MouseEventKind::ScrollUp => app.on_scroll_up(),
                    MouseEventKind::ScrollDown => app.on_scroll_down(),
                    _ => {}
                },
                Event::Resize(_, _) => {
                    // Next loop iteration redraws.
                }
                _ => {}
            }
        }

        // Live words while holding push-to-talk.
        if let Some(cap) = ptt.as_ref() {
            let elapsed = cap.started.elapsed().as_secs();
            let label = format!("Listening · {elapsed}/{}s", cap.max_secs);
            let live = cap.listen.partial();
            let changed = app.activity.as_deref() != Some(label.as_str()) || app.live_line != live;
            if changed {
                app.activity = Some(label);
                app.live_line = live;
                redraw(&mut terminal, &app)?;
            }
        }

        // Finish hold-to-talk when the listener stops (release or max).
        if let Some(cap) = ptt.as_ref()
            && cap.listen.is_finished()
        {
            let finished = ptt.take().expect("ptt present");
            app.ptt_held = false;
            app.live_line.clear();
            match finished.listen.finish().await {
                Ok(transcript) => {
                    app.activity = None;
                    let edit = flags.stt_edit_before_send;
                    accept_transcript(
                        state,
                        &client,
                        &tts,
                        &mut app,
                        &mut terminal,
                        &transcript,
                        edit,
                        &mut flags,
                        tone,
                        dm_voice,
                        &mut history,
                        &playback,
                    )
                    .await;
                }
                Err(e) => {
                    app.activity = None;
                    app.push_block(Block::System {
                        text: format!("Voice capture failed: {e:#}"),
                        kind: SystemKind::Error,
                    });
                }
            }
            app.tools_expanded = flags.tools_expanded;
            app.status_line = status::format_status_plain(state);
            app.activity = None;
            app.live_line.clear();
        }

        // Process submitted action (skip while PTT is recording).
        if ptt.is_none()
            && let Some(action) = app.take_pending()
        {
            if let Err(e) = process_action(
                state,
                &client,
                &tts,
                &stt,
                &mut app,
                &mut terminal,
                &action,
                &mut flags,
                tone,
                dm_voice,
                record_secs_default,
                max_record_secs,
                sample_rate,
                device,
                &mut history,
                &playback,
            )
            .await
            {
                app.push_block(Block::System {
                    text: format!("Error: {e:#}"),
                    kind: SystemKind::Error,
                });
            }
            app.tools_expanded = flags.tools_expanded;
            app.status_line = status::format_status_plain(state);
            app.activity = None;
        }
    }

    // Autosave on clean exit
    if state.alive()
        && let Err(e) = SaveGame::save_slot(state, Some("autosave"))
    {
        // Terminal still up briefly — push message then restore.
        app.push_block(Block::System {
            text: format!("Autosave failed: {e:#}"),
            kind: SystemKind::Error,
        });
        let _ = redraw(&mut terminal, &app);
        std::thread::sleep(Duration::from_millis(800));
    }

    guard.restore();
    Ok(())
}

fn redraw(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &FullscreenApp) -> Result<()> {
    terminal
        .draw(|frame| app.draw(frame))
        .context("draw frame")?;
    Ok(())
}

/// Whether a key event is the push-to-talk hotkey.
///
/// Ctrl+Space is the primary hold-to-talk key (no fn-key needed on Mac
/// laptops); F2 stays as a legacy alternative. Without terminal keyboard
/// enhancement, Ctrl+Space may arrive as a bare NUL byte (`KeyCode::Null`),
/// which we accept too — release is then emulated by pressing again.
fn is_ptt_key(key: &KeyEvent) -> bool {
    if key.code == KeyCode::F(2) || key.code == KeyCode::Null {
        return true;
    }
    key.code == KeyCode::Char(' ')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::SUPER)
}

#[allow(clippy::too_many_arguments)]
fn handle_key_with_ptt(
    app: &mut FullscreenApp,
    key: KeyEvent,
    ptt: &mut Option<PttCapture>,
    stt: &SttClient,
    enable_stt: bool,
    sample_rate: u32,
    device: i32,
    max_record_secs: u32,
) {
    // Hold-to-talk: Press starts, Release stops (plain Space still types).
    // Ignore key-repeat. Without release events, second Press toggles stop; Esc also stops.
    if is_ptt_key(&key) {
        match key.kind {
            KeyEventKind::Repeat => {}
            KeyEventKind::Press if enable_stt && ptt.is_none() && !app.ptt_held => {
                let max = max_record_secs.max(1);
                *ptt = Some(PttCapture {
                    listen: ListenHandle::start(stt.clone(), sample_rate, device, max),
                    started: std::time::Instant::now(),
                    max_secs: max,
                });
                app.ptt_held = true;
                app.live_line.clear();
                app.activity = Some("Listening".into());
            }
            KeyEventKind::Release | KeyEventKind::Press if app.ptt_held => {
                // Release = normal hold-to-talk end; Press while held = toggle stop
                // on terminals that only emit Press.
                if let Some(cap) = ptt.as_ref() {
                    cap.listen.stop();
                }
            }
            _ => {}
        }
        return;
    }

    // Ignore other keys while holding PTT (except Ctrl+C).
    if key.kind != KeyEventKind::Press {
        return;
    }

    // Ctrl+C always quits.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if let Some(cap) = ptt.as_ref() {
            cap.listen.stop();
        }
        app.should_quit = true;
        return;
    }

    if app.ptt_held {
        // Esc cancels / stops recording early.
        if key.code == KeyCode::Esc
            && let Some(cap) = ptt.as_ref()
        {
            cap.listen.stop();
        }
        return;
    }

    match key.code {
        KeyCode::Esc => app.on_esc(),
        KeyCode::Tab => {
            // Tab completes a slash command when the menu is open;
            // otherwise it keeps its focus-toggle role.
            if !(app.focus == crate::ui::app::Focus::Prompt && app.apply_completion()) {
                app.toggle_focus();
            }
        }
        KeyCode::BackTab => {
            if app.focus == crate::ui::app::Focus::Prompt && app.completions.is_open() {
                app.completions.move_up();
            } else {
                app.toggle_focus();
            }
        }
        KeyCode::Enter => {
            if app.focus == crate::ui::app::Focus::Prompt {
                app.submit_prompt();
            }
        }
        KeyCode::Backspace => app.on_backspace(),
        KeyCode::Delete => app.on_delete(),
        KeyCode::Left => app.on_left(),
        KeyCode::Right => app.on_right(),
        KeyCode::Home => app.on_home(),
        KeyCode::End => app.on_end(),
        KeyCode::Up => {
            if app.focus == crate::ui::app::Focus::Prompt {
                if app.completions.is_open() {
                    app.completions.move_up();
                } else {
                    let _ = app.history_up();
                }
            } else {
                app.on_scroll_up();
            }
        }
        KeyCode::Down => {
            if app.focus == crate::ui::app::Focus::Prompt {
                if app.completions.is_open() {
                    app.completions.move_down();
                } else {
                    let _ = app.history_down();
                }
            } else {
                app.on_scroll_down();
            }
        }
        KeyCode::PageUp => {
            app.scrollback.scroll_up(10);
        }
        KeyCode::PageDown => {
            app.scrollback.scroll_down(10);
        }
        KeyCode::Char(c) => {
            // Ctrl+L clear scrollback (nice to have)
            if key.modifiers.contains(KeyModifiers::CONTROL) && (c == 'l' || c == 'L') {
                app.scrollback.clear();
            } else {
                app.on_char(c);
            }
        }
        _ => {}
    }
}

/// Put a finished transcript in the composer, or send it when edit-before-send is off.
#[allow(clippy::too_many_arguments)]
async fn accept_transcript(
    state: &mut GameState,
    client: &DmClient,
    tts: &TtsClient,
    app: &mut FullscreenApp,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    transcript: &str,
    edit_before_send: bool,
    flags: &mut SessionFlags,
    tone: &str,
    dm_voice: &str,
    history: &mut ChatHistory,
    playback: &ActivePlayback,
) {
    app.live_line.clear();
    let action = transcript.trim();
    if action.is_empty() {
        app.push_block(Block::System {
            text: "Heard nothing — try again or type your action.".into(),
            kind: SystemKind::Info,
        });
        return;
    }
    if edit_before_send {
        let existing = app.prompt.input().trim().to_string();
        if existing.is_empty() {
            app.prompt.set(action);
        } else {
            app.prompt.set(format!("{existing} {action}"));
        }
        return;
    }
    if !state.alive() {
        app.push_block(Block::System {
            text: "You have fallen. Type /resurrect to continue testing, or /quit to end.".into(),
            kind: SystemKind::Info,
        });
        return;
    }
    if let Err(e) = run_player_turn(
        state, client, tts, app, terminal, action, true, flags, tone, dm_voice, history, playback,
    )
    .await
    {
        app.push_block(Block::System {
            text: format!("Error: {e:#}"),
            kind: SystemKind::Error,
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_action(
    state: &mut GameState,
    client: &DmClient,
    tts: &TtsClient,
    stt: &SttClient,
    app: &mut FullscreenApp,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    input: &str,
    flags: &mut SessionFlags,
    tone: &str,
    dm_voice: &str,
    record_secs_default: u32,
    max_record_secs: u32,
    sample_rate: u32,
    device: i32,
    history: &mut ChatHistory,
    playback: &ActivePlayback,
) -> Result<()> {
    // Fullscreen-local commands before generic meta.
    let (cmd, _) = crate::game::loop_::parse_command(input);
    if cmd == "theme" {
        app.cycle_theme();
        app.push_block(Block::System {
            text: format!("Theme: {}", app.theme_name),
            kind: SystemKind::Info,
        });
        return Ok(());
    }
    if cmd == "clear" || cmd == "cls" {
        app.scrollback.clear();
        app.push_block(Block::System {
            text: "Scrollback cleared.".into(),
            kind: SystemKind::Info,
        });
        return Ok(());
    }
    let is_slash = input.trim_start().starts_with('/');
    let bare_one_word = input.split_whitespace().count() == 1;
    if (cmd == "copy" || cmd == "again") && (is_slash || bare_one_word) {
        if cmd == "copy" {
            if app.last_dm_text.trim().is_empty() {
                app.push_block(Block::System {
                    text: "Nothing to copy yet — wait for DM narration.".into(),
                    kind: SystemKind::Info,
                });
            } else {
                match crate::util::clipboard::copy_text(&app.last_dm_text) {
                    Ok(msg) => app.push_block(Block::System {
                        text: msg,
                        kind: SystemKind::Success,
                    }),
                    Err(e) => app.push_block(Block::System {
                        text: format!("Copy failed: {e:#}"),
                        kind: SystemKind::Error,
                    }),
                }
            }
            return Ok(());
        }
        if let Some(prev) = app.history.last().map(str::to_string) {
            return run_player_turn(
                state, client, tts, app, terminal, &prev, false, flags, tone, dm_voice, history,
                playback,
            )
            .await;
        }
        app.push_block(Block::System {
            text: "No previous action to repeat. Type something first.".into(),
            kind: SystemKind::Info,
        });
        return Ok(());
    }

    match handle_meta_command(state, input, flags)? {
        (MetaResult::Quit, blocks) => {
            app.push_blocks(blocks);
            app.should_quit = true;
            return Ok(());
        }
        (MetaResult::Handled, blocks) => {
            app.push_blocks(blocks);
            app.tools_expanded = flags.tools_expanded;
            app.status_line = status::format_status_plain(state);
            return Ok(());
        }
        (MetaResult::Talk { seconds }, blocks) => {
            app.push_blocks(blocks);
            if !flags.enable_stt {
                app.push_block(Block::System {
                    text: "STT is off. Enable with /voice.".into(),
                    kind: SystemKind::Info,
                });
                return Ok(());
            }
            let secs =
                clamp_record_seconds(seconds.unwrap_or(record_secs_default), max_record_secs);
            let handle = ListenHandle::start(stt.clone(), sample_rate, device, secs);
            loop {
                if event::poll(Duration::from_millis(40)).context("poll events")?
                    && let Event::Key(key) = event::read().context("read event")?
                    && key.kind == KeyEventKind::Press
                    && matches!(key.code, KeyCode::Enter | KeyCode::Esc)
                {
                    handle.stop();
                }
                app.live_line = handle.partial();
                app.activity = Some("Listening".into());
                redraw(terminal, app)?;
                if handle.is_finished() {
                    break;
                }
            }
            app.live_line.clear();
            app.activity = None;
            match handle.finish().await {
                Ok(transcript) => {
                    let edit = flags.stt_edit_before_send;
                    accept_transcript(
                        state,
                        client,
                        tts,
                        app,
                        terminal,
                        &transcript,
                        edit,
                        flags,
                        tone,
                        dm_voice,
                        history,
                        playback,
                    )
                    .await;
                }
                Err(e) => {
                    app.push_block(Block::System {
                        text: format!("Voice capture failed: {e:#}"),
                        kind: SystemKind::Error,
                    });
                }
            }
            return Ok(());
        }
        (MetaResult::NotMeta, _) => {}
    }

    // While dead, only meta commands (handled above) are allowed.
    if !state.alive() {
        app.push_block(Block::System {
            text: "You have fallen. Type /resurrect to continue testing, or /quit to end.".into(),
            kind: SystemKind::Info,
        });
        return Ok(());
    }

    if let Some(blocks) = handle_state_query_blocks(state, input) {
        app.push_blocks(blocks);
        return Ok(());
    }

    run_player_turn(
        state, client, tts, app, terminal, input, false, flags, tone, dm_voice, history, playback,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_player_turn(
    state: &mut GameState,
    client: &DmClient,
    tts: &TtsClient,
    app: &mut FullscreenApp,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    input: &str,
    spoken: bool,
    flags: &SessionFlags,
    tone: &str,
    dm_voice: &str,
    history: &mut ChatHistory,
    playback: &ActivePlayback,
) -> Result<()> {
    // New turn stops any leftover TTS from the previous beat.
    playback.stop();

    app.push_block(Block::Player {
        text: input.to_string(),
        spoken,
    });
    app.activity = Some("Consulting the DM…".into());
    redraw(terminal, app)?;

    // Snapshot so Esc cancel can undo partial tool mutations mid-turn.
    let state_snapshot = state.clone();
    let before = StateSnapshot::from(&*state);
    let cancel = CancellationToken::new();
    let cancel_for_dm = cancel.clone();

    // Scope the pinned future so its `&mut GameState` borrow ends before
    // we restore/read `state` after cancel or success.
    let result = {
        let dm_fut =
            run_dm_turn_cancellable(client, state, input, tone, Some(cancel_for_dm), history);
        tokio::pin!(dm_fut);

        loop {
            tokio::select! {
                biased;
                res = &mut dm_fut => break res,
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    // Drain pending keys while the DM is thinking.
                    while event::poll(Duration::from_millis(0)).context("poll during DM turn")? {
                        match event::read().context("read during DM turn")? {
                            Event::Key(key) if key.kind == KeyEventKind::Press => {
                                if key.modifiers.contains(KeyModifiers::CONTROL)
                                    && key.code == KeyCode::Char('c')
                                {
                                    // Still allow hard quit during a long turn.
                                    app.should_quit = true;
                                    cancel.cancel();
                                    playback.stop();
                                } else if key.code == KeyCode::Esc {
                                    cancel.cancel();
                                    playback.stop();
                                    app.activity = Some("Cancelling…".into());
                                    redraw(terminal, app)?;
                                }
                            }
                            Event::Resize(_, _) => {
                                redraw(terminal, app)?;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    };

    let result = match result {
        Ok(r) => r,
        Err(e) if is_turn_cancelled(&e) => {
            // Restore pre-turn state so partial tool mutations do not stick.
            state.replace_with(state_snapshot);
            playback.stop();
            app.activity = None;
            app.push_block(Block::System {
                text: "Turn cancelled.".into(),
                kind: SystemKind::Info,
            });
            // Keep the draft so the player can edit / resend.
            app.prompt.set(input);
            app.status_line = status::format_status_plain(state);
            return Ok(());
        }
        Err(e) => {
            app.activity = None;
            app.push_block(Block::System {
                text: format!("The mystical connection wavers... ({e:#})"),
                kind: SystemKind::Error,
            });
            return Ok(());
        }
    };

    app.activity = None;
    if delta::took_damage(&before, state) {
        audio::maybe_play_damage_sfx();
    }
    app.push_blocks(turn_blocks_with_deltas(&result, &before, state));
    app.status_line = status::format_status_plain(state);

    // Speak the narration as soon as it is available; Esc stops speech.
    if flags.enable_tts {
        app.activity = Some("Speaking…".into());
        redraw(terminal, app)?;

        let speak_fut = maybe_speak_dm(tts, &result.text, dm_voice, true, Some(playback));
        tokio::pin!(speak_fut);

        let mut speak_done: Option<Option<Block>> = None;
        while speak_done.is_none() {
            tokio::select! {
                biased;
                res = &mut speak_fut => {
                    speak_done = Some(res);
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    while event::poll(Duration::from_millis(0)).context("poll during narrate")? {
                        match event::read().context("read during narrate")? {
                            Event::Key(key) if key.kind == KeyEventKind::Press => {
                                if key.code == KeyCode::Esc
                                    || (key.modifiers.contains(KeyModifiers::CONTROL)
                                        && key.code == KeyCode::Char('c'))
                                {
                                    // Stop speech immediately.
                                    playback.stop();
                                    if key.modifiers.contains(KeyModifiers::CONTROL)
                                        && key.code == KeyCode::Char('c')
                                    {
                                        app.should_quit = true;
                                    }
                                    app.activity = Some("Speech cancelled…".into());
                                    redraw(terminal, app)?;
                                }
                            }
                            Event::Resize(_, _) => {
                                redraw(terminal, app)?;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        if let Some(Some(err)) = speak_done {
            app.push_block(err);
        }
        app.activity = None;
        redraw(terminal, app)?;
    }

    Ok(())
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

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn ptt_accepts_ctrl_space_f2_and_nul() {
        assert!(is_ptt_key(&key(KeyCode::F(2), KeyModifiers::empty())));
        assert!(is_ptt_key(&key(KeyCode::Char(' '), KeyModifiers::CONTROL)));
        // Bare NUL (Ctrl+Space without keyboard enhancement).
        assert!(is_ptt_key(&key(KeyCode::Null, KeyModifiers::empty())));
        // Release events count too (matcher ignores press/release).
        assert!(is_ptt_key(&KeyEvent {
            kind: KeyEventKind::Release,
            ..key(KeyCode::F(2), KeyModifiers::empty())
        }));
    }

    #[test]
    fn ptt_rejects_typing_keys() {
        // Plain space must keep typing.
        assert!(!is_ptt_key(&key(KeyCode::Char(' '), KeyModifiers::empty())));
        assert!(!is_ptt_key(&key(KeyCode::Char('a'), KeyModifiers::empty())));
        // Other Ctrl combos are not PTT (Ctrl+L clears scrollback).
        assert!(!is_ptt_key(&key(KeyCode::Char('l'), KeyModifiers::CONTROL)));
        // Extra modifiers disqualify Ctrl+Space.
        assert!(!is_ptt_key(&key(
            KeyCode::Char(' '),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
        assert!(!is_ptt_key(&key(KeyCode::Esc, KeyModifiers::empty())));
    }
}
