//! Minimal inline markdown rendering for DM narration.

use crate::ui::theme;

/// Convert a minimal subset of inline markdown to ANSI.
///
/// Currently supports `**bold**`. When `NO_COLOR` is set, bold markers are
/// stripped and plain text is returned.
pub fn render_inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    let use_ansi = theme::colors_enabled();

    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'*' {
            // Find closing **
            if let Some(rel) = text[i + 2..].find("**") {
                let content = &text[i + 2..i + 2 + rel];
                if use_ansi {
                    // ANSI bold on, content, bold off
                    out.push_str("\x1b[1m");
                    out.push_str(content);
                    out.push_str("\x1b[22m");
                } else {
                    out.push_str(content);
                }
                i = i + 2 + rel + 2;
                continue;
            }
        }
        // Safe: we walk by char boundaries via finding ** or single bytes of ASCII;
        // for non-ASCII take one char.
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Strip the supported inline markers (`**bold**`) without ANSI styling.
///
/// Shared by the fullscreen TUI (which applies its own ratatui styles) so it
/// does not duplicate the `**` scan in `render_inline`.
pub fn strip_inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len()
            && bytes[i] == b'*'
            && bytes[i + 1] == b'*'
            && let Some(rel) = text[i + 2..].find("**")
        {
            out.push_str(&text[i + 2..i + 2 + rel]);
            i = i + 2 + rel + 2;
            continue;
        }
        // `i` is always at a char boundary (we advance by whole chars or
        // past a matched `**` pair on ASCII bytes).
        let ch = text[i..].chars().next().unwrap_or('\u{FFFD}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Truncate to at most `max` chars (newlines stripped). Appends `...`
/// only when there is room; for tiny `max` just takes the first chars so
/// the result never exceeds `max`.
pub fn truncate_chars(s: &str, max: usize) -> String {
    let cleaned: String = s.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    if cleaned.chars().count() <= max {
        cleaned
    } else if max <= 3 {
        cleaned.chars().take(max).collect()
    } else {
        let mut t: String = cleaned.chars().take(max - 3).collect();
        t.push_str("...");
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold_strips_or_styles() {
        let s = render_inline("Hello **world**!");
        assert!(s.contains("world"));
        assert!(!s.contains("**world**"));
        // Either ANSI bold or plain stripped text
        assert!(s.contains("Hello ") && s.contains("!"));
    }

    #[test]
    fn no_bold_passthrough() {
        let s = render_inline("plain text");
        assert_eq!(s, "plain text");
    }

    #[test]
    fn unmatched_stars_left() {
        let s = render_inline("almost **bold");
        assert!(s.contains("**bold"));
    }

    #[test]
    fn multiple_bold_segments() {
        let s = render_inline("**a** and **b**");
        assert!(s.contains('a'));
        assert!(s.contains('b'));
        assert!(!s.contains("**a**"));
    }

    #[test]
    fn strip_inline_matches_no_color_render() {
        assert_eq!(strip_inline("a **bold** z"), "a bold z");
        assert_eq!(strip_inline("almost **bold"), "almost **bold");
        assert_eq!(strip_inline("**a** and **b**"), "a and b");
    }

    #[test]
    fn truncate_chars_never_exceeds_max() {
        assert_eq!(truncate_chars("abcde", 80), "abcde");
        assert_eq!(truncate_chars("a\nb\rc", 80), "abc");
        let t = truncate_chars("abcdefghij", 8);
        assert_eq!(t.chars().count(), 8);
        assert!(t.ends_with("..."));
        // Tiny max: no ellipsis overflow.
        assert_eq!(truncate_chars("abcde", 2), "ab");
        assert_eq!(truncate_chars("abcde", 0), "");
    }
}
