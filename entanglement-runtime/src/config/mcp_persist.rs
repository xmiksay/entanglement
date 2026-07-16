//! Surgical persistence for the user config's `mcp:` section (#375, B6).
//!
//! Unlike the managed sibling files (grants/agent-models/agent-generation/env)
//! — deliberately kept *outside* `config.yml` so the runtime can rewrite them
//! freely without disturbing a hand-edited file — MCP servers stay part of the
//! primary `config.yml`: a live `/mcp add`/`remove` needs to mirror into the
//! same file a user would otherwise hand-edit, without clobbering whatever
//! else (`permissions`, `hooks`, …) they set alongside it.
//!
//! [`save_mcp`] loads the file as a [`serde_yaml::Value`] — not the typed
//! [`Config`][super::Config], which would drop any key it doesn't know about —
//! replaces only the top-level `mcp` mapping, and reserializes. A missing file
//! is created fresh with just that key (the full-rewrite fallback). Like every
//! other managed write in this module, it is locked (#329) and atomic.
//!
//! Note: a `serde_yaml::Value` round-trip does not preserve comments — no layer
//! in this codebase's config loader does (the merge in [`super::merge_value`]
//! already operates at the `Value` level with the same limitation), so this is
//! consistent with, not a regression from, the existing merge behavior.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_yaml::Value;

use crate::mcp::McpServerConfig;

use super::{atomic::atomic_write, lock, user_config_path, CONFIG_FILE_ENV};

/// Rewrite the `mcp:` key of the user's `config.yml` to exactly `servers`,
/// preserving every other top-level key. Read-modify-write under an exclusive
/// lock (#329) so two `skutter` instances doing a live add/remove can't
/// clobber each other — the closure re-reads the file's current on-disk state
/// itself, per [`lock::with_locked_file`]'s contract.
pub fn save_mcp(servers: &HashMap<String, McpServerConfig>) -> Result<()> {
    let Some(path) = user_config_path() else {
        bail!("no config directory available; set {CONFIG_FILE_ENV} to a path first");
    };
    lock::with_locked_file(&path, || {
        let mut doc = read_doc(&path)?;
        let mapping = doc.as_mapping_mut().ok_or_else(|| {
            anyhow::anyhow!(
                "{} does not parse as a YAML mapping at its root",
                path.display()
            )
        })?;
        let mcp_value = serde_yaml::to_value(servers).context("serializing mcp servers")?;
        mapping.insert(Value::String("mcp".to_string()), mcp_value);
        let body = serde_yaml::to_string(&doc).context("serializing config.yml")?;
        atomic_write(&path, &body)
    })
}

/// Read `path` as a [`Value`], defaulting to an empty mapping when the file is
/// absent (the full-rewrite fallback) or a comment-only/empty file (parses to
/// `Null` — the scaffolded template, #219, is exactly this until a user
/// uncomments a key).
fn read_doc(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Mapping(Default::default()));
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading user config {}", path.display()))?;
    let doc: Value = serde_yaml::from_str(&text)
        .with_context(|| format!("parsing user config {}", path.display()))?;
    Ok(if doc.is_null() {
        Value::Mapping(Default::default())
    } else {
        doc
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ENV_LOCK};

    fn stdio_cfg(command: &str) -> McpServerConfig {
        McpServerConfig {
            command: Some(command.to_string()),
            args: vec!["-y".to_string()],
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
            disabled: false,
        }
    }

    #[test]
    fn save_mcp_creates_a_fresh_file_when_absent() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yml");
        std::env::set_var(CONFIG_FILE_ENV, &path);

        let mut servers = HashMap::new();
        servers.insert("everything".to_string(), stdio_cfg("npx"));
        save_mcp(&servers).unwrap();

        let resolved = Config::load(dir.path()).unwrap();
        std::env::remove_var(CONFIG_FILE_ENV);

        assert_eq!(resolved.mcp.len(), 1);
        assert_eq!(resolved.mcp["everything"].command.as_deref(), Some("npx"));
    }

    #[test]
    fn save_mcp_preserves_sibling_keys() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yml");
        std::fs::write(&path, "provider: anthropic\nverbose: true\n").unwrap();
        std::env::set_var(CONFIG_FILE_ENV, &path);

        let mut servers = HashMap::new();
        servers.insert("srv".to_string(), stdio_cfg("my-server"));
        save_mcp(&servers).unwrap();

        let resolved = Config::load(dir.path()).unwrap();
        std::env::remove_var(CONFIG_FILE_ENV);

        assert_eq!(resolved.provider.as_deref(), Some("anthropic"));
        assert!(resolved.verbose);
        assert_eq!(resolved.mcp.len(), 1);
        assert!(resolved.mcp.contains_key("srv"));
    }

    #[test]
    fn save_mcp_round_trips_add_then_remove() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yml");
        std::env::set_var(CONFIG_FILE_ENV, &path);

        let mut servers = HashMap::new();
        servers.insert("srv".to_string(), stdio_cfg("my-server"));
        save_mcp(&servers).unwrap();
        assert!(Config::load(dir.path()).unwrap().mcp.contains_key("srv"));

        servers.remove("srv");
        save_mcp(&servers).unwrap();
        let resolved = Config::load(dir.path()).unwrap();
        std::env::remove_var(CONFIG_FILE_ENV);

        assert!(
            !resolved.mcp.contains_key("srv"),
            "removed server must not survive reload"
        );
    }

    #[test]
    fn save_mcp_overwrites_a_prior_mcp_section() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yml");
        std::fs::write(&path, "mcp:\n  old:\n    command: old-binary\n").unwrap();
        std::env::set_var(CONFIG_FILE_ENV, &path);

        let mut servers = HashMap::new();
        servers.insert("new".to_string(), stdio_cfg("new-binary"));
        save_mcp(&servers).unwrap();

        let resolved = Config::load(dir.path()).unwrap();
        std::env::remove_var(CONFIG_FILE_ENV);

        assert_eq!(resolved.mcp.len(), 1);
        assert!(resolved.mcp.contains_key("new"));
        assert!(!resolved.mcp.contains_key("old"));
    }
}
