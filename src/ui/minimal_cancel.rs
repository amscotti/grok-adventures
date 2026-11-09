//! Opt-in Esc watcher for minimal-mode DM waits.
//!
//! Minimal mode (the default) has no live composer, so while it awaits the DM
//! the keyboard is normally untouched. [`EscWatcher`] opens a short raw-mode
//! window for exactly one DM turn:
//!
//! - `Esc` cancels the turn ([`CancellationToken`]) and stops TTS playback.
//! - `Ctrl+C` cancels the turn and sets a shared quit flag so the caller can
//!   exit cleanly (the outer loop's autosave then runs).
//! - Every other key is ignored: minimal mode has no live composer, queue, or
//!   draft to feed during the wait.
//!
//! The watcher is strictly opt-in. [`EscWatcher::start`] returns `None` unless
//! stdin is a terminal, so pipes, scripted runs, and CI keep the existing
//! cooked `read_line` behavior and never enter raw mode. It also returns `None`
//! if raw mode cannot be enabled, letting the turn run as before.
//!
//! # Terminal restoration
//!
//! [`EscWatcher::start`] enables raw mode on the calling thread, then moves a
//! [`RawModeGuard`] into the watcher thread. The guard's `Drop` calls
//! `disable_raw_mode`, so cooked mode is restored when the watcher thread exits
//! for any reason: normal stop, poll error, or panic. [`EscWatcher::stop`]
//! signals the stop flag and joins that thread, so by the time it returns the
//! guard has run and the terminal is cooked. [`EscWatcher`]'s own `Drop` calls
//! `stop`, which covers early returns, `?`, and unwinding at the call site.
//! If the thread cannot be spawned, the spawn closure (and the guard inside
//! it) is dropped, restoring cooked mode before `start` returns `None`.

use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use tokio_util::sync::CancellationToken;

use crate::io::audio::ActivePlayback;

/// How long the watcher blocks for input before re-checking its stop flag.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// What one key means while the minimal watcher owns the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchAction {
    /// `Esc`: cancel the in-flight DM turn.
    Cancel,
    /// `Ctrl+C`: cancel the turn, then quit once state is restored.
    Quit,
    /// Any other key: ignored (minimal has no live composer).
    Ignore,
}

/// Classify a key event for minimal-mode cancellation.
///
/// Pure: no terminal state is touched. Platforms that report key releases are
/// ignored, `Esc` cancels (with or without modifiers), and quit is exactly
/// `Ctrl+C` — `Alt+C`, plain `c`, and other control keys are ignored.
pub fn classify_key(key: KeyEvent) -> WatchAction {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return WatchAction::Ignore;
    }
    match key.code {
        KeyCode::Esc => WatchAction::Cancel,
        KeyCode::Char('c' | 'C') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            WatchAction::Quit
        }
        _ => WatchAction::Ignore,
    }
}

