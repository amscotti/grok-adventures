//! Dungeon Master client (xAI chat + tools).

pub mod client;

pub use client::{
    ChatHistory, DmClient, TURN_CANCELLED_MSG, ToolCallRecord, TurnResult, UsageTotals,
    is_turn_cancelled,
};
