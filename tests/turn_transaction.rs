//! Turn-transaction and tool-loop safety tests (wiremock, no network).
//!
//! Covers: rollback on failed hops, commit on success, marker gating for
//! failed mutating tools, malformed arguments, the per-response tool cap,
//! and bounded retries for transient HTTP failures.

use grok_adventures::dm::{ChatHistory, DmClient};
use grok_adventures::game::loop_::run_dm_turn_cancellable;
use grok_adventures::game::state::GameState;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn tool_call_response(calls: &[(&str, &str)]) -> ResponseTemplate {
    let tool_calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (name, arguments))| {
            json!({
                "id": format!("call_{index}"),
                "type": "function",
                "function": { "name": name, "arguments": arguments }
            })
        })
        .collect();
    ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": tool_calls
            }
        }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12 }
    }))
}

fn narration_response(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{
            "message": { "role": "assistant", "content": text }
        }],
        "usage": { "prompt_tokens": 20, "completion_tokens": 5, "total_tokens": 25 }
    }))
}

/// Mount one-shot responses in order. Later requests fall through to any
/// catch-all mock mounted after this sequence.
async fn mount_sequence(server: &MockServer, responses: Vec<ResponseTemplate>) {
    for response in responses {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(response)
            .up_to_n_times(1)
            .mount(server)
            .await;
    }
}

fn chat_client(server: &MockServer) -> DmClient {
    DmClient::with_model("test-key", "test-model").with_base_url(server.uri())
}

