//! Streaming response bodies and opt-in request-body logging.

use futures::StreamExt;
use tokio::sync::mpsc;

#[cfg(doc)]
use super::HttpClient;
use super::{StreamGuard, STREAM_IDLE_TIMEOUT};

/// Forward a streaming response body over an mpsc channel, chunk by chunk, with
/// an **idle-gap** watchdog: if no bytes arrive within [`STREAM_IDLE_TIMEOUT`]
/// the stream is aborted with an error frame. This replaces the old
/// whole-request timeout — a long healthy SSE stream now runs to completion
/// while a hung one still dies fast (#241). `label` names the source (e.g.
/// `"openai-compat"`) for the error messages. The `reqwest::Client` is built
/// with `connect_timeout` only, so this per-chunk gap is what bounds a stalled
/// body.
///
/// The [`StreamGuard`] (the per-endpoint concurrency permit from
/// [`HttpClient::execute_with_retry`]) is moved into the pump task and dropped
/// when the body ends, so an endpoint's in-flight cap counts open streams for
/// their full lifetime — not just until the response headers arrive.
///
/// Races each read against the consumer dropping its [`mpsc::Receiver`]
/// (`tx.closed()`), not just against the idle-gap timeout (#552): a `Stop`
/// mid silent reasoning pause, or the OpenAI-compat parser's `break 'outer` on
/// `[DONE]` while a keep-alive proxy holds the connection open, both drop the
/// receiver without this task ever attempting (and failing) another `send` —
/// it was stuck awaiting the *next* chunk, which may never come. Without
/// racing the drop, the guard — and with it the endpoint/model permits and
/// the cross-process lease — stayed held for up to the full
/// [`STREAM_IDLE_TIMEOUT`] after the caller had already stopped wanting them;
/// with cap 3 that could lock a user out of their own endpoint for two
/// minutes after cancelling three turns.
pub fn spawn_byte_stream(
    response: reqwest::Response,
    label: &'static str,
    guard: StreamGuard,
) -> mpsc::Receiver<Result<Vec<u8>, anyhow::Error>> {
    let (tx, rx) = mpsc::channel::<Result<Vec<u8>, anyhow::Error>>(8);
    tokio::spawn(async move {
        let _guard = guard; // release the concurrency slot when the body ends
        let mut bytes = response.bytes_stream();
        loop {
            tokio::select! {
                biased;
                _ = tx.closed() => break, // consumer gone: release the guard now, not on the next idle tick
                next = tokio::time::timeout(STREAM_IDLE_TIMEOUT, bytes.next()) => {
                    match next {
                        Ok(Some(item)) => {
                            let chunk = item
                                .map(|c| c.to_vec())
                                .map_err(|e| anyhow::anyhow!("{label} stream read: {e}"));
                            if tx.send(chunk).await.is_err() {
                                break; // consumer dropped
                            }
                        }
                        Ok(None) => break, // stream ended cleanly
                        Err(_) => {
                            let _ = tx
                                .send(Err(anyhow::anyhow!(
                                    "{label} stream stalled: no data for {STREAM_IDLE_TIMEOUT:?}"
                                )))
                                .await;
                            break;
                        }
                    }
                }
            }
        }
    });
    rx
}

/// Env flag that opts into logging full LLM request bodies. A request body
/// carries the system prompt, the **entire conversation**, and the tool schemas
/// — repo/user data (never API keys: those ride in headers). It is therefore off
/// by default and only ever emitted when a human explicitly asks for it; `RUST_LOG`
/// verbosity alone is deliberately not enough (#165).
pub(super) const LOG_BODIES_ENV: &str = "ENTANGLEMENT_LOG_BODIES";

/// Cap on how many bytes of a request body ever reach the log, even opted in —
/// a full conversation can be megabytes; this keeps the sink bounded.
pub(super) const MAX_LOGGED_BODY_BYTES: usize = 8 * 1024;

/// Log an LLM request body at `debug!`, but **only** when
/// `ENTANGLEMENT_LOG_BODIES=1`. Shared by every provider client so body logging
/// is symmetric and greppable (`provider` tags the backend); the payload holds
/// conversation + repo data, so it stays behind the explicit opt-in and is
/// truncated to [`MAX_LOGGED_BODY_BYTES`] (#165).
pub fn log_request_body(provider: &str, body: &serde_json::Value) {
    if std::env::var(LOG_BODIES_ENV).as_deref() != Ok("1") {
        return;
    }
    let rendered = body.to_string();
    let (shown, truncated) = truncate_on_boundary(&rendered, MAX_LOGGED_BODY_BYTES);
    tracing::debug!(
        provider,
        bytes = rendered.len(),
        truncated,
        body = shown,
        "LLM request body (conversation + repo data; gated by ENTANGLEMENT_LOG_BODIES=1)"
    );
}

/// Truncate `s` to at most `max` bytes without splitting a UTF-8 char, returning
/// the slice and whether anything was dropped.
pub(super) fn truncate_on_boundary(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}
