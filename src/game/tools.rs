//! Game tools exposed to the DM.
//!
//! Definitions serialize to OpenAI/xAI function-calling format. Handlers mutate
//! or read [`GameState`] through its authoritative methods only.

use serde::Serialize;
use serde_json::{Value, json};

use crate::game::state::GameState;
use crate::util::dice;

/// Tools that mutate authoritative state. When any of these ran this turn,
/// marker fallback is skipped to avoid double-counting.
pub const MUTATING_TOOLS: &[&str] = &[
    "modify_hp",
    "add_to_inventory",
    "remove_from_inventory",
    "move_to_room",
    "add_companion",
    "update_companion",
    "remove_companion",
    "remember",
    "start_combat",
    "advance_turn",
    "end_combat",
];

/// All tool names for reference / tests.
pub const TOOL_NAMES: &[&str] = &[
    "roll_dice",
    "get_character_stats",
    "modify_hp",
    "get_inventory",
    "add_to_inventory",
    "remove_from_inventory",
    "inspect_item",
    "get_current_room",
    "move_to_room",
    "add_companion",
    "update_companion",
    "remove_companion",
    "get_companions",
    "remember",
    "get_journal",
    "start_combat",
    "advance_turn",
    "end_combat",
];

/// Whether `name` is a state-mutating tool.
pub fn is_mutating(name: &str) -> bool {
    MUTATING_TOOLS.contains(&name)
}

/// Result of dispatching a single tool call.
///
/// `content` is the human-readable tool message. `is_error` marks invalid
/// args / failed mutations; the string is still suitable for a `role: tool`
/// message so the model can recover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

impl std::fmt::Display for ToolResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.content)
    }
}

impl From<ToolResult> for String {
    fn from(r: ToolResult) -> Self {
        r.content
    }
}

/// OpenAI/xAI-compatible tool definition wrapper.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub type_: String,
    pub function: FunctionDef,
}

/// Function schema nested under a tool definition.
#[derive(Debug, Clone, Serialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolDef {
    ToolDef {
        type_: "function".to_string(),
        function: FunctionDef {
            name: name.to_string(),
            description: description.to_string(),
            parameters: json!({
                "type": "object",
                "properties": properties,
                "required": required,
            }),
        },
    }
}

