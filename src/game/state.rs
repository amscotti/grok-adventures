//! Authoritative game state.
//!
//! The story can invent freely; numbers cannot quietly lie. HP, inventory,
//! companions, journal, rooms, and the game log live here. Grok mutates them
//! only through tools / markers that call these methods.

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};

/// Cap on recent event log entries kept for the DM context window.
pub const MAX_LOG_ENTRIES: usize = 20;

/// Cap on remembered journal facts.
pub const MAX_JOURNAL_ENTRIES: usize = 20;

/// A stackable inventory entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryItem {
    pub name: String,
    pub description: String,
    pub quantity: i32,
}

impl InventoryItem {
    pub fn new(name: impl Into<String>, description: impl Into<String>, quantity: i32) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            quantity,
        }
    }

    /// Case-insensitive type match for stacking.
    pub fn same_type(&self, other_name: &str) -> bool {
        self.name.eq_ignore_ascii_case(other_name)
    }
}

impl fmt::Display for InventoryItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.quantity > 1 {
            write!(
                f,
                "{} (x{}) - {}",
                self.name, self.quantity, self.description
            )
        } else {
            write!(f, "{} - {}", self.name, self.description)
        }
    }
}

/// A named NPC traveling with the player.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Companion {
    pub name: String,
    pub description: String,
}

impl Companion {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }

    pub fn same_type(&self, other_name: &str) -> bool {
        self.name.eq_ignore_ascii_case(other_name)
    }
}

impl fmt::Display for Companion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} - {}", self.name, self.description)
    }
}

/// One starting class kit offered at character creation.
///
/// Single source of truth: [`GameState::apply_class_kit`] applies these
/// numbers, and the creation UI previews the same entry, so the picker can
/// never promise stats the kit does not grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassKit {
    /// Lowercase key accepted on the CLI / typed prompt (`"fighter"`).
    pub key: &'static str,
    /// Display name stored in [`GameState::class`] (`"Fighter"`).
    pub display: &'static str,
    /// Short menu tagline.
    pub tagline: &'static str,
    /// One-line overview shown while the class is highlighted.
    pub blurb: &'static str,
    /// What the class is best at, in a few words.
    pub best_for: &'static str,
    /// Ability scores in STR, DEX, CON, INT, WIS, CHA order.
    pub stats: [i32; 6],
    pub max_hp: i32,
    pub item: &'static str,
    pub item_desc: &'static str,
}

impl ClassKit {
    /// `STR 14 · DEX 12 · ...` preview line in stable D&D order.
    pub fn stats_line(&self) -> String {
        const ORDER: [&str; 6] = ["STR", "DEX", "CON", "INT", "WIS", "CHA"];
        ORDER
            .iter()
            .zip(self.stats.iter())
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// The class kits offered at character creation, in menu order.
pub const CLASS_KITS: &[ClassKit] = &[
    ClassKit {
        key: "fighter",
        display: "Fighter",
        tagline: "Front-line warrior",
        blurb: "Stand in the doorway and stay standing. Tough, direct, dependable.",
        best_for: "Holding the line, duels, surviving mistakes",
        stats: [14, 12, 14, 8, 10, 10],
        max_hp: 24,
        item: "Longsword",
        item_desc: "A sturdy blade for a warrior",
    },
    ClassKit {
        key: "rogue",
        display: "Rogue",
        tagline: "Stealth and precision",
        blurb: "Strike from the shadows, slip past danger, open what stays locked.",
        best_for: "Sneaking, traps, first strikes",
        stats: [10, 14, 12, 12, 10, 10],
        max_hp: 18,
        item: "Dagger",
        item_desc: "A sharp, concealable blade",
    },
    ClassKit {
        key: "wizard",
        display: "Wizard",
        tagline: "Arcane caster",
        blurb: "Fragile, brilliant, and armed with answers nobody else has.",
        best_for: "Spells, lore, solving the impossible",
        stats: [8, 12, 10, 14, 12, 10],
        max_hp: 14,
        item: "Spellbook",
        item_desc: "A worn tome of cantrips and first-circle spells",
    },
    ClassKit {
        key: "cleric",
        display: "Cleric",
        tagline: "Divine healer",
        blurb: "A blessed symbol, a steady hand, and power that mends or smites.",
        best_for: "Healing, undead, keeping everyone alive",
        stats: [12, 10, 12, 10, 14, 12],
        max_hp: 20,
        item: "Holy Symbol",
        item_desc: "A blessed emblem of your faith",
    },
];

/// Look up a class kit by key or display name (trimmed, case-insensitive).
pub fn class_kit(class: &str) -> Option<&'static ClassKit> {
    let key = class.trim().to_ascii_lowercase();
    CLASS_KITS
        .iter()
        .find(|kit| kit.key == key || kit.display.to_ascii_lowercase() == key)
}

/// Full mutable adventure state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameState {
    pub character_name: String,
    /// Optional class from character creation (fighter/rogue/wizard/cleric).
    #[serde(default)]
    pub class: Option<String>,
    pub stats: HashMap<String, i32>,
    pub hp: i32,
    pub max_hp: i32,
    pub inventory: Vec<InventoryItem>,
    pub companions: Vec<Companion>,
    pub journal: Vec<String>,
    pub current_room: String,
    pub visited_rooms: Vec<String>,
    pub game_log: Vec<String>,
    /// Whether initiative combat is active.
    #[serde(default)]
    pub in_combat: bool,
    /// Turn order (participant names) while in combat.
    #[serde(default)]
    pub initiative_order: Vec<String>,
    /// Index into [`Self::initiative_order`] for whose turn it is.
    #[serde(default)]
    pub initiative_index: usize,
}

