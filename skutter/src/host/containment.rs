//! Symlink-safe root containment (ADR-0054, #163): the one resolution the
//! host tools run a path through and the escape-root check
//! ([`super::escape`], ADR-0109) judges a call by, so the path a card names
//! is the path the tool then touches.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

/// Resolve `rel` against `root` and refuse anything outside it — for a tool
/// that never leaves the root, whatever the escape-root check decided
/// (`read_raw`, rhai-internal, stays strictly contained as in 0.7).
pub fn resolve_under_root(root: &Path, rel: &str) -> Result<PathBuf> {
    let (resolved, contained) = resolve_and_contained(root, rel)?;
    if !contained {
        return Err(anyhow::anyhow!("path escapes working directory: {rel}"));
    }
    Ok(resolved)
}

/// The absolute out-of-root target `rel` resolves to under `root`, or `None`
/// when `rel` stays contained (or can't be resolved at all). The escape-root
/// check (ADR-0109) asks for exactly this path, matching the resolution the
/// host tools use.
pub fn escaping_path(root: &Path, rel: &str) -> Option<PathBuf> {
    match resolve_and_contained(root, rel) {
        Ok((abs, false)) => Some(abs),
        _ => None,
    }
}

/// Resolve `rel` against `root` — lexical `..`/`.` normalization plus symlink-safe
/// canonicalization of the deepest existing ancestor (#163) — returning the
/// absolute target and whether it stays contained under `root`. The containment
/// flag is the AND of the lexical and canonical checks: the escape-root check
/// asks about a `false`, a tool it cleared uses the path either way, and
/// [`resolve_under_root`] refuses it. A genuinely malformed path (a `..`
/// popping past the filesystem root) is still a hard `Err`.
pub fn resolve_and_contained(root: &Path, rel: &str) -> Result<(PathBuf, bool)> {
    // Canonicalize the root defensively so containment holds even when the head
    // (or a test) passes a non-canonical root; startup also canonicalizes cwd.
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalizing working directory {}", root.display()))?;
    let joined = if Path::new(rel).is_absolute() {
        PathBuf::from(rel)
    } else {
        root.join(rel)
    };
    let mut norm = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !norm.pop() {
                    return Err(anyhow::anyhow!("path escapes working directory: {rel}"));
                }
            }
            other => norm.push(other.as_os_str()),
        }
    }
    let lexically_contained = norm.starts_with(&root);
    // Symlink defense (#163): canonicalize the deepest existing ancestor so a
    // symlink under root can't redirect the real target outside it. If the whole
    // path exists (incl. a final-component symlink) it canonicalizes directly;
    // otherwise we peel not-yet-existing tail components off until an ancestor
    // resolves, then re-append the plain tail.
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut ancestor = norm.clone();
    let canon = loop {
        match ancestor.canonicalize() {
            Ok(c) => break c,
            Err(_) => {
                let name = ancestor
                    .file_name()
                    .ok_or_else(|| anyhow::anyhow!("path escapes working directory: {rel}"))?
                    .to_os_string();
                if !ancestor.pop() {
                    return Err(anyhow::anyhow!("path escapes working directory: {rel}"));
                }
                tail.push(name);
            }
        }
    };
    let canonically_contained = canon.starts_with(&root);
    let mut resolved = canon;
    for name in tail.into_iter().rev() {
        resolved.push(name);
    }
    Ok((resolved, lexically_contained && canonically_contained))
}
