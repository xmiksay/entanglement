//! Tests for the `session_retention_days` config key (Issue 4, Phase 4.2).
//!
//! Lives in its own module so `tests.rs` stays under the 400-line cap. The env
//! var `ENTANGLEMENT_SESSION_RETENTION_DAYS` is process-global and read on every
//! resolve, so every test below serializes on [`super::ENV_LOCK] — even the ones
//! that don't themselves touch the env — so a sibling test's set/remove can't
//! race into another's resolve.

use super::tests::{defaults, merge_user};
use super::*;

#[test]
fn session_retention_defaults_to_30() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // The embedded default keeps the on-disk footprint bounded without a user
    // having to configure anything.
    assert_eq!(defaults().session_retention_days, 30);
}

#[test]
fn session_retention_parses_from_user_file_and_keeps_siblings() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let c = merge_user("session_retention_days: 7\n");
    assert_eq!(c.session_retention_days, 7);
    assert_eq!(c.agent.as_deref(), Some("build"));
}

#[test]
fn session_retention_env_overrides_config() {
    // Env > config > default.
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::env::set_var("ENTANGLEMENT_SESSION_RETENTION_DAYS", "90");
    let c = merge_user("session_retention_days: 7\n");
    assert_eq!(c.session_retention_days, 90);
    std::env::remove_var("ENTANGLEMENT_SESSION_RETENTION_DAYS");
}

#[test]
fn session_retention_env_overrides_default_when_config_absent() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::env::set_var("ENTANGLEMENT_SESSION_RETENTION_DAYS", "5");
    assert_eq!(defaults().session_retention_days, 5);
    std::env::remove_var("ENTANGLEMENT_SESSION_RETENTION_DAYS");
}

#[test]
fn session_retention_unparseable_env_falls_back_to_config() {
    // A typo'd env value is logged + ignored, not fatal — config/default wins.
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::env::set_var("ENTANGLEMENT_SESSION_RETENTION_DAYS", "not-a-number");
    let c = merge_user("session_retention_days: 14\n");
    assert_eq!(c.session_retention_days, 14);
    std::env::remove_var("ENTANGLEMENT_SESSION_RETENTION_DAYS");
}

#[test]
fn session_retention_provenance_reports_winning_layer() {
    // A user-file override surfaces in `Resolved::provenance` like every other
    // top-level key — the inspect-config surface must not miss it.
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let user_file = root.join("user.yml");
    std::fs::write(&user_file, "session_retention_days: 21\n").unwrap();

    std::env::set_var(CONFIG_FILE_ENV, &user_file);
    let resolved = Config::resolve(root).unwrap();
    std::env::remove_var(CONFIG_FILE_ENV);

    let prov: std::collections::HashMap<_, _> = resolved.provenance.iter().cloned().collect();
    assert_eq!(prov.get("session_retention_days"), Some(&ConfigLayer::User));
    assert_eq!(resolved.config.session_retention_days, 21);
}
