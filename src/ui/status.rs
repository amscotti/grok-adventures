//! Status chrome from authoritative [`GameState`].

use crate::game::state::GameState;
use crate::ui::theme;

fn item_label(count: usize) -> String {
    match count {
        1 => "1 item".to_string(),
        n => format!("{n} items"),
    }
}

fn status_parts(state: &GameState) -> Vec<String> {
    let mut parts = vec![
        format!("HP {}/{}", state.hp, state.max_hp),
        state.current_room.clone(),
        item_label(state.inventory.len()),
    ];
    if !state.companions.is_empty() {
        let names: Vec<&str> = state.companions.iter().map(|c| c.name.as_str()).collect();
        parts.push(names.join(", "));
    }
    parts
}

/// Build the compact status line (plain text, no ANSI):
/// `HP 18/20  ·  Room  ·  3 items` (companions appended when present).
pub fn format_status_plain(state: &GameState) -> String {
    status_parts(state).join("  ·  ")
}

/// Build the compact status line with theme colors for minimal mode.
pub fn format_status(state: &GameState) -> String {
    let parts = status_parts(state);
    let mut styled = Vec::with_capacity(parts.len());
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            styled.push(theme::hp(part, state.hp, state.max_hp));
        } else {
            styled.push(theme::dim(part));
        }
    }
    styled.join(&theme::dim("  ·  "))
}

/// Print the status line to stdout.
pub fn print_status(state: &GameState) {
    println!("{}", format_status(state));
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn status_includes_hp_room_items() {
        let mut state = GameState::new("Hero");
        state.hp = 18;
        state.current_room = "Crypt".into();
        state.add_item("Torch", "Lit", 1);
        state.add_item("Key", "Brass", 1);
        state.add_item("Coin", "Gold", 1);
        let s = format_status(&state);
        assert!(s.contains("18/20"));
        assert!(s.contains("Crypt"));
        assert!(s.contains("3 item"));
    }

    #[test]
    fn status_includes_companions_when_present() {
        let mut state = GameState::new("Hero");
        state.add_companion("Pip", "fairy");
        let s = format_status(&state);
        assert!(s.contains("Pip"));
    }

    #[test]
    fn empty_companions_omitted() {
        let state = GameState::new("Hero");
        let s = format_status(&state);
        assert!(!s.contains("👥"));
        assert_eq!(state.companions.len(), 0);
    }

    #[test]
    fn plain_status_has_no_ansi() {
        let mut state = GameState::new("Hero");
        state.hp = 18;
        state.current_room = "Crypt".into();
        let s = format_status_plain(&state);
        assert!(s.contains("18/20"));
        assert!(s.contains("Crypt"));
        assert!(!s.contains('\x1b'));
    }
}
