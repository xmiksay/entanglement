//! Unit tests for the layered user config (#172).
//!
//! Layer discovery reads a process-global env var (`ENTANGLEMENT_CONFIG_FILE`)
//! and the repo file under `root`, so tests that exercise the file layers set the
//! env var under a shared lock and point it at a temp file, mirroring the
//! agents/skills discovery tests.

use std::sync::Mutex;

use entanglement_core::Permission;

use super::*;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// The embedded defaults alone, parsed the way the loader does. Guards that the
/// `.expect` in [`super::default_layer`] is provably unreachable.
fn defaults() -> Config {
    parse(&[default_layer()]).unwrap().config
}

#[test]
fn builtin_parses_with_expected_defaults() {
    let c = defaults();
    // The embedded defaults are the pre-config behavior: build agent, auto-detect
    // provider, provider-default model, non-verbose, allow-all ceiling.
    assert_eq!(c.agent.as_deref(), Some("build"));
    assert_eq!(c.provider, None);
    assert_eq!(c.model, None);
    assert!(!c.verbose);
    assert_eq!(c.permissions.for_tool("bash"), Permission::Allow);
    assert_eq!(c.permissions.default, Permission::Allow);
}

/// Merge a user-file YAML string over the embedded defaults, the way the loader
/// does, without touching the filesystem.
fn merge_user(user: &str) -> Config {
    let base: Value = serde_yaml::from_str(DEFAULTS_YML).unwrap();
    let over: Value = serde_yaml::from_str(user).unwrap();
    let merged = merge_value(base, over);
    let raw: RawConfig = serde_yaml::from_value(merged).unwrap();
    let permissions = match &raw.permissions {
        Some(v) => permission_from_value(v).unwrap(),
        None => PermissionProfile::new(Permission::Allow),
    };
    Config {
        agent: raw.agent,
        provider: raw.provider,
        model: raw.model,
        verbose: raw.verbose,
        permissions,
    }
}

#[test]
fn field_override_keeps_siblings() {
    // Overriding one scalar must not reset the others to nothing.
    let c = merge_user("provider: anthropic\n");
    assert_eq!(c.provider.as_deref(), Some("anthropic"));
    // Untouched siblings survive from the embedded defaults.
    assert_eq!(c.agent.as_deref(), Some("build"));
    assert!(!c.verbose);
    assert_eq!(c.permissions.default, Permission::Allow);
}

#[test]
fn permissions_merge_key_wise() {
    // A user adds a `bash: ask` rule; the embedded `default: allow` survives
    // because the two mappings merge key-wise (not whole-block replace).
    let c = merge_user("permissions:\n  bash: ask\n");
    assert_eq!(c.permissions.default, Permission::Allow);
    assert_eq!(c.permissions.for_tool("bash"), Permission::Ask);
    assert_eq!(c.permissions.for_tool("read"), Permission::Allow);
}

#[test]
fn permissions_default_can_be_overridden() {
    let c = merge_user("permissions:\n  default: ask\n  read: allow\n");
    assert_eq!(c.permissions.default, Permission::Ask);
    assert_eq!(c.permissions.for_tool("read"), Permission::Allow);
    assert_eq!(c.permissions.for_tool("bash"), Permission::Ask);
}

#[test]
fn unknown_field_is_rejected() {
    let base: Value = serde_yaml::from_str(DEFAULTS_YML).unwrap();
    let over: Value = serde_yaml::from_str("typo_field: 1\n").unwrap();
    let err = serde_yaml::from_value::<RawConfig>(merge_value(base, over)).unwrap_err();
    assert!(err.to_string().contains("typo_field"), "got: {err}");
}

#[test]
fn project_layer_wins_over_user() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // User file: anthropic provider, verbose.
    let user_file = root.join("user-config.yml");
    std::fs::write(&user_file, "provider: anthropic\nverbose: true\n").unwrap();

    // Repo file overrides the provider and adds a permission rule.
    let repo_dir = root.join(".entanglement");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(
        repo_dir.join("config.yml"),
        "provider: openai\npermissions:\n  bash: deny\n",
    )
    .unwrap();

    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::env::set_var(CONFIG_FILE_ENV, &user_file);
    let resolved = Config::resolve(root).unwrap();
    std::env::remove_var(CONFIG_FILE_ENV);

    let c = &resolved.config;
    // Repo wins on the overlapping `provider`; the user's `verbose` (repo silent)
    // survives; the embedded `agent` (both files silent) survives.
    assert_eq!(c.provider.as_deref(), Some("openai"));
    assert!(c.verbose);
    assert_eq!(c.agent.as_deref(), Some("build"));
    assert_eq!(c.permissions.for_tool("bash"), Permission::Deny);

    // Provenance names the winning layer per field.
    let prov: std::collections::HashMap<_, _> = resolved.provenance.iter().cloned().collect();
    assert_eq!(prov.get("provider"), Some(&ConfigLayer::Project));
    assert_eq!(prov.get("verbose"), Some(&ConfigLayer::User));
    assert_eq!(prov.get("agent"), Some(&ConfigLayer::Default));
    assert_eq!(prov.get("permissions"), Some(&ConfigLayer::Project));
    // All three layers were discovered.
    assert_eq!(resolved.layers.len(), 3);
}

#[test]
fn missing_files_fall_back_to_embedded() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // Point the env at a path that does not exist; no repo file either.
    std::env::set_var(CONFIG_FILE_ENV, root.join("nope.yml"));
    let c = Config::load(root).unwrap();
    std::env::remove_var(CONFIG_FILE_ENV);
    assert_eq!(c.agent.as_deref(), Some("build"));
    assert_eq!(c.provider, None);
}

#[test]
fn malformed_user_file_is_a_loud_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let user_file = root.join("bad.yml");
    std::fs::write(&user_file, "provider: [unterminated\n").unwrap();
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::env::set_var(CONFIG_FILE_ENV, &user_file);
    let err = Config::load(root).unwrap_err();
    std::env::remove_var(CONFIG_FILE_ENV);
    assert!(
        format!("{err:#}").contains("parsing user config"),
        "got: {err:#}"
    );
}
