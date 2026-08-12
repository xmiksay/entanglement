//! RFC 8628 OAuth 2.0 Device Authorization Grant (#631, amending ADR-0153).
//!
//! The alternative to [`super::flow::AuthFlow`] for a host reached only over
//! SSH — no browser *and* no way to forward the ephemeral loopback port that
//! [`super::loopback`] binds. Instead of a redirect, the client is handed a
//! short `user_code` + a `verification_uri` to show the user, who opens it on
//! *any* device (not the host), and the client polls the token endpoint until
//! they finish.
//!
//! Structurally parallel to `AuthFlow`: both share discovery ([`discovery`])
//! and dynamic client registration ([`dcr`]); they differ only in "how does
//! the human authorize". There is no redirect listener here, and no PKCE —
//! RFC 8628 has no redirect URI to protect with PKCE against, since the
//! authorization server never sends anything back to the client directly.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use super::{dcr, discovery, http_client, token, ClientRegistration, OauthConfig, StoredAuth};

/// The client name presented at dynamic registration, mirroring
/// [`super::flow`]'s constant of the same name.
const CLIENT_NAME: &str = "skutter";

/// RFC 8628 §3.2 device authorization response (only the fields used).
#[derive(Debug, serde::Deserialize)]
struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

/// RFC 8628 §3.2: absent `interval` defaults to 5 seconds.
fn default_interval() -> u64 {
    5
}

/// Entry point, mirroring [`super::flow::AuthFlow::begin`].
pub struct DeviceFlow;

impl DeviceFlow {
    /// Resolve endpoints, register a client if needed, and request a device
    /// code. Performs network I/O but never blocks on the user — that's
    /// [`PendingDeviceAuthorization::poll`].
    pub async fn begin(
        server: &str,
        mcp_url: &str,
        cfg: &OauthConfig,
        resource_metadata_hint: Option<&str>,
    ) -> Result<PendingDeviceAuthorization> {
        let http = http_client();
        let endpoints = discovery::discover(
            &http,
            mcp_url,
            resource_metadata_hint,
            cfg.authorization_url.as_deref(),
            cfg.token_url.as_deref(),
            cfg.registration_url.as_deref(),
            cfg.device_authorization_url.as_deref(),
        )
        .await
        .with_context(|| format!("discovering OAuth metadata for MCP server `{server}`"))?;

        let device_endpoint = endpoints
            .device_authorization_endpoint
            .clone()
            .with_context(|| {
                format!(
                    "MCP server `{server}` advertises no device-authorization endpoint (RFC \
                     8628) — it may support only the browser flow (`/mcp connect {server}`), or \
                     set `oauth.device_authorization_url` in config.yml if you know the endpoint"
                )
            })?;

        let scopes = if cfg.scopes.is_empty() {
            endpoints.scopes_supported.clone()
        } else {
            cfg.scopes.clone()
        };

        let client = match &cfg.client_id {
            Some(id) => ClientRegistration {
                client_id: id.clone(),
                client_secret: cfg.client_secret.clone(),
            },
            None => {
                let endpoint = endpoints
                    .registration_endpoint
                    .as_deref()
                    .with_context(|| {
                        format!(
                        "MCP server `{server}` advertises no dynamic-registration endpoint and \
                         no `client_id` is configured — set `oauth.client_id` in config.yml"
                    )
                    })?;
                dcr::register(
                    &http,
                    endpoint,
                    None,
                    &[
                        "urn:ietf:params:oauth:grant-type:device_code",
                        "refresh_token",
                    ],
                    &scopes,
                    CLIENT_NAME,
                )
                .await
                .with_context(|| format!("registering a client with `{server}`"))?
            }
        };

        let resp = request_device_code(
            &http,
            &device_endpoint,
            &client,
            &scopes,
            endpoints.resource.as_deref(),
        )
        .await
        .with_context(|| format!("requesting a device code for `{server}`"))?;

        Ok(PendingDeviceAuthorization {
            http,
            server: server.to_string(),
            device_code: resp.device_code,
            interval: Duration::from_secs(resp.interval.max(1)),
            expires_at: Instant::now() + Duration::from_secs(resp.expires_in),
            verification_uri: resp.verification_uri,
            verification_uri_complete: resp.verification_uri_complete,
            user_code: resp.user_code,
            client,
            endpoints,
        })
    }
}

