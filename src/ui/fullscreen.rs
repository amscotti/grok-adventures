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
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
    MetaResult, SessionFlags, ensure_alive, handle_meta_command, handle_state_query_blocks,
    make_dm_client, maybe_speak_dm, run_dm_turn_cancellable, seed_history, session_usage_block,
    spawn_speak_dm, start_session_collect, stt_client, turn_blocks_with_deltas,
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
    /// Whether bracketed paste is active (multi-line clipboard input).
    paste_enabled: bool,
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
        // Deliver a multi-line paste as one Event::Paste instead of a burst
        // of Enter/character keys that would submit line by line.
        let paste_enabled = execute!(stdout, EnableBracketedPaste).is_ok();
        let backend = CrosstermBackend::new(io::stdout());
        let terminal = Terminal::new(backend).context("create terminal")?;
        Ok((
            terminal,
            Self {
                stdout,
                active: true,
                kb_enhanced,
                mouse_enabled,
                paste_enabled,
            },
        ))
    }

    fn restore(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        if self.paste_enabled {
            let _ = execute!(self.stdout, DisableBracketedPaste);
        }
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
    app.status_line = status::StatusLine::from_state(state);
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

    // Opening / resume. Keep the terminal responsive while the DM request is
    // in flight: Esc aborts it, Ctrl+C quits, and typing is buffered by the
    // composer so the first prompt is never a dead screen.
    app.activity = Some("Setting the scene…".into());
    redraw(&mut terminal, &app)?;
    let opening = {
        let opening_fut = start_session_collect(state, &client, resume, tone, &mut history);
        tokio::pin!(opening_fut);
        loop {
            tokio::select! {
                biased;
                res = &mut opening_fut => break Some(res),
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    match drain_busy_events(&mut app, &mut terminal)? {
                        BusyDrain::Cancel => {
                            playback.stop();
                            app.pending_action = None;
                            app.push_block(Block::System {
                                text: "Opening cancelled — type an action or /help.".into(),
                                kind: SystemKind::Info,
                            });
                            break None;
                        }
                        BusyDrain::Quit => {
                            playback.stop();
                            app.should_quit = true;
                            break None;
                        }
                        BusyDrain::None => {}
                    }
                }
            }
        }
    };
    app.activity = None;
    if let Some(opening) = opening {
        match opening {
            Ok((blocks, result)) => {
                app.push_blocks(blocks);
                app.usage.record_turn(&result.usage);
                app.tools_expanded = flags.tools_expanded;
                app.status_line = status::StatusLine::from_state(state);
                // Speak a fresh opening; resume is a local recap and stays silent.
                // Background speech keeps the composer available immediately.
                if flags.enable_tts && !resume {
                    spawn_speak_dm(&tts, &result.text, dm_voice, &playback);
                }
            }
            Err(e) => {
                app.push_block(Block::System {
                    text: format!("Failed to start session: {e:#}"),
                    kind: SystemKind::Error,
                });
            }
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
                app.status_line = status::StatusLine::from_state(state);
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
                Event::Paste(text) => app.on_paste(&text),
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

        // Live words while holding push-to-talk. Once released, keep the
        // last partial on screen but stop overwriting "Transcribing…".
        if let (Some(cap), true) = (ptt.as_ref(), app.ptt_held) {
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
            app.status_line = status::StatusLine::from_state(state);
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
                config.game.history_turns,
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
            app.status_line = status::StatusLine::from_state(state);
            app.activity = None;
        }
    }

    // Autosave on clean exit
    playback.stop();
    if state.alive()
        && state.has_progress()
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

/// Leave recording state immediately when capture is told to stop, so the
/// composer accepts keys while the transcription task finishes.
fn mark_ptt_transcribing(app: &mut FullscreenApp) {
    app.ptt_held = false;
    app.activity = Some("Transcribing…".into());
}

/// Stop the live capture (if any) and flip the UI to the transcribing state.
fn stop_ptt_capture(app: &mut FullscreenApp, ptt: &mut Option<PttCapture>) {
    if let Some(cap) = ptt.as_ref() {
        cap.listen.stop();
        mark_ptt_transcribing(app);
    }
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
                stop_ptt_capture(app, ptt);
            }
            // A second press while the previous capture is still transcribing
            // is ignored: only one capture may be in flight.
            _ => {}
        }
        return;
    }

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
        // Esc stops recording early; other keys are ignored while the mic is
        // live. After release the held flag is already false, so typing works
        // even while the transcription task is still finishing.
        if key.code == KeyCode::Esc {
            stop_ptt_capture(app, ptt);
        }
        return;
    }

    handle_idle_key(app, key);
}

