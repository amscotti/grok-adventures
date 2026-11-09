//! Integration tests (no network): save/load, tool dispatch, markers, state deltas.

use grok_adventures::game::delta::{StateSnapshot, describe_deltas, took_damage};
use grok_adventures::game::markers;
use grok_adventures::game::save::SaveGame;
use grok_adventures::game::state::GameState;
use grok_adventures::game::tools::{self, is_mutating};
use serde_json::json;
use tempfile::TempDir;

#[test]
fn save_load_preserves_full_state() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hero.json");

    let mut state = GameState::new("Testhero");
    state.modify_hp(-4);
    state.move_to("Crypt");
    state.add_item("Iron Dagger", "A short blade", 1);
    state.add_item("Arrow", "Wood", 10);
    state.add_companion("Pip", "fairy");
    state.add_journal_entry("The lagoon is cursed");
    state.log("Player: search the altar");
    state.set_last_narration("The altar hums under your hand.");

    SaveGame::save(&state, &path).expect("save");
    let loaded = SaveGame::load(&path).expect("load");

    assert_eq!(loaded.character_name, "Testhero");
    assert_eq!(loaded.hp, 16);
    assert_eq!(loaded.current_room, "Crypt");
    assert!(loaded.find_item("Iron Dagger").is_some());
    assert_eq!(loaded.find_item("Arrow").unwrap().quantity, 10);
    assert!(loaded.find_companion("Pip").is_some());
    assert!(loaded.journal.iter().any(|j| j == "The lagoon is cursed"));
    assert!(loaded.visited("Crypt"));
    assert_eq!(
        loaded.last_narration.as_deref(),
        Some("The altar hums under your hand.")
    );
    assert!(loaded.invariant_violations().is_empty());
}

#[test]
fn tool_dispatch_mutates_hp_inventory_room() {
    let mut state = GameState::new("Hero");

    let r = tools::dispatch(&mut state, "modify_hp", &json!({"amount": -3}));
    assert!(!r.is_error);
    assert_eq!(state.hp, 17);
    assert!(is_mutating("modify_hp"));

    let r = tools::dispatch(
        &mut state,
        "add_to_inventory",
        &json!({"item": "Iron Dagger", "description": "blade", "quantity": 1}),
    );
    assert!(!r.is_error);
    assert!(state.find_item("Iron Dagger").is_some());
    assert!(is_mutating("add_to_inventory"));

    let r = tools::dispatch(
        &mut state,
        "move_to_room",
        &json!({"room_name": "North Corridor"}),
    );
    assert!(!r.is_error);
    assert_eq!(state.current_room, "North Corridor");
    assert!(is_mutating("move_to_room"));

    // Read-only tools do not mutate.
    assert!(!is_mutating("get_inventory"));
    let before_hp = state.hp;
    let r = tools::dispatch(&mut state, "get_character_stats", &json!({}));
    assert!(!r.is_error);
    assert_eq!(state.hp, before_hp);
}

#[test]
fn markers_apply_only_as_fallback() {
    let mut state = GameState::new("Hero");
    let text = "A trap! [DAMAGE:5] You fall into [MOVE:Pit] and grab [ADD_ITEM:Torch:Lit brand].";

    // Mutating tool already ran → markers skipped.
    let out = markers::process_if_fallback(&mut state, text, true);
    assert_eq!(out, text);
    assert_eq!(state.hp, 20);
    assert_eq!(state.current_room, "Starting Chamber");
    assert!(state.inventory.is_empty());

    // No mutating tool → markers apply.
    let out = markers::process_if_fallback(&mut state, text, false);
    assert!(!out.contains("[DAMAGE:"));
    assert!(!out.contains("[MOVE:"));
    assert!(!out.contains("[ADD_ITEM:"));
    assert_eq!(state.hp, 15);
    assert_eq!(state.current_room, "Pit");
    assert!(state.find_item("Torch").is_some());
}

#[test]
fn state_deltas_match_tool_and_marker_effects() {
    let mut state = GameState::new("Hero");
    let before = StateSnapshot::from(&state);

    tools::dispatch(&mut state, "modify_hp", &json!({"amount": -2}));
    tools::dispatch(
        &mut state,
        "add_to_inventory",
        &json!({"item": "Iron Dagger", "description": "blade"}),
    );
    tools::dispatch(&mut state, "move_to_room", &json!({"room_name": "Hall"}));

    let lines = describe_deltas(&before, &state);
    assert!(
        lines.iter().any(|l| l == "💔 -2 HP (18/20)"),
        "hp delta missing: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l == "🚶 Starting Chamber → Hall"),
        "room delta missing: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l == "🎒 +Iron Dagger"),
        "item delta missing: {lines:?}"
    );
    assert!(took_damage(&before, &state));
}

#[test]
fn state_deltas_empty_when_unchanged() {
    let state = GameState::new("Hero");
    let before = StateSnapshot::from(&state);
    assert!(describe_deltas(&before, &state).is_empty());
    assert!(!took_damage(&before, &state));
}
