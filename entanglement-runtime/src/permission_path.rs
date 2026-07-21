//! Root-relative normalization for path-shaped permission arguments (#485,
//! [ADR-0125](../../../docs/adr/0125-permission-arguments-for-path-tools-are-normalized-root-relative.md)).
//!
//! [`permission_arg`][crate::permission::permission_arg] extracts a call's
//! tool-specific argument **verbatim** — the model may spell a `read`/`edit`/
//! `grep`/`glob` path either relatively (`src/main.rs`) or as an absolute path
//! that happens to resolve inside the project root
//! (`/home/user/project/src/main.rs`). Arg-scoped permission rules (#173,
//! `read(src/*)`) and grants (#174) are authored root-relative, so the two
//! spellings used to grade differently even though they name the same file.
//!
//! This module is a grading-only wrapper layered *on top of* `permission_arg`
//! — that function stays the pure verbatim extractor (the TUI transcript
//! render still shows the model's literal input, `tui/transcript/render_run.rs`)
//! — so only the call sites that feed a permission/grant decision route through
//! [`grading_arg`].

use std::path::{Component, Path, PathBuf};

/// Tools whose [`permission_arg`][crate::permission::permission_arg] value is a
/// filesystem path (or path glob), and therefore eligible for root-relative
/// normalization. `bash`/`call` (a shell command line) are deliberately absent
/// — a command line is never a path, so it must never be rewritten.
const PATH_ARG_TOOLS: &[&str] = &["read", "edit", "write", "apply_patch", "glob", "grep"];

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

/// Normalize a path-arg tool's argument root-relative when it names a location
/// inside `root` (#485): lexically normalize, then — if the result is
/// absolute and resolves under `root` — strip the root prefix (`root` itself
/// becomes `"."`). An out-of-root absolute path stays verbatim (lexically
/// normalized): a root-relative rule matching an outside path would be wrong,
/// and the escape-root gate (ADR-0109) already owns that case. A tool with no
/// path-arg concept (`bash`/`call`, or any tool `permission_arg` returns
/// `None` for) is untouched.
pub fn rooted_arg(root: &Path, tool: &str, arg: &str) -> String {
    if !PATH_ARG_TOOLS.contains(&tool) {
        return arg.to_string();
    }
    let normalized = normalize_lexical(arg);
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
/// [`permission_arg`][crate::permission::permission_arg] mapped through
/// [`rooted_arg`] when a project `root` is wired. `root: None` — the test-only
/// `spawn_tool_executor`/`spawn_tool_executor_with_hooks` wrappers, which pass
/// no escape-root policy — yields the verbatim argument, byte-identical to
/// pre-#485 behavior. This is the function every arg-scoped rule match and
/// grant-key lookup should use; `permission_arg` itself stays reserved for
/// display (the TUI transcript) and the escape-root/workdir extractors, which
/// have their own, unrelated normalization needs.
pub fn grading_arg(tool: &str, input: &str, root: Option<&Path>) -> Option<String> {
    let verbatim = crate::permission::permission_arg(tool, input)?;
    Some(match root {
        Some(root) => rooted_arg(root, tool, &verbatim),
        None => verbatim,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn rooted_arg_strips_an_in_root_absolute_path_per_tool() {
        let root = Path::new("/home/user/project");
        for tool in ["read", "edit", "write", "apply_patch"] {
            assert_eq!(
                rooted_arg(root, tool, "/home/user/project/src/main.rs"),
                "src/main.rs",
                "tool {tool}"
            );
        }
        // glob's `pattern` and grep's file-filter `path` are path-shaped too,
        // wildcards survive the strip untouched.
        assert_eq!(
            rooted_arg(root, "glob", "/home/user/project/src/*.rs"),
            "src/*.rs"
        );
        assert_eq!(
            rooted_arg(root, "grep", "/home/user/project/src/*"),
            "src/*"
        );
    }

    #[test]
    fn rooted_arg_leaves_a_relative_path_unchanged() {
        let root = Path::new("/home/user/project");
        assert_eq!(rooted_arg(root, "read", "src/main.rs"), "src/main.rs");
    }

    #[test]
    fn rooted_arg_maps_root_itself_to_dot() {
        let root = Path::new("/home/user/project");
        assert_eq!(rooted_arg(root, "read", "/home/user/project"), ".");
        assert_eq!(rooted_arg(root, "read", "/home/user/project/"), ".");
    }

    #[test]
    fn rooted_arg_leaves_an_out_of_root_absolute_path_verbatim() {
        let root = Path::new("/home/user/project");
        assert_eq!(
            rooted_arg(root, "read", "/etc/passwd"),
            "/etc/passwd",
            "out-of-root paths are the escape-root gate's problem, not this one"
        );
        // Still lexically normalized, just not root-relativized.
        assert_eq!(
            rooted_arg(root, "read", "/etc/../etc/passwd"),
            "/etc/passwd"
        );
    }

    #[test]
    fn rooted_arg_never_touches_a_command_line() {
        let root = Path::new("/home/user/project");
        assert_eq!(
            rooted_arg(root, "bash", "cat /home/user/project/src/main.rs"),
            "cat /home/user/project/src/main.rs"
        );
        assert_eq!(
            rooted_arg(root, "call", "cat /home/user/project/src/main.rs"),
            "cat /home/user/project/src/main.rs"
        );
    }

    #[test]
    fn grading_arg_is_verbatim_with_no_root_wired() {
        assert_eq!(
            grading_arg("read", r#"{"path":"/home/user/project/src/main.rs"}"#, None).as_deref(),
            Some("/home/user/project/src/main.rs"),
            "no root wired must stay byte-identical to plain permission_arg"
        );
    }

    #[test]
    fn grading_arg_normalizes_with_a_root_wired() {
        let root = Path::new("/home/user/project");
        assert_eq!(
            grading_arg(
                "read",
                r#"{"path":"/home/user/project/src/main.rs"}"#,
                Some(root)
            )
            .as_deref(),
            Some("src/main.rs")
        );
        // Already-relative input is passed through unchanged either way.
        assert_eq!(
            grading_arg("read", r#"{"path":"src/main.rs"}"#, Some(root)).as_deref(),
            Some("src/main.rs")
        );
        // bash's command-line arg is untouched even with a root wired.
        assert_eq!(
            grading_arg("bash", r#"{"command":"git status"}"#, Some(root)).as_deref(),
            Some("git status")
        );
    }
}
