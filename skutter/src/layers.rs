//! Shared layered-definition discovery for the file-based registries (#204).
//!
//! The agent loader ([`crate::agents`]) and the skill loader ([`crate::skills`])
//! were byte-identical scaffolds modulo names: three precedence layers
//! (embedded built-in < user dir < project dir, later wins on a `name`
//! collision), a per-directory reader, and the candidate-dir policy. That
//! common machinery
//! lives here now; each loader supplies only its own `Raw*` item type and the
//! closure that reads one directory (agents read flat `*.md`, skills walk for
//! `SKILL.md`).
//!
//! Since ADR-0074 the user and project layers are each a *list* of candidate
//! directories, not a single path: cross-vendor locations (Claude Code's
//! `~/.claude/<kind>` and a project's `.claude/<kind>` / `.agents/<kind>`) are
//! scanned before the native ones (`${config_dir}/entanglement/<kind>`,
//! `.entanglement/<kind>`), so on a `name` collision native wins. Foreign dirs
//! are read [`Strictness::Lenient`] — files entanglement doesn't own must not
//! abort the load — while native dirs stay [`Strictness::Strict`].
//!
//! The library never resolves a user directory or reads an env var itself:
//! the embedder says where the user layer is ([`UserLayer`]; `skutter` maps
//! its `ENTANGLEMENT_*_DIR` overrides and the real home/config dirs onto it).
//! An explicit [`UserLayer::Dir`] replaces the *whole* user layer (foreign +
//! native), which doubles as the opt-out for cross-vendor discovery; one that
//! points at a missing path is `warn!`ed by [`load_layers`] rather than
//! silently swallowed — a default home/config dir staying absent is still
//! fine (the common "no user layer yet" case).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Which of the three precedence layers a definition came from. Ordered low →
/// high, so `built-in < user < project` matches discovery order and the
/// later-wins collision rule both loaders rely on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    BuiltIn,
    User,
    Project,
}

impl Layer {
    /// Short label for the `skutter inspect` tables and `replaces=` logs.
    pub fn label(self) -> &'static str {
        match self {
            Layer::BuiltIn => "built-in",
            Layer::User => "user",
            Layer::Project => "project",
        }
    }
}

/// How a candidate directory's files are parsed (ADR-0074). Native dirs are
/// `Strict`: `deny_unknown_fields` + a malformed file aborts the load — loud
/// feedback on files authored *for* entanglement. Foreign (cross-vendor) dirs
/// are `Lenient`: unknown frontmatter keys are ignored and a malformed file is
/// warned and skipped, because a file entanglement never owned (Claude Code
/// carries keys like `allowed-tools`, `model`, `color`) must not brick startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strictness {
    Strict,
    Lenient,
}

/// The ordered candidate directories for `kind` (`agents`/`skills`), lowest
/// precedence first. Pure, so the path policy is unit-testable.
///
/// - [`UserLayer::Dir`] ⇒ the user layer is exactly that dir (strict),
///   foreign user dirs included in the replacement.
/// - [`UserLayer::Home`] ⇒ `<home>/.claude/<kind>` (lenient) then
///   `<config>/entanglement/<kind>` (strict).
/// - The project layer is `.claude/<kind>` (lenient) < `.agents/<kind>`
///   (lenient, cross-vendor beats vendor-specific — same tiebreak as the brief
///   chain in [`crate::system_prompt`]) < `.entanglement/<kind>` (strict).
/// - A kind opencode also reads (only `commands`, #733) adds opencode's own
///   dirs just below each native one, lenient: `<config>/opencode/<sub>` and
///   `.opencode/<sub>` for each of [`opencode_subdirs`].
pub fn candidate_dirs(
    root: &Path,
    kind: &str,
    user: &UserLayer,
) -> Vec<(Layer, PathBuf, Strictness)> {
    let mut dirs = Vec::new();
    match user {
        UserLayer::Empty => {}
        UserLayer::Dir(dir) => dirs.push((Layer::User, dir.clone(), Strictness::Strict)),
        UserLayer::Home { home, config } => {
            if let Some(home) = home {
                dirs.push((
                    Layer::User,
                    home.join(".claude").join(kind),
                    Strictness::Lenient,
                ));
            }
            if let Some(config) = config {
                for sub in opencode_subdirs(kind) {
                    dirs.push((
                        Layer::User,
                        config.join("opencode").join(sub),
                        Strictness::Lenient,
                    ));
                }
                dirs.push((
                    Layer::User,
                    config.join("entanglement").join(kind),
                    Strictness::Strict,
                ));
            }
        }
    }
    dirs.push((
        Layer::Project,
        root.join(".claude").join(kind),
        Strictness::Lenient,
    ));
    dirs.push((
        Layer::Project,
        root.join(".agents").join(kind),
        Strictness::Lenient,
    ));
    for sub in opencode_subdirs(kind) {
        dirs.push((
            Layer::Project,
            root.join(".opencode").join(sub),
            Strictness::Lenient,
        ));
    }
    dirs.push((
        Layer::Project,
        root.join(".entanglement").join(kind),
        Strictness::Strict,
    ));
    dirs
}