fn default_stats() -> HashMap<String, i32> {
    HashMap::from([
        ("STR".to_string(), 10),
        ("DEX".to_string(), 10),
        ("CON".to_string(), 10),
        ("INT".to_string(), 10),
        ("WIS".to_string(), 10),
        ("CHA".to_string(), 10),
    ])
}

impl GameState {
    /// Create a fresh adventurer in the Starting Chamber.
    pub fn new(character_name: impl Into<String>) -> Self {
        let current_room = "Starting Chamber".to_string();
        Self {
            character_name: character_name.into(),
            class: None,
            stats: default_stats(),
            hp: 20,
            max_hp: 20,
            inventory: Vec::new(),
            companions: Vec::new(),
            journal: Vec::new(),
            current_room: current_room.clone(),
            visited_rooms: vec![current_room],
            game_log: Vec::new(),
            in_combat: false,
            initiative_order: Vec::new(),
            initiative_index: 0,
        }
    }

    /// HP restored by a short rest: `max(1, max_hp / 4)`.
    pub fn short_rest_heal_amount(&self) -> i32 {
        (self.max_hp / 4).max(1)
    }

    /// Short rest: heal [`Self::short_rest_heal_amount`] HP (clamped to max). Logs the rest.
    /// Fails if dead.
    pub fn short_rest(&mut self) -> String {
        if !self.alive() {
            return "Cannot rest while dead. Use /resurrect first.".to_string();
        }
        if self.hp >= self.max_hp {
            self.log("Short rest (already at full HP)");
            return format!(
                "You catch your breath but are already at full strength (HP {}/{}).",
                self.hp, self.max_hp
            );
        }
        let amount = self.short_rest_heal_amount();
        let msg = self.modify_hp(amount);
        self.log(format!("Short rest: recovered up to {amount} HP"));
        format!("Short rest. {msg}")
    }

    /// Long rest: restore HP to `max_hp`. Fails if dead.
    pub fn long_rest(&mut self) -> String {
        if !self.alive() {
            return "Cannot long rest while dead. Use /resurrect first.".to_string();
        }
        let old = self.hp;
        self.hp = self.max_hp;
        self.log("Long rest: HP fully restored");
        if old >= self.max_hp {
            format!(
                "You rest through the night, already at full strength (HP {}/{}).",
                self.hp, self.max_hp
            )
        } else {
            format!(
                "Long rest. You wake refreshed — HP: {old} → {}/{}",
                self.hp, self.max_hp
            )
        }
    }

    /// Restore 1 HP if dead so the session can continue (testing / meta).
    pub fn resurrect(&mut self) -> String {
        if self.alive() {
            return format!(
                "{} is not dead (HP {}/{}).",
                self.character_name, self.hp, self.max_hp
            );
        }
        self.hp = 1;
        self.log(format!("{} resurrected with 1 HP", self.character_name));
        format!(
            "✨ {} rises again with 1 HP ({} / {}). The adventure can continue.",
            self.character_name, self.hp, self.max_hp
        )
    }

    /// Start initiative combat with the given participant names (includes the player).
    pub fn start_combat(&mut self, participants: Vec<String>) -> String {
        let names: Vec<String> = participants
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if names.is_empty() {
            return "Cannot start combat with no participants".to_string();
        }
        self.in_combat = true;
        self.initiative_order = names;
        self.initiative_index = 0;
        let order = self.initiative_order.join(", ");
        let current = self.current_turn_actor().unwrap_or("?").to_string();
        self.log(format!("Combat started: {order}"));
        format!("⚔️ Combat started!\nInitiative order: {order}\nCurrent turn: {current}")
    }

    /// Advance to the next creature in initiative order (wraps).
    pub fn advance_turn(&mut self) -> String {
        if !self.in_combat || self.initiative_order.is_empty() {
            return "Not in combat".to_string();
        }
        let prev = self.current_turn_actor().unwrap_or("?").to_string();
        self.initiative_index = (self.initiative_index + 1) % self.initiative_order.len();
        let current = self.current_turn_actor().unwrap_or("?").to_string();
        self.log(format!("Turn advanced: {prev} → {current}"));
        format!("Turn advanced. Now acting: {current}")
    }

    /// End combat and clear initiative state.
    pub fn end_combat(&mut self) -> String {
        if !self.in_combat {
            return "Not in combat".to_string();
        }
        self.in_combat = false;
        self.initiative_order.clear();
        self.initiative_index = 0;
        self.log("Combat ended");
        "Combat ended.".to_string()
    }

    /// Name of the participant whose turn it is, if any.
    pub fn current_turn_actor(&self) -> Option<&str> {
        if !self.in_combat || self.initiative_order.is_empty() {
            return None;
        }
        let idx = self.initiative_index % self.initiative_order.len();
        self.initiative_order.get(idx).map(|s| s.as_str())
    }

