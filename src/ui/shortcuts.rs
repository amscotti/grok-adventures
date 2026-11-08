//! Bottom shortcuts bar for fullscreen TUI.

/// Default shortcuts hint shown under the prompt.
pub fn shortcuts_line() -> String {
    shortcuts_line_for(false, false, false, true)
}

/// Shortcuts with tools-expand hint when tools are collapsed.
pub fn shortcuts_line_with_tools(tools_expanded: bool) -> String {
    shortcuts_line_for(tools_expanded, false, false, true)
}

/// Shortcuts line for the current UI state.
///
/// `busy` (DM turn or speech in flight) advertises that typing still works
/// and Enter queues. `focus_scrollback` makes the focus visible in text and
/// offers `esc prompt`; otherwise a non-empty draft advertises `esc clear`.
/// Ctrl+C is always the explicit quit.
pub fn shortcuts_line_for(
    tools_expanded: bool,
    busy: bool,
    focus_scrollback: bool,
    prompt_empty: bool,
) -> String {
    if busy {
        let mut parts = vec![
            "typing while DM works — Enter queues".to_string(),
            "esc cancel turn".to_string(),
            "ctrl+c quit".to_string(),
        ];
        if focus_scrollback {
            parts.push("focus: scrollback".to_string());
        }
        return parts.join("  ·  ");
    }

    let tools = if tools_expanded { "tools on" } else { "/tools" };
    let mut parts = vec![
        "enter send".to_string(),
        "↑ history".to_string(),
        "tab complete".to_string(),
        "ctrl+space/f2 talk".to_string(),
        "/help".to_string(),
        tools.to_string(),
    ];
    if focus_scrollback {
        parts.push("focus: scrollback".to_string());
        parts.push("esc prompt".to_string());
    } else if !prompt_empty {
        parts.push("esc clear".to_string());
    }
    parts.push("ctrl+c quit".to_string());
    parts.join("  ·  ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_shortcuts_string() {
        let s = shortcuts_line();
        assert!(s.contains("enter"));
        assert!(s.contains("tab"));
        assert!(s.contains("help"));
        assert!(s.contains("f2 talk") || s.contains("f2 hold"));
        assert!(s.contains("↑ hist") || s.contains("hist"));
        assert!(s.contains("ctrl+c quit"));
        assert!(!s.contains("esc quit"), "Esc never quits: {s}");
    }

    #[test]
    fn tools_hint_varies() {
        assert!(
            shortcuts_line_with_tools(false).contains("/tools")
                || shortcuts_line_with_tools(false).contains("tools")
        );
        assert!(shortcuts_line_with_tools(true).contains("tools on"));
        assert!(shortcuts_line_with_tools(false).contains("f2"));
    }

    #[test]
    fn busy_mentions_cancel_queue_and_quit() {
        let s = shortcuts_line_for(false, true, false, true);
        assert!(s.contains("esc cancel turn"));
        assert!(s.contains("ctrl+c quit"));
        assert!(s.contains("typing while DM works"));
        assert!(s.contains("Enter queues"));
        assert!(!s.contains("esc quit"));
    }

    #[test]
    fn non_empty_prompt_offers_esc_clear() {
        let s = shortcuts_line_for(false, false, false, false);
        assert!(s.contains("esc clear"), "{s}");
        assert!(!s.contains("esc prompt"));
    }

    #[test]
    fn scrollback_focus_is_visible_in_text() {
        let s = shortcuts_line_for(false, false, true, true);
        assert!(s.contains("focus: scrollback"), "{s}");
        assert!(s.contains("esc prompt"), "{s}");
        assert!(!s.contains("esc clear"));
    }
}