/// JSON-schema style tool definitions for the chat completions API.
pub fn tool_definitions() -> Vec<ToolDef> {
    vec![
        tool(
            "roll_dice",
            "Roll dice for skill checks, combat, or random outcomes. \
             Standard dice: d4, d6, d8, d10, d12, d20, d100. \
             Returns individual rolls and total. \
             Optional ability (STR/DEX/CON/INT/WIS/CHA) adds the character's \
             dnd5e ability modifier floor((score-10)/2) on top of any explicit modifier \
             (e.g. proficiency). Use ability for ability checks, saves, and attack rolls \
             tied to a score.",
            json!({
                "sides": {
                    "type": "integer",
                    "description": "Number of sides on each die (4, 6, 8, 10, 12, 20, or 100)"
                },
                "count": {
                    "type": "integer",
                    "description": "Number of dice to roll (default: 1)"
                },
                "modifier": {
                    "type": "integer",
                    "description": "Extra modifier to add to total (default: 0; proficiency, magic, etc.)"
                },
                "ability": {
                    "type": "string",
                    "description": "Ability score to apply as dnd5e modifier: STR, DEX, CON, INT, WIS, or CHA"
                }
            }),
            &["sides"],
        ),
        tool(
            "get_character_stats",
            "Get the player's current character information including \
             name, HP, and attribute scores (STR, DEX, CON, INT, WIS, CHA).",
            json!({}),
            &[],
        ),
        tool(
            "modify_hp",
            "Modify the player's HP. Use negative values for damage, \
             positive values for healing. HP is clamped between 0 and max_hp.",
            json!({
                "amount": {
                    "type": "integer",
                    "description": "HP change amount (negative = damage, positive = healing)"
                }
            }),
            &["amount"],
        ),
        tool(
            "get_inventory",
            "List all items currently in the player's inventory.",
            json!({}),
            &[],
        ),
        tool(
            "add_to_inventory",
            "Add an item to the player's inventory. If the item already \
             exists, it will be stacked (quantity increased).",
            json!({
                "item": {
                    "type": "string",
                    "description": "Name of the item to add"
                },
                "description": {
                    "type": "string",
                    "description": "Brief description of the item"
                },
                "quantity": {
                    "type": "integer",
                    "description": "Quantity to add (default: 1)"
                }
            }),
            &["item", "description"],
        ),
        tool(
            "remove_from_inventory",
            "Remove an item from the player's inventory. Specify quantity \
             for partial removal from stacks.",
            json!({
                "item": {
                    "type": "string",
                    "description": "Name of the item to remove"
                },
                "quantity": {
                    "type": "integer",
                    "description": "Quantity to remove (default: 1)"
                }
            }),
            &["item"],
        ),
        tool(
            "inspect_item",
            "Get detailed information about a specific item in inventory. \
             Use this BEFORE describing item contents to ensure consistency.",
            json!({
                "item_name": {
                    "type": "string",
                    "description": "Name of the item to inspect"
                }
            }),
            &["item_name"],
        ),
        tool(
            "get_current_room",
            "Get the player's current location/room name.",
            json!({}),
            &[],
        ),
        tool(
            "move_to_room",
            "Move the player to a new room/location. The DM should use \
             this when the player travels to a new area.",
            json!({
                "room_name": {
                    "type": "string",
                    "description": "Name of the destination room/area"
                }
            }),
            &["room_name"],
        ),
        tool(
            "add_companion",
            "Register a named NPC that is now traveling with the player \
             (familiar, hireling, summoned creature, pet, etc.). Call this \
             THE FIRST TIME a named companion appears, using the exact name \
             you intend to keep. The companion list is shown to you each turn, \
             so reuse those exact names and never rename an existing companion.",
            json!({
                "name": {
                    "type": "string",
                    "description": "The companion's exact name"
                },
                "description": {
                    "type": "string",
                    "description": "Short description, e.g. 'imp familiar bound to the player'"
                }
            }),
            &["name", "description"],
        ),
        tool(
            "update_companion",
            "Update an existing companion's name and/or description \
             (e.g. a true name is revealed, or a relationship changes).",
            json!({
                "name": {
                    "type": "string",
                    "description": "Current name of the companion to update"
                },
                "new_name": {
                    "type": "string",
                    "description": "New name (optional)"
                },
                "description": {
                    "type": "string",
                    "description": "New description (optional)"
                }
            }),
            &["name"],
        ),
        tool(
            "remove_companion",
            "Remove a companion (e.g. it departs, is dismissed, or dies).",
            json!({
                "name": {
                    "type": "string",
                    "description": "Name of the companion to remove"
                }
            }),
            &["name"],
        ),
        tool(
            "get_companions",
            "List the named companions currently traveling with the player.",
            json!({}),
            &[],
        ),
        tool(
            "remember",
            "Record an important fact so it persists across turns and \
             stays consistent — e.g. a named entity not traveling with the \
             player ('Your patron is Zarathra, a demon'), a quest goal, a \
             password, a clue, or world lore. The journal is shown to you \
             each turn; treat it as authoritative and do not contradict it.",
            json!({
                "fact": {
                    "type": "string",
                    "description": "A concise fact to remember, phrased as a statement"
                }
            }),
            &["fact"],
        ),
        tool(
            "get_journal",
            "List all remembered facts in the journal.",
            json!({}),
            &[],
        ),
        tool(
            "start_combat",
            "Begin initiative combat with an ordered list of participants \
             (include the player by name). Sets combat state so turns can \
             advance with advance_turn. Call when a fight begins.",
            json!({
                "participants": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Names in initiative order (player and foes/allies)"
                }
            }),
            &["participants"],
        ),
        tool(
            "advance_turn",
            "Advance initiative to the next participant in combat order. \
             Call after a creature finishes its turn.",
            json!({}),
            &[],
        ),
        tool(
            "end_combat",
            "End the current combat encounter and clear initiative order.",
            json!({}),
            &[],
        ),
    ]
}

fn arg_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_f64().map(|f| f as i64))
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

fn arg_u32(args: &Value, key: &str) -> Option<u32> {
    arg_i64(args, key).and_then(|n| u32::try_from(n).ok())
}

