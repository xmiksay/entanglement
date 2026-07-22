//! `/allow <path>` subcommand parsing + directory normalization (#486,
//! ADR-0126): kept in its own sibling module rather than folded into
//! `tui/commands.rs` or `tui/event_loop.rs` — both already at/over the
//! 400-line cap — mirroring how `/mcp` was split into `mcp_command.rs` (#373).

use std::path::{Path, PathBuf};

use crate::permission_path::normalize_lexical;

use super::app::App;
use super::commands::Command;

/// Resolve `/allow`'s raw path argument against the project `root` into the
/// normalized, root-relative form the grant store keys on (#486): joins a
/// relative input onto `root`, lexically folds `.`/`..`/`//` (#485's
/// `normalize_lexical` — no filesystem access, so a directory that doesn't
/// exist yet is still a valid target), and rejects anything that still
/// resolves outside `root`. Mirrors `permission_path::rooted_arg`'s
/// absolute-path handling but additionally catches a relative input that
/// escapes upward (`../etc`), which `rooted_arg` deliberately leaves alone
/// for path-arg tools (out-of-root is the escape-root gate's problem there);
/// `/allow` has no escape-root counterpart, so it rejects instead.
pub fn normalize_allow_dir(root: &Path, input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("usage: /allow <path>".to_string());
    }
    let candidate = Path::new(trimmed);
    let absolute: PathBuf = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let normalized = normalize_lexical(&absolute.to_string_lossy());
    match Path::new(&normalized).strip_prefix(root) {
        Ok(rel) if rel.as_os_str().is_empty() => Ok(".".to_string()),
        Ok(rel) => Ok(rel.to_string_lossy().into_owned()),
        Err(_) => Err(format!("outside the project root: {trimmed}")),
    }
}

/// Send `/allow <path>`: normalize against the head's root and record a
/// `SessionDir` grant for the active session directly through the installed
/// grant store — no wire traffic, this is head policy (ADR-0126), not an
/// engine op. A parse/outside-root error renders as a status line instead,
/// mirroring `mcp_command::send_mcp`.
pub(super) fn send_allow(app: &mut App, text: &str) {
    let rest = text
        .trim()
        .strip_prefix(&Command::Allow.slash_name())
        .map(str::trim)
        .unwrap_or("");
    match normalize_allow_dir(app.root(), rest) {
        Ok(dir) => app.apply_allow_grant(&dir),
        Err(message) => app.record_allow_error(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/home/user/project")
    }

    #[test]
    fn normalizes_a_relative_path() {
        assert_eq!(normalize_allow_dir(&root(), "src"), Ok("src".to_string()));
        assert_eq!(
            normalize_allow_dir(&root(), "./src/"),
            Ok("src".to_string())
        );
    }

    #[test]
    fn normalizes_an_in_root_absolute_path() {
        assert_eq!(
            normalize_allow_dir(&root(), "/home/user/project/src"),
            Ok("src".to_string())
        );
    }

    #[test]
    fn root_itself_normalizes_to_dot() {
        assert_eq!(normalize_allow_dir(&root(), "."), Ok(".".to_string()));
        assert_eq!(
            normalize_allow_dir(&root(), "/home/user/project"),
            Ok(".".to_string())
        );
    }

    #[test]
    fn rejects_an_out_of_root_relative_path() {
        assert!(normalize_allow_dir(&root(), "../etc").is_err());
    }

    #[test]
    fn rejects_an_out_of_root_absolute_path() {
        assert!(normalize_allow_dir(&root(), "/etc").is_err());
    }

    #[test]
    fn rejects_an_empty_path() {
        assert!(normalize_allow_dir(&root(), "").is_err());
        assert!(normalize_allow_dir(&root(), "   ").is_err());
    }
}
