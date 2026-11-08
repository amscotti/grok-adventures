//! Slash commands and natural-language state queries.
//!
//! Pure command parsing, local (bill-free) queries, and the presentation
//! blocks they return. No DM calls, audio, or fullscreen dependencies.

use std::io::{self, Write};
use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;

use crate::game::save::SaveGame;
use crate::game::state::GameState;
use crate::ui::blocks::{self, Block, SystemKind};

/// Result of handling a meta / slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaResult {
    Quit,
    Handled,
    /// Start push-to-talk recording (optional override seconds).
    Talk {
        seconds: Option<u32>,
    },
    NotMeta,
}

/// Mutable per-session UI / voice flags.
#[derive(Debug, Clone)]
pub struct SessionFlags {
    pub tools_expanded: bool,
    pub enable_tts: bool,
    pub enable_stt: bool,
    pub stt_edit_before_send: bool,
}

/// One slash command in the catalog: canonical name, aliases, argument
/// hint, and the one-line description shown by autocomplete and `/help`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    /// Argument placeholder (`"[slot]"`), empty when the command takes none.
    pub args: &'static str,
    pub description: &'static str,
}

impl SlashCommand {
    /// `/name` plus the argument hint when present (`/save [slot]`).
    pub fn usage(&self) -> String {
        if self.args.is_empty() {
            format!("/{}", self.name)
        } else {
            format!("/{} {}", self.name, self.args)
        }
    }

    /// Whether `word` (already downcased) names this command.
    pub fn matches(&self, word: &str) -> bool {
        self.name == word || self.aliases.contains(&word)
    }
}

/// Every slash command, in menu order. Single source of truth for the
/// known-command check in [`handle_meta_command`] and for autocomplete;
/// the dispatch match below handles each name/alias.
pub const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "help",
        aliases: &["h", "?"],
        args: "",
        description: "Show help",
    },
    SlashCommand {
        name: "stats",
        aliases: &["status"],
        args: "",
        description: "View character stats",
    },
    SlashCommand {
        name: "inventory",
        aliases: &["inv", "i"],
        args: "",
        description: "View inventory",
    },
    SlashCommand {
        name: "companions",
        aliases: &["party", "companion"],
        args: "",
        description: "View traveling companions",
    },
    SlashCommand {
        name: "journal",
        aliases: &["notes", "lore"],
        args: "",
        description: "View remembered facts",
    },
    SlashCommand {
        name: "rest",
        aliases: &[],
        args: "",
        description: "Short rest (heal ~1/4 max HP)",
    },
    SlashCommand {
        name: "longrest",
        aliases: &["long"],
        args: "",
        description: "Long rest (HP to full)",
    },
    SlashCommand {
        name: "resurrect",
        aliases: &[],
        args: "",
        description: "If dead, restore 1 HP",
    },
    SlashCommand {
        name: "save",
        aliases: &[],
        args: "[slot]",
        description: "Save the game",
    },
    SlashCommand {
        name: "load",
        aliases: &[],
        args: "[slot]",
        description: "Load a saved game",
    },
    SlashCommand {
        name: "talk",
        aliases: &[],
        args: "[seconds]",
        description: "Speak an action (mic)",
    },
    SlashCommand {
        name: "tools",
        aliases: &[],
        args: "",
        description: "Expand/collapse tool blocks",
    },
    SlashCommand {
        name: "theme",
        aliases: &[],
        args: "",
        description: "Cycle theme (fullscreen)",
    },
    SlashCommand {
        name: "tts",
        aliases: &[],
        args: "",
        description: "Toggle DM speech",
    },
    SlashCommand {
        name: "voice",
        aliases: &["stt"],
        args: "",
        description: "Toggle push-to-talk",
    },
    SlashCommand {
        name: "copy",
        aliases: &[],
        args: "",
        description: "Copy last DM narration",
    },
    SlashCommand {
        name: "again",
        aliases: &[],
        args: "",
        description: "Repeat your last action",
    },
    SlashCommand {
        name: "clear",
        aliases: &["cls"],
        args: "",
        description: "Clear the screen",
    },
    SlashCommand {
        name: "debug",
        aliases: &[],
        args: "",
        description: "Show complete game state",
    },
    SlashCommand {
        name: "quit",
        aliases: &["exit", "q"],
        args: "",
        description: "End the game",
    },
];

/// Look up a command by name or alias (`word` is trimmed + downcased here).
pub fn slash_command(word: &str) -> Option<&'static SlashCommand> {
    let word = word.trim().to_ascii_lowercase();
    SLASH_COMMANDS.iter().find(|c| c.matches(&word))
}

