//! Character creation: name prompt + highlightable class picker.
//!
//! Runs before either UI mode starts, so minimal and fullscreen share it.
//! On a real terminal the class pick is an inline arrow-key selector: the
//! highlighted class shows a live overview (blurb, HP, stats, starting
//! item) sourced from [`CLASS_KITS`] — the same entries
//! [`GameState::apply_class_kit`] grants. Without a TTY on stdout it falls
//! back to a numbered typed menu; without a TTY on stdin callers skip
//! prompting entirely (`--name` / `--class` cover scripts).

use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use crossterm::cursor::{Hide, MoveToColumn, MoveUp, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};
use crossterm::{execute, queue};

use crate::game::state::{CLASS_KITS, ClassKit, GameState, class_kit};
use crate::ui::theme;

/// Row index of the "No class" entry (after every kit).
pub const NO_CLASS_INDEX: usize = CLASS_KITS.len();
/// Total selectable rows: every kit plus "No class".
pub const ROW_COUNT: usize = CLASS_KITS.len() + 1;

const NO_CLASS_TAGLINE: &str = "Start unaffiliated";
const NO_CLASS_BLURB: &str = "No kit, no commitments. The dungeon will teach you.";
const NO_CLASS_BEST_FOR: &str = "Improvising, letting the story decide";

/// Highlight state for the class picker (pure, unit-testable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassPicker {
    selected: usize,
}

impl Default for ClassPicker {
    fn default() -> Self {
        Self::new()
    }
}

impl ClassPicker {
    pub fn new() -> Self {
        Self { selected: 0 }
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Move the highlight up one row, wrapping to the bottom.
    pub fn move_up(&mut self) {
        self.selected = (self.selected + ROW_COUNT - 1) % ROW_COUNT;
    }

    /// Move the highlight down one row, wrapping to the top.
    pub fn move_down(&mut self) {
        self.selected = (self.selected + 1) % ROW_COUNT;
    }

    /// Jump to a row by index; out-of-range is a no-op. Returns success.
    pub fn select_row(&mut self, row: usize) -> bool {
        if row < ROW_COUNT {
            self.selected = row;
            true
        } else {
            false
        }
    }

    /// Jump by typed number: `1..=4` pick a kit, `0` picks "No class".
    pub fn select_number(&mut self, n: usize) -> bool {
        match n {
            0 => self.select_row(NO_CLASS_INDEX),
            1.. => self.select_row(n - 1),
        }
    }

    /// The highlighted kit, or `None` when "No class" is highlighted.
    pub fn selected_kit(&self) -> Option<&'static ClassKit> {
        CLASS_KITS.get(self.selected)
    }
}

/// What a typed class answer means (fallback menu / `--class` parsing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypedChoice {
    Class(&'static ClassKit),
    Skip,
    Invalid,
}

/// Parse a typed class answer: kit number (`1`-`4`), key/display name,
/// first letter (`f`/`r`/`w`/`c`), or skip (`""`, `0`, `skip`, `none`).
pub fn parse_typed_choice(input: &str) -> TypedChoice {
    let text = input.trim().to_ascii_lowercase();
    match text.as_str() {
        "" | "0" | "skip" | "none" | "no class" | "noclass" | "no-class" => TypedChoice::Skip,
        "1" | "2" | "3" | "4" => {
            let idx = text.parse::<usize>().unwrap_or(1) - 1;
            CLASS_KITS
                .get(idx)
                .map_or(TypedChoice::Invalid, TypedChoice::Class)
        }
        "f" => TypedChoice::Class(&CLASS_KITS[0]),
        "r" => TypedChoice::Class(&CLASS_KITS[1]),
        "w" => TypedChoice::Class(&CLASS_KITS[2]),
        "c" => TypedChoice::Class(&CLASS_KITS[3]),
        other => class_kit(other).map_or(TypedChoice::Invalid, TypedChoice::Class),
    }
}

