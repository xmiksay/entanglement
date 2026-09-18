//! `skutter` provider selection: an explicit `ENTANGLEMENT_PROVIDER` whose key
//! env var is missing must exit cleanly (code 2), not panic (issue #106 part 1).

use std::process::Command;

/// A managed-file path guaranteed not to exist, so a child `skutter` process
/// never picks up state from the developer's *real*
/// `${config_dir}/entanglement/` — a real `.env` key would defeat
/// `env_remove` and make a genuine, hanging network call instead of exiting
/// on the missing-key path (#220's "env file loaded at startup" behavior);
/// a real `config.yml` is read unconditionally at startup regardless of
/// which provider/key env vars this test sets (`ENTANGLEMENT_CONFIG_FILE` is
/// consulted before any provider selection at all), so *every* spawn below
/// needs this override too, not just the env-file one — found the hard way:
/// a real config.yml this migration briefly mis-rewrote (ADR-0207 stage 6c)
/// made every test in this file fail on an unrelated "loading user config"
/// error instead of the provider-selection path they actually exercise.
/// `label` keys the temp name so two overrides in the same process (env
/// file + config file) never collide on one path.
fn no_managed_file(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "entanglement-test-no-such-{label}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ))
}

/// Every managed-file override a spawned `skutter` needs to run fully
/// isolated from the developer's real `${config_dir}/entanglement/` (see
/// [`no_managed_file`]).
fn isolated(cmd: &mut Command) -> &mut Command {
    cmd.env("ENTANGLEMENT_ENV_FILE", no_managed_file("env"))
        .env("ENTANGLEMENT_CONFIG_FILE", no_managed_file("config"))
}

/// Run `skutter run hi` with `ENTANGLEMENT_PROVIDER=<provider>` set and its key
/// env var removed, returning the finished output.
fn run_missing_key(provider: &str, key_env: &str) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_skutter"));
    cmd.args(["run", "hi"])
        .env("ENTANGLEMENT_PROVIDER", provider)
        .env_remove(key_env);
    isolated(&mut cmd)
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
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_skutter"));
    cmd.args(["run", "hi"]).env("ENTANGLEMENT_PROVIDER", "nope");
    let out = isolated(&mut cmd)
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

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_skutter"));
    cmd.args(["run", "hi"])
        .env("ENTANGLEMENT_PROVIDERS_FILE", &path)
        .env("ENTANGLEMENT_PROVIDER", "myproxy")
        .env_remove("MYPROXY_KEY");
    let out = isolated(&mut cmd)
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

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_skutter"));
    cmd.args(["run", "hi"])
        .env("ENTANGLEMENT_PROVIDERS_FILE", &path);
    let out = isolated(&mut cmd)
        .output()
        .expect("failed to spawn skutter");
    assert_ne!(out.status.code(), Some(0), "malformed catalog must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("provider catalog") || stderr.contains("typo_field"),
        "got: {stderr}"
    );
}
