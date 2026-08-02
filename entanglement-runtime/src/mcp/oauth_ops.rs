//! Orchestration for the three MCP OAuth operations (ADR-0153).
//!
//! The *policy* half: which server, which config, which token store, when to
//! open a browser, and what to reconnect afterwards. Every protocol detail —
//! discovery, PKCE, dynamic client registration, token exchange/refresh/
//! revocation — lives in `entanglement-provider::mcp::auth` and is reached
//! through core's re-export.
//!
//! Called only from the MCP responder, which answers the trusted-only
//! `InMsg::McpAuth` (a connect opens a browser and mints a durable credential,
//! so it must never be wire-driven — #472/ADR-0124's rationale).

use anyhow::{bail, Context, Result};
use entanglement_core::{AuthFlow, AuthOutcome, McpAuthAction, TokenStore};

use crate::config::McpTokenStore;
use crate::tools::SharedRegistry;

use super::live::{ActiveServers, ServerConfigs};
use super::McpServerConfig;

/// How long the user gets to complete browser consent before the flow gives up
/// and releases the loopback listener.
const AUTHORIZATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// The interim report a `Connect` makes once the authorization URL is known,
/// so the head can print it (and the browser can be launched) while the flow is
/// still parked on the redirect.
pub struct PendingUrl {
    pub url: String,
    pub browser_opened: bool,
}

/// Run `action` for `server`. `on_url` is invoked once, mid-`Connect`, with the
/// authorization URL — the responder uses it to emit the interim
/// `McpAuthChanged` before the flow blocks on the browser. `http` (#559) is
/// the shared endpoint pool the post-authorization reconnect rides.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    server: &str,
    action: McpAuthAction,
    configs: &ServerConfigs,
    registry: &SharedRegistry,
    active: &ActiveServers,
    secret_env: &[String],
    http: &entanglement_core::HttpClient,
    on_url: impl FnOnce(PendingUrl),
) -> Result<AuthOutcome> {
    let store = McpTokenStore::load();
    match action {
        McpAuthAction::Connect => {
            connect(server, configs, registry, active, secret_env, http, on_url).await
        }
        // Check and Disconnect are pure credential operations — they need the
        // store but neither the config nor a reconnect, so they delegate
        // straight to the provider's mechanism.
        McpAuthAction::Check => entanglement_core::mcp_auth_check(&store, server).await,
        McpAuthAction::Disconnect => {
            let outcome = entanglement_core::mcp_auth_disconnect(&store, server).await?;
            // Drop the live connection too: its token source would now fail on
            // every request, so leaving the tools registered is worse than
            // removing them.
            if outcome == AuthOutcome::Disconnected {
                let _ = super::live::mcp_disconnect_only(server, registry, active);
            }
            Ok(outcome)
        }
    }
}