/// Parse a `--class` flag value. `none` means explicitly no class.
pub fn parse_class_flag(value: &str) -> Result<Option<&'static ClassKit>, String> {
    match parse_typed_choice(value) {
        TypedChoice::Class(kit) => Ok(Some(kit)),
        TypedChoice::Skip if !value.trim().is_empty() => Ok(None),
        _ => Err(format!(
            "invalid --class '{}' (expected fighter|rogue|wizard|cleric|none)",
            value.trim()
        )),
    }
}

/// Role of one rendered picker line (drives styling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Title,
    Hint,
    Blank,
    Row { selected: bool },
    DetailTitle,
    Detail,
}

/// Plain-text picker lines. Line count is identical for every selection,
/// which is what makes in-place raw-mode redraw simple.
fn picker_lines(picker: &ClassPicker) -> Vec<(String, LineKind)> {
    let mut lines: Vec<(String, LineKind)> = vec![
        ("Choose a class".to_string(), LineKind::Title),
        (
            "↑/↓ or j/k browse · 1-4 jump · Enter confirm · Esc skip".to_string(),
            LineKind::Hint,
        ),
        (String::new(), LineKind::Blank),
    ];
    for (i, kit) in CLASS_KITS.iter().enumerate() {
        let marker = if picker.selected() == i { "›" } else { " " };
        lines.push((
            format!("{marker} {}  {:<9} {}", i + 1, kit.display, kit.tagline),
            LineKind::Row {
                selected: picker.selected() == i,
            },
        ));
    }
    let marker = if picker.selected() == NO_CLASS_INDEX {
        "›"
    } else {
        " "
    };
    lines.push((
        format!("{marker} 0  {:<9} {}", "No class", NO_CLASS_TAGLINE),
        LineKind::Row {
            selected: picker.selected() == NO_CLASS_INDEX,
        },
    ));
    lines.push((String::new(), LineKind::Blank));

    let (title, hp_stats, starts, best) = match picker.selected_kit() {
        Some(kit) => (
            format!("  {} — {}", kit.display, kit.blurb),
            format!("  HP {} · {}", kit.max_hp, kit.stats_line()),
            format!("  Starts with: {} — {}", kit.item, kit.item_desc),
            format!("  Best for: {}", kit.best_for),
        ),
        None => (
            format!("  No class — {NO_CLASS_BLURB}"),
            "  HP 20 · STR 10 · DEX 10 · CON 10 · INT 10 · WIS 10 · CHA 10".to_string(),
            "  Starts with: —".to_string(),
            format!("  Best for: {NO_CLASS_BEST_FOR}"),
        ),
    };
    lines.push((title, LineKind::DetailTitle));
    lines.push((hp_stats, LineKind::Detail));
    lines.push((starts, LineKind::Detail));
    lines.push((best, LineKind::Detail));
    lines
}

fn style_line(text: &str, kind: LineKind, styled: bool) -> String {
    if !styled {
        return text.to_string();
    }
    match kind {
        LineKind::Title | LineKind::DetailTitle => theme::player(text),
        LineKind::Hint => theme::dim(text),
        LineKind::Row { selected: true } => theme::player(text),
        LineKind::Row { selected: false } | LineKind::Detail => text.to_string(),
        LineKind::Blank => String::new(),
    }
}

