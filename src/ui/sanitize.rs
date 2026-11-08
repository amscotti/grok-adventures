//! Sanitization for untrusted text that is about to be written to a terminal.
//!
//! Model narration, error bodies, save fields (player/item/room/companion
//! names), and speech-to-text transcripts are all untrusted: a payload can
//! embed ANSI escape sequences (OSC 52 clipboard writes, CSI clear-screen,
//! window-title changes, …), C0/C1 controls, or Unicode bidi formatting that
//! rewrites or spoofs the display.
//!
//! [`sanitize_terminal_text`] removes those constructs while preserving
//! ordinary prose, emoji, CJK, accents, embedded newlines/tabs, and markdown
//! markers such as `**bold**`. It is a text-to-text filter: it does not add
//! any escape sequences of its own, so callers can safely style the result
//! afterwards (see [`crate::ui::theme`]).
//!
//! Removing an escape sequence never consumes the rest of the document:
//! complete CSI/OSC/DCS/APC/PM/SOS sequences are skipped as a unit, malformed
//! or unterminated sequences are stripped to the end of the input, and a
//! stray ESC with no following byte is simply dropped.

use std::iter::Peekable;
use std::str::Chars;

const ESC: char = '\u{1b}';
const BEL: char = '\u{07}';
const ST: char = '\u{9c}';

/// Sanitize untrusted text for terminal display.
///
/// Preserved:
/// - newline (`\n`) and tab (`\t`)
/// - all ordinary Unicode text (emoji, CJK, accented characters)
/// - markdown markers such as `**` (they carry no terminal control meaning)
///
/// Removed:
/// - ESC and complete escape sequences: CSI (`ESC [`), OSC (`ESC ]`, BEL/ST
///   terminated), DCS/APC/PM/SOS (`ESC P`/`_`/`^`/`X`, ST terminated),
///   charset designations, and other short escapes
/// - C1 equivalents of the above introducers (U+009B, U+009D, U+0090,
///   U+0098, U+009E, U+009F) and a lone C1 ST (U+009C)
/// - C0 controls except `\n`/`\t`, plus DEL
/// - C1 controls U+0080–U+009F
/// - bidi embedding/override/isolate controls U+202A–U+202E and
///   U+2066–U+2069
/// - zero-width characters U+200B–U+200D and U+FEFF
/// - line/paragraph separators U+2028 and U+2029
///
/// Unterminated or malformed escape sequences are stripped through the end of
/// the input, so a hostile payload cannot hide trailing text behind a
/// half-finished sequence. The scan always advances, so it cannot loop.
pub fn sanitize_terminal_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == ESC {
            skip_escape(&mut chars);
            continue;
        }

        // C1 control equivalents are handled as sequence introducers so their
        // payload is removed rather than left behind as plain text.
        match ch {
            '\u{9b}' => skip_csi(&mut chars),
            '\u{9d}' => skip_osc(&mut chars),
            '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => skip_until_st(&mut chars),
            '\u{9c}' => {}
            _ if is_disallowed_terminal_char(ch) => {}
            _ => out.push(ch),
        }
    }

    out
}

/// Sanitize text for a single-line terminal context (status bar, composer,
/// completion row).
///
/// Runs [`sanitize_terminal_text`] and then flattens newlines/tabs to spaces
/// so an untrusted value cannot move the cursor to another line and spoof
/// surrounding chrome.
pub fn sanitize_terminal_line(text: &str) -> String {
    sanitize_terminal_text(text).replace(['\n', '\t'], " ")
}

/// Whether `ch` is dropped by [`sanitize_terminal_text`] when it appears
/// outside an escape sequence.
///
/// Newline and tab are explicitly allowed. This is exposed so input editors
/// can apply the same policy to single characters without re-listing the
/// Unicode ranges.
pub fn is_disallowed_terminal_char(ch: char) -> bool {
    if ch == '\n' || ch == '\t' {
        return false;
    }
    matches!(
        ch,
        '\u{0}'..='\u{1f}'
            | '\u{7f}'
            | '\u{80}'..='\u{9f}'
            | '\u{200b}'..='\u{200d}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
    )
}

/// Skip an escape sequence whose ESC has already been consumed.
fn skip_escape(chars: &mut Peekable<Chars<'_>>) {
    let Some(next) = chars.next() else {
        return;
    };
    match next {
        '[' => skip_csi(chars),
        ']' => skip_osc(chars),
        'P' | 'X' | '^' | '_' => skip_until_st(chars),
        // Charset designations and similar escapes: intermediate bytes
        // (0x20–0x2F) followed by a final byte (0x30–0x7E).
        '\u{20}'..='\u{2f}' => {
            while let Some(&ch) = chars.peek() {
                chars.next();
                if !matches!(ch, '\u{20}'..='\u{2f}') {
                    break;
                }
            }
        }
        // Two-character escape (e.g. `ESC c`, `ESC 7`).
        _ => {}
    }
}

/// Skip a CSI sequence (final byte 0x40–0x7E). An unterminated sequence is
/// skipped to the end of the input.
fn skip_csi(chars: &mut Peekable<Chars<'_>>) {
    while let Some(&ch) = chars.peek() {
        if matches!(ch, '\u{40}'..='\u{7e}') {
            chars.next();
            return;
        }
        // A control byte aborts the malformed sequence; the main loop then
        // drops (or interprets) it. Without this, a newline could be eaten.
        if ch.is_control() || ch == '\u{7f}' {
            return;
        }
        chars.next();
    }
}