/// Handle one non-PTT key while the composer is idle.
fn handle_idle_key(app: &mut FullscreenApp, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return;
    }

    match key.code {
        KeyCode::Esc => app.on_esc(),
        KeyCode::Tab => {
            // Tab completes a slash command when the menu is open. Otherwise
            // it toggles focus only from an empty composer; a bare Tab must
            // never strand a non-empty draft in scrollback focus.
            if app.focus == crate::ui::app::Focus::Prompt && app.apply_completion() {
                // completed
            } else if app.prompt.is_empty() {
                app.toggle_focus();
            }
        }
        KeyCode::BackTab => {
            if app.focus == crate::ui::app::Focus::Prompt && app.completions.is_open() {
                app.completions.move_up();
            } else if app.prompt.is_empty() {
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
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.on_ctrl_edit(c);
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            app.on_char(c);
        }
        _ => {}
    }
}

/// What the caller should do after a key arrives during a DM turn or speech.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BusyOutcome {
    /// Key was handled (typing, editing, queueing, scrolling).
    Handled,
    /// Esc: cancel the in-flight turn or stop speech.
    Cancel,
    /// Ctrl+C: quit the app.
    Quit,
}

/// Handle a key while a DM turn or TTS is in flight.
///
/// The composer stays editable: printable/editing keys buffer into the prompt
/// and Enter queues the line for the outer loop (never a second concurrent
/// turn). Esc cancels the turn/speech; Ctrl+C quits. The caller owns playback
/// and cancellation tokens, so this helper is unit-testable without a live
/// TTS or game handle.
fn handle_busy_key(app: &mut FullscreenApp, key: KeyEvent) -> BusyOutcome {
    if key.kind != KeyEventKind::Press {
        return BusyOutcome::Handled;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return BusyOutcome::Quit;
    }
    if key.code == KeyCode::Esc {
        return BusyOutcome::Cancel;
    }

    match key.code {
        KeyCode::Enter => app.submit_prompt(),
        KeyCode::Tab => {
            if app.focus == crate::ui::app::Focus::Prompt {
                app.apply_completion();
            }
        }
        KeyCode::BackTab => {
            if app.focus == crate::ui::app::Focus::Prompt && app.completions.is_open() {
                app.completions.move_up();
            }
        }
        // Editing keys target the composer even if the user was scrolling.
        KeyCode::Backspace => {
            app.focus = crate::ui::app::Focus::Prompt;
            app.on_backspace();
        }
        KeyCode::Delete => {
            app.focus = crate::ui::app::Focus::Prompt;
            app.on_delete();
        }
        KeyCode::Left => {
            app.focus = crate::ui::app::Focus::Prompt;
            app.on_left();
        }
        KeyCode::Right => {
            app.focus = crate::ui::app::Focus::Prompt;
            app.on_right();
        }
        KeyCode::Home => {
            app.focus = crate::ui::app::Focus::Prompt;
            app.on_home();
        }
        KeyCode::End => {
            app.focus = crate::ui::app::Focus::Prompt;
            app.on_end();
        }
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
        KeyCode::PageUp => app.scrollback.scroll_up(10),
        KeyCode::PageDown => app.scrollback.scroll_down(10),
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.focus = crate::ui::app::Focus::Prompt;
            app.on_ctrl_edit(c);
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            app.focus = crate::ui::app::Focus::Prompt;
            app.on_char(c);
        }
        _ => {}
    }
    // Keep the slash menu current so Tab completion works mid-turn too.
    app.sync_completions();
    BusyOutcome::Handled
}

/// Queue of work the busy loop observed while the DM was talking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BusyDrain {
    None,
    Cancel,
    Quit,
}