fn arg_i32(args: &Value, key: &str) -> Option<i32> {
    arg_i64(args, key).and_then(|n| i32::try_from(n).ok())
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

fn missing(param: &str) -> ToolResult {
    ToolResult::err(format!("Missing required parameter: {param}"))
}

/// Execute a named tool against `state` with JSON `args`.
///
/// Returns [`ToolResult`] whose `content` is suitable for a `role: tool` message.
pub fn dispatch(state: &mut GameState, name: &str, args: &Value) -> ToolResult {
    match name {
        "roll_dice" => dispatch_roll_dice(state, args),
        "get_character_stats" => dispatch_get_character_stats(state),
        "modify_hp" => dispatch_modify_hp(state, args),
        "get_inventory" => dispatch_get_inventory(state),
        "add_to_inventory" => dispatch_add_to_inventory(state, args),
        "remove_from_inventory" => dispatch_remove_from_inventory(state, args),
        "inspect_item" => dispatch_inspect_item(state, args),
        "get_current_room" => dispatch_get_current_room(state),
        "move_to_room" => dispatch_move_to_room(state, args),
        "add_companion" => dispatch_add_companion(state, args),
        "update_companion" => dispatch_update_companion(state, args),
        "remove_companion" => dispatch_remove_companion(state, args),
        "get_companions" => dispatch_get_companions(state),
        "remember" => dispatch_remember(state, args),
        "get_journal" => dispatch_get_journal(state),
        "start_combat" => dispatch_start_combat(state, args),
        "advance_turn" => dispatch_advance_turn(state),
        "end_combat" => dispatch_end_combat(state),
        other => ToolResult::err(format!("Unknown tool: {other}")),
    }
}

fn dispatch_roll_dice(state: &GameState, args: &Value) -> ToolResult {
    let Some(sides) = arg_u32(args, "sides") else {
        return missing("sides");
    };
    let count = arg_u32(args, "count").unwrap_or(1);
    let explicit_mod = arg_i32(args, "modifier").unwrap_or(0);

    let mut ability_note = String::new();
    let mut ability_mod = 0i32;
    if let Some(ability_raw) = arg_str(args, "ability") {
        let ability = ability_raw.trim().to_uppercase();
        if !matches!(
            ability.as_str(),
            "STR" | "DEX" | "CON" | "INT" | "WIS" | "CHA"
        ) {
            return ToolResult::err(format!(
                "Unknown ability '{ability_raw}'. Use STR, DEX, CON, INT, WIS, or CHA."
            ));
        }
        let Some(&score) = state.stats.get(&ability) else {
            return ToolResult::err(format!("Character has no {ability} score."));
        };
        ability_mod = dice::ability_modifier(score);
        ability_note = format!(" [{ability} {score} → {ability_mod:+}]");
    }

    let modifier = explicit_mod + ability_mod;

    match dice::roll(sides, count, modifier) {
        Ok(roll) => {
            let mut result = format!("🎲 Rolled {count}d{sides}");
            if modifier > 0 {
                result.push_str(&format!("+{modifier}"));
            } else if modifier < 0 {
                result.push_str(&modifier.to_string());
            }
            result.push_str(&ability_note);
            result.push_str(": ");
            if count > 1 {
                let parts: Vec<String> = roll.rolls.iter().map(|r| r.to_string()).collect();
                result.push('[');
                result.push_str(&parts.join(", "));
                result.push(']');
                if modifier != 0 {
                    if modifier > 0 {
                        result.push_str(&format!(" + {modifier}"));
                    } else {
                        result.push_str(&format!(" - {}", modifier.unsigned_abs()));
                    }
                }
                result.push_str(" = ");
            }
            result.push_str(&roll.total.to_string());
            ToolResult::ok(result)
        }
        Err(e) => ToolResult::err(e.to_string()),
    }
}

fn dispatch_get_character_stats(state: &GameState) -> ToolResult {
    let class_line = state
        .class
        .as_ref()
        .map(|c| format!("\nClass: {c}"))
        .unwrap_or_default();
    let combat_line = if state.in_combat {
        let actor = state.current_turn_actor().unwrap_or("?");
        format!(
            "\nCombat: ACTIVE | order: {} | turn: {actor}",
            state.initiative_order.join(", ")
        )
    } else {
        String::new()
    };
    ToolResult::ok(format!(
        "📊 {}{class_line}\nHP: {}/{}\nStats: {}{combat_line}",
        state.character_name,
        state.hp,
        state.max_hp,
        state.stats_string()
    ))
}

fn dispatch_modify_hp(state: &mut GameState, args: &Value) -> ToolResult {
    let Some(amount) = arg_i32(args, "amount") else {
        return missing("amount");
    };
    let mut result = state.modify_hp(amount);
    if !state.alive() {
        result.push_str(&format!(
            "\n☠️ {} has fallen! (Player may type /resurrect to continue testing.)",
            state.character_name
        ));
    }
    ToolResult::ok(result)
}

fn dispatch_start_combat(state: &mut GameState, args: &Value) -> ToolResult {
    let Some(arr) = args.get("participants").and_then(|v| v.as_array()) else {
        return missing("participants");
    };
    let participants: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    let result = state.start_combat(participants);
    if result.starts_with("Cannot") {
        ToolResult::err(format!("❌ {result}"))
    } else {
        ToolResult::ok(result)
    }
}

fn dispatch_advance_turn(state: &mut GameState) -> ToolResult {
    let result = state.advance_turn();
    if result.starts_with("Not in combat") {
        ToolResult::err(format!("❌ {result}"))
    } else {
        ToolResult::ok(format!("⏭️ {result}"))
    }
}

fn dispatch_end_combat(state: &mut GameState) -> ToolResult {
    let result = state.end_combat();
    if result.starts_with("Not in combat") {
        ToolResult::err(format!("❌ {result}"))
    } else {
        ToolResult::ok(format!("🏳️ {result}"))
    }
}

fn dispatch_get_inventory(state: &GameState) -> ToolResult {
    if state.inventory.is_empty() {
        return ToolResult::ok("🎒 Inventory is empty.");
    }
    let mut result = String::from("🎒 Inventory:\n");
    for item in &state.inventory {
        result.push_str(&format!("  • {item}\n"));
    }
    ToolResult::ok(result)
}

fn dispatch_add_to_inventory(state: &mut GameState, args: &Value) -> ToolResult {
    let Some(item) = arg_str(args, "item") else {
        return missing("item");
    };
    let Some(description) = arg_str(args, "description") else {
        return missing("description");
    };
    let quantity = arg_i32(args, "quantity").unwrap_or(1);

    let result = state.add_item(item, description, quantity);
    if result.starts_with("Added") {
        state.log(format!("Acquired: {}", item.trim()));
        ToolResult::ok(format!("✅ {result}"))
    } else {
        ToolResult::err(format!("❌ {result}"))
    }
}

fn dispatch_remove_from_inventory(state: &mut GameState, args: &Value) -> ToolResult {
    let Some(item) = arg_str(args, "item") else {
        return missing("item");
    };
    let quantity = arg_i32(args, "quantity").unwrap_or(1);

    let result = state.remove_item(item, quantity);
    if result.starts_with("Removed") {
        state.log(format!("Used/dropped: {item}"));
        ToolResult::ok(format!("✅ {result}"))
    } else {
        ToolResult::err(format!("❌ {result}"))
    }
}

fn dispatch_inspect_item(state: &GameState, args: &Value) -> ToolResult {
    let Some(item_name) = arg_str(args, "item_name") else {
        return missing("item_name");
    };
    if let Some(item) = state.find_item(item_name) {
        ToolResult::ok(format!(
            "🔍 {} (x{}): {}",
            item.name, item.quantity, item.description
        ))
    } else {
        ToolResult::err(format!("❌ Item '{item_name}' not found in inventory."))
    }
}

fn dispatch_get_current_room(state: &GameState) -> ToolResult {
    ToolResult::ok(format!("📍 Current location: {}", state.current_room))
}

fn dispatch_move_to_room(state: &mut GameState, args: &Value) -> ToolResult {
    let Some(room_name) = arg_str(args, "room_name") else {
        return missing("room_name");
    };
    let old_room = state.current_room.clone();
    let result = state.move_to(room_name);
    if result.contains("cannot be blank") {
        ToolResult::err(format!("❌ {result}"))
    } else {
        state.log(format!("Traveled: {old_room} → {}", state.current_room));
        ToolResult::ok(format!("🚶 {result}"))
    }
}

fn dispatch_add_companion(state: &mut GameState, args: &Value) -> ToolResult {
    let Some(name) = arg_str(args, "name") else {
        return missing("name");
    };
    let Some(description) = arg_str(args, "description") else {
        return missing("description");
    };
    let result = state.add_companion(name, description);
    if result.contains("cannot be blank") {
        ToolResult::err(format!("❌ {result}"))
    } else {
        state.log(format!("Companion joined: {}", name.trim()));
        ToolResult::ok(format!("✅ {result}"))
    }
}

fn dispatch_update_companion(state: &mut GameState, args: &Value) -> ToolResult {
    let Some(name) = arg_str(args, "name") else {
        return missing("name");
    };
    let new_name = arg_str(args, "new_name");
    let description = arg_str(args, "description");
    let result = state.update_companion(name, new_name, description);
    if result.starts_with("Updated") {
        ToolResult::ok(format!("✅ {result}"))
    } else {
        ToolResult::err(format!("❌ {result}"))
    }
}

fn dispatch_remove_companion(state: &mut GameState, args: &Value) -> ToolResult {
    let Some(name) = arg_str(args, "name") else {
        return missing("name");
    };
    let result = state.remove_companion(name);
    if result.contains("not found") {
        ToolResult::err(format!("❌ {result}"))
    } else {
        state.log(format!("Companion departed: {name}"));
        ToolResult::ok(format!("✅ {result}"))
    }
}

fn dispatch_get_companions(state: &GameState) -> ToolResult {
    if state.companions.is_empty() {
        return ToolResult::ok("👥 No companions currently.");
    }
    let mut result = String::from("👥 Companions:\n");
    for companion in &state.companions {
        result.push_str(&format!("  • {companion}\n"));
    }
    ToolResult::ok(result)
}

fn dispatch_remember(state: &mut GameState, args: &Value) -> ToolResult {
    let Some(fact) = arg_str(args, "fact") else {
        return missing("fact");
    };
    let result = state.add_journal_entry(fact);
    if result.starts_with("Remembered:") || result.starts_with("Already remembered:") {
        ToolResult::ok(format!("📝 {result}"))
    } else {
        ToolResult::err(format!("❌ {result}"))
    }
}

fn dispatch_get_journal(state: &GameState) -> ToolResult {
    if state.journal.is_empty() {
        return ToolResult::ok("📝 Journal is empty.");
    }
    let mut result = String::from("📝 Journal:\n");
    for fact in &state.journal {
        result.push_str(&format!("  • {fact}\n"));
    }
    ToolResult::ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn fresh() -> GameState {
        GameState::new("Hero")
    }

    #[test]
    fn tool_definitions_cover_all_names() {
        let defs = tool_definitions();
        assert_eq!(defs.len(), TOOL_NAMES.len());
        let names: Vec<_> = defs.iter().map(|d| d.function.name.as_str()).collect();
        for expected in TOOL_NAMES {
            assert!(names.contains(expected), "missing tool def for {expected}");
        }
        for def in &defs {
            assert_eq!(def.type_, "function");
            assert!(!def.function.description.is_empty());
            assert_eq!(
                def.function.parameters.get("type").and_then(|t| t.as_str()),
                Some("object")
            );
        }
    }

    #[test]
    fn tool_definitions_serialize_openai_shape() {
        let defs = tool_definitions();
        let json = serde_json::to_value(&defs[0]).unwrap();
        assert_eq!(json["type"], "function");
        assert!(json["function"]["name"].is_string());
        assert!(json["function"]["parameters"]["properties"].is_object());
    }

    #[test]
    fn is_mutating_matches_const_list() {
        assert!(is_mutating("modify_hp"));
        assert!(is_mutating("add_to_inventory"));
        assert!(is_mutating("remember"));
        assert!(!is_mutating("roll_dice"));
        assert!(!is_mutating("get_inventory"));
        assert!(!is_mutating("get_character_stats"));
        assert!(!is_mutating("get_journal"));
        assert!(is_mutating("start_combat"));
        assert!(is_mutating("advance_turn"));
        assert!(is_mutating("end_combat"));
        assert_eq!(MUTATING_TOOLS.len(), 11);
    }

    #[test]
    fn combat_tools_cycle_order() {
        let mut state = fresh();
        let start = dispatch(
            &mut state,
            "start_combat",
            &json!({"participants": ["Hero", "Goblin", "Wolf"]}),
        );
        assert!(!start.is_error);
        assert!(state.in_combat);
        assert_eq!(state.current_turn_actor(), Some("Hero"));

        let adv = dispatch(&mut state, "advance_turn", &json!({}));
        assert!(!adv.is_error);
        assert_eq!(state.current_turn_actor(), Some("Goblin"));

        dispatch(&mut state, "advance_turn", &json!({}));
        dispatch(&mut state, "advance_turn", &json!({}));
        assert_eq!(state.current_turn_actor(), Some("Hero"));

        let end = dispatch(&mut state, "end_combat", &json!({}));
        assert!(!end.is_error);
        assert!(!state.in_combat);

        let bad = dispatch(&mut state, "advance_turn", &json!({}));
        assert!(bad.is_error);
    }

    #[test]
    fn roll_dice_basic() {
        let mut state = fresh();
        let result = dispatch(&mut state, "roll_dice", &json!({"sides": 20}));
        assert!(!result.is_error);
        assert!(result.content.contains("🎲"));
        assert!(result.content.contains("1d20"));
    }

    #[test]
    fn roll_dice_with_count_and_modifier() {
        let mut state = fresh();
        let result = dispatch(
            &mut state,
            "roll_dice",
            &json!({"sides": 6, "count": 2, "modifier": 3}),
        );
        assert!(!result.is_error);
        assert!(result.content.contains("2d6+3"));
        assert!(result.content.contains('='));
    }

    #[test]
    fn roll_dice_rejects_invalid_sides() {
        let mut state = fresh();
        let result = dispatch(&mut state, "roll_dice", &json!({"sides": 7}));
        assert!(result.is_error);
        assert!(result.content.contains("Invalid dice"));
    }

    #[test]
    fn roll_dice_missing_sides() {
        let mut state = fresh();
        let result = dispatch(&mut state, "roll_dice", &json!({}));
        assert!(result.is_error);
        assert!(result.content.contains("sides"));
    }

    #[test]
    fn roll_dice_ability_adds_modifier() {
        let mut state = fresh();
        state.stats.insert("STR".into(), 16); // +3
        let result = dispatch(
            &mut state,
            "roll_dice",
            &json!({"sides": 20, "ability": "STR"}),
        );
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("STR"));
        assert!(
            result.content.contains("+3") || result.content.contains("→ +3"),
            "{}",
            result.content
        );
        // Total must equal face + 3
        let total: i32 = result
            .content
            .rsplit(':')
            .next()
            .and_then(|s| s.trim().parse().ok())
            .expect("parse total");
        // d20 is 1..=20 so total with +3 is 4..=23
        assert!((4..=23).contains(&total), "total={total}");
    }

    #[test]
    fn roll_dice_ability_stacks_with_explicit_modifier() {
        let mut state = fresh();
        state.stats.insert("DEX".into(), 14); // +2
        let result = dispatch(
            &mut state,
            "roll_dice",
            &json!({"sides": 20, "ability": "dex", "modifier": 2}),
        );
        assert!(!result.is_error, "{}", result.content);
        // Combined +4 in expression
        assert!(
            result.content.contains("+4") || result.content.contains("1d20+4"),
            "{}",
            result.content
        );
    }

    #[test]
    fn roll_dice_rejects_unknown_ability() {
        let mut state = fresh();
        let result = dispatch(
            &mut state,
            "roll_dice",
            &json!({"sides": 20, "ability": "LUCK"}),
        );
        assert!(result.is_error);
        assert!(result.content.contains("Unknown ability"));
    }

    #[test]
    fn get_character_stats() {
        let mut state = fresh();
        let result = dispatch(&mut state, "get_character_stats", &json!({}));
        assert!(!result.is_error);
        assert!(result.content.contains("Hero"));
        assert!(result.content.contains("HP: 20/20"));
        assert!(result.content.contains("STR"));
    }

    #[test]
    fn modify_hp_damage_and_heal() {
        let mut state = fresh();
        let result = dispatch(&mut state, "modify_hp", &json!({"amount": -5}));
        assert!(!result.is_error);
        assert_eq!(state.hp, 15);
        assert!(result.content.contains("damage"));

        let result = dispatch(&mut state, "modify_hp", &json!({"amount": 2}));
        assert_eq!(state.hp, 17);
        assert!(result.content.contains("Healed"));
    }

    #[test]
    fn modify_hp_death_message() {
        let mut state = fresh();
        let result = dispatch(&mut state, "modify_hp", &json!({"amount": -100}));
        assert_eq!(state.hp, 0);
        assert!(!state.alive());
        assert!(result.content.contains("fallen"));
    }

    #[test]
    fn inventory_add_get_inspect_remove() {
        let mut state = fresh();

        let empty = dispatch(&mut state, "get_inventory", &json!({}));
        assert!(empty.content.contains("empty"));

        let add = dispatch(
            &mut state,
            "add_to_inventory",
            &json!({
                "item": "Arrow",
                "description": "Wooden arrow",
                "quantity": 5
            }),
        );
        assert!(!add.is_error);
        assert!(add.content.contains("Added"));
        assert_eq!(state.find_item("Arrow").unwrap().quantity, 5);

        dispatch(
            &mut state,
            "add_to_inventory",
            &json!({
                "item": "arrow",
                "description": "Wooden arrow",
                "quantity": 3
            }),
        );
        assert_eq!(state.find_item("Arrow").unwrap().quantity, 8);

        let inv = dispatch(&mut state, "get_inventory", &json!({}));
        assert!(inv.content.contains("Arrow"));

        let inspect = dispatch(&mut state, "inspect_item", &json!({"item_name": "Arrow"}));
        assert!(!inspect.is_error);
        assert!(inspect.content.contains("Wooden arrow"));
        assert!(inspect.content.contains("x8"));

        let remove = dispatch(
            &mut state,
            "remove_from_inventory",
            &json!({"item": "Arrow", "quantity": 2}),
        );
        assert!(!remove.is_error);
        assert_eq!(state.find_item("Arrow").unwrap().quantity, 6);

        let missing = dispatch(&mut state, "inspect_item", &json!({"item_name": "Sword"}));
        assert!(missing.is_error);
    }

    #[test]
    fn add_to_inventory_rejects_blank() {
        let mut state = fresh();
        let result = dispatch(
            &mut state,
            "add_to_inventory",
            &json!({"item": "  ", "description": "nothing"}),
        );
        assert!(result.is_error);
        assert!(state.inventory.is_empty());
    }

    #[test]
    fn room_get_and_move() {
        let mut state = fresh();
        let here = dispatch(&mut state, "get_current_room", &json!({}));
        assert!(here.content.contains("Starting Chamber"));

        let moved = dispatch(&mut state, "move_to_room", &json!({"room_name": "Crypt"}));
        assert!(!moved.is_error);
        assert_eq!(state.current_room, "Crypt");
        assert!(moved.content.contains("first visit"));
        assert!(state.visited("Crypt"));

        let blank = dispatch(&mut state, "move_to_room", &json!({"room_name": "   "}));
        assert!(blank.is_error);
        assert_eq!(state.current_room, "Crypt");
    }

    #[test]
    fn companions_lifecycle_tools() {
        let mut state = fresh();

        let empty = dispatch(&mut state, "get_companions", &json!({}));
        assert!(empty.content.contains("No companions"));

        let add = dispatch(
            &mut state,
            "add_companion",
            &json!({"name": "Pip", "description": "fairy companion"}),
        );
        assert!(!add.is_error);
        assert!(state.find_companion("Pip").is_some());

        let list = dispatch(&mut state, "get_companions", &json!({}));
        assert!(list.content.contains("Pip"));

        let update = dispatch(
            &mut state,
            "update_companion",
            &json!({
                "name": "Pip",
                "new_name": "Pipsqueak",
                "description": "brave fairy"
            }),
        );
        assert!(!update.is_error);
        assert!(state.find_companion("Pipsqueak").is_some());
        assert!(state.find_companion("Pip").is_none());

        let remove = dispatch(
            &mut state,
            "remove_companion",
            &json!({"name": "Pipsqueak"}),
        );
        assert!(!remove.is_error);
        assert!(state.companions.is_empty());

        let missing = dispatch(&mut state, "remove_companion", &json!({"name": "Nobody"}));
        assert!(missing.is_error);
    }

    #[test]
    fn update_companion_collision() {
        let mut state = fresh();
        dispatch(
            &mut state,
            "add_companion",
            &json!({"name": "Bob", "description": "fighter"}),
        );
        dispatch(
            &mut state,
            "add_companion",
            &json!({"name": "Pip", "description": "fairy"}),
        );
        let result = dispatch(
            &mut state,
            "update_companion",
            &json!({"name": "Bob", "new_name": "Pip"}),
        );
        assert!(result.is_error);
        assert!(result.content.contains("already exists"));
    }

    #[test]
    fn remember_and_get_journal() {
        let mut state = fresh();

        let empty = dispatch(&mut state, "get_journal", &json!({}));
        assert!(empty.content.contains("empty"));

        let rem = dispatch(
            &mut state,
            "remember",
            &json!({"fact": "Your patron is Zarathra"}),
        );
        assert!(!rem.is_error);
        assert!(rem.content.contains("Remembered"));

        let again = dispatch(
            &mut state,
            "remember",
            &json!({"fact": "your patron is zarathra"}),
        );
        assert!(!again.is_error);
        assert!(again.content.contains("Already remembered"));
        assert_eq!(state.journal.len(), 1);

        let journal = dispatch(&mut state, "get_journal", &json!({}));
        assert!(journal.content.contains("Zarathra"));

        let blank = dispatch(&mut state, "remember", &json!({"fact": "   "}));
        assert!(blank.is_error);
    }

    #[test]
    fn unknown_tool() {
        let mut state = fresh();
        let result = dispatch(&mut state, "teleport", &json!({}));
        assert!(result.is_error);
        assert!(result.content.contains("Unknown tool"));
    }

    #[test]
    fn mutating_tools_actually_change_state() {
        for name in MUTATING_TOOLS {
            let mut state = fresh();
            match *name {
                "modify_hp" => {
                    let start = state.hp;
                    dispatch(&mut state, name, &json!({"amount": -1}));
                    assert!(state.hp < start);
                }
                "add_to_inventory" => {
                    dispatch(
                        &mut state,
                        name,
                        &json!({"item": "Gem", "description": "shiny"}),
                    );
                    assert!(state.find_item("Gem").is_some());
                }
                "remove_from_inventory" => {
                    state.add_item("Gem", "shiny", 1);
                    dispatch(&mut state, name, &json!({"item": "Gem"}));
                    assert!(state.find_item("Gem").is_none());
                }
                "move_to_room" => {
                    dispatch(&mut state, name, &json!({"room_name": "Hall"}));
                    assert_eq!(state.current_room, "Hall");
                }
                "add_companion" => {
                    dispatch(
                        &mut state,
                        name,
                        &json!({"name": "Wolf", "description": "loyal"}),
                    );
                    assert!(state.find_companion("Wolf").is_some());
                }
                "update_companion" => {
                    state.add_companion("Wolf", "loyal");
                    dispatch(
                        &mut state,
                        name,
                        &json!({"name": "Wolf", "description": "fierce"}),
                    );
                    assert_eq!(state.find_companion("Wolf").unwrap().description, "fierce");
                }
                "remove_companion" => {
                    state.add_companion("Wolf", "loyal");
                    dispatch(&mut state, name, &json!({"name": "Wolf"}));
                    assert!(state.find_companion("Wolf").is_none());
                }
                "remember" => {
                    dispatch(
                        &mut state,
                        name,
                        &json!({"fact": "The door opens with a song"}),
                    );
                    assert!(
                        state
                            .journal
                            .iter()
                            .any(|j| j.contains("door opens with a song"))
                    );
                }
                "start_combat" => {
                    dispatch(
                        &mut state,
                        name,
                        &json!({"participants": ["Hero", "Goblin"]}),
                    );
                    assert!(state.in_combat);
                    assert_eq!(state.initiative_order.len(), 2);
                }
                "advance_turn" => {
                    state.start_combat(vec!["Hero".into(), "Goblin".into()]);
                    dispatch(&mut state, name, &json!({}));
                    assert_eq!(state.initiative_index, 1);
                    assert_eq!(state.current_turn_actor(), Some("Goblin"));
                }
                "end_combat" => {
                    state.start_combat(vec!["Hero".into(), "Goblin".into()]);
                    dispatch(&mut state, name, &json!({}));
                    assert!(!state.in_combat);
                    assert!(state.initiative_order.is_empty());
                }
                other => panic!("untested mutating tool: {other}"),
            }
        }
    }
}
