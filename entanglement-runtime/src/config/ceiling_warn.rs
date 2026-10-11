//! Startup warning when the project config layer overrides a `permissions`
//! ceiling key an earlier layer set.
//!
//! The project layer is trusted by design (ADR-0047) — a repository may
//! legitimately re-shape the ceiling for work inside it — but a hostile repo
//! can use the same precedence to silently *loosen* a restriction the user
//! configured (`bash: ask` → `allow`). The mitigation ADR-0047 prescribes is
//! inspection, not restriction: keep the semantics, make the override loud.

use serde_yaml::Value;

use super::{ConfigLayer, RawLayer};

/// `(key, earlier, project)` for every `permissions` key the project layer sets
/// to a value different from what the pre-project layers resolved to. A key the
/// earlier layers never set is not an override — the user expressed no opinion
/// to contradict. Pure, so tests assert on data instead of captured log output.
pub(super) fn project_permission_overrides(layers: &[RawLayer]) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for project in layers.iter().filter(|l| l.layer == ConfigLayer::Project) {
        let Some(perms) = permissions_of(&project.doc) else {
            continue;
        };
        for (key, value) in perms {
            let earlier = layers
                .iter()
                .filter(|l| l.layer != ConfigLayer::Project)
                .rev()
                .find_map(|l| permissions_of(&l.doc).and_then(|m| m.get(key)));
            if let Some(earlier) = earlier {
                if earlier != value {
                    out.push((display(key), display(earlier), display(value)));
                }
            }
        }
    }
    out
}

/// Log one warning per overridden ceiling key — only for a finding new since
/// this root's previous load ([`super::warn_once`]). Returns how many it
/// logged.
pub(super) fn warn_project_permission_overrides(layers: &[RawLayer]) -> usize {
    let overrides = project_permission_overrides(layers);
    let keys: Vec<String> = overrides
        .iter()
        .map(|(k, e, p)| format!("{k}\0{e}\0{p}"))
        .collect();
    let fresh = super::warn_once::fresh(super::warn_once::scope("ceiling", layers), &keys);
    let mut logged = 0;
    for ((key, earlier, project), _) in overrides.into_iter().zip(fresh).filter(|(_, f)| *f) {
        logged += 1;
        tracing::warn!(
            %key,
            %earlier,
            %project,
            "project config.yml overrides a permission ceiling key set by an \
             earlier layer (trusted per ADR-0047) — verify the change is intended"
        );
    }
    logged
}

fn permissions_of(doc: &Value) -> Option<&serde_yaml::Mapping> {
    doc.get("permissions")?.as_mapping()
}

fn display(v: &Value) -> String {
    serde_yaml::to_string(v)
        .map(|s| s.trim_end().to_string())
        .unwrap_or_else(|_| "<unprintable>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(layer: ConfigLayer, yaml: &str) -> RawLayer {
        RawLayer {
            layer,
            source: format!("test ({})", layer.label()),
            doc: serde_yaml::from_str(yaml).unwrap(),
        }
    }

    #[test]
    fn project_loosening_a_user_key_is_reported() {
        let layers = vec![
            layer(ConfigLayer::User, "permissions:\n  bash: ask\n"),
            layer(ConfigLayer::Project, "permissions:\n  bash: allow\n"),
        ];
        assert_eq!(
            project_permission_overrides(&layers),
            vec![("bash".to_string(), "ask".to_string(), "allow".to_string())]
        );
    }

    /// #785: the TUI's reload path re-runs the load; the warning doesn't repeat.
    #[test]
    fn a_repeated_load_warns_once_per_override() {
        let layers = vec![
            layer(ConfigLayer::User, "permissions:\n  dedupe_785: ask\n"),
            layer(ConfigLayer::Project, "permissions:\n  dedupe_785: allow\n"),
        ];
        assert_eq!(warn_project_permission_overrides(&layers), 1);
        assert_eq!(warn_project_permission_overrides(&layers), 0);
        // #813: fixed, then reintroduced — it warns again.
        let fixed = vec![
            layer(ConfigLayer::User, "permissions:\n  dedupe_785: ask\n"),
            layer(ConfigLayer::Project, "permissions:\n  dedupe_785: ask\n"),
        ];
        assert_eq!(warn_project_permission_overrides(&fixed), 0);
        assert_eq!(warn_project_permission_overrides(&layers), 1);
    }

    #[test]
    fn identical_value_is_not_an_override() {
        let layers = vec![
            layer(ConfigLayer::User, "permissions:\n  bash: ask\n"),
            layer(ConfigLayer::Project, "permissions:\n  bash: ask\n"),
        ];
        assert!(project_permission_overrides(&layers).is_empty());
    }

    #[test]
    fn key_the_user_never_set_is_not_an_override() {
        let layers = vec![
            layer(ConfigLayer::User, "verbose: true\n"),
            layer(ConfigLayer::Project, "permissions:\n  bash: allow\n"),
        ];
        assert!(project_permission_overrides(&layers).is_empty());
    }

    #[test]
    fn no_project_layer_reports_nothing() {
        let layers = vec![layer(ConfigLayer::User, "permissions:\n  bash: deny\n")];
        assert!(project_permission_overrides(&layers).is_empty());
    }

    #[test]
    fn argument_scoped_keys_compare_as_written() {
        let layers = vec![
            layer(ConfigLayer::User, "permissions:\n  \"bash(git *)\": ask\n"),
            layer(
                ConfigLayer::Project,
                "permissions:\n  \"bash(git *)\": allow\n",
            ),
        ];
        assert_eq!(
            project_permission_overrides(&layers),
            vec![(
                "bash(git *)".to_string(),
                "ask".to_string(),
                "allow".to_string()
            )]
        );
    }

    #[test]
    fn latest_earlier_layer_is_the_comparison_base() {
        // Default says allow, user tightens to deny, project flips back to
        // allow: the override is measured against the user's deny.
        let layers = vec![
            layer(ConfigLayer::Default, "permissions:\n  edit: allow\n"),
            layer(ConfigLayer::User, "permissions:\n  edit: deny\n"),
            layer(ConfigLayer::Project, "permissions:\n  edit: allow\n"),
        ];
        assert_eq!(
            project_permission_overrides(&layers),
            vec![("edit".to_string(), "deny".to_string(), "allow".to_string())]
        );
    }
}
