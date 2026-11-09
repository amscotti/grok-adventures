//! xAI chat completions client with OpenAI-compatible tool calling.
//!
//! # Prompt caching
//! xAI automatically caches shared message prefixes on consecutive requests.
//! There is no per-message `cache_control` field on chat completions (unlike
//! some other providers). To improve hit rate, set the `x-grok-conv-id`
//! HTTP header to a stable session id via [`DmClient::with_conv_id`].
//! Keep the static system instructions and tool schemas identical across
//! turns; put authoritative game state in the (dynamic) system prompt body.

use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::config::{DEFAULT_DM_MODEL, DEFAULT_REASONING_EFFORT};
use crate::game::state::GameState;
use crate::game::tools;
use crate::util::http::{api_client, read_text_bounded};
use crate::util::message_content::deserialize_message_content;
use crate::util::redact::redact_secrets;

const DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";
/// One chat hop. High/xhigh reasoning can think for a while; this is the
/// ceiling for a single request, not the whole turn.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_TOOL_ITERATIONS: usize = 10;
/// Hard cap on tool calls executed from one assistant response. Extra calls
/// still receive an error `role: tool` message so the protocol stays valid.
const MAX_TOOL_CALLS_PER_RESPONSE: usize = 32;
/// Total HTTP attempts (initial try + retries) for transient failures.
const MAX_HTTP_ATTEMPTS: u32 = 3;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(250);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(5);
/// Soft cap on retained multi-turn history content (user + assistant text).
const HISTORY_MAX_CHARS: usize = 8_000;
/// Hard cap on one chat-completions response body (2 MiB).
const MAX_CHAT_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Error message returned when a turn is cancelled mid-flight.
pub const TURN_CANCELLED_MSG: &str = "turn cancelled";

/// Returns true if `err` (or any cause) is a turn-cancellation.
pub fn is_turn_cancelled(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.to_string() == TURN_CANCELLED_MSG)
}

/// One executed tool call from a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallRecord {
    pub name: String,
    pub arguments: String,
    pub result: String,
}

/// Token usage for a turn (summed across tool-loop requests).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageTotals {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    /// Chat-completion requests that returned a response (tool hops included,
    /// even if the API omitted the usage block).
    pub requests: u32,
}

impl UsageTotals {
    pub fn add(&mut self, other: &UsageTotals) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
        self.requests += other.requests;
    }
}

/// Result of one DM query (possibly multiple tool hops).
#[derive(Debug, Clone)]
pub struct TurnResult {
    pub text: String,
    pub tool_calls: Vec<ToolCallRecord>,
    pub mutating_tool_ran: bool,
    pub usage: UsageTotals,
}

/// Capped multi-turn chat history (player actions + final DM narration).
///
/// Tool-call hops stay ephemeral inside a single turn and are not retained.
/// System prompt (with fresh [`GameState::to_context_string`]) is always
/// rebuilt by the caller each turn and is not stored here.
#[derive(Debug, Clone, Default)]
pub struct ChatHistory {
    /// Max completed exchanges (user + assistant pairs). `0` disables history.
    max_turns: usize,
    /// Soft char budget across retained exchange text.
    max_chars: usize,
    exchanges: VecDeque<(String, String)>,
}

