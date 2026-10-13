//! `config.yml` `rhai: { trusted: [<dir>, …] }` (ADR-0228): folders whose
//! scripts `rhai {"path": …}` runs with no launch card and every nested call
//! force-allowed (explicit denies still win).
//!
//! Unlike every other key, the lists of all layers **add up** rather than the
//! later layer replacing the earlier, and each entry resolves against its own
//! layer: a project entry relative to the project root, a user entry with
//! `~` expanded and relative to the user config file's folder. Both layers
//! are honored — the project layer is trusted code (ADR-0047), and trusting
//! its own `scripts/` folder is the point; ADR-0228 records that this lets a
//! repository unlock credentialed `http()` for its trusted scripts. Entries
//! are canonicalized here, so a symlinked folder is trusted as its target
//! and containment is checked against real paths (ADR-0054).

use std::path::{Path, PathBuf};

use serde_yaml::Value;

use super::{ConfigLayer, RawLayer};

/// Take `rhai:` out of every layer (before the merge, which would let the
/// project list replace the user's) and resolve the union of the trusted
/// folders. Returns the canonical folders, each with the layer that listed
/// it first, and one warning per entry that was ignored.
pub(super) fn extract(
    layers: &mut [RawLayer],
    root: Option<&Path>,
) -> (Vec<(PathBuf, ConfigLayer)>, Vec<String>) {
    let mut trusted: Vec<(PathBuf, ConfigLayer)> = Vec::new();
    let mut warnings = Vec::new();
    for layer in layers.iter_mut() {
        let Value::Mapping(map) = &mut layer.doc else {
            continue;
        };
        let Some(block) = map.remove("rhai") else {
            continue;
        };
        let warn = |what: String| format!("{}: `rhai.{what}", layer.source);
        let Value::Mapping(block) = block else {
            warnings.push(warn(
                "` must be a map with `trusted: [<dir>, …]` — ignored".into(),
            ));
            continue;
        };
        for (key, value) in block {
            match key.as_str() {
                Some("trusted") => {}
                _ => {
                    warnings.push(warn(format!(
                        "{}` is not a known key — ignored",
                        yaml(&key)
                    )));
                    continue;
                }
            }
            let Value::Sequence(entries) = value else {
                warnings.push(warn("trusted` must be a list of folders — ignored".into()));
                continue;
            };
            for entry in entries {
                let Some(dir) = entry.as_str() else {
                    warnings.push(warn(format!(
                        "trusted` entry {} is not a path — ignored",
                        yaml(&entry)
                    )));
                    continue;
                };
                match resolve(layer.layer, &layer.source, root, dir) {
                    // A folder both layers list is the project's: its trust
                    // then never depends on the user-layer suppression.
                    Ok(path) => match trusted.iter_mut().find(|(p, _)| *p == path) {
                        Some(seen) if layer.layer == ConfigLayer::Project => {
                            seen.1 = ConfigLayer::Project
                        }
                        Some(_) => {}
                        None => trusted.push((path, layer.layer)),
                    },
                    Err(why) => {
                        warnings.push(warn(format!("trusted` entry {dir:?} {why} — ignored")))
                    }
                }
            }
        }
    }
    (trusted, warnings)
}

/// The folders by layer plus the modes whose project config adds `prompt`
/// rules — where the user layer's trust is dropped (ADR-0228).
pub(super) fn folders(
    trusted: &[(PathBuf, ConfigLayer)],
    layers: &[RawLayer],
) -> crate::rhai_trust::TrustedFolders {
    let of = |want: ConfigLayer| {
        trusted
            .iter()
            .filter(|(_, l)| *l == want)
            .map(|(p, _)| p.clone())
            .collect()
    };
    crate::rhai_trust::TrustedFolders {
        project: of(ConfigLayer::Project),
        user: of(ConfigLayer::User),
        user_suppressed: project_prompt_modes(layers),
        user_suppressed_everywhere: project_ceiling_prompts(layers),
    }
}