/// opencode's directory names for `kind`. Only slash commands are read from
/// opencode's tree; it documents the singular `command/` and newer releases
/// also accept the plural, so both are scanned (#733).
fn opencode_subdirs(kind: &str) -> &'static [&'static str] {
    match kind {
        "commands" => &["command", "commands"],
        _ => &[],
    }
}

/// Recursively collect the files under `dir` that `is_match` accepts, shared
/// by the skill (`SKILL.md`) and slash-command (`*.md`) walkers. `seen` holds
/// canonicalized paths of already-visited files *and* directories: it dedupes
/// symlinked duplicate files and breaks symlink directory cycles.
/// `std::fs::metadata` follows symlinks so a linked directory is still
/// traversed once.
pub fn walk_files(
    dir: &Path,
    found: &mut Vec<PathBuf>,
    seen: &mut HashSet<PathBuf>,
    is_match: &dyn Fn(&Path) -> bool,
) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading definitions dir {}", dir.display()))?;
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("reading an entry under {}", dir.display()))?
            .path();
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            // A broken symlink (or a racing removal) is skipped, not fatal — but
            // not silent either (#186): a dangling link would drop a definition
            // with zero signal.
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "skipping unreadable definitions entry (broken symlink or racing removal)",
                );
                continue;
            }
        };
        if meta.is_dir() {
            subdirs.push(path);
        } else if meta.is_file() && is_match(&path) {
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
            walk_files(&sub, found, seen, is_match)?;
        }
    }
    Ok(())
}

/// Where a loader's user layer comes from — the embedder's answer; the
/// library resolves no user directory itself.
#[derive(Debug, Clone, Default)]
pub enum UserLayer {
    /// No user layer: built-ins and the project only.
    #[default]
    Empty,
    /// Exactly this dir (strict) — `skutter`'s `ENTANGLEMENT_*_DIR`
    /// override, the cross-vendor opt-out, and the seam tests use to stay off
    /// the real user dirs (#773). A missing dir is an empty layer, `warn!`ed
    /// (#803) — a typo'd path must not go silent.
    Dir(PathBuf),
    /// The cross-vendor + native dirs under the user's home and config dirs
    /// (ADR-0074); see [`candidate_dirs`].
    Home {
        home: Option<PathBuf>,
        config: Option<PathBuf>,
    },
}

/// Discover layered definitions for `kind`, seeded with `built_ins`: read each
/// candidate dir from [`candidate_dirs`] in precedence order, appending that
/// layer's raw items to the accumulator. `read_dir(layer, dir, strictness,
/// &mut acc)` does the actual reading and is where a missing dir is treated as
/// empty; the only thing added here is the loud path — an explicit
/// [`UserLayer::Dir`] that does not exist is `warn!`ed rather than silently
/// skipped (#204).
pub fn load_layers<T>(
    root: &Path,
    kind: &str,
    user: &UserLayer,
    mut acc: Vec<T>,
    mut read_dir: impl FnMut(Layer, &Path, Strictness, &mut Vec<T>) -> Result<()>,
) -> Result<Vec<T>> {
    if let UserLayer::Dir(dir) = user {
        warn_if_missing_override(kind, dir);
    }
    for (layer, dir, strictness) in candidate_dirs(root, kind, user) {
        read_dir(layer, &dir, strictness, &mut acc)?;
    }
    Ok(acc)
}

