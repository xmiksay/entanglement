//! The `config.yml` `undo:` block (#787): whether per-write file backups
//! (ADR-0212) are recorded, and their size caps. It deserializes straight
//! into the library's `BackupLimits`, whose serde form owns the size grammar
//! (bytes or e.g. `8MiB`); this module re-applies it on a config reload.

use crate::file_backup::FileBackups;

use super::Config;

/// Apply a reloaded config's `undo:` block to the live `backups`. A config
/// that no longer parses keeps the previous limits — a mid-save edit must not
/// switch backups off or reset the caps.
pub fn reapply(backups: &FileBackups, loaded: anyhow::Result<Config>) {
    match loaded {
        Ok(config) if config.undo != backups.limits() => {
            tracing::info!("undo backup limits reloaded: {:?}", config.undo);
            backups.set_limits(config.undo);
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("undo limits not reloaded, keeping previous: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use crate::file_backup::{BackupLimits, DEFAULT_MAX_ROOT_BYTES};

    use super::*;

    #[test]
    fn layered_config_resolves_undo_with_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user.yml");
        std::fs::write(&user, "undo:\n  max_file_bytes: 1MiB\n").unwrap();
        let project = dir.path().join(".entanglement");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("config.yml"), "undo:\n  enabled: false\n").unwrap();

        let resolved = Config::resolve_with(dir.path(), Some(&user)).unwrap();
        // Deep-merged: the project's `enabled` keeps the user's cap.
        assert_eq!(
            resolved.config.undo,
            BackupLimits {
                enabled: false,
                max_file_bytes: 1024 * 1024,
                max_root_bytes: DEFAULT_MAX_ROOT_BYTES,
            }
        );
        let prov: std::collections::HashMap<_, _> = resolved.provenance.into_iter().collect();
        assert_eq!(prov.get("undo"), Some(&super::super::ConfigLayer::Project));

        std::fs::write(&user, "undo:\n  max_root_bytes: lots\n").unwrap();
        let err = Config::resolve_with(dir.path(), Some(&user)).err().unwrap();
        assert!(format!("{err:#}").contains("not a size"), "{err:#}");

        std::fs::write(&user, "undo:\n  max_file_mb: 8\n").unwrap();
        assert!(
            Config::resolve_with(dir.path(), Some(&user)).is_err(),
            "an unknown `undo:` key is a config error"
        );
    }

    #[test]
    fn template_example_parses_to_the_defaults() {
        // The scaffolded template's commented `undo:` block, uncommented,
        // must spell exactly the built-in defaults.
        let block: String = super::super::layers::TEMPLATE_YML
            .lines()
            .skip_while(|l| *l != "#undo:")
            .skip(1)
            .take_while(|l| l.starts_with("#  "))
            .map(|l| format!("{}\n", &l[3..]))
            .collect();
        assert!(!block.is_empty(), "template lost its `undo:` example");
        let l: BackupLimits = serde_yaml::from_str(&block).unwrap();
        assert_eq!(l, BackupLimits::default());
    }

    #[test]
    fn reapply_swaps_limits_and_keeps_them_on_a_bad_config() {
        let dir = tempfile::tempdir().unwrap();
        let backups =
            FileBackups::new(crate::session_store::SessionStore::at(dir.path().join("s")));
        let user = dir.path().join("user.yml");
        std::fs::write(&user, "undo:\n  enabled: false\n  max_file_bytes: 2KiB\n").unwrap();

        reapply(&backups, Config::load_with(dir.path(), Some(&user)));
        let off = backups.limits();
        assert!(!off.enabled);
        assert_eq!(off.max_file_bytes, 2048);

        std::fs::write(&user, "undo:\n  max_file_bytes: 0\n").unwrap();
        reapply(&backups, Config::load_with(dir.path(), Some(&user)));
        assert_eq!(backups.limits(), off, "a broken edit keeps the last limits");
    }
}
