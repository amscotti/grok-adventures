//! Slash-command autocomplete, shared by the minimal line editor and the
//! fullscreen composer.
//!
//! Pure logic only: given the prompt text + cursor, compute matching
//! commands from [`SLASH_COMMANDS`], track the highlighted row, and apply a
//! completion. Each UI renders [`CompletionMenu`] its own way.

use crate::game::commands::{SLASH_COMMANDS, SlashCommand};

/// Max suggestion rows either UI shows at once.
pub const MAX_VISIBLE: usize = 6;

/// The word being completed: text after a leading `/`, when the cursor is
/// still inside the first word. `None` outside slash-command position.
pub fn slash_word(input: &str, cursor: usize) -> Option<String> {
    let before: String = input.chars().take(cursor).collect();
    let word = before.strip_prefix('/')?;
    if word.chars().any(char::is_whitespace) {
        return None;
    }
    Some(word.to_ascii_lowercase())
}

/// Commands matching `word` by name or alias prefix. Canonical-name prefix
/// matches come first (catalog order within each group). An exact canonical
/// name that matches nothing else yields no suggestions — it is complete.
pub fn suggestions_for(word: &str) -> Vec<&'static SlashCommand> {
    let mut name_hits: Vec<&'static SlashCommand> = Vec::new();
    let mut alias_hits: Vec<&'static SlashCommand> = Vec::new();
    for cmd in SLASH_COMMANDS {
        if cmd.name.starts_with(word) {
            name_hits.push(cmd);
        } else if cmd.aliases.iter().any(|a| a.starts_with(word)) {
            alias_hits.push(cmd);
        }
    }
    let hits: Vec<&'static SlashCommand> = name_hits.into_iter().chain(alias_hits).collect();
    if hits.len() == 1 && hits[0].name == word {
        return Vec::new();
    }
    hits
}

/// Open/closed autocomplete state for one prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompletionMenu {
    items: Vec<&'static SlashCommand>,
    selected: usize,
    /// Input snapshot the items were computed for (change detection).
    for_input: String,
    /// Word the user dismissed with Esc; menu stays closed while the word
    /// still extends it, and reopens once it no longer does.
    dismissed_word: Option<String>,
}

impl CompletionMenu {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_open(&self) -> bool {
        !self.items.is_empty()
    }

    pub fn items(&self) -> &[&'static SlashCommand] {
        &self.items
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn selected_item(&self) -> Option<&'static SlashCommand> {
        self.items.get(self.selected).copied()
    }

    /// Rows to render: at most `max`, windowed so the selection is visible.
    /// Returns `(start_index, rows)`.
    pub fn visible(&self, max: usize) -> (usize, &[&'static SlashCommand]) {
        if self.items.len() <= max {
            return (0, &self.items);
        }
        let start = self
            .selected
            .saturating_add(1)
            .saturating_sub(max)
            .min(self.items.len() - max);
        (start, &self.items[start..start + max])
    }

    /// Recompute from the current prompt. Idempotent when nothing changed,
    /// so UIs may call it every frame. Selection resets when the word does.
    pub fn update(&mut self, input: &str, cursor: usize) {
        let snapshot = format!("{input}\u{1}{cursor}");
        if snapshot == self.for_input {
            return;
        }
        self.for_input = snapshot;
        let Some(word) = slash_word(input, cursor) else {
            self.items.clear();
            self.selected = 0;
            self.dismissed_word = None;
            return;
        };
        if self
            .dismissed_word
            .as_deref()
            .is_some_and(|dismissed| word.starts_with(dismissed))
        {
            self.items.clear();
            return;
        }
        self.dismissed_word = None;
        let next = suggestions_for(&word);
        if next != self.items {
            self.selected = 0;
        }
        self.items = next;
        if self.selected >= self.items.len() {
            self.selected = 0;
        }
    }

    /// Close until the word stops extending the dismissed one (Esc).
    pub fn dismiss(&mut self, input: &str, cursor: usize) {
        self.dismissed_word = slash_word(input, cursor);
        self.items.clear();
        self.selected = 0;
    }

    /// Close unconditionally (submit / clear).
    pub fn close(&mut self) {
        self.items.clear();
        self.selected = 0;
        self.for_input.clear();
        self.dismissed_word = None;
    }

    pub fn move_up(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + self.items.len() - 1) % self.items.len();
        }
    }

    pub fn move_down(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1) % self.items.len();
        }
    }

    /// Apply the highlighted command: replace the `/word` before the cursor
    /// with `/name` (plus a trailing space when the command takes args).
    /// Returns `(new_input, new_cursor)`, or `None` when closed.
    pub fn apply_selected(&self, input: &str, cursor: usize) -> Option<(String, usize)> {
        let cmd = self.selected_item()?;
        slash_word(input, cursor)?;
        // Replace the whole first word: drop any of it after the cursor,
        // keep whatever follows the next whitespace.
        let after: String = input
            .chars()
            .skip(cursor)
            .skip_while(|c| !c.is_whitespace())
            .collect();
        let mut completed = format!("/{}", cmd.name);
        if !cmd.args.is_empty() {
            completed.push(' ');
        }
        let after = if completed.ends_with(' ') {
            after.trim_start().to_string()
        } else {
            after
        };
        let new_cursor = completed.chars().count();
        Some((format!("{completed}{after}"), new_cursor))
    }
}