/// Drain all pending terminal events while a turn/speech future is polled.
/// Typed keys and pastes update the composer live; only Esc/Ctrl+C change the
/// caller's control flow.
fn drain_busy_events(
    app: &mut FullscreenApp,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<BusyDrain> {
    let mut drain = BusyDrain::None;
    let mut changed = false;
    while event::poll(Duration::from_millis(0)).context("poll during DM turn")? {
        match event::read().context("read during DM turn")? {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match handle_busy_key(app, key) {
                    BusyOutcome::Handled => {}
                    BusyOutcome::Cancel => {
                        if drain != BusyDrain::Quit {
                            drain = BusyDrain::Cancel;
                        }
                    }
                    BusyOutcome::Quit => drain = BusyDrain::Quit,
                }
                changed = true;
            }
            Event::Paste(text) => {
                app.on_paste(&text);
                app.sync_completions();
                changed = true;
            }
            Event::Resize(_, _) => changed = true,
            _ => {}
        }
    }
    if changed {
        redraw(terminal, app)?;
    }
    Ok(drain)
}

/// On cancel, keep a draft typed during the turn; otherwise restore the
/// cancelled action so it can be edited and resent.
fn restore_cancelled_draft(app: &mut FullscreenApp, cancelled: &str) {
    if app.prompt.input().trim().is_empty() {
        app.prompt.set(cancelled);
    }
}

