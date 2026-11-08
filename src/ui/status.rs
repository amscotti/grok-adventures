//! Status chrome from authoritative [`GameState`].
//!
//! Minimal mode prints one themed line. Fullscreen mode keeps the same data as
//! structured [`StatusLine`] chunks so the renderer can color the HP segment
//! and drop lower-priority fields with a visible ellipsis when the terminal is
//! narrow.

use ratatui::text::Span;

use crate::game::state::GameState;
use crate::ui::sanitize;
use crate::ui::theme;

/// Separator between status segments.
const SEPARATOR: &str = "  ·  ";
/// Display width of [`SEPARATOR`] (`·` is one cell).
const SEPARATOR_WIDTH: usize = 5;

fn item_label(count: usize) -> String {
    match count {
        1 => "1 item".to_string(),
        n => format!("{n} items"),
    }
}

/// Sanitize a save-field value for the single-line status bar. Escape
/// sequences are stripped and line breaks/tabs are flattened so a hostile
/// name cannot push the status chrome onto another line.
fn status_field(text: &str) -> String {
    sanitize::sanitize_terminal_line(text)
}

/// Which piece of status a chunk represents. Order is priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Hp,
    Room,
    Items,
    Companions,
}

/// One themed segment of the status bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusChunk {
    pub kind: StatusKind,
    pub text: String,
    /// `(current, max)` when this is the HP segment.
    pub hp: Option<(i32, i32)>,
}

/// Structured status bar contents, in priority order:
/// HP → room → items → companions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusLine {
    chunks: Vec<StatusChunk>,
}

impl StatusLine {
    /// Build structured status from game state.
    pub fn from_state(state: &GameState) -> Self {
        let mut chunks = vec![StatusChunk {
            kind: StatusKind::Hp,
            text: format!("HP {}/{}", state.hp, state.max_hp),
            hp: Some((state.hp, state.max_hp)),
        }];
        chunks.push(StatusChunk {
            kind: StatusKind::Room,
            text: status_field(&state.current_room),
            hp: None,
        });
        chunks.push(StatusChunk {
            kind: StatusKind::Items,
            text: item_label(state.inventory.len()),
            hp: None,
        });
        if !state.companions.is_empty() {
            let names: Vec<String> = state
                .companions
                .iter()
                .map(|c| status_field(&c.name))
                .collect();
            chunks.push(StatusChunk {
                kind: StatusKind::Companions,
                text: names.join(", "),
                hp: None,
            });
        }
        Self { chunks }
    }

    /// Single unchunked status (tests / callers that only need text).
    pub fn plain(text: impl Into<String>) -> Self {
        let text = text.into();
        let text = sanitize::sanitize_terminal_line(&text);
        Self {
            chunks: if text.is_empty() {
                Vec::new()
            } else {
                vec![StatusChunk {
                    kind: StatusKind::Room,
                    text,
                    hp: None,
                }]
            },
        }
    }

