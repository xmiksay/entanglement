//! Minimal unified-diff parser + applier backing the `apply_patch` host tool
//! (#455). Deliberately hand-rolled instead of pulling in the `diffy` crate:
//! `diffy` is already a workspace dependency but is `tui`-feature-gated and
//! named in `LEAN_FORBIDDEN` (`Makefile`), and `apply_patch` lives in
//! `entanglement-runtime::host` — unconditional lean-library code alongside
//! `edit`/`write` — so pulling it in would either break `make check-lean` or
//! require relitigating the ADR-0025 lean-library boundary for a single tool.
//!
//! Matching is deliberately **not fuzzy**: a hunk's declared `@@ -oldStart`
//! position (offset by the net line-count delta of hunks already applied in
//! this same patch) is the *only* position tried. If the context/deleted
//! lines don't match exactly there, the whole patch is rejected before any
//! write — no alternate-position search (unlike `patch`/`git apply`'s offset
//! hunting). Keeps v1 predictable: a mismatch is always the caller's stale
//! view of the file, never a silent wrong-place apply.

use anyhow::{Context, Result};

/// One line inside a hunk body, marker stripped.
#[derive(Debug)]
enum HunkLine {
    Context(String),
    Delete(String),
    Insert(String),
}

/// One `@@ -oldStart,oldLines +newStart,newLines @@` block plus its body
/// lines. Only `old_start` is needed to place the hunk; line counts are
/// re-derived from the body itself when applying.
#[derive(Debug)]
pub struct Hunk {
    old_start: usize,
    lines: Vec<HunkLine>,
}

/// Parse `patch` (unified-diff text) into its hunks. Lines before the first
/// `@@` are treated as the optional `---`/`+++ ` file header and skipped;
/// `\ No newline at end of file` markers are informational and dropped. Any
/// other malformed line, or a patch with no hunks at all, is a hard error.
pub fn parse_hunks(patch: &str) -> Result<Vec<Hunk>> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<Hunk> = None;
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("@@ ") {
            if let Some(h) = current.take() {
                hunks.push(h);
            }
            current = Some(Hunk {
                old_start: parse_hunk_header(rest)?,
                lines: Vec::new(),
            });
            continue;
        }
        let Some(hunk) = current.as_mut() else {
            if line.starts_with("---") || line.starts_with("+++") || line.trim().is_empty() {
                continue;
            }
            anyhow::bail!("malformed patch: expected a hunk header (@@ ...), found: {line}");
        };
        if let Some(rest) = line.strip_prefix(' ') {
            hunk.lines.push(HunkLine::Context(rest.to_string()));
        } else if let Some(rest) = line.strip_prefix('+') {
            hunk.lines.push(HunkLine::Insert(rest.to_string()));
        } else if let Some(rest) = line.strip_prefix('-') {
            hunk.lines.push(HunkLine::Delete(rest.to_string()));
        } else if line.starts_with('\\') {
            // "\ No newline at end of file" — informational only.
        } else {
            anyhow::bail!("malformed patch: unexpected line (missing ' '/'+'/'-' marker): {line}");
        }
    }
    if let Some(h) = current.take() {
        hunks.push(h);
    }
    if hunks.is_empty() {
        anyhow::bail!("patch contains no hunks");
    }
    Ok(hunks)
}

/// Parse the body of a hunk header after the leading `"@@ "` — `-oldStart[,oldLen]
/// +newStart[,newLen] @@` (optional trailing function-context text, as `git
/// diff` sometimes appends, is ignored). Only `oldStart` is extracted.
fn parse_hunk_header(rest: &str) -> Result<usize> {
    let old_part = rest
        .split_whitespace()
        .next()
        .with_context(|| "malformed hunk header: missing old-range".to_string())?;
    let old_start = old_part
        .strip_prefix('-')
        .with_context(|| format!("malformed hunk header: expected -oldStart, got: {old_part}"))?
        .split(',')
        .next()
        .unwrap_or("");
    old_start
        .parse::<usize>()
        .with_context(|| format!("malformed hunk header: non-numeric old-start: {old_start}"))
}

