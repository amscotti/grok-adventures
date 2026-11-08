//! Input buffer for the fullscreen prompt (cursor-aware line editing).

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

    /// Insert a character at the cursor (ignores control chars).
    pub fn insert_char(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        let idx = self.byte_index_at(self.cursor);
        self.input.insert(idx, c);
        self.cursor += 1;
    }

    /// Insert a string of printable text at the cursor.
    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            self.insert_char(c);
        }
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

    /// Display width of text before the cursor (ASCII = 1 per char).
    /// Used for terminal cursor placement; non-ASCII counts as one cell each.
    pub fn cursor_display_cols(&self) -> u16 {
        self.cursor.min(u16::MAX as usize) as u16
    }
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
