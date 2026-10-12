//! Config layer discovery and merging (#172, ADR-0047): finding the
//! default/user/project layers, reading each into a YAML [`Value`], the
//! key-wise deep merge, per-key provenance for `skutter inspect config`, and
//! the first-run user-file scaffold (#219). Split out of `mod.rs` (#745),
//! which keeps the schema ([`super::Config`]) and its validation.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_yaml::Value;

use super::{migrate_permissions, DEFAULTS_YML};

/// The commented starter file written on first run (#219). Distinct from
/// [`DEFAULTS_YML`]: every setting here is commented *out*, so an untouched
/// scaffold is a pure no-op that never pins a default — it only exists to be a
/// discoverable, editable starting point.
pub(super) const TEMPLATE_YML: &str = include_str!("template.yml");

/// Env var overriding the user config file path (tests + non-XDG setups).
pub(super) const CONFIG_FILE_ENV: &str = "ENTANGLEMENT_CONFIG_FILE";

/// Which of the three precedence layers a value came from. Ordered low → high so
/// `Default < User < Project` matches discovery order and the later-wins rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConfigLayer {
    Default,
    User,
    Project,
}

impl ConfigLayer {
    /// Short label for `skutter inspect config`.
    pub fn label(self) -> &'static str {
        match self {
            ConfigLayer::Default => "default",
            ConfigLayer::User => "user",
            ConfigLayer::Project => "project",
        }
    }
}

/// One discovered config layer *before* merging: its precedence, a display label
/// for its origin (`built-in (defaults.yml)` or a file path), and the parsed doc.
#[derive(Clone)]
pub(super) struct RawLayer {
    pub(super) layer: ConfigLayer,
    pub(super) source: String,
    pub(super) doc: Value,
}

/// Enumerate the present config layers in precedence order — embedded defaults,
/// then the user file, then the project file. Missing files are skipped; an
/// unreadable or malformed file is an error.
/// `user_file` is the user layer's path — production passes
/// [`user_config_path`]; `None` reads no user layer.
pub(super) fn discover(root: &Path, user_file: Option<&Path>) -> Result<Vec<RawLayer>> {
    let mut layers = vec![default_layer()];
    if let Some(path) = user_file {
        migrate_permissions::migrate_if_legacy(path).with_context(|| {
            format!(
                "migrating legacy `permissions:` shape in {}",
                path.display()
            )
        })?;
        read_layer(ConfigLayer::User, path, &mut layers)?;
    }
    let project_path = root.join(".entanglement").join("config.yml");
    migrate_permissions::migrate_if_legacy(&project_path).with_context(|| {
        format!(
            "migrating legacy `permissions:` shape in {}",
            project_path.display()
        )
    })?;
    read_layer(ConfigLayer::Project, &project_path, &mut layers)?;
    Ok(layers)
}

/// The always-present embedded default layer.
pub(super) fn default_layer() -> RawLayer {
    RawLayer {
        layer: ConfigLayer::Default,
        source: "built-in (defaults.yml)".to_string(),
        doc: serde_yaml::from_str(DEFAULTS_YML)
            .expect("embedded defaults.yml is valid — guarded by test"),
    }
}

/// Read + parse the file at `path` (if it exists) as one layer. A missing file is
/// fine; an unreadable file or invalid YAML is a loud error.
pub(super) fn read_layer(
    layer: ConfigLayer,
    path: &Path,
    layers: &mut Vec<RawLayer>,
) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading user config {}", path.display()))?;
    let doc: Value = serde_yaml::from_str(&text)
        .with_context(|| format!("parsing user config {}", path.display()))?;
    // A comment-only or empty file parses to `Null`; it sets nothing, so skip it
    // rather than let it wipe the lower layers in the merge (the scaffolded
    // template, #219, is exactly this until a user uncomments a key).
    if doc.is_null() {
        return Ok(());
    }
    layers.push(RawLayer {
        layer,
        source: path.display().to_string(),
        doc,
    });
    Ok(())
}