/// The full authorization-code flow, then a reconnect so the server's tools
/// become usable without a restart.
#[allow(clippy::too_many_arguments)]
async fn connect(
    server: &str,
    configs: &ServerConfigs,
    registry: &SharedRegistry,
    active: &ActiveServers,
    secret_env: &[String],
    http: &entanglement_core::HttpClient,
    on_url: impl FnOnce(PendingUrl),
) -> Result<AuthOutcome> {
    let cfg = resolve_config(server, configs)?;
    let Some(oauth) = cfg.oauth.clone() else {
        bail!(
            "MCP server `{server}` has no `oauth:` block — add one to its `mcp:` entry in \
             config.yml (an empty block is enough; endpoints are discovered via RFC 9728/8414). \
             If `{server}` issues a static bearer token instead (no discovery endpoints to find), \
             `/mcp connect` can never complete — adding `oauth: {{}}` only moves the failure. Use \
             `headers: {{Authorization: \"Bearer ${{VAR}}\"}}` on its `mcp:` entry with the token \
             in the managed `.env` instead"
        );
    };
    let Some(url) = cfg.url.clone() else {
        bail!("MCP server `{server}` uses the stdio transport, which has no OAuth flow");
    };

    let pending = AuthFlow::begin(server, &url, &oauth, None)
        .await
        .with_context(|| format!("starting authorization for MCP server `{server}`"))?;

    // Report the URL before parking on the redirect: a headless session needs
    // it, and even with a browser the user may want to open it elsewhere.
    let authorize_url = pending.authorize_url().to_string();
    let browser_opened = super::browser::open(&authorize_url);
    on_url(PendingUrl {
        url: authorize_url,
        browser_opened,
    });

    let auth = pending.complete(AUTHORIZATION_TIMEOUT).await?;
    let store = McpTokenStore::load();
    store
        .save(server, &auth)
        .with_context(|| format!("persisting the credential for MCP server `{server}`"))?;

    // Now that a credential exists, connect for real. A failure here leaves the
    // credential stored (it is valid — the server is simply unreachable), so a
    // later retry needs no re-authorization.
    if let Err(e) =
        super::live::mcp_reconnect(server, &cfg, registry, active, secret_env, http).await
    {
        tracing::warn!(
            server = %server,
            "authorized, but connecting afterwards failed: {e:#} — the credential is stored; \
             retry with `/mcp connect {server}` or restart"
        );
    }
    Ok(AuthOutcome::Authorized)
}

/// The effective config for `server`: a user `mcp:` entry, or a bundled/
/// available definition (#542) for a server that has no config-map entry.
fn resolve_config(server: &str, configs: &ServerConfigs) -> Result<McpServerConfig> {
    if let Some(cfg) = configs.lock().unwrap().get(server) {
        return Ok(cfg.clone());
    }
    bail!("unknown MCP server `{server}` — check `/mcp list` for the configured names")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    fn configs_with(entries: Vec<(&str, McpServerConfig)>) -> ServerConfigs {
        Arc::new(Mutex::new(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect::<HashMap<_, _>>(),
        ))
    }

    fn http_server(url: &str, oauth: Option<entanglement_core::OauthConfig>) -> McpServerConfig {
        McpServerConfig {
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some(url.to_string()),
            headers: HashMap::new(),
            capabilities: HashMap::new(),
            oauth,
            disabled: false,
            state: None,
        }
    }

    #[test]
    fn resolve_config_finds_a_configured_server() {
        let configs = configs_with(vec![("srv", http_server("https://ex/mcp", None))]);
        assert_eq!(
            resolve_config("srv", &configs).unwrap().url.as_deref(),
            Some("https://ex/mcp")
        );
    }

    #[test]
    fn resolve_config_rejects_an_unknown_server_by_name() {
        let configs = configs_with(vec![]);
        let err = resolve_config("ghost", &configs).unwrap_err();
        assert!(err.to_string().contains("ghost"));
        assert!(err.to_string().contains("/mcp list"));
    }

    #[tokio::test]
    async fn connect_refuses_a_server_with_no_oauth_block() {
        let configs = configs_with(vec![("srv", http_server("https://ex/mcp", None))]);
        let registry = SharedRegistry::default();
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let http = entanglement_core::HttpClient::new().unwrap();
        let err = connect("srv", &configs, &registry, &active, &[], &http, |_| {})
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no `oauth:` block"));
        // #561: a bearer-only server has no discovery endpoints to find, so the
        // error must point at the actual fix (static `headers:`), not just name
        // the missing block.
        assert!(err.to_string().contains("headers:"));
        assert!(err.to_string().contains("static bearer token"));
    }

    #[tokio::test]
    async fn connect_refuses_a_stdio_server() {
        let mut cfg = http_server("https://ex/mcp", Some(Default::default()));
        cfg.url = None;
        cfg.command = Some("some-server".into());
        let configs = configs_with(vec![("srv", cfg)]);
        let registry = SharedRegistry::default();
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let http = entanglement_core::HttpClient::new().unwrap();
        let err = connect("srv", &configs, &registry, &active, &[], &http, |_| {})
            .await
            .unwrap_err();
        assert!(err.to_string().contains("stdio transport"));
    }
}
