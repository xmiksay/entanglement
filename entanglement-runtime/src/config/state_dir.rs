//! The managed-state directory (ADR-0213): where skutter keeps the seven
//! files **it** writes on the user's behalf — grants, per-agent model pins and
//! generation overrides, aux-model pins, escape-root grants, and the MCP/LLM
//! OAuth token stores.
//!
//! One `ENTANGLEMENT_STATE_DIR` relocates all seven together, replacing the
//! seven per-file `ENTANGLEMENT_*_FILE` overrides it superseded: the files are
//! siblings by design, and nobody needs to scatter them. The hand-edited
//! inputs — `config.yml`, `providers.yml`, the provider-key `.env`, agents,
//! skills, preamble, brief — keep their own overrides and never move with it.

use std::ffi::OsString;
use std::path::PathBuf;

/// Env var relocating every managed-state file at once.
pub const STATE_DIR_ENV: &str = "ENTANGLEMENT_STATE_DIR";

pub const GRANTS_FILE: &str = "grants.yml";
pub const AGENT_MODELS_FILE: &str = "agent-models.yml";
pub const AGENT_GENERATION_FILE: &str = "agent-generation.yml";
pub const AUX_MODELS_FILE: &str = "aux-models.yml";
pub const EXTRA_ROOTS_FILE: &str = "extra-roots.yml";
pub const MCP_TOKENS_FILE: &str = "mcp-tokens.yml";
pub const LLM_TOKENS_FILE: &str = "llm-tokens.yml";

/// Every file [`STATE_DIR_ENV`] relocates — and nothing else.
pub const MANAGED_STATE_FILES: [&str; 7] = [
    GRANTS_FILE,
    AGENT_MODELS_FILE,
    AGENT_GENERATION_FILE,
    AUX_MODELS_FILE,
    EXTRA_ROOTS_FILE,
    MCP_TOKENS_FILE,
    LLM_TOKENS_FILE,
];

/// The managed-state directory: `ENTANGLEMENT_STATE_DIR`, else
/// `${config_dir}/entanglement`. `None` when neither resolves (persistence
/// then no-ops, as each store documents).
pub fn state_dir() -> Option<PathBuf> {
    resolve(
        |name| std::env::var_os(name),
        || crate::user_dirs::config_dir().map(|d| d.join("entanglement")),
    )
}

/// `file` (one of [`MANAGED_STATE_FILES`]) inside [`state_dir`].
pub fn state_file(file: &str) -> Option<PathBuf> {
    state_dir().map(|d| d.join(file))
}

/// [`state_dir`] over an injected env lookup and default, so the precedence
/// is unit-testable without mutating process env (#747). A set-but-empty
/// value counts as unset — an empty path would resolve files against cwd.
pub(crate) fn resolve(
    env: impl Fn(&str) -> Option<OsString>,
    default_dir: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    env(STATE_DIR_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(default_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_env_var_wins_over_the_config_dir() {
        let dir = resolve(
            |n| (n == STATE_DIR_ENV).then(|| "/srv/state".into()),
            || panic!("the default must not be consulted"),
        );
        assert_eq!(dir, Some(PathBuf::from("/srv/state")));
    }

    #[test]
    fn unset_or_empty_falls_back_to_the_default() {
        let default = || Some(PathBuf::from("/cfg/entanglement"));
        assert_eq!(resolve(|_| None, default), default());
        assert_eq!(resolve(|_| Some(OsString::new()), default), default());
        assert_eq!(resolve(|_| None, || None), None);
    }

    #[test]
    fn exactly_the_seven_managed_files_and_no_hand_edited_input() {
        let mut names = MANAGED_STATE_FILES.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 7, "seven distinct managed files");
        for input in ["config.yml", "providers.yml", ".env"] {
            assert!(!MANAGED_STATE_FILES.contains(&input), "{input}");
        }
    }
}
