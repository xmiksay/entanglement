//! Tests for the `tui:` block and the retired `discovery:` key (ADR-0213)
//! through the real layered loader. Tests that point `ENTANGLEMENT_CONFIG_FILE`
//! at a temp file serialize on [`super::ENV_LOCK`].

use super::tests::{defaults, merge_user};
use super::*;

/// Resolve `root` with the user layer at `user_file`.
fn resolve_with_user(root: &Path, user_file: &Path) -> Resolved {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::env::set_var(CONFIG_FILE_ENV, user_file);
    let resolved = Config::resolve(root);
    std::env::remove_var(CONFIG_FILE_ENV);
    resolved.unwrap()
}

#[test]
fn tui_defaults_match_the_pre_adr_0213_behavior() {
    assert_eq!(defaults().tui, TuiConfig::default());
}

#[test]
fn tui_keys_parse_and_merge_field_by_field() {
    let c = merge_user("tui:\n  notify: true\n");
    assert!(c.tui.notify);
    assert!(c.tui.mouse, "an unset sibling keeps its default");
    assert!(!merge_user("tui: {mouse: false}\n").tui.mouse);
}

#[test]
fn project_and_user_tui_fields_combine_and_report_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let user_file = root.join("user.yml");
    std::fs::write(&user_file, "tui:\n  notify: true\n").unwrap();
    let repo = root.join(".entanglement");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("config.yml"), "tui:\n  mouse: false\n").unwrap();

    let resolved = resolve_with_user(root, &user_file);
    assert_eq!(
        resolved.config.tui,
        TuiConfig {
            notify: true,
            mouse: false
        }
    );
    let prov: HashMap<_, _> = resolved.provenance.iter().cloned().collect();
    assert_eq!(prov.get("tui"), Some(&ConfigLayer::Project));
}

/// What the TUI's reload arm does on a watcher notice: re-resolve and pick up
/// the edited block — no restart.
#[test]
fn an_edited_tui_block_is_seen_on_the_next_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let user_file = dir.path().join("user.yml");
    std::fs::write(&user_file, "tui: {notify: false}\n").unwrap();
    assert!(!resolve_with_user(dir.path(), &user_file).config.tui.notify);
    std::fs::write(&user_file, "tui: {notify: true, mouse: false}\n").unwrap();
    let tui = resolve_with_user(dir.path(), &user_file).config.tui;
    assert!(tui.notify && !tui.mouse);
}

#[test]
fn a_retired_discovery_key_is_ignored_with_one_warning() {
    let dir = tempfile::tempdir().unwrap();
    let user_file = dir.path().join("user.yml");
    std::fs::write(&user_file, "discovery:\n  zai: invoke\nagent: build\n").unwrap();
    let resolved = resolve_with_user(dir.path(), &user_file);
    assert_eq!(resolved.config.agent.as_deref(), Some("build"));
    assert_eq!(resolved.warnings.len(), 1, "{:?}", resolved.warnings);
    assert!(resolved.warnings[0].contains("discovery"));
    assert!(resolved.warnings[0].contains(&user_file.display().to_string()));
}

#[test]
fn the_shipped_defaults_carry_no_retired_key() {
    assert!(parse(&[default_layer()]).unwrap().warnings.is_empty());
}