    pub fn chunks(&self) -> &[StatusChunk] {
        &self.chunks
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Fit the status into `width` display cells.
    ///
    /// Chunks are kept in priority order (HP first). When everything does not
    /// fit, the next chunk is truncated by display width and a trailing `…`
    /// is appended, so narrow terminals never silently drop companions. Wide
    /// glyphs are never split.
    pub fn fit(&self, width: usize) -> Vec<StatusChunk> {
        if width == 0 || self.chunks.is_empty() {
            return Vec::new();
        }

        let full_width = self.full_width();
        if full_width <= width {
            return self.chunks.clone();
        }

        // Reserve one cell for the trailing ellipsis.
        let budget = width.saturating_sub(1);
        let mut out: Vec<StatusChunk> = Vec::new();
        let mut used = 0usize;

        for chunk in &self.chunks {
            let sep = if out.is_empty() { 0 } else { SEPARATOR_WIDTH };
            let chunk_width = display_width(&chunk.text);
            if used + sep + chunk_width <= budget {
                out.push(chunk.clone());
                used += sep + chunk_width;
                continue;
            }

            let remaining = budget.saturating_sub(used + sep);
            if remaining > 0 {
                let text = take_display(&chunk.text, remaining);
                if !text.is_empty() {
                    out.push(StatusChunk {
                        text: text.to_string(),
                        ..chunk.clone()
                    });
                }
            }
            break;
        }

        // Attach the ellipsis directly to the last chunk so no separator cell
        // is spent on it. If no chunk fit at all, show just the ellipsis.
        if let Some(last) = out.last_mut() {
            last.text.push('…');
        } else {
            out.push(StatusChunk {
                kind: StatusKind::Hp,
                text: "…".to_string(),
                hp: self.chunks.first().and_then(|c| c.hp),
            });
        }
        out
    }

    fn full_width(&self) -> usize {
        if self.chunks.is_empty() {
            return 0;
        }
        self.chunks
            .iter()
            .map(|c| display_width(&c.text))
            .sum::<usize>()
            + SEPARATOR_WIDTH * (self.chunks.len() - 1)
    }
}

impl std::fmt::Display for StatusLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = self
            .chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(SEPARATOR);
        f.write_str(&text)
    }
}

impl From<&str> for StatusLine {
    fn from(text: &str) -> Self {
        Self::plain(text)
    }
}

impl From<String> for StatusLine {
    fn from(text: String) -> Self {
        Self::plain(text)
    }
}

/// Display width of `text` in terminal cells (Unicode-width aware).
///
/// Shared by the status bar, scrollback wrapping, and the composer cursor so
/// all three agree on CJK/emoji widths without adding a dependency.
pub(crate) fn display_width(text: &str) -> usize {
    Span::raw(text).width()
}

/// Longest prefix of `text` whose display width is at most `max` cells.
pub(crate) fn take_display(text: &str, max: usize) -> &str {
    let mut width = 0;
    for (idx, ch) in text.char_indices() {
        let end = idx + ch.len_utf8();
        let char_width = display_width(&text[idx..end]);
        if width + char_width > max {
            return &text[..idx];
        }
        width += char_width;
    }
    text
}

fn status_parts(state: &GameState) -> Vec<String> {
    StatusLine::from_state(state)
        .chunks()
        .iter()
        .map(|c| c.text.clone())
        .collect()
}

/// Build the compact status line (plain text, no ANSI):
/// `HP 18/20  ·  Room  ·  3 items` (companions appended when present).
pub fn format_status_plain(state: &GameState) -> String {
    status_parts(state).join(SEPARATOR)
}

/// Build the compact status line with theme colors for minimal mode.
pub fn format_status(state: &GameState) -> String {
    let parts = status_parts(state);
    let mut styled = Vec::with_capacity(parts.len());
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            styled.push(theme::hp(part, state.hp, state.max_hp));
        } else {
            styled.push(theme::dim(part));
        }
    }
    styled.join(&theme::dim(SEPARATOR))
}

