//! Fullscreen app state and ratatui drawing.

use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as UiBlock, BorderType, Borders, Paragraph};

use crate::game::commands::SessionUsage;
use crate::ui::blocks::{Block, SystemKind};
use crate::ui::completion::{CompletionMenu, MAX_VISIBLE, suggestion_line};
use crate::ui::markdown;
use crate::ui::prompt::{self, Prompt, PromptHistory};
use crate::ui::sanitize;
use crate::ui::scrollback::Scrollback;
use crate::ui::shortcuts;
use crate::ui::status;
use crate::ui::theme;

/// Which pane currently receives keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Prompt,
    Scrollback,
}

/// Theme names cycled by `/theme`.
pub const THEME_CYCLE: &[&str] = &["groknight", "grokday", "mono"];

/// Fullscreen TUI application state.
#[derive(Debug)]
pub struct FullscreenApp {
    pub scrollback: Scrollback,
    pub focus: Focus,
    pub prompt: Prompt,
    pub history: PromptHistory,
    pub status_line: status::StatusLine,
    pub tools_expanded: bool,
    pub theme_name: String,
    pub activity: Option<String>,
    pub should_quit: bool,
    /// Set when the user submits a line; consumed by the game loop.
    pub pending_action: Option<String>,
    pub show_shortcuts: bool,
    /// F2 hold-to-talk is active (recording).
    pub ptt_held: bool,
    /// Words arriving from streaming speech-to-text. Shown in the composer
    /// until the utterance is committed.
    pub live_line: String,
    /// Slash-command autocomplete state for the composer.
    pub completions: CompletionMenu,
    /// Compact density: less padding / tighter layout.
    pub compact: bool,
    /// Last DM narration text (for `/copy`).
    pub last_dm_text: String,
    /// Last action actually sent to the DM (for `/again`). Meta commands,
    /// local queries, and the `/again` line itself are never recorded here.
    pub last_action: Option<String>,
    /// Running token usage for this process (shown by `/usage`; never saved).
    pub usage: SessionUsage,
}

impl FullscreenApp {
    pub fn new(theme_name: impl Into<String>, tools_expanded: bool, show_shortcuts: bool) -> Self {
        Self::with_options(theme_name, tools_expanded, show_shortcuts, false)
    }

    pub fn with_options(
        theme_name: impl Into<String>,
        tools_expanded: bool,
        show_shortcuts: bool,
        compact: bool,
    ) -> Self {
        Self {
            scrollback: Scrollback::new(),
            focus: Focus::Prompt,
            prompt: Prompt::new(),
            history: PromptHistory::new(),
            status_line: status::StatusLine::default(),
            tools_expanded,
            theme_name: theme_name.into(),
            activity: None,
            should_quit: false,
            pending_action: None,
            show_shortcuts,
            ptt_held: false,
            live_line: String::new(),
            completions: CompletionMenu::new(),
            compact,
            last_dm_text: String::new(),
            last_action: None,
            usage: SessionUsage::default(),
        }
    }

    /// Drop per-campaign context after `/load` swaps in a new game state.
    ///
    /// Prompt recall, `/copy`, and `/again` must never leak the previous
    /// campaign into the loaded one. The loaded save's narration (if any)
    /// becomes the new copy text.
    pub fn reset_session_after_load(&mut self, narration: Option<&str>) {
        self.history = PromptHistory::new();
        self.last_dm_text = narration.unwrap_or_default().to_string();
        self.last_action = None;
    }

    pub fn push_block(&mut self, block: Block) {
        if let Block::Dm { text } = &block {
            self.last_dm_text = text.clone();
        }
        self.scrollback.push(block);
    }