/// Skip an OSC string terminated by BEL or ST (ESC `\` / U+009C). An
/// unterminated string is skipped to the end of the input.
fn skip_osc(chars: &mut Peekable<Chars<'_>>) {
    while let Some(ch) = chars.next() {
        match ch {
            BEL | ST => return,
            ESC if matches!(chars.peek(), Some('\\')) => {
                chars.next();
                return;
            }
            _ => {}
        }
    }
}

/// Skip a DCS/APC/PM/SOS string terminated by ST. An unterminated string is
/// skipped to the end of the input.
fn skip_until_st(chars: &mut Peekable<Chars<'_>>) {
    while let Some(ch) = chars.next() {
        match ch {
            ST => return,
            ESC if matches!(chars.peek(), Some('\\')) => {
                chars.next();
                return;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_normal_prose_unicode_and_markdown() {
        let text = "Café ☕ — 龍の洞窟 🐉 **bold** stays";
        assert_eq!(sanitize_terminal_text(text), text);
    }

    #[test]
    fn preserves_newline_and_tab() {
        assert_eq!(sanitize_terminal_text("a\nb\tc\r\nd"), "a\nb\tc\nd");
    }

    #[test]
    fn strips_osc_52_clipboard_bel_terminated() {
        let hostile = "before\x1b]52;c;SGVsbG8gZnJvbSB0aGUgZGFyayBzaWRl\x07after";
        let safe = sanitize_terminal_text(hostile);
        assert_eq!(safe, "beforeafter");
        assert!(!safe.contains('\u{1b}'));
        assert!(!safe.contains(BEL));
        assert!(!safe.contains("52;"));
    }

    #[test]
    fn strips_osc_52_st_terminated() {
        let hostile = "before\x1b]52;c;cGF5bG9hZA==\x1b\\after";
        assert_eq!(sanitize_terminal_text(hostile), "beforeafter");
    }

    #[test]
    fn strips_osc_window_title() {
        let hostile = "\x1b]0;owned by payload\x07visible";
        assert_eq!(sanitize_terminal_text(hostile), "visible");
    }

    #[test]
    fn strips_csi_clear_screen_and_sgr() {
        let hostile = "a\x1b[2Jb\x1b[1;31mRED\x1b[0mc";
        assert_eq!(sanitize_terminal_text(hostile), "abREDc");
        assert!(!sanitize_terminal_text(hostile).contains('\u{1b}'));
    }

    #[test]
    fn strips_bel_and_c0_del() {
        assert_eq!(
            sanitize_terminal_text("ring\x07\u{0}\u{7f}ring"),
            "ringring"
        );
    }

    #[test]
    fn strips_bidi_and_zero_width_controls() {
        let hostile = "safe\u{202e}evil\u{202c}\u{2067}x\u{2069}\u{200b}\u{200c}\u{200d}\u{feff}\u{2028}\u{2029}end";
        assert_eq!(sanitize_terminal_text(hostile), "safeevilxend");
    }

    #[test]
    fn strips_unterminated_osc_to_end() {
        assert_eq!(
            sanitize_terminal_text("safe\x1b]52;c;AAAA"),
            "safe",
            "unterminated OSC must not leave payload behind or hang"
        );
    }

    #[test]
    fn strips_unterminated_csi_and_dcs_to_end() {
        assert_eq!(sanitize_terminal_text("safe\x1b[31"), "safe");
        assert_eq!(sanitize_terminal_text("safe\x1bP1;2;3qpayload"), "safe");
    }

    #[test]
    fn strips_dcs_apc_pm_sos_and_short_escapes() {
        assert_eq!(
            sanitize_terminal_text("a\x1bPdcs\x1b\\b\x1b_apc\x1b\\c\x1b^pm\x1b\\d\x1bXsos\x1b\\e"),
            "abcde"
        );
        // Two-character escape and charset designation.
        assert_eq!(sanitize_terminal_text("a\x1bcb"), "ab");
        assert_eq!(sanitize_terminal_text("a\x1b(Bb"), "ab");
    }

    #[test]
    fn strips_c1_introducers_and_st() {
        assert_eq!(sanitize_terminal_text("a\u{9b}2Jb"), "ab");
        assert_eq!(sanitize_terminal_text("a\u{9d}0;title\u{9c}b"), "ab");
        assert_eq!(sanitize_terminal_text("a\u{90}payload\u{9c}b"), "ab");
        assert_eq!(sanitize_terminal_text("a\u{9c}b"), "ab");
    }

    #[test]
    fn lone_escape_is_dropped() {
        assert_eq!(sanitize_terminal_text("tail\x1b"), "tail");
    }

    #[test]
    fn single_line_sanitizer_flattens_breaks() {
        assert_eq!(
            sanitize_terminal_line("Room\nSpoofed\t> prompt"),
            "Room Spoofed > prompt"
        );
        assert_eq!(sanitize_terminal_line("A\x1b[2J\nB"), "A B");
    }

    #[test]
    fn malformed_csi_with_newline_does_not_eat_the_line_break() {
        assert_eq!(sanitize_terminal_text("a\x1b[31\nb"), "a\nb");
    }
}
