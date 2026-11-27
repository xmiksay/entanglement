//! Symlink-safe root containment (ADR-0054, #163): the one resolution the
//! host tools enforce and the escape-root gate (ADR-0109) and the `rhai`
//! launch check against, so a grant key (the resolved path) is identical on
//! both sides.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

/// Resolve `rel` against `root`, rejecting paths that escape the root via `..`
/// (and absolute paths that don't live under it) **or via a symlink**. The root
/// is canonicalized, then the resolved path is normalized lexically and its
/// deepest existing ancestor is canonicalized (following symlinks) and required
/// to stay under the canonical root; the not-yet-existing tail (for `edit`/
/// `write` creating a file) is `..`-free plain names, so re-appending it can't
/// escape. This upgrades ADR-0008's lexical-only containment to a real
/// write/read boundary without breaking the create path (ADR-0054, #163).
pub fn resolve_under_root(root: &Path, rel: &str) -> Result<PathBuf> {
    let (resolved, contained) = resolve_and_contained(root, rel)?;
    if !contained {
        return Err(anyhow::anyhow!("path escapes working directory: {rel}"));
    }
    Ok(resolved)
}

/// The absolute out-of-root target `rel` resolves to under `root`, or `None`
/// when `rel` stays contained (or can't be resolved at all). Used by the
/// executor's escape-root gate (ADR-0109) to decide whether a call needs
/// approval, matching the exact resolution the host tools use so the grant key
/// (the resolved path) is identical on both sides.
pub fn escaping_path(root: &Path, rel: &str) -> Option<PathBuf> {
    match resolve_and_contained(root, rel) {
        Ok((abs, false)) => Some(abs),
        _ => None,
    }
}

/// Resolve `rel` against `root` — lexical `..`/`.` normalization plus symlink-safe
/// canonicalization of the deepest existing ancestor (#163) — returning the
/// absolute target and whether it stays contained under `root`. The containment
/// flag is the AND of the lexical and canonical checks; a strict caller rejects
/// `false`, an escape-root caller (ADR-0109) may still use the path after a grant
/// check. A genuinely malformed path (a `..` popping past the filesystem root)
/// is still a hard `Err`.
pub(crate) fn resolve_and_contained(root: &Path, rel: &str) -> Result<(PathBuf, bool)> {
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
