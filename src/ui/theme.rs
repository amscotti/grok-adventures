//! GrokNight theme helpers via `owo-colors`.
//!
//! All helpers respect `NO_COLOR` (when set and non-empty, colors are disabled).

use owo_colors::{OwoColorize, Style};

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
    Style::new().fg_rgb::<180, 120, 255>().bold()
}

/// DM narration accent.
pub fn dm_style() -> Style {
    Style::new().fg_rgb::<140, 200, 255>()
}

/// Tool / mechanics accent.
pub fn tool_style() -> Style {
    Style::new().fg_rgb::<255, 200, 100>().dimmed()
}

/// Error accent.
pub fn error_style() -> Style {
    Style::new().fg_rgb::<255, 90, 90>().bold()
}

/// Success accent.
pub fn success_style() -> Style {
    Style::new().fg_rgb::<100, 220, 140>()
}

/// Muted secondary text.
pub fn muted() -> Style {
    Style::new().dimmed()
}

/// Style for primary body text.
pub fn primary() -> Style {
    Style::new()
}

/// HP color by percentage of max: green ≥50%, yellow ≥25%, red otherwise.
pub fn hp_color(hp: i32, max_hp: i32) -> Style {
    if max_hp <= 0 {
        return Style::new().fg_rgb::<255, 90, 90>();
    }
    let pct = (hp as f64 / max_hp as f64 * 100.0) as i32;
    if pct >= 50 {
        Style::new().fg_rgb::<100, 220, 140>()
    } else if pct >= 25 {
        Style::new().fg_rgb::<255, 200, 80>()
    } else {
        Style::new().fg_rgb::<255, 90, 90>()
    }
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
    }

    #[test]
    fn paint_returns_text() {
        let s = paint("hello", muted());
        assert!(s.contains("hello"));
    }
}