async fn request_device_code(
    http: &reqwest::Client,
    device_endpoint: &str,
    client: &ClientRegistration,
    scopes: &[String],
    resource: Option<&str>,
) -> Result<DeviceAuthorizationResponse> {
    let mut form = vec![("client_id", client.client_id.as_str())];
    let scope_joined;
    if !scopes.is_empty() {
        scope_joined = scopes.join(" ");
        form.push(("scope", scope_joined.as_str()));
    }
    if let Some(r) = resource {
        form.push(("resource", r));
    }
    let mut req = http.post(device_endpoint).form(&form);
    if let Some(secret) = &client.client_secret {
        req = req.basic_auth(&client.client_id, Some(secret));
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("POST {device_endpoint}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "device-authorization endpoint returned HTTP {status}: {}",
            token::oauth_error(&body)
        );
    }
    serde_json::from_str(&body).with_context(|| {
        format!("parsing the device-authorization response from {device_endpoint}")
    })
}

/// A device code waiting on the user to visit `verification_uri` and enter
/// `user_code` (or `verification_uri_complete`, which pre-fills it).
#[derive(Debug)]
pub struct PendingDeviceAuthorization {
    http: reqwest::Client,
    server: String,
    device_code: String,
    interval: Duration,
    expires_at: Instant,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    user_code: String,
    client: ClientRegistration,
    endpoints: discovery::Endpoints,
}

impl PendingDeviceAuthorization {
    pub fn verification_uri(&self) -> &str {
        &self.verification_uri
    }

    pub fn verification_uri_complete(&self) -> Option<&str> {
        self.verification_uri_complete.as_deref()
    }

    pub fn user_code(&self) -> &str {
        &self.user_code
    }

    /// Poll the token endpoint (RFC 8628 §3.4/§3.5) until the user finishes,
    /// the device code expires, or the server reports a terminal failure.
    pub async fn poll(self) -> Result<StoredAuth> {
        let mut interval = self.interval;
        loop {
            if Instant::now() >= self.expires_at {
                bail!(
                    "device code for `{}` expired before authorization completed — run \
                     `/mcp connect {} --device-code` again",
                    self.server,
                    self.server
                );
            }
            tokio::time::sleep(interval).await;
            match poll_once(
                &self.http,
                &self.endpoints.token_endpoint,
                &self.client,
                &self.device_code,
                self.endpoints.resource.as_deref(),
            )
            .await?
            {
                PollOutcome::Granted(tokens) => {
                    return Ok(StoredAuth {
                        client_id: self.client.client_id,
                        client_secret: self.client.client_secret,
                        token_endpoint: self.endpoints.token_endpoint,
                        revocation_endpoint: self.endpoints.revocation_endpoint,
                        resource: self.endpoints.resource,
                        tokens,
                    });
                }
                PollOutcome::Pending => continue,
                PollOutcome::SlowDown => {
                    // RFC 8628 §3.5: back off by 5s and keep the new interval
                    // for the rest of this poll, not just the next tick.
                    interval += Duration::from_secs(5);
                    continue;
                }
            }
        }
    }
}

// No `PartialEq` derive: `Granted` carries a `TokenSet`, which deliberately
// implements no `PartialEq` (nothing should ever diff two token sets by
// value) — tests match on the variant instead of `assert_eq!`ing it.
#[derive(Debug)]
enum PollOutcome {
    Granted(super::TokenSet),
    Pending,
    SlowDown,
}