/// A default user path that isn't there is the normal "no user layer" case;
/// an *explicit* [`UserLayer::Dir`] that isn't there is a mistake worth a
/// warning. Returns whether it warned.
fn warn_if_missing_override(kind: &str, path: &Path) -> bool {
    if path.exists() {
        return false;
    }
    tracing::warn!(
        kind,
        path = %path.display(),
        "explicit definitions-dir override points at a missing path; treating the user layer as empty",
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> UserLayer {
        UserLayer::Home {
            home: Some("/home/u".into()),
            config: Some("/home/u/.config".into()),
        }
    }

    /// A trivial reader that records the layer + dir + strictness it was asked
    /// to read.
    fn record(
        layer: Layer,
        dir: &Path,
        strictness: Strictness,
        acc: &mut Vec<(Layer, PathBuf, Strictness)>,
    ) -> Result<()> {
        acc.push((layer, dir.to_path_buf(), strictness));
        Ok(())
    }

    /// #803: an embedder's missing `UserLayer::Dir` warns like a missing env
    /// override, and still loads as an empty user layer.
    #[test]
    fn a_missing_user_layer_dir_warns_and_is_empty() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("typo-agents");
        assert!(warn_if_missing_override("agents", &missing));
        assert!(!warn_if_missing_override("agents", root.path()));
        let out = load_layers(
            root.path(),
            "agents",
            &UserLayer::Dir(missing.clone()),
            Vec::new(),
            record,
        )
        .unwrap();
        assert_eq!(out[0], (Layer::User, missing, Strictness::Strict));
    }

    #[test]
    fn override_replaces_whole_user_layer() {
        let root = Path::new("/repo");
        let out = candidate_dirs(root, "agents", &UserLayer::Dir("/override".into()));
        assert_eq!(
            out,
            vec![
                (Layer::User, PathBuf::from("/override"), Strictness::Strict),
                (
                    Layer::Project,
                    PathBuf::from("/repo/.claude/agents"),
                    Strictness::Lenient
                ),
                (
                    Layer::Project,
                    PathBuf::from("/repo/.agents/agents"),
                    Strictness::Lenient
                ),
                (
                    Layer::Project,
                    PathBuf::from("/repo/.entanglement/agents"),
                    Strictness::Strict
                ),
            ]
        );
    }

    #[test]
    fn default_user_layer_is_claude_then_native() {
        let root = Path::new("/repo");
        let out = candidate_dirs(root, "skills", &home());
        assert_eq!(
            out[0],
            (
                Layer::User,
                PathBuf::from("/home/u/.claude/skills"),
                Strictness::Lenient
            )
        );
        assert_eq!(
            out[1],
            (
                Layer::User,
                PathBuf::from("/home/u/.config/entanglement/skills"),
                Strictness::Strict
            )
        );
        // Project trio follows, cross-vendor < vendor-specific < native.
        assert_eq!(
            out[2..]
                .iter()
                .map(|(l, p, s)| (*l, p.clone(), *s))
                .collect::<Vec<_>>(),
            vec![
                (
                    Layer::Project,
                    PathBuf::from("/repo/.claude/skills"),
                    Strictness::Lenient
                ),
                (
                    Layer::Project,
                    PathBuf::from("/repo/.agents/skills"),
                    Strictness::Lenient
                ),
                (
                    Layer::Project,
                    PathBuf::from("/repo/.entanglement/skills"),
                    Strictness::Strict
                ),
            ]
        );
    }

    #[test]
    fn commands_add_opencode_dirs_below_each_native_one() {
        let out = candidate_dirs(Path::new("/repo"), "commands", &home());
        let paths: Vec<(PathBuf, Strictness)> = out.into_iter().map(|(_, p, s)| (p, s)).collect();
        let lenient = |p: &str| (PathBuf::from(p), Strictness::Lenient);
        let strict = |p: &str| (PathBuf::from(p), Strictness::Strict);
        assert_eq!(
            paths,
            vec![
                lenient("/home/u/.claude/commands"),
                lenient("/home/u/.config/opencode/command"),
                lenient("/home/u/.config/opencode/commands"),
                strict("/home/u/.config/entanglement/commands"),
                lenient("/repo/.claude/commands"),
                lenient("/repo/.agents/commands"),
                lenient("/repo/.opencode/command"),
                lenient("/repo/.opencode/commands"),
                strict("/repo/.entanglement/commands"),
            ]
        );
    }

    #[test]
    fn missing_home_and_config_yield_project_only() {
        let no_dirs = UserLayer::Home {
            home: None,
            config: None,
        };
        for user in [no_dirs, UserLayer::Empty] {
            let out = candidate_dirs(Path::new("/repo"), "skills", &user);
            assert!(out.iter().all(|(l, _, _)| *l == Layer::Project));
            assert_eq!(out.len(), 3);
        }
    }

    #[test]
    fn built_ins_seed_then_user_then_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let user = root.join("user");
        std::fs::create_dir_all(&user).unwrap();

        let out = load_layers(
            root,
            "agents",
            &UserLayer::Dir(user.clone()),
            vec![(Layer::BuiltIn, root.to_path_buf(), Strictness::Strict)],
            record,
        )
        .unwrap();

        assert_eq!(out[0].0, Layer::BuiltIn);
        assert_eq!(out[1], (Layer::User, user, Strictness::Strict));
        assert_eq!(
            out.last().unwrap(),
            &(
                Layer::Project,
                root.join(".entanglement").join("agents"),
                Strictness::Strict
            )
        );
    }
}
