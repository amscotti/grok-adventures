//! Bottom shortcuts bar for fullscreen TUI.

/// Default shortcuts hint shown under the prompt.
pub fn shortcuts_line() -> String {
    shortcuts_line_for(false, false)
}

/// Shortcuts with tools-expand hint when tools are collapsed.
pub fn shortcuts_line_with_tools(tools_expanded: bool) -> String {
    shortcuts_line_for(tools_expanded, false)
}

/// Shortcuts line; when `busy` (DM turn in flight), Esc cancels the turn.
pub fn shortcuts_line_for(tools_expanded: bool, busy: bool) -> String {
    let tools = if tools_expanded { "tools on" } else { "/tools" };
    let esc = if busy { "esc cancel turn" } else { "esc quit" };
    format!(
        "enter send  ·  ↑ history  ·  tab complete  ·  ctrl+space/f2 talk  ·  /help  ·  {tools}  ·  {esc}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_shortcuts_string() {
        let s = shortcuts_line();
        assert!(s.contains("enter"));
        assert!(s.contains("tab"));
        assert!(s.contains("esc quit"));
        assert!(s.contains("help"));
        assert!(s.contains("f2 talk") || s.contains("f2 hold"));
        assert!(s.contains("↑ hist") || s.contains("hist"));
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
    fn busy_mentions_cancel() {
        let s = shortcuts_line_for(false, true);
        assert!(s.contains("esc cancel turn"));
        assert!(!s.contains("esc quit"));
    }
}