/// Render the picker as one multi-line string (no trailing newline).
pub fn render_picker(picker: &ClassPicker, styled: bool) -> String {
    picker_lines(picker)
        .iter()
        .map(|(text, kind)| style_line(text, *kind, styled))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Numbered menu for the typed fallback: every kit with its overview.
pub fn render_typed_menu() -> String {
    let mut out = String::from("Choose a class:\n");
    for (i, kit) in CLASS_KITS.iter().enumerate() {
        out.push_str(&format!(
            "{}  ) {} — {}\n    {}\n    HP {} · {}\n    Starts with: {} — {}\n",
            i + 1,
            kit.display,
            kit.tagline,
            kit.blurb,
            kit.max_hp,
            kit.stats_line(),
            kit.item,
            kit.item_desc
        ));
    }
    out.push_str(&format!(
        "0  ) No class — {NO_CLASS_TAGLINE}\n    {NO_CLASS_BLURB}\n    HP 20 · all stats 10"
    ));
    out
}

/// One-line identity for status/hello lines: `Name · Class · HP x/y`.
pub fn summary_line(state: &GameState) -> String {
    let class = state.class.as_deref().unwrap_or("No class");
    format!(
        "{} · {} · HP {}/{}",
        state.character_name, class, state.hp, state.max_hp
    )
}

/// Multi-line confirmation card printed after creation (plain text).
pub fn format_character_summary(state: &GameState) -> String {
    let items = if state.inventory.is_empty() {
        "—".to_string()
    } else {
        state
            .inventory
            .iter()
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let class = state.class.as_deref().unwrap_or("No class");
    format!(
        "✦ {} · {}\n  HP {}/{} · {}\n  Starts with: {}",
        state.character_name,
        class,
        state.hp,
        state.max_hp,
        state.stats_string(),
        items
    )
}

/// Whether prompts may read from the keyboard (stdin is a terminal).
pub fn stdin_interactive() -> bool {
    io::stdin().is_terminal()
}

/// Whether inline raw-mode drawing is safe (stdout is a terminal).
pub fn stdout_interactive() -> bool {
    io::stdout().is_terminal()
}

/// Print the creation banner shown before the name prompt.
pub fn print_creation_header() {
    println!();
    println!("{}", theme::player("✦ Character Creation"));
    println!(
        "{}",
        theme::dim("Name your adventurer and pick a class — or skip and improvise.")
    );
    println!();
}

/// Ask once for a character name; empty input keeps `"Adventurer"`.
pub fn prompt_character_name() -> Result<String> {
    print!("{} ", theme::player("Character name [Adventurer]:"));
    let _ = io::stdout().flush();
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("reading character name")?;
    let name = line.trim();
    if name.is_empty() {
        Ok("Adventurer".to_string())
    } else {
        Ok(name.to_string())
    }
}

/// Print the post-creation confirmation card plus a trailing blank line.
pub fn print_character_summary(state: &GameState) {
    println!("{}", format_character_summary(state));
    println!();
}

/// Offer the class pick. Raw highlight picker on a full TTY, numbered
/// typed menu when only stdin is a TTY, no prompt at all otherwise.
pub fn prompt_character_class() -> Result<Option<&'static ClassKit>> {
    if stdin_interactive() && stdout_interactive() {
        run_raw_picker()
    } else if stdin_interactive() {
        run_typed_picker()
    } else {
        Ok(None)
    }
}

/// Typed fallback: print every overview, then loop until a valid answer.
fn run_typed_picker() -> Result<Option<&'static ClassKit>> {
    println!("{}", render_typed_menu());
    loop {
        print!("Class [1-4, name, or Enter to skip]: ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        let n = io::stdin()
            .read_line(&mut line)
            .context("reading character class")?;
        if n == 0 {
            return Ok(None); // EOF: start classless rather than spin.
        }
        match parse_typed_choice(&line) {
            TypedChoice::Class(kit) => return Ok(Some(kit)),
            TypedChoice::Skip => return Ok(None),
            TypedChoice::Invalid => {
                println!(
                    "{}",
                    theme::dim(&format!(
                        "Didn't catch '{}' — enter 1-4, a class name, or Enter to skip.",
                        line.trim()
                    ))
                );
            }
        }
    }
}

/// RAII: restore cooked mode + cursor when the raw picker exits.
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), Show);
        let _ = disable_raw_mode();
    }
}

/// Terminal width in columns, defaulting to 80 when unknown or zero
/// (some PTYs report 0×0 until resized).
fn terminal_width() -> usize {
    crossterm::terminal::size()
        .ok()
        .filter(|(w, _)| *w > 0)
        .map_or(80, |(w, _)| w as usize)
}