/// The winning layer for each top-level key any layer set, in a stable key order.
pub(super) fn provenance(raw_layers: &[RawLayer]) -> Vec<(String, ConfigLayer)> {
    const KEYS: &[&str] = &[
        "agent",
        "mode",
        "provider",
        "model",
        "verbose",
        "permissions",
        "modes",
        "hooks",
        "mcp",
        "http_auth",
        "web_search",
        "max_turns",
        "idle_ttl_secs",
        "auto_compact",
        "editor",
        "session_retention_days",
        "tui",
        "undo",
    ];
    KEYS.iter()
        .filter_map(|key| {
            // Highest layer that carries this key wins (layers are low→high).
            raw_layers
                .iter()
                .rev()
                .find(|rl| rl.doc.get(*key).is_some())
                .map(|rl| (key.to_string(), rl.layer))
        })
        .collect()
}

/// Deep-merge `over` onto `base`: mappings merge key-wise recursively; scalars
/// and sequences are replaced by `over`. (The config file has no identity-keyed
/// sequences, so the catalog's by-`name`/`id` sequence merge isn't needed.)
pub(super) fn merge_value(base: Value, over: Value) -> Value {
    match (base, over) {
        (Value::Mapping(mut base_map), Value::Mapping(over_map)) => {
            for (key, over_val) in over_map {
                let merged = match base_map.remove(&key) {
                    Some(base_val) => merge_value(base_val, over_val),
                    None => over_val,
                };
                base_map.insert(key, merged);
            }
            Value::Mapping(base_map)
        }
        (_, over) => over,
    }
}

/// The user config path: `${config_dir}/entanglement/config.yml`, overridable via
/// `ENTANGLEMENT_CONFIG_FILE` (which tests point at a temp file).
pub fn user_config_path() -> Option<PathBuf> {
    resolve_user_config_path(|name| std::env::var_os(name), crate::user_dirs::config_dir)
}

/// [`user_config_path`] over an injected env lookup and config dir, so the
/// precedence is unit-testable without mutating process env (#773).
pub(super) fn resolve_user_config_path(
    env: impl Fn(&str) -> Option<OsString>,
    config_dir: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    env(CONFIG_FILE_ENV)
        .map(PathBuf::from)
        .or_else(|| config_dir().map(|d| d.join("entanglement").join("config.yml")))
}

/// First-run scaffold (#219): if the user config file does not exist yet, write a
/// commented starter template ([`TEMPLATE_YML`]) so `${config_dir}/entanglement/`
/// is a discoverable, editable starting point instead of an empty directory.
///
/// Best-effort and non-authoritative: the template is fully commented, so it
/// changes nothing until a user edits it — the embedded defaults still drive
/// behavior either way. Returns the written path on success, `None` if a file was
/// already present (or the config dir is unknown). Callers treat a write error as
/// non-fatal; startup must not fail because the home directory is read-only.
pub fn scaffold_if_missing() -> Result<Option<PathBuf>> {
    user_config_path().map_or(Ok(None), scaffold_at)
}

/// [`scaffold_if_missing`] at an explicit `path`.
pub(super) fn scaffold_at(path: PathBuf) -> Result<Option<PathBuf>> {
    if path.exists() {
        return Ok(None);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    std::fs::write(&path, TEMPLATE_YML)
        .with_context(|| format!("writing default config {}", path.display()))?;
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_env_override_wins_over_the_config_dir() {
        let path = resolve_user_config_path(
            |n| (n == CONFIG_FILE_ENV).then(|| "/srv/config.yml".into()),
            || panic!("the default must not be consulted"),
        );
        assert_eq!(path, Some(PathBuf::from("/srv/config.yml")));
    }

    /// ADR-0213: `ENTANGLEMENT_STATE_DIR` relocates managed files only, never
    /// this hand-edited input.
    #[test]
    fn the_state_dir_never_moves_config_yml() {
        let path = resolve_user_config_path(
            |n| (n == super::super::state_dir::STATE_DIR_ENV).then(|| "/state".into()),
            || Some(PathBuf::from("/cfg")),
        );
        assert_eq!(path, Some(PathBuf::from("/cfg/entanglement/config.yml")));
    }

    #[test]
    fn unset_falls_back_to_the_config_dir() {
        let path = resolve_user_config_path(|_| None, || Some(PathBuf::from("/cfg")));
        assert_eq!(path, Some(PathBuf::from("/cfg/entanglement/config.yml")));
        assert_eq!(resolve_user_config_path(|_| None, || None), None);
    }
}
