//! Save / load helpers for Grok Adventures.
//!
//! Persists the full [`GameState`] as pretty-printed JSON wrapped in a small
//! schema envelope so future migrations can detect the format. Legacy saves
//! containing a raw [`GameState`] are still accepted. Writes are atomic: the
//! JSON goes to a temporary file in the same directory, is synced, then
//! renamed over the target, with the previous save kept as `.bak`.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::game::state::{
    Companion, Exchange, GameState, InventoryItem, MAX_COMBAT_PARTICIPANTS,
    MAX_COMPANION_DESC_CHARS, MAX_COMPANION_NAME_CHARS, MAX_EXCHANGE_DM_CHARS,
    MAX_EXCHANGE_PLAYER_CHARS, MAX_EXCHANGES, MAX_ITEM_DESC_CHARS, MAX_ITEM_NAME_CHARS,
    MAX_ITEM_QUANTITY, MAX_JOURNAL_ENTRIES, MAX_JOURNAL_FACT_CHARS, MAX_LOG_ENTRIES,
    MAX_NARRATION_CHARS, MAX_OBJECTIVE_CHARS, MAX_ROOM_NAME_CHARS, MAX_THREAT_CHARS,
    normalize_story_text,
};

/// Current save-file schema version written by this build.
pub const SAVE_SCHEMA_VERSION: u32 = 1;

#[derive(Serialize)]
struct SaveEnvelopeRef<'a> {
    schema_version: u32,
    state: &'a GameState,
}

#[derive(Deserialize)]
struct SaveEnvelope {
    schema_version: u32,
    state: GameState,
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Namespace for save path resolution and I/O.
pub struct SaveGame;

impl SaveGame {
    const DEFAULT_DIR: &'static str = "saves";
    const DEFAULT_SLOT: &'static str = "autosave";

    /// Resolve a user-supplied save argument to a filesystem path.
    ///
    /// - `None` / blank → `saves/autosave.json`
    /// - `"hero1"` → `saves/hero1.json` (sanitized)
    /// - `"foo.json"` or path with `/` or `\` → used literally
    pub fn resolve_path(arg: Option<&str>) -> String {
        let Some(arg) = arg else {
            return format!("{}/{}.json", Self::DEFAULT_DIR, Self::DEFAULT_SLOT);
        };
        let cleaned = arg.trim();
        if cleaned.is_empty() {
            return format!("{}/{}.json", Self::DEFAULT_DIR, Self::DEFAULT_SLOT);
        }

        if cleaned.contains('/') || cleaned.contains('\\') || cleaned.ends_with(".json") {
            cleaned.to_string()
        } else {
            let safe: String = cleaned
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            let safe = if safe.is_empty() {
                Self::DEFAULT_SLOT.to_string()
            } else {
                safe
            };
            format!("{}/{}.json", Self::DEFAULT_DIR, safe)
        }
    }

    /// Write game state to disk atomically. Creates parent directories as needed
    /// and keeps the previous save as `<path>.bak` when one exists.
    ///
    /// Refuses to write when the target path is a symlink. The temp+rename
    /// would replace the link rather than write through it, but an explicit
    /// refusal makes the safety property visible to the caller.
    pub fn save(state: &GameState, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                bail!("refusing to write save through symlink {}", path.display());
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| format!("checking save path {}", path.display()));
            }
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating save directory {}", parent.display()))?;
        }

        let json = serde_json::to_string_pretty(&SaveEnvelopeRef {
            schema_version: SAVE_SCHEMA_VERSION,
            state,
        })
        .context("serializing game state")?;

        let tmp = temp_path(path);
        let result = write_atomic(path, &tmp, json.as_bytes());
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// Load game state from disk, logging any repairs.
    pub fn load(path: impl AsRef<Path>) -> Result<GameState> {
        let (state, warnings) = Self::load_with_warnings(path)?;
        for warning in &warnings {
            tracing::warn!(target: "grok_adventures::save", "{warning}");
        }
        Ok(state)
    }

    /// Load game state from disk and return the repairs applied, if any.
    ///
    /// Accepts both the current schema envelope and legacy raw-`GameState`
    /// JSON. Impossible values are clamped or dropped rather than trusted.
    pub fn load_with_warnings(path: impl AsRef<Path>) -> Result<(GameState, Vec<String>)> {
        let path = path.as_ref();
        if !path.exists() {
            bail!("No save file found at {}", path.display());
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading save from {}", path.display()))?;
        let mut state =
            decode_save(&raw).with_context(|| format!("parsing save file {}", path.display()))?;
        let mut warnings = Vec::new();
        repair_state(&mut state, &mut warnings);
        Ok((state, warnings))
    }

    /// Convenience: resolve + load.
    pub fn load_slot(arg: Option<&str>) -> Result<GameState> {
        let path = Self::resolve_path(arg);
        Self::load(path)
    }

    /// Convenience: resolve + save. Returns the path written.
    pub fn save_slot(state: &GameState, arg: Option<&str>) -> Result<PathBuf> {
        let path = Self::resolve_path(arg);
        Self::save(state, &path)?;
        Ok(PathBuf::from(path))
    }
}

