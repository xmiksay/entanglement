//! The `config.yml` `undo:` block (#787): whether per-write file backups
//! (ADR-0212) are recorded, and their size caps. Deserializes straight into
//! [`crate::file_backup::BackupLimits`]; this module owns the size grammar.
//!
//! A size is a plain integer (bytes) or a string `<digits>[ ]<unit>` with a
//! case-insensitive unit: `B`, decimal `KB`/`MB`/`GB` (powers of 1000) or
//! binary `KiB`/`MiB`/`GiB` (powers of 1024). No fractions. It must be at
//! least 1 byte — `enabled: false` is the off switch, so a `0` cap is
//! rejected rather than read as one — and at most [`MAX_SIZE`].

use serde::{Deserialize, Deserializer};

use super::Config;
use crate::file_backup::FileBackups;

const KIB: u64 = 1024;
const MIB: u64 = KIB * 1024;
const GIB: u64 = MIB * 1024;

/// The sanity ceiling on any cap: 64 GiB. Far beyond any real backup budget,
/// and it keeps `stored + size` arithmetic nowhere near `u64::MAX`.
pub const MAX_SIZE: u64 = 64 * GIB;

/// Parse a human size string (see the module doc for the grammar).
pub fn parse_size(text: &str) -> Result<u64, String> {
    let t = text.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (digits, unit) = t.split_at(split);
    if digits.is_empty() {
        return Err(format!(
            "`{text}` is not a size — expected bytes or e.g. `8MiB`, `256MB`"
        ));
    }
    let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" => 1000,
        "mb" => 1000 * 1000,
        "gb" => 1000 * 1000 * 1000,
        "kib" => KIB,
        "mib" => MIB,
        "gib" => GIB,
        other => {
            return Err(format!(
                "unknown size unit `{other}` in `{text}` — use B, KB, MB, GB, KiB, MiB or GiB"
            ))
        }
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("`{text}` is too large (max {})", format_size(MAX_SIZE)))?;
    check_size(n.saturating_mul(multiplier))
}

fn check_size(n: u64) -> Result<u64, String> {
    match n {
        0 => Err(
            "a backup cap must be at least 1 byte — set `undo.enabled: false` \
                  to turn backups off"
                .to_string(),
        ),
        n if n > MAX_SIZE => Err(format!(
            "a backup cap of {n} bytes exceeds the {} maximum",
            format_size(MAX_SIZE)
        )),
        n => Ok(n),
    }
}

/// Render `n` bytes exactly: the largest binary unit that divides it, else
/// plain bytes — so a cap set as `8MB` never displays as a rounded `7 MiB`.
pub fn format_size(n: u64) -> String {
    for (unit, name) in [(GIB, "GiB"), (MIB, "MiB"), (KIB, "KiB")] {
        if n >= unit && n.is_multiple_of(unit) {
            return format!("{} {name}", n / unit);
        }
    }
    format!("{n} bytes")
}

/// Apply a reloaded config's `undo:` block to the live `backups`. A config
/// that no longer parses keeps the previous limits — a mid-save edit must not
/// switch backups off or reset the caps.
pub(crate) fn reapply(backups: &FileBackups, loaded: anyhow::Result<Config>) {
    match loaded {
        Ok(config) if config.undo != backups.limits() => {
            tracing::info!("undo backup limits reloaded: {:?}", config.undo);
            backups.set_limits(config.undo);
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("undo limits not reloaded, keeping previous: {e:#}"),
    }
}

/// serde `deserialize_with` for a size field: a YAML integer or a size string.
pub(crate) fn de_size<'de, D: Deserializer<'de>>(de: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Bytes(u64),
        Text(String),
    }
    let parsed = match Raw::deserialize(de)? {
        Raw::Bytes(n) => check_size(n),
        Raw::Text(s) => parse_size(&s),
    };
    parsed.map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_backup::BackupLimits;

    #[test]
    fn sizes_parse_with_decimal_and_binary_units() {
        assert_eq!(parse_size("1024"), Ok(1024));
        assert_eq!(parse_size("8MiB"), Ok(8 * MIB));
        assert_eq!(parse_size("8 mib"), Ok(8 * MIB));
        assert_eq!(parse_size("256MB"), Ok(256_000_000));
        assert_eq!(parse_size("2KiB"), Ok(2048));
        assert_eq!(parse_size("3kb"), Ok(3000));
        assert_eq!(parse_size("1GiB"), Ok(GIB));
        assert_eq!(parse_size("1 GB"), Ok(1_000_000_000));
        assert_eq!(parse_size(" 12B "), Ok(12));
        assert_eq!(parse_size("64GiB"), Ok(MAX_SIZE));
    }

    #[test]
    fn nonsense_sizes_are_rejected() {
        for bad in [
            "", "MiB", "-1", "1.5MiB", "8 megs", "8TiB", "0", "0MiB", "65GiB",
        ] {
            assert!(parse_size(bad).is_err(), "`{bad}` must be rejected");
        }
        // Overflowing digits or multiplication is a clean error, not a wrap.
        assert!(parse_size("99999999999999999999999").is_err());
        assert!(parse_size("18446744073709551615GiB").is_err());
    }

    #[test]
    fn sizes_format_exactly() {
        assert_eq!(format_size(8 * MIB), "8 MiB");
        assert_eq!(format_size(256_000_000), "250000 KiB");
        assert_eq!(format_size(3 * GIB), "3 GiB");
        assert_eq!(format_size(1000), "1000 bytes");
    }

    #[test]
    fn block_accepts_integers_and_strings_and_fills_defaults() {
        let l: BackupLimits = serde_yaml::from_str("max_file_bytes: 4096").unwrap();
        assert_eq!(
            l,
            BackupLimits {
                max_file_bytes: 4096,
                ..BackupLimits::default()
            }
        );
        let l: BackupLimits = serde_yaml::from_str("enabled: false\nmax_root_bytes: 1GiB").unwrap();
        assert!(!l.enabled);
        assert_eq!(l.max_root_bytes, GIB);
        assert_eq!(l.max_file_bytes, crate::file_backup::DEFAULT_MAX_FILE_BYTES);
    }

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
                max_file_bytes: MIB,
                max_root_bytes: crate::file_backup::DEFAULT_MAX_ROOT_BYTES,
            }
        );
        let prov: std::collections::HashMap<_, _> = resolved.provenance.into_iter().collect();
        assert_eq!(prov.get("undo"), Some(&super::super::ConfigLayer::Project));

        std::fs::write(&user, "undo:\n  max_root_bytes: lots\n").unwrap();
        let err = Config::resolve_with(dir.path(), Some(&user)).err().unwrap();
        assert!(format!("{err:#}").contains("not a size"), "{err:#}");
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

    #[test]
    fn block_rejects_bad_sizes_and_unknown_keys() {
        for bad in [
            "max_file_bytes: 0",
            "max_file_bytes: 8 parsecs",
            "max_root_bytes: 100GiB",
            "max_file_mb: 8",
        ] {
            assert!(
                serde_yaml::from_str::<BackupLimits>(bad).is_err(),
                "`{bad}` must be a config error"
            );
        }
    }
}