fn truncate_to_width(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else {
        text.chars().take(width.saturating_sub(1)).collect()
    }
}

/// Draw the picker frame. First draw starts at the cursor; redraws move
/// back to the frame top. Cursor ends on the last frame line.
fn draw_picker(picker: &ClassPicker, first: bool) -> Result<()> {
    let lines = picker_lines(picker);
    let width = terminal_width();
    let styled = theme::colors_enabled();
    let mut out = io::stdout();
    if !first && lines.len() > 1 {
        queue!(out, MoveUp((lines.len() - 1) as u16)).context("move to frame top")?;
    }
    for (i, (text, kind)) in lines.iter().enumerate() {
        queue!(out, MoveToColumn(0), Clear(ClearType::CurrentLine)).context("clear picker line")?;
        let shown = style_line(&truncate_to_width(text, width), *kind, styled);
        write!(out, "{shown}").context("write picker line")?;
        if i + 1 < lines.len() {
            write!(out, "\r\n").context("write picker newline")?;
        }
    }
    out.flush().context("flush picker")?;
    Ok(())
}

/// Erase the picker frame, leaving the cursor at its first line.
fn clear_picker(picker: &ClassPicker) -> Result<()> {
    let count = picker_lines(picker).len();
    let mut out = io::stdout();
    if count > 1 {
        queue!(out, MoveUp((count - 1) as u16)).context("move to frame top")?;
    }
    for i in 0..count {
        queue!(out, MoveToColumn(0), Clear(ClearType::CurrentLine)).context("clear picker")?;
        if i + 1 < count {
            write!(out, "\r\n").context("clear picker newline")?;
        }
    }
    if count > 1 {
        queue!(out, MoveUp((count - 1) as u16), MoveToColumn(0)).context("rewind picker")?;
    }
    out.flush().context("flush picker clear")?;
    Ok(())
}

