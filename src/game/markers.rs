//! Marker fallback parser.
//!
//! When the DM narrates a state change without calling a tool, lightweight
//! text markers in the response apply the change. This runs only as a fallback
//! so tool-driven mutations are never double-applied.

use std::sync::LazyLock;

use regex::Regex;

use crate::game::state::GameState;
use crate::util::dice;

static ROLL_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[ROLL:(\d+)d(\d+)(?:([+-]\d+))?\]").expect("valid regex"));
/// Grok Adventures dice-notation alias for rolls: `[[2d6+3]]`.
static ROLL_ALIAS_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[(\d+)d(\d+)(?:([+-]\d+))?\]\]").expect("valid regex"));
static ADD_ITEM_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[ADD_ITEM:([^:]+):([^\]]+)\]").expect("valid regex"));
static REMOVE_ITEM_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[REMOVE_ITEM:([^\]]+)\]").expect("valid regex"));
static DAMAGE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[DAMAGE:(\d+)\]").expect("valid regex"));
static HEAL_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[HEAL:(\d+)\]").expect("valid regex"));
static MOVE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[MOVE:([^\]]+)\]").expect("valid regex"));

/// Process markers in a DM response and execute game actions.
/// Replaces each marker with human-readable text.
pub fn process(state: &mut GameState, response: &str) -> String {
    let mut result = response.to_string();
    result = process_rolls(&result);
    result = process_add_items(state, &result);
    result = process_remove_items(state, &result);
    result = process_damage(state, &result);
    result = process_healing(state, &result);
    result = process_moves(state, &result);
    result
}

/// Apply markers only when no state-mutating tool ran this turn.
///
/// Read-only tools must not disable the fallback. When `mutating_tool_ran` is
/// true the response is returned unchanged so tool mutations are not double-counted.
pub fn process_if_fallback(
    state: &mut GameState,
    response: &str,
    mutating_tool_ran: bool,
) -> String {
    if mutating_tool_ran {
        response.to_string()
    } else {
        process(state, response)
    }
}

fn format_roll_replacement(count: u32, sides: u32, modifier: i32) -> Option<String> {
    let roll = dice::roll(sides, count, modifier).ok()?;

    let mod_str = if modifier > 0 {
        format!("+{modifier}")
    } else if modifier < 0 {
        modifier.to_string()
    } else {
        String::new()
    };

    if count > 1 {
        let parts: Vec<String> = roll.rolls.iter().map(|r| r.to_string()).collect();
        Some(format!(
            "🎲 {count}d{sides}{mod_str} [{}] = **{}**",
            parts.join(", "),
            roll.total
        ))
    } else {
        Some(format!("🎲 {count}d{sides}{mod_str} = **{}**", roll.total))
    }
}

fn replace_roll_captures(text: &str, pattern: &Regex) -> String {
    pattern
        .replace_all(text, |caps: &regex::Captures| {
            let original = caps.get(0).map(|m| m.as_str()).unwrap_or("");
            let count: u32 = match caps[1].parse() {
                Ok(c) => c,
                Err(_) => return original.to_string(),
            };
            let sides: u32 = match caps[2].parse() {
                Ok(s) => s,
                Err(_) => return original.to_string(),
            };
            let modifier: i32 = caps
                .get(3)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);

            format_roll_replacement(count, sides, modifier).unwrap_or_else(|| original.to_string())
        })
        .into_owned()
}

fn process_rolls(text: &str) -> String {
    let after_primary = replace_roll_captures(text, &ROLL_PATTERN);
    replace_roll_captures(&after_primary, &ROLL_ALIAS_PATTERN)
}

fn process_add_items(state: &mut GameState, text: &str) -> String {
    ADD_ITEM_PATTERN
        .replace_all(text, |caps: &regex::Captures| {
            let item_name = caps[1].trim();
            let description = caps[2].trim();
            let added = state.add_item(item_name, description, 1);
            if added.starts_with("Added") {
                state.log(format!("Acquired: {item_name}"));
                format!("**[Added: {item_name}]**")
            } else {
                format!("**[Could not add: {item_name}]**")
            }
        })
        .into_owned()
}

