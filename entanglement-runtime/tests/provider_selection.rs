//! `skutter` provider selection: an explicit `ENTANGLEMENT_PROVIDER` whose key
//! env var is missing must exit cleanly (code 2), not panic (issue #106 part 1).

use std::process::Command;

/// Run `skutter run hi` with `ENTANGLEMENT_PROVIDER=<provider>` set and its key
/// env var removed, returning the finished output.
fn run_missing_key(provider: &str, key_env: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_skutter"))
        .args(["run", "hi"])
        .env("ENTANGLEMENT_PROVIDER", provider)
        .env_remove(key_env)
        .output()
        .expect("failed to spawn skutter")
}

#[test]
fn missing_zai_key_exits_cleanly() {
    let out = run_missing_key("zai", "ZAI_API_KEY");
    assert_eq!(out.status.code(), Some(2), "expected clean exit code 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ZAI_API_KEY"),
        "stderr should name the missing key env var, got: {stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "must not panic, got: {stderr}"
    );
}

#[test]
fn missing_openai_key_exits_cleanly() {
    let out = run_missing_key("openai", "OPENAI_API_KEY");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("OPENAI_API_KEY"), "got: {stderr}");
    assert!(!stderr.contains("panicked"), "got: {stderr}");
}

#[test]
fn missing_anthropic_key_exits_cleanly() {
    let out = run_missing_key("anthropic", "ANTHROPIC_API_KEY");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("ANTHROPIC_API_KEY"), "got: {stderr}");
    assert!(!stderr.contains("panicked"), "got: {stderr}");
}

#[test]
fn unknown_provider_exits_cleanly() {
    let out = Command::new(env!("CARGO_BIN_EXE_skutter"))
        .args(["run", "hi"])
        .env("ENTANGLEMENT_PROVIDER", "nope")
        .output()
        .expect("failed to spawn skutter");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown provider='nope'"), "got: {stderr}");
}

/// A provider defined only in the user override YAML is selectable by name — the
/// whole point of the catalog. With its key env missing it exits cleanly naming
/// that env, which proves the lookup found the custom entry.
#[test]
fn user_defined_provider_is_looked_up() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("providers.yml");
    std::fs::write(
        &path,
        "providers:\n\
         \x20 - name: myproxy\n\
         \x20   base_url: http://localhost:9/v1\n\
         \x20   key_env: MYPROXY_KEY\n\
         \x20   default_model: custom-1\n\
         \x20   models:\n\
         \x20     - id: custom-1\n",
    )
    .expect("write user catalog");

    let out = Command::new(env!("CARGO_BIN_EXE_skutter"))
        .args(["run", "hi"])
        .env("ENTANGLEMENT_PROVIDERS_FILE", &path)
        .env("ENTANGLEMENT_PROVIDER", "myproxy")
        .env_remove("MYPROXY_KEY")
        .output()
        .expect("failed to spawn skutter");
    assert_eq!(out.status.code(), Some(2), "expected clean exit code 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("MYPROXY_KEY"), "got: {stderr}");
    assert!(
        !stderr.contains("unknown ENTANGLEMENT_PROVIDER"),
        "got: {stderr}"
    );
    assert!(!stderr.contains("panicked"), "got: {stderr}");
}

/// A malformed user override is a loud error, never a silent fallback.
#[test]
fn malformed_user_catalog_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("providers.yml");
    // `deny_unknown_fields` should reject the misspelled key.
    std::fs::write(&path, "providers:\n  - name: zai\n    typo_field: 1\n")
        .expect("write user catalog");

    let out = Command::new(env!("CARGO_BIN_EXE_skutter"))
        .args(["run", "hi"])
        .env("ENTANGLEMENT_PROVIDERS_FILE", &path)
        .output()
        .expect("failed to spawn skutter");
    assert_ne!(out.status.code(), Some(0), "malformed catalog must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("provider catalog") || stderr.contains("typo_field"),
        "got: {stderr}"
    );
}
