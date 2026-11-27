//! Hidden credentials the rhai `http()` binding injects at send time
//! (ADR-0227): a map of host → headers/cookies whose values may reference
//! `${VAR}`; the script and the model never see the resolved values. Plain
//! data the embedder hands the executor — `skutter` reads it from the user
//! layer of `config.yml` (`http_auth:`), never from a project layer.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

/// One host's credentials.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpAuthEntry {
    /// Request headers, e.g. `Authorization: "Bearer ${GITHUB_TOKEN}"`.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Cookies, joined into one `Cookie` header.
    #[serde(default)]
    pub cookies: BTreeMap<String, String>,
}

/// The `http_auth:` map, keyed by host (lowercased on lookup).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct HttpAuth(pub BTreeMap<String, HttpAuthEntry>);

impl HttpAuth {
    /// The entry for `host`: an exact key, else the longest key `host` ends
    /// with at a dot boundary (`github.com` covers `api.github.com`, never
    /// `notgithub.com` or `github.com.evil.io`).
    pub fn entry_for(&self, host: &str) -> Option<(&str, &HttpAuthEntry)> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.0
            .iter()
            .filter(|(key, _)| host_matches(&host, &key.to_ascii_lowercase()))
            .max_by_key(|(key, _)| key.len())
            .map(|(key, entry)| (key.as_str(), entry))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Every `${VAR}` any entry references — what no child process may
    /// inherit (`crate::secret_env`).
    pub fn referenced_vars(&self) -> Vec<String> {
        let mut vars = Vec::new();
        for entry in self.0.values() {
            for value in entry.headers.values().chain(entry.cookies.values()) {
                let mut rest = value.as_str();
                while let Some(start) = rest.find("${") {
                    let after = &rest[start + 2..];
                    let Some(end) = after.find('}') else { break };
                    vars.push(after[..end].to_string());
                    rest = &after[end + 1..];
                }
            }
        }
        vars.sort();
        vars.dedup();
        vars
    }

    /// One warning per key that can never match: matching is on the bare
    /// host, so a scheme, port, path or wildcard makes the entry dead.
    pub fn key_warnings(&self) -> Vec<String> {
        self.0
            .keys()
            .filter(|k| k.contains(['/', ':', '*', '?', '@']))
            .map(|k| {
                format!(
                    "http_auth: key `{k}` never matches — use the bare host (`github.com` also \
                     covers its subdomains), no scheme, port, path or wildcard"
                )
            })
            .collect()
    }
}

/// An entry's headers resolved for sending, plus every secret value they
/// carry — each expanded `${VAR}`, each cookie value and each whole header
/// value — so an error can be scrubbed of any of them, not just a full
/// header a server would never echo verbatim.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ResolvedAuth {
    pub headers: Vec<(String, String)>,
    pub secrets: Vec<String>,
}

fn host_matches(host: &str, key: &str) -> bool {
    let key = key.trim_end_matches('.');
    !key.is_empty()
        && (host == key
            || host
                .strip_suffix(key)
                .is_some_and(|prefix| prefix.ends_with('.')))
}

impl HttpAuthEntry {
    /// The headers to inject, `${VAR}` resolved through `lookup`, cookies
    /// folded into one `Cookie` header. An unresolvable var is an error
    /// naming the var (never its value).
    pub fn resolve(&self, lookup: impl Fn(&str) -> Option<String>) -> Result<ResolvedAuth, String> {
        let mut out = ResolvedAuth::default();
        for (name, value) in &self.headers {
            let expanded = expand(value, &lookup, &mut out.secrets)?;
            out.secrets.push(expanded.clone());
            out.headers.push((name.clone(), expanded));
        }
        if !self.cookies.is_empty() {
            let mut pairs = Vec::new();
            for (name, value) in &self.cookies {
                let expanded = expand(value, &lookup, &mut out.secrets)?;
                out.secrets.push(expanded.clone());
                pairs.push(format!("{name}={expanded}"));
            }
            let cookie = pairs.join("; ");
            out.secrets.push(cookie.clone());
            out.headers.push(("Cookie".to_string(), cookie));
        }
        out.secrets.retain(|s| !s.is_empty());
        Ok(out)
    }
}

