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
//! It also closes the shared bug: an *explicitly-set* `ENTANGLEMENT_*_DIR`
//! override that points at a missing path was silently swallowed, contradicting
//! the "loud, never a silent fallback" doctrine the provider catalog documents.
//! [`load_layers`] now `warn!`s when that happens — the default
//! `${config_dir}` path staying absent is still fine (the common "no user layer
//! yet" case), but a user who set the env var and mistyped it gets a signal.

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

/// A resolved user-layer directory plus whether it came from an explicit
/// `ENTANGLEMENT_*_DIR` override (vs the default `${config_dir}` path). The flag
/// is what lets [`load_layers`] be loud about a missing *explicit* override
/// while staying quiet about a missing *default*.
struct UserDir {
    path: PathBuf,
    explicit: bool,
}

/// The user directory for `kind` (`agents`/`skills`): the `env`-overridden path
/// if that var is set, else `${config_dir}/entanglement/<kind>`. `None` only
/// when there is no config dir *and* no override. Marks whether the path was
/// explicitly overridden so a missing one can be surfaced.
fn user_config_dir(env: &str, kind: &str) -> Option<UserDir> {
    if let Some(p) = std::env::var_os(env) {
        return Some(UserDir {
            path: PathBuf::from(p),
            explicit: true,
        });
    }
    dirs::config_dir().map(|d| UserDir {
        path: d.join("entanglement").join(kind),
        explicit: false,
    })
}

/// Discover layered definitions for `kind`, seeded with `built_ins`: read the
/// user dir (`env`-overridden or `${config_dir}/entanglement/<kind>`), then the
/// project dir (`<root>/.entanglement/<kind>`), appending each layer's raw
/// items to the accumulator in precedence order. `read_dir(layer, dir, &mut acc)`
/// does the actual reading and is where a missing dir is treated as empty; the
/// only thing added here is the loud path — an explicitly-overridden user dir
/// that does not exist is `warn!`ed rather than silently skipped (#204).
pub fn load_layers<T>(
    root: &Path,
    kind: &str,
    env: &str,
    mut acc: Vec<T>,
    mut read_dir: impl FnMut(Layer, &Path, &mut Vec<T>) -> Result<()>,
) -> Result<Vec<T>> {
    if let Some(user) = user_config_dir(env, kind) {
        // A default `${config_dir}` path that isn't there is the normal "no user
        // layer" case; an explicit override that isn't there is a user mistake.
        if user.explicit && !user.path.exists() {
            tracing::warn!(
                kind,
                env,
                path = %user.path.display(),
                "explicit definitions-dir override points at a missing path; treating the user layer as empty",
            );
        }
        read_dir(Layer::User, &user.path, &mut acc)?;
    }
    read_dir(
        Layer::Project,
        &root.join(".entanglement").join(kind),
        &mut acc,
    )?;
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

    /// A trivial reader that records the layer + dir it was asked to read.
    fn record(layer: Layer, dir: &Path, acc: &mut Vec<(Layer, PathBuf)>) -> Result<()> {
        acc.push((layer, dir.to_path_buf()));
        Ok(())
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
            vec![(Layer::BuiltIn, root.to_path_buf())],
            record,
        )
        .unwrap();
        std::env::remove_var(TEST_ENV);

        assert_eq!(out[0].0, Layer::BuiltIn);
        assert_eq!(out[1], (Layer::User, user));
        assert_eq!(
            out[2],
            (Layer::Project, root.join(".entanglement").join("agents"))
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
        let out: Vec<(Layer, PathBuf)> =
            load_layers(root, "skills", TEST_ENV, Vec::new(), record).unwrap();
        std::env::remove_var(TEST_ENV);

        assert_eq!(out[0], (Layer::User, missing));
    }
}