    pub fn push_blocks(&mut self, blocks: impl IntoIterator<Item = Block>) {
        for b in blocks {
            self.push_block(b);
        }
    }

    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Prompt => Focus::Scrollback,
            Focus::Scrollback => Focus::Prompt,
        };
    }

    pub fn cycle_theme(&mut self) {
        let idx = THEME_CYCLE
            .iter()
            .position(|t| t.eq_ignore_ascii_case(&self.theme_name))
            .unwrap_or(0);
        let next = THEME_CYCLE[(idx + 1) % THEME_CYCLE.len()];
        self.theme_name = next.to_string();
    }

    /// Take pending submitted action, if any.
    pub fn take_pending(&mut self) -> Option<String> {
        self.pending_action.take()
    }

    /// Submit current prompt text (trimmed). Empty input is ignored.
    /// Pushes non-empty submissions onto history.
    pub fn submit_prompt(&mut self) {
        self.completions.close();
        let text = self.prompt.take();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        self.history.push(trimmed.to_string());
        self.pending_action = Some(trimmed.to_string());
    }

    /// Handle a character when prompt is focused.
    pub fn on_char(&mut self, c: char) {
        if self.focus == Focus::Prompt {
            self.history.abandon_browse();
            self.prompt.insert_char(c);
        }
    }

    pub fn on_backspace(&mut self) {
        if self.focus == Focus::Prompt {
            self.history.abandon_browse();
            self.prompt.backspace();
        }
    }

    pub fn on_delete(&mut self) {
        if self.focus == Focus::Prompt {
            self.history.abandon_browse();
            self.prompt.delete();
        }
    }

    pub fn on_left(&mut self) {
        if self.focus == Focus::Prompt {
            self.prompt.move_left();
        }
    }

    pub fn on_right(&mut self) {
        if self.focus == Focus::Prompt {
            self.prompt.move_right();
        }
    }

    pub fn on_home(&mut self) {
        if self.focus == Focus::Prompt {
            self.prompt.move_home();
        }
    }

    pub fn on_end(&mut self) {
        if self.focus == Focus::Prompt {
            self.prompt.move_end();
        }
    }

    pub fn on_delete_word_back(&mut self) {
        if self.focus == Focus::Prompt {
            self.history.abandon_browse();
            self.prompt.delete_word_back();
        }
    }

    pub fn on_delete_to_start(&mut self) {
        if self.focus == Focus::Prompt {
            self.history.abandon_browse();
            self.prompt.delete_to_start();
        }
    }

    pub fn on_delete_to_end(&mut self) {
        if self.focus == Focus::Prompt {
            self.history.abandon_browse();
            self.prompt.delete_to_end();
        }
    }

    /// Apply a readline-style Ctrl editing command. Returns true when the
    /// key was recognized. Ctrl+L clears the transcript in either focus;
    /// the cursor/delete commands only act on the prompt.
    pub fn on_ctrl_edit(&mut self, c: char) -> bool {
        match c.to_ascii_lowercase() {
            'a' => self.on_home(),
            'e' => self.on_end(),
            'u' => self.on_delete_to_start(),
            'k' => self.on_delete_to_end(),
            'w' => self.on_delete_word_back(),
            'd' => self.on_delete(),
            'l' => self.scrollback.clear(),
            _ => return false,
        }
        true
    }

    /// Insert clipboard text as one editable line and focus the composer.
    /// Pasting never submits.
    pub fn on_paste(&mut self, text: &str) {
        self.focus = Focus::Prompt;
        self.history.abandon_browse();
        self.prompt.insert_paste(text);
    }

    /// History previous (↑) when prompt-focused. The current draft is
    /// saved and restored by ↓, so recall works mid-typing too.
    /// Returns true if history was applied.
    pub fn history_up(&mut self) -> bool {
        if self.focus != Focus::Prompt {
            return false;
        }
        if let Some(line) = self.history.up(self.prompt.input()) {
            self.prompt.set(line);
            true
        } else {
            false
        }
    }

    /// History next (↓) when prompt-focused.
    pub fn history_down(&mut self) -> bool {
        if self.focus != Focus::Prompt {
            return false;
        }
        if let Some(line) = self.history.down() {
            self.prompt.set(line);
            true
        } else {
            false
        }
    }

    /// Recompute slash-command suggestions from the composer. Cheap and
    /// idempotent; the run loop calls it before every redraw.
    pub fn sync_completions(&mut self) {
        if self.focus == Focus::Prompt {
            self.completions
                .update(self.prompt.input(), self.prompt.cursor());
        } else {
            self.completions.close();
        }
    }

    /// Apply the highlighted completion (Tab). Returns true when applied.
    pub fn apply_completion(&mut self) -> bool {
        let Some((text, cursor)) = self
            .completions
            .apply_selected(self.prompt.input(), self.prompt.cursor())
        else {
            return false;
        };
        self.history.abandon_browse();
        self.prompt.set_with_cursor(text, cursor);
        self.sync_completions();
        true
    }

    /// Esc: dismiss autocomplete, return from scrollback, then clear a
    /// non-empty draft. Quitting is explicit (Ctrl+C or `/quit`).
    pub fn on_esc(&mut self) {
        if self.completions.is_open() {
            self.completions
                .dismiss(self.prompt.input(), self.prompt.cursor());
            return;
        }
        if self.focus == Focus::Scrollback {
            self.focus = Focus::Prompt;
            return;
        }
        if !self.prompt.is_empty() {
            self.history.abandon_browse();
            self.prompt.clear();
        }
    }

    pub fn on_scroll_up(&mut self) {
        self.scrollback.scroll_up(1);
    }

    pub fn on_scroll_down(&mut self) {
        self.scrollback.scroll_down(1);
    }

    /// Draw the full app into the frame.
    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        let colors = Colors::for_theme(&self.theme_name);

        // Paint the whole frame (including margins) so the palette's contrast
        // is measured against a known background rather than the terminal's.
        // Mono / NO_COLOR leaves the terminal background untouched.
        if let Some(bg) = colors.background {
            frame.buffer_mut().set_style(area, Style::default().bg(bg));
        }

        // Compact: tighter prompt row and zero outer margin (non-compact keeps a 1-cell margin).
        let margin = u16::from(!self.compact);
        let prompt_height = if self.compact { 2 } else { 3 };
        let mut constraints = vec![
            Constraint::Length(1), // status
            Constraint::Min(3),    // scrollback
        ];
        if self.activity.is_some() {
            constraints.push(Constraint::Length(1));
        }
        let completion_rows = if self.completions.is_open() {
            self.completions.visible(MAX_VISIBLE).1.len() as u16
        } else {
            0
        };
        if completion_rows > 0 {
            constraints.push(Constraint::Length(completion_rows));
        }
        constraints.push(Constraint::Length(prompt_height)); // prompt
        if self.show_shortcuts {
            constraints.push(Constraint::Length(1)); // shortcuts
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(margin)
            .constraints(constraints)
            .split(area);

        let mut i = 0;
        self.draw_status(frame, chunks[i], &colors);
        i += 1;
        self.draw_scrollback(frame, chunks[i], &colors);
        i += 1;
        if self.activity.is_some() {
            self.draw_activity(frame, chunks[i], &colors);
            i += 1;
        }
        if completion_rows > 0 {
            self.draw_completions(frame, chunks[i], &colors);
            i += 1;
        }
        self.draw_prompt(frame, chunks[i], &colors);
        i += 1;
        if self.show_shortcuts {
            self.draw_shortcuts(frame, chunks[i], &colors);
        }
    }

    fn draw_status(&self, frame: &mut Frame, area: Rect, colors: &Colors) {
        let chunks = self.status_line.fit(area.width as usize);
        let mut spans = Vec::with_capacity(chunks.len() * 2);
        for (i, chunk) in chunks.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("  ·  ", Style::default().fg(colors.muted)));
            }
            let style = match chunk.hp {
                Some((hp, max_hp)) => colors.hp_style(hp, max_hp),
                None => Style::default().fg(colors.status_fg),
            };
            spans.push(Span::styled(chunk.text.clone(), style));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_scrollback(&self, frame: &mut Frame, area: Rect, colors: &Colors) {
        let border_style = if self.focus == Focus::Scrollback {
            Style::default().fg(colors.accent)
        } else {
            Style::default().fg(colors.muted)
        };
        let borders = if self.compact {
            Borders::NONE
        } else {
            Borders::ALL
        };
        let block = UiBlock::default()
            .borders(borders)
            .border_type(BorderType::Rounded)
            .border_style(border_style);

        let inner = if self.compact {
            area
        } else {
            block.inner(area)
        };
        if !self.compact {
            frame.render_widget(block, area);
        }

        let lines = self.build_scrollback_lines(colors, !self.compact && inner.height >= 24);
        // Wrap to visual rows *before* selecting the window, otherwise long
        // prose expands past `height` inside Paragraph::wrap and the newest
        // content (state deltas included) is clipped off the bottom.
        let visual = wrap_lines(lines, inner.width as usize);
        let height = inner.height as usize;
        let total = visual.len();
        let scroll = self.scrollback.scroll() as usize;
        // Pin to bottom: show last `height` visual rows, shifted up by scroll.
        // Scroll offsets are visual rows, matching PageUp/mouse semantics.
        let end = total.saturating_sub(scroll);
        let start = end.saturating_sub(height);
        let visible: Vec<Line> = visual.into_iter().skip(start).take(height).collect();

        frame.render_widget(Paragraph::new(visible), inner);
    }

    fn build_scrollback_lines(&self, colors: &Colors, separators: bool) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for entry in self.scrollback.entries() {
            lines.extend(block_to_lines(&entry.block, self.tools_expanded, colors));
            // Blank separator after each block. At short heights (and in
            // compact density) the separator rows cost more than they add.
            if separators {
                lines.push(Line::from(""));
            }
        }
        lines
    }

    fn draw_activity(&self, frame: &mut Frame, area: Rect, colors: &Colors) {
        let text = sanitize::sanitize_terminal_text(self.activity.as_deref().unwrap_or(""));
        // Fixed-width rune + text keeps the strip to one row, so animating it
        // cannot reflow the transcript above.
        let para = Paragraph::new(Line::from(Span::styled(
            format!(" {} {text}", spinner_frame()),
            Style::default()
                .fg(colors.activity)
                .add_modifier(Modifier::ITALIC),
        )));
        frame.render_widget(para, area);
    }

    fn draw_completions(&self, frame: &mut Frame, area: Rect, colors: &Colors) {
        let (start, rows) = self.completions.visible(MAX_VISIBLE);
        let lines: Vec<Line> = rows
            .iter()
            .enumerate()
            .map(|(i, cmd)| {
                let selected = start + i == self.completions.selected();
                let text = truncate_to_width(&suggestion_line(cmd, selected), area.width as usize);
                let style = if selected {
                    Style::default()
                        .fg(colors.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(colors.muted)
                };
                Line::from(Span::styled(format!(" {text}"), style))
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn draw_prompt(&self, frame: &mut Frame, area: Rect, colors: &Colors) {
        let border_style = if self.focus == Focus::Prompt {
            Style::default().fg(colors.accent)
        } else {
            Style::default().fg(colors.muted)
        };
        let borders = if self.compact {
            Borders::TOP
        } else {
            Borders::ALL
        };
        let mut block = UiBlock::default()
            .borders(borders)
            .border_type(BorderType::Rounded)
            .border_style(border_style);
        // Text indicator so scrollback focus is visible even when the
        // shortcut bar is hidden (Esc returns to the prompt).
        if self.focus == Focus::Scrollback {
            block = block.title("focus: scrollback");
        }

        let inner = block.inner(area);
        let (body_text, interim) = composer_text(&self.prompt, &self.live_line);
        let prefix = Span::styled(
            "› ",
            Style::default()
                .fg(colors.accent)
                .add_modifier(Modifier::BOLD),
        );
        let body_style = if interim {
            Style::default()
                .fg(colors.muted)
                .add_modifier(Modifier::ITALIC)
        } else {
            Style::default().fg(colors.text)
        };
        // Reserve one cell after the visible body for the terminal cursor.
        let body_width = inner.width.saturating_sub(3) as usize;
        let cursor_char = if interim {
            body_text.chars().count()
        } else {
            self.prompt.sanitized_cursor_char_index()
        };
        let window = prompt::composer_window(&body_text, cursor_char, body_width);
        let body = Span::styled(window.text, body_style);
        let para = Paragraph::new(Line::from(vec![prefix, body])).block(block);
        frame.render_widget(para, area);

        // Place terminal cursor at the edit position when prompt is focused.
        if self.focus == Focus::Prompt {
            let cols = window.cursor_col.min(u16::MAX as usize) as u16;
            let x = inner.x.saturating_add(2).saturating_add(cols);
            let y = inner.y;
            // Clamp within inner width.
            let max_x = inner.x.saturating_add(inner.width.saturating_sub(1));
            frame.set_cursor_position((x.min(max_x), y));
        }
    }

    fn draw_shortcuts(&self, frame: &mut Frame, area: Rect, colors: &Colors) {
        let busy = self.activity.is_some();
        let text = shortcuts::shortcuts_line_for(
            self.tools_expanded,
            busy,
            self.focus == Focus::Scrollback,
            self.prompt.is_empty(),
        );
        let para = Paragraph::new(Line::from(Span::styled(
            truncate_to_width(&text, area.width as usize),
            Style::default().fg(colors.muted),
        )));
        frame.render_widget(para, area);
    }
}

/// Palette derived from theme name + NO_COLOR.
#[derive(Debug, Clone, Copy)]
struct Colors {
    /// Painted frame background; `None` leaves the terminal default (mono).
    background: Option<Color>,
    accent: Color,
    dm: Color,
    tool: Color,
    text: Color,
    muted: Color,
    error: Color,
    success: Color,
    activity: Color,
    status_fg: Color,
    /// Mono / NO_COLOR: no RGB styling, no painted background.
    mono: bool,
}

impl Colors {
    fn for_theme(name: &str) -> Self {
        Self::for_theme_with(name, theme::colors_enabled())
    }

    fn for_theme_with(name: &str, colors_enabled: bool) -> Self {
        if !colors_enabled {
            return Self::mono();
        }
        match name.to_ascii_lowercase().as_str() {
            // Warm paper background. Contrast vs #f7f3ea:
            // text 15.00:1, muted 6.67:1, accent 6.67:1, DM 6.27:1,
            // tools 5.84:1, error 6.26:1, success 6.08:1, HP mid 5.05:1.
            "grokday" => Self {
                background: Some(Color::Rgb(247, 243, 234)),
                accent: Color::Rgb(107, 63, 160),
                dm: Color::Rgb(30, 90, 160),
                tool: Color::Rgb(130, 85, 0),
                text: Color::Rgb(32, 30, 28),
                muted: Color::Rgb(90, 85, 78),
                error: Color::Rgb(175, 30, 30),
                success: Color::Rgb(25, 105, 55),
                activity: Color::Rgb(120, 50, 130),
                status_fg: Color::Rgb(32, 30, 28),
                mono: false,
            },
            "mono" => Self::mono(),
            // groknight (default). Contrast vs #12131a:
            // text 14.87:1, muted 8.35:1, accent 8.76:1, DM 10.84:1,
            // tools 12.53:1, error 8.00:1, success 11.67:1.
            _ => Self {
                background: Some(Color::Rgb(18, 19, 26)),
                accent: Color::Rgb(200, 160, 255),
                dm: Color::Rgb(140, 205, 255),
                tool: Color::Rgb(255, 205, 110),
                text: Color::Rgb(232, 230, 227),
                muted: Color::Rgb(170, 174, 185),
                error: Color::Rgb(255, 135, 135),
                success: Color::Rgb(130, 225, 160),
                activity: Color::Rgb(255, 175, 215),
                status_fg: Color::Rgb(205, 205, 215),
                mono: false,
            },
        }
    }

    /// Style for the HP segment, matching the minimal theme thresholds.
    fn hp_style(&self, hp: i32, max_hp: i32) -> Style {
        let color = if self.mono {
            Color::Reset
        } else {
            let (r, g, b) = theme::hp_rgb(hp, max_hp);
            Color::Rgb(r, g, b)
        };
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    }

    fn mono() -> Self {
        Self {
            background: None,
            accent: Color::Reset,
            dm: Color::Reset,
            tool: Color::Reset,
            text: Color::Reset,
            muted: Color::Reset,
            error: Color::Reset,
            success: Color::Reset,
            activity: Color::Reset,
            status_fg: Color::Reset,
            mono: true,
        }
    }
}

/// Composer text, and whether it is an in-progress transcript.
///
/// Both the draft and the live STT transcript are untrusted terminal input,
/// so the displayed body is sanitized. Cursor math uses the same sanitized
/// draft (see [`Prompt::cursor_display_cols`]) to keep columns aligned.
fn composer_text(prompt: &Prompt, live_line: &str) -> (String, bool) {
    let live_line = sanitize::sanitize_terminal_line(live_line);
    if live_line.is_empty() {
        return (sanitize::sanitize_terminal_line(prompt.input()), false);
    }
    let draft = sanitize::sanitize_terminal_line(prompt.input().trim_end());
    if draft.is_empty() {
        (live_line, true)
    } else {
        (format!("{draft} {live_line}"), true)
    }
}

fn block_to_lines(block: &Block, tools_expanded: bool, colors: &Colors) -> Vec<Line<'static>> {
    match block {
        Block::Player { text, spoken } => {
            let label = if *spoken { "You · voice" } else { "You" };
            let text = sanitize::sanitize_terminal_text(text);
            let mut lines = vec![Line::from(Span::styled(
                label.to_string(),
                Style::default()
                    .fg(colors.accent)
                    .add_modifier(Modifier::BOLD),
            ))];
            for line in text.lines() {
                lines.push(Line::from(Span::styled(
                    format!("  {line}"),
                    Style::default().fg(colors.text),
                )));
            }
            lines
        }
        Block::Dm { text } => {
            let mut lines = vec![Line::from(Span::styled(
                "DM".to_string(),
                Style::default().fg(colors.dm).add_modifier(Modifier::BOLD),
            ))];
            let plain = markdown::strip_inline(text);
            for line in plain.lines() {
                lines.push(Line::from(Span::styled(
                    format!("  {line}"),
                    Style::default().fg(colors.text),
                )));
            }
            lines
        }
        Block::Tools { calls } => {
            let header = format!("Tools ({})", calls.len());
            let mut lines = vec![Line::from(Span::styled(
                header,
                Style::default().fg(colors.tool),
            ))];
            if calls.is_empty() {
                return lines;
            }
            if !tools_expanded {
                let names: Vec<String> = calls
                    .iter()
                    .map(|c| sanitize::sanitize_terminal_text(&c.name))
                    .collect();
                lines.push(Line::from(Span::styled(
                    format!("  {}", names.join(", ")),
                    Style::default().fg(colors.muted),
                )));
            } else {
                for call in calls {
                    let name = sanitize::sanitize_terminal_text(&call.name);
                    let summary = markdown::truncate_chars(&call.result_summary, 80);
                    lines.push(Line::from(vec![
                        Span::styled("  · ".to_string(), Style::default().fg(colors.muted)),
                        Span::styled(name, Style::default().fg(colors.tool)),
                        Span::styled(" → ".to_string(), Style::default().fg(colors.muted)),
                        Span::styled(summary, Style::default().fg(colors.muted)),
                    ]));
                }
            }
            lines
        }
        Block::State { text } => {
            let mut lines = vec![Line::from(Span::styled(
                "State".to_string(),
                Style::default().fg(colors.muted),
            ))];
            let plain = markdown::strip_inline(text);
            for line in plain.lines() {
                lines.push(Line::from(Span::styled(
                    format!("  {line}"),
                    Style::default().fg(colors.text),
                )));
            }
            lines
        }
        Block::System { text, kind } => {
            let style = match kind {
                SystemKind::Info => Style::default().fg(colors.muted),
                SystemKind::Error => Style::default()
                    .fg(colors.error)
                    .add_modifier(Modifier::BOLD),
                SystemKind::Success => Style::default().fg(colors.success),
            };
            let text = sanitize::sanitize_terminal_text(text);
            vec![Line::from(Span::styled(text, style))]
        }
        Block::Dice { text } => {
            let plain = markdown::strip_inline(text);
            vec![Line::from(Span::styled(
                plain,
                Style::default().fg(colors.success),
            ))]
        }
    }
}

/// Width of one character in terminal cells.
fn char_width(c: char) -> usize {
    let mut buf = [0u8; 4];
    status::display_width(c.encode_utf8(&mut buf))
}

/// Pre-wrap logical transcript lines into visual rows at `width` cells.
///
/// Skipping this step and letting `Paragraph::wrap` do it after the window is
/// chosen is what clipped the newest rows: the paragraph wrapped *after* the
/// last logical lines were selected, so it could need more rows than the pane
/// had and silently drop the bottom.
fn wrap_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for line in lines {
        out.extend(wrap_line(line, width));
    }
    out
}

fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }

    let mut cells: Vec<(char, Style)> = Vec::new();
    for span in line.spans {
        for ch in span.content.chars() {
            cells.push((ch, span.style));
        }
    }
    if cells.is_empty() {
        return vec![Line::from("")];
    }

    // Body lines are built as `"  {text}"`; continuation rows get a matching
    // two-space hanging indent so wrapped prose stays visually under the body
    // rather than under the block header.
    let hanging = if cells.len() >= 2 && cells[0].0 == ' ' && cells[1].0 == ' ' {
        2.min(width)
    } else {
        0
    };
    let indent_style = cells[0].1;

    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut start = 0usize;
    let mut first = true;

    while start < cells.len() {
        let indent = if first { 0 } else { hanging };
        let avail = width.saturating_sub(indent);
        if avail == 0 {
            // Pane narrower than the indent: emit one cell so we still make
            // progress and let the terminal clip it.
            let end = (start + 1).min(cells.len());
            rows.push(cells[start..end].to_vec());
            start = end;
            first = false;
            continue;
        }

        let mut col = 0usize;
        let mut idx = start;
        let mut last_space = None;
        let mut has_content = false;
        while idx < cells.len() {
            let (ch, _) = cells[idx];
            let w = char_width(ch);
            if col + w > avail {
                break;
            }
            if ch == ' ' {
                if has_content {
                    last_space = Some(idx);
                }
            } else {
                has_content = true;
            }
            col += w;
            idx += 1;
        }

        let overflow = idx < cells.len();
        let mut end = idx;
        let mut skip_space = overflow;
        if overflow && let Some(space) = last_space {
            end = space;
            skip_space = true;
        }
        if end == start {
            // A single wide glyph is wider than the pane: take it anyway.
            end = (start + 1).min(cells.len());
            skip_space = false;
        }

        let mut row = Vec::new();
        if !first && indent > 0 {
            row.extend((0..indent).map(|_| (' ', indent_style)));
        }
        row.extend_from_slice(&cells[start..end]);
        rows.push(row);

        start = end;
        if skip_space {
            while start < cells.len() && cells[start].0 == ' ' {
                start += 1;
            }
        }
        first = false;
    }

    rows.into_iter().map(|row| cells_to_line(&row)).collect()
}

