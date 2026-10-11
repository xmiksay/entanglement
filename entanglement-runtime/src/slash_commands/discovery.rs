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
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading command file {}", path.display()))?;
        raws.push(RawCommand {
            layer,
            strictness,
            name: command_name(dir, &path),
            source: path.display().to_string(),
            content,
        });
    }
    Ok(())
}

/// `<dir>/frontend/review.md` → `frontend:review`.
pub(super) fn command_name(dir: &Path, file: &Path) -> String {
    let rel = file.strip_prefix(dir).unwrap_or(file).with_extension("");
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_relative_path_with_colon_namespaces() {
        let dir = Path::new("/p/.entanglement/commands");
        assert_eq!(command_name(dir, &dir.join("review.md")), "review");
        assert_eq!(
            command_name(dir, &dir.join("frontend").join("review.md")),
            "frontend:review"
        );
        assert_eq!(
            command_name(dir, &dir.join("a").join("b").join("c.md")),
            "a:b:c"
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
