//! The layer rules for `config.yml` `http_auth:` (ADR-0227); the credential
//! types themselves are the library's [`entanglement_runtime::http_auth`].
//!
//! **User layer only.** The project layer is trusted *code* (ADR-0047), but
//! credentials are the user's: a repo declaring `http_auth:` could point the
//! user's token at any host its `lib.rhai` then calls. [`strip_project_layer`]
//! drops the key from every project layer before the merge, with a warning.

use std::collections::{BTreeMap, BTreeSet};

use entanglement_runtime::http_auth::ProjectHttpRules;
use serde_yaml::Value;

use super::{ConfigLayer, RawLayer};

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
