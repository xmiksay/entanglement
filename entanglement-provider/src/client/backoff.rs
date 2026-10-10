//! Retry policy helpers: backoff growth, retryable-status / transient-error
//! classification and `Retry-After` parsing.

use std::time::Duration;

use super::MAX_RETRY_AFTER;

/// Grow the backoff geometrically, cap it, then add up to 100% jitter.
pub(super) fn next_backoff(backoff: Duration, max: Duration) -> Duration {
    let capped = std::cmp::min(backoff * 2, max);
    let jitter = jitter_unit() * capped.as_millis() as f64;
    Duration::from_millis((capped.as_millis() as f64 + jitter) as u64)
}

/// A jitter factor in `[0, 1)` from the wall clock's subsecond nanos — retry
/// spreading needs no statistical quality, and this keeps the crate free of
/// the `rand`/`getrandom` dependency trees it would otherwise pull for this
/// one call.
pub(super) fn jitter_unit() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos) / 1e9
}

/// A retryable HTTP status: server errors and 429 Too Many Requests.
pub(super) fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status.as_u16() == 429
}

/// Check if a transport error is transient and should be retried.
pub(super) fn is_transient_error(error: &reqwest::Error) -> bool {
    // Connection establishment, timeout, or a request-send fault. A dropped
    // keep-alive connection reset between requests renders as `is_request()`
    // (reqwest's "error sending request for url ...") — not `is_connect()`,
    // which flags connection *establishment* only. Retrying is safe: the body
    // was sent up front and no response body was consumed, so a resend is a
    // fresh attempt, not a partial-write hazard.
    if error.is_timeout() || error.is_connect() || error.is_request() {
        return true;
    }
    if let Some(status) = error.status() {
        if is_retryable_status(status) {
            return true;
        }
    }
    error.to_string().contains("incomplete")
}

/// Parse a `Retry-After` header (delta-seconds or an HTTP date) into a
/// duration, clamped to [`MAX_RETRY_AFTER`]. A hostile or misbehaving
/// endpoint/proxy can send an arbitrary huge delta-seconds value (e.g.
/// `u64::MAX`) or a far-future HTTP-date; unclamped, that value later feeds
/// `Instant::now() + delay`, which **panics** on overflow — taking down the
/// session task silently, since nothing watches its `JoinHandle` (#548). This
/// is the single choke point every caller (`send_with_retry`,
/// [`extract_retry_after_from_response`]) goes through.
pub(super) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let val = headers.get("Retry-After")?.to_str().ok()?;
    let delay = if let Ok(seconds) = val.parse::<u64>() {
        Duration::from_secs(seconds)
    } else if let Ok(datetime) = httpdate::parse_http_date(val) {
        datetime.duration_since(std::time::SystemTime::now()).ok()?
    } else {
        return None;
    };
    Some(delay.min(MAX_RETRY_AFTER))
}

/// Extract a `Retry-After` duration from a 429 response, for callers that want to
/// report the server-advised backoff when surfacing a rate-limit error.
pub fn extract_retry_after_from_response(response: &reqwest::Response) -> Option<Duration> {
    if response.status().as_u16() != 429 {
        return None;
    }
    parse_retry_after(response.headers())
}