    /// Apply a starting class kit (fighter / rogue / wizard / cleric).
    /// Sets modest stats, max HP, starting item, and `class`.
    pub fn apply_class_kit(&mut self, class: &str) -> Result<String, String> {
        let Some(kit) = class_kit(class) else {
            return Err(format!(
                "Unknown class '{}'. Choose fighter, rogue, wizard, or cleric.",
                class.trim().to_ascii_lowercase()
            ));
        };
        let (display, stats, max_hp, item, item_desc) =
            (kit.display, kit.stats, kit.max_hp, kit.item, kit.item_desc);

        self.class = Some(display.to_string());
        self.stats = HashMap::from([
            ("STR".to_string(), stats[0]),
            ("DEX".to_string(), stats[1]),
            ("CON".to_string(), stats[2]),
            ("INT".to_string(), stats[3]),
            ("WIS".to_string(), stats[4]),
            ("CHA".to_string(), stats[5]),
        ]);
        self.max_hp = max_hp;
        self.hp = max_hp;
        let item_msg = self.add_item(item, item_desc, 1);
        self.log(format!("Chose class: {display}"));
        Ok(format!(
            "Class set to {display}. HP {max_hp}/{max_hp}. Stats: {}. {item_msg}",
            self.stats_string()
        ))
    }

    /// Whether `room` appears in `visited_rooms` (case-insensitive).
    pub fn visited(&self, room: &str) -> bool {
        self.visited_rooms
            .iter()
            .any(|r| r.eq_ignore_ascii_case(room))
    }

    /// Mark a room as visited (no-op if already present, case-insensitive).
    pub fn visit(&mut self, room: impl Into<String>) {
        let room = room.into();
        if !self.visited(&room) {
            self.visited_rooms.push(room);
        }
    }

    /// Append a log entry, trimming to [`MAX_LOG_ENTRIES`].
    pub fn log(&mut self, entry: impl Into<String>) {
        self.game_log.push(entry.into());
        while self.game_log.len() > MAX_LOG_ENTRIES {
            self.game_log.remove(0);
        }
    }

    /// Most recent `count` log entries (or fewer if the log is shorter).
    pub fn recent_log(&self, count: usize) -> &[String] {
        let start = self.game_log.len().saturating_sub(count);
        &self.game_log[start..]
    }

    /// Find an inventory item by name (case-insensitive).
    pub fn find_item(&self, name: &str) -> Option<&InventoryItem> {
        self.inventory.iter().find(|i| i.same_type(name))
    }

    fn find_item_mut(&mut self, name: &str) -> Option<&mut InventoryItem> {
        self.inventory.iter_mut().find(|i| i.same_type(name))
    }

    /// Add an item (stacks by case-insensitive name). Rejects blank names and
    /// non-positive quantities.
    pub fn add_item(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        quantity: i32,
    ) -> String {
        let name = name.into();
        let name = name.trim();
        if name.is_empty() {
            return "Item name cannot be blank".to_string();
        }
        if quantity <= 0 {
            return "Quantity must be positive".to_string();
        }

        if let Some(existing) = self.find_item_mut(name) {
            existing.quantity += quantity;
            let total = existing.quantity;
            format!("Added {quantity}x {name} (now have {total})")
        } else {
            self.inventory
                .push(InventoryItem::new(name, description, quantity));
            format!("Added {quantity}x {name} to inventory")
        }
    }

    /// Remove quantity of an item. Rejects non-positive quantities.
    pub fn remove_item(&mut self, name: &str, quantity: i32) -> String {
        if quantity <= 0 {
            return "Quantity must be positive".to_string();
        }

        let Some(idx) = self.inventory.iter().position(|i| i.same_type(name)) else {
            return format!("Item '{name}' not found in inventory");
        };

        let existing = &mut self.inventory[idx];
        if existing.quantity <= quantity {
            let removed_name = existing.name.clone();
            self.inventory.remove(idx);
            format!("Removed {removed_name} from inventory")
        } else {
            existing.quantity -= quantity;
            let remaining = existing.quantity;
            format!("Removed {quantity}x {name} ({remaining} remaining)")
        }
    }

    /// Find a companion by name (case-insensitive).
    pub fn find_companion(&self, name: &str) -> Option<&Companion> {
        self.companions.iter().find(|c| c.same_type(name))
    }

    fn find_companion_mut(&mut self, name: &str) -> Option<&mut Companion> {
        self.companions.iter_mut().find(|c| c.same_type(name))
    }