/// Print the status line to stdout.
pub fn print_status(state: &GameState) {
    println!("{}", format_status(state));
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn status_includes_hp_room_items() {
        let mut state = GameState::new("Hero");
        state.hp = 18;
        state.current_room = "Crypt".into();
        state.add_item("Torch", "Lit", 1);
        state.add_item("Key", "Brass", 1);
        state.add_item("Coin", "Gold", 1);
        let s = format_status(&state);
        assert!(s.contains("18/20"));
        assert!(s.contains("Crypt"));
        assert!(s.contains("3 item"));
    }

    #[test]
    fn status_includes_companions_when_present() {
        let mut state = GameState::new("Hero");
        state.add_companion("Pip", "fairy");
        let s = format_status(&state);
        assert!(s.contains("Pip"));
    }

    #[test]
    fn empty_companions_omitted() {
        let state = GameState::new("Hero");
        let s = format_status(&state);
        assert!(!s.contains("👥"));
        assert_eq!(state.companions.len(), 0);
    }

    #[test]
    fn plain_status_has_no_ansi() {
        let mut state = GameState::new("Hero");
        state.hp = 18;
        state.current_room = "Crypt".into();
        let s = format_status_plain(&state);
        assert!(s.contains("18/20"));
        assert!(s.contains("Crypt"));
        assert!(!s.contains('\x1b'));
    }

    #[test]
    fn structured_status_is_in_priority_order() {
        let mut state = GameState::new("Hero");
        state.current_room = "Hall".into();
        state.add_companion("Pip", "fairy");
        let line = StatusLine::from_state(&state);
        let kinds: Vec<StatusKind> = line.chunks().iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            vec![
                StatusKind::Hp,
                StatusKind::Room,
                StatusKind::Items,
                StatusKind::Companions
            ]
        );
        assert_eq!(line.to_string(), format_status_plain(&state));
    }

    #[test]
    fn fit_keeps_hp_and_marks_truncation_with_ellipsis() {
        let mut state = GameState::new("Hero");
        state.hp = 2;
        state.current_room = "The Long Gallery Beneath the Crumbling West Tower".into();
        state.add_item("Torch", "Lit", 1);
        state.add_companion("Pip the Unreasonably Chatty Fairy", "fairy");
        let line = StatusLine::from_state(&state);

        for width in [80, 40] {
            let fitted = line.fit(width);
            let text = fitted
                .iter()
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join(SEPARATOR);
            assert!(
                text.contains('…'),
                "width {width} should mark truncation: {text:?}"
            );
            assert!(text.starts_with("HP 2/20"), "HP first: {text:?}");
            assert!(
                display_width(&text) <= width,
                "width {width} overflowed: {text:?} ({} cells)",
                display_width(&text)
            );
        }
    }

    #[test]
    fn fit_never_splits_wide_glyphs() {
        let mut state = GameState::new("Hero");
        state.current_room = "龍".repeat(30);
        let line = StatusLine::from_state(&state);
        let fitted = line.fit(20);
        let text = fitted
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(SEPARATOR);
        assert!(display_width(&text) <= 20, "{text:?}");
        assert!(text.contains('…'));
    }

    #[test]
    fn hostile_room_and_companion_names_are_neutralized() {
        let mut state = GameState::new("Hero");
        state.current_room = "\x1b]52;c;cGF5bG9hZA==\x07\x1b[2J\u{202e}\u{2067}Room".into();
        state.add_companion("Pip\x1b]0;owned\x07\u{202c}\nSpoof", "fairy");

        let plain = format_status_plain(&state);
        assert!(plain.contains("Room"));
        assert!(plain.contains("Pip"));
        assert!(!plain.contains('\x1b'));
        assert!(!plain.contains('\x07'));
        assert!(!plain.contains('\u{202e}'));
        assert!(!plain.contains('\u{2067}'));
        assert!(
            !plain.contains('\n'),
            "status stayed on one line: {plain:?}"
        );

        // Styled output adds legitimate SGR, but never OSC/BEL/bidi.
        let styled = format_status(&state);
        assert!(styled.contains("Room"));
        assert!(!styled.contains("\x1b]"));
        assert!(!styled.contains('\x07'));
        assert!(!styled.contains('\u{202e}'));
        assert!(!styled.contains('\u{2067}'));
    }

    #[test]
    fn fit_plain_hostile_status_is_sanitized() {
        let line = StatusLine::plain("HP 20/20 \x1b]52;c;cGF5bG9hZA==\x07\u{202e}\u{2067}");
        let fitted = line.fit(80);
        let text = fitted.iter().map(|c| c.text.as_str()).collect::<String>();
        assert!(text.contains("HP 20/20"));
        assert!(!text.contains('\x1b'));
        assert!(!text.contains('\x07'));
        assert!(!text.contains('\u{202e}'));
        assert!(!text.contains('\u{2067}'));
    }
}
