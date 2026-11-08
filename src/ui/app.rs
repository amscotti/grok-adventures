//! Fullscreen app state and ratatui drawing.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as UiBlock, BorderType, Borders, Paragraph, Wrap};

use crate::ui::blocks::{Block, SystemKind};
use crate::ui::completion::{CompletionMenu, MAX_VISIBLE, suggestion_line};
use crate::ui::markdown;
use crate::ui::prompt::{Prompt, PromptHistory};
use crate::ui::scrollback::Scrollback;
use crate::ui::shortcuts;
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
    pub status_line: String,
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
            status_line: String::new(),
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
        }
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

    /// Esc: dismiss autocomplete, then clear input, then request quit.
    pub fn on_esc(&mut self) {
        if self.completions.is_open() {
            self.completions
                .dismiss(self.prompt.input(), self.prompt.cursor());
            return;
        }
        if self.focus == Focus::Prompt && !self.prompt.is_empty() {
            self.history.abandon_browse();
            self.prompt.clear();
        } else {
            self.should_quit = true;
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
        let style = Style::default().fg(colors.status_fg);
        let line = Paragraph::new(Line::from(Span::styled(
            truncate_to_width(&self.status_line, area.width as usize),
            style,
        )));
        frame.render_widget(line, area);
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

        let lines = self.build_scrollback_lines(colors);
        let height = inner.height as usize;
        let total = lines.len();
        let scroll = self.scrollback.scroll() as usize;
        // Pin to bottom: show last `height` lines, shifted up by scroll.
        let end = total.saturating_sub(scroll);
        let start = end.saturating_sub(height);
        let visible: Vec<Line> = lines.into_iter().skip(start).take(height).collect();

        let para = Paragraph::new(visible).wrap(Wrap { trim: false });
        frame.render_widget(para, inner);
    }

    fn build_scrollback_lines(&self, colors: &Colors) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for entry in self.scrollback.entries() {
            lines.extend(block_to_lines(&entry.block, self.tools_expanded, colors));
            // Blank separator after each block (skipped in compact density).
            if !self.compact {
                lines.push(Line::from(""));
            }
        }
        lines
    }

    fn draw_activity(&self, frame: &mut Frame, area: Rect, colors: &Colors) {
        let text = self.activity.as_deref().unwrap_or("");
        let para = Paragraph::new(Line::from(Span::styled(
            format!(" {text}"),
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
        let block = UiBlock::default()
            .borders(borders)
            .border_type(BorderType::Rounded)
            .border_style(border_style);

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
        let cols = if interim {
            body_text.chars().count().min(u16::MAX as usize) as u16
        } else {
            self.prompt.cursor_display_cols()
        };
        let body = Span::styled(body_text, body_style);
        let para = Paragraph::new(Line::from(vec![prefix, body])).block(block);
        frame.render_widget(para, area);

        // Place terminal cursor at the edit position when prompt is focused.
        if self.focus == Focus::Prompt {
            let x = inner.x.saturating_add(2).saturating_add(cols);
            let y = inner.y;
            // Clamp within inner width.
            let max_x = inner.x.saturating_add(inner.width.saturating_sub(1));
            frame.set_cursor_position((x.min(max_x), y));
        }
    }

    fn draw_shortcuts(&self, frame: &mut Frame, area: Rect, colors: &Colors) {
        let busy = self.activity.is_some();
        let text = shortcuts::shortcuts_line_for(self.tools_expanded, busy);
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
    accent: Color,
    dm: Color,
    tool: Color,
    text: Color,
    muted: Color,
    error: Color,
    success: Color,
    activity: Color,
    status_fg: Color,
}

impl Colors {
    fn for_theme(name: &str) -> Self {
        if !theme::colors_enabled() {
            return Self::mono();
        }
        match name.to_ascii_lowercase().as_str() {
            "grokday" => Self {
                accent: Color::Rgb(120, 60, 180),
                dm: Color::Rgb(0, 100, 180),
                tool: Color::Rgb(160, 100, 0),
                text: Color::Black,
                muted: Color::DarkGray,
                error: Color::Red,
                success: Color::Green,
                activity: Color::Magenta,
                status_fg: Color::Black,
            },
            "mono" => Self::mono(),
            // groknight (default) — quiet light status on a dark terminal
            _ => Self {
                accent: Color::Rgb(180, 120, 255),
                dm: Color::Rgb(140, 200, 255),
                tool: Color::Rgb(255, 200, 100),
                text: Color::White,
                muted: Color::DarkGray,
                error: Color::Rgb(255, 90, 90),
                success: Color::Rgb(100, 220, 140),
                activity: Color::Magenta,
                status_fg: Color::Rgb(200, 200, 210),
            },
        }
    }

    fn mono() -> Self {
        Self {
            accent: Color::Reset,
            dm: Color::Reset,
            tool: Color::Reset,
            text: Color::Reset,
            muted: Color::Reset,
            error: Color::Reset,
            success: Color::Reset,
            activity: Color::Reset,
            status_fg: Color::Reset,
        }
    }
}

/// Composer text, and whether it is an in-progress transcript.
fn composer_text(prompt: &Prompt, live_line: &str) -> (String, bool) {
    if live_line.is_empty() {
        return (prompt.input().to_string(), false);
    }
    let draft = prompt.input().trim_end();
    if draft.is_empty() {
        (live_line.to_string(), true)
    } else {
        (format!("{draft} {live_line}"), true)
    }
}

fn block_to_lines(block: &Block, tools_expanded: bool, colors: &Colors) -> Vec<Line<'static>> {
    match block {
        Block::Player { text, spoken } => {
            let label = if *spoken { "You · voice" } else { "You" };
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
                let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
                lines.push(Line::from(Span::styled(
                    format!("  {}", names.join(", ")),
                    Style::default().fg(colors.muted),
                )));
            } else {
                for call in calls {
                    let summary = markdown::truncate_chars(&call.result_summary, 80);
                    lines.push(Line::from(vec![
                        Span::styled("  · ".to_string(), Style::default().fg(colors.muted)),
                        Span::styled(call.name.clone(), Style::default().fg(colors.tool)),
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
            vec![Line::from(Span::styled(text.clone(), style))]
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

fn truncate_to_width(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= width {
        s.to_string()
    } else {
        s.chars().take(width).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn esc_clears_then_quits() {
        let mut app = FullscreenApp::new("groknight", false, true);
        app.prompt.insert_str("draft");
        app.on_esc();
        assert!(!app.should_quit);
        assert!(app.prompt.is_empty());
        app.on_esc();
        assert!(app.should_quit);
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
        app.status_line = "HP 20/20".into();
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
}