fn decode_save(raw: &str) -> Result<GameState> {
    let value: Value = serde_json::from_str(raw)?;
    if value.get("schema_version").is_some() || value.get("state").is_some() {
        let envelope: SaveEnvelope = serde_json::from_value(value)?;
        if envelope.schema_version == 0 {
            bail!("invalid save schema version 0");
        }
        if envelope.schema_version > SAVE_SCHEMA_VERSION {
            bail!(
                "save schema version {} is newer than supported version {}",
                envelope.schema_version,
                SAVE_SCHEMA_VERSION
            );
        }
        Ok(envelope.state)
    } else {
        // Legacy save: the file itself is a raw GameState.
        Ok(serde_json::from_value(value)?)
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| OsString::from("save"));
    let mut name = OsString::from(".");
    name.push(file_name);
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    path.with_file_name(name)
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".bak");
    PathBuf::from(name)
}

fn write_atomic(path: &Path, tmp: &Path, bytes: &[u8]) -> Result<()> {
    {
        let mut file = File::create(tmp)
            .with_context(|| format!("creating temporary save {}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing temporary save {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing temporary save {}", tmp.display()))?;
    }

    if path.is_file() {
        let backup = backup_path(path);
        fs::copy(path, &backup)
            .with_context(|| format!("backing up previous save to {}", backup.display()))?;
    }

    fs::rename(tmp, path).with_context(|| format!("replacing save at {}", path.display()))?;
    Ok(())
}

/// Truncate a repaired field to `max_chars` characters, logging a warning.
/// Trailing whitespace left by truncation is trimmed.
fn cap_chars(value: &str, max_chars: usize, label: &str, warnings: &mut Vec<String>) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    warnings.push(format!("truncated {label} to {max_chars} characters"));
    value
        .chars()
        .take(max_chars)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// Repair values that should be impossible in a live game.
fn repair_state(state: &mut GameState, warnings: &mut Vec<String>) {
    if state.max_hp < 1 {
        warnings.push(format!("repaired max_hp {} to 1", state.max_hp));
        state.max_hp = 1;
    }
    if state.hp < 0 || state.hp > state.max_hp {
        let repaired = state.hp.clamp(0, state.max_hp);
        warnings.push(format!(
            "repaired HP {} to {repaired} (max {})",
            state.hp, state.max_hp
        ));
        state.hp = repaired;
    }

    repair_inventory(state, warnings);
    repair_companions(state, warnings);
    repair_journal(state, warnings);
    repair_rooms(state, warnings);
    repair_story(state, warnings);
    repair_exchanges(state, warnings);

    if state.game_log.len() > MAX_LOG_ENTRIES {
        let excess = state.game_log.len() - MAX_LOG_ENTRIES;
        warnings.push(format!(
            "trimmed game_log from {} to {MAX_LOG_ENTRIES} entries",
            state.game_log.len()
        ));
        state.game_log.drain(0..excess);
    }

    repair_combat(state, warnings);
}

