//! Raw-mode single-line editor for minimal mode.
//!
//! Cooked `read_line` cannot edit: arrow keys arrive as escape sequences
//! and print as garbage (`^[[A`). This editor puts the terminal in raw
//! mode for one line at a time and offers the same editing model as the
//! fullscreen composer — cursor keys, Home/End, word delete, ↑/↓ history
//! (draft preserved), plus slash-command autocomplete (Tab completes,
//! ↑/↓ choose while the menu is open).
//!
//! Callers gate on TTY: piped stdin keeps the plain `read_line` path.

use std::io::{self, Write};

use anyhow::{Context, Result};
use crossterm::cursor::{MoveTo, MoveToColumn, MoveUp};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};
use crossterm::{execute, queue};

use crate::ui::completion::{CompletionMenu, MAX_VISIBLE, suggestion_line};
use crate::ui::prompt::{Prompt, PromptHistory};
use crate::ui::theme;

const MENU_HINT: &str = "  tab complete · ↑/↓ choose · esc dismiss";

/// Stateful line reader: owns the ↑/↓ history across calls.
#[derive(Debug, Default)]
pub struct LineEditor {
    history: PromptHistory,
}

struct RawGuard {
    paste: bool,
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        if self.paste {
            let _ = execute!(io::stdout(), DisableBracketedPaste);
        }
        let _ = disable_raw_mode();
    }
}

impl LineEditor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read one edited line, prefilled with `initial` (empty for a fresh
    /// prompt; the transcript for STT edit-before-send).
    ///
    /// `Ok(None)` means EOF: Ctrl+D or Ctrl+C on an empty line.
    pub fn read_line(&mut self, initial: &str) -> Result<Option<String>> {
        let mut prompt = Prompt::new();
        prompt.set(initial);
        let mut menu = CompletionMenu::new();
        menu.update(prompt.input(), prompt.cursor());
        let mut view = View::default();

        enable_raw_mode().context("enable raw mode for line editor")?;
        let paste = execute!(io::stdout(), EnableBracketedPaste).is_ok();
        let _guard = RawGuard { paste };
        view.draw(&prompt, &menu)?;

        loop {
            match event::read().context("read line-editor key")? {
                Event::Resize(_, _) => view.draw(&prompt, &menu)?,
                Event::Paste(text) => {
                    self.history.abandon_browse();
                    prompt.insert_str(&text.replace(['\r', '\n'], " "));
                    menu.update(prompt.input(), prompt.cursor());
                    view.draw(&prompt, &menu)?;
                }
                Event::Key(key) => {
                    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                        continue;
                    }
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    if ctrl {
                        match key.code {
                            KeyCode::Char('c') => {
                                if prompt.is_empty() {
                                    view.finish(&prompt)?;
                                    return Ok(None);
                                }
                                prompt.clear();
                                self.history.abandon_browse();
                            }
                            KeyCode::Char('d') => {
                                if prompt.is_empty() {
                                    view.finish(&prompt)?;
                                    return Ok(None);
                                }
                                prompt.delete();
                            }
                            KeyCode::Char('a') => prompt.move_home(),
                            KeyCode::Char('e') => prompt.move_end(),
                            KeyCode::Char('u') => prompt.delete_to_start(),
                            KeyCode::Char('k') => prompt.delete_to_end(),
                            KeyCode::Char('w') => prompt.delete_word_back(),
                            KeyCode::Char('l') => {
                                execute!(io::stdout(), Clear(ClearType::All), MoveTo(0, 0))
                                    .context("clear screen")?;
                                view = View::default();
                            }
                            _ => continue,
                        }
                        menu.update(prompt.input(), prompt.cursor());
                        view.draw(&prompt, &menu)?;
                        continue;
                    }
                    match key.code {
                        KeyCode::Enter => {
                            let line = prompt.input().to_string();
                            menu.close();
                            view.finish(&prompt)?;
                            self.history.push(line.clone());
                            return Ok(Some(line));
                        }
                        KeyCode::Tab => {
                            if let Some((text, cursor)) =
                                menu.apply_selected(prompt.input(), prompt.cursor())
                            {
                                prompt.set_with_cursor(text, cursor);
                            }
                        }
                        KeyCode::BackTab => menu.move_up(),
                        KeyCode::Esc => {
                            if menu.is_open() {
                                menu.dismiss(prompt.input(), prompt.cursor());
                            } else if !prompt.is_empty() {
                                prompt.clear();
                                self.history.abandon_browse();
                            }
                        }
                        KeyCode::Backspace => {
                            self.history.abandon_browse();
                            prompt.backspace();
                        }
                        KeyCode::Delete => {
                            self.history.abandon_browse();
                            prompt.delete();
                        }
                        KeyCode::Left => prompt.move_left(),
                        KeyCode::Right => prompt.move_right(),
                        KeyCode::Home => prompt.move_home(),
                        KeyCode::End => prompt.move_end(),
                        KeyCode::Up => {
                            if menu.is_open() {
                                menu.move_up();
                            } else if let Some(line) = self.history.up(prompt.input()) {
                                prompt.set(line);
                            }
                        }
                        KeyCode::Down => {
                            if menu.is_open() {
                                menu.move_down();
                            } else if let Some(line) = self.history.down() {
                                prompt.set(line);
                            }
                        }
                        KeyCode::Char(c)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::ALT | KeyModifiers::SUPER) =>
                        {
                            self.history.abandon_browse();
                            prompt.insert_char(c);
                        }
                        _ => continue,
                    }
                    menu.update(prompt.input(), prompt.cursor());
                    view.draw(&prompt, &menu)?;
                }
                _ => {}
            }
        }
    }
}

