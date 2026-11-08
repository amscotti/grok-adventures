//! Save / load helpers for Grok Adventures.
//!
//! Persists the full [`GameState`] as pretty-printed JSON so a player can
//! resume mid-adventure. Slot names map to files under the local `saves/`
//! directory; explicit paths (containing `/` or ending in `.json`) are used
//! as-is.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::game::state::GameState;

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

    /// Write game state to disk. Creates parent directories as needed.
    pub fn save(state: &GameState, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating save directory {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(state).context("serializing game state")?;
        fs::write(path, json).with_context(|| format!("writing save to {}", path.display()))?;
        Ok(())
    }

    /// Load game state from disk.
    pub fn load(path: impl AsRef<Path>) -> Result<GameState> {
        let path = path.as_ref();
        if !path.exists() {
            bail!("No save file found at {}", path.display());
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading save from {}", path.display()))?;
        let state: GameState = serde_json::from_str(&raw)
            .with_context(|| format!("parsing save file {}", path.display()))?;
        Ok(state)
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
}