    /// Add a companion. Same-name re-add updates description instead of duplicating.
    pub fn add_companion(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> String {
        let name = name.into();
        let name = name.trim();
        if name.is_empty() {
            return "Companion name cannot be blank".to_string();
        }
        let description = description.into();

        if let Some(existing) = self.find_companion_mut(name) {
            existing.description = description;
            let n = existing.name.clone();
            format!("Companion {n} already present (description updated)")
        } else {
            self.companions.push(Companion::new(name, description));
            format!("Added companion {name}")
        }
    }

    /// Remove a companion by name.
    pub fn remove_companion(&mut self, name: &str) -> String {
        if let Some(idx) = self.companions.iter().position(|c| c.same_type(name)) {
            let removed = self.companions.remove(idx);
            format!("Removed companion {}", removed.name)
        } else {
            format!("Companion '{name}' not found")
        }
    }

    /// Update a companion's name and/or description. Refuses renames that
    /// would collide with a different existing companion.
    pub fn update_companion(
        &mut self,
        name: &str,
        new_name: Option<&str>,
        description: Option<&str>,
    ) -> String {
        // Resolve index first to avoid borrow conflicts while scanning.
        let Some(idx) = self.companions.iter().position(|c| c.same_type(name)) else {
            return format!("Companion '{name}' not found");
        };

        if let Some(new_name) = new_name {
            let cleaned = new_name.trim();
            if !cleaned.is_empty() {
                let existing_name = self.companions[idx].name.clone();
                if let Some(conflict) = self.find_companion(cleaned)
                    && !conflict.same_type(&existing_name)
                {
                    return format!(
                        "Cannot rename to '{cleaned}': a companion with that name already exists"
                    );
                }
                self.companions[idx].name = cleaned.to_string();
            }
        }

        if let Some(description) = description
            && !description.trim().is_empty()
        {
            self.companions[idx].description = description.to_string();
        }

        format!("Updated companion {}", self.companions[idx].name)
    }

    /// Record a remembered fact (case-insensitive de-dupe, size-capped).
    pub fn add_journal_entry(&mut self, entry: impl Into<String>) -> String {
        let entry = entry.into();
        let fact = entry.trim();
        if fact.is_empty() {
            return "Journal entry cannot be blank".to_string();
        }
        if self
            .journal
            .iter()
            .any(|known| known.eq_ignore_ascii_case(fact))
        {
            return format!("Already remembered: {fact}");
        }
        self.journal.push(fact.to_string());
        while self.journal.len() > MAX_JOURNAL_ENTRIES {
            self.journal.remove(0);
        }
        format!("Remembered: {fact}")
    }

    /// Move the player to a new room. Rejects blank names.
    pub fn move_to(&mut self, room: impl Into<String>) -> String {
        let room = room.into();
        let room = room.trim();
        if room.is_empty() {
            return "Room name cannot be blank".to_string();
        }

        let old_room = self.current_room.clone();
        let first_visit = !self.visited(room);
        self.current_room = room.to_string();
        self.visit(room);

        let mut msg = format!("Moved from {old_room} to {room}");
        if first_visit {
            msg.push_str(" (first visit!)");
        }
        msg
    }

    /// Modify HP with clamping to `[0, max_hp]`.
    pub fn modify_hp(&mut self, amount: i32) -> String {
        let old_hp = self.hp;
        self.hp = (self.hp + amount).clamp(0, self.max_hp);

        if amount < 0 {
            format!(
                "💔 Took {} damage! HP: {old_hp} → {}",
                amount.unsigned_abs(),
                self.hp
            )
        } else if amount > 0 {
            format!("💚 Healed {amount} HP! HP: {old_hp} → {}", self.hp)
        } else {
            format!("HP unchanged: {}", self.hp)
        }
    }

    /// Whether the character is still alive (`hp > 0`).
    pub fn alive(&self) -> bool {
        self.hp > 0
    }

    /// Replace this state's fields with those from another (load-game swap).
    pub fn replace_with(&mut self, other: GameState) {
        *self = other;
    }

    /// Consistency checks for autopilot / stress tests.
    pub fn invariant_violations(&self) -> Vec<String> {
        let mut violations = Vec::new();

        if !(0..=self.max_hp).contains(&self.hp) {
            violations.push(format!(
                "HP {} is outside valid range [0, {}]",
                self.hp, self.max_hp
            ));
        }

        let mut seen = HashSet::new();
        for item in &self.inventory {
            if item.quantity <= 0 {
                violations.push(format!(
                    "Item '{}' has non-positive quantity ({})",
                    item.name, item.quantity
                ));
            }
            let key = item.name.to_ascii_lowercase();
            if !seen.insert(key) {
                violations.push(format!("Duplicate item '{}' failed to stack", item.name));
            }
        }

        let mut seen_companions = HashSet::new();
        for companion in &self.companions {
            if companion.name.trim().is_empty() {
                violations.push("Companion has a blank name".to_string());
            }
            let key = companion.name.to_ascii_lowercase();
            if !seen_companions.insert(key) {
                violations.push(format!(
                    "Duplicate companion '{}' not merged",
                    companion.name
                ));
            }
        }

        if self.current_room.trim().is_empty() {
            violations.push("current_room is blank".to_string());
        }

        for room in &self.visited_rooms {
            if room.trim().is_empty() {
                violations.push("visited_rooms contains a blank entry".to_string());
            }
        }

        if self.game_log.len() > MAX_LOG_ENTRIES {
            violations.push(format!(
                "game_log has {} entries (max {MAX_LOG_ENTRIES})",
                self.game_log.len()
            ));
        }

        if self.journal.len() > MAX_JOURNAL_ENTRIES {
            violations.push(format!(
                "journal has {} entries (max {MAX_JOURNAL_ENTRIES})",
                self.journal.len()
            ));
        }

        violations
    }

    /// Format ability scores for display (`STR: 10, DEX: 10, ...`).
    pub fn stats_string(&self) -> String {
        // Stable D&D order when present; append any extras alphabetically.
        const ORDER: &[&str] = &["STR", "DEX", "CON", "INT", "WIS", "CHA"];
        let mut parts = Vec::new();
        let mut used = HashSet::new();
        for key in ORDER {
            if let Some(v) = self.stats.get(*key) {
                parts.push(format!("{key}: {v}"));
                used.insert(*key);
            }
        }
        let mut extras: Vec<_> = self
            .stats
            .iter()
            .filter(|(k, _)| !used.contains(k.as_str()))
            .collect();
        extras.sort_by(|a, b| a.0.cmp(b.0));
        for (k, v) in extras {
            parts.push(format!("{k}: {v}"));
        }
        parts.join(", ")
    }

    /// Dynamic system-prompt context from authoritative state.
    pub fn to_context_string(&self) -> String {
        let mut str = String::new();
        str.push_str(&format!("Character: {}\n", self.character_name));
        if let Some(ref class) = self.class {
            str.push_str(&format!("Class: {class}\n"));
        }
        str.push_str(&format!("HP: {}/{}\n", self.hp, self.max_hp));
        str.push_str(&format!("Location: {}\n", self.current_room));
        str.push_str(&format!("Stats: {}\n", self.stats_string()));
        if self.in_combat {
            str.push_str("Combat: ACTIVE\n");
            str.push_str(&format!(
                "Initiative order: {}\n",
                self.initiative_order.join(", ")
            ));
            if let Some(actor) = self.current_turn_actor() {
                str.push_str(&format!(
                    "Current turn: {actor} (#{})\n",
                    self.initiative_index + 1
                ));
            }
        } else {
            str.push_str("Combat: none\n");
        }
        str.push_str("\nInventory:\n");
        if self.inventory.is_empty() {
            str.push_str("  (empty)\n");
        } else {
            for item in &self.inventory {
                str.push_str(&format!("  - {item}\n"));
            }
        }
        str.push_str("\nCompanions:\n");
        if self.companions.is_empty() {
            str.push_str("  (none)\n");
        } else {
            for companion in &self.companions {
                str.push_str(&format!("  - {companion}\n"));
            }
        }
        str.push_str("\nRemembered facts (journal):\n");
        if self.journal.is_empty() {
            str.push_str("  (none)\n");
        } else {
            for fact in &self.journal {
                str.push_str(&format!("  - {fact}\n"));
            }
        }
        str.push_str("\nVisited rooms: ");
        str.push_str(&self.visited_rooms.join(", "));
        str.push('\n');
        str
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn new_has_defaults() {
        let state = GameState::new("Adventurer");
        assert_eq!(state.character_name, "Adventurer");
        assert_eq!(state.hp, 20);
        assert_eq!(state.max_hp, 20);
        assert_eq!(state.current_room, "Starting Chamber");
        assert!(state.inventory.is_empty());
        assert_eq!(state.stats.get("STR"), Some(&10));
        assert!(state.visited("Starting Chamber"));
        assert!(state.class.is_none());
        assert!(!state.in_combat);
        assert!(state.initiative_order.is_empty());
        assert_eq!(state.initiative_index, 0);
    }

    #[test]
    fn short_rest_heals_quarter_max() {
        let mut state = GameState::new("A");
        state.max_hp = 20;
        state.hp = 5;
        assert_eq!(state.short_rest_heal_amount(), 5); // max(1, 20/4)
        let msg = state.short_rest();
        assert_eq!(state.hp, 10);
        assert!(msg.contains("Short rest"));
        assert!(state.game_log.iter().any(|e| e.contains("Short rest")));

        // Full HP: no overheal
        state.hp = 20;
        let msg = state.short_rest();
        assert_eq!(state.hp, 20);
        assert!(msg.contains("full"));

        // Dead: reject
        state.hp = 0;
        let msg = state.short_rest();
        assert_eq!(state.hp, 0);
        assert!(msg.contains("dead"));
    }

    #[test]
    fn short_rest_minimum_one_hp() {
        let mut state = GameState::new("A");
        state.max_hp = 3;
        state.hp = 1;
        assert_eq!(state.short_rest_heal_amount(), 1);
        state.short_rest();
        assert_eq!(state.hp, 2);
    }

    #[test]
    fn long_rest_restores_full_hp() {
        let mut state = GameState::new("A");
        state.hp = 3;
        let msg = state.long_rest();
        assert_eq!(state.hp, state.max_hp);
        assert!(msg.contains("Long rest"));
        assert!(msg.contains("3 →"));

        state.hp = 0;
        let msg = state.long_rest();
        assert_eq!(state.hp, 0);
        assert!(msg.contains("dead"));
    }

    #[test]
    fn resurrect_from_death() {
        let mut state = GameState::new("Hero");
        state.hp = 0;
        let msg = state.resurrect();
        assert_eq!(state.hp, 1);
        assert!(state.alive());
        assert!(msg.contains("rises") || msg.contains("1 HP"));

        let msg = state.resurrect();
        assert_eq!(state.hp, 1);
        assert!(msg.contains("not dead"));
    }

    #[test]
    fn combat_initiative_cycles() {
        let mut state = GameState::new("Hero");
        let msg = state.start_combat(vec!["Hero".into(), "Goblin".into(), "Wolf".into()]);
        assert!(state.in_combat);
        assert_eq!(state.initiative_order.len(), 3);
        assert_eq!(state.current_turn_actor(), Some("Hero"));
        assert!(msg.contains("Goblin"));

        let msg = state.advance_turn();
        assert_eq!(state.current_turn_actor(), Some("Goblin"));
        assert!(msg.contains("Goblin"));

        state.advance_turn();
        assert_eq!(state.current_turn_actor(), Some("Wolf"));
        state.advance_turn();
        assert_eq!(state.current_turn_actor(), Some("Hero"));
        assert_eq!(state.initiative_index, 0);

        let msg = state.end_combat();
        assert!(!state.in_combat);
        assert!(state.initiative_order.is_empty());
        assert_eq!(state.initiative_index, 0);
        assert!(msg.contains("ended"));

        assert!(state.advance_turn().contains("Not in combat"));
        assert!(state.end_combat().contains("Not in combat"));
        assert!(state.start_combat(vec![]).contains("no participants"));
        assert!(
            state
                .start_combat(vec!["  ".into()])
                .contains("no participants")
        );
    }

    #[test]
    fn class_kits_apply_stats_and_items() {
        for (class, item, str_expected, max_hp) in [
            ("fighter", "Longsword", 14, 24),
            ("rogue", "Dagger", 10, 18),
            ("wizard", "Spellbook", 8, 14),
            ("cleric", "Holy Symbol", 12, 20),
        ] {
            let mut state = GameState::new("Hero");
            let msg = state.apply_class_kit(class).unwrap();
            assert_eq!(
                state.class.as_deref(),
                Some(match class {
                    "fighter" => "Fighter",
                    "rogue" => "Rogue",
                    "wizard" => "Wizard",
                    "cleric" => "Cleric",
                    _ => unreachable!(),
                })
            );
            assert_eq!(state.stats.get("STR"), Some(&str_expected));
            assert_eq!(state.max_hp, max_hp);
            assert_eq!(state.hp, max_hp);
            assert!(state.find_item(item).is_some());
            assert!(msg.contains(item) || msg.contains("Class set"));
        }

        let mut state = GameState::new("Hero");
        assert!(state.apply_class_kit("bard").is_err());
    }

    #[test]
    fn class_kit_catalog_lookup() {
        assert_eq!(CLASS_KITS.len(), 4);
        let mut keys: Vec<&str> = CLASS_KITS.iter().map(|k| k.key).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["cleric", "fighter", "rogue", "wizard"]);

        // Lookup is case-insensitive and accepts display names.
        assert_eq!(class_kit("fighter").unwrap().display, "Fighter");
        assert_eq!(class_kit("  WIZARD ").unwrap().key, "wizard");
        assert_eq!(class_kit("Cleric").unwrap().max_hp, 20);
        assert!(class_kit("bard").is_none());
        assert!(class_kit("").is_none());

        // Every catalog entry applies cleanly and matches its own numbers.
        for kit in CLASS_KITS {
            let mut state = GameState::new("Hero");
            state.apply_class_kit(kit.key).unwrap();
            assert_eq!(state.class.as_deref(), Some(kit.display));
            assert_eq!(state.max_hp, kit.max_hp);
            assert!(state.find_item(kit.item).is_some());
            assert!(kit.stats_line().contains("STR"));
        }
    }

    #[test]
    fn serde_defaults_for_new_fields() {
        // Old saves without class/combat fields still deserialize.
        let json = r#"{
            "character_name": "Legacy",
            "stats": {"STR": 10, "DEX": 10, "CON": 10, "INT": 10, "WIS": 10, "CHA": 10},
            "hp": 10,
            "max_hp": 20,
            "inventory": [],
            "companions": [],
            "journal": [],
            "current_room": "Hall",
            "visited_rooms": ["Hall"],
            "game_log": []
        }"#;
        let state: GameState = serde_json::from_str(json).unwrap();
        assert_eq!(state.character_name, "Legacy");
        assert!(state.class.is_none());
        assert!(!state.in_combat);
        assert!(state.initiative_order.is_empty());
        assert_eq!(state.initiative_index, 0);
    }