impl ChatHistory {
    /// Create history that retains up to `max_turns` prior exchanges.
    /// `0` means disabled (API messages only get the current user turn).
    pub fn new(max_turns: u32) -> Self {
        Self {
            max_turns: max_turns as usize,
            max_chars: HISTORY_MAX_CHARS,
            exchanges: VecDeque::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.max_turns > 0
    }

    pub fn len(&self) -> usize {
        self.exchanges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.exchanges.is_empty()
    }

    /// Record a completed turn (player action + final assistant text).
    /// No-op when history is disabled.
    pub fn push(&mut self, user_action: &str, assistant_text: &str) {
        if !self.enabled() {
            return;
        }
        self.exchanges
            .push_back((user_action.to_string(), assistant_text.to_string()));
        self.trim();
    }

    /// OpenAI-style messages for prior turns (user / assistant only).
    pub fn api_messages(&self) -> Vec<Value> {
        let mut out = Vec::with_capacity(self.exchanges.len() * 2);
        for (user, assistant) in &self.exchanges {
            out.push(json!({
                "role": "user",
                "content": format!("Player action: {user}"),
            }));
            out.push(json!({
                "role": "assistant",
                "content": assistant,
            }));
        }
        out
    }

    fn char_len(&self) -> usize {
        self.exchanges.iter().map(|(u, a)| u.len() + a.len()).sum()
    }

    fn trim(&mut self) {
        while self.exchanges.len() > self.max_turns {
            self.exchanges.pop_front();
        }
        while self.char_len() > self.max_chars && self.exchanges.len() > 1 {
            self.exchanges.pop_front();
        }
    }
}

/// HTTP client for Grok chat completions.
#[derive(Clone)]
pub struct DmClient {
    client: reqwest::Client,
    api_key: String,
    model: String,
    /// Sent as top-level `reasoning_effort` on chat completions.
    reasoning_effort: String,
    base_url: String,
    /// Optional conversation id for xAI prompt-cache routing (`x-grok-conv-id`).
    conv_id: Option<String>,
}

impl fmt::Debug for DmClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DmClient")
            .field("client", &self.client)
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("base_url", &self.base_url)
            .field("conv_id", &self.conv_id)
            .finish()
    }
}

impl DmClient {
    /// Create a client with the given API key.
    ///
    /// Model defaults to `XAI_MODEL` or [`DEFAULT_DM_MODEL`] (`grok-4.7`).
    /// Reasoning effort defaults to `XAI_REASONING_EFFORT` or `low`.
    pub fn new(api_key: impl Into<String>) -> Self {
        let model = env_nonempty("XAI_MODEL").unwrap_or_else(|| DEFAULT_DM_MODEL.to_string());
        let effort = env_nonempty("XAI_REASONING_EFFORT")
            .unwrap_or_else(|| DEFAULT_REASONING_EFFORT.to_string());
        Self::with_model(api_key, model).with_reasoning_effort(effort)
    }