fn process_remove_items(state: &mut GameState, text: &str) -> String {
    REMOVE_ITEM_PATTERN
        .replace_all(text, |caps: &regex::Captures| {
            let item_name = caps[1].trim();
            let removed = state.remove_item(item_name, 1);
            if removed.starts_with("Removed") {
                state.log(format!("Used/dropped: {item_name}"));
                format!("**[Removed: {item_name}]**")
            } else {
                format!("**[Item not found: {item_name}]**")
            }
        })
        .into_owned()
}

/// Parse a marker's digit string, clamping values too large for `i32`
/// instead of silently treating them as zero.
fn marker_amount(raw: &str) -> i32 {
    raw.parse::<i64>()
        .map(|n| n.clamp(0, i64::from(i32::MAX)) as i32)
        .unwrap_or(i32::MAX)
}

fn process_damage(state: &mut GameState, text: &str) -> String {
    DAMAGE_PATTERN
        .replace_all(text, |caps: &regex::Captures| {
            let amount = marker_amount(&caps[1]);
            let old_hp = state.hp;
            state.modify_hp(-amount);
            let applied = (i64::from(old_hp) - i64::from(state.hp)).max(0);
            format!("**💔 -{applied} HP ({}/{})**", state.hp, state.max_hp)
        })
        .into_owned()
}

fn process_healing(state: &mut GameState, text: &str) -> String {
    HEAL_PATTERN
        .replace_all(text, |caps: &regex::Captures| {
            let amount = marker_amount(&caps[1]);
            let old_hp = state.hp;
            state.modify_hp(amount);
            let applied = (i64::from(state.hp) - i64::from(old_hp)).max(0);
            format!("**💚 +{applied} HP ({}/{})**", state.hp, state.max_hp)
        })
        .into_owned()
}