async fn poll_once(
    http: &reqwest::Client,
    token_endpoint: &str,
    client: &ClientRegistration,
    device_code: &str,
    resource: Option<&str>,
) -> Result<PollOutcome> {
    let mut form = vec![
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ("device_code", device_code),
        ("client_id", client.client_id.as_str()),
    ];
    if let Some(r) = resource {
        form.push(("resource", r));
    }
    let mut req = http.post(token_endpoint).form(&form);
    if let Some(secret) = &client.client_secret {
        req = req.basic_auth(&client.client_id, Some(secret));
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("POST {token_endpoint}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    classify_poll_response(status.is_success(), &body)
        .with_context(|| format!("parsing the device-code poll response from {token_endpoint}"))
}

/// Turn one token-endpoint poll response into a [`PollOutcome`] (RFC 8628
/// §3.4/§3.5), pulled out of [`poll_once`] so every branch — granted,
/// `authorization_pending`, `slow_down`, and a terminal denial/malformed body
/// — is unit-testable with no network mock.
fn classify_poll_response(is_success: bool, body: &str) -> Result<PollOutcome> {
    if is_success {
        let parsed: token::TokenResponse = serde_json::from_str(body)?;
        return Ok(PollOutcome::Granted(parsed.into_token_set(None)));
    }
    match token::oauth_error_code(body).as_deref() {
        Some("authorization_pending") => Ok(PollOutcome::Pending),
        Some("slow_down") => Ok(PollOutcome::SlowDown),
        _ => bail!("device authorization failed: {}", token::oauth_error(body)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_authorization_response_defaults_interval_and_complete_uri() {
        let resp: DeviceAuthorizationResponse = serde_json::from_str(
            r#"{
                "device_code": "dc",
                "user_code": "ABCD-EFGH",
                "verification_uri": "https://as.example/device",
                "expires_in": 1800
            }"#,
        )
        .unwrap();
        assert_eq!(resp.device_code, "dc");
        assert_eq!(resp.user_code, "ABCD-EFGH");
        assert!(resp.verification_uri_complete.is_none());
        assert_eq!(
            resp.interval, 5,
            "absent interval must default to 5 (RFC 8628 §3.2)"
        );
    }

    #[test]
    fn device_authorization_response_carries_the_complete_uri_and_interval_when_present() {
        let resp: DeviceAuthorizationResponse = serde_json::from_str(
            r#"{
                "device_code": "dc",
                "user_code": "ABCD-EFGH",
                "verification_uri": "https://as.example/device",
                "verification_uri_complete": "https://as.example/device?user_code=ABCD-EFGH",
                "expires_in": 1800,
                "interval": 10
            }"#,
        )
        .unwrap();
        assert_eq!(
            resp.verification_uri_complete.as_deref(),
            Some("https://as.example/device?user_code=ABCD-EFGH")
        );
        assert_eq!(resp.interval, 10);
    }

    #[test]
    fn classify_poll_response_success_yields_granted() {
        let body = r#"{"access_token":"at","token_type":"Bearer","expires_in":3600}"#;
        let got = classify_poll_response(true, body).unwrap();
        match got {
            PollOutcome::Granted(tokens) => assert_eq!(tokens.access_token, "at"),
            other => panic!("expected Granted, got {other:?}"),
        }
    }

    #[test]
    fn classify_poll_response_authorization_pending_keeps_polling() {
        let body = r#"{"error":"authorization_pending"}"#;
        assert!(matches!(
            classify_poll_response(false, body).unwrap(),
            PollOutcome::Pending
        ));
    }

    #[test]
    fn classify_poll_response_slow_down_backs_off() {
        let body = r#"{"error":"slow_down"}"#;
        assert!(matches!(
            classify_poll_response(false, body).unwrap(),
            PollOutcome::SlowDown
        ));
    }

    #[test]
    fn classify_poll_response_denial_is_terminal() {
        let body = r#"{"error":"access_denied","error_description":"user declined"}"#;
        let err = classify_poll_response(false, body).unwrap_err();
        assert!(err.to_string().contains("access_denied"));
        assert!(err.to_string().contains("user declined"));
    }

    #[test]
    fn classify_poll_response_expired_token_is_terminal() {
        let body = r#"{"error":"expired_token"}"#;
        let err = classify_poll_response(false, body).unwrap_err();
        assert!(err.to_string().contains("expired_token"));
    }

    #[test]
    fn classify_poll_response_malformed_success_body_is_an_error_not_a_panic() {
        let err = classify_poll_response(true, "not json").unwrap_err();
        assert!(!err.to_string().is_empty());
    }
}
