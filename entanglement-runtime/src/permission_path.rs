//! Root-relative normalization for path-shaped permission arguments (#485,
//! [ADR-0125](../../../docs/adr/archive/0125-permission-arguments-for-path-tools-are-normalized-root-relative.md)).
//!
//! [`permission_arg`][crate::permission::permission_arg] extracts a call's
//! tool-specific argument **verbatim** — the model may spell a path either
//! relatively (`src/main.rs`) or as an absolute path that happens to resolve
//! inside the project root (`/home/user/project/src/main.rs`). Arg-scoped
//! permission rules (#173, `tool(src/*)`) and grants (#174) are authored
//! root-relative, so the two spellings used to grade differently even though
//! they name the same file.
//!
//! This module is a grading-only wrapper layered *on top of* `permission_arg`
//! — that function stays the pure verbatim extractor (a head's transcript
//! render still shows the model's literal input) — so only the call sites
//! that feed a permission/grant decision route through [`grading_arg`].
//! Which arguments are paths is the registered tool's answer
//! ([`ArgKind::Path`], #962), never a name list here.

use std::path::{Component, Path, PathBuf};

use crate::permission::{permission_arg, permission_scope};
use crate::tool_names::RHAI_TOOL;
use crate::tools::{ArgKind, ToolRegistry};

/// Fold `.`/`..`/`//` out of `path` **lexically** — no filesystem access, so a
/// symlink component is left exactly as written (resolving those is the
/// escape-root gate's job, not this one, ADR-0109). A `..` that has nothing to
/// pop (a leading `..` in a relative path, or one immediately after an
/// absolute root) is kept as-is rather than treated as an error — this is a
/// normalizer, not a validator. An empty result (`"."`, `""`, or a path that
/// fully cancels itself out) normalizes to `"."`.
pub fn normalize_lexical(path: &str) -> String {
    let mut out: Vec<Component> = Vec::new();
    for comp in Path::new(path).components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) => {
                    // Can't go above an absolute root — drop it.
                }
                _ => out.push(comp),
            },
            _ => out.push(comp),
        }
    }
    if out.is_empty() {
        return ".".to_string();
    }
    let mut result = PathBuf::new();
    for comp in out {
        result.push(comp.as_os_str());
    }
    result.to_string_lossy().into_owned()
}

/// Normalize a path argument root-relative when it names a location inside
/// `root` (#485): lexically normalize, then — if the result is absolute and
/// resolves under `root` — strip the root prefix (`root` itself becomes
/// `"."`). An out-of-root absolute path stays verbatim (lexically
/// normalized): a root-relative rule matching an outside path would be
/// wrong, and the escape-root gate (ADR-0109) already owns that case.
pub fn rooted_path(root: &Path, path: &str) -> String {
    root_relative(root, normalize_lexical(path))
}

/// [`rooted_path`] for a script path, graded on its *canonical* file
/// (ADR-0228): a script's grade must follow the code that runs, so a
/// symlink in `scripts/deploy/` pointing elsewhere grades as where it
/// points. The launch re-checks the file it read against this. Unresolvable
/// (missing) falls back to the lexical form — the read then fails anyway.
pub fn rooted_script(root: &Path, path: &str) -> String {
    match root.join(path).canonicalize() {
        Ok(canonical) => rooted_path(root, &canonical.to_string_lossy()),
        Err(_) => rooted_path(root, path),
    }
}

fn root_relative(root: &Path, normalized: String) -> String {
    let path = Path::new(&normalized);
    if path.is_absolute() {
        if let Ok(stripped) = path.strip_prefix(root) {
            return if stripped.as_os_str().is_empty() {
                ".".to_string()
            } else {
                stripped.to_string_lossy().into_owned()
            };
        }
    }
    normalized
}

