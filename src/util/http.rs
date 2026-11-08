//! Shared HTTP client for xAI calls.
//!
//! Reqwest's default client has no timeout, so a stalled request would hang
//! the game until the process is killed.

use std::time::Duration;

/// Client with a connect cap and a total request deadline.
pub fn api_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .build()
        .expect("reqwest client")
}