fn repair_inventory(state: &mut GameState, warnings: &mut Vec<String>) {
    let mut repaired: Vec<InventoryItem> = Vec::new();
    for mut item in std::mem::take(&mut state.inventory) {
        let name = item.name.trim();
        if name.is_empty() {
            warnings.push("dropped inventory item with a blank name".to_string());
            continue;
        }
        item.name = cap_chars(name, MAX_ITEM_NAME_CHARS, "inventory item name", warnings);
        if item.name.is_empty() {
            warnings.push("dropped inventory item with a blank name".to_string());
            continue;
        }
        if item.description.chars().count() > MAX_ITEM_DESC_CHARS {
            item.description = cap_chars(
                &item.description,
                MAX_ITEM_DESC_CHARS,
                &format!("description for item '{}'", item.name),
                warnings,
            );
        }
        let clamped = item.quantity.clamp(1, MAX_ITEM_QUANTITY);
        if clamped != item.quantity {
            warnings.push(format!(
                "clamped item '{}' quantity {} to {clamped}",
                item.name, item.quantity
            ));
            item.quantity = clamped;
        }
        if let Some(existing) = repaired
            .iter_mut()
            .find(|existing| existing.same_type(&item.name))
        {
            warnings.push(format!("merged duplicate inventory item '{}'", item.name));
            existing.quantity = existing
                .quantity
                .saturating_add(item.quantity)
                .min(MAX_ITEM_QUANTITY);
        } else {
            repaired.push(item);
        }
    }
    state.inventory = repaired;
}

fn repair_companions(state: &mut GameState, warnings: &mut Vec<String>) {
    let mut repaired: Vec<Companion> = Vec::new();
    for mut companion in std::mem::take(&mut state.companions) {
        let name = companion.name.trim();
        if name.is_empty() {
            warnings.push("dropped companion with a blank name".to_string());
            continue;
        }
        companion.name = cap_chars(name, MAX_COMPANION_NAME_CHARS, "companion name", warnings);
        if companion.name.is_empty() {
            warnings.push("dropped companion with a blank name".to_string());
            continue;
        }
        if companion.description.chars().count() > MAX_COMPANION_DESC_CHARS {
            companion.description = cap_chars(
                &companion.description,
                MAX_COMPANION_DESC_CHARS,
                &format!("description for companion '{}'", companion.name),
                warnings,
            );
        }
        if repaired
            .iter()
            .any(|existing| existing.same_type(&companion.name))
        {
            warnings.push(format!("dropped duplicate companion '{}'", companion.name));
        } else {
            repaired.push(companion);
        }
    }
    state.companions = repaired;
}

fn repair_journal(state: &mut GameState, warnings: &mut Vec<String>) {
    let mut repaired: Vec<String> = Vec::new();
    for fact in std::mem::take(&mut state.journal) {
        let fact = fact.trim();
        if fact.is_empty() {
            warnings.push("dropped blank journal entry".to_string());
            continue;
        }
        let fact = if fact.chars().count() > MAX_JOURNAL_FACT_CHARS {
            warnings.push(format!(
                "truncated journal entry to {MAX_JOURNAL_FACT_CHARS} characters"
            ));
            fact.chars().take(MAX_JOURNAL_FACT_CHARS).collect()
        } else {
            fact.to_string()
        };
        if repaired
            .iter()
            .any(|known| known.eq_ignore_ascii_case(&fact))
        {
            warnings.push(format!("dropped duplicate journal entry '{fact}'"));
        } else {
            repaired.push(fact);
        }
    }
    if repaired.len() > MAX_JOURNAL_ENTRIES {
        let excess = repaired.len() - MAX_JOURNAL_ENTRIES;
        warnings.push(format!(
            "trimmed journal from {} to {MAX_JOURNAL_ENTRIES} entries",
            repaired.len()
        ));
        repaired.drain(0..excess);
    }
    state.journal = repaired;
}