/// The grading-time argument for a permission/grant decision:
/// [`permission_arg`] mapped through [`rooted_path`] (a `rhai` script path
/// through [`rooted_script`]) when the tool declares a [`ArgKind::Path`]
/// argument and a project `root` is wired. `root: None` — a
/// `ToolExecutor::builder` with no `.escape_root(..)` — yields the verbatim
/// argument, byte-identical to pre-#485 behavior. This is the function every
/// arg-scoped rule match and grant-key lookup should use; `permission_arg`
/// itself stays reserved for display and the escape-root/workdir
/// extractors, which have their own, unrelated normalization needs.
pub fn grading_arg(
    tools: &ToolRegistry,
    tool: &str,
    input: &str,
    root: Option<&Path>,
) -> Option<String> {
    let verbatim = permission_arg(tools, tool, input)?;
    Some(match root {
        Some(root) if tool == RHAI_TOOL => rooted_script(root, &verbatim),
        Some(root) if permission_scope(tools, tool).arg == ArgKind::Path => {
            rooted_path(root, &verbatim)
        }
        _ => verbatim,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> ToolRegistry {
        crate::host::every_tool("/home/user/project".into())
    }

    #[test]
    fn normalize_lexical_folds_dot_and_dotdot() {
        assert_eq!(normalize_lexical("src/./main.rs"), "src/main.rs");
        assert_eq!(normalize_lexical("src/../src/main.rs"), "src/main.rs");
        assert_eq!(normalize_lexical("src//main.rs"), "src/main.rs");
        assert_eq!(normalize_lexical("."), ".");
        assert_eq!(normalize_lexical(""), ".");
        // A leading `..` in a relative path has nothing to pop — kept as-is.
        assert_eq!(
            normalize_lexical("../sibling/main.rs"),
            "../sibling/main.rs"
        );
        // `..` right after an absolute root can't go higher — dropped.
        assert_eq!(normalize_lexical("/../etc/passwd"), "/etc/passwd");
    }

    #[test]
    fn rooted_path_strips_an_in_root_absolute_path() {
        let root = Path::new("/home/user/project");
        assert_eq!(
            rooted_path(root, "/home/user/project/src/main.rs"),
            "src/main.rs"
        );
        // A path glob is path-shaped too: wildcards survive the strip
        // untouched.
        assert_eq!(rooted_path(root, "/home/user/project/src/*.rs"), "src/*.rs");
        assert_eq!(rooted_path(root, "/home/user/project/src/*"), "src/*");
    }

    #[test]
    fn rooted_path_leaves_a_relative_path_unchanged() {
        let root = Path::new("/home/user/project");
        assert_eq!(rooted_path(root, "src/main.rs"), "src/main.rs");
    }

    #[test]
    fn rooted_path_maps_root_itself_to_dot() {
        let root = Path::new("/home/user/project");
        assert_eq!(rooted_path(root, "/home/user/project"), ".");
        assert_eq!(rooted_path(root, "/home/user/project/"), ".");
    }

    #[test]
    fn rooted_path_leaves_an_out_of_root_absolute_path_verbatim() {
        let root = Path::new("/home/user/project");
        assert_eq!(
            rooted_path(root, "/etc/passwd"),
            "/etc/passwd",
            "out-of-root paths are the escape-root gate's problem, not this one"
        );
        // Still lexically normalized, just not root-relativized.
        assert_eq!(rooted_path(root, "/etc/../etc/passwd"), "/etc/passwd");
    }

    /// ADR-0228: a script path grades on its canonical target —
    /// root-relative inside the root, absolute outside it.
    #[cfg(unix)]
    #[test]
    fn rooted_script_canonicalizes_a_script_path() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let root = base.join("proj");
        std::fs::create_dir_all(root.join("scripts/deploy")).unwrap();
        std::fs::write(root.join("scripts/real.rhai"), "1").unwrap();
        std::fs::write(base.join("out.rhai"), "1").unwrap();
        std::os::unix::fs::symlink(
            root.join("scripts/real.rhai"),
            root.join("scripts/deploy/x.rhai"),
        )
        .unwrap();
        std::os::unix::fs::symlink(base.join("out.rhai"), root.join("scripts/out.rhai")).unwrap();
        assert_eq!(
            rooted_script(&root, "scripts/deploy/x.rhai"),
            "scripts/real.rhai"
        );
        let abs = root.join("scripts/./real.rhai");
        assert_eq!(
            rooted_script(&root, abs.to_str().unwrap()),
            "scripts/real.rhai"
        );
        assert_eq!(
            rooted_script(&root, "scripts/out.rhai"),
            base.join("out.rhai").to_string_lossy()
        );
        assert_eq!(
            rooted_script(&root, "scripts/./gone.rhai"),
            "scripts/gone.rhai"
        );
        // A plain path stays lexical (ADR-0125).
        assert_eq!(
            rooted_path(&root, "scripts/deploy/x.rhai"),
            "scripts/deploy/x.rhai"
        );
    }

    #[test]
    fn grading_arg_is_verbatim_with_no_root_wired() {
        assert_eq!(
            grading_arg(
                &tools(),
                "read",
                r#"{"path":"/home/user/project/src/main.rs"}"#,
                None
            )
            .as_deref(),
            Some("/home/user/project/src/main.rs"),
            "no root wired must stay byte-identical to plain permission_arg"
        );
    }

    #[test]
    fn grading_arg_normalizes_with_a_root_wired() {
        let root = Path::new("/home/user/project");
        assert_eq!(
            grading_arg(
                &tools(),
                "read",
                r#"{"path":"/home/user/project/src/main.rs"}"#,
                Some(root)
            )
            .as_deref(),
            Some("src/main.rs")
        );
        // Already-relative input is passed through unchanged either way.
        assert_eq!(
            grading_arg(&tools(), "read", r#"{"path":"src/main.rs"}"#, Some(root)).as_deref(),
            Some("src/main.rs")
        );
        // bash's command-line arg is untouched even with a root wired.
        assert_eq!(
            grading_arg(&tools(), "bash", r#"{"command":"git status"}"#, Some(root)).as_deref(),
            Some("git status")
        );
    }
}