    #[test]
    fn to_context_includes_combat_and_class() {
        let mut state = GameState::new("A");
        state.apply_class_kit("wizard").unwrap();
        state.start_combat(vec!["A".into(), "Ooze".into()]);
        let ctx = state.to_context_string();
        assert!(ctx.contains("Class: Wizard"));
        assert!(ctx.contains("Combat: ACTIVE"));
        assert!(ctx.contains("Initiative order:"));
        assert!(ctx.contains("Current turn: A"));
    }

    #[test]
    fn custom_name() {
        let state = GameState::new("Thorin");
        assert_eq!(state.character_name, "Thorin");
    }

    #[test]
    fn visited_case_insensitive() {
        let state = GameState::new("A");
        // Starting Chamber is pre-visited
        let mut state = state;
        state.current_room = "Dungeon".into();
        state.visited_rooms = vec!["Dungeon".into()];
        assert!(state.visited("dungeon"));
        assert!(state.visited("DUNGEON"));
        assert!(!state.visited("Forest"));
    }

    #[test]
    fn visit_does_not_duplicate() {
        let mut state = GameState::new("A");
        state.current_room = "Cave".into();
        state.visited_rooms = vec!["Cave".into()];
        state.visit("Cave");
        state.visit("cave");
        assert_eq!(state.visited_rooms.len(), 1);
    }

