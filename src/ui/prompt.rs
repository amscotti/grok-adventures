//! Input buffer for the fullscreen prompt (cursor-aware line editing).

use crate::ui::sanitize;
use crate::ui::status;

/// Editable prompt line with a character-index cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prompt {
    input: String,
    /// Cursor position in characters (0..=char_len).
    cursor: usize,
}

impl Prompt {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.input.is_empty()
    }

    /// Number of Unicode scalar values in the buffer.
    pub fn char_len(&self) -> usize {
        self.input.chars().count()
    }

    /// Byte offset of the current cursor (char index).
    fn byte_index_at(&self, char_idx: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }

    /// Insert a character at the cursor (ignores control and invisible
    /// formatting characters).
    pub fn insert_char(&mut self, c: char) {
        if c.is_control() || sanitize::is_disallowed_terminal_char(c) {
            return;
        }
        let idx = self.byte_index_at(self.cursor);
        self.input.insert(idx, c);
        self.cursor += 1;
    }

    /// Insert pasted text at the cursor.
    ///
    /// The paste is sanitized as a whole first so escape sequences cannot
    /// smuggle their payload in as plain text. This is a one-line prompt, so
    /// newlines/tabs (which the sanitizer otherwise preserves) are dropped,
    /// matching the previous behaviour of [`Self::insert_char`].
    pub fn insert_str(&mut self, s: &str) {
        for c in sanitize::sanitize_terminal_text(s).chars() {
            if !c.is_control() {
                self.insert_char(c);
            }
        }
    }

    /// Insert clipboard text as one editable line.
    ///
    /// Clipboard line breaks and tabs are flattened to single spaces before
    /// [`Self::insert_str`] sanitizes the result; pasting never submits.
    pub fn insert_paste(&mut self, s: &str) {
        self.insert_str(&normalize_paste(s));
    }

    /// Delete the character before the cursor, if any.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let char_idx = self.cursor - 1;
        let start = self.byte_index_at(char_idx);
        let end = self.byte_index_at(self.cursor);
        self.input.replace_range(start..end, "");
        self.cursor = char_idx;
    }

    /// Delete the character under the cursor, if any.
    pub fn delete(&mut self) {
        if self.cursor >= self.char_len() {
            return;
        }
        let start = self.byte_index_at(self.cursor);
        let end = self.byte_index_at(self.cursor + 1);
        self.input.replace_range(start..end, "");
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.char_len() {
            self.cursor += 1;
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.char_len();
    }

    /// Clear the buffer without returning it.
    pub fn clear(&mut self) {
        self.input.clear();
        self.cursor = 0;
    }

    /// Take the current input, leaving the buffer empty.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.input)
    }

    /// Replace the entire buffer (e.g. after paste / restore / history).
    /// Cursor moves to the end.
    pub fn set(&mut self, s: impl Into<String>) {
        self.input = s.into();
        self.cursor = self.char_len();
    }

    /// Replace the buffer and place the cursor (clamped), e.g. after a
    /// completion replaces the first word.
    pub fn set_with_cursor(&mut self, s: impl Into<String>, cursor: usize) {
        self.input = s.into();
        self.cursor = cursor.min(self.char_len());
    }

    /// Delete the word before the cursor (Ctrl+W): trailing spaces, then
    /// the run of non-space characters before them.
    pub fn delete_word_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let before: Vec<char> = self.input.chars().take(self.cursor).collect();
        let mut end = before.len();
        while end > 0 && before[end - 1] == ' ' {
            end -= 1;
        }
        while end > 0 && before[end - 1] != ' ' {
            end -= 1;
        }
        let start = self.byte_index_at(end);
        let stop = self.byte_index_at(self.cursor);
        self.input.replace_range(start..stop, "");
        self.cursor = end;
    }

    /// Delete everything before the cursor (Ctrl+U).
    pub fn delete_to_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let stop = self.byte_index_at(self.cursor);
        self.input.replace_range(..stop, "");
        self.cursor = 0;
    }

    /// Delete everything from the cursor to the end (Ctrl+K).
    pub fn delete_to_end(&mut self) {
        let start = self.byte_index_at(self.cursor);
        self.input.truncate(start);
    }

    /// Display width of text before the cursor (CJK/emoji count as their
    /// terminal cells, not one column per scalar value).
    /// Used for terminal cursor placement.
    ///
    /// Counts the sanitized prefix so control/escape characters that are
    /// stripped at render time do not shift the cursor off its glyph.
    pub fn cursor_display_cols(&self) -> u16 {
        let prefix: String = self.input.chars().take(self.cursor).collect();
        let sanitized = sanitize::sanitize_terminal_line(&prefix);
        status::display_width(&sanitized).min(u16::MAX as usize) as u16
    }

    /// Char index of the cursor inside the *sanitized* body text.
    ///
    /// `cursor()` indexes the raw buffer; sanitization can remove characters,
    /// so display code maps through the sanitized prefix instead.
    pub fn sanitized_cursor_char_index(&self) -> usize {
        let prefix: String = self.input.chars().take(self.cursor).collect();
        sanitize::sanitize_terminal_line(&prefix).chars().count()
    }
}

