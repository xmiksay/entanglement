//! [`list_files`] — the shared glob-walk + root-containment engine behind
//! `glob`/`grep` (ADR-0008; symlink-safe containment ADR-0054/#163; the
//! escape-root search-widening path #482/[ADR-0132](../../../docs/adr/0132-glob-grep-escape-root-search-via-durable-grant.md);
//! CLI-shaped ergonomics — brace sets, directory auto-expansion, cap/out-of-root
//! reporting — [ADR-0150](../../../docs/adr/0150-search-tool-cli-ergonomics.md);
//! `.gitignore` awareness — [ADR-0170](../../../docs/adr/0170-gitignore-aware-glob-grep-walk.md)).
//! Split out of `host/mod.rs` to stay under the 400-line file cap (#451).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::WalkBuilder;

use super::brace::expand_braces;

/// Cap on how many paths `glob` returns and how many matches `grep` reports —
/// bounds the work + output for pathologically large trees.
pub(crate) const MAX_RESULTS: usize = 1000;

/// Result of [`list_files`]: the matched files plus enough metadata for the
/// caller to distinguish "no match at all" from "matched only directories",
/// "every entry errored", "walk hit the cap", or "every match fell outside the
/// root". Without those distinctions a zero-result walk looks identical to a
/// typo, and the model has no way to self-correct — see ADR-0016/ADR-0150.
#[derive(Debug, Default)]
pub struct FileList {
    /// Files (in arbitrary glob-walk order), already capped at [`MAX_RESULTS`].
    pub files: Vec<PathBuf>,
    /// Entries the pattern matched but that were directories (filtered out).
    pub matched_dirs: usize,
    /// Entries the glob iterator yielded as `Err` (permissions, IO, etc.).
    pub skipped_errors: usize,
    /// True iff the walk stopped early because `files` hit [`MAX_RESULTS`].
    pub capped: bool,
    /// Matches dropped because their canonical path escapes the root with no
    /// durable grant covering them (ADR-0054/ADR-0132) — surfaced so an
    /// absolute-path typo doesn't read as a clean no-match (ADR-0150).
    pub out_of_root: usize,
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
/// `excludes` list (ADR-0099). A match ignored by `.gitignore` (root or
/// nested, plus `.git/info/exclude` and the user's global excludes file) is
/// dropped the same unconditional way — see [`gitignore_allowed_paths`]
/// ([ADR-0170](../../../docs/adr/0170-gitignore-aware-glob-grep-walk.md)).
/// Skips directories (counted in [`FileList::matched_dirs`]) and logs
/// unreadable entries as `warn!` (counted in [`FileList::skipped_errors`])
/// instead of silently dropping them. Excluded/ignored entries are dropped
/// before either count, so an excluded or `.gitignore`d subtree looks to the
/// caller like it was never in the walk at all. Bounds the walk at
/// [`MAX_RESULTS`] files, recording the early stop in [`FileList::capped`].
///
/// Two CLI-shaped rewrites happen before globbing (ADR-0150): brace sets
/// expand (`**/*.{rs,md}` walks both alternatives, deduped against overlap;
/// `excludes` entries expand the same way), and a metachar-free pattern naming
/// an existing directory expands to `dir/**/*` — the `grep -r`-style shape
/// most real calls intend. Directory expansion is containment-gated: the
/// directory's canonical path must sit under the canonical root or carry a
/// durable grant, or the pattern is left alone (expanding an ungranted
/// absolute directory like `/` would walk the whole filesystem, since
/// out-of-root entries never count toward the cap).
///
/// `extra_roots` widens containment for `glob`/`grep` (#482,
/// [ADR-0132](../../../docs/adr/0132-glob-grep-escape-root-search-via-durable-grant.md)):
/// a match whose canonical path escapes `root` is still admitted when it (or
/// an ancestor of it) has a **durable** (`Session`/`Always`) `read`-tool grant
/// in the store — a search never forces its own approval prompt, it only rides
/// a grant a `read`/`edit`/`write` call already earned. `None` (or no matching
/// grant) reduces to the strict pre-#482 drop, now counted in
/// [`FileList::out_of_root`].
pub fn list_files_with_extra_roots(
    root: &Path,
    pattern: &str,
    excludes: &[String],
    extra_roots: Option<&crate::extra_roots::ExtraRootStore>,
) -> Result<FileList> {
    let mut exclude_patterns: Vec<::glob::Pattern> = Vec::new();
    for entry in excludes {
        for alt in expand_braces(entry)? {
            exclude_patterns.push(
                ::glob::Pattern::new(&alt)
                    .with_context(|| format!("invalid exclude pattern: {entry}"))?,
            );
        }
    }
    // Containment (#163): a `..` or absolute `pattern` makes the glob walk
    // outside root, and a symlink under root resolves elsewhere — route glob/
    // grep through the same boundary as read/write by dropping any entry whose
    // canonical path escapes the canonical root (ADR-0054), unless a durable
    // extra-root grant widens it (#482, above).
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let allowed = gitignore_allowed_paths(root);
    let mut list = FileList::default();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    'alternatives: for alt in expand_braces(pattern)? {
        let effective = expand_dir_pattern(root, &canon_root, &alt, extra_roots);
        let abs = root.join(&effective).to_string_lossy().into_owned();
        let entries = ::glob::glob(&abs).with_context(|| format!("invalid glob: {pattern}"))?;
        for entry in entries {
            let p = match entry {
                Ok(p) => p,
                Err(err) => {
                    tracing::warn!(?err, pattern, "glob entry skipped");
                    list.skipped_errors += 1;
                    continue;
                }
            };
            if !seen.insert(p.clone()) {
                continue;
            }
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
            // `.gitignore` filtering only applies to matches actually inside
            // `root` (what `allowed` was walked from) — an extra-root grant is
            // a deliberate, narrow escape hatch onto an unrelated directory,
            // and applying its own `.gitignore` there would silently hide
            // files the grant was specifically approved to expose.
            if contained && !allowed.contains(&p) {
                continue;
            }
            if !contained {
                let widened = canon.as_ref().is_some_and(|c| {
                    extra_roots.is_some_and(|store| store.is_durably_allowed_under("read", c))
                });
                if !widened {
                    list.out_of_root += 1;
                    continue;
                }
            }
            match std::fs::metadata(&p) {
                Ok(m) if m.is_file() => {
                    list.files.push(p);
                    if list.files.len() >= MAX_RESULTS {
                        list.capped = true;
                        break 'alternatives;
                    }
                }
                Ok(m) if m.is_dir() => list.matched_dirs += 1,
                _ => {}
            }
        }
    }
    Ok(list)
}