static INVENTORY_QUERY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(what('?s| is| do i have)?( in)?( my)?|check( my)?|show( me)?( my)?|list( my)?)\s*(inventory|items|possessions|belongings|pack|bag|stuff|equipment)",
    )
    .unwrap()
});

static LOCATION_QUERY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(where am i|what room (am i in|is this)|which room (am i in|is this)|current location|where is this place|what is this place)",
    )
    .unwrap()
});

static HEALTH_QUERY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(how('?s| is)?( my)?|check( my)?|what('?s| is)?( my)?)\s*(health|hp|hit points|condition|wounds)",
    )
    .unwrap()
});

/// Split input into a downcased command word and optional remainder.
pub fn parse_command(input: &str) -> (String, Option<String>) {
    let stripped = input.trim();
    // Strip leading slash for slash commands
    let stripped = stripped.strip_prefix('/').unwrap_or(stripped);
    if let Some((cmd, rest)) = stripped.split_once(char::is_whitespace) {
        let rest = rest.trim();
        if rest.is_empty() {
            (cmd.to_ascii_lowercase(), None)
        } else {
            (cmd.to_ascii_lowercase(), Some(rest.to_string()))
        }
    } else {
        (stripped.to_ascii_lowercase(), None)
    }
}

/// Handle slash/keyword meta commands.
///
/// Returns `(result, blocks)` — callers print or push blocks to the TUI.
/// Side effects that only make sense in minimal mode (ANSI clear screen) still run here.
pub fn handle_meta_command(
    state: &mut GameState,
    input: &str,
    flags: &mut SessionFlags,
) -> Result<(MetaResult, Vec<Block>)> {
    let (cmd, arg) = parse_command(input);

    // Only treat as meta if it's a known bare word or started with /
    let is_slash = input.trim_start().starts_with('/');
    let known = slash_command(&cmd).is_some();

    if !is_slash && !known {
        return Ok((MetaResult::NotMeta, vec![]));
    }
    // Bare words that are multi-word natural language should not match if
    // the original had more than the command — except save/load/talk/long with args.
    if !is_slash {
        let words: Vec<&str> = input.split_whitespace().collect();
        let arg_cmds = ["save", "load", "tools", "talk", "long"];
        if words.len() > 1 && !arg_cmds.contains(&cmd.as_str()) {
            return Ok((MetaResult::NotMeta, vec![]));
        }
    }

    // Normalize "long rest" / "/longrest"
    let is_long_rest = cmd == "longrest"
        || (cmd == "long"
            && arg
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case("rest")));

    match cmd.as_str() {
        "quit" | "exit" | "q" => Ok((
            MetaResult::Quit,
            vec![Block::System {
                text: format!("Farewell, {}!", state.character_name),
                kind: SystemKind::Info,
            }],
        )),
        "help" | "h" | "?" => Ok((MetaResult::Handled, vec![help_block()])),
        "stats" | "status" => Ok((MetaResult::Handled, vec![stats_block(state)])),
        "inventory" | "inv" | "i" => Ok((MetaResult::Handled, vec![inventory_block(state)])),
        "companions" | "party" | "companion" => {
            Ok((MetaResult::Handled, vec![companions_block(state)]))
        }
        "journal" | "notes" | "lore" => Ok((MetaResult::Handled, vec![journal_block(state)])),
        "rest" => {
            let text = state.short_rest();
            Ok((MetaResult::Handled, vec![Block::State { text }]))
        }
        "longrest" => {
            let text = state.long_rest();
            Ok((MetaResult::Handled, vec![Block::State { text }]))
        }
        "long" if is_long_rest => {
            let text = state.long_rest();
            Ok((MetaResult::Handled, vec![Block::State { text }]))
        }
        "long" => {
            // Bare "long" without "rest": slash form gets a hint; bare word is not meta.
            if is_slash {
                Ok((
                    MetaResult::Handled,
                    vec![Block::System {
                        text: "Did you mean /longrest (or long rest)?".into(),
                        kind: SystemKind::Info,
                    }],
                ))
            } else {
                Ok((MetaResult::NotMeta, vec![]))
            }
        }
        "resurrect" => {
            let text = state.resurrect();
            Ok((MetaResult::Handled, vec![Block::State { text }]))
        }
        "save" => {
            let path = SaveGame::resolve_path(arg.as_deref());
            let block = match SaveGame::save(state, &path) {
                Ok(()) => Block::System {
                    text: format!("Saved game to {path}"),
                    kind: SystemKind::Success,
                },
                Err(e) => Block::System {
                    text: format!("Save failed: {e:#}"),
                    kind: SystemKind::Error,
                },
            };
            Ok((MetaResult::Handled, vec![block]))
        }
        "load" => {
            let path = SaveGame::resolve_path(arg.as_deref());
            let blocks = match SaveGame::load(&path) {
                Ok(loaded) => {
                    state.replace_with(loaded);
                    vec![Block::System {
                        text: format!("Loaded game from {path}"),
                        kind: SystemKind::Success,
                    }]
                }
                Err(e) => vec![Block::System {
                    text: format!("Load failed: {e:#}"),
                    kind: SystemKind::Error,
                }],
            };
            Ok((MetaResult::Handled, blocks))
        }
        "clear" | "cls" => {
            // Minimal mode: clear terminal scrollback. Fullscreen ignores this
            // (caller may also clear its own scrollback).
            print!("\x1b[2J\x1b[H");
            let _ = io::stdout().flush();
            Ok((MetaResult::Handled, vec![]))
        }
        "debug" => Ok((MetaResult::Handled, vec![debug_state_block(state)])),
        "tools" => {
            flags.tools_expanded = !flags.tools_expanded;
            let msg = if flags.tools_expanded {
                "Tool blocks expanded"
            } else {
                "Tool blocks collapsed"
            };
            Ok((
                MetaResult::Handled,
                vec![Block::System {
                    text: msg.into(),
                    kind: SystemKind::Info,
                }],
            ))
        }
        "theme" => {
            // Fullscreen handles cycling; minimal acknowledges only.
            Ok((
                MetaResult::Handled,
                vec![Block::System {
                    text: "Theme cycling is available in fullscreen mode (/theme).".into(),
                    kind: SystemKind::Info,
                }],
            ))
        }
        "tts" => {
            flags.enable_tts = !flags.enable_tts;
            let msg = if flags.enable_tts {
                "TTS on — DM narration will be spoken"
            } else {
                "TTS off — silent narration"
            };
            Ok((
                MetaResult::Handled,
                vec![Block::System {
                    text: msg.into(),
                    kind: SystemKind::Info,
                }],
            ))
        }
        "voice" | "stt" => {
            flags.enable_stt = !flags.enable_stt;
            let msg = if flags.enable_stt {
                "STT on — `talk` (Enter to stop) or hold Ctrl+Space (F2 also works) in fullscreen"
            } else {
                "STT off — typing only"
            };
            Ok((
                MetaResult::Handled,
                vec![Block::System {
                    text: msg.into(),
                    kind: SystemKind::Info,
                }],
            ))
        }
        "talk" => {
            let seconds = arg.as_ref().and_then(|a| a.parse::<u32>().ok());
            Ok((MetaResult::Talk { seconds }, vec![]))
        }
        // /copy and /again are handled by the interactive loops (need last DM /
        // history). Acknowledge if they slip through handle_meta_command alone.
        "copy" => Ok((
            MetaResult::Handled,
            vec![Block::System {
                text: "Use /copy after a DM turn to copy the last narration (OSC 52).".into(),
                kind: SystemKind::Info,
            }],
        )),
        "again" => Ok((
            MetaResult::Handled,
            vec![Block::System {
                text: "Use /again to re-run your last action.".into(),
                kind: SystemKind::Info,
            }],
        )),
        _ => Ok((MetaResult::NotMeta, vec![])),
    }
}

