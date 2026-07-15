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
        hooks: raw.hooks,
        mcp: raw.mcp,
        web_search: raw.web_search,
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
fn comment_only_user_file_is_a_no_op() {
    // The scaffolded template (#219) is fully commented → parses to `Null`. It
    // must not wipe the embedded defaults in the merge, and it must not surface as
    // a discovered layer (it sets nothing).
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let user_file = root.join("scaffold.yml");
    std::fs::write(&user_file, TEMPLATE_YML).unwrap();

    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::env::set_var(CONFIG_FILE_ENV, &user_file);
    let resolved = Config::resolve(root).unwrap();
    std::env::remove_var(CONFIG_FILE_ENV);

    let c = &resolved.config;
    assert_eq!(c.agent.as_deref(), Some("build"));
    assert_eq!(c.provider, None);
    assert_eq!(c.permissions.default, Permission::Allow);
    // Only the embedded default layer — the comment-only file is skipped.
    assert_eq!(resolved.layers.len(), 1);
    assert_eq!(resolved.layers[0].0, ConfigLayer::Default);
}

#[test]
fn scaffold_writes_template_when_missing_then_leaves_it_alone() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("nested").join("config.yml");

    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::env::set_var(CONFIG_FILE_ENV, &target);

    // First run creates the file (and its parent dir) with the template.
    let written = scaffold_if_missing().unwrap();
    assert_eq!(written.as_deref(), Some(target.as_path()));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), TEMPLATE_YML);

    // A subsequent run must not overwrite a user's edits.
    std::fs::write(&target, "provider: openai\n").unwrap();
    let again = scaffold_if_missing().unwrap();
    std::env::remove_var(CONFIG_FILE_ENV);
    assert_eq!(again, None);
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "provider: openai\n"
    );
}

#[test]
fn scaffolded_template_is_fully_commented() {
    // Guard the shipped template's "pure no-op" property: every setting is
    // commented out, so it parses to `Null` and the null-skip in `read_layer`
    // keeps it from touching the merge until a user uncomments a key. It must
    // still be valid YAML (a stray syntax error would be a loud loader error).
    let doc: Value = serde_yaml::from_str(TEMPLATE_YML).unwrap();
    assert!(
        doc.is_null(),
        "template must be fully commented, got: {doc:?}"
    );
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

#[test]
fn hooks_default_to_empty() {
    // No `hooks:` section ⇒ every lifecycle list is empty (a no-op).
    assert!(defaults().hooks.is_empty());
}

#[test]
fn hooks_parse_from_user_file() {
    let c = merge_user(
        "hooks:\n  \
         pre_tool_use:\n    \
         - command: \"echo hi\"\n      \
         tools: [bash, edit]\n  \
         user_prompt_submit:\n    \
         - command: \"logger\"\n",
    );
    assert_eq!(c.hooks.pre_tool_use.len(), 1);
    assert_eq!(c.hooks.pre_tool_use[0].command, "echo hi");
    assert_eq!(c.hooks.pre_tool_use[0].tools, vec!["bash", "edit"]);
    assert!(c.hooks.post_tool_use.is_empty());
    assert_eq!(c.hooks.user_prompt_submit.len(), 1);
    assert!(!c.hooks.is_empty());
}

#[test]
fn unknown_hook_event_is_a_loud_error() {
    // `deny_unknown_fields` on `Hooks` rejects a typo'd lifecycle name.
    let base: Value = serde_yaml::from_str(DEFAULTS_YML).unwrap();
    let over: Value = serde_yaml::from_str("hooks:\n  pre_tool_yoos:\n    - command: x\n").unwrap();
    let merged = merge_value(base, over);
    assert!(serde_yaml::from_value::<RawConfig>(merged).is_err());
}

#[test]
fn web_search_defaults_to_disabled() {
    // No `web_search:` section ⇒ disabled with no knobs (a no-op).
    let c = defaults();
    assert!(!c.web_search.enabled);
    assert_eq!(c.web_search.max_uses, None);
    assert!(c.web_search.allowed_domains.is_empty());
}

#[test]
fn web_search_parses_from_user_file() {
    let c = merge_user(
        "web_search:\n  \
         enabled: true\n  \
         max_uses: 5\n  \
         allowed_domains: [docs.rs, example.com]\n",
    );
    assert!(c.web_search.enabled);
    assert_eq!(c.web_search.max_uses, Some(5));
    assert_eq!(c.web_search.allowed_domains, vec!["docs.rs", "example.com"]);
}

#[test]
fn web_search_merges_key_wise_over_layers() {
    // A layer enabling web search must not have to restate every knob; the
    // mapping merges key-wise like every other section.
    let base: Value = serde_yaml::from_str(DEFAULTS_YML).unwrap();
    let user: Value = serde_yaml::from_str("web_search:\n  max_uses: 3\n").unwrap();
    let repo: Value = serde_yaml::from_str("web_search:\n  enabled: true\n").unwrap();
    let merged = merge_value(merge_value(base, user), repo);
    let raw: RawConfig = serde_yaml::from_value(merged).unwrap();
    assert!(raw.web_search.enabled);
    assert_eq!(raw.web_search.max_uses, Some(3));
}

#[test]
fn unknown_web_search_field_is_a_loud_error() {
    // `deny_unknown_fields` on `WebSearchConfig` rejects a typo'd knob.
    let base: Value = serde_yaml::from_str(DEFAULTS_YML).unwrap();
    let over: Value = serde_yaml::from_str("web_search:\n  enabbled: true\n").unwrap();
    let merged = merge_value(base, over);
    let err = serde_yaml::from_value::<RawConfig>(merged).unwrap_err();
    assert!(err.to_string().contains("enabbled"), "got: {err}");
}