    #[test]
    fn modify_hp_damage_heal_clamp() {
        let mut state = GameState::new("A");
        let result = state.modify_hp(-5);
        assert_eq!(state.hp, 15);
        assert!(result.contains("damage"));

        let result = state.modify_hp(3);
        assert_eq!(state.hp, 18);
        assert!(result.contains("Healed"));

        state.modify_hp(100);
        assert_eq!(state.hp, 20);

        state.modify_hp(-100);
        assert_eq!(state.hp, 0);
    }

    #[test]
    fn inventory_stack_and_remove() {
        let mut state = GameState::new("A");
        state.add_item("Sword", "Sharp blade", 1);
        assert_eq!(state.inventory.len(), 1);
        assert!(state.find_item("Sword").is_some());

        state.add_item("Arrow", "Wooden", 10);
        state.add_item("Arrow", "Wooden", 5);
        assert_eq!(state.inventory.len(), 2);
        assert_eq!(state.find_item("Arrow").unwrap().quantity, 15);

        state.remove_item("Arrow", 1);
        assert_eq!(state.find_item("Arrow").unwrap().quantity, 14);

        state.add_item("Key", "Rusty", 1);
        state.remove_item("Key", 1);
        assert!(state.find_item("Key").is_none());
    }

    #[test]
    fn inventory_rejects_bad_inputs() {
        let mut state = GameState::new("A");
        let r = state.add_item("   ", "nothing", 1);
        assert!(state.inventory.is_empty());
        assert!(r.contains("blank"));

        let r = state.add_item("Sword", "Sharp", 0);
        assert!(state.inventory.is_empty());
        assert!(r.contains("positive"));

        let r = state.add_item("Sword", "Sharp", -3);
        assert!(state.inventory.is_empty());
        assert!(r.contains("positive"));

        state.add_item("Arrow", "Wooden", 5);
        let r = state.remove_item("Arrow", -2);
        assert_eq!(state.find_item("Arrow").unwrap().quantity, 5);
        assert!(r.contains("positive"));
    }

