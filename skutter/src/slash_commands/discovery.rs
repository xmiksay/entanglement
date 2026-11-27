//! Filesystem discovery of slash-command files (#733): every `*.md` under each
//! layered `commands` dir, named by its path relative to that dir —
//! `frontend/review.md` is `frontend:review` (Claude Code's namespace
//! separator, ADR-0221).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::layers::{Layer, Strictness, UserLayer};

/// Env var replacing the whole user commands layer (tests, non-XDG setups, and
/// the cross-vendor opt-out — same contract as `ENTANGLEMENT_SKILLS_DIR`).
pub const COMMANDS_DIR_ENV: &str = "ENTANGLEMENT_COMMANDS_DIR";

/// A discovered command file *before* parsing.
#[derive(Debug)]
pub(super) struct RawCommand {
    pub(super) layer: Layer,
    pub(super) strictness: Strictness,
    pub(super) name: String,
    pub(super) source: String,
    pub(super) content: String,
}

/// Every command file in precedence order — user dirs, then project dirs
/// (there are no embedded built-ins). Later entries win on a name collision.
pub(super) fn discover(root: &Path, user: &UserLayer) -> Result<Vec<RawCommand>> {
    crate::layers::load_layers(
        root,
        "commands",
        COMMANDS_DIR_ENV,
        user,
        Vec::new(),
        discover_dir,
    )
}

/// Append every `*.md` under `dir` (recursively), tagged with `layer`. A
/// missing dir is fine; an unreadable dir or file is an error.
pub(super) fn discover_dir(
    layer: Layer,
    dir: &Path,
    strictness: Strictness,
    raws: &mut Vec<RawCommand>,
) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let mut found: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    crate::layers::walk_files(dir, &mut found, &mut seen, &|p| {
        p.extension().and_then(|e| e.to_str()) == Some("md")
    })?;
    // Sorted so a collision within one layer resolves deterministically.
    found.sort();
    for path in found {
        let Some(name) = command_name(dir, &path) else {
            tracing::warn!(
                source = %path.display(),
                "skipping command file: `:` is the namespace separator, so a \
                 path component containing it would collide with a subdirectory \
                 command (`a:b.md` vs `a/b.md`) — rename it"
            );
            continue;
        };
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading command file {}", path.display()))?;
        raws.push(RawCommand {
            layer,
            strictness,
            name,
            source: path.display().to_string(),
            content,
        });
    }
    Ok(())
}

/// `<dir>/frontend/review.md` → `frontend:review`. `None` when a path
/// component itself contains `:` (#798): `a:b.md` would otherwise yield the
/// same name as `a/b.md`, the later sorted path silently winning.
pub(super) fn command_name(dir: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(dir).unwrap_or(file).with_extension("");
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    (!parts.iter().any(|p| p.contains(':'))).then(|| parts.join(":"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_relative_path_with_colon_namespaces() {
        let dir = Path::new("/p/.entanglement/commands");
        assert_eq!(
            command_name(dir, &dir.join("review.md")).as_deref(),
            Some("review")
        );
        assert_eq!(
            command_name(dir, &dir.join("frontend").join("review.md")).as_deref(),
            Some("frontend:review")
        );
        assert_eq!(
            command_name(dir, &dir.join("a").join("b").join("c.md")).as_deref(),
            Some("a:b:c")
        );
    }

    #[test]
    fn only_markdown_files_are_discovered_recursively() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("ns")).unwrap();
        std::fs::write(dir.path().join("a.md"), "A").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "skip").unwrap();
        std::fs::write(dir.path().join("ns").join("b.md"), "B").unwrap();
        let mut raws = Vec::new();
        discover_dir(Layer::Project, dir.path(), Strictness::Strict, &mut raws).unwrap();
        let names: Vec<&str> = raws.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["a", "ns:b"]);
    }

    #[test]
    fn a_colon_in_a_file_or_dir_name_is_skipped_not_colliding() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        std::fs::create_dir_all(dir.path().join("x:y")).unwrap();
        std::fs::write(dir.path().join("a").join("b.md"), "nested").unwrap();
        std::fs::write(dir.path().join("a:b.md"), "colon").unwrap();
        std::fs::write(dir.path().join("x:y").join("z.md"), "colon dir").unwrap();
        let mut raws = Vec::new();
        discover_dir(Layer::Project, dir.path(), Strictness::Strict, &mut raws).unwrap();
        let found: Vec<(&str, &str)> = raws
            .iter()
            .map(|r| (r.name.as_str(), r.content.as_str()))
            .collect();
        assert_eq!(found, vec![("a:b", "nested")]);
    }

    #[test]
    fn missing_dir_is_empty() {
        let mut raws = Vec::new();
        discover_dir(
            Layer::User,
            Path::new("/no/such/commands/dir"),
            Strictness::Strict,
            &mut raws,
        )
        .unwrap();
        assert!(raws.is_empty());
    }
}
