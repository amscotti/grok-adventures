//! GrokNight theme helpers via `owo-colors`.
//!
//! All helpers respect `NO_COLOR` (when set and non-empty, colors are disabled).
//!
//! Minimal mode is dark-terminal only. The palette below is tuned for a
//! near-black background (`#12131a`); WCAG AA contrast ratios against that
//! background are:
//!
//! - player accent `#c8a0ff` — 8.76:1
//! - DM narration `#8ccdff` — 10.84:1
//! - tools `#ffcd6e` — 12.53:1
//! - error `#ff8787` — 8.00:1
//! - success `#82e1a0` — 11.67:1
//! - muted `#aaaeb9` — 8.35:1 (was dimmed DarkGray, ~3.5:1)

use owo_colors::{OwoColorize, Style};

/// RGB for healthy HP (also the dark-theme success accent).
const HP_HIGH: (u8, u8, u8) = (130, 225, 160);
/// RGB for wounded HP.
const HP_MID: (u8, u8, u8) = (255, 205, 110);
/// RGB for critical HP.
const HP_LOW: (u8, u8, u8) = (255, 135, 135);

/// Whether ANSI color should be emitted.
pub fn colors_enabled() -> bool {
    !matches!(std::env::var("NO_COLOR"), Ok(v) if !v.is_empty())
}

/// Apply `style` when colors are enabled; otherwise return plain text.
pub fn paint(text: &str, style: Style) -> String {
    if colors_enabled() {
        format!("{}", text.style(style))
    } else {
        text.to_string()
    }
}

/// Player accent (magenta / cyan family for GrokNight).
pub fn player_style() -> Style {
    Style::new().fg_rgb::<200, 160, 255>().bold()
}

/// DM narration accent.
pub fn dm_style() -> Style {
    Style::new().fg_rgb::<140, 205, 255>()
}

/// Tool / mechanics accent.
pub fn tool_style() -> Style {
    Style::new().fg_rgb::<255, 205, 110>()
}

/// Error accent.
pub fn error_style() -> Style {
    Style::new().fg_rgb::<255, 135, 135>().bold()
}

/// Success accent.
pub fn success_style() -> Style {
    Style::new().fg_rgb::<130, 225, 160>()
}

/// Muted secondary text.
pub fn muted() -> Style {
    Style::new().fg_rgb::<170, 174, 185>()
}

/// Style for primary body text.
pub fn primary() -> Style {
    Style::new()
}

/// HP color by percentage of max: green ≥50%, yellow ≥25%, red otherwise.
pub fn hp_rgb(hp: i32, max_hp: i32) -> (u8, u8, u8) {
    if max_hp <= 0 {
        return HP_LOW;
    }
    let pct = (hp as f64 / max_hp as f64 * 100.0) as i32;
    if pct >= 50 {
        HP_HIGH
    } else if pct >= 25 {
        HP_MID
    } else {
        HP_LOW
    }
}

/// HP color by percentage of max: green ≥50%, yellow ≥25%, red otherwise.
pub fn hp_color(hp: i32, max_hp: i32) -> Style {
    let (r, g, b) = hp_rgb(hp, max_hp);
    Style::new().truecolor(r, g, b)
}

/// Convenience: paint with player style.
pub fn player(text: &str) -> String {
    paint(text, player_style())
}

/// Convenience: paint with DM style.
pub fn dm(text: &str) -> String {
    paint(text, dm_style())
}

/// Convenience: paint with tool style.
pub fn tool(text: &str) -> String {
    paint(text, tool_style())
}

/// Convenience: paint with error style.
pub fn error(text: &str) -> String {
    paint(text, error_style())
}

/// Convenience: paint with success style.
pub fn success(text: &str) -> String {
    paint(text, success_style())
}

/// Convenience: paint muted.
pub fn dim(text: &str) -> String {
    paint(text, muted())
}

/// Convenience: paint HP with threshold color.
pub fn hp(text: &str, current: i32, max: i32) -> String {
    paint(text, hp_color(current, max))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hp_color_thresholds() {
        // Style comparison via painting with NO_COLOR so we only check it doesn't panic.
        let _ = std::env::var("NO_COLOR");
        let _g = paint("18/20", hp_color(18, 20));
        let _y = paint("8/20", hp_color(8, 20));
        let _r = paint("2/20", hp_color(2, 20));
        let _dead = paint("0/0", hp_color(0, 0));
        assert_eq!(hp_rgb(20, 20), HP_HIGH);
        assert_eq!(hp_rgb(8, 20), HP_MID);
        assert_eq!(hp_rgb(2, 20), HP_LOW);
        assert_eq!(hp_rgb(0, 0), HP_LOW);
    }

    #[test]
    fn paint_returns_text() {
        let s = paint("hello", muted());
        assert!(s.contains("hello"));
    }
}