    /// Create a client with an explicit model id and the default reasoning effort.
    pub fn with_model(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: api_client(REQUEST_TIMEOUT),
            api_key: api_key.into(),
            model: model.into(),
            reasoning_effort: DEFAULT_REASONING_EFFORT.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            conv_id: None,
        }
    }

    /// Override `reasoning_effort` (`low`, `medium`, `high`, `xhigh`).
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        let effort = effort.into();
        let effort = effort.trim();
        if !effort.is_empty() {
            self.reasoning_effort = effort.to_ascii_lowercase();
        }
        self
    }

    /// Override base URL (tests / proxies).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Set `x-grok-conv-id` for better automatic prompt-cache hit rate.
    pub fn with_conv_id(mut self, conv_id: impl Into<String>) -> Self {
        self.conv_id = Some(conv_id.into());
        self
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn reasoning_effort(&self) -> &str {
        &self.reasoning_effort
    }

    /// Run a full tool-calling loop for one player action.
    ///
    /// `system` is the full system prompt (static + dynamic). `user` is the
    /// player action text. Tool handlers run against a working copy of
    /// `state`; changes are committed only when the model produces final
    /// narration. A failed or cancelled turn leaves `state` untouched.
    /// Does not append to `history` — callers should [`ChatHistory::push`]
    /// after a successful turn.
    pub async fn query(
        &self,
        system: &str,
        user: &str,
        state: &mut GameState,
        history: &ChatHistory,
    ) -> Result<TurnResult> {
        self.query_cancellable(system, user, state, None, history)
            .await
    }

    /// Like [`query`](Self::query), but abort when `cancel` is cancelled.
    ///
    /// Checks the token before each HTTP hop and races the request against
    /// `cancel.cancelled()`. On cancel returns an error whose message is
    /// [`TURN_CANCELLED_MSG`] (see [`is_turn_cancelled`]).
    ///
    /// The turn is transactional: tool calls mutate a working copy of `state`
    /// and the copy is committed only after the model returns final narration.
    /// Errors and cancellation leave the caller's state untouched.
    pub async fn query_cancellable(
        &self,
        system: &str,
        user: &str,
        state: &mut GameState,
        cancel: Option<CancellationToken>,
        history: &ChatHistory,
    ) -> Result<TurnResult> {
        let tools = serde_json::to_value(tools::tool_definitions())?;
        let mut messages: Vec<Value> = vec![json!({"role": "system", "content": system})];
        messages.extend(history.api_messages());
        messages.push(json!({
            "role": "user",
            "content": format!("Player action: {user}"),
        }));

        let mut all_tool_calls: Vec<ToolCallRecord> = Vec::new();
        let mut mutating = false;
        let mut usage = UsageTotals::default();
        let mut final_text: Option<String> = None;

        // Transaction boundary: tools mutate `working`, never the caller's
        // state. Commit happens exactly once, after final narration arrives.
        let mut working = state.clone();
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        for iteration in 0..MAX_TOOL_ITERATIONS {
            check_cancelled(cancel.as_ref())?;

            let body = completion_body(&self.model, &self.reasoning_effort, &messages, &tools, 0.7);
            let resp = self
                .send_with_retries(&url, &body, cancel.as_ref(), iteration)
                .await?;

            check_cancelled(cancel.as_ref())?;

            let status = resp.status();
            let resp_text = read_response_body(resp, cancel.as_ref()).await?;

            if !status.is_success() {
                bail!(
                    "xAI API error {status}: {}",
                    truncate_err(&redact_secrets(&resp_text), 500)
                );
            }

            let parsed: ChatCompletionResponse =
                serde_json::from_str(&resp_text).with_context(|| {
                    format!(
                        "parsing xAI chat response: {}",
                        truncate_err(&redact_secrets(&resp_text), 300)
                    )
                })?;

            // Count the response even when the API omits its usage block: the
            // request was made, and tool hops each add another round trip.
            usage.requests += 1;
            if let Some(u) = parsed.usage {
                usage.add(&UsageTotals {
                    prompt_tokens: u.prompt_tokens.unwrap_or(0),
                    completion_tokens: u.completion_tokens.unwrap_or(0),
                    total_tokens: u.total_tokens.unwrap_or(0),
                    requests: 0,
                });
            }

            let choice = parsed
                .choices
                .into_iter()
                .next()
                .context("xAI response had no choices")?;
            let message = choice.message;

            // Append assistant message (preserve tool_calls for the protocol).
            let mut assistant_msg = json!({
                "role": "assistant",
                "content": message.content.clone().unwrap_or_default(),
            });
            if let Some(ref tcs) = message.tool_calls
                && !tcs.is_empty()
            {
                assistant_msg["tool_calls"] = serde_json::to_value(tcs)?;
            }
            messages.push(assistant_msg);

            let tool_calls = message.tool_calls.unwrap_or_default();
            if tool_calls.is_empty() {
                final_text = Some(message.content.unwrap_or_default());
                break;
            }

            for (index, tc) in tool_calls.into_iter().enumerate() {
                check_cancelled(cancel.as_ref())?;
                let name = tc.function.name.clone();
                let args_str = tc.function.arguments.clone();

                let result = if index >= MAX_TOOL_CALLS_PER_RESPONSE {
                    tools::ToolResult::err(format!(
                        "❌ Too many tool calls in one response (max \
                         {MAX_TOOL_CALLS_PER_RESPONSE}); '{name}' was not executed."
                    ))
                } else {
                    match parse_tool_args(&args_str) {
                        // Malformed args must never execute the tool: a
                        // fallback to `{}` would silently run zero-arg tools
                        // like advance_turn / end_combat.
                        Ok(args) => tools::dispatch(&mut working, &name, &args),
                        Err(reason) => tools::ToolResult::err(format!(
                            "❌ Invalid arguments for tool '{name}': {reason}"
                        )),
                    }
                };

                // A failed mutation changed nothing; it must not disable the
                // marker safety net (e.g. failed modify_hp + [DAMAGE:5]).
                if !result.is_error && tools::is_mutating(&name) {
                    mutating = true;
                }

                let content = result.content;
                all_tool_calls.push(ToolCallRecord {
                    name: name.clone(),
                    arguments: args_str,
                    result: content.clone(),
                });
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tc.id,
                    "content": content,
                }));
            }
        }

        let Some(mut final_text) = final_text else {
            bail!("DM turn exceeded {MAX_TOOL_ITERATIONS} tool iterations without final narration");
        };

        if final_text.is_empty() && !all_tool_calls.is_empty() {
            final_text = "(The DM finished tool work but returned no narration.)".to_string();
        }

        // Close the commit window: cancellation observed before this point
        // still rolls the whole turn back.
        check_cancelled(cancel.as_ref())?;
        *state = working;

        Ok(TurnResult {
            text: final_text,
            tool_calls: all_tool_calls,
            mutating_tool_ran: mutating,
            usage,
        })
    }

    /// POST one chat-completions body, retrying transient failures.
    ///
    /// Retries 429/500/502/503/504 and connection errors up to
    /// [`MAX_HTTP_ATTEMPTS`] total attempts with exponential backoff.
    /// `Retry-After` (integer seconds) overrides the backoff, capped at
    /// [`RETRY_MAX_DELAY`]. Auth/validation 4xx responses are returned
    /// immediately for the caller to report.
    async fn send_with_retries(
        &self,
        url: &str,
        body: &Value,
        cancel: Option<&CancellationToken>,
        iteration: usize,
    ) -> Result<reqwest::Response> {
        let mut attempt = 0u32;
        loop {
            check_cancelled(cancel)?;

            let mut request = self
                .client
                .post(url)
                .bearer_auth(&self.api_key)
                .header("Content-Type", "application/json");
            if let Some(ref id) = self.conv_id {
                request = request.header("x-grok-conv-id", id);
            }
            let request = request.json(body);

            let sent = match cancel {
                Some(token) => {
                    tokio::select! {
                        biased;
                        _ = token.cancelled() => bail!(TURN_CANCELLED_MSG),
                        resp = request.send() => resp,
                    }
                }
                None => request.send().await,
            };

            match sent {
                Ok(resp) => {
                    if is_retryable_status(resp.status()) && attempt + 1 < MAX_HTTP_ATTEMPTS {
                        let delay = retry_delay(resp.headers(), attempt);
                        drop(resp);
                        sleep_or_cancel(delay, cancel).await?;
                        attempt += 1;
                        continue;
                    }
                    return Ok(resp);
                }
                Err(err) => {
                    if err.is_connect() && attempt + 1 < MAX_HTTP_ATTEMPTS {
                        sleep_or_cancel(exponential_backoff(attempt), cancel).await?;
                        attempt += 1;
                        continue;
                    }
                    return Err(err)
                        .with_context(|| format!("xAI chat request failed (iter {iteration})"));
                }
            }
        }
    }
}

