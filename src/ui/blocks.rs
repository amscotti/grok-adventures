//! Block model and minimal linear rendering.

use crate::ui::markdown;
use crate::ui::theme;

/// Kind of system message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemKind {
    Info,
    Error,
    Success,
}

/// One tool invocation summary for the Tools block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallView {
    pub name: String,
    pub result_summary: String,
}

/// A transcript block printed in minimal mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Player { text: String, spoken: bool },
    Dm { text: String },
    Tools { calls: Vec<ToolCallView> },
    State { text: String },
    System { text: String, kind: SystemKind },
    Dice { text: String },
}

/// Format a block as a multi-line string (no trailing newline on last line of
/// content is guaranteed; callers usually print with `println!`).
pub fn format_block(block: &Block, tools_expanded: bool) -> String {
    match block {
        Block::Player { text, spoken } => {
            let label = if *spoken { "You · voice" } else { "You" };
            let mut out = theme::player(label);
            out.push('\n');
            for line in text.lines() {
                out.push_str("  ");
                out.push_str(line);
                out.push('\n');
            }
            // trim trailing newline for consistency
            if out.ends_with('\n') {
                out.pop();
            }
            out
        }
        Block::Dm { text } => {
            let mut out = theme::dm("DM");
            out.push('\n');
            let rendered = markdown::render_inline(text);
            for line in rendered.lines() {
                out.push_str("  ");
                out.push_str(line);
                out.push('\n');
            }
            if out.ends_with('\n') {
                out.pop();
            }
            out
        }
        Block::Tools { calls } => format_tools(calls, tools_expanded),
        Block::State { text } => {
            let mut out = theme::dim("State");
            out.push('\n');
            for line in text.lines() {
                out.push_str("  ");
                out.push_str(&markdown::render_inline(line));
                out.push('\n');
            }
            if out.ends_with('\n') {
                out.pop();
            }
            out
        }
        Block::System { text, kind } => match kind {
            SystemKind::Info => theme::dim(text),
            SystemKind::Error => theme::error(text),
            SystemKind::Success => theme::success(text),
        },
        Block::Dice { text } => markdown::render_inline(text),
    }
}

fn format_tools(calls: &[ToolCallView], expanded: bool) -> String {
    if calls.is_empty() {
        return theme::tool("Tools");
    }
    let header = theme::tool(&format!("Tools ({})", calls.len()));
    if !expanded {
        // Collapsed: one summary line listing names
        let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
        return format!("{header}\n  {}", theme::dim(&names.join(", ")));
    }
    let mut out = header;
    for call in calls {
        let summary = markdown::truncate_chars(&call.result_summary, 80);
        out.push('\n');
        out.push_str("  · ");
        out.push_str(&theme::tool(&call.name));
        out.push_str(" → ");
        out.push_str(&theme::dim(&summary));
    }
    out
}

/// Print a block to stdout followed by a blank line.
pub fn print_block(block: &Block, tools_expanded: bool) {
    println!("{}", format_block(block, tools_expanded));
    println!();
}

/// Plain lines for the session header: title + version, then detail and hint.
pub fn session_header_lines(title: &str, detail: &str, hint: &str) -> Vec<String> {
    let mut lines = vec![format!("{title}  v{}", env!("CARGO_PKG_VERSION"))];
    if !detail.is_empty() {
        lines.push(detail.to_string());
    }
    if !hint.is_empty() {
        lines.push(hint.to_string());
    }
    lines
}

/// Print the session header: colored title, dim version, then detail and hint.
pub fn print_session_header(title: &str, detail: &str, hint: &str) {
    println!();
    let mut lines = session_header_lines(title, detail, hint).into_iter();
    if lines.next().is_some() {
        println!(
            "{}  {}",
            theme::player(title),
            theme::dim(&format!("v{}", env!("CARGO_PKG_VERSION")))
        );
    }
    for line in lines {
        println!("{}", theme::dim(&line));
    }
    println!();
}

/// Dim horizontal rule.
pub fn print_rule() {
    println!("{}", theme::dim(&"─".repeat(60)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_player_block() {
        let b = Block::Player {
            text: "search the room".into(),
            spoken: false,
        };
        let s = format_block(&b, false);
        assert!(s.contains("You"));
        assert!(s.contains("search the room"));
    }

    #[test]
    fn format_tools_collapsed_and_expanded() {
        let calls = vec![
            ToolCallView {
                name: "roll_dice".into(),
                result_summary: "14".into(),
            },
            ToolCallView {
                name: "modify_hp".into(),
                result_summary: "💔 Took 2 damage".into(),
            },
        ];
        let b = Block::Tools { calls };
        let collapsed = format_block(&b, false);
        assert!(collapsed.contains("Tools (2)"));
        assert!(collapsed.contains("roll_dice"));

        let expanded = format_block(&b, true);
        assert!(expanded.contains("→"));
        assert!(expanded.contains("14") || expanded.contains("roll_dice"));
    }

    #[test]
    fn format_dm_renders_bold() {
        let b = Block::Dm {
            text: "A **dark** hall.".into(),
        };
        let s = format_block(&b, false);
        assert!(s.contains("DM"));
        assert!(s.contains("dark"));
        assert!(!s.contains("**dark**"));
    }

    #[test]
    fn session_header_lists_model_and_version() {
        let lines = session_header_lines(
            "Grok Adventures",
            "model grok-4.7 · low",
            "/help for commands",
        );
        assert!(lines[0].starts_with("Grok Adventures  v"));
        assert!(lines[0].contains(env!("CARGO_PKG_VERSION")));
        assert_eq!(lines[1], "model grok-4.7 · low");
        assert_eq!(lines[2], "/help for commands");
    }

    #[test]
    fn format_system_kinds() {
        let info = format_block(
            &Block::System {
                text: "hello".into(),
                kind: SystemKind::Info,
            },
            false,
        );
        assert!(info.contains("hello"));
        let err = format_block(
            &Block::System {
                text: "boom".into(),
                kind: SystemKind::Error,
            },
            false,
        );
        assert!(err.contains("boom"));
    }
}
