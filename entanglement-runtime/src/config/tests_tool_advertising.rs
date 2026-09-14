//! Tests for the `tool_advertising` config key (ADR-0196, Phase P1).
//!
//! Lives in its own module so `tests.rs` stays under the 400-line cap, mirroring
//! `tests_retention.rs`. The env var `ENTANGLEMENT_TOOL_ADVERTISING` is
//! process-global and read on every resolve, so every test below serializes on
//! [`super::ENV_LOCK`] — even the ones that don't themselves touch the env — so
//! a sibling test's set/remove can't race into another's resolve.
//!
//! The full precedence chain (env > config > catalog > default) is exercised in
//! `crate::tool_advertising`'s unit tests against a `Config` built here; these
//! tests cover the config *layer* itself: parse, deny-unknown, layer merge,
//! provenance.

use entanglement_core::ToolAdvertising;

use super::tests::{defaults, merge_user};
use super::*;

#[test]
fn tool_advertising_defaults_to_none() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // None means "no install-wide opinion" — the catalog (then tool_search)
    // decides per model, so a fresh install is behaviorally unchanged from
    // the resolver's own default.
    assert_eq!(defaults().tool_advertising, None);
}

#[test]
fn tool_advertising_parses_both_values() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    assert_eq!(
        merge_user("tool_advertising: tool_search\n").tool_advertising,
        Some(ToolAdvertising::ToolSearch)
    );
    // An explicit `full` is a real override — it pins every model onto the
    // full advertised surface off any catalog `tool_advertising: tool_search`
    // preference, distinct from absent.
    assert_eq!(
        merge_user("tool_advertising: full\n").tool_advertising,
        Some(ToolAdvertising::Full)
    );
}

#[test]
fn tool_advertising_unknown_value_is_a_loud_error() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // A typo'd mode is a validation error (unknown enum variant), not a
    // silent null — same loudness as every other miskeyed config value.
    let base: Value = serde_yaml::from_str(DEFAULTS_YML).unwrap();
    let over: Value = serde_yaml::from_str("tool_advertising: banana\n").unwrap();
    let err = serde_yaml::from_value::<RawConfig>(merge_value(base, over)).unwrap_err();
    assert!(err.to_string().contains("banana"), "got: {err}");
}

#[test]
fn tool_advertising_project_layer_wins_over_user() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let user_file = root.join("user.yml");
    std::fs::write(&user_file, "tool_advertising: tool_search\n").unwrap();
    let repo_file = root.join(".entanglement").join("config.yml");
    std::fs::create_dir_all(repo_file.parent().unwrap()).unwrap();
    std::fs::write(&repo_file, "tool_advertising: full\n").unwrap();

    std::env::set_var(CONFIG_FILE_ENV, &user_file);
    let resolved = Config::resolve(root).unwrap();
    std::env::remove_var(CONFIG_FILE_ENV);

    assert_eq!(
        resolved.config.tool_advertising,
        Some(ToolAdvertising::Full)
    );
    let prov: std::collections::HashMap<_, _> = resolved.provenance.iter().cloned().collect();
    assert_eq!(prov.get("tool_advertising"), Some(&ConfigLayer::Project));
}