/// Expand every `${VAR}` in `value`, recording each resolved value.
fn expand(
    value: &str,
    lookup: &impl Fn(&str) -> Option<String>,
    secrets: &mut Vec<String>,
) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| "unterminated `${` in an http_auth value".to_string())?;
        let var = &after[..end];
        let resolved =
            lookup(var).ok_or_else(|| format!("http_auth references unset variable `{var}`"))?;
        out.push_str(&resolved);
        secrets.push(resolved);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Per mode, the `http(...)` allow rules only a project layer declares
/// (`(rule, source)` while collecting; the source is dropped on return).
pub type ProjectHttpRules = BTreeMap<String, BTreeSet<(String, String)>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(keys: &[&str]) -> HttpAuth {
        HttpAuth(
            keys.iter()
                .map(|k| (k.to_string(), HttpAuthEntry::default()))
                .collect(),
        )
    }

    #[test]
    fn host_matches_exactly_or_at_a_dot_boundary_only() {
        let a = auth(&["github.com"]);
        assert!(a.entry_for("github.com").is_some());
        assert!(a.entry_for("api.github.com").is_some());
        assert!(a.entry_for("API.GitHub.com.").is_some());
        assert!(a.entry_for("notgithub.com").is_none());
        assert!(a.entry_for("github.com.evil.io").is_none());
        assert!(a.entry_for("com").is_none());
    }

    #[test]
    fn the_longest_matching_key_wins() {
        let a = auth(&["github.com", "api.github.com"]);
        assert_eq!(a.entry_for("api.github.com").unwrap().0, "api.github.com");
        assert_eq!(a.entry_for("raw.github.com").unwrap().0, "github.com");
    }

    #[test]
    fn resolve_expands_vars_and_folds_cookies() {
        let entry: HttpAuthEntry = serde_yaml::from_str(
            "headers: {Authorization: \"Bearer ${TOK}\"}\ncookies: {a: \"${A}\", b: x}",
        )
        .unwrap();
        let got = entry
            .resolve(|v| match v {
                "TOK" => Some("t0k".into()),
                "A" => Some("1".into()),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            got.headers,
            vec![
                ("Authorization".to_string(), "Bearer t0k".to_string()),
                ("Cookie".to_string(), "a=1; b=x".to_string()),
            ]
        );
        // The bare token and each cookie value are scrub targets, not only
        // the whole header.
        for secret in ["t0k", "Bearer t0k", "1", "x", "a=1; b=x"] {
            assert!(
                got.secrets.contains(&secret.to_string()),
                "{secret}: {:?}",
                got.secrets
            );
        }
    }

    #[test]
    fn an_unterminated_var_is_an_error_not_a_literal() {
        let entry: HttpAuthEntry = serde_yaml::from_str("headers: {X: \"${OPEN\"}").unwrap();
        let err = entry.resolve(|_| Some("v".into())).unwrap_err();
        assert!(err.contains("unterminated"), "{err}");
    }

    #[test]
    fn referenced_vars_and_dead_keys_are_reported() {
        let a: HttpAuth = serde_yaml::from_str(
            "github.com: {headers: {A: \"${GH} ${GH2}\"}, cookies: {s: \"${SESS}\"}}\n\
             \"https://x.io\": {}\n\"x.io:443\": {}\n\"*.x.io\": {}",
        )
        .unwrap();
        assert_eq!(a.referenced_vars(), vec!["GH", "GH2", "SESS"]);
        let warnings = a.key_warnings();
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings.iter().all(|w| w.contains("never matches")));
    }

    #[test]
    fn an_unset_var_names_the_var_not_a_value() {
        let entry: HttpAuthEntry =
            serde_yaml::from_str("headers: {X-Key: \"${MISSING}\"}").unwrap();
        let err = entry.resolve(|_| None).unwrap_err();
        assert!(err.contains("MISSING"), "{err}");
    }
}
