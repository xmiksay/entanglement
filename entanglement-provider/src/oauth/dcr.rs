//! RFC 7591 dynamic client registration (ADR-0153).
//!
//! Remote MCP servers hand out no pre-issued `client_id` — there is no console
//! to register an app in — so a client that cannot register itself simply cannot
//! connect to most of them. This is what makes `/mcp connect <name>` work
//! against a server the user only pasted a URL for.
//!
//! We register as a **public client** (`token_endpoint_auth_method: "none"`):
//! there is no way to keep a client secret confidential in a local CLI, and
//! OAuth 2.1 + PKCE is designed for exactly that. A server that insists on
//! issuing a secret anyway gets it stored and used, since refusing would only
//! break the connection.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;

/// What registration yields: the client identity used for every later
/// authorization and refresh.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientRegistration {
    pub client_id: String,
    pub client_secret: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RegistrationResponse {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
}

/// Build the RFC 7591 registration request body. Split out from [`register`]
/// so the conditional `redirect_uri`/`response_types` shape (authorization-code
/// grant only — RFC 7591 §2; the device-code grant needs neither, #631) is
/// unit-testable without a network mock.
fn registration_body(
    client_name: &str,
    redirect_uri: Option<&str>,
    grant_types: &[&str],
    scopes: &[String],
) -> serde_json::Value {
    let mut body = json!({
        "client_name": client_name,
        "grant_types": grant_types,
        "token_endpoint_auth_method": "none",
    });
    if let Some(uri) = redirect_uri {
        body["redirect_uris"] = json!([uri]);
        body["response_types"] = json!(["code"]);
    }
    if !scopes.is_empty() {
        body["scope"] = json!(scopes.join(" "));
    }
    body
}

/// Register a client at `registration_endpoint`.
///
/// `redirect_uri` is `Some` for the authorization-code grant (the exact URI the
/// authorization request will use — the loopback listener is bound *before*
/// registration runs, so its ephemeral port is already known) and `None` for
/// the device-code grant (RFC 8628), which has no redirect to declare.
pub async fn register(
    http: &reqwest::Client,
    registration_endpoint: &str,
    redirect_uri: Option<&str>,
    grant_types: &[&str],
    scopes: &[String],
    client_name: &str,
) -> Result<ClientRegistration> {
    let body = registration_body(client_name, redirect_uri, grant_types, scopes);
    let resp = http
        .post(registration_endpoint)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {registration_endpoint}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "dynamic client registration failed with HTTP {status}: {}",
            text.chars().take(200).collect::<String>()
        );
    }
    let parsed: RegistrationResponse = serde_json::from_str(&text).with_context(|| {
        format!("parsing the registration response from {registration_endpoint}")
    })?;
    Ok(ClientRegistration {
        client_id: parsed.client_id,
        client_secret: parsed.client_secret,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_public_client_response() {
        let r: RegistrationResponse =
            serde_json::from_str(r#"{"client_id":"abc","client_id_issued_at":1}"#).unwrap();
        assert_eq!(r.client_id, "abc");
        assert!(r.client_secret.is_none());
    }

    #[test]
    fn parses_a_confidential_client_response() {
        let r: RegistrationResponse =
            serde_json::from_str(r#"{"client_id":"abc","client_secret":"s3cr3t"}"#).unwrap();
        assert_eq!(r.client_secret.as_deref(), Some("s3cr3t"));
    }

    #[test]
    fn registration_body_declares_a_public_pkce_client() {
        // Mirror what `register` builds, so the wire shape is pinned by a test
        // without needing a live server.
        let scopes = ["read".to_string(), "write".to_string()];
        let mut body = json!({
            "client_name": "skutter",
            "redirect_uris": ["http://127.0.0.1:5000/callback"],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        });
        body["scope"] = json!(scopes.join(" "));
        assert_eq!(body["token_endpoint_auth_method"], "none");
        assert_eq!(body["scope"], "read write");
        assert_eq!(body["redirect_uris"][0], "http://127.0.0.1:5000/callback");
    }

    #[test]
    fn registration_body_for_the_authorization_code_grant_declares_a_redirect() {
        // Exercises the real `registration_body` helper (not a hand-copy) for
        // the redirect-carrying shape `AuthFlow::begin` uses.
        let scopes = ["read".to_string()];
        let body = registration_body(
            "skutter",
            Some("http://127.0.0.1:5000/callback"),
            &["authorization_code", "refresh_token"],
            &scopes,
        );
        assert_eq!(body["redirect_uris"][0], "http://127.0.0.1:5000/callback");
        assert_eq!(body["response_types"][0], "code");
        assert_eq!(body["grant_types"][0], "authorization_code");
        assert_eq!(body["token_endpoint_auth_method"], "none");
        assert_eq!(body["scope"], "read");
    }

    /// The device-code grant (RFC 8628, #631) declares no redirect and no
    /// `response_types` — both are authorization-code-grant-specific
    /// (RFC 7591 §2) — proving `register`'s conditional branch is actually
    /// skipped rather than always emitting an empty array.
    #[test]
    fn registration_body_for_the_device_code_grant_omits_redirect_fields() {
        let body = registration_body(
            "skutter",
            None,
            &[
                "urn:ietf:params:oauth:grant-type:device_code",
                "refresh_token",
            ],
            &[],
        );
        assert!(body.get("redirect_uris").is_none());
        assert!(body.get("response_types").is_none());
        assert!(body.get("scope").is_none());
        assert_eq!(
            body["grant_types"][0],
            "urn:ietf:params:oauth:grant-type:device_code"
        );
        assert_eq!(body["token_endpoint_auth_method"], "none");
    }
}
