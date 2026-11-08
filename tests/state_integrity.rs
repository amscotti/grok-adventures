//! Integration tests for state-integrity fixes: save schema/repair, combat
//! validation, rests, journal eviction, and effective HP reporting.

use std::fs;

use grok_adventures::game::markers;
use grok_adventures::game::save::SaveGame;
use grok_adventures::game::state::{GameState, MAX_ITEM_QUANTITY, MAX_JOURNAL_ENTRIES};
use grok_adventures::game::tools;
use serde_json::json;
use tempfile::TempDir;

#[test]
fn save_round_trip_uses_envelope_and_legacy_raw_loads() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hero.json");
    let mut state = GameState::new("Envelope Hero");
    state.modify_hp(-4);

    SaveGame::save(&state, &path).expect("save");
    let raw = fs::read_to_string(&path).unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(value["schema_version"].as_u64(), Some(1));
    assert_eq!(value["state"]["character_name"], "Envelope Hero");

    let loaded = SaveGame::load(&path).expect("load envelope");
    assert_eq!(loaded.hp, 16);

    // Legacy save: the whole file is a raw GameState.
    let legacy_path = dir.path().join("legacy.json");
    fs::write(&legacy_path, serde_json::to_string(&state).unwrap()).unwrap();
    let legacy = SaveGame::load(&legacy_path).expect("load legacy");
    assert_eq!(legacy.character_name, "Envelope Hero");
    assert_eq!(legacy.hp, 16);
}

#[test]
fn objective_state_round_trips_and_legacy_saves_default_to_none() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("objective.json");
    let mut state = GameState::new("Hero");
    let result = tools::dispatch(
        &mut state,
        "set_objective",
        &json!({
            "objective": "  Recover the Sunstone  ",
            "threat": "The eclipse begins in 3 days"
        }),
    );
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(state.objective.as_deref(), Some("Recover the Sunstone"));

    SaveGame::save(&state, &path).expect("save");
    let loaded = SaveGame::load(&path).expect("load");
    assert_eq!(loaded.objective.as_deref(), Some("Recover the Sunstone"));
    assert_eq!(
        loaded.threat.as_deref(),
        Some("The eclipse begins in 3 days")
    );
    let context = loaded.to_context_string();
    assert!(context.contains("Objective: Recover the Sunstone"));
    assert!(context.contains("Threat: The eclipse begins in 3 days"));

    // Legacy raw save with no objective/threat fields.
    let legacy_path = dir.path().join("legacy.json");
    fs::write(
        &legacy_path,
        r#"{
  "character_name": "Legacy",
  "stats": {"STR":10,"DEX":10,"CON":10,"INT":10,"WIS":10,"CHA":10},
  "hp": 20,
  "max_hp": 20,
  "inventory": [],
  "companions": [],
  "journal": [],
  "current_room": "Starting Chamber",
  "visited_rooms": ["Starting Chamber"],
  "game_log": []
}"#,
    )
    .unwrap();
    let legacy = SaveGame::load(&legacy_path).expect("legacy load");
    assert!(legacy.objective.is_none());
    assert!(legacy.threat.is_none());
    assert!(legacy.last_narration.is_none());
    assert!(!legacy.to_context_string().contains("Objective:"));
}

#[test]
fn load_repairs_impossible_values() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("broken.json");
    let mut state = GameState::new("Hero");
    state.max_hp = 0;
    state.hp = -9;
    state.add_item("Potion", "red", 1);
    state.inventory[0].quantity = i32::MAX;
    state.current_room.clear();
    state.journal = vec!["Fact".into(), "fact".into()];
    fs::write(&path, serde_json::to_string(&state).unwrap()).unwrap();

    let (loaded, warnings) = SaveGame::load_with_warnings(&path).expect("load");
    assert_eq!(loaded.max_hp, 1);
    assert_eq!(loaded.hp, 0);
    assert_eq!(loaded.inventory[0].quantity, MAX_ITEM_QUANTITY);
    assert!(!loaded.current_room.trim().is_empty());
    assert_eq!(loaded.journal, vec!["Fact"]);
    assert!(loaded.invariant_violations().is_empty());
    assert!(!warnings.is_empty());
}

#[test]
fn rests_refused_in_combat_and_combat_requires_player() {
    let mut state = GameState::new("Hero");
    let missing_player = state.start_combat(vec!["Goblin".into()]);
    assert!(missing_player.contains("must include the player"));
    assert!(!state.in_combat);

    state.start_combat(vec!["Hero".into(), "Goblin".into()]);
    state.hp = 5;
    assert_eq!(state.short_rest(), "Cannot rest during combat.");
    assert_eq!(state.long_rest(), "Cannot rest during combat.");
    assert_eq!(state.hp, 5);
}

#[test]
fn journal_eviction_reports_oldest_fact() {
    let mut state = GameState::new("Hero");
    for i in 0..MAX_JOURNAL_ENTRIES {
        state.add_journal_entry(format!("fact {i}"));
    }
    let result = state.add_journal_entry("fact 20");
    assert_eq!(state.journal.len(), MAX_JOURNAL_ENTRIES);
    assert_eq!(state.journal[0], "fact 1");
    assert!(result.contains("oldest fact evicted: fact 0"));
}

#[test]
fn markers_and_tools_report_effective_hp() {
    let mut state = GameState::new("Hero");
    state.hp = 19;
    let result = tools::dispatch(&mut state, "modify_hp", &json!({"amount": 5}));
    assert_eq!(state.hp, 20);
    assert!(result.content.contains("Healed 1 HP"), "{}", result.content);

    let out = markers::process(&mut state, "[HEAL:5]");
    assert!(out.contains("+0 HP"), "{out}");
    assert!(!out.contains("+5 HP"), "{out}");

    let rejected = tools::dispatch(&mut state, "modify_hp", &json!({"amount": i64::MAX}));
    assert!(rejected.is_error);
    assert_eq!(state.hp, 20);
}
