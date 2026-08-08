//! [`list_files`] — the shared glob-walk + root-containment engine behind
//! `glob`/`grep` (ADR-0008; symlink-safe containment ADR-0054/#163; the
//! escape-root search-widening path #482/[ADR-0132](../../../docs/adr/0132-glob-grep-escape-root-search-via-durable-grant.md);
//! CLI-shaped ergonomics — brace sets, directory auto-expansion, cap/out-of-root
//! reporting — [ADR-0150](../../../docs/adr/0150-search-tool-cli-ergonomics.md);
//! `.gitignore` awareness — [ADR-0170](../../../docs/adr/0170-gitignore-aware-glob-grep-walk.md),
//! single-pass + bounded since #678).
//! Split out of `host/mod.rs` to stay under the 400-line file cap (#451).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::WalkBuilder;

use super::brace::expand_braces;

/// Cap on how many paths `glob` returns and how many matches `grep` reports —
/// bounds the output for pathologically large trees.
pub(crate) const MAX_RESULTS: usize = 1000;

/// Cap on how many directory entries a single walk will *visit* (matching or
/// not) before giving up. [`MAX_RESULTS`] bounds the output, but a pattern
/// matching almost nothing in a huge tree bounds no work at all — the TUI
/// froze on a black screen for exactly that shape (#678: `**/*` over a
/// workspace root). Ignored subtrees are pruned before they cost anything, so
/// only real, walkable entries count against this.
pub(crate) const MAX_SCANNED: usize = 100_000;