fn repair_story(state: &mut GameState, warnings: &mut Vec<String>) {
    repair_story_field(
        &mut state.objective,
        "objective",
        MAX_OBJECTIVE_CHARS,
        warnings,
    );
    repair_story_field(&mut state.threat, "threat", MAX_THREAT_CHARS, warnings);
    repair_story_field(
        &mut state.last_narration,
        "last_narration",
        MAX_NARRATION_CHARS,
        warnings,
    );
}

fn repair_story_field(
    field: &mut Option<String>,
    label: &str,
    max_chars: usize,
    warnings: &mut Vec<String>,
) {
    let Some(raw) = field.take() else {
        return;
    };
    match normalize_story_text(&raw, max_chars) {
        Some(cleaned) if cleaned == raw => *field = Some(cleaned),
        Some(cleaned) => {
            warnings.push(format!(
                "normalized {label} (trimmed/capped to {max_chars} characters)"
            ));
            *field = Some(cleaned);
        }
        None => warnings.push(format!("cleared blank {label}")),
    }
}

fn repair_exchanges(state: &mut GameState, warnings: &mut Vec<String>) {
    let mut repaired: Vec<Exchange> = Vec::new();
    for exchange in std::mem::take(&mut state.recent_exchanges) {
        let Some(player) = normalize_story_text(&exchange.player, MAX_EXCHANGE_PLAYER_CHARS) else {
            warnings.push("dropped recent exchange with a blank player action".to_string());
            continue;
        };
        let Some(dm) = normalize_story_text(&exchange.dm, MAX_EXCHANGE_DM_CHARS) else {
            warnings.push("dropped recent exchange with a blank DM reply".to_string());
            continue;
        };
        if player != exchange.player {
            warnings.push(format!(
                "normalized recent exchange player action (trimmed/capped to {MAX_EXCHANGE_PLAYER_CHARS} characters)"
            ));
        }
        if dm != exchange.dm {
            warnings.push(format!(
                "normalized recent exchange DM reply (trimmed/capped to {MAX_EXCHANGE_DM_CHARS} characters)"
            ));
        }
        repaired.push(Exchange { player, dm });
    }
    if repaired.len() > MAX_EXCHANGES {
        let excess = repaired.len() - MAX_EXCHANGES;
        warnings.push(format!(
            "trimmed recent exchanges from {} to {MAX_EXCHANGES} entries",
            repaired.len()
        ));
        repaired.drain(0..excess);
    }
    state.recent_exchanges = repaired;
}

fn repair_rooms(state: &mut GameState, warnings: &mut Vec<String>) {
    let mut repaired: Vec<String> = Vec::new();
    for room in std::mem::take(&mut state.visited_rooms) {
        let room = room.trim();
        if room.is_empty() {
            warnings.push("dropped blank visited room".to_string());
            continue;
        }
        let room = cap_chars(room, MAX_ROOM_NAME_CHARS, "visited room name", warnings);
        if room.is_empty() {
            warnings.push("dropped blank visited room".to_string());
            continue;
        }
        if repaired
            .iter()
            .any(|known| known.eq_ignore_ascii_case(&room))
        {
            warnings.push(format!("dropped duplicate visited room '{room}'"));
        } else {
            repaired.push(room);
        }
    }

    let current = state.current_room.trim().to_string();
    let current = cap_chars(&current, MAX_ROOM_NAME_CHARS, "current_room", warnings);
    if current.is_empty() {
        let fallback = repaired
            .first()
            .cloned()
            .unwrap_or_else(|| "Starting Chamber".to_string());
        warnings.push(format!("repaired blank current_room to '{fallback}'"));
        state.current_room = fallback;
    } else {
        if current != state.current_room {
            warnings.push(format!("normalized current_room to '{current}'"));
        }
        state.current_room = current;
    }

    if !repaired
        .iter()
        .any(|room| room.eq_ignore_ascii_case(&state.current_room))
    {
        warnings.push(format!(
            "added current_room '{}' to visited_rooms",
            state.current_room
        ));
        repaired.push(state.current_room.clone());
    }
    state.visited_rooms = repaired;
}

