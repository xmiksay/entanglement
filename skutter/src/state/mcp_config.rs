//! Surgical persistence for the user config's `mcp:` section (#375, B6), so
//! a live `/mcp add`/`remove` survives a restart.
//!
//! The library changes only its in-memory server set (#966);
//! [`spawn_mcp_config_writer`] writes that set back here on every successful
//! `OutEvent::McpChanged`. Unlike the managed state files — kept *outside*
//! `config.yml` so they can be rewritten freely — MCP servers stay part of the
//! primary `config.yml`: a live change mirrors into the same file a user would
//! otherwise hand-edit, without clobbering whatever else (`permissions`,
//! `hooks`, …) they set alongside it.
//!
//! [`save_mcp`] loads the file as a [`serde_yaml::Value`] — not the typed
//! `Config`, which would drop any key it doesn't know about — replaces only
//! the top-level `mcp` mapping, and reserializes. A missing file is created
//! fresh with just that key (the full-rewrite fallback). It is locked (#329)
//! and atomic.
//!
//! Note: a `serde_yaml::Value` round-trip does not preserve comments — no layer
//! of the config loader does (its layer merge operates at the `Value` level
//! with the same limitation).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use entanglement_core::{Holly, OutEvent};
use entanglement_runtime::config::atomic::atomic_write;
use entanglement_runtime::config::lock;
use entanglement_runtime::mcp::{McpServerConfig, ServerConfigs};
use serde_yaml::Value;
use tokio::sync::broadcast::error::RecvError;

/// Write `configs` back to `path` (the user `config.yml`) after every
/// successful live add/remove. A lagged subscriber writes too — the whole set
/// goes out each time, so a missed event costs nothing. A write failure is
/// logged: the change itself already happened in this process.
pub fn spawn_mcp_config_writer(
    holly: &Holly,
    configs: ServerConfigs,
    path: Option<PathBuf>,
) -> tokio::task::JoinHandle<()> {
    let mut events = holly.subscribe();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(OutEvent::McpChanged { error: None, .. }) | Err(RecvError::Lagged(_)) => {}
                Ok(_) => continue,
                Err(RecvError::Closed) => break,
            }
            let servers = configs
                .lock()
                .expect("MCP server-config mutex poisoned")
                .clone();
            let Some(path) = path.clone() else {
                tracing::warn!(
                    "MCP server change not saved: no config directory \
                     (set ENTANGLEMENT_CONFIG_FILE to a path)"
                );
                continue;
            };
            let saved = tokio::task::spawn_blocking(move || save_mcp(&path, &servers)).await;
            match saved {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("saving the MCP servers to config.yml failed: {e:#}"),
                Err(e) => tracing::warn!("saving the MCP servers to config.yml panicked: {e}"),
            }
        }
    })
}

/// Rewrite the `mcp:` key of `path` to exactly `servers`, preserving every
/// other top-level key. Read-modify-write under an exclusive lock (#329) so
/// two `skutter` instances doing a live add/remove can't clobber each other —
/// the closure re-reads the file's current on-disk state itself, per
/// [`lock::with_locked_file`]'s contract.
pub fn save_mcp(path: &Path, servers: &HashMap<String, McpServerConfig>) -> Result<()> {
    lock::with_locked_file(path, || {
        let mut doc = read_doc(path)?;
        let mapping = doc.as_mapping_mut().ok_or_else(|| {
            anyhow::anyhow!(
                "{} does not parse as a YAML mapping at its root",
                path.display()
            )
        })?;
        let mcp_value = serde_yaml::to_value(servers).context("serializing mcp servers")?;
        mapping.insert(Value::String("mcp".to_string()), mcp_value);
        let body = serde_yaml::to_string(&doc).context("serializing config.yml")?;
        atomic_write(path, &body)
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
    use entanglement_core::McpAction;
    use entanglement_runtime::config::Config;
    use std::sync::{Arc, Mutex};

    fn stdio_cfg(command: &str) -> McpServerConfig {
        McpServerConfig {
            command: Some(command.to_string()),
            args: vec!["-y".to_string()],
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
            disabled: false,
            capabilities: HashMap::new(),
            oauth: None,
            description: None,
        }
    }

    #[test]
    fn save_mcp_creates_a_fresh_file_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yml");

        let mut servers = HashMap::new();
        servers.insert("everything".to_string(), stdio_cfg("npx"));
        save_mcp(&path, &servers).unwrap();

        let resolved = Config::load_with(dir.path(), Some(&path)).unwrap();

        assert_eq!(resolved.mcp.len(), 1);
        assert_eq!(resolved.mcp["everything"].command.as_deref(), Some("npx"));
    }

    #[test]
    fn save_mcp_preserves_sibling_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yml");
        std::fs::write(&path, "provider: anthropic\nverbose: true\n").unwrap();

        let mut servers = HashMap::new();
        servers.insert("srv".to_string(), stdio_cfg("my-server"));
        save_mcp(&path, &servers).unwrap();

        let resolved = Config::load_with(dir.path(), Some(&path)).unwrap();

        assert_eq!(resolved.provider.as_deref(), Some("anthropic"));
        assert!(resolved.verbose);
        assert_eq!(resolved.mcp.len(), 1);
        assert!(resolved.mcp.contains_key("srv"));
    }

    #[test]
    fn save_mcp_round_trips_add_then_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yml");

        let mut servers = HashMap::new();
        servers.insert("srv".to_string(), stdio_cfg("my-server"));
        save_mcp(&path, &servers).unwrap();
        assert!(Config::load_with(dir.path(), Some(&path))
            .unwrap()
            .mcp
            .contains_key("srv"));

        servers.remove("srv");
        save_mcp(&path, &servers).unwrap();
        let resolved = Config::load_with(dir.path(), Some(&path)).unwrap();

        assert!(
            !resolved.mcp.contains_key("srv"),
            "removed server must not survive reload"
        );
    }

    #[test]
    fn save_mcp_overwrites_a_prior_mcp_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yml");
        std::fs::write(&path, "mcp:\n  old:\n    command: old-binary\n").unwrap();

        let mut servers = HashMap::new();
        servers.insert("new".to_string(), stdio_cfg("new-binary"));
        save_mcp(&path, &servers).unwrap();

        let resolved = Config::load_with(dir.path(), Some(&path)).unwrap();

        assert_eq!(resolved.mcp.len(), 1);
        assert!(resolved.mcp.contains_key("new"));
        assert!(!resolved.mcp.contains_key("old"));
    }

    /// A successful live change writes the whole set out.
    #[tokio::test]
    async fn the_writer_saves_the_set_on_a_successful_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yml");
        let holly = Holly::spawn(entanglement_core::EngineConfig::default());
        let configs: ServerConfigs = Arc::new(Mutex::new(HashMap::new()));
        let writer = spawn_mcp_config_writer(&holly, configs.clone(), Some(path.clone()));

        holly.emit_mcp_change_failed("srv".into(), McpAction::Added, "nope".into());
        configs
            .lock()
            .unwrap()
            .insert("srv".to_string(), stdio_cfg("my-server"));
        holly.emit_mcp_changed("srv".into(), McpAction::Added);

        let mut saved = false;
        for _ in 0..100 {
            if path.exists() {
                saved = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(saved, "the successful add must be written");
        let resolved = Config::load_with(dir.path(), Some(&path)).unwrap();
        assert!(resolved.mcp.contains_key("srv"));
        writer.abort();
    }
}