fn cells_to_line(cells: &[(char, Style)]) -> Line<'static> {
    if cells.is_empty() {
        return Line::from("");
    }
    let mut spans = Vec::new();
    let mut style = cells[0].1;
    let mut text = String::new();
    for &(ch, cell_style) in cells {
        if cell_style != style && !text.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut text), style));
            style = cell_style;
        }
        text.push(ch);
    }
    if !text.is_empty() {
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

/// Activity rune frames at roughly 4 fps (250 ms each).
const SPINNER_FRAMES: [&str; 6] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴"];

fn spinner_frame_at(millis: u128) -> &'static str {
    SPINNER_FRAMES[((millis / 250) % SPINNER_FRAMES.len() as u128) as usize]
}

fn spinner_frame() -> &'static str {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    spinner_frame_at(millis)
}

fn truncate_to_width(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    // Status lines, completions, and shortcut hints all render through here;
    // sanitize before measuring so stripped bytes cannot affect the width.
    let s = sanitize::sanitize_terminal_line(s);
    if status::display_width(&s) <= width {
        return s;
    }
    let mut out = status::take_display(&s, width.saturating_sub(1)).to_string();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::commands::format_session_usage;
    use crate::ui::blocks::Block;

    #[test]
    fn composer_shows_live_line_in_place_of_an_empty_draft() {
        let prompt = Prompt::new();
        let (text, interim) = composer_text(&prompt, "open the door");
        assert_eq!(text, "open the door");
        assert!(interim);
        let mut prompt = Prompt::new();
        prompt.set("look");
        let (text, interim) = composer_text(&prompt, "around");
        assert_eq!(text, "look around");
        assert!(interim);
    }

    #[test]
    fn focus_toggle() {
        let mut app = FullscreenApp::new("groknight", false, true);
        assert_eq!(app.focus, Focus::Prompt);
        app.toggle_focus();
        assert_eq!(app.focus, Focus::Scrollback);
        app.toggle_focus();
        assert_eq!(app.focus, Focus::Prompt);
    }

    #[test]
    fn scrollback_focus_has_a_text_indicator_without_shortcut_bar() {
        let mut app = FullscreenApp::new("groknight", false, false);
        app.toggle_focus();
        let buffer = draw_app(&app, 60, 15);
        assert!(
            buffer_text(&buffer).contains("focus: scrollback"),
            "focus must be visible in text, not only border color"
        );
    }

    #[test]
    fn theme_cycle() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.cycle_theme();
        assert_eq!(app.theme_name, "grokday");
        app.cycle_theme();
        assert_eq!(app.theme_name, "mono");
        app.cycle_theme();
        assert_eq!(app.theme_name, "groknight");
    }

    #[test]
    fn scrollback_push_via_app() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.push_block(Block::Dm { text: "hi".into() });
        assert_eq!(app.scrollback.len(), 1);
        assert_eq!(app.last_dm_text, "hi");
    }

    #[test]
    fn submit_prompt_sets_pending_and_history() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.prompt.insert_str("  look around  ");
        app.submit_prompt();
        assert_eq!(app.take_pending().as_deref(), Some("look around"));
        assert!(app.prompt.is_empty());
        assert!(app.take_pending().is_none());
        assert_eq!(app.history.last(), Some("look around"));
    }

    #[test]
    fn submit_empty_ignored() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.submit_prompt();
        assert!(app.pending_action.is_none());
    }

    #[test]
    fn load_reset_clears_session_context_and_keeps_narration() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.history.push("attack the goblin");
        app.last_action = Some("attack the goblin".into());
        app.last_dm_text = "The goblin falls.".into();

        app.reset_session_after_load(Some("A new road waits."));
        assert!(app.history.last().is_none());
        assert!(app.last_action.is_none());
        assert_eq!(app.last_dm_text, "A new road waits.");

        app.last_dm_text = "stale".into();
        app.reset_session_after_load(None);
        assert!(app.last_dm_text.is_empty());
    }

    #[test]
    fn session_usage_starts_empty_and_survives_load_reset() {
        let mut app = FullscreenApp::new("groknight", false, true);
        assert_eq!(format_session_usage(&app.usage), "No API calls yet.");

        app.usage.record_turn(&crate::dm::UsageTotals {
            prompt_tokens: 10,
            completion_tokens: 2,
            total_tokens: 12,
            requests: 1,
        });
        assert_eq!(app.usage.turns, 1);
        assert_eq!(app.usage.total_tokens, 12);

        // `/load` resets campaign context, not the process's API spend.
        app.reset_session_after_load(Some("A new road waits."));
        assert_eq!(app.usage.turns, 1);
        assert!(format_session_usage(&app.usage).contains("Total tokens: 12"));
    }

    #[test]
    fn esc_clears_draft_and_never_quits() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.prompt.insert_str("draft");
        app.on_esc();
        assert!(!app.should_quit);
        assert_eq!(app.focus, Focus::Prompt);
        assert!(app.prompt.is_empty());
        app.on_esc();
        assert!(!app.should_quit, "an empty prompt must not quit");
    }

    #[test]
    fn esc_from_scrollback_returns_to_prompt_first() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.prompt.insert_str("draft");
        app.toggle_focus();
        assert_eq!(app.focus, Focus::Scrollback);
        app.on_esc();
        assert_eq!(app.focus, Focus::Prompt);
        assert_eq!(app.prompt.input(), "draft", "draft survives focus return");
        assert!(!app.should_quit);
        app.on_esc();
        assert!(app.prompt.is_empty());
    }

    #[test]
    fn ctrl_editing_commands() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.prompt.set("hello world");

        assert!(app.on_ctrl_edit('a'));
        assert_eq!(app.prompt.cursor(), 0);
        assert!(app.on_ctrl_edit('e'));
        assert_eq!(app.prompt.cursor(), 11);

        app.prompt.set("hello world");
        app.prompt.move_home();
        app.prompt.move_right();
        app.prompt.move_right();
        assert!(app.on_ctrl_edit('u'));
        assert_eq!(app.prompt.input(), "llo world");
        assert_eq!(app.prompt.cursor(), 0);

        // Cursor starts at the end after `set`, so Ctrl+K is a no-op.
        app.prompt.set("hello world");
        assert!(app.on_ctrl_edit('k'));
        assert_eq!(app.prompt.input(), "hello world");
        app.prompt.move_home();
        app.prompt.move_right();
        app.prompt.move_right();
        assert!(app.on_ctrl_edit('k'));
        assert_eq!(app.prompt.input(), "he");

        app.prompt.set("attack the goblin");
        assert!(app.on_ctrl_edit('w'));
        assert_eq!(app.prompt.input(), "attack the ");

        app.prompt.set("hello");
        app.prompt.move_home();
        assert!(app.on_ctrl_edit('d'));
        assert_eq!(app.prompt.input(), "ello");

        assert!(!app.on_ctrl_edit('z'), "unknown Ctrl keys are ignored");
    }

    #[test]
    fn ctrl_l_clears_scrollback_from_either_focus() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.push_block(Block::Dm { text: "hi".into() });
        app.toggle_focus();
        assert_eq!(app.focus, Focus::Scrollback);
        assert!(app.on_ctrl_edit('L'));
        assert!(app.scrollback.is_empty());
        assert_eq!(app.focus, Focus::Scrollback, "Ctrl+L does not steal focus");
    }

    #[test]
    fn paste_goes_into_the_composer_as_one_line() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.toggle_focus();
        app.on_paste("look\r\naround\nnow\tplease");
        assert_eq!(app.focus, Focus::Prompt);
        assert_eq!(app.prompt.input(), "look around now please");
        assert!(app.pending_action.is_none(), "paste never submits");
    }

    #[test]
    fn history_up_when_empty() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.history.push("go north");
        app.history.push("look");
        assert!(app.history_up());
        assert_eq!(app.prompt.input(), "look");
        assert!(app.history_up());
        assert_eq!(app.prompt.input(), "go north");
        assert!(app.history_down());
        assert_eq!(app.prompt.input(), "look");
        assert!(app.history_down());
        assert_eq!(app.prompt.input(), "");
    }

    #[test]
    fn history_up_recalls_over_draft_and_restores_it() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.history.push("old action");
        app.prompt.insert_str("partial draft");
        assert!(app.history_up());
        assert_eq!(app.prompt.input(), "old action");
        assert!(app.history_down());
        assert_eq!(app.prompt.input(), "partial draft");
    }

    #[test]
    fn slash_completion_flow() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.prompt.insert_str("/sa");
        app.sync_completions();
        assert!(app.completions.is_open());
        assert_eq!(app.completions.selected_item().unwrap().name, "save");
        assert!(app.apply_completion());
        assert_eq!(app.prompt.input(), "/save ");
        assert!(!app.completions.is_open());

        // Non-slash text never opens the menu.
        app.prompt.set("look around");
        app.sync_completions();
        assert!(!app.completions.is_open());
        assert!(!app.apply_completion());
    }

    #[test]
    fn draw_renders_completion_dropdown() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = FullscreenApp::new("groknight", false, true);
        app.status_line = status::StatusLine::plain("HP 20/20");
        app.prompt.insert_str("/sa");
        app.sync_completions();
        assert!(app.completions.is_open());

        let backend = TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("/save"), "dropdown missing /save: {text}");
        assert!(text.contains("Save the game"));
        assert!(text.contains("/sa"), "composer missing input: {text}");
    }

    #[test]
    fn esc_dismisses_completions_before_clearing() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.prompt.insert_str("/s");
        app.sync_completions();
        assert!(app.completions.is_open());
        app.on_esc();
        assert!(!app.completions.is_open());
        assert_eq!(app.prompt.input(), "/s");
        assert!(!app.should_quit);
    }

    #[test]
    fn cursor_keys_on_prompt() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.prompt.insert_str("abc");
        app.on_home();
        assert_eq!(app.prompt.cursor(), 0);
        app.on_right();
        assert_eq!(app.prompt.cursor(), 1);
        app.on_char('X');
        assert_eq!(app.prompt.input(), "aXbc");
        app.on_delete();
        assert_eq!(app.prompt.input(), "aXc");
        app.on_end();
        assert_eq!(app.prompt.cursor(), 3);
    }

    #[test]
    fn strip_md() {
        assert_eq!(markdown::strip_inline("a **bold** z"), "a bold z");
    }

    const HOSTILE: &str = "\x1b]52;c;cGF5bG9hZA==\x07\x1b[2J\u{202e}\u{2067}\u{200b}";

    #[test]
    fn composer_sanitizes_draft_and_live_transcript() {
        let mut prompt = Prompt::new();
        // `set` is used for STT prefills/history restore, so bypass insertion
        // filtering to prove the display path is safe on its own.
        prompt.set(format!("look {HOSTILE}"));
        let (text, interim) = composer_text(&prompt, &format!("run {HOSTILE}"));
        assert!(interim);
        assert!(text.contains("look"));
        assert!(text.contains("run"));
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\x07'));
        assert!(!text.contains('\u{202e}'));
        assert!(!text.contains('\u{2067}'));
        assert!(!text.contains('\u{200b}'));

        // A live line containing only control bytes must not switch the
        // composer into interim mode.
        let empty_prompt = Prompt::new();
        let (text, interim) = composer_text(&empty_prompt, HOSTILE);
        assert!(!interim);
        assert_eq!(text, "");
    }

    #[test]
    fn fullscreen_block_lines_build_clean_span_text() {
        let colors = Colors::mono();
        let blocks = [
            Block::Player {
                text: format!("search {HOSTILE}"),
                spoken: false,
            },
            Block::Dm {
                text: format!("A **dark** hall. {HOSTILE}"),
            },
            Block::Tools {
                calls: vec![crate::ui::blocks::ToolCallView {
                    name: format!("roll{HOSTILE}"),
                    result_summary: format!("14 {HOSTILE}"),
                }],
            },
            Block::System {
                text: format!("error {HOSTILE}"),
                kind: SystemKind::Error,
            },
            Block::Dice {
                text: format!("**rolled** {HOSTILE}"),
            },
        ];
        for block in &blocks {
            for expanded in [false, true] {
                let lines = block_to_lines(block, expanded, &colors);
                let text: String = lines
                    .iter()
                    .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
                    .collect();
                assert!(!text.contains('\u{1b}'), "ESC in {text:?}");
                assert!(!text.contains('\x07'), "BEL in {text:?}");
                assert!(!text.contains('\u{202e}'), "bidi in {text:?}");
                assert!(!text.contains('\u{2067}'), "bidi isolate in {text:?}");
                assert!(!text.contains('\u{200b}'), "zero-width in {text:?}");
            }
        }
    }

    #[test]
    fn draw_neutralizes_hostile_status_and_prompt() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = FullscreenApp::new("mono", false, true);
        app.status_line = status::StatusLine::plain(format!("HP 20/20 {HOSTILE}"));
        app.prompt.set(format!("look {HOSTILE}"));
        app.live_line = format!("run {HOSTILE}");
        app.push_block(Block::System {
            text: format!("error {HOSTILE}"),
            kind: SystemKind::Error,
        });

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("HP 20/20"));
        assert!(text.contains("look"));
        assert!(!text.contains('\u{1b}'), "ESC rendered: {text:?}");
        assert!(!text.contains('\x07'), "BEL rendered: {text:?}");
        assert!(!text.contains("52;c"), "OSC payload rendered: {text:?}");
        assert!(!text.contains('\u{202e}'), "bidi rendered: {text:?}");
        assert!(
            !text.contains('\u{2067}'),
            "bidi isolate rendered: {text:?}"
        );
        assert!(!text.contains('\u{200b}'), "zero-width rendered: {text:?}");
    }

    // ---- Regression fixtures for the fullscreen layout bugs -------------

    fn draw_app(app: &FullscreenApp, width: u16, height: u16) -> ratatui::buffer::Buffer {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn draw_app_with_cursor(
        app: &FullscreenApp,
        width: u16,
        height: u16,
    ) -> (ratatui::buffer::Buffer, ratatui::layout::Position) {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        (terminal.backend().buffer().clone(), cursor)
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        buffer.content().iter().map(|c| c.symbol()).collect()
    }

    fn buffer_rows(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
        let width = buffer.area.width as usize;
        if width == 0 {
            return Vec::new();
        }
        buffer
            .content()
            .chunks(width)
            .map(|cells| cells.iter().map(|c| c.symbol()).collect())
            .collect()
    }

    /// Three-paragraph DM block plus three State deltas (as the turn
    /// composition now emits them). Each paragraph ends with a marker.
    fn transcript_app() -> FullscreenApp {
        let mut app = FullscreenApp::new("groknight", false, false);
        let p1 = format!("{} P1MARK.", "The first paragraph keeps going. ".repeat(9));
        let p2 = format!("{} P2MARK.", "The second paragraph keeps going. ".repeat(9));
        let p3 = format!(
            "{} The final sentence ends with FINAL-SENTENCE-MARKER.",
            "The third paragraph keeps going. ".repeat(9)
        );
        app.push_block(Block::Dm {
            text: format!("{p1}\n\n{p2}\n\n{p3}"),
        });
        app.push_block(Block::State {
            text:
                "💔 -2 HP (18/20) DELTA-ONE\n🚶 Hall → Crypt DELTA-TWO\n🎒 +Iron Dagger DELTA-THREE"
                    .into(),
        });
        app
    }

    fn assert_tail_visible(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) {
        let text = buffer_text(buffer);
        assert!(
            text.contains("FINAL-SENTENCE-MARKER"),
            "{width}x{height} clipped the final sentence: {text:?}"
        );
        for delta in ["DELTA-ONE", "DELTA-TWO", "DELTA-THREE"] {
            assert!(
                text.contains(delta),
                "{width}x{height} clipped {delta}: {text:?}"
            );
        }
    }

    #[test]
    fn draw_80x24_shows_newest_wrapped_rows() {
        let app = transcript_app();
        let buffer = draw_app(&app, 80, 24);
        assert_tail_visible(&buffer, 80, 24);
    }

    #[test]
    fn draw_40x20_shows_newest_wrapped_rows() {
        let app = transcript_app();
        let buffer = draw_app(&app, 40, 20);
        assert_tail_visible(&buffer, 40, 20);
    }

    #[test]
    fn draw_200x60_shows_all_paragraphs() {
        let app = transcript_app();
        let buffer = draw_app(&app, 200, 60);
        let text = buffer_text(&buffer);
        for marker in ["P1MARK", "P2MARK", "FINAL-SENTENCE-MARKER"] {
            assert!(text.contains(marker), "200x60 missing {marker}");
        }
        assert_tail_visible(&buffer, 200, 60);
    }

    #[test]
    fn draw_wraps_a_long_unbroken_url() {
        let mut app = FullscreenApp::new("groknight", false, false);
        let url = format!("https://example.com/{}", "a".repeat(220));
        app.push_block(Block::Dm {
            text: format!("The only exit is labelled {url}/URL-END-MARKER."),
        });
        let buffer = draw_app(&app, 80, 24);
        let text = buffer_text(&buffer);
        assert!(
            text.contains("URL-END-MARKER"),
            "URL tail clipped: {text:?}"
        );
        let rows = buffer_rows(&buffer);
        assert!(
            rows.iter().any(|row| row.matches('a').count() > 50),
            "long URL should fill a wrapped row: {rows:?}"
        );
    }

    #[test]
    fn draw_80x24_keeps_player_narration_state_tools_order() {
        let mut app = FullscreenApp::new("groknight", false, false);
        app.push_block(Block::Player {
            text: "open the door".into(),
            spoken: false,
        });
        app.push_block(Block::Dm {
            text: "The door groans open.".into(),
        });
        app.push_block(Block::Dice {
            text: "Rolled 12".into(),
        });
        app.push_block(Block::State {
            text: "💔 -2 HP (18/20)\n🎒 +Iron Dagger".into(),
        });
        app.push_block(Block::Tools {
            calls: vec![crate::ui::blocks::ToolCallView {
                name: "move_to_room".into(),
                result_summary: "Moved".into(),
            }],
        });

        let buffer = draw_app(&app, 80, 24);
        let rows = buffer_rows(&buffer);
        let at = |needle: &str| {
            rows.iter()
                .position(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("missing {needle}: {rows:?}"))
        };
        let player = at("open the door");
        let narration = at("The door groans open.");
        let dice = at("Rolled 12");
        let state = at("💔");
        let tools = at("Tools (1)");
        assert!(
            player < narration && narration < dice && dice < state && state < tools,
            "turn order drifted: player {player}, narration {narration}, dice {dice}, state {state}, tools {tools}"
        );
    }

    #[test]
    fn wrap_line_preserves_styles_and_hanging_indent() {
        let red = Style::default().fg(Color::Red);
        let blue = Style::default().fg(Color::Blue);
        let line = Line::from(vec![
            Span::styled("  alpha beta ".to_string(), red),
            Span::styled("gamma delta epsilon".to_string(), blue),
        ]);
        let rows = wrap_line(line, 12);
        assert!(rows.len() > 1);
        assert_eq!(rows[0].spans[0].style.fg, Some(Color::Red));
        for row in rows.iter().skip(1) {
            let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(text.starts_with("  "), "no hanging indent: {text:?}");
        }
        let joined: String = rows
            .iter()
            .flat_map(|row| row.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(joined.contains("alpha") && joined.contains("epsilon"));
    }

    #[test]
    fn scroll_offset_is_measured_in_visual_rows() {
        let mut app = FullscreenApp::new("groknight", false, false);
        let body: Vec<String> = (0..80)
            .map(|i| format!("LINE-{i:03} {}", "x".repeat(100)))
            .collect();
        app.push_block(Block::Dm {
            text: body.join("\n"),
        });
        let bottom = draw_app(&app, 80, 24);
        assert!(buffer_text(&bottom).contains("LINE-079"));

        app.scrollback.scroll_up(40);
        let scrolled = draw_app(&app, 80, 24);
        let text = buffer_text(&scrolled);
        assert!(
            !text.contains("LINE-079"),
            "bottom should scroll away: {text:?}"
        );
        assert!(
            text.contains("LINE-06"),
            "scrolled visual rows should show older lines: {text:?}"
        );
    }

    #[test]
    fn status_hp_urgency_changes_rendered_color() {
        if !theme::colors_enabled() {
            return;
        }
        let mut healthy = FullscreenApp::new("groknight", false, false);
        let mut state = crate::game::state::GameState::new("Hero");
        healthy.status_line = status::StatusLine::from_state(&state);
        let healthy_buffer = draw_app(&healthy, 80, 12);

        state.hp = 2;
        let mut wounded = FullscreenApp::new("groknight", false, false);
        wounded.status_line = status::StatusLine::from_state(&state);
        let wounded_buffer = draw_app(&wounded, 80, 12);

        // Status starts at (1,1) (one-cell margin).
        let healthy_cell = &healthy_buffer[(1, 1)];
        let wounded_cell = &wounded_buffer[(1, 1)];
        assert_eq!(healthy_cell.symbol(), "H");
        assert_eq!(wounded_cell.symbol(), "H");
        assert_eq!(
            healthy_cell.fg,
            Color::Rgb(130, 225, 160),
            "20/20 should be healthy green"
        );
        assert_eq!(
            wounded_cell.fg,
            Color::Rgb(255, 135, 135),
            "2/20 should be critical red"
        );
        assert_ne!(healthy_cell.fg, wounded_cell.fg);
        assert_eq!(
            healthy_cell.bg,
            Color::Rgb(18, 19, 26),
            "groknight paints its own background"
        );
    }

    #[test]
    fn status_truncates_with_ellipsis_at_80_and_40_columns() {
        let mut app = FullscreenApp::new("groknight", false, false);
        let mut state = crate::game::state::GameState::new("Hero");
        state.hp = 2;
        state.current_room = "The Long Gallery Beneath the Crumbling West Tower".into();
        state.add_item("Torch", "Lit", 1);
        state.add_companion("Pip the Unreasonably Chatty Fairy", "fairy");
        app.status_line = status::StatusLine::from_state(&state);

        for width in [80u16, 40] {
            let buffer = draw_app(&app, width, 12);
            let status = buffer_rows(&buffer)[1].trim().to_string();
            assert!(status.starts_with("HP 2/20"), "{status:?}");
            assert!(status.contains('…'), "{width} columns: {status:?}");
            assert!(
                status::display_width(&status) <= width as usize,
                "{width} columns overflowed: {status:?}"
            );
        }
    }

    #[test]
    fn composer_long_ascii_shows_cursor_adjacent_tail() {
        let mut app = FullscreenApp::new("groknight", false, false);
        let action = format!("{} TAIL-MARKER", "walk north and search ".repeat(8));
        app.prompt.set(action);
        let (buffer, cursor) = draw_app_with_cursor(&app, 40, 20);
        let text = buffer_text(&buffer);
        assert!(text.contains("TAIL-MARKER"), "tail missing: {text:?}");
        assert!(
            text.contains('…'),
            "left clip should show ellipsis: {text:?}"
        );
        assert!(!text.contains("walk north and search walk north and search"));
        assert!(cursor.x > 20, "cursor should sit near the tail: {cursor:?}");
    }

    #[test]
    fn composer_cjk_cursor_follows_display_width() {
        let mut app = FullscreenApp::new("groknight", false, false);
        app.prompt.set("龍".repeat(20));
        let (buffer, cursor) = draw_app_with_cursor(&app, 40, 20);
        let text = buffer_text(&buffer);
        assert!(text.contains('…'), "wide text should clip left: {text:?}");
        // Old char-count math put the cursor at x = 23 (20 glyphs, not 40
        // cells). Display-width math must put it near the right edge, one cell
        // past the visible glyphs.
        assert!(cursor.x > 30, "CJK cursor too far left: {cursor:?}");
        assert_eq!(cursor.x, 37, "cursor after the visible glyphs: {cursor:?}");
    }

    #[test]
    fn theme_names_map_to_stable_palettes() {
        let night = Colors::for_theme_with("groknight", true);
        assert_eq!(night.background, Some(Color::Rgb(18, 19, 26)));
        assert_eq!(night.text, Color::Rgb(232, 230, 227));
        assert!(!night.mono);

        let day = Colors::for_theme_with("grokday", true);
        assert_eq!(day.background, Some(Color::Rgb(247, 243, 234)));
        assert_eq!(day.text, Color::Rgb(32, 30, 28));

        // Unknown names fall back to groknight; matching is case-insensitive.
        let unknown = Colors::for_theme_with("unknown", true);
        assert_eq!(unknown.background, night.background);
        let upper = Colors::for_theme_with("GROKDAY", true);
        assert_eq!(upper.background, day.background);

        let mono = Colors::for_theme_with("mono", true);
        assert_eq!(mono.background, None);
        assert!(mono.mono);
        let disabled = Colors::for_theme_with("groknight", false);
        assert_eq!(disabled.background, None);
        assert!(disabled.mono);
    }

    #[test]
    fn spinner_advances_at_four_fps_and_stays_one_cell() {
        assert_eq!(spinner_frame_at(0), SPINNER_FRAMES[0]);
        assert_eq!(spinner_frame_at(249), SPINNER_FRAMES[0]);
        assert_eq!(spinner_frame_at(250), SPINNER_FRAMES[1]);
        assert_eq!(spinner_frame_at(1_250), SPINNER_FRAMES[5]);
        assert_eq!(spinner_frame_at(1_500), SPINNER_FRAMES[0]);
        for frame in SPINNER_FRAMES {
            assert_eq!(status::display_width(frame), 1);
        }
    }

    #[test]
    fn activity_strip_renders_spinner_and_text() {
        let mut app = FullscreenApp::new("groknight", false, false);
        app.push_block(Block::Dm {
            text: "A quiet room.".into(),
        });
        app.activity = Some("Consulting the DM…".into());
        let buffer = draw_app(&app, 60, 14);
        let text = buffer_text(&buffer);
        assert!(text.contains("Consulting the DM…"), "{text:?}");
        assert!(
            SPINNER_FRAMES.iter().any(|frame| text.contains(frame)),
            "spinner missing: {text:?}"
        );
        // The story row must not move when the animation frame changes: the
        // strip owns exactly one fixed row and the rune is one cell wide.
        let story_row = buffer_rows(&buffer)
            .iter()
            .position(|row| row.contains("A quiet room."))
            .unwrap();
        app.activity = Some("Speaking…".into());
        let later = draw_app(&app, 60, 14);
        assert_eq!(
            buffer_rows(&later)
                .iter()
                .position(|row| row.contains("A quiet room.")),
            Some(story_row),
            "spinner reflowed the transcript"
        );
    }
}
