//! User-defined slash commands end to end through the real `skutter` binary
//! (#733): `skutter run "/name args"` expands the template before the prompt
//! reaches the engine (the echo backend prints the user text back), and
//! `skutter inspect commands` lists what resolved. Every child runs in a
//! temp project with `HOME`/XDG pointed into its own sandbox.

use std::path::Path;
use std::process::Output;

use crate::skutter_cmd::isolated_skutter;

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
    std::fs::write(path, content).expect("write command file");
}

/// A temp project carrying a native `review` command, a namespaced foreign
/// (`.claude`) one, an opencode one, and a native file named like a built-in.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp project");
    let root = dir.path();
    write(
        &root.join(".entanglement/commands/review.md"),
        "---\ndescription: Review a file\nargument-hint: <file> [focus]\n---\n\
         Please review $1 focusing on $2. All: $ARGUMENTS",
    );
    write(
        &root.join(".claude/commands/frontend/lint.md"),
        "---\nallowed-tools: Bash(npm:*)\nmodel: sonnet\n---\nLint the frontend.",
    );
    write(
        &root.join(".opencode/command/ship.md"),
        "---\nagent: build\nsubtask: true\n---\nShip it",
    );
    write(&root.join(".entanglement/commands/help.md"), "never used");
    dir
}

fn skutter(project: &Path, args: &[&str]) -> Output {
    let (mut cmd, _sandbox) = isolated_skutter();
    cmd.current_dir(project)
        .args(args)
        .env("ENTANGLEMENT_PROVIDER", "echo")
        .output()
        .expect("failed to spawn skutter")
}

fn stdout(out: &Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "skutter failed; stderr: {stderr}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn run_head_expands_a_user_command() {
    let dir = project();
    let out = stdout(&skutter(
        dir.path(),
        &["run", "/review src/a.rs \"error paths\""],
    ));
    assert!(
        out.contains(
            r#"Please review src/a.rs focusing on error paths. All: src/a.rs \"error paths\""#
        ),
        "echo must show the expanded template; got: {out}"
    );
}

#[test]
fn run_head_expands_namespaced_and_appends_args_without_placeholder() {
    let dir = project();
    let out = stdout(&skutter(dir.path(), &["run", "/frontend:lint", "now"]));
    assert!(
        out.contains(r"Lint the frontend.\n\nARGUMENTS: now"),
        "got: {out}"
    );
    let out = stdout(&skutter(dir.path(), &["run", "/ship"]));
    assert!(out.contains("Ship it"), "got: {out}");
}

#[test]
fn run_head_sends_builtin_and_unknown_names_literally() {
    let dir = project();
    let out = stdout(&skutter(dir.path(), &["run", "/help me"]));
    assert!(
        out.contains("/help me"),
        "a built-in name is never expanded: {out}"
    );
    assert!(!out.contains("never used"), "{out}");
    let out = stdout(&skutter(dir.path(), &["run", "/nope x"]));
    assert!(out.contains("/nope x"), "{out}");
}

#[test]
fn inspect_commands_lists_layer_source_and_clash() {
    let dir = project();
    let table = stdout(&skutter(dir.path(), &["inspect", "commands"]));
    assert!(table.contains("/review <file> [focus]"), "{table}");
    assert!(table.contains("/frontend:lint"), "{table}");
    assert!(table.contains("/ship"), "{table}");
    assert!(table.contains("/help (hidden by built-in)"), "{table}");
    assert!(table.contains("project"), "{table}");
    assert!(table.contains("review.md"), "{table}");

    let detail = stdout(&skutter(dir.path(), &["inspect", "commands", "/review"]));
    assert!(detail.contains("description: Review a file"), "{detail}");
    assert!(detail.contains("Please review $1"), "{detail}");

    let missing = skutter(dir.path(), &["inspect", "commands", "zzz"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("unknown command `/zzz`"));
}

#[test]
fn malformed_native_command_is_a_loud_startup_error() {
    let dir = tempfile::tempdir().expect("temp project");
    write(
        &dir.path().join(".entanglement/commands/bad.md"),
        "---\ndescription: d\ntypo: 1\n---\nbody",
    );
    let out = skutter(dir.path(), &["run", "hi"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("typo"), "{stderr}");
}