    #[test]
    fn move_to_tracks_visits() {
        let mut state = GameState::new("A");
        state.current_room = "Entrance".into();
        state.visited_rooms = vec!["Entrance".into()];

        let result = state.move_to("Crypt");
        assert_eq!(state.current_room, "Crypt");
        assert!(state.visited("Crypt"));
        assert!(result.contains("first visit"));

        let result = state.move_to("Crypt");
        assert!(!result.contains("first visit"));

        let result = state.move_to("   ");
        assert_eq!(state.current_room, "Crypt");
        assert!(result.contains("blank"));

        state.move_to("  Hall  ");
        assert_eq!(state.current_room, "Hall");
    }

    #[test]
    fn companions_lifecycle() {
        let mut state = GameState::new("A");
        state.add_companion("Skritchnik", "imp familiar");
        assert!(state.find_companion("skritchnik").is_some());
        assert_eq!(state.companions.len(), 1);

        state.add_companion("imp", "updated desc");
        // "imp" is a different name from "Skritchnik"
        assert_eq!(state.companions.len(), 2);

        // Merge same name
        state.add_companion("Imp", "familiar");
        state.add_companion("imp", "updated desc");
        assert_eq!(
            state.find_companion("Imp").unwrap().description,
            "updated desc"
        );

        state.remove_companion("pip");
        assert!(state.find_companion("Pip").is_none());

        state.add_companion("Pip", "fairy");
        state.remove_companion("pip");
        assert!(state.find_companion("Pip").is_none());
    }

    #[test]
    fn companion_merge_same_name() {
        let mut state = GameState::new("A");
        state.add_companion("Imp", "familiar");
        state.add_companion("imp", "updated desc");
        assert_eq!(state.companions.len(), 1);
        assert_eq!(
            state.find_companion("Imp").unwrap().description,
            "updated desc"
        );
    }

    #[test]
    fn update_companion_rename_and_collision() {
        let mut state = GameState::new("A");
        state.add_companion("Bob", "generic");
        state.update_companion("Bob", Some("Robert"), Some("the brave"));
        assert!(state.find_companion("Bob").is_none());
        assert_eq!(
            state.find_companion("Robert").unwrap().description,
            "the brave"
        );

        let mut state = GameState::new("A");
        state.add_companion("Bob", "fighter");
        state.add_companion("Pip", "fairy");
        let result = state.update_companion("Bob", Some("Pip"), None);
        assert!(result.contains("already exists"));
        assert!(state.find_companion("Bob").is_some());
        assert_eq!(state.companions.len(), 2);

        // Case-only rename of same companion
        let mut state = GameState::new("A");
        state.add_companion("bob", "fighter");
        let result = state.update_companion("bob", Some("Bob"), None);
        assert!(result.contains("Updated"));
        assert_eq!(state.companions.len(), 1);
        assert_eq!(state.find_companion("Bob").unwrap().name, "Bob");
    }

    #[test]
    fn companion_blank_name() {
        let mut state = GameState::new("A");
        let result = state.add_companion("  ", "ghost");
        assert!(state.companions.is_empty());
        assert!(result.contains("blank"));
    }

    #[test]
    fn journal_dedupe_and_cap() {
        let mut state = GameState::new("A");
        state.add_journal_entry("Your patron is Zarathra");
        assert!(state.journal.iter().any(|j| j == "Your patron is Zarathra"));

        let r = state.add_journal_entry("   ");
        assert!(r.contains("blank"));

        state.add_journal_entry("The fountain is a trap");
        let r = state.add_journal_entry("the fountain is a trap");
        assert_eq!(state.journal.len(), 2);
        assert!(r.contains("Already remembered"));

        let mut state = GameState::new("A");
        for i in 0..(MAX_JOURNAL_ENTRIES + 5) {
            state.add_journal_entry(format!("fact {i}"));
        }
        assert_eq!(state.journal.len(), MAX_JOURNAL_ENTRIES);
        assert_eq!(state.journal[0], "fact 5");
    }

    #[test]
    fn log_maintains_max_size() {
        let mut state = GameState::new("A");
        for i in 0..25 {
            state.log(format!("Entry {i}"));
        }
        assert_eq!(state.game_log.len(), MAX_LOG_ENTRIES);
        assert_eq!(state.game_log[0], "Entry 5");
        assert_eq!(state.recent_log(2).len(), 2);
        assert_eq!(state.recent_log(2)[1], "Entry 24");
    }

    #[test]
    fn alive_flag() {
        let mut state = GameState::new("A");
        state.hp = 1;
        assert!(state.alive());
        state.hp = 0;
        assert!(!state.alive());
    }

