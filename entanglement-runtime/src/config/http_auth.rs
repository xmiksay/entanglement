//! `config.yml` `http_auth:` (ADR-0227): hidden credentials the rhai
//! `http()` binding injects at send time. A map of host → headers/cookies
//! whose values may reference `${VAR}`; the script and the model never see
//! the resolved values.
//!
//! **User layer only.** The project layer is trusted *code* (ADR-0047), but
//! credentials are the user's: a repo declaring `http_auth:` could point the
//! user's token at any host its `lib.rhai` then calls. [`strip_project_layer`]
//! drops the key from every project layer before the merge, with a warning.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_yaml::Value;

use super::{ConfigLayer, RawLayer};

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

/// The `http(...)` allow rules a project layer's `modes:` adds. They still
/// grade (the project layer is trusted code), but never count as the
/// explicit rule that lets an unattended mode make a credentialed call — a
/// repo must not be able to unlock silent use of the user's token. One
/// warning each.
pub(super) fn project_http_rules(layers: &[RawLayer]) -> (ProjectHttpRules, Vec<String>) {
    let rules_of = |layer: ConfigLayer| {
        let mut out: ProjectHttpRules = BTreeMap::new();
        for l in layers.iter().filter(|l| l.layer == layer) {
            let Some(modes) = l.doc.get("modes").and_then(Value::as_mapping) else {
                continue;
            };
            for (mode, tuning) in modes {
                let (Some(mode), Some(allows)) = (
                    mode.as_str(),
                    tuning.get("allow").and_then(Value::as_sequence),
                ) else {
                    continue;
                };
                for rule in allows.iter().filter_map(Value::as_str) {
                    if rule.starts_with("http(") || rule.starts_with("http{") {
                        out.entry(mode.to_string())
                            .or_default()
                            .insert((rule.to_string(), l.source.clone()));
                    }
                }
            }
        }
        out
    };
    let user = rules_of(ConfigLayer::User);
    let mut project_only: ProjectHttpRules = BTreeMap::new();
    let mut warnings = Vec::new();
    for (mode, rules) in rules_of(ConfigLayer::Project) {
        let user_keys: BTreeSet<&str> = user
            .get(&mode)
            .into_iter()
            .flatten()
            .map(|(k, _)| k.as_str())
            .collect();
        for (rule, source) in rules {
            // The same rule in the user config is the user's own decision.
            if user_keys.contains(rule.as_str()) {
                continue;
            }
            warnings.push(format!(
                "{source}: `{mode}` allow rule `{rule}` grades `http` calls but never unlocks a \
                 credentialed call in an unattended mode — add the same rule to your user \
                 config for that (ADR-0227)"
            ));
            project_only
                .entry(mode.clone())
                .or_default()
                .insert((rule, String::new()));
        }
    }
    (project_only, warnings)
}

/// Per mode, the `http(...)` allow rules only a project layer declares
/// (`(rule, source)` while collecting; the source is dropped on return).
pub type ProjectHttpRules = BTreeMap<String, BTreeSet<(String, String)>>;

/// Remove `http_auth:` from every project layer, one warning each.
pub(super) fn strip_project_layer(layers: &mut [RawLayer]) -> Vec<String> {
    let mut out = Vec::new();
    for layer in layers
        .iter_mut()
        .filter(|l| l.layer == ConfigLayer::Project)
    {
        if let Value::Mapping(map) = &mut layer.doc {
            if map.remove("http_auth").is_some() {
                out.push(format!(
                    "{}: `http_auth:` is honored only in the user config (ADR-0227) and was \
                     ignored — a repository must not route your credentials",
                    layer.source
                ));
            }
        }
    }
    out
}

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

    #[test]
    fn project_mode_http_rules_are_collected_with_a_warning() {
        let doc = |s: &str| serde_yaml::from_str::<Value>(s).unwrap();
        let layer = |layer, s: &str| RawLayer {
            layer,
            source: format!("{layer:?}.yml"),
            doc: doc(s),
        };
        let project = layer(
            ConfigLayer::Project,
            "modes:\n  auto:\n    allow: [\"http(GET https://a.io/*)\", \"http(GET https://b.io/*)\", \"bash(ls)\"]",
        );
        // The user config declares the a.io rule too — in `auto` only.
        let user = layer(
            ConfigLayer::User,
            "modes:\n  auto:\n    allow: [\"http(GET https://a.io/*)\"]\n  build:\n    allow: [\"http(GET https://b.io/*)\"]",
        );
        let (rules, warnings) = project_http_rules(&[user, project]);
        let auto: Vec<&str> = rules["auto"].iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            auto,
            vec!["http(GET https://b.io/*)"],
            "a user rule keeps its unlock"
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("add the same rule to your user config"));
    }

    #[test]
    fn only_the_project_layer_loses_http_auth() {
        let doc = |s: &str| serde_yaml::from_str::<Value>(s).unwrap();
        let mut layers = vec![
            RawLayer {
                layer: ConfigLayer::User,
                source: "user.yml".into(),
                doc: doc("http_auth: {a.com: {}}"),
            },
            RawLayer {
                layer: ConfigLayer::Project,
                source: "repo.yml".into(),
                doc: doc("http_auth: {b.com: {}}"),
            },
        ];
        let warnings = strip_project_layer(&mut layers);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("repo.yml"), "{}", warnings[0]);
        assert!(layers[0].doc.get("http_auth").is_some());
        assert!(layers[1].doc.get("http_auth").is_none());
    }
}
