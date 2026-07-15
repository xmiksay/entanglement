//! Shared layered-definition discovery for the file-based registries (#204).
//!
//! The agent loader ([`crate::agents`]) and the skill loader ([`crate::skills`])
//! were byte-identical scaffolds modulo names: three precedence layers
//! (embedded built-in < user dir < project dir, later wins on a `name`
//! collision), a per-directory reader, and the `${config_dir}/entanglement/<kind>`
//! path resolution with an `ENTANGLEMENT_*_DIR` override. That common machinery
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
//! It also closes the shared bug: an *explicitly-set* `ENTANGLEMENT_*_DIR`
//! override that points at a missing path was silently swallowed, contradicting
//! the "loud, never a silent fallback" doctrine the provider catalog documents.
//! [`load_layers`] now `warn!`s when that happens — the default
//! `${config_dir}` path staying absent is still fine (the common "no user layer
//! yet" case), but a user who set the env var and mistyped it gets a signal.
//! An explicit override replaces the *whole* user layer (foreign + native),
//! which doubles as the opt-out for cross-vendor discovery.

use std::path::{Path, PathBuf};

use anyhow::Result;

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
/// precedence first. Pure — env/home/config are parameters — so the path policy
/// is unit-testable without mutating process-global state.
///
/// - `env_override` set ⇒ the user layer is exactly that dir (strict), foreign
///   user dirs included in the replacement; this keeps tests hermetic and is
///   the cross-vendor opt-out.
/// - Otherwise the user layer is `<home>/.claude/<kind>` (lenient) then
///   `<config>/entanglement/<kind>` (strict).
/// - The project layer is `.claude/<kind>` (lenient) < `.agents/<kind>`
///   (lenient, cross-vendor beats vendor-specific — same tiebreak as the brief
///   chain in [`crate::system_prompt`]) < `.entanglement/<kind>` (strict).
pub(crate) fn candidate_dirs(
    root: &Path,
    kind: &str,
    env_override: Option<PathBuf>,
    home: Option<&Path>,
    config: Option<&Path>,
) -> Vec<(Layer, PathBuf, Strictness)> {
    let mut dirs = Vec::new();
    if let Some(over) = env_override {
        dirs.push((Layer::User, over, Strictness::Strict));
    } else {
        if let Some(home) = home {
            dirs.push((
                Layer::User,
                home.join(".claude").join(kind),
                Strictness::Lenient,
            ));
        }
        if let Some(config) = config {
            dirs.push((
                Layer::User,
                config.join("entanglement").join(kind),
                Strictness::Strict,
            ));
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
    dirs.push((
        Layer::Project,
        root.join(".entanglement").join(kind),
        Strictness::Strict,
    ));
    dirs
}

/// Discover layered definitions for `kind`, seeded with `built_ins`: read each
/// candidate dir from [`candidate_dirs`] in precedence order, appending that
/// layer's raw items to the accumulator. `read_dir(layer, dir, strictness,
/// &mut acc)` does the actual reading and is where a missing dir is treated as
/// empty; the only thing added here is the loud path — an explicitly-overridden
/// user dir that does not exist is `warn!`ed rather than silently skipped (#204).
pub fn load_layers<T>(
    root: &Path,
    kind: &str,
    env: &str,
    mut acc: Vec<T>,
    mut read_dir: impl FnMut(Layer, &Path, Strictness, &mut Vec<T>) -> Result<()>,
) -> Result<Vec<T>> {
    let env_override = std::env::var_os(env).map(PathBuf::from);
    if let Some(over) = &env_override {
        // A default path that isn't there is the normal "no user layer" case;
        // an explicit override that isn't there is a user mistake.
        if !over.exists() {
            tracing::warn!(
                kind,
                env,
                path = %over.display(),
                "explicit definitions-dir override points at a missing path; treating the user layer as empty",
            );
        }
    }
    let home = dirs::home_dir();
    let config = dirs::config_dir();
    for (layer, dir, strictness) in
        candidate_dirs(root, kind, env_override, home.as_deref(), config.as_deref())
    {
        read_dir(layer, &dir, strictness, &mut acc)?;
    }
    Ok(acc)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// `load_layers` reads a process-global env var; tests that set it must not
    /// race under cargo's parallel test threads.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const TEST_ENV: &str = "ENTANGLEMENT_LAYERS_TEST_DIR";

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

    #[test]
    fn override_replaces_whole_user_layer() {
        let root = Path::new("/repo");
        let out = candidate_dirs(
            root,
            "agents",
            Some(PathBuf::from("/override")),
            Some(Path::new("/home/u")),
            Some(Path::new("/home/u/.config")),
        );
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
        let out = candidate_dirs(
            root,
            "skills",
            None,
            Some(Path::new("/home/u")),
            Some(Path::new("/home/u/.config")),
        );
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
    fn missing_home_and_config_yield_project_only() {
        let out = candidate_dirs(Path::new("/repo"), "skills", None, None, None);
        assert!(out.iter().all(|(l, _, _)| *l == Layer::Project));
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn built_ins_seed_then_user_then_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let user = root.join("user");
        std::fs::create_dir_all(&user).unwrap();

        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(TEST_ENV, &user);
        let out = load_layers(
            root,
            "agents",
            TEST_ENV,
            vec![(Layer::BuiltIn, root.to_path_buf(), Strictness::Strict)],
            record,
        )
        .unwrap();
        std::env::remove_var(TEST_ENV);

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

    #[test]
    fn missing_explicit_override_still_reads_and_does_not_abort() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let missing = root.join("no-such-dir");

        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(TEST_ENV, &missing);
        // A missing explicit override warns but is not fatal: the closure is
        // still invoked for the (absent) user dir, which treats it as empty.
        let out: Vec<(Layer, PathBuf, Strictness)> =
            load_layers(root, "skills", TEST_ENV, Vec::new(), record).unwrap();
        std::env::remove_var(TEST_ENV);

        assert_eq!(out[0], (Layer::User, missing, Strictness::Strict));
    }
}
