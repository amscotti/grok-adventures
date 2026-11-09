//! Shared HTTP client and bounded response-body reader for xAI calls.
//!
//! Reqwest's default client has no timeout, so a stalled request would hang
//! the game until the process is killed. Response bodies are also read through
//! a hard byte cap so a hostile or broken endpoint cannot exhaust memory.

use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Client with a connect cap and a total request deadline.
pub fn api_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .build()
        .expect("reqwest client")
}

/// Read a response body, rejecting it when it exceeds `max_bytes`.
///
/// `Content-Length` is checked up front when present; the streaming read also
/// enforces the cap per chunk so chunked responses cannot grow without bound.
pub async fn read_body_bounded(
    mut resp: reqwest::Response,
    max_bytes: usize,
    label: &str,
) -> Result<Vec<u8>> {
    let declared = resp.content_length();
    if let Some(length) = declared
        && length > max_bytes as u64
    {
        bail!(
            "{label} response is too large: Content-Length {length} bytes exceeds the \
             {max_bytes}-byte limit"
        );
    }

    let capacity = declared.unwrap_or(0).min(max_bytes as u64) as usize;
    let mut body = Vec::with_capacity(capacity);
    while let Some(chunk) = resp
        .chunk()
        .await
        .with_context(|| format!("reading {label} response body"))?
    {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            bail!("{label} response body exceeds the {max_bytes}-byte limit while streaming");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Read a UTF-8 response body, rejecting it when it exceeds `max_bytes`.
pub async fn read_text_bounded(
    resp: reqwest::Response,
    max_bytes: usize,
    label: &str,
) -> Result<String> {
    let bytes = read_body_bounded(resp, max_bytes, label).await?;
    String::from_utf8(bytes).with_context(|| format!("{label} response body was not valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn body_response(body: &[u8]) -> (MockServer, reqwest::Response) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/body"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        let resp = api_client(Duration::from_secs(5))
            .get(format!("{}/body", server.uri()))
            .send()
            .await
            .expect("request");
        (server, resp)
    }

    #[tokio::test]
    async fn accepts_body_at_or_below_the_cap() {
        let body = b"hello world";
        let (_server, resp) = body_response(body).await;
        let out = read_body_bounded(resp, body.len(), "test").await.unwrap();
        assert_eq!(out, body);

        let (_server, resp) = body_response(body).await;
        let text = read_text_bounded(resp, 64, "test").await.unwrap();
        assert_eq!(text, "hello world");
    }

    #[tokio::test]
    async fn rejects_body_over_the_cap() {
        let (_server, resp) = body_response(&[b'x'; 128]).await;
        let err = read_body_bounded(resp, 16, "test").await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("too large") || msg.contains("exceeds"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test]
    async fn rejects_invalid_utf8_text() {
        let (_server, resp) = body_response(&[0xff, 0xfe]).await;
        let err = read_text_bounded(resp, 16, "test").await.unwrap_err();
        assert!(format!("{err:#}").contains("UTF-8"));
    }
}