/// Intercept natural-language state queries. Returns true if handled (and prints).
pub fn handle_state_query(state: &GameState, input: &str, tools_expanded: bool) -> bool {
    if let Some(blocks) = handle_state_query_blocks(state, input) {
        for b in &blocks {
            blocks::print_block(b, tools_expanded);
        }
        return true;
    }
    false
}

/// Natural-language state queries as blocks (no printing).
pub fn handle_state_query_blocks(state: &GameState, input: &str) -> Option<Vec<Block>> {
    let down = input.trim().to_ascii_lowercase();
    if INVENTORY_QUERY.is_match(&down) {
        return Some(vec![inventory_narrative_block(state)]);
    }
    if LOCATION_QUERY.is_match(&down) {
        return Some(vec![location_narrative_block(state)]);
    }
    if HEALTH_QUERY.is_match(&down) {
        return Some(vec![health_narrative_block(state)]);
    }
    None
}

/// Map HP to a descriptive condition label (pure, testable).
pub fn health_condition(hp: i32, max_hp: i32) -> &'static str {
    if max_hp <= 0 {
        return "dead";
    }
    let pct = (hp as f64 / max_hp as f64 * 100.0) as i32;
    match pct {
        100 => "in perfect health",
        75..=99 => "slightly wounded",
        50..=74 => "wounded",
        25..=49 => "badly wounded",
        1..=24 => "near death",
        _ => "dead",
    }
}

