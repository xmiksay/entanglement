//! Discovery of raw agent definition files across the three precedence
//! layers (embedded built-ins < user < project), before any parsing.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::AgentLayer;
use crate::layers::Strictness;

/// Embedded built-in definitions, parsed through the same loader as user/project
/// files. `(filename, contents)` — the filename only feeds parse-error messages;
/// the agent's identity is its frontmatter `name`.
pub(super) const BUILT_INS: &[(&str, &str)] = &[
    ("general.md", include_str!("general.md")),
    ("plan.md", include_str!("plan.md")),
    ("debug.md", include_str!("debug.md")),
];

/// Env var overriding the user agents directory (tests + non-XDG setups).
const AGENTS_DIR_ENV: &str = "ENTANGLEMENT_AGENTS_DIR";

/// A discovered agent definition file *before* parsing: which layer it came from
/// (#185), a display label for its origin (`built-in (general.md)` or the file
/// path), the raw file content, and — for a real on-disk file only — the path
/// itself, so a strict-layer parse failure can attempt the retired-key
/// self-heal ([`migrate_legacy_agent_file`]). `None` for an embedded built-in,
/// which has no file to rewrite and never carries a retired key.
pub(super) struct RawAgent {
    pub(super) layer: AgentLayer,
    pub(super) strictness: Strictness,
    pub(super) source: String,
    pub(super) content: String,
    pub(super) path: Option<PathBuf>,
}

/// Enumerate every agent definition in precedence order — embedded built-ins,
/// then the user dir, then the project dir — without parsing them. Later entries
/// win on a `name` collision, so consumers keep the last match. A missing dir is
/// fine; an unreadable dir or file is an error. A missing *explicit*
/// `ENTANGLEMENT_AGENTS_DIR` override is warned by [`crate::layers::load_layers`].
pub(super) fn discover(root: &Path) -> Result<Vec<RawAgent>> {
    let built_ins: Vec<RawAgent> = BUILT_INS
        .iter()
        .map(|(file, contents)| RawAgent {
            layer: AgentLayer::BuiltIn,
            strictness: Strictness::Strict,
            source: format!("built-in ({file})"),
            content: (*contents).to_string(),
            path: None,
        })
        .collect();
    crate::layers::load_layers(root, "agents", AGENTS_DIR_ENV, built_ins, read_dir_raws)
}

/// Append every `*.md` file in `dir` (if it exists) to `raws`, tagged with
/// `layer` and sorted for deterministic collision resolution within the
/// directory.
fn read_dir_raws(
    layer: AgentLayer,
    dir: &Path,
    strictness: Strictness,
    raws: &mut Vec<RawAgent>,
) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("reading agents dir {}", dir.display()))?;
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("md"))
        .collect();
    files.sort();
    for path in files {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading agent definition {}", path.display()))?;
        raws.push(RawAgent {
            layer,
            strictness,
            source: path.display().to_string(),
            content,
            path: Some(path),
        });
    }
    Ok(())
}
