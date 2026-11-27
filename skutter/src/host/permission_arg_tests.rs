//! Each host tool's permission argument and workdir (#962), asked through
//! the generic [`crate::permission`] entry points over the real host
//! registry — the shapes argument- and workdir-scoped rules match.

use crate::permission::{permission_arg, permission_scope, permission_workdir};
use crate::tool_names::is_non_maskable;
use crate::tools::{ArgKind, PermissionScope};

fn arg(tool: &str, input: &str) -> Option<String> {
    permission_arg(&super::every_tool("/p".into()), tool, input)
}

fn workdir(tool: &str, input: &str) -> Option<String> {
    permission_workdir(&super::every_tool("/p".into()), tool, input)
}

#[test]
fn permission_arg_extracts_per_tool_shape() {
    // bash → the raw command.
    assert_eq!(
        arg("bash", r#"{"command":"git status"}"#).as_deref(),
        Some("git status")
    );
    // edit/write/read/apply_patch → the target path.
    assert_eq!(
        arg(
            "edit",
            r#"{"path":"src/main.rs","oldString":"a","newString":"b"}"#
        )
        .as_deref(),
        Some("src/main.rs")
    );
    assert_eq!(
        arg("write", r#"{"path":"README.md","content":"x"}"#).as_deref(),
        Some("README.md")
    );
    assert_eq!(
        arg(
            "apply_patch",
            r#"{"path":"src/lib.rs","patch":"@@ -1 +1 @@"}"#
        )
        .as_deref(),
        Some("src/lib.rs")
    );
    // glob → the pattern itself, since a glob pattern is a path.
    assert_eq!(
        arg("glob", r#"{"pattern":"src/*.rs"}"#).as_deref(),
        Some("src/*.rs")
    );
    // grep → the optional file filter, which is a path; the regex `pattern`
    // is never returned, since it isn't one.
    assert_eq!(
        arg("grep", r#"{"pattern":"foo","path":"src/*"}"#).as_deref(),
        Some("src/*")
    );
    // grep without a `path` filter yields None — resolution falls through
    // to grep's name-only rules.
    assert_eq!(arg("grep", r#"{"pattern":"foo"}"#), None);
    // Tools without a meaningful argument, and malformed input, yield None.
    assert_eq!(arg("bash", "not json"), None);
}

/// ADR-0150: glob's optional `path` base dir joins onto the pattern for
/// grading, matching what the tool actually walks.
#[test]
fn glob_path_plus_pattern_grades_joined() {
    assert_eq!(
        arg("glob", r#"{"pattern":"**/*.rs","path":"src"}"#).as_deref(),
        Some("src/**/*.rs")
    );
}

/// ADR-0150 regression: grep's graded arg stays the raw `path` even when
/// it names a directory — the walk's `dir/**/*` auto-expansion happens
/// after grading and must never leak into it.
#[test]
fn grep_graded_arg_is_raw_path_even_for_directory() {
    assert_eq!(
        arg("grep", r#"{"pattern":"foo","path":"src/tui"}"#).as_deref(),
        Some("src/tui")
    );
}

#[test]
fn permission_workdir_extracts_bash_only() {
    assert_eq!(
        workdir("bash", r#"{"command":"ls","workdir":"/tmp"}"#).as_deref(),
        Some("/tmp")
    );
    // No `workdir` field, a tool with no workdir concept, and malformed
    // input all yield None.
    assert_eq!(workdir("bash", r#"{"command":"ls"}"#), None);
    assert_eq!(workdir("read", r#"{"path":"x","workdir":"/tmp"}"#), None);
    assert_eq!(workdir("bash", "not json"), None);
}

/// `web_fetch`'s argument is normalized exactly as sent.
#[test]
fn web_fetch_grades_its_normalized_url() {
    // web_fetch → the URL (#736).
    assert_eq!(
        arg("web_fetch", r#"{"url":"https://docs.rs/x"}"#).as_deref(),
        Some("https://docs.rs/x")
    );
    // …normalized exactly as the fetcher will request it.
    assert_eq!(
        arg("web_fetch", r#"{"url":" HTTPS://PASTEBIN.COM.:443/x"}"#).as_deref(),
        Some("https://pastebin.com/x")
    );
    assert_eq!(arg("web_fetch", r#"{"url":"nope"}"#), None);
}

/// One input carrying every field any tool's argument or workdir reads,
/// so each tool that declares one yields it from this input.
const EVERY_FIELD: &str = r#"{"agent":"plan","agent_id":"s-1","command":"x",
    "path":"src/a.rs","pattern":"*.rs","url":"https://a.example/","method":"GET",
    "workdir":"/tmp"}"#;

/// #878/#881/#962: what a tool declares agrees with what it extracts,
/// for every host tool registered the way `skutter` registers it.
#[test]
fn the_declared_scope_matches_what_is_extracted() {
    let tools = super::every_tool("/p".into());
    let names = tools.names();
    for tool in names.iter().map(String::as_str).chain(["mcp__gh__create"]) {
        let scope = permission_scope(&tools, tool);
        assert_eq!(
            scope.arg != ArgKind::None,
            permission_arg(&tools, tool, EVERY_FIELD).is_some(),
            "{tool}"
        );
        assert_eq!(
            scope.workdir,
            permission_workdir(&tools, tool, EVERY_FIELD).is_some(),
            "{tool}"
        );
    }
    assert_eq!(
        permission_scope(&tools, "mcp__gh__create"),
        PermissionScope::default()
    );
}

/// Every gradable host tool is scopable today; only the non-maskable
/// ones (never graded under their own name) take no argument. A new
/// maskable tool that forgets its `permission_arg` would grade every
/// scoped rule as a silent no-op, so this trips until the omission is
/// deliberate.
#[test]
fn every_maskable_known_tool_takes_an_argument() {
    let tools = super::every_tool("/p".into());
    let names = tools.names();
    for tool in names.iter().map(String::as_str) {
        // A tool graded as another (`call_grading`) is never graded
        // under its own name either.
        let graded_as_itself = tools
            .get(tool)
            .is_none_or(|t| t.call_grading("{}").is_none());
        assert_eq!(
            permission_scope(&tools, tool).arg != ArgKind::None,
            !is_non_maskable(tool) && graded_as_itself,
            "{tool}: maskable but declares no argument — implement \
             `Tool::permission_scope`, or exempt it here if it truly takes none"
        );
    }
}