/// Result of [`list_files`]: the matched files plus enough metadata for the
/// caller to distinguish "no match at all" from "matched only directories",
/// "every entry errored", "walk hit the cap", or "every match fell outside the
/// root". Without those distinctions a zero-result walk looks identical to a
/// typo, and the model has no way to self-correct — see ADR-0016/ADR-0150.
#[derive(Debug, Default)]
pub struct FileList {
    /// Files (in walk order), already capped at [`MAX_RESULTS`].
    pub files: Vec<PathBuf>,
    /// Entries the pattern matched but that were directories (filtered out).
    pub matched_dirs: usize,
    /// Entries the walker yielded as `Err` (permissions, IO, etc.).
    pub skipped_errors: usize,
    /// True iff the walk stopped early because `files` hit [`MAX_RESULTS`].
    pub capped: bool,
    /// Matches dropped because their canonical path escapes the root with no
    /// durable grant covering them (ADR-0054/ADR-0132) — surfaced so an
    /// absolute-path typo doesn't read as a clean no-match (ADR-0150).
    pub out_of_root: usize,
    /// True iff the walk stopped early because it visited [`MAX_SCANNED`]
    /// entries — the hard backstop for huge non-ignored trees (#678).
    pub scan_capped: bool,
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
/// never visited at all: the traversal is the `ignore` crate's own walker
/// (ripgrep's engine), which prunes ignored directories before descending
/// (#629/[ADR-0170](../../../docs/adr/0170-gitignore-aware-glob-grep-walk.md);
/// single-pass since #678 — the earlier two-pass shape eagerly enumerated the
/// whole tree first, unbounded). Skips directories (counted in
/// [`FileList::matched_dirs`]) and logs unreadable entries as `warn!`
/// (counted in [`FileList::skipped_errors`]) instead of silently dropping
/// them. Excluded/ignored entries are dropped before either count, so an
/// excluded or `.gitignore`d subtree looks to the caller like it was never in
/// the walk at all. Bounds the output at [`MAX_RESULTS`] files
/// ([`FileList::capped`]) and the traversal itself at [`MAX_SCANNED`] visited
/// entries ([`FileList::scan_capped`]).
///
/// Two CLI-shaped rewrites happen before globbing (ADR-0150): brace sets
/// expand (`**/*.{rs,md}` walks both alternatives, deduped against overlap;
/// `excludes` entries expand the same way), and a metachar-free pattern naming
/// an existing directory expands to `dir/**/*` — the `grep -r`-style shape
/// most real calls intend. Directory expansion is containment-gated: the
/// directory's canonical path must sit under the canonical root or carry a
/// durable grant, or the pattern is left alone.
///
/// `extra_roots` widens containment for `glob`/`grep` (#482,
/// [ADR-0132](../../../docs/adr/0132-glob-grep-escape-root-search-via-durable-grant.md)):
/// a match whose canonical path escapes `root` is still admitted when it (or
/// an ancestor of it) has a **durable** (`Session`/`Always`) `read`-tool grant
/// in the store — a search never forces its own approval prompt, it only rides
/// a grant a `read`/`edit`/`write` call already earned. `None` (or no matching
/// grant) reduces to the strict pre-#482 drop, now counted in
/// [`FileList::out_of_root`]. A walk rooted outside `root` via such a grant
/// disables `.gitignore` filtering entirely: the grant is a deliberate, narrow
/// escape hatch, and the granted directory's own `.gitignore` must not
/// silently hide files the grant was specifically approved to expose.
pub fn list_files_with_extra_roots(
    root: &Path,
    pattern: &str,
    excludes: &[String],
    extra_roots: Option<&crate::extra_roots::ExtraRootStore>,
) -> Result<FileList> {
    list_files_bounded(root, pattern, excludes, extra_roots, MAX_SCANNED)
}

/// [`list_files_with_extra_roots`] with an explicit scan budget — the real
/// implementation; split out so tests can exercise budget exhaustion without
/// creating 100k files.
pub(crate) fn list_files_bounded(
    root: &Path,
    pattern: &str,
    excludes: &[String],
    extra_roots: Option<&crate::extra_roots::ExtraRootStore>,
    max_scanned: usize,
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
    // Containment (#163): a `..` or absolute `pattern` makes the walk go
    // outside root, and a symlink under root resolves elsewhere — route glob/
    // grep through the same boundary as read/write by dropping any entry whose
    // canonical path escapes the canonical root (ADR-0054), unless a durable
    // extra-root grant widens it (#482, above).
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    // The glob-crate iterator matched component-by-component, so a `*` never
    // crossed a `/`; matching the whole path at once needs the literal-
    // separator option to keep that. Leading dots stay matchable (the
    // iterator's default, pinned by the dotfile test).
    let match_options = ::glob::MatchOptions {
        require_literal_separator: true,
        ..::glob::MatchOptions::new()
    };
    let mut list = FileList::default();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut scanned: usize = 0;
    'alternatives: for alt in expand_braces(pattern)? {
        let effective = expand_dir_pattern(root, &canon_root, &alt, extra_roots);
        let abs = root.join(&effective);
        let compiled = ::glob::Pattern::new(&abs.to_string_lossy())
            .with_context(|| format!("invalid glob: {pattern}"))?;
        // Bare-`**` trap (ADR-0016/ADR-0150): the glob iterator yielded only
        // directories for a pattern whose final component is `**`; the
        // compiled pattern matches files under them too, so filter to dirs.
        let dirs_only = effective
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .is_some_and(|last| last == "**");
        let Some(walk_root) = walk_root_of(&abs) else {
            // Nonexistent literal prefix: glob parity — a clean no-match, not
            // an error entry.
            continue;
        };
        let canon_walk = walk_root
            .canonicalize()
            .unwrap_or_else(|_| walk_root.clone());
        let mut builder = WalkBuilder::new(&walk_root);
        // `follow_links(true)` mirrors the old glob-crate walk so an
        // out-of-root escape through a symlink still reaches (and is counted
        // by) the containment check below instead of silently vanishing; the
        // `ignore` crate detects symlink loops, which `glob`'s `**` never did.
        // `hidden(false)`: dotfiles were always enumerable — only `.git` and
        // `.gitignore` rules filter anything. `.git` is pruned before it can
        // burn scan budget (dropped unconditionally anyway, ADR-0099).
        builder
            .follow_links(true)
            .hidden(false)
            .sort_by_file_name(|a, b| a.cmp(b))
            .filter_entry(|e| e.file_name() != ".git");
        if canon_walk.starts_with(&canon_root) {
            // `require_git(false)`: honor `.gitignore` even before `git init`.
            builder.require_git(false);
        } else {
            // Walking a granted external directory: no `.gitignore` there.
            builder.standard_filters(false);
        }
        for entry in builder.build() {
            if scanned >= max_scanned {
                list.scan_capped = true;
                break 'alternatives;
            }
            scanned += 1;
            let p = match entry {
                Ok(e) => e.into_path(),
                Err(err) => {
                    tracing::warn!(?err, pattern, "walk entry skipped");
                    list.skipped_errors += 1;
                    continue;
                }
            };
            if !compiled.matches_path_with(&p, match_options) {
                continue;
            }
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
                Ok(m) if m.is_file() && !dirs_only => {
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

/// The deepest metachar-free prefix of `abs` — where the walk starts, so
/// `src/**/*.rs` walks `root/src` (not `root`) and an absolute granted
/// pattern like `/granted/dir/**/*` walks `/granted/dir`. `None` when that
/// prefix doesn't exist on disk: the whole alternative matches nothing.
fn walk_root_of(abs: &Path) -> Option<PathBuf> {
    let mut prefix = PathBuf::new();
    for component in abs.components() {
        let s = component.as_os_str().to_string_lossy();
        if s.contains(['*', '?', '[']) {
            break;
        }
        prefix.push(component);
    }
    if prefix.symlink_metadata().is_ok() {
        Some(prefix)
    } else {
        None
    }
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
