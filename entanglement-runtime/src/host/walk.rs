//! [`list_files`] — the shared glob-walk + root-containment engine behind
//! `glob`/`grep` (ADR-0008; symlink-safe containment ADR-0054/#163; the
//! escape-root search-widening path #482/[ADR-0132](../../../docs/adr/0132-glob-grep-escape-root-search-via-durable-grant.md)).
//! Split out of `host/mod.rs` to stay under the 400-line file cap (#451).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Cap on how many paths `glob` returns and how many matches `grep` reports —
/// bounds the work + output for pathologically large trees.
pub(crate) const MAX_RESULTS: usize = 1000;

/// Result of [`list_files`]: the matched files plus enough metadata for the
/// caller to distinguish "no match at all" from "matched only directories" or
/// "every entry errored." Without that distinction a bare-`**` pattern (which
/// matches directories only) looks identical to a typo, and the model has no
/// way to self-correct — see ADR-0016.
#[derive(Debug, Default)]
pub struct FileList {
    /// Files (in arbitrary glob-walk order), already capped at [`MAX_RESULTS`].
    pub files: Vec<PathBuf>,
    /// Entries the pattern matched but that were directories (filtered out).
    pub matched_dirs: usize,
    /// Entries the glob iterator yielded as `Err` (permissions, IO, etc.).
    pub skipped_errors: usize,
}

impl FileList {
    /// True iff the pattern matched at least one entry of any kind.
    #[allow(dead_code)]
    pub fn matched_anything(&self) -> bool {
        !self.files.is_empty() || self.matched_dirs > 0
    }
}

/// [`list_files_with_extra_roots`] with no escape-root store — byte-identical
/// to strict root containment (the pre-#482 behavior).
pub fn list_files(root: &Path, pattern: &str, excludes: &[String]) -> Result<FileList> {
    list_files_with_extra_roots(root, pattern, excludes, None)
}

/// Enumerate files under `root` matching `pattern` (a glob relative to root),
/// yielding display paths relative to root. `excludes` is a caller-supplied
/// list of glob patterns (matched against the root-relative path) additional
/// entries are dropped for — on top of that, any path with a `.git` path
/// component is **always** dropped, unconditionally: `.git` internals are
/// large, mostly binary, and never something an agent needs to read or
/// search, so the exclusion isn't tied to (and can't be defeated by) the
/// `excludes` list (ADR-0099). Skips directories (counted in
/// [`FileList::matched_dirs`]) and logs unreadable entries as `warn!` (counted
/// in [`FileList::skipped_errors`]) instead of silently dropping them.
/// Excluded entries are dropped before either count, so an excluded subtree
/// looks to the caller like it was never in the walk at all. Bounds the walk
/// at [`MAX_RESULTS`] files.
///
/// `extra_roots` widens containment for `glob`/`grep` (#482,
/// [ADR-0132](../../../docs/adr/0132-glob-grep-escape-root-search-via-durable-grant.md)):
/// a match whose canonical path escapes `root` is still admitted when it (or
/// an ancestor of it) has a **durable** (`Session`/`Always`) `read`-tool grant
/// in the store — a search never forces its own approval prompt, it only rides
/// a grant a `read`/`edit`/`write` call already earned. `None` (or no matching
/// grant) reduces to the strict pre-#482 drop.
pub fn list_files_with_extra_roots(
    root: &Path,
    pattern: &str,
    excludes: &[String],
    extra_roots: Option<&crate::extra_roots::ExtraRootStore>,
) -> Result<FileList> {
    let abs = root.join(pattern).to_string_lossy().into_owned();
    let entries = ::glob::glob(&abs).with_context(|| format!("invalid glob: {pattern}"))?;
    let exclude_patterns: Vec<::glob::Pattern> = excludes
        .iter()
        .map(|p| ::glob::Pattern::new(p).with_context(|| format!("invalid exclude pattern: {p}")))
        .collect::<Result<_>>()?;
    // Containment (#163): a `..` or absolute `pattern` makes the glob walk
    // outside root, and a symlink under root resolves elsewhere — route glob/
    // grep through the same boundary as read/write by dropping any entry whose
    // canonical path escapes the canonical root (ADR-0054), unless a durable
    // extra-root grant widens it (#482, above).
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut list = FileList::default();
    for entry in entries {
        let p = match entry {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(?err, pattern, "glob entry skipped");
                list.skipped_errors += 1;
                continue;
            }
        };
        if p.components().any(|c| c.as_os_str() == ".git") {
            continue;
        }
        if !exclude_patterns.is_empty() {
            let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy();
            if exclude_patterns.iter().any(|pat| pat.matches(&rel)) {
                continue;
            }
        }
        let canon = p.canonicalize().ok();
        let contained = canon.as_ref().is_some_and(|c| c.starts_with(&canon_root));
        if !contained {
            let widened = canon.as_ref().is_some_and(|c| {
                extra_roots.is_some_and(|store| store.is_durably_allowed_under("read", c))
            });
            if !widened {
                continue;
            }
        }
        match std::fs::metadata(&p) {
            Ok(m) if m.is_file() => {
                list.files.push(p);
                if list.files.len() >= MAX_RESULTS {
                    break;
                }
            }
            Ok(m) if m.is_dir() => list.matched_dirs += 1,
            _ => {}
        }
    }
    Ok(list)
}
