//! Relative path arguments → root-relative (#764, ADR-0224).
//!
//! Applied once, to the call as the model sent it, before grading, the
//! escape-root gate or execution, so a workdir can't be used to smuggle a
//! path past a rule: every later stage sees the path the call really names,
//! spelled exactly as it would be with no workdir.

use std::path::Path;

use serde_json::{Map, Value};

use crate::permission_path::normalize_lexical;

/// What `tool`'s input becomes for a session working in `workdir` (absolute)
/// under `root`, or `None` when nothing changes — no workdir, a workdir equal
/// to the root, a tool with no path argument, or only absolute paths.
///
/// - `read`/`read_raw`/`edit`/`write`/`apply_patch`, and `rhai`'s script
///   `path` (ADR-0228): a relative `path` joins onto the workdir.
/// - `glob`/`glob_json`/`grep`/`grep_json`: a relative `path` joins, an absent
///   one becomes the workdir (a `glob` with an absolute `pattern` excepted).
/// - `bash`/`call`: a relative `workdir` joins, an absent one becomes the
///   workdir; `call`'s `input_file`/`output_file` join too.
pub(crate) fn rewrite(
    root: &Path,
    workdir: Option<&Path>,
    tool: &str,
    input: &str,
) -> Option<String> {
    let base = base(root, workdir?)?;
    let mut value: Value = serde_json::from_str(input).ok()?;
    let args = value.as_object_mut()?;
    let changed = match tool {
        "read" | "read_raw" | "edit" | "write" | "apply_patch" | "rhai" => {
            join(args, "path", &base)
        }
        "glob" | "glob_json" => {
            let absolute_pattern = args
                .get("pattern")
                .and_then(Value::as_str)
                .is_some_and(|p| Path::new(p).is_absolute());
            if absolute_pattern && !args.contains_key("path") {
                false
            } else {
                join_or_default(args, "path", &base)
            }
        }
        "grep" | "grep_json" => join_or_default(args, "path", &base),
        "bash" => join_or_default(args, "workdir", &base),
        "call" => {
            let workdir = join_or_default(args, "workdir", &base);
            let input_file = join(args, "input_file", &base);
            let output_file = join(args, "output_file", &base);
            workdir || input_file || output_file
        }
        _ => false,
    };
    changed.then(|| value.to_string())
}

/// The prefix a relative argument joins onto: the workdir relative to the
/// root (so a joined path stays root-relative, as the model would have
/// spelled it), or absolute for a workdir outside the root.
fn base(root: &Path, workdir: &Path) -> Option<String> {
    match workdir.strip_prefix(root) {
        Ok(rel) if rel.as_os_str().is_empty() => None,
        Ok(rel) => Some(rel.to_string_lossy().into_owned()),
        Err(_) => Some(workdir.to_string_lossy().into_owned()),
    }
}

/// Join a present, relative string `key` onto `base`.
fn join(args: &mut Map<String, Value>, key: &str, base: &str) -> bool {
    let Some(Value::String(rel)) = args.get(key) else {
        return false;
    };
    if Path::new(rel).is_absolute() {
        return false;
    }
    let joined = normalize_lexical(&format!("{base}/{rel}"));
    args.insert(key.to_string(), Value::String(joined));
    true
}

/// [`join`], or set `key` to `base` when the call left it out.
fn join_or_default(args: &mut Map<String, Value>, key: &str, base: &str) -> bool {
    if args.contains_key(key) {
        return join(args, key, base);
    }
    args.insert(key.to_string(), Value::String(base.to_string()));
    true
}

#[cfg(test)]
mod tests {
    use super::rewrite;
    use serde_json::{json, Value};
    use std::path::Path;

    const ROOT: &str = "/p";

    fn run(workdir: &str, tool: &str, input: Value) -> Option<Value> {
        rewrite(
            Path::new(ROOT),
            Some(Path::new(workdir)),
            tool,
            &input.to_string(),
        )
        .map(|s| serde_json::from_str(&s).expect("rewritten input is JSON"))
    }

    #[test]
    fn no_workdir_or_the_root_changes_nothing() {
        let input = json!({"path": "a.rs"}).to_string();
        assert_eq!(rewrite(Path::new(ROOT), None, "read", &input), None);
        assert_eq!(run(ROOT, "read", json!({"path": "a.rs"})), None);
    }

    #[test]
    fn file_tools_join_a_relative_path_root_relative() {
        for tool in ["read", "read_raw", "edit", "write", "apply_patch", "rhai"] {
            assert_eq!(
                run("/p/sub", tool, json!({"path": "a.rs", "x": 1})),
                Some(json!({"path": "sub/a.rs", "x": 1})),
                "{tool}"
            );
        }
    }

    /// ADR-0228: an inline `rhai` script has no path to rewrite.
    #[test]
    fn an_inline_rhai_script_is_untouched() {
        assert_eq!(run("/p/sub", "rhai", json!({"script": "1"})), None);
    }

    #[test]
    fn dot_dot_and_absolute_paths() {
        assert_eq!(
            run("/p/sub", "read", json!({"path": "../b.rs"})),
            Some(json!({"path": "b.rs"}))
        );
        assert_eq!(run("/p/sub", "read", json!({"path": "/etc/hosts"})), None);
    }

    #[test]
    fn an_out_of_root_workdir_joins_absolute() {
        assert_eq!(
            run("/x/y", "write", json!({"path": "a.rs"})),
            Some(json!({"path": "/x/y/a.rs"}))
        );
        assert_eq!(
            run("/x/y", "bash", json!({"command": "ls"})),
            Some(json!({"command": "ls", "workdir": "/x/y"}))
        );
    }

    #[test]
    fn searches_default_their_base_to_the_workdir() {
        assert_eq!(
            run("/p/sub", "glob", json!({"pattern": "**/*.rs"})),
            Some(json!({"pattern": "**/*.rs", "path": "sub"}))
        );
        assert_eq!(
            run(
                "/p/sub",
                "grep_json",
                json!({"pattern": "x", "path": "src"})
            ),
            Some(json!({"pattern": "x", "path": "sub/src"}))
        );
        assert_eq!(
            run("/p/sub", "glob", json!({"pattern": "/abs/**"})),
            None,
            "an absolute pattern needs no base"
        );
    }

    #[test]
    fn exec_tools_default_their_cwd_and_join_files() {
        assert_eq!(
            run("/p/sub", "bash", json!({"command": "ls", "workdir": "t"})),
            Some(json!({"command": "ls", "workdir": "sub/t"}))
        );
        assert_eq!(
            run(
                "/p/sub",
                "call",
                json!({"command": "cat", "input_file": "in", "output_file": "/o"})
            ),
            Some(
                json!({"command": "cat", "workdir": "sub", "input_file": "sub/in", "output_file": "/o"})
            )
        );
    }

    #[test]
    fn other_tools_and_bad_input_are_untouched() {
        assert_eq!(run("/p/sub", "agent", json!({"prompt": "x"})), None);
        assert_eq!(
            rewrite(
                Path::new(ROOT),
                Some(Path::new("/p/sub")),
                "read",
                "not json"
            ),
            None
        );
    }
}
