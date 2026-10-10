//! Per-request header assembly and server-request replies for
//! [`McpHttpClient`] — split out of `http.rs` along the 400-line file cap when
//! `ping` replies (#727) joined the transport. A child module, so it reaches
//! the client's private fields directly.

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;

use super::{McpHttpClient, PROTOCOL_HEADER, SESSION_HEADER};
use crate::oauth::AuthRequired;

/// How long a reply to a server-initiated request (`ping`, #727) may take
/// before it is abandoned — the drain it interrupts is waiting on it.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

impl McpHttpClient {
    /// Answer a server-initiated request seen on an SSE drain (`ping`, #727).
    /// Deliberately *outside* the endpoint pool: the drain that found the
    /// request still holds this endpoint's concurrency permit, so a pooled
    /// `POST` could wait on itself under a cap of 1. Best-effort — a failed
    /// reply is only logged; the server decides what an unanswered ping means.
    pub(super) async fn reply(&self, frame: Value) {
        let sent = async {
            let headers = self.request_headers(false).await?;
            let req = self
                .http
                .client()
                .post(&self.url)
                .headers(headers)
                .json(&frame);
            let status = tokio::time::timeout(REPLY_TIMEOUT, req.send())
                .await
                .context("timed out")??
                .status();
            anyhow::ensure!(status.is_success(), "HTTP {status}");
            anyhow::Ok(())
        };
        if let Err(e) = sent.await {
            tracing::debug!(server = %self.server, "MCP server-request reply failed: {e:#}");
        }
    }

    /// The headers every `POST` carries: the static ones, the OAuth bearer
    /// (when configured), the negotiated protocol version, and the session id.
    pub(super) async fn request_headers(&self, force_refresh: bool) -> Result<HeaderMap> {
        let mut headers = self.headers.clone();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        // Accept both shapes the streamable-HTTP server may answer with.
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        if let Some(auth) = &self.auth {
            // A failure here (no stored token, refresh rejected) is itself an
            // authorization problem, not a transport one — surface it as such so
            // the runtime reports "needs auth".
            let token = auth.access_token(force_refresh).await.map_err(|e| {
                anyhow::Error::new(AuthRequired {
                    server: self.server.clone(),
                    resource_metadata: None,
                })
                .context(format!("obtaining an access token: {e}"))
            })?;
            let value = HeaderValue::from_str(&format!("Bearer {token}"))
                .context("building the MCP bearer header")?;
            headers.insert(AUTHORIZATION, value);
        }
        if let Some(v) = self
            .protocol_version
            .lock()
            .expect("MCP protocol-version mutex poisoned")
            .clone()
        {
            let value =
                HeaderValue::from_str(&v).context("building the MCP-Protocol-Version header")?;
            headers.insert(PROTOCOL_HEADER, value);
        }
        if let Some(sid) = self
            .session_id
            .lock()
            .expect("MCP session-id mutex poisoned")
            .clone()
        {
            let value =
                HeaderValue::from_str(&sid).context("building the Mcp-Session-Id header")?;
            headers.insert(SESSION_HEADER, value);
        }
        Ok(headers)
    }
}