/// One rendered suggestion row: `› /save [slot]  Save the game`.
pub fn suggestion_line(cmd: &SlashCommand, selected: bool) -> String {
    let marker = if selected { "›" } else { " " };
    format!("{marker} {:<18} {}", cmd.usage(), cmd.description)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(items: &[&'static SlashCommand]) -> Vec<&'static str> {
        items.iter().map(|c| c.name).collect()
    }

    #[test]
    fn slash_word_only_in_first_word() {
        assert_eq!(slash_word("/st", 3).as_deref(), Some("st"));
        assert_eq!(slash_word("/", 1).as_deref(), Some(""));
        assert_eq!(slash_word("/save slot", 10), None);
        assert_eq!(slash_word("look", 4), None);
        assert_eq!(slash_word("", 0), None);
        // Cursor mid-word still completes the part before it.
        assert_eq!(slash_word("/stats", 3).as_deref(), Some("st"));
    }

    #[test]
    fn suggestions_prefix_and_alias() {
        assert!(names(&suggestions_for("st")).contains(&"stats"));
        // "stt" is an alias of voice; canonical entry is suggested.
        assert_eq!(names(&suggestions_for("stt")), vec!["voice"]);
        assert_eq!(names(&suggestions_for("inv")), vec!["inventory"]);
        // Bare "/" lists everything.
        assert_eq!(suggestions_for("").len(), SLASH_COMMANDS.len());
        assert!(suggestions_for("zzz").is_empty());
    }

    #[test]
    fn exact_command_is_already_complete() {
        assert!(suggestions_for("stats").is_empty());
        assert!(suggestions_for("quit").is_empty());
        // ...but a prefix of a longer command still completes.
        assert_eq!(names(&suggestions_for("stat")), vec!["stats"]);
        // Exact alias still offers the canonical command.
        assert_eq!(names(&suggestions_for("inv")), vec!["inventory"]);
    }

    #[test]
    fn menu_navigation_wraps_and_windows() {
        let mut menu = CompletionMenu::new();
        menu.update("/", 1);
        assert!(menu.is_open());
        assert_eq!(menu.selected(), 0);
        menu.move_up();
        assert_eq!(menu.selected(), menu.items().len() - 1);
        let (start, rows) = menu.visible(MAX_VISIBLE);
        assert_eq!(rows.len(), MAX_VISIBLE);
        assert!(start + rows.len() > menu.selected());
        menu.move_down();
        assert_eq!(menu.selected(), 0);
    }

    #[test]
    fn menu_dismiss_until_word_stops_extending() {
        let mut menu = CompletionMenu::new();
        menu.update("/s", 2);
        assert!(menu.is_open());
        menu.dismiss("/s", 2);
        assert!(!menu.is_open());
        menu.update("/s", 2);
        assert!(!menu.is_open(), "same word stays dismissed");
        // Continuing to type the same command keeps it dismissed...
        menu.update("/stats", 6);
        assert!(!menu.is_open(), "extending the word stays dismissed");
        // ...until the word no longer extends the dismissed one.
        menu.update("/", 1);
        assert!(menu.is_open(), "deleting back reopens");
        menu.update("look", 4);
        assert!(!menu.is_open());
        menu.update("/s", 2);
        assert!(menu.is_open(), "leaving slash position resets dismissal");
    }

    #[test]
    fn apply_selected_replaces_word_and_adds_space_for_args() {
        let mut menu = CompletionMenu::new();
        menu.update("/sa", 3);
        assert_eq!(menu.selected_item().unwrap().name, "save");
        let (input, cursor) = menu.apply_selected("/sa", 3).unwrap();
        assert_eq!(input, "/save ");
        assert_eq!(cursor, 6);

        let mut menu = CompletionMenu::new();
        menu.update("/st", 3);
        assert_eq!(menu.selected_item().unwrap().name, "stats");
        let (input, cursor) = menu.apply_selected("/st", 3).unwrap();
        assert_eq!(input, "/stats");
        assert_eq!(cursor, 6);
    }

    #[test]
    fn apply_replaces_whole_word_and_keeps_args() {
        let mut menu = CompletionMenu::new();
        menu.update("/sta", 4);
        // Rest of the word after the cursor is replaced, not kept.
        let (input, _) = menu.apply_selected("/stats", 4).unwrap();
        assert_eq!(input, "/stats");
        // Arguments after the word survive, without a doubled space.
        let mut menu = CompletionMenu::new();
        menu.update("/sa", 3);
        let (input, cursor) = menu.apply_selected("/sa my slot", 3).unwrap();
        assert_eq!(input, "/save my slot");
        assert_eq!(cursor, 6);
    }

    #[test]
    fn menu_closes_outside_slash_position() {
        let mut menu = CompletionMenu::new();
        menu.update("/s", 2);
        assert!(menu.is_open());
        menu.update("/save slot", 10);
        assert!(!menu.is_open());
        menu.update("look", 4);
        assert!(!menu.is_open());
        assert!(menu.apply_selected("/s", 2).is_none());
    }

    #[test]
    fn suggestion_line_format() {
        let save = crate::game::commands::slash_command("save").unwrap();
        let line = suggestion_line(save, true);
        assert!(line.starts_with("› /save [slot]"));
        assert!(line.contains("Save the game"));
        assert!(suggestion_line(save, false).starts_with("  /save"));
    }
}
