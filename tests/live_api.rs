//! Live xAI API smoke test. Ignored by default — requires XAI_API_KEY.
//!
//! Run with:
//!   XAI_API_KEY=... cargo test --test live_api -- --ignored --nocapture

use grok_adventures::dm::{ChatHistory, DmClient};
use grok_adventures::game::loop_::build_system_prompt;
use grok_adventures::game::state::GameState;

#[tokio::test]
#[ignore = "billed live API; set XAI_API_KEY and run with --ignored"]
async fn one_dm_turn_smoke() {
    let api_key = std::env::var("XAI_API_KEY").expect("XAI_API_KEY must be set for live test");
    assert!(!api_key.is_empty(), "XAI_API_KEY is empty");

    let mut state = GameState::new("LiveTester");
    let client = DmClient::new(api_key);
    let system = build_system_prompt(&state);
    let history = ChatHistory::new(0);

    let result = client
        .query(
            &system,
            "Begin the adventure. Describe where LiveTester wakes up. Keep it to one short paragraph.",
            &mut state,
            &history,
        )
        .await
        .expect("DM query should succeed");

    assert!(
        !result.text.trim().is_empty(),
        "expected non-empty narration"
    );
    eprintln!("DM: {}", result.text);
    eprintln!(
        "tools={} mutating={} tokens={}",
        result.tool_calls.len(),
        result.mutating_tool_ran,
        result.usage.total_tokens
    );
}