    #[test]
    fn json_round_trip() {
        let mut state = GameState::new("Test");
        state.hp = 15;
        state.add_item("Sword", "Sharp blade", 1);
        state.add_companion("Skritchnik", "imp familiar");
        state.add_journal_entry("Patron is Zarathra");
        state.visit("Forest");
        state.log("Test entry");

        let json = serde_json::to_string(&state).unwrap();
        let restored: GameState = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.character_name, "Test");
        assert_eq!(restored.hp, 15);
        assert_eq!(restored.max_hp, 20);
        assert_eq!(restored.inventory.len(), 1);
        assert!(restored.find_item("Sword").is_some());
        assert!(restored.find_companion("Skritchnik").is_some());
        assert!(restored.journal.iter().any(|j| j == "Patron is Zarathra"));
        assert!(restored.visited("Forest"));
        assert!(restored.game_log.iter().any(|e| e == "Test entry"));
    }

    #[test]
    fn to_context_includes_visited() {
        let mut state = GameState::new("A");
        state.current_room = "Entrance".into();
        state.visited_rooms = vec!["Entrance".into()];
        state.visit("Crypt");
        state.visit("Hall");
        let ctx = state.to_context_string();
        assert!(ctx.contains("Visited rooms:"));
        assert!(ctx.contains("Entrance"));
        assert!(ctx.contains("Crypt"));
        assert!(ctx.contains("Hall"));
    }

    #[test]
    fn inventory_item_display() {
        let single = InventoryItem::new("Sword", "A sharp blade", 1);
        assert_eq!(format!("{single}"), "Sword - A sharp blade");
        let stacked = InventoryItem::new("Arrow", "Wooden arrow", 20);
        assert_eq!(format!("{stacked}"), "Arrow (x20) - Wooden arrow");
    }

    #[test]
    fn companion_display_and_same_type() {
        let c = Companion::new("Pip", "fairy companion");
        assert_eq!(format!("{c}"), "Pip - fairy companion");
        assert!(c.same_type("pip"));
        assert!(!c.same_type("Zix"));
    }

    #[test]
    fn replace_with_copies_fields() {
        let mut target = GameState::new("Old");
        let mut source = GameState::new("New");
        source.hp = 5;
        source.max_hp = 30;
        source.current_room = "Cave".into();
        source.add_item("Key", "Brass", 1);
        source.add_companion("Wolf", "loyal");
        source.add_journal_entry("Beware the lagoon");

        target.replace_with(source);
        assert_eq!(target.character_name, "New");
        assert_eq!(target.hp, 5);
        assert_eq!(target.max_hp, 30);
        assert_eq!(target.current_room, "Cave");
        assert!(target.find_item("Key").is_some());
        assert!(target.find_companion("Wolf").is_some());
        assert!(target.journal.iter().any(|j| j == "Beware the lagoon"));
    }

    #[test]
    fn invariant_violations_cover_cases() {
        let state = GameState::new("A");
        assert!(state.invariant_violations().is_empty());

        let mut state = GameState::new("A");
        state.hp = -1;
        assert!(
            state
                .invariant_violations()
                .iter()
                .any(|v| v.contains("outside valid range"))
        );
        state.hp = 25;
        assert!(
            state
                .invariant_violations()
                .iter()
                .any(|v| v.contains("outside valid range"))
        );

        let mut state = GameState::new("A");
        state
            .inventory
            .push(InventoryItem::new("Ghost", "should not exist", 0));
        assert!(
            state
                .invariant_violations()
                .iter()
                .any(|v| v.contains("non-positive quantity"))
        );

        let mut state = GameState::new("A");
        state.inventory.push(InventoryItem::new("Potion", "x", 1));
        state.inventory.push(InventoryItem::new("potion", "x", 1));
        assert!(
            state
                .invariant_violations()
                .iter()
                .any(|v| v.contains("failed to stack"))
        );

        let mut state = GameState::new("A");
        state.current_room = "   ".into();
        assert!(
            state
                .invariant_violations()
                .iter()
                .any(|v| v.contains("current_room is blank"))
        );

        let mut state = GameState::new("A");
        state.visited_rooms.push("  ".into());
        assert!(
            state
                .invariant_violations()
                .iter()
                .any(|v| v.contains("blank entry"))
        );

        let mut state = GameState::new("A");
        state.game_log = vec!["overflow".into(); MAX_LOG_ENTRIES + 5];
        assert!(
            state
                .invariant_violations()
                .iter()
                .any(|v| v.contains("game_log"))
        );

        let mut state = GameState::new("A");
        state.companions.push(Companion::new("Imp", "x"));
        state.companions.push(Companion::new("imp", "y"));
        assert!(
            state
                .invariant_violations()
                .iter()
                .any(|v| v.contains("Duplicate companion"))
        );

        let mut state = GameState::new("A");
        state.companions.push(Companion::new("  ", "no name"));
        assert!(
            state
                .invariant_violations()
                .iter()
                .any(|v| v.contains("blank name"))
        );

        let mut state = GameState::new("A");
        state.journal = vec!["fact".into(); MAX_JOURNAL_ENTRIES + 3];
        assert!(
            state
                .invariant_violations()
                .iter()
                .any(|v| v.contains("journal"))
        );

        let mut state = GameState::new("A");
        state.hp = -5;
        state.current_room.clear();
        state.inventory.push(InventoryItem::new("Bad", "x", -1));
        assert!(state.invariant_violations().len() >= 3);
    }
}