fn last_tool_message(body: &[u8]) -> Value {
    let request: Value = serde_json::from_slice(body).expect("request body is JSON");
    request["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .cloned()
        .expect("expected a tool message")
}

#[tokio::test]
async fn failed_turn_rolls_back_tool_mutations() {
    let server = MockServer::start().await;
    mount_sequence(
        &server,
        vec![tool_call_response(&[("modify_hp", r#"{"amount":-5}"#)])],
    )
    .await;
    // Everything after the tool hop fails; Retry-After: 0 keeps the three
    // retry attempts fast while still exercising the retry path.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(500)
                .insert_header("Retry-After", "0")
                .set_body_string("boom"),
        )
        .mount(&server)
        .await;

    let client = chat_client(&server);
    let mut state = GameState::new("Hero");
    let history = ChatHistory::new(2);

    let err = client
        .query("sys", "attack the goblin", &mut state, &history)
        .await
        .expect_err("500 should fail the turn");

    assert!(err.to_string().contains("500"), "err was: {err:#}");
    assert_eq!(state.hp, 20, "failed turn must not commit -5 HP");
    assert!(state.game_log.is_empty(), "failed turn must not log");
    assert!(history.is_empty(), "failed turn must not extend history");
}

#[tokio::test]
async fn successful_turn_commits_tool_mutations() {
    let server = MockServer::start().await;
    mount_sequence(
        &server,
        vec![
            tool_call_response(&[("modify_hp", r#"{"amount":-5}"#)]),
            narration_response("The blade bites deep."),
        ],
    )
    .await;

    let client = chat_client(&server);
    let mut state = GameState::new("Hero");
    let history = ChatHistory::new(0);

    let result = client
        .query("sys", "attack", &mut state, &history)
        .await
        .expect("turn should succeed");

    assert_eq!(state.hp, 15, "successful turn must commit -5 HP");
    assert!(result.mutating_tool_ran);
    assert_eq!(result.tool_calls.len(), 1);
    assert_eq!(result.tool_calls[0].name, "modify_hp");
    assert_eq!(result.text, "The blade bites deep.");
    assert_eq!(result.usage.total_tokens, 37);
    assert_eq!(
        result.usage.requests, 2,
        "one tool hop plus final narration"
    );
}

#[tokio::test]
async fn later_tool_hops_see_earlier_effects() {
    let server = MockServer::start().await;
    mount_sequence(
        &server,
        vec![
            tool_call_response(&[("modify_hp", r#"{"amount":-5}"#)]),
            tool_call_response(&[("get_character_stats", "{}")]),
            narration_response("You still stand, barely."),
        ],
    )
    .await;

    let client = chat_client(&server);
    let mut state = GameState::new("Hero");
    let history = ChatHistory::new(0);

    let result = client
        .query("sys", "attack", &mut state, &history)
        .await
        .expect("turn should succeed");

    assert_eq!(result.tool_calls.len(), 2);
    assert_eq!(
        result.usage.requests, 3,
        "two tool hops plus final narration"
    );
    assert_eq!(result.usage.total_tokens, 49);
    assert!(
        result.tool_calls[1].result.contains("HP: 15/20"),
        "second hop must see the first hop's mutation: {}",
        result.tool_calls[1].result
    );
    assert_eq!(state.hp, 15);
}

#[tokio::test]
async fn failed_mutating_tool_does_not_disable_marker_fallback() {
    let server = MockServer::start().await;
    mount_sequence(
        &server,
        vec![
            // Missing required `amount`: dispatch returns an error and the
            // mutating flag must stay false.
            tool_call_response(&[("modify_hp", "{}")]),
            narration_response("A trap springs! [DAMAGE:5]"),
        ],
    )
    .await;

    let client = chat_client(&server);
    let mut state = GameState::new("Hero");
    let mut history = ChatHistory::new(0);

    let result = run_dm_turn_cancellable(&client, &mut state, "walk", "heroic", None, &mut history)
        .await
        .expect("turn should succeed");

    assert!(
        !result.mutating_tool_ran,
        "an errored mutating tool must not disable markers"
    );
    assert_eq!(state.hp, 15, "marker fallback should apply [DAMAGE:5]");
    assert!(!result.text.contains("[DAMAGE:"));
    assert!(result.text.contains("💔"), "text: {}", result.text);
}

#[tokio::test]
async fn successful_mutating_tool_suppresses_marker_fallback() {
    let server = MockServer::start().await;
    mount_sequence(
        &server,
        vec![
            tool_call_response(&[("modify_hp", r#"{"amount":-5}"#)]),
            narration_response("A trap springs! [DAMAGE:5]"),
        ],
    )
    .await;

    let client = chat_client(&server);
    let mut state = GameState::new("Hero");
    let mut history = ChatHistory::new(0);

    let result = run_dm_turn_cancellable(&client, &mut state, "walk", "heroic", None, &mut history)
        .await
        .expect("turn should succeed");

    assert!(result.mutating_tool_ran);
    assert_eq!(state.hp, 15, "marker must not double-apply damage");
    assert!(
        result.text.contains("[DAMAGE:5]"),
        "markers stay raw when a mutating tool ran: {}",
        result.text
    );
}

#[tokio::test]
async fn malformed_tool_arguments_do_not_execute() {
    let server = MockServer::start().await;
    mount_sequence(
        &server,
        vec![
            tool_call_response(&[("advance_turn", "{not valid json")]),
            narration_response("The goblin waits."),
        ],
    )
    .await;

    let client = chat_client(&server);
    let mut state = GameState::new("Hero");
    state.start_combat(vec!["Hero".into(), "Goblin".into()]);
    let history = ChatHistory::new(0);

    let result = client
        .query("sys", "wait", &mut state, &history)
        .await
        .expect("turn should succeed");

    assert_eq!(state.initiative_index, 0, "malformed call must not advance");
    assert_eq!(state.current_turn_actor(), Some("Hero"));
    assert!(!result.mutating_tool_ran);
    assert_eq!(result.tool_calls.len(), 1);
    assert!(result.tool_calls[0].result.contains("Invalid arguments"));
    assert!(result.tool_calls[0].result.contains("advance_turn"));

    // The model must receive the parse failure as a tool error message.
    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(requests.len(), 2);
    let tool_message = last_tool_message(&requests[1].body);
    let content = tool_message["content"].as_str().unwrap();
    assert!(content.contains("Invalid arguments"), "content: {content}");
    assert!(content.contains("advance_turn"), "content: {content}");
}

#[tokio::test]
async fn tool_calls_are_capped_per_response() {
    let server = MockServer::start().await;
    let calls: Vec<(&str, &str)> = (0..33).map(|_| ("get_character_stats", "{}")).collect();
    mount_sequence(
        &server,
        vec![tool_call_response(&calls), narration_response("Done.")],
    )
    .await;

    let client = chat_client(&server);
    let mut state = GameState::new("Hero");
    let history = ChatHistory::new(0);

    let result = client
        .query("sys", "look", &mut state, &history)
        .await
        .expect("turn should succeed");

    assert_eq!(result.tool_calls.len(), 33);
    assert!(
        result.tool_calls[..32]
            .iter()
            .all(|call| !call.result.contains("Too many tool calls")),
        "first 32 calls should execute"
    );
    assert!(
        result.tool_calls[32].result.contains("Too many tool calls"),
        "33rd call should be rejected: {}",
        result.tool_calls[32].result
    );

    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(requests.len(), 2);
    let tool_message = last_tool_message(&requests[1].body);
    assert!(
        tool_message["content"]
            .as_str()
            .unwrap()
            .contains("Too many tool calls")
    );
}

#[tokio::test]
async fn retries_transient_500_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("transient"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(narration_response("Recovered."))
        .mount(&server)
        .await;

    let client = chat_client(&server);
    let mut state = GameState::new("Hero");
    let history = ChatHistory::new(0);

    let result = client
        .query("sys", "look", &mut state, &history)
        .await
        .expect("retry should recover");

    assert_eq!(result.text, "Recovered.");
    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(requests.len(), 2, "one retry after the 500");
}

#[tokio::test]
async fn honors_retry_after_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "0")
                .set_body_string("slow down"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(narration_response("Okay."))
        .mount(&server)
        .await;

    let client = chat_client(&server);
    let mut state = GameState::new("Hero");
    let history = ChatHistory::new(0);

    let result = client
        .query("sys", "look", &mut state, &history)
        .await
        .expect("429 with Retry-After should be retried");

    assert_eq!(result.text, "Okay.");
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("requests recorded")
            .len(),
        2
    );
}

#[tokio::test]
async fn does_not_retry_auth_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
        .mount(&server)
        .await;

    let client = chat_client(&server);
    let mut state = GameState::new("Hero");
    let history = ChatHistory::new(0);

    let err = client
        .query("sys", "look", &mut state, &history)
        .await
        .expect_err("401 should fail");

    assert!(err.to_string().contains("401"), "err was: {err:#}");
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("requests recorded")
            .len(),
        1,
        "auth errors must not be retried"
    );
}