/// Flatten clipboard line breaks and tabs into single spaces.
///
/// `\r\n`, lone `\r`, `\n`, and `\t` each become one space so a multi-line
/// paste stays one editable prompt line. Other control/escape sequences are
/// left for [`sanitize::sanitize_terminal_text`] to remove.
pub fn normalize_paste(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                // Treat CRLF as one break, not two spaces.
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(' ');
            }
            '\n' | '\t' => out.push(' '),
            _ => out.push(ch),
        }
    }
    out
}

/// A width-bounded view of the composer body with the cursor located inside
/// it. `text` may start with `…` when the left side was clipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerWindow {
    pub text: String,
    /// Display columns from the start of [`Self::text`] to the cursor.
    pub cursor_col: usize,
    pub clipped_left: bool,
}

/// Choose a visible window of `text` that keeps the cursor on screen.
///
/// `cursor_char` is a char index into `text`; `width` is the number of cells
/// available for the body (the `› ` prefix is accounted for by the caller).
/// Long text is clipped on the left with a leading `…`; near the end of the
/// buffer the window ends at the cursor, otherwise it keeps the cursor roughly
/// centered so both sides stay editable.
pub fn composer_window(text: &str, cursor_char: usize, width: usize) -> ComposerWindow {
    let chars: Vec<char> = text.chars().collect();
    let cursor_char = cursor_char.min(chars.len());
    let cursor_display = char_slice_width(&chars[..cursor_char]);
    let total = char_slice_width(&chars);

    if width == 0 {
        return ComposerWindow {
            text: String::new(),
            cursor_col: 0,
            clipped_left: total > 0,
        };
    }
    if total <= width {
        return ComposerWindow {
            text: text.to_string(),
            cursor_col: cursor_display,
            clipped_left: false,
        };
    }

    // Keep the cursor near the middle, but slide the window to end at the
    // cursor when there is little or no text after it.
    let half = width / 2;
    let after = total.saturating_sub(cursor_display);
    let start_target = if after < half {
        cursor_display.saturating_sub(width.saturating_sub(1))
    } else {
        cursor_display.saturating_sub(half)
    };

    let clipped_left = start_target > 0;
    let ellipsis = usize::from(clipped_left);
    let budget = width.saturating_sub(ellipsis);

    // First char at/after the requested start column.
    let mut start_idx = chars.len();
    let mut acc = 0usize;
    for (i, &ch) in chars.iter().enumerate() {
        if acc >= start_target {
            start_idx = i;
            break;
        }
        acc += status::display_width(ch.encode_utf8(&mut [0u8; 4]));
    }

    let start_display = char_slice_width(&chars[..start_idx]);
    let mut visible = String::new();
    let mut visible_width = 0usize;
    for &ch in &chars[start_idx..] {
        let w = status::display_width(ch.encode_utf8(&mut [0u8; 4]));
        if visible_width + w > budget {
            break;
        }
        visible.push(ch);
        visible_width += w;
    }

    let mut out = String::new();
    if clipped_left {
        out.push('…');
    }
    out.push_str(&visible);

    ComposerWindow {
        text: out,
        cursor_col: ellipsis + cursor_display.saturating_sub(start_display),
        clipped_left,
    }
}

fn char_slice_width(chars: &[char]) -> usize {
    chars
        .iter()
        .map(|&c| status::display_width(c.encode_utf8(&mut [0u8; 4])))
        .sum()
}

/// Ring of previously submitted prompt lines (oldest first).
///
/// Browsing: `None` means live draft; `Some(i)` views `entries[i]`.
/// Up walks toward older entries; Down toward newer, then restores the draft.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptHistory {
    entries: Vec<String>,
    browse_index: Option<usize>,
    /// Saved live draft when browsing starts.
    draft: String,
}