/// Whether a project layer's `permissions:` ceiling asks for prompts — a
/// non-empty `prompt` list or `default: prompt` — which holds in every mode.
fn project_ceiling_prompts(layers: &[RawLayer]) -> bool {
    layers
        .iter()
        .filter(|l| l.layer == ConfigLayer::Project)
        .filter_map(|l| l.doc.get("permissions"))
        .any(|p| {
            let rules = p
                .get("prompt")
                .and_then(Value::as_sequence)
                .is_some_and(|rules| !rules.is_empty());
            rules || p.get("default").and_then(Value::as_str) == Some("prompt")
        })
}

/// Every mode a project layer's `modes:` gives a non-empty `prompt` list.
fn project_prompt_modes(layers: &[RawLayer]) -> std::collections::BTreeSet<String> {
    layers
        .iter()
        .filter(|l| l.layer == ConfigLayer::Project)
        .filter_map(|l| l.doc.get("modes").and_then(Value::as_mapping))
        .flatten()
        .filter(|(_, tuning)| {
            tuning
                .get("prompt")
                .and_then(Value::as_sequence)
                .is_some_and(|rules| !rules.is_empty())
        })
        .filter_map(|(mode, _)| mode.as_str().map(String::from))
        .collect()
}

fn yaml(value: &Value) -> String {
    serde_yaml::to_string(value)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// One entry's canonical folder, per its layer's base.
fn resolve(
    layer: ConfigLayer,
    source: &str,
    root: Option<&Path>,
    dir: &str,
) -> Result<PathBuf, String> {
    let path = match layer {
        ConfigLayer::User => {
            let expanded = match dir.strip_prefix('~') {
                Some(rest) if rest.is_empty() || rest.starts_with('/') => dirs::home_dir()
                    .ok_or("needs a home directory")?
                    .join(rest.trim_start_matches('/')),
                _ => PathBuf::from(dir),
            };
            let base = Path::new(source).parent().unwrap_or(Path::new(""));
            base.join(expanded)
        }
        ConfigLayer::Project => root.ok_or("has no project root")?.join(dir),
        ConfigLayer::Default => return Err("is not allowed in the embedded defaults".into()),
    };
    let canon = path
        .canonicalize()
        .map_err(|e| format!("does not resolve ({}: {e})", path.display()))?;
    if !canon.is_dir() {
        return Err(format!("is not a folder ({})", canon.display()));
    }
    Ok(canon)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(layer: ConfigLayer, source: &Path, yaml: &str) -> RawLayer {
        RawLayer {
            layer,
            source: source.display().to_string(),
            doc: serde_yaml::from_str(yaml).unwrap(),
        }
    }

    #[test]
    fn user_and_project_lists_add_up_each_against_its_own_base() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let (root, user_dir) = (base.join("proj"), base.join("cfg"));
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::create_dir_all(user_dir.join("mine")).unwrap();
        let mut layers = vec![
            layer(
                ConfigLayer::User,
                &user_dir.join("config.yml"),
                "rhai:\n  trusted: [mine]\nverbose: true\n",
            ),
            layer(
                ConfigLayer::Project,
                &root.join(".entanglement/config.yml"),
                "rhai:\n  trusted: [scripts, missing]\n",
            ),
        ];
        let (trusted, warnings) = extract(&mut layers, Some(&root));
        assert_eq!(
            trusted,
            vec![
                (user_dir.join("mine"), ConfigLayer::User),
                (root.join("scripts"), ConfigLayer::Project)
            ]
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("\"missing\" does not resolve"),
            "{warnings:?}"
        );
        assert!(
            layers.iter().all(|l| l.doc.get("rhai").is_none()),
            "stripped"
        );
        assert_eq!(layers[0].doc.get("verbose"), Some(&Value::Bool(true)));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_folder_is_trusted_as_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(base.join("real")).unwrap();
        std::os::unix::fs::symlink(base.join("real"), base.join("link")).unwrap();
        let mut layers = vec![layer(
            ConfigLayer::Project,
            &base.join("config.yml"),
            "rhai:\n  trusted: [link]\n",
        )];
        let (trusted, _) = extract(&mut layers, Some(&base));
        assert_eq!(trusted, vec![(base.join("real"), ConfigLayer::Project)]);
    }

    #[test]
    fn malformed_blocks_warn_and_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("config.yml");
        for (doc, needle) in [
            ("rhai: yes\n", "must be a map"),
            ("rhai:\n  trusted: scripts\n", "must be a list"),
            ("rhai:\n  trusted: [1]\n", "is not a path"),
            ("rhai:\n  trustd: [x]\n", "not a known key"),
        ] {
            let mut layers = vec![layer(ConfigLayer::Project, &src, doc)];
            let (trusted, warnings) = extract(&mut layers, Some(tmp.path()));
            assert!(trusted.is_empty());
            assert!(
                warnings.iter().any(|w| w.contains(needle)),
                "{doc}: {warnings:?}"
            );
        }
    }

    /// A folder both layers list is the project's; a project `prompt` list
    /// for a mode suppresses the user layer's trust there (ADR-0228).
    #[test]
    fn folders_split_by_layer_and_project_prompt_modes_suppress_user_trust() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let (root, user_dir) = (base.join("proj"), base.join("cfg"));
        for d in [root.join("scripts"), user_dir.join("mine")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let mut layers = vec![
            layer(
                ConfigLayer::User,
                &user_dir.join("config.yml"),
                "rhai:\n  trusted: [mine, ../proj/scripts]\nmodes:\n  plan:\n    prompt: [read]\n",
            ),
            layer(
                ConfigLayer::Project,
                &root.join(".entanglement/config.yml"),
                "rhai:\n  trusted: [scripts]\nmodes:\n  build:\n    prompt: [\"bash(make *)\"]\n  auto:\n    prompt: []\n    allow: [read]\n",
            ),
        ];
        let (trusted, _) = extract(&mut layers, Some(&root));
        let folders = folders(&trusted, &layers);
        assert_eq!(folders.project, vec![root.join("scripts")]);
        assert_eq!(folders.user, vec![user_dir.join("mine")]);
        assert_eq!(
            folders.user_suppressed.into_iter().collect::<Vec<_>>(),
            ["build"],
            "project prompt rules only; an empty list or a user one does not count"
        );
        assert!(!folders.user_suppressed_everywhere);
    }

    /// A project `permissions:` `prompt` list or `default: prompt` drops the
    /// user layer's trust in every mode; a user-layer one does not.
    #[test]
    fn a_project_ceiling_prompt_suppresses_user_trust_everywhere() {
        let doc = |layer, yaml: &str| RawLayer {
            layer,
            source: "config.yml".into(),
            doc: serde_yaml::from_str(yaml).unwrap(),
        };
        let user_only = [doc(ConfigLayer::User, "permissions: {prompt: [bash]}\n")];
        assert!(!folders(&[], &user_only).user_suppressed_everywhere);
        let empty = [doc(ConfigLayer::Project, "permissions: {prompt: []}\n")];
        assert!(!folders(&[], &empty).user_suppressed_everywhere);
        let project = [doc(ConfigLayer::Project, "permissions: {prompt: [bash]}\n")];
        assert!(folders(&[], &project).user_suppressed_everywhere);
        let default = [doc(
            ConfigLayer::Project,
            "permissions: {default: prompt}\n",
        )];
        assert!(folders(&[], &default).user_suppressed_everywhere);
        let allow = [doc(ConfigLayer::Project, "permissions: {default: allow}\n")];
        assert!(!folders(&[], &allow).user_suppressed_everywhere);
    }
}
