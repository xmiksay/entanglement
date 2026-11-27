//! `skutter inspect mcp-tokens` (#558).
//!
//! Surfaces which MCP servers hold a stored OAuth credential
//! (`mcp-tokens.yml`, [`crate::config::McpTokenStore`], ADR-0153) — otherwise
//! only visible one server at a time via `/mcp connect`/`/mcp` panel. Never
//! prints a token value: only the server name, token endpoint, whether a
//! refresh token is on file, and whether the access token looks expired —
//! exactly the "is this server actually authorized right now" question, with
//! every secret redacted.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use entanglement_core::{StoredAuth, TokenStore};

use crate::config::McpTokenStore;

/// Print every MCP server with a stored credential: token endpoint, refresh
/// availability, and access-token expiry — never the token values themselves.
pub fn inspect_mcp_tokens() -> Result<()> {
    let store = McpTokenStore::load();
    let mut servers = store.servers();
    servers.sort();
    print!("{}", render_mcp_tokens(&store, &servers));
    Ok(())
}

fn render_mcp_tokens(store: &dyn TokenStore, servers: &[String]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "MCP OAuth credentials (mcp-tokens.yml, ADR-0153):");
    if servers.is_empty() {
        let _ = writeln!(out, "  (none stored — run `/mcp connect <server>`)");
        return out;
    }
    for name in servers {
        match store.load(name) {
            Ok(Some(auth)) => {
                let _ = writeln!(out, "  {name}: {}", describe(&auth));
            }
            Ok(None) => {
                // Raced with a concurrent delete between `servers()` and
                // `load()` — report it rather than silently dropping the row.
                let _ = writeln!(out, "  {name}: (credential removed since listing)");
            }
            Err(e) => {
                let _ = writeln!(out, "  {name}: (could not read: {e:#})");
            }
        }
    }
    out
}

/// One credential's redacted summary: token endpoint, refresh-token presence,
/// and expiry — never `access_token`/`refresh_token`/`client_secret` values.
fn describe(auth: &StoredAuth) -> String {
    let refresh = if auth.tokens.refresh_token.is_some() {
        "refresh token on file"
    } else {
        "no refresh token"
    };
    let expiry = match auth.tokens.expires_at {
        Some(exp) => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if exp <= now {
                "access token expired".to_string()
            } else {
                format!("access token valid for {}s", exp - now)
            }
        }
        None => "access token has no expiry".to_string(),
    };
    format!("{} ({refresh}, {expiry})", auth.token_endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::TokenSet;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeStore(Mutex<BTreeMap<String, StoredAuth>>);

    impl TokenStore for FakeStore {
        fn load(&self, server: &str) -> Result<Option<StoredAuth>> {
            Ok(self.0.lock().unwrap().get(server).cloned())
        }
        fn save(&self, server: &str, auth: &StoredAuth) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert(server.to_string(), auth.clone());
            Ok(())
        }
        fn delete(&self, server: &str) -> Result<()> {
            self.0.lock().unwrap().remove(server);
            Ok(())
        }
    }

    fn auth(expires_at: Option<u64>, refresh: Option<&str>) -> StoredAuth {
        StoredAuth {
            client_id: "cid".into(),
            client_secret: Some("super-secret".into()),
            token_endpoint: "https://as.example/token".into(),
            revocation_endpoint: None,
            resource: None,
            tokens: TokenSet {
                access_token: "very-secret-access-token".into(),
                refresh_token: refresh.map(|s| s.to_string()),
                token_type: "Bearer".into(),
                expires_at,
                scope: None,
            },
        }
    }

    #[test]
    fn no_servers_is_reported_explicitly() {
        let rendered = render_mcp_tokens(&FakeStore::default(), &[]);
        assert!(rendered.contains("(none stored"));
    }

    #[test]
    fn never_prints_secret_values() {
        let store = FakeStore::default();
        store
            .save("chessbase", &auth(Some(9_999_999_999), Some("rt-value")))
            .unwrap();
        let rendered = render_mcp_tokens(&store, &["chessbase".to_string()]);
        assert!(rendered.contains("chessbase: https://as.example/token"));
        assert!(rendered.contains("refresh token on file"));
        assert!(rendered.contains("access token valid for"));
        assert!(!rendered.contains("very-secret-access-token"));
        assert!(!rendered.contains("rt-value"));
        assert!(!rendered.contains("super-secret"));
    }

    #[test]
    fn reports_an_expired_access_token() {
        let store = FakeStore::default();
        store.save("srv", &auth(Some(1), None)).unwrap();
        let rendered = render_mcp_tokens(&store, &["srv".to_string()]);
        assert!(rendered.contains("access token expired"));
        assert!(rendered.contains("no refresh token"));
    }
}