/// The set of paths under `root` that survive `.gitignore` filtering — every
/// path the `ignore` crate's own walker (ripgrep's engine) would keep, given
/// root and nested `.gitignore` files, `.git/info/exclude`, and the user's
/// global excludes file (`core.excludesFile`). `glob`/`grep` used to have no
/// `.gitignore` awareness at all: `target/`/`node_modules/` were enumerated
/// like any other tree, consuming the [`MAX_RESULTS`] walk budget and
/// polluting results unless a caller passed `exclude` manually (#629,
/// [ADR-0170](../../../docs/adr/0170-gitignore-aware-glob-grep-walk.md); named
/// as the seam by [ADR-0008](../../../docs/adr/0008-host-tools-workdir-and-bounded-output.md)).
///
/// Walked independently of the `glob`-crate enumeration above (rather than
/// replacing it) so that walk's own quirks — the bare-`**` trap
/// (ADR-0016/ADR-0150), brace/dir-pattern expansion, containment, extra-root
/// widening — stay exactly as tested; this only decides set membership,
/// checked as an unconditional drop alongside the `.git` filter — but only for
/// matches actually contained under `root`. A match admitted through
/// extra-root widening (#482/ADR-0132) lives on a directory the caller was
/// specifically, narrowly granted access to; that grant is the whole point,
/// so its own `.gitignore` (if any) is not consulted.
/// `require_git(false)`: a `.gitignore` is honored even in a directory that
/// isn't (yet) a git repository — an agent shouldn't see it ignored simply
/// because `git init` hasn't run yet, and it keeps this walk from having to
/// search `root`'s ancestors for a repo boundary. `hidden(false)`: dotfiles
/// were always enumerable before (only `.git` and `.gitignore` rules filter
/// anything); this crate's own "skip hidden entries" heuristic is unrelated
/// to `.gitignore` and would otherwise start hiding them. `follow_links(true)`
/// mirrors the `glob`-crate walk so a symlinked path lands in both sets with
/// the same shape, and an out-of-root escape through one still reaches (and
/// is counted by) the containment check below instead of silently vanishing
/// here.
fn gitignore_allowed_paths(root: &Path) -> HashSet<PathBuf> {
    WalkBuilder::new(root)
        .follow_links(true)
        .hidden(false)
        .require_git(false)
        .build()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.into_path())
        .collect()
}

/// ADR-0150: a metachar-free pattern naming an existing directory expands to
/// `dir/**/*` — the `grep -r DIR` / Claude-Code shape the majority of real
/// calls intend (a directory filter matched zero *files* by construction
/// before, so no legitimate use is lost). Gated on containment: the canonical
/// directory must sit under `canon_root` or carry a durable `read` grant
/// (ADR-0132), or the pattern is returned untouched.
fn expand_dir_pattern(
    root: &Path,
    canon_root: &Path,
    pattern: &str,
    extra_roots: Option<&crate::extra_roots::ExtraRootStore>,
) -> String {
    if pattern.contains(['*', '?', '[', '{']) {
        return pattern.to_string();
    }
    let Ok(canon) = root.join(pattern).canonicalize() else {
        return pattern.to_string();
    };
    if !canon.is_dir() {
        return pattern.to_string();
    }
    let allowed = canon.starts_with(canon_root)
        || extra_roots.is_some_and(|s| s.is_durably_allowed_under("read", &canon));
    if !allowed {
        return pattern.to_string();
    }
    let base = pattern.trim_end_matches('/');
    if base.is_empty() {
        // "" (grep's absent filter never lands here — its default has
        // metachars) or "/": rebuilding with a leading slash would re-root
        // the walk at the filesystem root via `Path::join`.
        if pattern.starts_with('/') {
            "/**/*".to_string()
        } else {
            "**/*".to_string()
        }
    } else {
        format!("{base}/**/*")
    }
}