/// Detect if input is a local inventory/location/health query (for tests).
pub fn is_local_query(input: &str) -> bool {
    let down = input.trim().to_ascii_lowercase();
    INVENTORY_QUERY.is_match(&down)
        || LOCATION_QUERY.is_match(&down)
        || HEALTH_QUERY.is_match(&down)
}

fn stats_block(state: &GameState) -> Block {
    let class = state
        .class
        .as_ref()
        .map(|c| format!("\nClass: {c}"))
        .unwrap_or_default();
    let combat = if state.in_combat {
        let actor = state.current_turn_actor().unwrap_or("?");
        format!(
            "\nCombat: ACTIVE\nInitiative: {}\nCurrent turn: {actor}",
            state.initiative_order.join(", ")
        )
    } else {
        String::new()
    };
    let text = format!(
        "Name: {}{class}\nHP: {}/{}\nLocation: {}\nStats: {}\nCompanions: {}{combat}",
        state.character_name,
        state.hp,
        state.max_hp,
        state.current_room,
        state.stats_string(),
        if state.companions.is_empty() {
            "(none)".to_string()
        } else {
            state
                .companions
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    Block::State { text }
}

fn inventory_block(state: &GameState) -> Block {
    let text = if state.inventory.is_empty() {
        "(empty)".to_string()
    } else {
        state
            .inventory
            .iter()
            .map(|i| format!("• {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Block::State { text }
}

fn companions_block(state: &GameState) -> Block {
    let text = if state.companions.is_empty() {
        "(none)".to_string()
    } else {
        state
            .companions
            .iter()
            .map(|c| format!("• {c}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Block::State { text }
}

fn journal_block(state: &GameState) -> Block {
    let text = if state.journal.is_empty() {
        "(empty)".to_string()
    } else {
        state
            .journal
            .iter()
            .map(|f| format!("• {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Block::State { text }
}

fn inventory_narrative_block(state: &GameState) -> Block {
    let mut text = format!("{} checks their belongings…\n", state.character_name);
    if state.inventory.is_empty() {
        text.push_str("Your pack is empty. You carry nothing but the clothes on your back.");
    } else {
        text.push_str("You are carrying:\n");
        for item in &state.inventory {
            if item.quantity > 1 {
                text.push_str(&format!(
                    "  • **{}** (x{}) - {}\n",
                    item.name, item.quantity, item.description
                ));
            } else {
                text.push_str(&format!("  • **{}** - {}\n", item.name, item.description));
            }
        }
    }
    text.push_str("\nWhat do you do next?");
    Block::State { text }
}

fn location_narrative_block(state: &GameState) -> Block {
    let mut text = String::from("You look around, getting your bearings…\n");
    text.push_str(&format!("You are in: **{}**\n\n", state.current_room));
    text.push_str("Places you've visited:\n");
    for room in &state.visited_rooms {
        text.push_str(&format!("  • {room}\n"));
    }
    text.push_str("\nWhat do you do next?");
    Block::State { text }
}

fn health_narrative_block(state: &GameState) -> Block {
    let condition = health_condition(state.hp, state.max_hp);
    let text = format!(
        "{} takes stock of their condition…\nHP: **{}/{}** - You are {}.\n\nWhat do you do next?",
        state.character_name, state.hp, state.max_hp, condition
    );
    Block::State { text }
}

fn help_block() -> Block {
    let text = r#"Available Commands
Movement: go north, enter the cave, walk to the forest
Actions: search the room, attack goblin, open chest
Inventory: check inventory, use potion, drop sword
Character: check stats, check health

System Commands
/stats or stats — View character stats
/inv or inventory — View inventory (actual game state)
/companions or party — View traveling companions
/journal or notes — View remembered facts
/rest — Short rest (heal ~1/4 max HP)
/longrest or long rest — Long rest (HP to full)
/resurrect — If dead, restore 1 HP so you can continue testing
/save [slot|path] — Save the game (default: autosave)
/load [slot|path] — Load a saved game
/tools — Expand/collapse tool blocks
/theme — Cycle theme (fullscreen)
/tts — Toggle DM speech (TTS)
/voice — Toggle push-to-talk (STT)
/talk [max seconds] — Record mic until Enter (or max); transcribe an action
/copy — Copy last DM narration to clipboard (OSC 52; also cache/last-copy.txt)
/again — Repeat your last action
/debug — Show complete game state
/quit or exit — End the game
/help — Show this help

Tip: Questions like "what's in my inventory?", "where am I?", "how's my health?"
     return accurate game state directly (not through the AI).
Tip: GROK_ADVENTURES_DEBUG=1 expands tools and logs marker/usage details.
Tip: Type always works — voice is optional and not required.
Tip: --fullscreen enables sticky status + scrollback TUI; hold Ctrl+Space (or F2) to talk, release to send.
Tip: ↑/↓ cycles prompt history; ←/→ edit in place. Type / for command autocomplete (Tab completes).
Tip: After talk (minimal), edit the transcript or press Enter to send as-is.
Tip: On death the loop stays open for /resurrect (testing) or /quit.
Tip: Autopilot personality: --player-style cautious|balanced|reckless (or GROK_ADVENTURES_PLAYER_STYLE).
Tip: DM model defaults to grok-4.7 ([xai].model or XAI_MODEL). Reasoning effort defaults to low.
Tip: Speech-to-text uses grok-voice-transcribe-2.0. Words appear as you speak; batch upload is the fallback.
Tip: Hold Ctrl+Space (or F2) in fullscreen. Release leaves the line in the prompt when edit-before-send is on."#;
    Block::State {
        text: text.to_string(),
    }
}

fn debug_state_block(state: &GameState) -> Block {
    let mut text = String::new();
    text.push_str(&format!("Character: {}\n", state.character_name));
    if let Some(ref class) = state.class {
        text.push_str(&format!("Class: {class}\n"));
    }
    text.push_str(&format!("HP: {}/{}\n", state.hp, state.max_hp));
    text.push_str(&format!("Location: {}\n", state.current_room));
    text.push_str(&format!("Stats: {}\n", state.stats_string()));
    if state.in_combat {
        text.push_str(&format!(
            "Combat: ACTIVE | order: {:?} | index: {}\n\n",
            state.initiative_order, state.initiative_index
        ));
    } else {
        text.push_str("Combat: none\n\n");
    }
    text.push_str(&format!("Inventory ({} items):\n", state.inventory.len()));
    if state.inventory.is_empty() {
        text.push_str("  (empty)\n");
    } else {
        for item in &state.inventory {
            text.push_str(&format!(
                "  - {}: {} (qty: {})\n",
                item.name, item.description, item.quantity
            ));
        }
    }
    text.push_str(&format!("\nCompanions ({}):\n", state.companions.len()));
    if state.companions.is_empty() {
        text.push_str("  (none)\n");
    } else {
        for c in &state.companions {
            text.push_str(&format!("  - {}: {}\n", c.name, c.description));
        }
    }
    text.push_str(&format!("\nJournal ({}):\n", state.journal.len()));
    if state.journal.is_empty() {
        text.push_str("  (empty)\n");
    } else {
        for f in &state.journal {
            text.push_str(&format!("  - {f}\n"));
        }
    }
    text.push_str(&format!(
        "\nVisited Rooms ({}):\n",
        state.visited_rooms.len()
    ));
    for r in &state.visited_rooms {
        text.push_str(&format!("  - {r}\n"));
    }
    text.push_str("\nRecent Log:\n");
    for e in state.recent_log(5) {
        text.push_str(&format!("  {e}\n"));
    }
    Block::State { text }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::state::GameState;
    use pretty_assertions::assert_eq;

    fn test_flags() -> SessionFlags {
        SessionFlags {
            tools_expanded: false,
            enable_tts: false,
            enable_stt: false,
            stt_edit_before_send: false,
        }
    }

    fn state_block_text(blocks: &[Block]) -> String {
        blocks
            .iter()
            .filter_map(|b| match b {
                Block::State { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn system_block_text(blocks: &[Block]) -> String {
        blocks
            .iter()
            .filter_map(|b| match b {
                Block::System { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn parse_command_edge_cases() {
        assert_eq!(parse_command(""), ("".into(), None));
        assert_eq!(parse_command("   "), ("".into(), None));
        assert_eq!(parse_command("/"), ("".into(), None));
        assert_eq!(parse_command("/quit"), ("quit".into(), None));
        assert_eq!(
            parse_command("  /SAVE   Hero  "),
            ("save".into(), Some("Hero".into()))
        );
        assert_eq!(
            parse_command("talk\t10"),
            ("talk".into(), Some("10".into()))
        );
        assert_eq!(parse_command("save  "), ("save".into(), None));
        // Multi-space remainder is trimmed but inner spacing preserved.
        assert_eq!(
            parse_command("/save   my   slot  "),
            ("save".into(), Some("my   slot".into()))
        );
    }

    #[test]
    fn meta_unknown_and_bare_word_guards() {
        let mut state = GameState::new("Hero");
        let mut flags = test_flags();
        // Unknown slash command is explicitly not meta.
        let (r, _) = handle_meta_command(&mut state, "/frobnicate", &mut flags).unwrap();
        assert_eq!(r, MetaResult::NotMeta);
        // Unknown bare word is not meta.
        let (r, _) = handle_meta_command(&mut state, "frobnicate", &mut flags).unwrap();
        assert_eq!(r, MetaResult::NotMeta);
        // Known word with extra words is natural language, not meta ...
        let (r, _) = handle_meta_command(&mut state, "stats foo", &mut flags).unwrap();
        assert_eq!(r, MetaResult::NotMeta);
        let (r, _) = handle_meta_command(&mut state, "rest by the fire", &mut flags).unwrap();
        assert_eq!(r, MetaResult::NotMeta);
        // ... but slash form always counts.
        let (r, _) = handle_meta_command(&mut state, "/stats foo", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        // Arg-commands accept bare multi-word form (save into a tempdir).
        let dir = tempfile::TempDir::new().unwrap();
        let slot = dir.path().join("slot.json");
        let slot_str = slot.to_str().unwrap().to_string();
        let (r, _) =
            handle_meta_command(&mut state, &format!("save {slot_str}"), &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert!(slot.exists());
    }

    #[test]
    fn meta_quit_aliases() {
        let mut state = GameState::new("Hero");
        let mut flags = test_flags();
        for cmd in ["quit", "exit", "q", "/quit", "/EXIT", "Q"] {
            let (r, blocks) = handle_meta_command(&mut state, cmd, &mut flags).unwrap();
            assert_eq!(r, MetaResult::Quit, "cmd: {cmd}");
            assert!(system_block_text(&blocks).contains("Farewell"));
        }
    }

    #[test]
    fn meta_help_aliases() {
        let mut state = GameState::new("Hero");
        let mut flags = test_flags();
        for cmd in ["help", "h", "?", "/help"] {
            let (r, blocks) = handle_meta_command(&mut state, cmd, &mut flags).unwrap();
            assert_eq!(r, MetaResult::Handled, "cmd: {cmd}");
            assert!(state_block_text(&blocks).contains("Available Commands"));
        }
    }

    #[test]
    fn meta_stats_status_inventory_companions_journal_debug() {
        let mut state = GameState::new("Hero");
        state.add_item("Torch", "a flickering torch", 2);
        state.add_companion("Lyra", "a wary scout");
        state.add_journal_entry("The crypt bell tolls at midnight");
        let mut flags = test_flags();

        let (r, blocks) = handle_meta_command(&mut state, "/stats", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        let text = state_block_text(&blocks);
        assert!(text.contains("Hero") && text.contains("Starting Chamber"));

        let (r, blocks) = handle_meta_command(&mut state, "status", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert!(state_block_text(&blocks).contains("HP: 20/20"));

        for cmd in ["inventory", "inv", "i"] {
            let (r, blocks) = handle_meta_command(&mut state, cmd, &mut flags).unwrap();
            assert_eq!(r, MetaResult::Handled, "cmd: {cmd}");
            assert!(state_block_text(&blocks).contains("Torch"), "cmd: {cmd}");
        }
        // Empty inventory reports (empty).
        let mut bare = GameState::new("Hero");
        let (r, blocks) = handle_meta_command(&mut bare, "inv", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert!(state_block_text(&blocks).contains("(empty)"));

        for cmd in ["companions", "party", "companion"] {
            let (r, blocks) = handle_meta_command(&mut state, cmd, &mut flags).unwrap();
            assert_eq!(r, MetaResult::Handled, "cmd: {cmd}");
            assert!(state_block_text(&blocks).contains("Lyra"), "cmd: {cmd}");
        }

        for cmd in ["journal", "notes", "lore"] {
            let (r, blocks) = handle_meta_command(&mut state, cmd, &mut flags).unwrap();
            assert_eq!(r, MetaResult::Handled, "cmd: {cmd}");
            assert!(
                state_block_text(&blocks).contains("crypt bell"),
                "cmd: {cmd}"
            );
        }

        let (r, blocks) = handle_meta_command(&mut state, "/debug", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        let text = state_block_text(&blocks);
        assert!(text.contains("Character: Hero") && text.contains("Torch"));
    }

    #[test]
    fn meta_tools_toggles_both_ways() {
        let mut state = GameState::new("Hero");
        let mut flags = test_flags();
        let (r, blocks) = handle_meta_command(&mut state, "tools", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert!(flags.tools_expanded);
        assert!(system_block_text(&blocks).contains("expanded"));
        let (r, blocks) =
            handle_meta_command(&mut state, "tools extra words here", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert!(!flags.tools_expanded);
        assert!(system_block_text(&blocks).contains("collapsed"));
    }

    #[test]
    fn meta_theme_copy_again_acknowledge() {
        let mut state = GameState::new("Hero");
        let mut flags = test_flags();
        let (r, blocks) = handle_meta_command(&mut state, "/theme", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert!(system_block_text(&blocks).contains("fullscreen"));

        let (r, blocks) = handle_meta_command(&mut state, "/copy", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert!(system_block_text(&blocks).contains("/copy"));

        let (r, blocks) = handle_meta_command(&mut state, "/again", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert!(system_block_text(&blocks).contains("/again"));
    }

    #[test]
    fn meta_long_bare_word_is_not_meta_but_slash_hints() {
        let mut state = GameState::new("Hero");
        let mut flags = test_flags();
        let (r, _) = handle_meta_command(&mut state, "long", &mut flags).unwrap();
        assert_eq!(r, MetaResult::NotMeta);
        let (r, blocks) = handle_meta_command(&mut state, "/long", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert!(system_block_text(&blocks).contains("longrest"));
        // "long rest" (both forms) performs a full heal.
        state.hp = 3;
        let (r, _) = handle_meta_command(&mut state, "long rest", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert_eq!(state.hp, state.max_hp);
    }

    #[test]
    fn meta_talk_invalid_seconds_yields_none() {
        let mut state = GameState::new("Hero");
        let mut flags = test_flags();
        let (r, _) = handle_meta_command(&mut state, "/talk banana", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Talk { seconds: None });
        let (r, _) = handle_meta_command(&mut state, "talk 12 extra", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Talk { seconds: None });
    }

    #[test]
    fn meta_clear_and_stt_toggle() {
        let mut state = GameState::new("Hero");
        let mut flags = test_flags();
        for cmd in ["clear", "cls", "/clear"] {
            let (r, blocks) = handle_meta_command(&mut state, cmd, &mut flags).unwrap();
            assert_eq!(r, MetaResult::Handled, "cmd: {cmd}");
            assert!(blocks.is_empty(), "cmd: {cmd}");
        }
        let (r, _) = handle_meta_command(&mut state, "stt", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert!(flags.enable_stt);
        let (r, blocks) = handle_meta_command(&mut state, "stt", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert!(!flags.enable_stt);
        assert!(system_block_text(&blocks).contains("typing only"));
    }

    #[test]
    fn meta_load_missing_file_is_error_block() {
        let mut state = GameState::new("Hero");
        let mut flags = test_flags();
        let (r, blocks) = handle_meta_command(
            &mut state,
            "load /definitely/not/here/nope.json",
            &mut flags,
        )
        .unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert_eq!(state.character_name, "Hero");
        assert!(system_block_text(&blocks).contains("Load failed"));
    }

    #[test]
    fn meta_save_to_directory_is_error_block() {
        let mut state = GameState::new("Hero");
        let mut flags = test_flags();
        let dir = tempfile::TempDir::new().unwrap();
        let dir_str = dir.path().to_str().unwrap().to_string();
        let (r, blocks) =
            handle_meta_command(&mut state, &format!("save {dir_str}"), &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert!(system_block_text(&blocks).contains("Save failed"));
    }

    #[test]
    fn health_condition_full_boundaries() {
        assert_eq!(health_condition(20, 20), "in perfect health");
        assert_eq!(health_condition(99, 100), "slightly wounded");
        assert_eq!(health_condition(75, 100), "slightly wounded");
        assert_eq!(health_condition(74, 100), "wounded");
        assert_eq!(health_condition(50, 100), "wounded");
        assert_eq!(health_condition(49, 100), "badly wounded");
        assert_eq!(health_condition(25, 100), "badly wounded");
        assert_eq!(health_condition(24, 100), "near death");
        assert_eq!(health_condition(1, 100), "near death");
        assert_eq!(health_condition(0, 100), "dead");
        assert_eq!(health_condition(-5, 100), "dead");
        assert_eq!(health_condition(10, 0), "dead");
        assert_eq!(health_condition(10, -4), "dead");
        assert_eq!(health_condition(150, 100), "dead");
    }

    #[test]
    fn state_query_blocks_cover_all_kinds() {
        let mut state = GameState::new("Aria");
        state.add_item("Rope", "50 feet of hempen rope", 1);
        // Inventory variants.
        for q in [
            "what's in my inventory?",
            "CHECK MY BAG",
            "  show me my stuff  ",
            "list my equipment",
        ] {
            let blocks = handle_state_query_blocks(&state, q);
            assert!(blocks.is_some(), "query: {q}");
            let text = state_block_text(&blocks.unwrap());
            assert!(text.contains("Rope"), "query: {q}");
        }
        // Location variants.
        for q in ["where am i", "WHAT ROOM AM I IN?", "current location"] {
            let blocks = handle_state_query_blocks(&state, q);
            assert!(blocks.is_some(), "query: {q}");
            assert!(
                state_block_text(&blocks.unwrap()).contains("Starting Chamber"),
                "query: {q}"
            );
        }
        // Health variants reflect live HP.
        state.hp = 5;
        let blocks = handle_state_query_blocks(&state, "how's my health").unwrap();
        assert!(state_block_text(&blocks).contains("5/20"));
        // Non-queries return None.
        for q in [
            "attack the goblin",
            "where is this goblin from?",
            "search the room",
            "",
            "health",
        ] {
            assert!(
                handle_state_query_blocks(&state, q).is_none(),
                "query: {q:?}"
            );
        }
    }

    #[test]
    fn meta_stats_debug_reflect_combat_class_and_empties() {
        let mut bare = GameState::new("Hero");
        let mut flags = test_flags();
        // Empty companions/journal/debug arms.
        let (r, blocks) = handle_meta_command(&mut bare, "companions", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert!(state_block_text(&blocks).contains("(none)"));
        let (r, blocks) = handle_meta_command(&mut bare, "journal", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert!(state_block_text(&blocks).contains("(empty)"));
        let (r, blocks) = handle_meta_command(&mut bare, "/debug", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        let text = state_block_text(&blocks);
        assert!(text.contains("Combat: none"));

        // TTS toggle back on (covers the "TTS on" arm).
        let (r, blocks) = handle_meta_command(&mut bare, "tts", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        assert!(flags.enable_tts);
        assert!(system_block_text(&blocks).contains("TTS on"));

        // Class + combat + log arms.
        let mut s = GameState::new("Hero");
        s.class = Some("fighter".into());
        s.start_combat(vec!["Hero".into(), "Goblin".into()]);
        s.log("entered the crypt");
        let (r, blocks) = handle_meta_command(&mut s, "stats", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        let text = state_block_text(&blocks);
        assert!(text.contains("Class: fighter") && text.contains("Combat: ACTIVE"));
        let (r, blocks) = handle_meta_command(&mut s, "debug", &mut flags).unwrap();
        assert_eq!(r, MetaResult::Handled);
        let text = state_block_text(&blocks);
        assert!(text.contains("Class: fighter"));
        assert!(text.contains("entered the crypt"));

        // Empty-pack narrative + multi-quantity narrative arms.
        let blocks = handle_state_query_blocks(&bare, "check inventory").unwrap();
        assert!(state_block_text(&blocks).contains("pack is empty"));
        s.add_item("Torch", "a flickering torch", 2);
        let blocks = handle_state_query_blocks(&s, "check inventory").unwrap();
        assert!(state_block_text(&blocks).contains("(x2)"));
    }

    #[test]
    fn slash_catalog_names_are_unique_and_dispatchable() {
        let mut words: Vec<&str> = Vec::new();
        for cmd in SLASH_COMMANDS {
            words.push(cmd.name);
            words.extend(cmd.aliases.iter().copied());
            assert_eq!(slash_command(cmd.name).unwrap().name, cmd.name);
            for alias in cmd.aliases {
                assert_eq!(
                    slash_command(alias).unwrap().name,
                    cmd.name,
                    "alias {alias}"
                );
            }
            assert!(cmd.usage().starts_with('/'));
            assert!(!cmd.description.is_empty());
        }
        let total = words.len();
        words.sort_unstable();
        words.dedup();
        assert_eq!(words.len(), total, "duplicate name/alias in catalog");
        assert!(slash_command("frobnicate").is_none());
        assert!(slash_command("").is_none());
    }

    #[test]
    fn is_local_query_matches_blocks() {
        let state = GameState::new("Hero");
        for q in [
            "check inventory",
            "where am i",
            "check my hp",
            "WHAT'S MY CONDITION",
        ] {
            assert_eq!(
                is_local_query(q),
                handle_state_query_blocks(&state, q).is_some(),
                "query: {q}"
            );
        }
    }
}
