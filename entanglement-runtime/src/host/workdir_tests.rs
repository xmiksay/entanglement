//! Each host tool's path grading through the generic [`path_grading`]
//! entry points, run over the real host registry: which arguments join onto
//! a session workdir ([`rewrite`], #764, ADR-0224) and which path the
//! escape-root gate checks ([`escape_target`], ADR-0109).
//!
//! [`path_grading`]: crate::path_grading

use super::{host_tools, BashTool, GlobJsonTool, GrepJsonTool, ReadRawTool};
use crate::path_grading::{escape_target, rewrite};
use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Every host tool with a path argument, as `engine_setup` registers them.
fn registry() -> ToolRegistry {
    let root = PathBuf::from(ROOT);
    let mut reg = host_tools(root.clone());
    reg.register(ReadRawTool::new(root.clone()));
    reg.register(GlobJsonTool::new(root.clone()));
    reg.register(GrepJsonTool::new(root.clone()));
    reg.register(BashTool::new(root));
    reg
}

const ROOT: &str = "/p";

fn run(workdir: &str, tool: &str, input: Value) -> Option<Value> {
    rewrite(
        &registry(),
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
    assert_eq!(
        rewrite(&registry(), Path::new(ROOT), None, "read", &input),
        None
    );
    assert_eq!(run(ROOT, "read", json!({"path": "a.rs"})), None);
}

#[test]
fn file_tools_join_a_relative_path_root_relative() {
    for tool in ["read", "read_raw", "edit", "write", "apply_patch"] {
        assert_eq!(
            run("/p/sub", tool, json!({"path": "a.rs", "x": 1})),
            Some(json!({"path": "sub/a.rs", "x": 1})),
            "{tool}"
        );
    }
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
fn bash_defaults_its_cwd_and_joins_a_relative_workdir() {
    assert_eq!(
        run("/p/sub", "bash", json!({"command": "ls", "workdir": "t"})),
        Some(json!({"command": "ls", "workdir": "sub/t"}))
    );
    assert_eq!(
        run("/p/sub", "bash", json!({"command": "ls"})),
        Some(json!({"command": "ls", "workdir": "sub"}))
    );
}

#[test]
fn other_tools_and_bad_input_are_untouched() {
    assert_eq!(run("/p/sub", "agent", json!({"prompt": "x"})), None);
    assert_eq!(
        rewrite(
            &registry(),
            Path::new(ROOT),
            Some(Path::new("/p/sub")),
            "read",
            "not json"
        ),
        None
    );
}

/// ADR-0109: the file tools are escape-checked on `path`, `bash` on its
/// `workdir`; a search (ADR-0132), `read_raw` and the JSON variants never
/// trip the gate. A host tool losing its `escape_target` would silently drop
/// the approval prompt, so the whole table is pinned here.
#[test]
fn escape_targets_per_host_tool() {
    let reg = registry();
    #[cfg(feature = "web-fetch")]
    let reg = {
        let mut reg = reg;
        reg.register(super::WebFetchTool::new().expect("web_fetch client builds"));
        reg
    };
    let input = json!({"path": "/etc/x", "workdir": "/tmp", "pattern": "*",
        "command": "ls", "url": "https://a.example/"})
    .to_string();
    let expected: &[(&str, Option<&str>)] = &[
        ("read", Some("/etc/x")),
        ("edit", Some("/etc/x")),
        ("write", Some("/etc/x")),
        ("apply_patch", Some("/etc/x")),
        ("bash", Some("/tmp")),
        ("read_raw", None),
        ("glob", None),
        ("grep", None),
        ("glob_json", None),
        ("grep_json", None),
        #[cfg(feature = "web-fetch")]
        ("web_fetch", None),
    ];
    for (tool, want) in expected {
        assert!(reg.get(tool).is_some(), "{tool} is registered");
        assert_eq!(
            escape_target(tool, &reg, &input).as_deref(),
            *want,
            "{tool}"
        );
    }
    let no_workdir = json!({"command": "ls"}).to_string();
    assert_eq!(escape_target("bash", &reg, &no_workdir), None);
    assert_eq!(escape_target("read", &reg, "not json"), None);
}
