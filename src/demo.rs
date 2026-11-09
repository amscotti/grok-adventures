//! Deterministic offline demo: the UI without an API key or a network call.
//!
//! The transcript is static by design. It exists so a fresh checkout can show
//! the player/DM/dice/tools/state blocks and the status line, not to simulate
//! a playable session.

use crate::game::delta::{StateSnapshot, describe_deltas};
use crate::game::state::GameState;
use crate::ui::blocks::{self, Block, ToolCallView};
use crate::ui::status;
use crate::ui::theme;

/// Header label that makes the showcase unmistakably not a live game.
pub const DEMO_LABEL: &str = "Demo (no API calls)";
/// Closing pointer to the real, keyed game.
pub const DEMO_OUTRO: &str = "Run `cargo run` with XAI_API_KEY for a live game.";

const DEMO_HERO: &str = "Demo Hero";
const DEMO_ROOM: &str = "Hidden Vault";

/// Build the final demo state plus the transcript blocks that describe it.
///
/// State is advanced through the same methods a real turn uses so the State
/// block comes from [`describe_deltas`] and stays consistent with the status
/// line.
fn demo_scene() -> (GameState, Vec<Block>) {
    let mut state = GameState::new(DEMO_HERO);
    let before = StateSnapshot::from(&state);

    state.modify_hp(-2);
    state.move_to(DEMO_ROOM);
    state.add_item("Iron Key", "Tarnished but warm", 1);
    let deltas = describe_deltas(&before, &state);

    let blocks = vec![
        Block::Player {
            text: "I search the altar for a hidden switch.".to_string(),
            spoken: false,
        },
        Block::Dm {
            text: "Your fingers find a seam in the cold stone. It clicks, and a small \
                  **iron key** tumbles into your palm. A dart hisses from the wall — \
                  you twist away, but not quite fast enough."
                .to_string(),
        },
        Block::Dice {
            text: "🎲 d20: 14 + DEX 2 = 16 vs DC 12 — success".to_string(),
        },
        Block::Tools {
            calls: vec![
                ToolCallView {
                    name: "roll_dice".to_string(),
                    result_summary: "16 vs DC 12 (success)".to_string(),
                },
                ToolCallView {
                    name: "add_to_inventory".to_string(),
                    result_summary: "Added Iron Key".to_string(),
                },
            ],
        },
        Block::State {
            text: deltas.join("\n"),
        },
    ];

    (state, blocks)
}

/// Deterministic final state for the demo beat (useful for the status line).
pub fn demo_state() -> GameState {
    demo_scene().0
}

/// Deterministic demo transcript blocks, in display order.
pub fn demo_blocks() -> Vec<Block> {
    demo_scene().1
}

/// Render the complete demo as a string: header, blocks, status line, outro.
///
/// Pure content aside from `NO_COLOR`-aware theme styling; it never reads the
/// API key, loads config, or touches the network.
pub fn render_demo() -> String {
    let (state, blocks) = demo_scene();
    let mut out = String::new();
    out.push('\n');

    let mut header = blocks::session_header_lines(
        "Grok Adventures",
        DEMO_LABEL,
        "static showcase — no model, no input, no saving",
    )
    .into_iter();
    if let Some(title) = header.next() {
        out.push_str(&theme::player(&title));
        out.push('\n');
    }
    for line in header {
        out.push_str(&theme::dim(&line));
        out.push('\n');
    }
    out.push('\n');

    // Collapsed tools are intentional: `--demo` never sets debug expansion.
    for block in &blocks {
        out.push_str(&blocks::format_block(block, false));
        out.push_str("\n\n");
    }

    out.push_str(&status::format_status(&state));
    out.push_str("\n\n");
    out.push_str(&theme::dim(DEMO_OUTRO));
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::sanitize;

    fn assert_no_unsafe_controls(text: &str) {
        assert!(
            !text.contains('\x1b'),
            "raw escape in demo content: {text:?}"
        );
        assert!(!text.contains('\x07'), "BEL in demo content: {text:?}");
        assert!(
            !text.chars().any(sanitize::is_disallowed_terminal_char),
            "control/bidi/zero-width char in demo content: {text:?}"
        );
    }

    #[test]
    fn demo_has_required_blocks_in_order() {
        let blocks = demo_blocks();
        assert!(matches!(blocks[0], Block::Player { .. }));
        assert!(matches!(blocks[1], Block::Dm { .. }));
        assert!(matches!(blocks[2], Block::Dice { .. }));
        assert!(matches!(blocks[3], Block::Tools { .. }));
        assert!(matches!(blocks[4], Block::State { .. }));
        assert_eq!(blocks.len(), 5, "keep the showcase short");
    }

    #[test]
    fn demo_tools_are_collapsed_and_state_is_aggregated() {
        let blocks = demo_blocks();
        let Block::Tools { calls } = &blocks[3] else {
            panic!("expected Tools block");
        };
        assert_eq!(calls.len(), 2);
        let Block::State { text } = &blocks[4] else {
            panic!("expected State block");
        };
        // One State block aggregates HP, room, and inventory deltas.
        assert!(text.contains("HP"));
        assert!(text.contains("Hidden Vault"));
        assert!(text.contains("Iron Key"));
        assert!(text.lines().count() >= 3);
    }

    #[test]
    fn rendered_demo_has_label_status_and_outro() {
        let rendered = render_demo();
        assert!(rendered.contains(DEMO_LABEL));
        assert!(rendered.contains("HP 18/20"));
        assert!(rendered.contains(DEMO_ROOM));
        assert!(rendered.contains("1 item"));
        assert!(rendered.contains(DEMO_OUTRO));
        assert!(rendered.contains("Tools (2)"));
        assert!(rendered.contains("State"));
    }

    #[test]
    fn demo_content_is_deterministic() {
        assert_eq!(demo_blocks(), demo_blocks());
        assert_eq!(render_demo(), render_demo());
        assert_eq!(demo_state().hp, 18);
        assert_eq!(demo_state().current_room, DEMO_ROOM);
    }

    #[test]
    fn demo_content_carries_no_escape_payloads() {
        for block in demo_blocks() {
            match block {
                Block::Player { text, .. }
                | Block::Dm { text }
                | Block::Dice { text }
                | Block::State { text } => assert_no_unsafe_controls(&text),
                Block::Tools { calls } => {
                    for call in calls {
                        assert_no_unsafe_controls(&call.name);
                        assert_no_unsafe_controls(&call.result_summary);
                    }
                }
                Block::System { text, .. } => assert_no_unsafe_controls(&text),
            }
        }

        // Styling may add SGR, but never OSC/BEL/bidi/zero-width payloads.
        let rendered = render_demo();
        assert!(!rendered.contains("\x1b]"), "OSC survived: {rendered:?}");
        assert!(!rendered.contains('\x07'), "BEL survived: {rendered:?}");
        assert!(!rendered.contains('\u{202e}'));
        assert!(!rendered.contains('\u{2067}'));
        assert!(!rendered.contains('\u{200b}'));
    }
}
