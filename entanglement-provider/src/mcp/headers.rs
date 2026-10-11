//! Static per-server request headers for the MCP HTTP transport (#312).
//!
//! Split out of `http.rs` along the 400-line file cap when the transport moved
//! into this crate (ADR-0153). Header *values* may reference `${VAR}`, expanded
//! from the process environment so a token never has to be written into a config
//! file in the clear — the pre-OAuth way of authenticating a remote server, and
//! still the right answer for a server issuing long-lived static tokens.

use std::collections::HashMap;

use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

/// The challenge header a `401` carries, pointing at the protected-resource
/// metadata document (RFC 9728) that bootstraps OAuth discovery.
const WWW_AUTHENTICATE: &str = "WWW-Authenticate";

/// Parse the config header map into a `HeaderMap`, expanding `${VAR}` references
/// in each value from the environment. An invalid header name/value is a hard
/// error so a misconfigured auth header fails loudly at connect time.
pub fn build_headers(headers: &HashMap<String, String>) -> Result<HeaderMap> {
    let mut out = HeaderMap::new();
    for (name, raw) in headers {
        let value = expand_env(raw);
        let name = HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("invalid header name `{name}`"))?;
        let value = HeaderValue::from_str(&value)
            .with_context(|| format!("invalid value for header `{name}`"))?;
        out.insert(name, value);
    }
    Ok(out)
}

/// Expand `${VAR}` references from the process environment. An unset variable
/// expands to an empty string (with a warning) so a missing token yields an
/// obviously-broken auth header rather than a literal `${VAR}` on the wire.
pub fn expand_env(input: &str) -> String {
    expand_env_with(input, |var| std::env::var(var).ok())
}

/// [`expand_env`] over an injected env lookup, so the expansion is
/// unit-testable without mutating process env (#773).
fn expand_env_with(input: &str, env: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // Unterminated `${` — emit the remainder verbatim.
            out.push_str(&rest[start..]);
            return out;
        };
        let var = &after[..end];
        match env(var) {
            Some(v) => out.push_str(&v),
            None => tracing::warn!("MCP header references unset env var `{var}`"),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Extract the `resource_metadata="…"` parameter from a `WWW-Authenticate`
/// challenge (RFC 9728 §5.1). Returns `None` when the header is absent or
/// carries no such parameter — discovery then falls back to the well-known path
/// derived from the server URL.
pub fn resource_metadata_url(headers: &HeaderMap) -> Option<String> {
    let challenge = headers.get(WWW_AUTHENTICATE)?.to_str().ok()?;
    let start = challenge.find("resource_metadata=")? + "resource_metadata=".len();
    let rest = &challenge[start..];
    let value = if let Some(unquoted) = rest.strip_prefix('"') {
        unquoted.split('"').next()?
    } else {
        // Unquoted parameters run to the next comma or whitespace.
        rest.split([',', ' ']).next()?
    };
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_present_env_var() {
        let env = |v: &str| (v == "TOKEN").then(|| "secret".to_string());
        assert_eq!(expand_env_with("Bearer ${TOKEN}", env), "Bearer secret");
    }

    #[test]
    fn unset_env_var_expands_empty() {
        assert_eq!(expand_env_with("Bearer ${MISSING}", |_| None), "Bearer ");
    }

    #[test]
    fn literal_without_vars_is_unchanged() {
        assert_eq!(expand_env("Bearer static-token"), "Bearer static-token");
    }

    #[test]
    fn unterminated_brace_is_verbatim() {
        assert_eq!(expand_env("a${b"), "a${b");
    }

    #[test]
    fn builds_auth_header() {
        let mut h = HashMap::new();
        h.insert("Authorization".to_string(), "Bearer abc".to_string());
        let map = build_headers(&h).unwrap();
        assert_eq!(map.get("authorization").unwrap(), "Bearer abc");
    }

    #[test]
    fn rejects_invalid_header_name() {
        let mut h = HashMap::new();
        h.insert("bad header".to_string(), "x".to_string());
        assert!(build_headers(&h).is_err());
    }

    fn challenge(v: &'static str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(WWW_AUTHENTICATE, HeaderValue::from_static(v));
        h
    }

    #[test]
    fn extracts_quoted_resource_metadata() {
        let h = challenge(
            r#"Bearer error="invalid_token", resource_metadata="https://ex.com/.well-known/oauth-protected-resource""#,
        );
        assert_eq!(
            resource_metadata_url(&h).unwrap(),
            "https://ex.com/.well-known/oauth-protected-resource"
        );
    }

    #[test]
    fn extracts_unquoted_resource_metadata() {
        let h = challenge("Bearer resource_metadata=https://ex.com/meta, error=x");
        assert_eq!(resource_metadata_url(&h).unwrap(), "https://ex.com/meta");
    }

    #[test]
    fn absent_or_irrelevant_challenge_yields_none() {
        assert!(resource_metadata_url(&HeaderMap::new()).is_none());
        assert!(resource_metadata_url(&challenge("Bearer error=\"invalid_token\"")).is_none());
    }
}
