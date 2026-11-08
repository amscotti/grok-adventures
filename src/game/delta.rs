//! State delta toasts — compact lines after a turn when HP, room, or inventory changed.
//!
//! Pure snapshot comparison so tool effects are visible without expanding tool blocks.

use std::collections::BTreeMap;

use crate::game::state::GameState;

/// Frozen view of the fields we toast on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSnapshot {
    pub hp: i32,
    pub max_hp: i32,
    pub room: String,
    /// Item display name → total quantity.
    pub items: BTreeMap<String, i32>,
}

impl From<&GameState> for StateSnapshot {
    fn from(state: &GameState) -> Self {
        let mut items: BTreeMap<String, i32> = BTreeMap::new();
        for item in &state.inventory {
            let total = items.entry(item.name.clone()).or_insert(0);
            *total = (*total).saturating_add(item.quantity);
        }
        Self {
            hp: state.hp,
            max_hp: state.max_hp,
            room: state.current_room.clone(),
            items,
        }
    }
}

/// Human-readable delta lines for HP / room / inventory changes.
///
/// Examples: `💔 -2 HP (18/20)`, `💚 +3 HP (20/20)`, `🚶 A → B`, `🎒 +Iron Dagger`.
pub fn describe_deltas(before: &StateSnapshot, after: &GameState) -> Vec<String> {
    let after = StateSnapshot::from(after);
    let mut lines = Vec::new();

    if after.hp != before.hp {
        let delta = i64::from(after.hp) - i64::from(before.hp);
        if delta < 0 {
            lines.push(format!("💔 {delta} HP ({}/{})", after.hp, after.max_hp));
        } else {
            lines.push(format!("💚 +{delta} HP ({}/{})", after.hp, after.max_hp));
        }
    }

    if after.room != before.room {
        lines.push(format!("🚶 {} → {}", before.room, after.room));
    }

    // Inventory adds (new items or quantity increases).
    for (name, after_qty) in &after.items {
        let before_qty = before.items.get(name).copied().unwrap_or(0);
        let gained = (*after_qty).saturating_sub(before_qty);
        if gained > 0 {
            if before_qty == 0 && gained == 1 {
                lines.push(format!("🎒 +{name}"));
            } else {
                lines.push(format!("🎒 +{name} (x{gained})"));
            }
        }
    }

    // Inventory removals (gone or quantity decreases).
    for (name, before_qty) in &before.items {
        let after_qty = after.items.get(name).copied().unwrap_or(0);
        let lost = (*before_qty).saturating_sub(after_qty);
        if lost > 0 {
            if after_qty == 0 && lost == 1 {
                lines.push(format!("🎒 -{name}"));
            } else {
                lines.push(format!("🎒 -{name} (x{lost})"));
            }
        }
    }

    lines
}

/// True when HP decreased (used to trigger optional damage SFX).
pub fn took_damage(before: &StateSnapshot, after: &GameState) -> bool {
    after.hp < before.hp
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn no_change_empty_deltas() {
        let state = GameState::new("Hero");
        let before = StateSnapshot::from(&state);
        assert!(describe_deltas(&before, &state).is_empty());
        assert!(!took_damage(&before, &state));
    }

    #[test]
    fn hp_damage_and_heal() {
        let mut state = GameState::new("Hero");
        let before = StateSnapshot::from(&state);
        state.modify_hp(-2);
        let lines = describe_deltas(&before, &state);
        assert_eq!(lines, vec!["💔 -2 HP (18/20)".to_string()]);
        assert!(took_damage(&before, &state));

        let before = StateSnapshot::from(&state);
        state.modify_hp(1);
        let lines = describe_deltas(&before, &state);
        assert_eq!(lines, vec!["💚 +1 HP (19/20)".to_string()]);
        assert!(!took_damage(&before, &state));
    }

    #[test]
    fn room_move() {
        let mut state = GameState::new("Hero");
        let before = StateSnapshot::from(&state);
        state.move_to("Dark Crypt");
        let lines = describe_deltas(&before, &state);
        assert_eq!(lines, vec!["🚶 Starting Chamber → Dark Crypt".to_string()]);
    }

    #[test]
    fn inventory_add_and_remove() {
        let mut state = GameState::new("Hero");
        let before = StateSnapshot::from(&state);
        state.add_item("Iron Dagger", "A rusty blade", 1);
        let lines = describe_deltas(&before, &state);
        assert_eq!(lines, vec!["🎒 +Iron Dagger".to_string()]);

        let before = StateSnapshot::from(&state);
        state.add_item("Arrow", "Wood", 5);
        let lines = describe_deltas(&before, &state);
        assert_eq!(lines, vec!["🎒 +Arrow (x5)".to_string()]);

        let before = StateSnapshot::from(&state);
        state.remove_item("Arrow", 2);
        let lines = describe_deltas(&before, &state);
        assert_eq!(lines, vec!["🎒 -Arrow (x2)".to_string()]);

        let before = StateSnapshot::from(&state);
        state.remove_item("Iron Dagger", 1);
        let lines = describe_deltas(&before, &state);
        assert_eq!(lines, vec!["🎒 -Iron Dagger".to_string()]);
    }

    #[test]
    fn combined_hp_room_item() {
        let mut state = GameState::new("Hero");
        let before = StateSnapshot::from(&state);
        state.modify_hp(-5);
        state.move_to("Hall");
        state.add_item("Key", "Brass", 1);
        let lines = describe_deltas(&before, &state);
        assert_eq!(
            lines,
            vec![
                "💔 -5 HP (15/20)".to_string(),
                "🚶 Starting Chamber → Hall".to_string(),
                "🎒 +Key".to_string(),
            ]
        );
    }

    #[test]
    fn snapshot_from_stacks_quantities() {
        let mut state = GameState::new("Hero");
        state.add_item("Potion", "red", 2);
        state.add_item("Potion", "red", 3);
        let snap = StateSnapshot::from(&state);
        assert_eq!(snap.items.get("Potion"), Some(&5));
    }

    #[test]
    fn snapshot_quantity_sum_saturates() {
        let mut state = GameState::new("Hero");
        state.inventory.push(crate::game::state::InventoryItem::new(
            "Gold",
            "coins",
            i32::MAX,
        ));
        state.inventory.push(crate::game::state::InventoryItem::new(
            "Gold",
            "more coins",
            i32::MAX,
        ));
        let snap = StateSnapshot::from(&state);
        // The per-key sum must saturate instead of wrapping.
        assert_eq!(snap.items.get("Gold"), Some(&i32::MAX));
    }

    #[test]
    fn hp_delta_handles_extreme_values() {
        let state = GameState::new("Hero");
        let mut before = StateSnapshot::from(&state);
        before.hp = i32::MIN;
        let mut after = GameState::new("Hero");
        after.hp = i32::MAX;
        let lines = describe_deltas(&before, &after);
        assert_eq!(lines, vec!["💚 +4294967295 HP (2147483647/20)".to_string()]);
    }
}