/// Apply `hunks` to `content` in order, splicing each hunk's post-image
/// (context + inserted lines) in place of its pre-image (context + deleted
/// lines). A hunk's start position is its declared `old_start` shifted by the
/// net line-count delta of every hunk applied before it — the running splice
/// naturally keeps later hunks aligned. Returns a hard error, leaving the
/// input untouched, the moment any hunk's pre-image doesn't match exactly.
pub fn apply_hunks(content: &str, hunks: &[Hunk]) -> Result<String> {
    let had_trailing_newline = content.is_empty() || content.ends_with('\n');
    let mut lines: Vec<&str> = content.lines().collect();
    let mut offset: isize = 0;

    for (i, hunk) in hunks.iter().enumerate() {
        let pre: Vec<&str> = hunk
            .lines
            .iter()
            .filter_map(|l| match l {
                HunkLine::Context(s) | HunkLine::Delete(s) => Some(s.as_str()),
                HunkLine::Insert(_) => None,
            })
            .collect();
        let post: Vec<&str> = hunk
            .lines
            .iter()
            .filter_map(|l| match l {
                HunkLine::Context(s) | HunkLine::Insert(s) => Some(s.as_str()),
                HunkLine::Delete(_) => None,
            })
            .collect();

        let start = (hunk.old_start as isize - 1 + offset).max(0) as usize;
        let end = start + pre.len();
        let actual = lines.get(start..end).with_context(|| {
            format!(
                "hunk {} context mismatch: file has {} line(s), hunk expects {} at line {}",
                i + 1,
                lines.len(),
                pre.len(),
                hunk.old_start
            )
        })?;
        if actual != pre.as_slice() {
            anyhow::bail!(
                "hunk {} context does not match file content at line {}",
                i + 1,
                hunk.old_start
            );
        }

        let post_len = post.len();
        lines.splice(start..end, post);
        offset += post_len as isize - pre.len() as isize;
    }

    let mut out = lines.join("\n");
    if had_trailing_newline && !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_hunk() {
        let patch = "--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n";
        let hunks = parse_hunks(patch).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[0].lines.len(), 4);
    }

    #[test]
    fn parses_multiple_hunks() {
        let patch = "\
@@ -1,2 +1,2 @@
-a
+A
 b
@@ -10,2 +10,2 @@
-x
+X
 y
";
        let hunks = parse_hunks(patch).unwrap();
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[1].old_start, 10);
    }

    #[test]
    fn rejects_patch_with_no_hunks() {
        let err = parse_hunks("--- a/f\n+++ b/f\n").unwrap_err();
        assert!(format!("{err}").contains("no hunks"));
    }

    #[test]
    fn rejects_malformed_body_line() {
        let err = parse_hunks("@@ -1,1 +1,1 @@\n*oops\n").unwrap_err();
        assert!(format!("{err}").contains("malformed patch"));
    }

    #[test]
    fn applies_single_hunk() {
        let content = "alpha\nbeta\ngamma\n";
        let hunks = parse_hunks("@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n").unwrap();
        let out = apply_hunks(content, &hunks).unwrap();
        assert_eq!(out, "alpha\nBETA\ngamma\n");
    }

    #[test]
    fn applies_multiple_hunks_with_shifting_offsets() {
        let content = "one\ntwo\nthree\nfour\nfive\n";
        let patch = "\
@@ -1,2 +1,3 @@
 one
+ONE_AND_A_HALF
 two
@@ -4,2 +5,2 @@
-four
+FOUR
 five
";
        let hunks = parse_hunks(patch).unwrap();
        let out = apply_hunks(content, &hunks).unwrap();
        assert_eq!(out, "one\nONE_AND_A_HALF\ntwo\nthree\nFOUR\nfive\n");
    }

    #[test]
    fn context_mismatch_errors_without_partial_result() {
        let content = "alpha\nbeta\ngamma\n";
        let hunks = parse_hunks("@@ -1,3 +1,3 @@\n alpha\n-WRONG\n+BETA\n gamma\n").unwrap();
        let err = apply_hunks(content, &hunks).unwrap_err();
        assert!(format!("{err}").contains("context does not match"));
    }

    #[test]
    fn out_of_range_hunk_errors() {
        let content = "alpha\n";
        let hunks = parse_hunks("@@ -10,1 +10,1 @@\n-alpha\n+ALPHA\n").unwrap();
        let err = apply_hunks(content, &hunks).unwrap_err();
        assert!(format!("{err}").contains("context mismatch"));
    }

    #[test]
    fn insert_into_empty_file() {
        let hunks = parse_hunks("@@ -0,0 +1,2 @@\n+first\n+second\n").unwrap();
        let out = apply_hunks("", &hunks).unwrap();
        assert_eq!(out, "first\nsecond\n");
    }

    #[test]
    fn preserves_missing_trailing_newline() {
        let content = "alpha\nbeta";
        let hunks = parse_hunks("@@ -1,2 +1,2 @@\n alpha\n-beta\n+BETA\n").unwrap();
        let out = apply_hunks(content, &hunks).unwrap();
        assert_eq!(out, "alpha\nBETA");
    }
}