/// Restores cooked terminal mode when dropped.
///
/// Constructed only after [`enable_raw_mode`] succeeds, and moved into the
/// watcher thread so unwinding there cannot strand the terminal in raw mode.
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Option<Self> {
        enable_raw_mode().ok().map(|()| Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// A single-turn keyboard watcher for minimal mode.
///
/// See the module docs for the lifecycle and terminal-restoration guarantee.
pub struct EscWatcher {
    cancel: CancellationToken,
    quit: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl EscWatcher {
    /// Start watching stdin for the duration of one DM turn.
    ///
    /// Returns `None` when stdin is not a terminal (pipes/CI) or when raw mode
    /// cannot be enabled; callers then run the turn exactly as before.
    pub fn start(playback: &ActivePlayback) -> Option<Self> {
        Self::start_if(io::stdin().is_terminal(), playback)
    }

    /// Testable core of [`start`](Self::start); never touches the terminal
    /// when `stdin_is_terminal` is false.
    fn start_if(stdin_is_terminal: bool, playback: &ActivePlayback) -> Option<Self> {
        if !stdin_is_terminal {
            return None;
        }
        let guard = RawModeGuard::enable()?;

        let cancel = CancellationToken::new();
        let quit = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));

        let spawned = {
            let cancel = cancel.clone();
            let quit = Arc::clone(&quit);
            let stop = Arc::clone(&stop);
            let playback = playback.clone();
            thread::Builder::new()
                .name("minimal-esc-watcher".into())
                .spawn(move || {
                    // Owns the raw-mode lifetime: whatever ends this closure
                    // (stop flag, poll error, panic) restores cooked mode.
                    let _guard = guard;
                    watch_loop(&stop, &cancel, &quit, &playback);
                })
        };

        match spawned {
            Ok(thread) => Some(Self {
                cancel,
                quit,
                stop,
                thread: Some(thread),
            }),
            // The closure was dropped, so its guard already restored cooked
            // mode. The turn runs without a watcher.
            Err(_) => None,
        }
    }

    /// Token to hand to `run_dm_turn_cancellable`.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Whether `Ctrl+C` was seen during the wait.
    pub fn quit_requested(&self) -> bool {
        self.quit.load(Ordering::SeqCst)
    }

    /// Stop watching, join the poll thread, and restore cooked mode.
    ///
    /// Idempotent: [`Drop`] calls this again after an explicit stop.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for EscWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Poll stdin until the owner asks us to stop or reading fails.
fn watch_loop(
    stop: &AtomicBool,
    cancel: &CancellationToken,
    quit: &AtomicBool,
    playback: &ActivePlayback,
) {
    while !stop.load(Ordering::SeqCst) {
        match event::poll(POLL_INTERVAL) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) => match classify_key(key) {
                    WatchAction::Cancel => {
                        cancel.cancel();
                        playback.stop();
                    }
                    WatchAction::Quit => {
                        quit.store(true, Ordering::SeqCst);
                        cancel.cancel();
                        playback.stop();
                    }
                    WatchAction::Ignore => {}
                },
                Ok(_) => {}
                Err(_) => break,
            },
            Ok(false) => {}
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn esc_cancels() {
        assert_eq!(
            classify_key(key(KeyCode::Esc, KeyModifiers::NONE)),
            WatchAction::Cancel
        );
        // Terminal modifiers on Esc are harmless; the key still cancels.
        assert_eq!(
            classify_key(key(KeyCode::Esc, KeyModifiers::SHIFT)),
            WatchAction::Cancel
        );
    }

    #[test]
    fn ctrl_c_quits() {
        assert_eq!(
            classify_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            WatchAction::Quit
        );
        assert_eq!(
            classify_key(key(KeyCode::Char('C'), KeyModifiers::CONTROL)),
            WatchAction::Quit
        );
    }

    #[test]
    fn plain_characters_are_ignored() {
        assert_eq!(
            classify_key(key(KeyCode::Char('c'), KeyModifiers::NONE)),
            WatchAction::Ignore
        );
        assert_eq!(
            classify_key(key(KeyCode::Char('a'), KeyModifiers::NONE)),
            WatchAction::Ignore
        );
        assert_eq!(
            classify_key(key(KeyCode::Enter, KeyModifiers::NONE)),
            WatchAction::Ignore
        );
    }

    #[test]
    fn only_control_c_quits() {
        assert_eq!(
            classify_key(key(KeyCode::Char('c'), KeyModifiers::ALT)),
            WatchAction::Ignore
        );
        assert_eq!(
            classify_key(key(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            WatchAction::Ignore
        );
        assert_eq!(
            classify_key(key(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )),
            WatchAction::Quit,
            "Ctrl+C with an extra modifier still quits"
        );
    }

    #[test]
    fn key_releases_are_ignored() {
        let release =
            KeyEvent::new_with_kind(KeyCode::Esc, KeyModifiers::NONE, KeyEventKind::Release);
        assert_eq!(classify_key(release), WatchAction::Ignore);
    }

    #[test]
    fn watcher_does_not_start_without_a_terminal() {
        let playback = ActivePlayback::new();
        // Deterministic core: non-TTY stdin must never enable raw mode or
        // spawn a thread.
        assert!(EscWatcher::start_if(false, &playback).is_none());
        // `cargo test` may inherit a developer's terminal; only call the real
        // `start` when the runner itself has no TTY (CI), so the test can
        // never put an interactive terminal into raw mode.
        if !io::stdin().is_terminal() {
            assert!(EscWatcher::start(&playback).is_none());
        }
    }
}