fn process_moves(state: &mut GameState, text: &str) -> String {
    MOVE_PATTERN
        .replace_all(text, |caps: &regex::Captures| {
            let room_name = caps[1].trim();
            let old_room = state.current_room.clone();
            let moved = state.move_to(room_name);
            if moved.contains("cannot be blank") {
                return "**[Invalid room]**".to_string();
            }
            let first_visit = moved.contains("first visit");
            state.log(format!("Moved: {old_room} → {}", state.current_room));
            if first_visit {
                format!("**{}** *(first visit)*", state.current_room)
            } else {
                format!("**{}**", state.current_room)
            }
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn fresh() -> GameState {
        GameState::new("Hero")
    }

    #[test]
    fn roll_marker_replaces_with_dice_text() {
        let mut state = fresh();
        let out = process(&mut state, "You swing [ROLL:1d20+5] at the goblin.");
        assert!(!out.contains("[ROLL:"));
        assert!(out.contains("🎲"));
        assert!(out.contains("1d20+5"));
        assert!(out.contains("**"));
    }

    #[test]
    fn roll_alias_double_bracket() {
        let mut state = fresh();
        let out = process(&mut state, "Roll [[2d6+3]] for damage.");
        assert!(!out.contains("[[2d6+3]]"));
        assert!(out.contains("🎲"));
        assert!(out.contains("2d6+3"));
    }

    #[test]
    fn add_item_marker_stacks() {
        let mut state = fresh();
        let out = process(
            &mut state,
            "You find [ADD_ITEM:Health Potion:Restores vitality].",
        );
        assert!(out.contains("**[Added: Health Potion]**"));
        assert_eq!(state.inventory.len(), 1);
        assert_eq!(state.find_item("Health Potion").unwrap().quantity, 1);
        assert_eq!(
            state.find_item("Health Potion").unwrap().description,
            "Restores vitality"
        );

        process(&mut state, "[ADD_ITEM:health potion:Restores vitality]");
        assert_eq!(state.inventory.len(), 1);
        assert_eq!(state.find_item("Health Potion").unwrap().quantity, 2);
    }

    #[test]
    fn remove_item_marker() {
        let mut state = fresh();
        state.add_item("Torch", "Lit stick", 1);
        let out = process(&mut state, "You drop [REMOVE_ITEM:Torch].");
        assert!(out.contains("**[Removed: Torch]**"));
        assert!(state.find_item("Torch").is_none());

        let missing = process(&mut state, "[REMOVE_ITEM:Ghost]");
        assert!(missing.contains("**[Item not found: Ghost]**"));
    }

    #[test]
    fn damage_and_heal_markers() {
        let mut state = fresh();
        assert_eq!(state.hp, 20);

        let out = process(&mut state, "The trap springs! [DAMAGE:5]");
        assert_eq!(state.hp, 15);
        assert!(out.contains("**💔 -5 HP (15/20)**"));

        let out = process(&mut state, "You drink a potion. [HEAL:3]");
        assert_eq!(state.hp, 18);
        assert!(out.contains("**💚 +3 HP (18/20)**"));
    }

    #[test]
    fn move_marker() {
        let mut state = fresh();
        let out = process(&mut state, "You enter [MOVE:Dark Cave].");
        assert_eq!(state.current_room, "Dark Cave");
        assert!(state.visited("Dark Cave"));
        assert!(out.contains("**Dark Cave**"));
        assert!(out.contains("first visit"));

        let again = process(&mut state, "[MOVE:Dark Cave]");
        assert!(again.contains("**Dark Cave**"));
        assert!(!again.contains("first visit"));
    }

    #[test]
    fn multiple_markers_in_one_response() {
        let mut state = fresh();
        let response = "You take [DAMAGE:2] and grab [ADD_ITEM:Key:Brass key], \
                        then step into [MOVE:Hall].";
        let out = process(&mut state, response);
        assert_eq!(state.hp, 18);
        assert!(state.find_item("Key").is_some());
        assert_eq!(state.current_room, "Hall");
        assert!(out.contains("💔"));
        assert!(out.contains("**[Added: Key]**"));
        assert!(out.contains("**Hall**"));
    }

    #[test]
    fn process_if_fallback_skips_when_mutating_tool_ran() {
        let mut state = fresh();
        let response = "You find [ADD_ITEM:Sword:A blade] and take [DAMAGE:4].";

        let out = process_if_fallback(&mut state, response, true);
        assert_eq!(out, response);
        assert!(state.inventory.is_empty());
        assert_eq!(state.hp, 20);

        let out = process_if_fallback(&mut state, response, false);
        assert_ne!(out, response);
        assert!(state.find_item("Sword").is_some());
        assert_eq!(state.hp, 16);
    }

    #[test]
    fn process_if_fallback_double_apply_prevention() {
        let mut state = fresh();
        state.add_item("Sword", "A blade", 1);
        let response = "You equip [ADD_ITEM:Sword:A blade].";

        let _ = process_if_fallback(&mut state, response, true);
        assert_eq!(state.find_item("Sword").unwrap().quantity, 1);

        let mut state2 = fresh();
        state2.add_item("Sword", "A blade", 1);
        let _ = process_if_fallback(&mut state2, response, false);
        assert_eq!(state2.find_item("Sword").unwrap().quantity, 2);
    }

    #[test]
    fn plain_text_unchanged() {
        let mut state = fresh();
        let text = "The dungeon is quiet. Nothing happens.";
        assert_eq!(process(&mut state, text), text);
        assert_eq!(state.hp, 20);
        assert!(state.inventory.is_empty());
    }

    #[test]
    fn negative_modifier_roll() {
        let mut state = fresh();
        let out = process(&mut state, "[ROLL:1d6-1]");
        assert!(out.contains("1d6-1"));
        assert!(out.contains("🎲"));
    }

    #[test]
    fn huge_markers_clamp_without_panicking() {
        let mut state = fresh();
        let out = process(
            &mut state,
            "The world ends. [DAMAGE:999999999999999999999999]",
        );
        assert_eq!(state.hp, 0);
        assert!(out.contains("-20 HP"), "{out}");

        let out = process(&mut state, "[HEAL:999999999999999999999999]");
        assert_eq!(state.hp, 20);
        assert!(out.contains("+20 HP"), "{out}");
    }

    #[test]
    fn heal_marker_reports_effective_delta_at_full_hp() {
        let mut state = fresh();
        assert_eq!(state.hp, 20);
        let out = process(&mut state, "[HEAL:5]");
        assert_eq!(state.hp, 20);
        assert!(out.contains("+0 HP (20/20)"), "{out}");
        assert!(!out.contains("+5 HP"), "{out}");
    }
}