/// Chat-completions body. `reasoning_effort` is a top-level field on this API
/// (the Responses API nests it under `reasoning.effort` instead).
fn completion_body(
    model: &str,
    reasoning_effort: &str,
    messages: &[Value],
    tools: &Value,
    temperature: f64,
) -> Value {
    json!({
        "model": model,
        "messages": messages,
        "tools": tools,
        "tool_choice": "auto",
        "temperature": temperature,
        "reasoning_effort": reasoning_effort,
    })
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().and_then(|value| {
        let value = value.trim();
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
}

fn check_cancelled(cancel: Option<&CancellationToken>) -> Result<()> {
    if cancel.is_some_and(|t| t.is_cancelled()) {
        bail!(TURN_CANCELLED_MSG);
    }
    Ok(())
}

/// Read a response body, aborting promptly if the turn is cancelled.
async fn read_response_body(
    resp: reqwest::Response,
    cancel: Option<&CancellationToken>,
) -> Result<String> {
    let read = read_text_bounded(resp, MAX_CHAT_BODY_BYTES, "xAI chat");
    match cancel {
        Some(token) => tokio::select! {
            biased;
            _ = token.cancelled() => bail!(TURN_CANCELLED_MSG),
            text = read => text,
        },
        None => read.await,
    }
}

/// Parse tool-call arguments without a `{}` fallback.
///
/// The fallback used to execute zero-argument tools (advance_turn,
/// end_combat, ...) on malformed model output. Reject instead so the model
/// receives an error tool result and can retry with valid JSON.
fn parse_tool_args(raw: &str) -> std::result::Result<Value, String> {
    let value: Value = serde_json::from_str(raw).map_err(|err| format!("invalid JSON ({err})"))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err("expected a JSON object".to_string())
    }
}

fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504)
}