/// Inline arrow-key class selector (Enter confirm, Esc/q skip, Ctrl+C abort).
fn run_raw_picker() -> Result<Option<&'static ClassKit>> {
    let mut picker = ClassPicker::new();
    enable_raw_mode().context("enable raw mode for class picker")?;
    let _guard = RawModeGuard;
    execute!(io::stdout(), Hide).context("hide cursor")?;
    draw_picker(&picker, true)?;

    loop {
        let event = event::read().context("read class picker key")?;
        match event {
            Event::Resize(_, _) => draw_picker(&picker, false)?,
            Event::Key(key) => {
                let nav = matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat);
                let press = key.kind == KeyEventKind::Press;
                if press
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('c')
                {
                    clear_picker(&picker)?;
                    bail!("character creation interrupted");
                }
                match key.code {
                    KeyCode::Up if nav => {
                        picker.move_up();
                        draw_picker(&picker, false)?;
                    }
                    KeyCode::Down if nav => {
                        picker.move_down();
                        draw_picker(&picker, false)?;
                    }
                    KeyCode::Char('k') if nav => {
                        picker.move_up();
                        draw_picker(&picker, false)?;
                    }
                    KeyCode::Char('j') if nav => {
                        picker.move_down();
                        draw_picker(&picker, false)?;
                    }
                    KeyCode::Char(c @ '0'..='9') if press => {
                        if picker.select_number(c as usize - '0' as usize) {
                            draw_picker(&picker, false)?;
                        }
                    }
                    KeyCode::Enter if press => {
                        clear_picker(&picker)?;
                        return Ok(picker.selected_kit());
                    }
                    KeyCode::Esc if press => {
                        clear_picker(&picker)?;
                        return Ok(None);
                    }
                    KeyCode::Char('q') if press => {
                        clear_picker(&picker)?;
                        return Ok(None);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_navigation_wraps() {
        let mut p = ClassPicker::new();
        assert_eq!(p.selected(), 0);
        p.move_up();
        assert_eq!(p.selected(), NO_CLASS_INDEX);
        p.move_down();
        assert_eq!(p.selected(), 0);
        p.move_down();
        assert_eq!(p.selected(), 1);
        assert_eq!(p.selected_kit().unwrap().key, "rogue");
        assert!(p.select_row(NO_CLASS_INDEX));
        assert!(p.selected_kit().is_none());
        assert!(!p.select_row(ROW_COUNT));
    }

    #[test]
    fn picker_number_jump() {
        let mut p = ClassPicker::new();
        assert!(p.select_number(3));
        assert_eq!(p.selected_kit().unwrap().key, "wizard");
        assert!(p.select_number(0));
        assert!(p.selected_kit().is_none());
        assert!(!p.select_number(9));
    }

    #[test]
    fn typed_choice_parsing() {
        assert_eq!(parse_typed_choice(""), TypedChoice::Skip);
        assert_eq!(parse_typed_choice(" 0 "), TypedChoice::Skip);
        assert_eq!(parse_typed_choice("none"), TypedChoice::Skip);
        assert_eq!(parse_typed_choice("2"), parse_typed_choice("rogue"));
        assert_eq!(parse_typed_choice(" Fighter "), parse_typed_choice("f"));
        assert_eq!(parse_typed_choice("WIZARD"), parse_typed_choice("3"));
        assert_eq!(parse_typed_choice("bard"), TypedChoice::Invalid);
        assert_eq!(parse_typed_choice("5"), TypedChoice::Invalid);
    }

    #[test]
    fn class_flag_parsing() {
        assert_eq!(parse_class_flag("cleric").unwrap().unwrap().key, "cleric");
        assert_eq!(parse_class_flag("none").unwrap(), None);
        assert!(parse_class_flag("bard").is_err());
        assert!(parse_class_flag("").is_err());
    }

    #[test]
    fn render_shows_highlight_and_selected_overview() {
        let mut p = ClassPicker::new();
        p.select_number(2); // Rogue
        let frame = render_picker(&p, false);
        assert!(frame.contains("Choose a class"));
        assert!(frame.contains("› 2  Rogue"));
        assert!(!frame.contains("› 1  Fighter"));
        let rogue = class_kit("rogue").unwrap();
        assert!(frame.contains(rogue.blurb));
        assert!(frame.contains(&format!("HP {}", rogue.max_hp)));
        assert!(frame.contains(rogue.item));
        assert!(frame.contains(rogue.best_for));
        for kit in CLASS_KITS {
            assert!(frame.contains(kit.display), "missing {}", kit.display);
            assert!(frame.contains(kit.tagline), "missing {}", kit.tagline);
        }
        assert!(frame.contains("No class"));
    }

    #[test]
    fn render_height_is_constant_across_selections() {
        let mut p = ClassPicker::new();
        let heights: Vec<usize> = (0..ROW_COUNT)
            .map(|row| {
                p.select_row(row);
                render_picker(&p, false).lines().count()
            })
            .collect();
        assert!(heights.iter().all(|h| *h == heights[0]), "{heights:?}");
    }

    #[test]
    fn typed_menu_lists_every_kit() {
        let menu = render_typed_menu();
        for kit in CLASS_KITS {
            assert!(menu.contains(kit.display));
            assert!(menu.contains(kit.blurb));
            assert!(menu.contains(kit.item));
        }
        assert!(menu.contains("No class"));
    }

    #[test]
    fn summary_reflects_applied_kit() {
        let mut state = GameState::new("Hero");
        state.apply_class_kit("wizard").unwrap();
        let card = format_character_summary(&state);
        assert!(card.contains("Hero · Wizard"));
        assert!(card.contains("HP 14/14"));
        assert!(card.contains("Spellbook"));
        assert_eq!(summary_line(&state), "Hero · Wizard · HP 14/14");

        let plain = GameState::new("Nobody");
        assert!(format_character_summary(&plain).contains("No class"));
    }
}