fn repair_combat(state: &mut GameState, warnings: &mut Vec<String>) {
    let mut order: Vec<String> = Vec::new();
    for name in std::mem::take(&mut state.initiative_order) {
        let name = name.trim();
        if name.is_empty() {
            warnings.push("dropped blank combat participant".to_string());
            continue;
        }
        if order.iter().any(|known| known.eq_ignore_ascii_case(name)) {
            warnings.push(format!("dropped duplicate combat participant '{name}'"));
        } else {
            order.push(name.to_string());
        }
    }
    if order.len() > MAX_COMBAT_PARTICIPANTS {
        warnings.push(format!(
            "trimmed initiative order to {MAX_COMBAT_PARTICIPANTS} participants"
        ));
        order.truncate(MAX_COMBAT_PARTICIPANTS);
    }

    let mut in_combat = state.in_combat && !order.is_empty();
    if state.in_combat && order.is_empty() {
        warnings.push("ended combat with an empty initiative order".to_string());
    }
    if !state.in_combat && !order.is_empty() {
        warnings.push("cleared stale initiative order outside combat".to_string());
    }

    if in_combat {
        let player = state.character_name.trim();
        if player.is_empty() || !order.iter().any(|name| name.eq_ignore_ascii_case(player)) {
            warnings.push(format!(
                "ended combat because the player '{}' was missing from initiative",
                state.character_name
            ));
            in_combat = false;
        } else if state.initiative_index >= order.len() {
            warnings.push(format!(
                "reset initiative_index {} to 0 (order has {} participants)",
                state.initiative_index,
                order.len()
            ));
            state.initiative_index = 0;
        }
    }

    if in_combat {
        state.in_combat = true;
        state.initiative_order = order;
    } else {
        state.in_combat = false;
        state.initiative_order.clear();
        state.initiative_index = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    #[test]
    fn resolve_path_defaults() {
        assert_eq!(SaveGame::resolve_path(None), "saves/autosave.json");
        assert_eq!(SaveGame::resolve_path(Some("")), "saves/autosave.json");
        assert_eq!(SaveGame::resolve_path(Some("   ")), "saves/autosave.json");
    }

    #[test]
    fn resolve_path_slot_names() {
        assert_eq!(SaveGame::resolve_path(Some("hero1")), "saves/hero1.json");
        assert_eq!(
            SaveGame::resolve_path(Some("my hero!")),
            "saves/my_hero_.json"
        );
    }

    #[test]
    fn resolve_path_explicit() {
        assert_eq!(SaveGame::resolve_path(Some("./run.json")), "./run.json");
        assert_eq!(SaveGame::resolve_path(Some("/tmp/cq.json")), "/tmp/cq.json");
        assert_eq!(SaveGame::resolve_path(Some("backup.json")), "backup.json");
    }

    #[test]
    fn save_load_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("roundtrip.json");

        let mut state = GameState::new("Thorin");
        state.hp = 12;
        state.current_room = "Hall".into();
        state.add_item("Axe", "Dwarven", 1);
        state.add_companion("Pip", "fairy");
        state.add_journal_entry("The door needs three keys");
        state
            .set_objective("Recover the Sunstone", Some("The eclipse begins in 3 days"))
            .unwrap();
        state.set_last_narration("The crypt door seals behind you.");
        state.record_exchange("enter the crypt", "The crypt door seals behind you.");
        state.record_exchange("light a torch", "Green flame catches on the old wood.");
        state.visit("Crypt");
        state.log("Player: go north");

        SaveGame::save(&state, &path).expect("save");
        assert!(path.exists());

        let loaded = SaveGame::load(&path).expect("load");
        assert_eq!(loaded.character_name, "Thorin");
        assert_eq!(loaded.hp, 12);
        assert_eq!(loaded.current_room, "Hall");
        assert!(loaded.find_item("Axe").is_some());
        assert!(loaded.find_companion("Pip").is_some());
        assert!(
            loaded
                .journal
                .iter()
                .any(|j| j == "The door needs three keys")
        );
        assert!(loaded.visited("Crypt"));
        assert!(loaded.game_log.iter().any(|e| e == "Player: go north"));
        assert_eq!(loaded.objective.as_deref(), Some("Recover the Sunstone"));
        assert_eq!(
            loaded.threat.as_deref(),
            Some("The eclipse begins in 3 days")
        );
        assert_eq!(
            loaded.last_narration.as_deref(),
            Some("The crypt door seals behind you.")
        );
        assert_eq!(
            loaded.recent_exchanges,
            vec![
                Exchange {
                    player: "enter the crypt".into(),
                    dm: "The crypt door seals behind you.".into(),
                },
                Exchange {
                    player: "light a torch".into(),
                    dm: "Green flame catches on the old wood.".into(),
                },
            ]
        );
    }

    #[test]
    fn save_writes_schema_envelope() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("envelope.json");
        SaveGame::save(&GameState::new("Envelope Hero"), &path).expect("save");

        let raw = fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["schema_version"].as_u64(), Some(1));
        assert_eq!(value["state"]["character_name"], "Envelope Hero");
        assert!(value.get("character_name").is_none());
    }

    #[test]
    fn loads_legacy_raw_game_state() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("legacy-raw.json");
        let mut state = GameState::new("Legacy");
        state.hp = 7;
        // Legacy saves predate `recent_exchanges`; the field must default.
        let mut raw: Value = serde_json::to_value(&state).unwrap();
        raw.as_object_mut().unwrap().remove("recent_exchanges");
        fs::write(&path, serde_json::to_string_pretty(&raw).unwrap()).unwrap();

        let (loaded, warnings) = SaveGame::load_with_warnings(&path).expect("legacy load");
        assert_eq!(loaded.character_name, "Legacy");
        assert_eq!(loaded.hp, 7);
        assert!(loaded.last_narration.is_none());
        assert!(loaded.recent_exchanges.is_empty());
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn save_load_ignores_legacy_scene_field() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("legacy.json");
        // Saves written before scene images were removed carry a stale field.
        fs::write(
            &path,
            r#"{
  "character_name": "Hero",
  "stats": {"STR":10,"DEX":10,"CON":10,"INT":10,"WIS":10,"CHA":10},
  "hp": 20,
  "max_hp": 20,
  "inventory": [],
  "companions": [],
  "journal": [],
  "current_room": "Starting Chamber",
  "visited_rooms": ["Starting Chamber"],
  "game_log": [],
  "last_scene_path": "cache/scenes/scene-hall.jpg"
}"#,
        )
        .unwrap();
        let loaded = SaveGame::load(&path).expect("legacy load");
        assert_eq!(loaded.character_name, "Hero");
        assert!(loaded.objective.is_none());
        assert!(loaded.threat.is_none());
        assert!(loaded.recent_exchanges.is_empty());
    }

    #[test]
    fn load_missing_file_errors() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing.json");
        let err = SaveGame::load(&path).unwrap_err();
        assert!(err.to_string().contains("No save file"));
    }

    #[test]
    fn load_invalid_json_errors() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.json");
        fs::write(&path, "{not valid json").unwrap();
        let err = SaveGame::load(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("parsing") || msg.contains("expected") || msg.contains("Failed"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn save_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("deep").join("slot.json");
        let state = GameState::new("Hero");
        SaveGame::save(&state, &path).expect("save nested");
        assert!(path.exists());
        let loaded = SaveGame::load(&path).unwrap();
        assert_eq!(loaded.character_name, "Hero");
    }

    #[test]
    fn load_repairs_impossible_state() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("broken.json");

        let mut state = GameState::new("Hero");
        state.max_hp = 0;
        state.hp = -7;
        state
            .inventory
            .push(InventoryItem::new("Potion", "red", -3));
        state
            .inventory
            .push(InventoryItem::new("potion", "more", i32::MAX));
        state.current_room = "   ".into();
        state.visited_rooms = vec!["Hall".into(), "hall".into(), "  ".into()];
        state.journal = vec!["Fact".into(), "fact".into(), "".into()];
        state.game_log = vec!["entry".into(); MAX_LOG_ENTRIES + 2];
        state.in_combat = true;
        state.initiative_order = vec!["Goblin".into()];
        state.initiative_index = 9;

        // Write as a legacy raw state to exercise the repair path.
        fs::write(&path, serde_json::to_string_pretty(&state).unwrap()).unwrap();
        let (loaded, warnings) = SaveGame::load_with_warnings(&path).expect("load broken");

        assert_eq!(loaded.max_hp, 1);
        assert_eq!(loaded.hp, 0);
        assert_eq!(loaded.inventory.len(), 1);
        assert_eq!(loaded.inventory[0].quantity, MAX_ITEM_QUANTITY);
        assert_eq!(loaded.current_room, "Hall");
        assert_eq!(loaded.visited_rooms, vec!["Hall"]);
        assert_eq!(loaded.journal, vec!["Fact"]);
        assert_eq!(loaded.game_log.len(), MAX_LOG_ENTRIES);
        assert!(!loaded.in_combat);
        assert!(loaded.initiative_order.is_empty());
        assert_eq!(loaded.initiative_index, 0);
        assert!(loaded.invariant_violations().is_empty());
        assert!(!warnings.is_empty());
        assert!(warnings.iter().any(|w| w.contains("max_hp")));
        assert!(warnings.iter().any(|w| w.contains("HP")));
        assert!(warnings.iter().any(|w| w.contains("quantity")));
        assert!(warnings.iter().any(|w| w.contains("current_room")));
        assert!(warnings.iter().any(|w| w.contains("combat")));
    }

    #[test]
    fn load_repairs_recent_exchanges() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("exchanges.json");

        let mut state = GameState::new("Hero");
        for i in 0..MAX_EXCHANGES + 2 {
            state.recent_exchanges.push(Exchange {
                player: format!("action {i}"),
                dm: format!("reply {i}"),
            });
        }
        state.recent_exchanges.push(Exchange {
            player: "  ".into(),
            dm: "lost reply".into(),
        });
        state.recent_exchanges.push(Exchange {
            player: "look".into(),
            dm: "   ".into(),
        });
        let long_player = format!("  {}  ", "p".repeat(MAX_EXCHANGE_PLAYER_CHARS + 10));
        let long_dm = format!("  {}  ", "d".repeat(MAX_EXCHANGE_DM_CHARS + 10));
        state.recent_exchanges.push(Exchange {
            player: long_player,
            dm: long_dm,
        });
        fs::write(&path, serde_json::to_string_pretty(&state).unwrap()).unwrap();

        let (loaded, warnings) = SaveGame::load_with_warnings(&path).expect("load exchanges");
        assert_eq!(loaded.recent_exchanges.len(), MAX_EXCHANGES);
        let latest = loaded.recent_exchanges.last().unwrap();
        assert_eq!(latest.player.chars().count(), MAX_EXCHANGE_PLAYER_CHARS);
        assert_eq!(latest.player, latest.player.trim());
        assert_eq!(latest.dm.chars().count(), MAX_EXCHANGE_DM_CHARS);
        assert_eq!(latest.dm, latest.dm.trim());
        assert!(
            loaded.recent_exchanges.iter().all(
                |exchange| !exchange.player.trim().is_empty() && !exchange.dm.trim().is_empty()
            )
        );
        assert!(loaded.invariant_violations().is_empty());
        assert!(
            warnings.iter().any(|w| w.contains("blank player action")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("blank DM reply")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("trimmed recent exchanges")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("player action") && w.contains("capped")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("DM reply") && w.contains("capped")),
            "{warnings:?}"
        );
    }

    #[test]
    fn load_repairs_overlong_and_blank_story_fields() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("story.json");

        let mut state = GameState::new("Hero");
        state.objective = Some(format!("  {}  ", "x".repeat(MAX_OBJECTIVE_CHARS + 20)));
        state.threat = Some("   ".into());
        state.last_narration = Some(format!("  {}  ", "n".repeat(MAX_NARRATION_CHARS + 20)));
        fs::write(&path, serde_json::to_string_pretty(&state).unwrap()).unwrap();

        let (loaded, warnings) = SaveGame::load_with_warnings(&path).expect("load story");
        let objective = loaded.objective.as_deref().expect("objective kept");
        assert_eq!(objective.chars().count(), MAX_OBJECTIVE_CHARS);
        assert_eq!(objective, objective.trim());
        assert!(loaded.threat.is_none());
        let narration = loaded.last_narration.as_deref().expect("narration kept");
        assert_eq!(narration.chars().count(), MAX_NARRATION_CHARS);
        assert_eq!(narration, narration.trim());
        assert!(loaded.invariant_violations().is_empty());
        assert!(warnings.iter().any(|w| w.contains("objective")));
        assert!(warnings.iter().any(|w| w.contains("threat")));
        assert!(warnings.iter().any(|w| w.contains("last_narration")));
    }

    #[test]
    fn atomic_save_keeps_backup_and_leaves_no_temp_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("hero.json");

        let first = GameState::new("First");
        SaveGame::save(&first, &path).expect("first save");

        let mut second = GameState::new("Second");
        second.hp = 10;
        SaveGame::save(&second, &path).expect("second save");

        let backup = backup_path(&path);
        assert!(backup.exists());
        let old = SaveGame::load(&backup).expect("load backup");
        assert_eq!(old.character_name, "First");
        let current = SaveGame::load(&path).expect("load current");
        assert_eq!(current.character_name, "Second");

        let leftovers: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn failed_save_cleans_up_temp_file() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("a-directory");
        fs::create_dir(&target).unwrap();

        let err = SaveGame::save(&GameState::new("Hero"), &target).unwrap_err();
        assert!(!err.to_string().is_empty());

        let leftovers: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_refuses_symlink_target() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().unwrap();
        let outside = dir.path().join("outside.json");
        fs::write(&outside, "keep me").unwrap();
        let link = dir.path().join("save.json");
        symlink(&outside, &link).unwrap();

        let err = SaveGame::save(&GameState::new("Hero"), &link).unwrap_err();
        assert!(
            format!("{err:#}").contains("symlink"),
            "unexpected error: {err:#}"
        );
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep me");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn load_repairs_overlong_field_values() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("overlong.json");

        let long_room = "r".repeat(MAX_ROOM_NAME_CHARS + 10);
        let long_item_name = "i".repeat(MAX_ITEM_NAME_CHARS + 10);
        let long_item_desc = "d".repeat(MAX_ITEM_DESC_CHARS + 10);
        let long_companion_name = "c".repeat(MAX_COMPANION_NAME_CHARS + 10);
        let long_companion_desc = "e".repeat(MAX_COMPANION_DESC_CHARS + 10);

        let mut state = GameState::new("Hero");
        state
            .inventory
            .push(InventoryItem::new(&long_item_name, &long_item_desc, 1));
        state
            .companions
            .push(Companion::new(&long_companion_name, &long_companion_desc));
        state.current_room = long_room.clone();
        state.visited_rooms = vec![long_room];
        fs::write(&path, serde_json::to_string_pretty(&state).unwrap()).unwrap();

        let (loaded, warnings) = SaveGame::load_with_warnings(&path).expect("load overlong");
        assert_eq!(
            loaded.inventory[0].name.chars().count(),
            MAX_ITEM_NAME_CHARS
        );
        assert_eq!(
            loaded.inventory[0].description.chars().count(),
            MAX_ITEM_DESC_CHARS
        );
        assert_eq!(
            loaded.companions[0].name.chars().count(),
            MAX_COMPANION_NAME_CHARS
        );
        assert_eq!(
            loaded.companions[0].description.chars().count(),
            MAX_COMPANION_DESC_CHARS
        );
        assert_eq!(loaded.current_room.chars().count(), MAX_ROOM_NAME_CHARS);
        assert!(
            loaded
                .visited_rooms
                .iter()
                .all(|room| room.chars().count() <= MAX_ROOM_NAME_CHARS)
        );
        assert!(loaded.invariant_violations().is_empty());
        assert!(
            warnings.iter().filter(|w| w.contains("truncated")).count() >= 5,
            "warnings: {warnings:?}"
        );
    }
}