/// Maximum stored history entries.
pub const HISTORY_CAP: usize = 50;

/// Push a submitted line onto a plain `Vec<String>` history with the same
/// semantics as [`PromptHistory::push`]: trims, ignores empties, dedups
/// consecutive repeats, caps at [`HISTORY_CAP`]. Shared by minimal-mode
/// `/again` history so the two UIs cannot drift.
pub fn push_history_entry(history: &mut Vec<String>, line: &str) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    if history.last().map(String::as_str) != Some(trimmed) {
        history.push(trimmed.to_string());
        if history.len() > HISTORY_CAP {
            let excess = history.len() - HISTORY_CAP;
            history.drain(0..excess);
        }
    }
}

impl PromptHistory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn browsing(&self) -> bool {
        self.browse_index.is_some()
    }

    /// Push a submitted line (dedup consecutive identical). Cap at [`HISTORY_CAP`].
    /// Resets browse state.
    pub fn push(&mut self, line: impl Into<String>) {
        let line = line.into();
        push_history_entry(&mut self.entries, &line);
        self.browse_index = None;
        self.draft.clear();
    }

    /// Most recent submitted line, if any.
    pub fn last(&self) -> Option<&str> {
        self.entries.last().map(String::as_str)
    }

    /// Move up in history (toward older). Saves `current` as draft on first step.
    /// Returns the line to place in the prompt, if any change.
    pub fn up(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        match self.browse_index {
            None => {
                self.draft = current.to_string();
                let idx = self.entries.len() - 1;
                self.browse_index = Some(idx);
                Some(self.entries[idx].clone())
            }
            Some(0) => Some(self.entries[0].clone()),
            Some(i) => {
                let idx = i - 1;
                self.browse_index = Some(idx);
                Some(self.entries[idx].clone())
            }
        }
    }

    /// Move down in history (toward newer / live draft).
    /// Returns the line to place in the prompt, if any change.
    pub fn down(&mut self) -> Option<String> {
        match self.browse_index {
            None => None,
            Some(i) if i + 1 >= self.entries.len() => {
                self.browse_index = None;
                Some(std::mem::take(&mut self.draft))
            }
            Some(i) => {
                let idx = i + 1;
                self.browse_index = Some(idx);
                Some(self.entries[idx].clone())
            }
        }
    }

    /// Exit browse mode without changing entries (e.g. user typed a char).
    pub fn abandon_browse(&mut self) {
        self.browse_index = None;
        self.draft.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_backspace() {
        let mut p = Prompt::new();
        p.insert_char('h');
        p.insert_char('i');
        assert_eq!(p.input(), "hi");
        assert_eq!(p.cursor(), 2);
        p.backspace();
        assert_eq!(p.input(), "h");
        assert_eq!(p.cursor(), 1);
        p.backspace();
        p.backspace(); // no-op
        assert!(p.is_empty());
        assert_eq!(p.cursor(), 0);
    }

    #[test]
    fn insert_delete_at_cursor() {
        let mut p = Prompt::new();
        p.insert_str("hello");
        p.move_home();
        p.move_right();
        p.move_right(); // cursor after "he"
        assert_eq!(p.cursor(), 2);
        p.insert_char('X');
        assert_eq!(p.input(), "heXllo");
        assert_eq!(p.cursor(), 3);
        p.delete(); // remove first 'l' under cursor
        assert_eq!(p.input(), "heXlo");
        assert_eq!(p.cursor(), 3); // still after X
        p.backspace(); // remove X (char before cursor)
        assert_eq!(p.input(), "helo");
        assert_eq!(p.cursor(), 2);
        p.move_home();
        p.delete(); // remove 'h'
        assert_eq!(p.input(), "elo");
    }

    #[test]
    fn home_end_left_right() {
        let mut p = Prompt::new();
        p.insert_str("abc");
        p.move_home();
        assert_eq!(p.cursor(), 0);
        p.move_right();
        assert_eq!(p.cursor(), 1);
        p.move_end();
        assert_eq!(p.cursor(), 3);
        p.move_left();
        assert_eq!(p.cursor(), 2);
        p.move_left();
        p.move_left();
        p.move_left(); // clamp
        assert_eq!(p.cursor(), 0);
    }

    #[test]
    fn delete_at_end_is_noop() {
        let mut p = Prompt::new();
        p.insert_str("ab");
        p.delete();
        assert_eq!(p.input(), "ab");
        p.move_home();
        p.delete();
        assert_eq!(p.input(), "b");
    }

    #[test]
    fn take_clears() {
        let mut p = Prompt::new();
        p.insert_str("go north");
        assert_eq!(p.take(), "go north");
        assert!(p.is_empty());
        assert_eq!(p.cursor(), 0);
    }

    #[test]
    fn set_moves_cursor_to_end() {
        let mut p = Prompt::new();
        p.set("look");
        assert_eq!(p.input(), "look");
        assert_eq!(p.cursor(), 4);
    }

    #[test]
    fn ignores_control_chars() {
        let mut p = Prompt::new();
        p.insert_char('\n');
        p.insert_char('\t');
        p.insert_char('a');
        assert_eq!(p.input(), "a");
    }

    #[test]
    fn insert_rejects_bidi_and_zero_width() {
        let mut p = Prompt::new();
        for c in [
            '\u{202e}', '\u{202c}', '\u{2066}', '\u{2069}', '\u{200b}', '\u{feff}',
        ] {
            p.insert_char(c);
        }
        p.insert_char('a');
        assert_eq!(p.input(), "a");
        assert_eq!(p.cursor(), 1);
    }

    #[test]
    fn paste_is_sanitized_as_a_whole() {
        let mut p = Prompt::new();
        p.insert_str("look\x1b]52;c;cGF5bG9hZA==\x07\x1b[2J\u{202e}\u{2067} around");
        assert_eq!(p.input(), "look around");
        assert_eq!(p.cursor(), "look around".chars().count());
        assert!(!p.input().contains('\u{1b}'));
        assert!(!p.input().contains('\x07'));
        assert!(!p.input().contains('\u{202e}'));
        assert!(!p.input().contains('\u{2067}'));
    }

    #[test]
    fn paste_normalizes_line_breaks_and_tabs() {
        assert_eq!(
            normalize_paste("look\r\naround\rhere\nnow\tplease"),
            "look around here now please"
        );
        let mut p = Prompt::new();
        p.insert_paste("look\r\naround\rhere\nnow\tplease");
        assert_eq!(p.input(), "look around here now please");
        assert_eq!(p.cursor(), "look around here now please".chars().count());
    }

    #[test]
    fn paste_sanitizes_controls_after_normalizing() {
        let mut p = Prompt::new();
        p.insert_paste("look\x1b]52;c;cGF5bG9hZA==\x07\x1b[2J\u{202e}\taround");
        assert_eq!(p.input(), "look around");
        assert!(!p.input().contains('\u{1b}'));
        assert!(!p.input().contains('\x07'));
        assert!(!p.input().contains('\u{202e}'));
    }

    #[test]
    fn paste_inserts_at_the_cursor() {
        let mut p = Prompt::new();
        p.insert_str("look");
        p.move_home();
        p.move_right();
        p.move_right();
        p.insert_paste("!\n");
        assert_eq!(p.input(), "lo! ok");
        assert_eq!(p.cursor(), 4);
    }

    #[test]
    fn cursor_columns_ignore_stripped_escape_text() {
        let mut p = Prompt::new();
        p.set("a\x1b[2Jb");
        assert_eq!(p.cursor_display_cols(), 2);
        assert_eq!(p.sanitized_cursor_char_index(), 2);
        p.move_home();
        p.move_right();
        assert_eq!(p.cursor_display_cols(), 1);
    }

    #[test]
    fn cursor_columns_count_display_cells_for_cjk() {
        let mut p = Prompt::new();
        p.set("龍龍");
        assert_eq!(p.cursor_display_cols(), 4);
        p.move_home();
        p.move_right();
        assert_eq!(p.cursor_display_cols(), 2);
    }

    #[test]
    fn composer_window_keeps_short_text_whole() {
        let window = composer_window("look around", 4, 40);
        assert_eq!(window.text, "look around");
        assert_eq!(window.cursor_col, 4);
        assert!(!window.clipped_left);
    }

    #[test]
    fn composer_window_shows_long_ascii_tail() {
        let text = format!("{}TAIL-MARKER", "walk north and search ".repeat(8));
        let cursor = text.chars().count();
        let window = composer_window(&text, cursor, 40);
        assert!(window.clipped_left);
        assert!(window.text.starts_with('…'));
        assert!(window.text.ends_with("TAIL-MARKER"));
        assert!(status::display_width(&window.text) <= 40);
        assert_eq!(window.cursor_col, status::display_width(&window.text));
    }

    #[test]
    fn composer_window_places_cursor_after_cjk_glyphs() {
        let text = "龍".repeat(20);
        let window = composer_window(&text, 20, 36);
        assert!(window.clipped_left);
        assert!(window.text.starts_with('…'));
        assert_eq!(window.text.chars().filter(|c| *c == '龍').count(), 17);
        assert!(status::display_width(&window.text) <= 36);
        // 20 CJK glyphs are 40 cells. The cursor must land after the visible
        // glyphs (~35), not 20 cells early from counting chars.
        assert_eq!(window.cursor_col, 35);
        assert!(window.cursor_col > 20);
    }

    #[test]
    fn composer_window_cursor_stays_visible_mid_text() {
        let text = "abcdefghijklmnopqrstuvwxyz";
        let window = composer_window(text, 20, 10);
        assert!(window.cursor_col < 10);
        let before: String = window.text.chars().take(window.cursor_col).collect();
        assert_eq!(status::display_width(&before), window.cursor_col);
    }

    #[test]
    fn unicode_cursor_insert() {
        let mut p = Prompt::new();
        p.insert_str("café");
        assert_eq!(p.char_len(), 4);
        p.move_home();
        p.move_right();
        p.insert_char('X');
        assert_eq!(p.input(), "cXafé");
    }

    #[test]
    fn ctrl_word_and_line_deletes() {
        let mut p = Prompt::new();
        p.insert_str("attack the goblin  ");
        p.delete_word_back();
        assert_eq!(p.input(), "attack the ");
        assert_eq!(p.cursor(), 11);
        p.delete_word_back();
        assert_eq!(p.input(), "attack ");
        p.move_home();
        p.delete_word_back(); // no-op at start
        assert_eq!(p.input(), "attack ");

        p.set("hello world");
        p.move_home();
        p.move_right();
        p.move_right();
        p.delete_to_end();
        assert_eq!(p.input(), "he");
        p.delete_to_start();
        assert_eq!(p.input(), "");
        assert_eq!(p.cursor(), 0);
    }

    #[test]
    fn history_up_down_navigation() {
        let mut h = PromptHistory::new();
        h.push("look");
        h.push("go north");
        h.push("attack");

        // Start from empty draft
        assert_eq!(h.up("").as_deref(), Some("attack"));
        assert_eq!(h.up("attack").as_deref(), Some("go north"));
        assert_eq!(h.up("go north").as_deref(), Some("look"));
        // Stay at oldest
        assert_eq!(h.up("look").as_deref(), Some("look"));

        assert_eq!(h.down().as_deref(), Some("go north"));
        assert_eq!(h.down().as_deref(), Some("attack"));
        // Restore draft
        assert_eq!(h.down().as_deref(), Some(""));
        assert!(!h.browsing());
        assert!(h.down().is_none());
    }

    #[test]
    fn history_preserves_draft() {
        let mut h = PromptHistory::new();
        h.push("old");
        assert_eq!(h.up("partial draft").as_deref(), Some("old"));
        assert_eq!(h.down().as_deref(), Some("partial draft"));
    }

    #[test]
    fn history_cap_and_dedup() {
        let mut h = PromptHistory::new();
        h.push("same");
        h.push("same");
        assert_eq!(h.len(), 1);
        for i in 0..60 {
            h.push(format!("cmd{i}"));
        }
        assert_eq!(h.len(), HISTORY_CAP);
        assert_eq!(h.entries().first().map(String::as_str), Some("cmd10"));
        assert_eq!(h.last(), Some("cmd59"));
    }

    #[test]
    fn history_empty_up() {
        let mut h = PromptHistory::new();
        assert!(h.up("x").is_none());
    }

    #[test]
    fn plain_history_entry_matches_prompt_history() {
        let mut v = Vec::new();
        push_history_entry(&mut v, "  go north  ");
        push_history_entry(&mut v, "go north");
        push_history_entry(&mut v, "   ");
        assert_eq!(v, vec!["go north".to_string()]);
        let mut h = PromptHistory::new();
        h.push("  go north  ");
        assert_eq!(h.entries(), v.as_slice());
    }

    #[test]
    fn history_push_resets_browse() {
        let mut h = PromptHistory::new();
        h.push("a");
        h.push("b");
        let _ = h.up("");
        assert!(h.browsing());
        h.push("c");
        assert!(!h.browsing());
        assert_eq!(h.last(), Some("c"));
    }
}
