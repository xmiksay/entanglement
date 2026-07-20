//! Filesystem discovery of `SKILL.md` files, split out of `mod.rs` (issue
//! #451): walking the layered dirs (embedded built-ins, user, project) into
//! raw `(layer, source, content)` records, independent of frontmatter parsing
//! (which stays in `mod.rs` next to [`super::SkillMeta`]).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::SkillLayer;
use crate::layers::Strictness;

/// Embedded stock skills, parsed through the same loader as user/project files.
/// `(filename, contents)` — the filename only feeds parse-error messages; the
/// skill's identity is its frontmatter `name`.
pub(super) const BUILT_INS: &[(&str, &str)] = &[("commit.md", include_str!("commit.md"))];

/// Env var overriding the user skills directory (tests + non-XDG setups).
pub(super) const SKILLS_DIR_ENV: &str = "ENTANGLEMENT_SKILLS_DIR";

/// The `SKILL.md` marker file that makes a directory a skill.
pub(super) const SKILL_FILE: &str = "SKILL.md";

/// A discovered `SKILL.md` *before* parsing: which layer it came from (#186), a
/// display label for its origin (`built-in (commit.md)` or the file path), the
/// pre-resolved `root_dir`, and the raw file content.
pub(super) struct RawSkill {
    pub(super) layer: SkillLayer,
    pub(super) strictness: Strictness,
    pub(super) source: String,
    pub(super) root_dir: Option<PathBuf>,
    pub(super) content: String,
}

/// Enumerate every `SKILL.md` in precedence order — embedded built-ins, then the
/// user dir, then the project dir — without parsing. Later entries win on a
/// `name` collision, so consumers keep the last match. A missing dir is fine; an
/// unreadable dir or file is an error. A missing *explicit*
/// `ENTANGLEMENT_SKILLS_DIR` override is warned by [`crate::layers::load_layers`].
pub(super) fn discover(root: &Path) -> Result<Vec<RawSkill>> {
    let built_ins: Vec<RawSkill> = BUILT_INS
        .iter()
        .map(|(file, contents)| RawSkill {
            // Embedded built-ins are guarded by `built_ins_parse`, so a parse
            // failure downstream is a build-time bug, not a runtime condition.
            layer: SkillLayer::BuiltIn,
            strictness: Strictness::Strict,
            source: format!("built-in ({file})"),
            root_dir: None,
            content: (*contents).to_string(),
        })
        .collect();
    crate::layers::load_layers(root, "skills", SKILLS_DIR_ENV, built_ins, discover_dir)
}

/// Append every `SKILL.md` under `dir` (recursively), tagged with `layer`, to
/// `raws`. A missing dir is fine; an unreadable dir or file is an error.
/// Symlinked duplicates (a link to an already-seen file, or a directory cycle)
/// are deduped by canonical path.
fn discover_dir(
    layer: SkillLayer,
    dir: &Path,
    strictness: Strictness,
    raws: &mut Vec<RawSkill>,
) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let mut found: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    walk(dir, &mut found, &mut seen)?;
    // Sort for deterministic collision resolution within a single layer.
    found.sort();
    for skill_md in found {
        let content = std::fs::read_to_string(&skill_md)
            .with_context(|| format!("reading skill file {}", skill_md.display()))?;
        // `root_dir` is the SKILL.md's directory — resolved once, here.
        let root_dir = skill_md.parent().map(Path::to_path_buf);
        raws.push(RawSkill {
            layer,
            strictness,
            source: skill_md.display().to_string(),
            root_dir,
            content,
        });
    }
    Ok(())
}

/// Recursively collect `SKILL.md` paths under `dir`. `seen` holds canonicalized
/// paths of already-visited files *and* directories: it dedupes symlinked
/// duplicate files and breaks symlink directory cycles. `std::fs::metadata`
/// follows symlinks so a linked directory is still traversed once.
pub(super) fn walk(
    dir: &Path,
    found: &mut Vec<PathBuf>,
    seen: &mut HashSet<PathBuf>,
) -> Result<()> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("reading skills dir {}", dir.display()))?;
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("reading an entry under {}", dir.display()))?
            .path();
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            // A broken symlink (or a racing removal) is skipped, not fatal — but
            // no longer silent (#186): a dangling link to a skill dir would drop
            // the skill with zero signal, so log it at `warn!`.
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "skipping unreadable skills entry (broken symlink or racing removal)",
                );
                continue;
            }
        };
        if meta.is_dir() {
            subdirs.push(path);
        } else if meta.is_file() && path.file_name().and_then(|n| n.to_str()) == Some(SKILL_FILE) {
            let canon = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if seen.insert(canon) {
                found.push(path);
            }
        }
    }
    subdirs.sort();
    for sub in subdirs {
        let canon = std::fs::canonicalize(&sub).unwrap_or_else(|_| sub.clone());
        // A directory already seen (a symlink pointing back into the tree) is
        // not re-entered, so a cycle cannot loop forever.
        if seen.insert(canon) {
            walk(&sub, found, seen)?;
        }
    }
    Ok(())
}
