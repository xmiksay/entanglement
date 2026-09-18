//! Self-heal a legacy native-layer agent file (ADR-0207 stage 6a). See the
//! parent module's "Migrating a legacy native-layer file" doc section for the
//! full picture; this module holds just the mechanics, split out to keep
//! `agents/mod.rs` from growing further past its (already grandfathered)
//! line cap.

use std::path::Path;

use anyhow::{Context, Result};

/// Frontmatter keys `Agent` carried directly until ADR-0207 moved every
/// permission fact onto the session's independent permission mode.
const RETIRED_AGENT_KEYS: &[&str] = &[
    "tools",
    "disallowed_tools",
    "permission",
    "mode",
    "can_spawn",
    "spawnable_agents",
    "sandbox",
];

/// Remove [`RETIRED_AGENT_KEYS`] from `frontmatter`, if any are present.
/// `Ok(None)` means none were found — the caller's original parse error is a
/// genuine typo, unrelated to the ADR-0207 migration, and must still abort.
/// Otherwise returns the re-serialized frontmatter with those keys gone, plus
/// the removed keys/values (for [`infer_legacy_mode`]).
fn strip_retired_keys(frontmatter: &str) -> Result<Option<(String, serde_yaml::Mapping)>> {
    let value: serde_yaml::Value =
        serde_yaml::from_str(frontmatter).context("frontmatter is not valid YAML")?;
    let serde_yaml::Value::Mapping(mut map) = value else {
        return Ok(None);
    };
    let mut removed = serde_yaml::Mapping::new();
    for key in RETIRED_AGENT_KEYS {
        if let Some(v) = map.remove(serde_yaml::Value::String((*key).to_string())) {
            removed.insert(serde_yaml::Value::String((*key).to_string()), v);
        }
    }
    if removed.is_empty() {
        return Ok(None);
    }
    let cleaned = serde_yaml::to_string(&serde_yaml::Value::Mapping(map))
        .context("re-serializing frontmatter after dropping retired keys")?;
    Ok(Some((cleaned, removed)))
}

/// Best-effort guess at which permission mode a legacy file's dropped rules
/// map onto, so the migration warning points somewhere useful instead of just
/// saying authority is gone. Not exhaustive — every shape the old
/// `tools`/`disallowed_tools`/`permission` keys could take isn't reconstructed
/// here, only the common cases: an explicit deny-ish `permission`, a
/// `disallowed_tools` list (inherently a restriction), or a `tools` allowlist
/// with no write/exec entry all read as `research`; anything else defaults to
/// `build` (the old unrestricted posture).
fn infer_legacy_mode(removed: &serde_yaml::Mapping) -> &'static str {
    let get = |k: &str| removed.get(serde_yaml::Value::String(k.to_string()));
    if get("disallowed_tools").is_some() {
        return "research";
    }
    if let Some(serde_yaml::Value::String(p)) = get("permission") {
        if p.to_lowercase().contains("deny") {
            return "research";
        }
    }
    if let Some(serde_yaml::Value::Sequence(list)) = get("tools") {
        const WRITE_MARKERS: [&str; 6] = ["write", "edit", "apply_patch", "bash", "call", "rhai"];
        let write_capable = list.iter().any(|v| {
            v.as_str()
                .map(|s| {
                    let s = s.to_lowercase();
                    WRITE_MARKERS.iter().any(|m| s.contains(m))
                })
                .unwrap_or(false)
        });
        if !write_capable {
            return "research";
        }
    }
    "build"
}

/// Self-heal a native-layer file that still carries [`RETIRED_AGENT_KEYS`]
/// (ADR-0207 stage 6a): back it up to `<path>.bak`, rewrite it without those
/// keys, and warn naming the file and the closest replacement mode. `Ok(None)`
/// when `frontmatter` carries none of them, so the caller's original parse
/// error stands — this must never paper over a genuine typo.
pub(super) fn migrate_legacy_agent_file(
    path: &Path,
    frontmatter: &str,
    body: &str,
) -> Result<Option<String>> {
    let Some((cleaned, removed)) = strip_retired_keys(frontmatter)? else {
        return Ok(None);
    };
    let mode = infer_legacy_mode(&removed);
    let bak = path.with_extension("md.bak");
    std::fs::copy(path, &bak)
        .with_context(|| format!("backing up {} to {}", path.display(), bak.display()))?;
    let rewritten = format!("---\n{cleaned}---\n{body}");
    std::fs::write(path, &rewritten)
        .with_context(|| format!("rewriting {} without retired keys", path.display()))?;
    let dropped: Vec<&str> = removed.keys().filter_map(|k| k.as_str()).collect();
    tracing::warn!(
        file = %path.display(),
        dropped = %dropped.join(", "),
        suggested_mode = mode,
        backup = %bak.display(),
        "agent definition carried retired authority keys (ADR-0207); dropped them and rewrote \
         the file — authority now lives in the session's permission mode (`--mode`/`/mode`), not \
         the agent; based on what this file allowed, the closest mode is `{mode}`",
    );
    Ok(Some(cleaned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_retired_keys_is_a_noop_when_none_are_present() {
        assert!(
            strip_retired_keys("name: x\ndescription: d\nmodel: glm-4.7\n")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn infer_legacy_mode_reads_disallowed_tools_or_deny_as_research() {
        let (_, removed) = strip_retired_keys("disallowed_tools: [write, bash]\n")
            .unwrap()
            .expect("retired key present");
        assert_eq!(infer_legacy_mode(&removed), "research");

        let (_, removed) = strip_retired_keys("permission: deny\n")
            .unwrap()
            .expect("retired key present");
        assert_eq!(infer_legacy_mode(&removed), "research");

        let (_, removed) = strip_retired_keys("tools: [read, glob, grep]\n")
            .unwrap()
            .expect("retired key present");
        assert_eq!(infer_legacy_mode(&removed), "research");
    }

    #[test]
    fn infer_legacy_mode_reads_a_write_capable_tools_list_as_build() {
        let (_, removed) = strip_retired_keys("tools: [read, write, bash]\n")
            .unwrap()
            .expect("retired key present");
        assert_eq!(infer_legacy_mode(&removed), "build");
    }
}
