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
        match std::env::var(var) {
            Ok(v) => out.push_str(&v),
            Err(_) => tracing::warn!("MCP header references unset env var `{var}`"),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_present_env_var() {
        std::env::set_var("MCP_TEST_TOKEN_XYZ", "secret");
        assert_eq!(expand_env("Bearer ${MCP_TEST_TOKEN_XYZ}"), "Bearer secret");
        std::env::remove_var("MCP_TEST_TOKEN_XYZ");
    }

    #[test]
    fn unset_env_var_expands_empty() {
        std::env::remove_var("MCP_TEST_MISSING_XYZ");
        assert_eq!(expand_env("Bearer ${MCP_TEST_MISSING_XYZ}"), "Bearer ");
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
}