/// Exponential backoff: 250ms, 500ms, 1s, ... capped at [`RETRY_MAX_DELAY`].
fn exponential_backoff(attempt: u32) -> Duration {
    let factor = 1u32 << attempt.min(4);
    (RETRY_BASE_DELAY * factor).min(RETRY_MAX_DELAY)
}

/// Delay before the next attempt: `Retry-After` seconds when the server sent
/// one (capped at [`RETRY_MAX_DELAY`]), otherwise exponential backoff.
fn retry_delay(headers: &HeaderMap, attempt: u32) -> Duration {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|secs| Duration::from_secs(secs.min(RETRY_MAX_DELAY.as_secs())))
        .unwrap_or_else(|| exponential_backoff(attempt))
}

/// Sleep between retries, but wake immediately when the turn is cancelled.
async fn sleep_or_cancel(delay: Duration, cancel: Option<&CancellationToken>) -> Result<()> {
    match cancel {
        Some(token) => {
            tokio::select! {
                biased;
                _ = token.cancelled() => bail!(TURN_CANCELLED_MSG),
                _ = tokio::time::sleep(delay) => {}
            }
            Ok(())
        }
        None => {
            tokio::time::sleep(delay).await;
            Ok(())
        }
    }
}

fn truncate_err(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

// --- Wire types (OpenAI-compatible) ---

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<Choice>,
    usage: Option<UsageWire>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: AssistantMessage,
}

#[derive(Debug, Deserialize, Serialize)]
struct AssistantMessage {
    role: Option<String>,
    #[serde(default, deserialize_with = "deserialize_message_content")]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallWire>>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct ToolCallWire {
    id: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    function: FunctionWire,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct FunctionWire {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct UsageWire {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    total_tokens: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn default_model_is_set() {
        // Don't rely on env; just construct.
        let c = DmClient::with_model("key", "grok-test");
        assert_eq!(c.model(), "grok-test");
        assert_eq!(c.reasoning_effort(), "low");
    }

    #[test]
    fn debug_never_prints_the_api_key() {
        const FAKE_KEY: &str = "xai-dm-debug-SECRET-0987654321";
        let client = DmClient::with_model(FAKE_KEY, "grok-test");
        let out = format!("{client:?}");
        assert!(!out.contains(FAKE_KEY), "debug leaked key: {out}");
        assert!(out.contains("<redacted>"));
    }

    #[tokio::test]
    async fn provider_error_body_is_redacted_before_display() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const FAKE_KEY: &str = "xai-error-body-SECRET-123456789";
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(format!(
                "invalid api_key={FAKE_KEY} Authorization: Bearer sk-fake-token-12345"
            )))
            .mount(&server)
            .await;

        let client = DmClient::with_model("test-key", "grok-test").with_base_url(server.uri());
        let mut state = GameState::new("Hero");
        let err = client
            .query("sys", "look", &mut state, &ChatHistory::new(0))
            .await
            .expect_err("400 should fail");
        let msg = format!("{err:#}");
        assert!(!msg.contains(FAKE_KEY), "error leaked key: {msg}");
        assert!(
            !msg.contains("sk-fake-token-12345"),
            "error leaked key: {msg}"
        );
        assert!(msg.contains("[REDACTED]"), "error was not redacted: {msg}");
    }

