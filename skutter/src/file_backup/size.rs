//! The byte-size grammar of [`super::BackupLimits`]'s serde form (#787): a
//! plain integer (bytes) or a string `<digits>[ ]<unit>` with a
//! case-insensitive unit: `B`, decimal `KB`/`MB`/`GB` (powers of 1000) or
//! binary `KiB`/`MiB`/`GiB` (powers of 1024). No fractions. It must be at
//! least 1 byte — `enabled: false` is the off switch, so a `0` cap is
//! rejected rather than read as one — and at most [`MAX_SIZE`].

use serde::{Deserialize, Deserializer};

const KIB: u64 = 1024;
const MIB: u64 = KIB * 1024;
const GIB: u64 = MIB * 1024;

/// The sanity ceiling on any cap: 64 GiB. Far beyond any real backup budget,
/// and it keeps `stored + size` arithmetic nowhere near `u64::MAX`.
pub(super) const MAX_SIZE: u64 = 64 * GIB;

/// Parse a human size string (see the module doc for the grammar).
pub(super) fn parse_size(text: &str) -> Result<u64, String> {
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

/// serde `deserialize_with` for a size field: an integer or a size string.
pub(super) fn de_size<'de, D: Deserializer<'de>>(de: D) -> Result<u64, D::Error> {
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
    use super::super::{BackupLimits, DEFAULT_MAX_FILE_BYTES};
    use super::*;

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
    fn limits_accept_integers_and_strings_and_fill_defaults() {
        let parse = |y: &str| serde_yaml::from_str::<BackupLimits>(y).unwrap();
        assert_eq!(
            parse("max_file_bytes: 4096"),
            BackupLimits {
                max_file_bytes: 4096,
                ..BackupLimits::default()
            }
        );
        let l = parse("enabled: false\nmax_root_bytes: 1GiB");
        assert!(!l.enabled);
        assert_eq!(l.max_root_bytes, GIB);
        assert_eq!(l.max_file_bytes, DEFAULT_MAX_FILE_BYTES);
    }

    #[test]
    fn limits_reject_bad_sizes_and_unknown_keys() {
        for bad in [
            "max_file_bytes: 0",
            "max_file_bytes: 8 parsecs",
            "max_root_bytes: 100GiB",
            "max_file_mb: 8",
        ] {
            assert!(
                serde_yaml::from_str::<BackupLimits>(bad).is_err(),
                "`{bad}` must be a parse error"
            );
        }
    }
}
