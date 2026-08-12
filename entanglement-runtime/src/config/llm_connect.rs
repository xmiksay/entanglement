//! `skutter config connect|disconnect <provider>` — mint / drop the OAuth
//! credential for an OAuth-protected **LLM provider endpoint** (#684 edge d).
//!
//! The LLM twin of `/mcp connect` (`mcp/oauth_ops.rs`), as a pre-engine CLI
//! fast path instead of an engine message: a catalog entry carrying an
//! `oauth:` block authenticates with a bearer from the managed LLM token file
//! (`llm-tokens.yml`, [`McpTokenStore::load_llm`]) instead of a static
//! `key_env` key, and this is the single-user surface that fills that file.
//! Discovery/DCR/PKCE/refresh are the same OAuth 2.1 stack MCP servers use
//! (ADR-0153/0182) — the `oauth:` block's overrides short-circuit discovery
//! for an endpoint that publishes no RFC 9728/8414 metadata.

use anyhow::{bail, Context, Result};
use entanglement_provider::{Catalog, ProviderEntry};

use super::McpTokenStore;

/// How long `connect` waits for the user to finish authorizing in their
/// browser — mirrors `mcp/oauth_ops.rs`.
const AUTHORIZATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Resolve the endpoint base the OAuth discovery runs against — the same
/// `{NAME}_API_BASE`/`{NAME}_BASE` > `entry.base_url` precedence the wire
/// factories use, so `connect` discovers against the URL requests will
/// actually hit.
fn discovery_base(entry: &ProviderEntry) -> Result<String> {
    let name = entry.name.to_uppercase();
    let env_base = [format!("{name}_API_BASE"), format!("{name}_BASE")]
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|v| !v.trim().is_empty()));
    env_base
        .or_else(|| entry.base_url.clone())
        .with_context(|| {
            format!(
                "provider `{}` has no base URL to discover OAuth metadata against — \
             set `base_url` in providers.yml (or {name}_API_BASE)",
                entry.name
            )
        })
}

/// The catalog entry, refusing a provider that isn't OAuth-protected.
fn oauth_entry<'a>(catalog: &'a Catalog, provider: &str) -> Result<&'a ProviderEntry> {
    let entry = catalog
        .provider(provider)
        .with_context(|| format!("unknown provider `{provider}`"))?;
    if entry.oauth.is_none() {
        bail!(
            "provider `{provider}` is not OAuth-protected (no `oauth:` block in the \
             catalog) — use `skutter config set-key {provider}` for a static API key"
        );
    }
    Ok(entry)
}

/// Run the authorization flow for `provider` and persist the credential to
/// the managed LLM token file. `device_code` selects RFC 8628 (no browser,
/// no loopback listener) for a headless host.
pub async fn connect(catalog: &Catalog, provider: &str, device_code: bool) -> Result<()> {
    let entry = oauth_entry(catalog, provider)?;
    let oauth = entry.oauth.clone().unwrap_or_default();
    let base = discovery_base(entry)?;

    let auth = if device_code {
        let pending = entanglement_core::DeviceFlow::begin(provider, &base, &oauth, None)
            .await
            .with_context(|| format!("starting the device-code flow for `{provider}`"))?;
        match pending.verification_uri_complete() {
            Some(complete) => println!("Visit: {complete}"),
            None => println!(
                "Visit: {}\nEnter code: {}",
                pending.verification_uri(),
                pending.user_code()
            ),
        }
        pending
            .poll()
            .await
            .with_context(|| format!("authorizing `{provider}` via device code"))?
    } else {
        let pending = entanglement_core::AuthFlow::begin(provider, &base, &oauth, None)
            .await
            .with_context(|| format!("starting the authorization flow for `{provider}`"))?;
        let url = pending.authorize_url().to_string();
        if crate::mcp::browser::open(&url) {
            println!("Opened your browser to authorize `{provider}`. If it didn't open, visit:");
        } else {
            println!("Visit this URL to authorize `{provider}`:");
        }
        println!("{url}");
        pending
            .complete(AUTHORIZATION_TIMEOUT)
            .await
            .with_context(|| format!("authorizing `{provider}`"))?
    };

    McpTokenStore::load_llm()
        .save(provider, &auth)
        .with_context(|| format!("persisting the credential for `{provider}`"))?;
    println!("Authorized `{provider}` — select it with ENTANGLEMENT_PROVIDER={provider}.");
    Ok(())
}

/// Drop `provider`'s stored credential (revoking upstream when the server
/// advertised RFC 7009 revocation), mirroring `/mcp disconnect`.
pub async fn disconnect(catalog: &Catalog, provider: &str) -> Result<()> {
    // Validate the name against the catalog for a typo-proof error, but drop
    // the credential even if the entry has since lost its `oauth:` block —
    // a stored token with no config left pointing at it is exactly the state
    // `disconnect` exists to clean up.
    if catalog.provider(provider).is_none() {
        bail!("unknown provider `{provider}`");
    }
    let store = McpTokenStore::load_llm();
    let outcome = entanglement_core::mcp_auth_disconnect(&store, provider)
        .await
        .with_context(|| format!("disconnecting `{provider}`"))?;
    println!("`{provider}`: {}", outcome.as_str());
    Ok(())
}

// `TokenStore::save` is a trait method; bring it into scope for the calls above.
use entanglement_core::TokenStore as _;