    #[tokio::test]
    async fn oversized_chat_body_is_rejected() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = "x".repeat(MAX_CHAT_BODY_BYTES + 1);
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let client = DmClient::with_model("test-key", "grok-test").with_base_url(server.uri());
        let mut state = GameState::new("Hero");
        let err = client
            .query("sys", "look", &mut state, &ChatHistory::new(0))
            .await
            .expect_err("oversized body should fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("too large") || msg.contains("exceeds"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn completion_body_pins_model_and_reasoning_effort() {
        let body = completion_body("grok-4.7", "low", &[], &json!([]), 0.7);
        assert_eq!(body["model"], "grok-4.7");
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(body["tool_choice"], "auto");
        assert!((body["temperature"].as_f64().unwrap() - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn truncate_err_does_not_split_a_multibyte_char() {
        let s = "err ☃☃☃";
        let out = truncate_err(s, 6);
        assert!(out.ends_with('…'));
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn usage_add() {
        let mut a = UsageTotals {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            requests: 2,
        };
        a.add(&UsageTotals {
            prompt_tokens: 3,
            completion_tokens: 2,
            total_tokens: 5,
            requests: 1,
        });
        assert_eq!(a.prompt_tokens, 13);
        assert_eq!(a.completion_tokens, 7);
        assert_eq!(a.total_tokens, 20);
        assert_eq!(a.requests, 3);
        assert_eq!(UsageTotals::default().requests, 0);
    }

    #[test]
    fn is_turn_cancelled_detects_message() {
        let err = anyhow::anyhow!(TURN_CANCELLED_MSG);
        assert!(is_turn_cancelled(&err));
        let wrapped = err.context("outer");
        assert!(is_turn_cancelled(&wrapped));
        assert!(!is_turn_cancelled(&anyhow::anyhow!("other error")));
    }

    #[tokio::test]
    async fn cancel_before_request_aborts() {
        let client = DmClient::with_model("key", "grok-test").with_base_url("http://127.0.0.1:1"); // unreachable if used
        let mut state = GameState::new("Hero");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let history = ChatHistory::new(0);

        let err = client
            .query_cancellable("sys", "look around", &mut state, Some(cancel), &history)
            .await
            .expect_err("should cancel");
        assert!(is_turn_cancelled(&err));
    }

    #[tokio::test]
    async fn cancel_aborts_in_flight_http() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(30))
                    .set_body_json(serde_json::json!({
                        "choices": [{
                            "message": {
                                "role": "assistant",
                                "content": "too late"
                            }
                        }]
                    })),
            )
            .mount(&server)
            .await;

        let client = DmClient::with_model("key", "grok-test").with_base_url(server.uri());
        let mut state = GameState::new("Hero");
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let history = ChatHistory::new(0);

        let query = client.query_cancellable("sys", "look", &mut state, Some(cancel2), &history);
        tokio::pin!(query);

        // Cancel after a brief delay so the request is in flight.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();
        });

        let err = tokio::time::timeout(Duration::from_secs(5), query)
            .await
            .expect("query should finish quickly after cancel")
            .expect_err("should be cancelled");
        assert!(is_turn_cancelled(&err), "err was: {err:#}");
    }

    #[test]
    fn chat_history_disabled_is_noop() {
        let mut h = ChatHistory::new(0);
        h.push("look", "You see a door.");
        assert!(h.is_empty());
        assert!(h.api_messages().is_empty());
    }

    #[test]
    fn chat_history_caps_turns() {
        let mut h = ChatHistory::new(2);
        h.push("a", "A");
        h.push("b", "B");
        h.push("c", "C");
        assert_eq!(h.len(), 2);
        let msgs = h.api_messages();
        assert_eq!(msgs.len(), 4);
        assert!(
            msgs[0]["content"]
                .as_str()
                .unwrap()
                .contains("Player action: b")
        );
        assert_eq!(msgs[1]["content"], "B");
        assert!(
            msgs[2]["content"]
                .as_str()
                .unwrap()
                .contains("Player action: c")
        );
        assert_eq!(msgs[3]["content"], "C");
    }

    #[test]
    fn chat_history_caps_chars() {
        let mut h = ChatHistory {
            max_turns: 10,
            max_chars: 50,
            exchanges: VecDeque::new(),
        };
        h.push("short", "ok");
        h.push("x", &"y".repeat(100));
        // First exchange dropped once second pushes over the budget.
        assert_eq!(h.len(), 1);
        assert!(
            h.api_messages()[0]["content"]
                .as_str()
                .unwrap()
                .contains("Player action: x")
        );
    }
}
