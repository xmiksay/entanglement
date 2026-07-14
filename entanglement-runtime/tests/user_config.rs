//! `skutter inspect config` (#172): the layered user config resolved end-to-end
//! through the binary — embedded default < user < repository, later wins. Guards
//! the `main.rs` wiring the in-crate unit tests don't reach.

use std::process::Command;

/// Run `skutter inspect config` in `cwd` with the user config path env pointed at
/// `user_config` (may be a non-existent path to exercise the embedded fallback).
fn inspect_config(cwd: &std::path::Path, user_config: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_skutter"))
        .args(["inspect", "config"])
        .current_dir(cwd)
        .env("ENTANGLEMENT_CONFIG_FILE", user_config)
        .output()
        .expect("failed to spawn skutter")
}

#[test]
fn embedded_defaults_when_no_files() {
    let dir = tempfile::tempdir().unwrap();
    let out = inspect_config(dir.path(), &dir.path().join("nope.yml"));
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("agent:    build"), "got: {stdout}");
    assert!(stdout.contains("provider: (auto-detect)"), "got: {stdout}");
    assert!(stdout.contains("default: Allow"), "got: {stdout}");
}

#[test]
fn repo_layer_overrides_user_layer() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let user_file = root.join("user.yml");
    std::fs::write(&user_file, "provider: anthropic\nverbose: true\n").unwrap();
    let repo = root.join(".entanglement");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("config.yml"),
        "provider: openai\npermissions:\n  bash: deny\n",
    )
    .unwrap();

    let out = inspect_config(root, &user_file);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Repo wins the overlapping `provider`; the user's `verbose` survives; the
    // embedded `agent` survives; the repo's permission rule reaches the ceiling.
    assert!(
        stdout.contains("provider: openai       ← project"),
        "got: {stdout}"
    );
    assert!(
        stdout.contains("verbose:  true         ← user"),
        "got: {stdout}"
    );
    assert!(
        stdout.contains("agent:    build        ← default"),
        "got: {stdout}"
    );
    assert!(stdout.contains("bash: Deny"), "got: {stdout}");
}

#[test]
fn first_run_scaffolds_a_commented_template() {
    // #219: on first run the binary writes a starter config where the file is
    // missing, then resolves to the embedded defaults (the template is fully
    // commented, so it changes nothing). Guards the `main.rs` scaffold wiring.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("entanglement").join("config.yml");
    assert!(!cfg.exists());

    let out = inspect_config(dir.path(), &cfg);
    assert_eq!(out.status.code(), Some(0));

    // The file now exists, is fully commented, and left the defaults in force.
    let written = std::fs::read_to_string(&cfg).unwrap();
    assert!(written.contains("#agent: build"), "got: {written}");
    assert!(
        written.contains("scaffolded on first run"),
        "got: {written}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("agent:    build"), "got: {stdout}");
}

#[test]
fn configured_hooks_show_in_inspect() {
    // #199: a `hooks:` section resolves end-to-end and `inspect config` renders it,
    // including the per-tool filter. Empty ⇒ `(none)`.
    let dir = tempfile::tempdir().unwrap();
    let user_file = dir.path().join("user.yml");
    std::fs::write(
        &user_file,
        "hooks:\n  pre_tool_use:\n    - command: \"echo hi\"\n      tools: [bash]\n",
    )
    .unwrap();

    let out = inspect_config(dir.path(), &user_file);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("pre_tool_use:"), "got: {stdout}");
    assert!(stdout.contains("echo hi"), "got: {stdout}");
    assert!(stdout.contains("[tools: bash]"), "got: {stdout}");
}

#[test]
fn no_hooks_show_as_none() {
    let dir = tempfile::tempdir().unwrap();
    let out = inspect_config(dir.path(), &dir.path().join("nope.yml"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("hooks (← default):"), "got: {stdout}");
    assert!(stdout.contains("(none)"), "got: {stdout}");
}

#[test]
fn malformed_config_exits_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.yml");
    std::fs::write(&bad, "provider: [unterminated\n").unwrap();
    let out = inspect_config(dir.path(), &bad);
    // A loud error, never a panic or a silent fallback.
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("parsing user config"), "got: {stderr}");
    assert!(!stderr.contains("panicked"), "got: {stderr}");
}