/// Visible slice of `chars` for a `width`-column field, scrolled so the
/// cursor stays on screen. Returns `(start, text)`.
fn visible_window(input: &str, cursor: usize, width: usize) -> (usize, String) {
    let width = width.max(1);
    let chars: Vec<char> = input.chars().collect();
    let start = if cursor >= width {
        cursor - width + 1
    } else {
        0
    };
    let start = start.min(chars.len().saturating_sub(width.min(chars.len())));
    let start = if cursor < start { cursor } else { start };
    let text: String = chars.iter().skip(start).take(width).collect();
    (start, text)
}

/// Terminal width in columns, defaulting to 80 when unknown or zero
/// (some PTYs report 0×0 until resized).
fn terminal_width() -> usize {
    crossterm::terminal::size()
        .ok()
        .filter(|(w, _)| *w > 0)
        .map_or(80, |(w, _)| w as usize)
}

fn truncate_chars(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

/// Draws the prompt line plus any suggestion rows beneath it, always
/// leaving the cursor on the prompt line at the edit position.
#[derive(Debug, Default)]
struct View {
    /// Extra lines currently drawn below the prompt line.
    extra_lines: usize,
}

impl View {
    fn extra_texts(&self, menu: &CompletionMenu, width: usize) -> Vec<String> {
        if !menu.is_open() {
            return Vec::new();
        }
        let (start, rows) = menu.visible(MAX_VISIBLE);
        let mut lines: Vec<String> = rows
            .iter()
            .enumerate()
            .map(|(i, cmd)| {
                let text =
                    truncate_chars(&suggestion_line(cmd, start + i == menu.selected()), width);
                if start + i == menu.selected() {
                    theme::player(&text)
                } else {
                    theme::dim(&text)
                }
            })
            .collect();
        lines.push(theme::dim(&truncate_chars(MENU_HINT, width)));
        lines
    }

    fn draw(&mut self, prompt: &Prompt, menu: &CompletionMenu) -> Result<()> {
        let width = terminal_width();
        let field = width.saturating_sub(2).max(1);
        let (start, shown) = visible_window(prompt.input(), prompt.cursor(), field);
        let extras = self.extra_texts(menu, width);

        let mut out = io::stdout();
        queue!(out, MoveToColumn(0), Clear(ClearType::CurrentLine)).context("draw prompt")?;
        write!(out, "{}{}", theme::player("› "), shown).context("write prompt")?;
        for line in &extras {
            queue!(out, MoveToColumn(0)).context("draw suggestion")?;
            write!(out, "\r\n").context("suggestion newline")?;
            queue!(out, Clear(ClearType::CurrentLine)).context("clear suggestion")?;
            write!(out, "{line}").context("write suggestion")?;
        }
        // Erase leftovers when the menu shrank since the last draw.
        let mut below = extras.len();
        for _ in extras.len()..self.extra_lines {
            write!(out, "\r\n").context("clear stale suggestion")?;
            queue!(out, Clear(ClearType::CurrentLine)).context("clear stale line")?;
            below += 1;
        }
        if below > 0 {
            queue!(out, MoveUp(below as u16)).context("return to prompt line")?;
        }
        let col = 2 + (prompt.cursor() - start);
        queue!(out, MoveToColumn(col as u16)).context("place cursor")?;
        out.flush().context("flush line editor")?;
        self.extra_lines = extras.len();
        Ok(())
    }

    /// Final draw without the menu, then a newline so the next output
    /// starts fresh (the submitted line stays visible, like shell echo).
    fn finish(&mut self, prompt: &Prompt) -> Result<()> {
        self.draw(prompt, &CompletionMenu::new())?;
        write!(io::stdout(), "\r\n").context("finish line")?;
        io::stdout().flush().context("flush finish")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_keeps_cursor_visible() {
        let (start, text) = visible_window("abcdefghij", 10, 5);
        assert_eq!(text, "fghij");
        assert_eq!(start, 5);
        let (start, text) = visible_window("abcdefghij", 0, 5);
        assert_eq!((start, text.as_str()), (0, "abcde"));
        let (start, text) = visible_window("abc", 2, 10);
        assert_eq!((start, text.as_str()), (0, "abc"));
        let (_, text) = visible_window("", 0, 5);
        assert_eq!(text, "");
    }
}