/// `/again` repeats the last dispatched DM action, never the prompt-history
/// line (which would otherwise be the literal `/again`).
fn repeat_target(app: &FullscreenApp) -> Option<String> {
    app.last_action.clone()
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
    history_turns: u32,
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
        if let Some(block) = ensure_alive(state) {
            app.push_block(block);
            return Ok(());
        }
        if let Some(prev) = repeat_target(app) {
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
            playback.stop();
            app.should_quit = true;
            return Ok(());
        }
        (MetaResult::Handled, blocks) => {
            app.push_blocks(blocks);
            app.tools_expanded = flags.tools_expanded;
            app.status_line = status::StatusLine::from_state(state);
            return Ok(());
        }
        (MetaResult::Loaded, blocks) => {
            playback.stop();
            app.push_blocks(blocks);
            // New campaign: no stale DM conversation, `/copy` text, or
            // `/again` action may survive the state swap.
            *history = ChatHistory::new(history_turns);
            seed_history(history, state);
            app.reset_session_after_load(state.last_narration.as_deref());
            app.tools_expanded = flags.tools_expanded;
            app.status_line = status::StatusLine::from_state(state);
            return Ok(());
        }
        (MetaResult::Usage, _) => {
            // Local only: the session accumulator lives on the app, never in
            // a save, and `/usage` never reaches the DM.
            app.push_block(session_usage_block(&app.usage));
            return Ok(());
        }
        (MetaResult::Talk { seconds }, blocks) => {
            app.push_blocks(blocks);
            if let Some(block) = ensure_alive(state) {
                app.push_block(block);
                app.status_line = status::StatusLine::from_state(state);
                return Ok(());
            }
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
                    // Keep the composer editable while the DM is thinking.
                    match drain_busy_events(app, terminal)? {
                        BusyDrain::Cancel => {
                            cancel.cancel();
                            playback.stop();
                            app.activity = Some("Cancelling…".into());
                            redraw(terminal, app)?;
                        }
                        BusyDrain::Quit => {
                            // Still allow hard quit during a long turn.
                            app.should_quit = true;
                            cancel.cancel();
                            playback.stop();
                        }
                        BusyDrain::None => {}
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
            // Keep a draft typed during the turn; otherwise restore the
            // cancelled action so the player can edit / resend. A line queued
            // with Enter is dropped: Esc means stop, not "run the next one".
            app.pending_action = None;
            restore_cancelled_draft(app, input);
            app.status_line = status::StatusLine::from_state(state);
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
    app.usage.record_turn(&result.usage);
    app.last_action = Some(input.to_string());
    if delta::took_damage(&before, state) {
        audio::maybe_play_damage_sfx();
    }
    app.push_blocks(turn_blocks_with_deltas(&result, &before, state));
    app.status_line = status::StatusLine::from_state(state);

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
                    // Typing stays live while narration plays; Esc stops speech.
                    match drain_busy_events(app, terminal)? {
                        BusyDrain::Cancel => {
                            playback.stop();
                            app.activity = Some("Speech cancelled…".into());
                            redraw(terminal, app)?;
                        }
                        BusyDrain::Quit => {
                            playback.stop();
                            app.should_quit = true;
                        }
                        BusyDrain::None => {}
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
    fn again_repeats_dm_action_not_the_again_line() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.last_action = Some("attack the goblin".into());
        app.history.push("/again");
        assert_eq!(
            repeat_target(&app).as_deref(),
            Some("attack the goblin"),
            "the literal /again line must never be replayed"
        );
    }

    #[test]
    fn again_without_a_dm_action_has_no_target() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.history.push("/help");
        app.history.push("look around");
        assert_eq!(repeat_target(&app), None);
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

    fn idle(app: &mut FullscreenApp, code: KeyCode, modifiers: KeyModifiers) {
        handle_idle_key(app, key(code, modifiers));
    }

    #[test]
    fn busy_keys_buffer_into_the_composer() {
        let mut app = FullscreenApp::new("groknight", false, true);
        for c in "look around".chars() {
            assert_eq!(
                handle_busy_key(&mut app, key(KeyCode::Char(c), KeyModifiers::empty())),
                BusyOutcome::Handled
            );
        }
        assert_eq!(app.prompt.input(), "look around");
        assert_eq!(app.prompt.cursor(), 11);

        // Editing keys work too, including readline word delete.
        handle_busy_key(&mut app, key(KeyCode::Backspace, KeyModifiers::empty()));
        assert_eq!(app.prompt.input(), "look aroun");
        handle_busy_key(&mut app, key(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.prompt.input(), "look ");
    }

    #[test]
    fn busy_enter_queues_for_the_outer_loop() {
        let mut app = FullscreenApp::new("groknight", false, true);
        for c in "open the door".chars() {
            handle_busy_key(&mut app, key(KeyCode::Char(c), KeyModifiers::empty()));
        }
        assert_eq!(
            handle_busy_key(&mut app, key(KeyCode::Enter, KeyModifiers::empty())),
            BusyOutcome::Handled
        );
        assert!(app.prompt.is_empty());
        assert_eq!(app.take_pending().as_deref(), Some("open the door"));
        assert!(app.take_pending().is_none(), "exactly one queued action");
        assert_eq!(app.history.last(), Some("open the door"));
    }

    #[test]
    fn busy_esc_cancels_without_clearing_the_draft() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.prompt.insert_str("typed during turn");
        assert_eq!(
            handle_busy_key(&mut app, key(KeyCode::Esc, KeyModifiers::empty())),
            BusyOutcome::Cancel
        );
        assert_eq!(app.prompt.input(), "typed during turn");
        assert!(!app.should_quit);
    }

    #[test]
    fn busy_ctrl_c_quits() {
        let mut app = FullscreenApp::new("groknight", false, true);
        assert_eq!(
            handle_busy_key(&mut app, key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            BusyOutcome::Quit
        );
        assert!(app.should_quit);
    }

    #[test]
    fn cancel_keeps_new_draft_or_restores_cancelled_action() {
        let mut app = FullscreenApp::new("groknight", false, true);
        restore_cancelled_draft(&mut app, "cancelled action");
        assert_eq!(app.prompt.input(), "cancelled action");

        // Whitespace-only drafts are effectively empty.
        app.prompt.set("   ");
        restore_cancelled_draft(&mut app, "cancelled action");
        assert_eq!(app.prompt.input(), "cancelled action");

        app.prompt.set("typed during turn");
        restore_cancelled_draft(&mut app, "cancelled action");
        assert_eq!(
            app.prompt.input(),
            "typed during turn",
            "cancel must not overwrite a newer draft"
        );
    }

    #[test]
    fn ctrl_a_moves_home() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.prompt.set("look around");
        idle(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(app.prompt.cursor(), 0);
        assert_eq!(app.prompt.input(), "look around");
    }

    #[test]
    fn ctrl_e_moves_end() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.prompt.set("look around");
        app.prompt.move_home();
        idle(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(app.prompt.cursor(), 11);
    }

    #[test]
    fn ctrl_u_deletes_to_start() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.prompt.set("look around");
        app.prompt.move_home();
        app.prompt.move_right();
        app.prompt.move_right();
        idle(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(app.prompt.input(), "ok around");
        assert_eq!(app.prompt.cursor(), 0);
    }

    #[test]
    fn ctrl_k_deletes_to_end() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.prompt.set("look around");
        app.prompt.move_home();
        app.prompt.move_right();
        app.prompt.move_right();
        idle(&mut app, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(app.prompt.input(), "lo");
        assert_eq!(app.prompt.cursor(), 2);
    }

    #[test]
    fn ctrl_w_deletes_word_back() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.prompt.set("attack the goblin");
        idle(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(app.prompt.input(), "attack the ");
    }

    #[test]
    fn ctrl_d_deletes_at_cursor_and_does_not_quit_when_empty() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.prompt.set("look");
        app.prompt.move_home();
        idle(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(app.prompt.input(), "ook");
        assert!(!app.should_quit);

        app.prompt.clear();
        idle(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(!app.should_quit, "Ctrl+C is the explicit quit");
    }

    #[test]
    fn ctrl_l_clears_scrollback() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.push_block(Block::Dm {
            text: "A quiet room.".into(),
        });
        idle(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert!(app.scrollback.is_empty());
    }

    #[test]
    fn plain_characters_are_not_treated_as_control() {
        let mut app = FullscreenApp::new("groknight", false, true);
        idle(&mut app, KeyCode::Char('a'), KeyModifiers::empty());
        idle(&mut app, KeyCode::Char('e'), KeyModifiers::empty());
        idle(&mut app, KeyCode::Char('w'), KeyModifiers::empty());
        assert_eq!(app.prompt.input(), "aew");
    }

    #[test]
    fn tab_completes_a_slash_command_before_toggling_focus() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.prompt.insert_str("/sa");
        app.sync_completions();
        idle(&mut app, KeyCode::Tab, KeyModifiers::empty());
        assert_eq!(app.prompt.input(), "/save ");
        assert_eq!(app.focus, crate::ui::app::Focus::Prompt);
    }

    #[test]
    fn tab_toggles_focus_only_when_the_prompt_is_empty() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.prompt.insert_str("draft");
        idle(&mut app, KeyCode::Tab, KeyModifiers::empty());
        assert_eq!(
            app.focus,
            crate::ui::app::Focus::Prompt,
            "Tab must not strand a draft in scrollback focus"
        );
        assert_eq!(app.prompt.input(), "draft");

        app.prompt.clear();
        idle(&mut app, KeyCode::Tab, KeyModifiers::empty());
        assert_eq!(app.focus, crate::ui::app::Focus::Scrollback);
        idle(&mut app, KeyCode::Tab, KeyModifiers::empty());
        assert_eq!(app.focus, crate::ui::app::Focus::Prompt);
    }

    #[test]
    fn esc_returns_from_scrollback_before_clearing() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.prompt.insert_str("draft");
        app.toggle_focus();
        idle(&mut app, KeyCode::Esc, KeyModifiers::empty());
        assert_eq!(app.focus, crate::ui::app::Focus::Prompt);
        assert_eq!(app.prompt.input(), "draft");
        idle(&mut app, KeyCode::Esc, KeyModifiers::empty());
        assert!(app.prompt.is_empty());
        assert!(!app.should_quit);
    }

    #[test]
    fn ptt_release_marks_transcribing_and_ungates_input() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.ptt_held = true;
        app.activity = Some("Listening".into());
        mark_ptt_transcribing(&mut app);
        assert!(!app.ptt_held);
        assert_eq!(app.activity.as_deref(), Some("Transcribing…"));
        // Keys are accepted even though the transcription task is still
        // finishing (ptt itself stays Some in the caller until it resolves).
        handle_idle_key(&mut app, key(KeyCode::Char('a'), KeyModifiers::empty()));
        assert_eq!(app.prompt.input(), "a");
    }
}
