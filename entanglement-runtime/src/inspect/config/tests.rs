//! `render_config` tests.

use super::*;

#[test]
fn embedded_defaults_report_the_default_layer_and_engine_fallback_text() {
    let dir = tempfile::tempdir().unwrap();
    let resolved = Config::resolve_with(dir.path(), None).unwrap();
    let rendered = render_config(&resolved);

    assert!(rendered.contains("max_turns: 200") && rendered.contains("← default"));
    assert!(rendered.contains("idle_ttl_secs: (disabled — no auto-hibernation sweep)"));
    assert!(rendered.contains("auto_compact: true"));
    assert!(rendered.contains("editor:   ($VISUAL → $EDITOR → vi)"));
    assert!(rendered.contains("session_retention_days: 30") && rendered.contains("← default"));
    assert!(
        rendered.contains(
            "undo:     enabled=true max_file_bytes=8 MiB max_root_bytes=256 MiB ← default"
        ),
        "{rendered}"
    );
}

#[test]
fn undo_block_renders_effective_sizes_and_its_layer() {
    let dir = tempfile::tempdir().unwrap();
    let user_file = dir.path().join("user-config.yml");
    std::fs::write(&user_file, "undo: {enabled: false, max_root_bytes: 1GiB}\n").unwrap();
    let resolved = Config::resolve_with(dir.path(), Some(&user_file)).unwrap();

    let rendered = render_config(&resolved);
    assert!(
        rendered
            .contains("undo:     enabled=false max_file_bytes=8 MiB max_root_bytes=1 GiB ← user"),
        "{rendered}"
    );
}

#[test]
fn user_layer_overrides_report_their_winning_layer() {
    let dir = tempfile::tempdir().unwrap();
    let user_file = dir.path().join("user-config.yml");
    std::fs::write(
        &user_file,
        "max_turns: 50\nidle_ttl_secs: 120\nauto_compact: false\neditor: \"code --wait\"\n",
    )
    .unwrap();

    let resolved = Config::resolve_with(dir.path(), Some(&user_file)).unwrap();

    let rendered = render_config(&resolved);
    assert!(rendered.contains("max_turns: 50") && rendered.contains("← user"));
    assert!(rendered.contains("idle_ttl_secs: 120") && rendered.contains("← user"));
    assert!(rendered.contains("auto_compact: false") && rendered.contains("← user"));
    assert!(rendered.contains("editor:   code --wait") && rendered.contains("← user"));
}

#[test]
fn tui_block_and_retired_key_warnings_are_rendered() {
    let dir = tempfile::tempdir().unwrap();
    let user_file = dir.path().join("user-config.yml");
    std::fs::write(
        &user_file,
        "tui: {notify: true, statusline: {right: '{mode}', command: 'echo hi'}, \
         theme: {base: light, slots: {diff.add: '#2e7d32'}}, \
         keybindings: {pick_model: [ctrl+o, leader o], export: ctrl+g}}\n\
         discovery: {zai: invoke}\n",
    )
    .unwrap();
    let resolved = Config::resolve_with(dir.path(), Some(&user_file)).unwrap();

    let rendered = render_config(&resolved);
    assert!(
        rendered.contains("tui:      notify=true mouse=true ← user"),
        "{rendered}"
    );
    assert!(
        rendered.contains(r#"tui.statusline: template="{repo} › {cwd_rel}  ⎇ {branch} {worktree}" right="{mode}" command="echo hi" timeout_ms=1000 ← user"#),
        "{rendered}"
    );
    assert!(
        rendered.contains("tui.theme: light [diff.add=#2e7d32] ← user"),
        "{rendered}"
    );
    assert!(
        rendered.contains("tui.keybindings: [export=ctrl+g, pick_model=ctrl+o|leader o] ← user"),
        "{rendered}"
    );
    assert!(
        rendered.contains("warnings:") && rendered.contains("`discovery:`"),
        "{rendered}"
    );
    // The export/jump_attention clash is a warning, not a load failure.
    assert!(rendered.contains("`jump_attention`"), "{rendered}");
}

#[test]
fn rhai_trusted_folders_render_with_their_listing_layer() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::create_dir_all(root.join("mine")).unwrap();
    std::fs::create_dir_all(root.join(".entanglement")).unwrap();
    std::fs::write(
        root.join(".entanglement/config.yml"),
        "rhai: {trusted: [scripts]}\n",
    )
    .unwrap();
    let user_file = root.join("user-config.yml");
    std::fs::write(&user_file, "rhai: {trusted: [mine]}\n").unwrap();
    let rendered = render_config(&Config::resolve_with(&root, Some(&user_file)).unwrap());
    let block = format!(
        "rhai.trusted (the layers' lists add up; canonical folder ← listing layer):\n  {} ← user\n  {} ← project\n",
        root.join("mine").display(),
        root.join("scripts").display()
    );
    assert!(rendered.contains(&block), "{rendered}");
    assert!(!rendered.contains("suppressed"), "{rendered}");

    // A project `prompt` list for a mode drops the user layer's trust there.
    std::fs::write(
        root.join(".entanglement/config.yml"),
        "rhai: {trusted: [scripts]}\nmodes: {build: {prompt: [\"bash(make *)\"]}}\n",
    )
    .unwrap();
    let rendered = render_config(&Config::resolve_with(&root, Some(&user_file)).unwrap());
    assert!(
        rendered.contains("user-layer trust suppressed in mode(s) build"),
        "{rendered}"
    );
    std::fs::write(
        root.join(".entanglement/config.yml"),
        "rhai: {trusted: [scripts]}\npermissions: {prompt: [bash]}\n",
    )
    .unwrap();
    let rendered = render_config(&Config::resolve_with(&root, Some(&user_file)).unwrap());
    assert!(
        rendered.contains("user-layer trust suppressed in every mode"),
        "{rendered}"
    );

    let empty = render_config(&Config::resolve_with(&root.join("mine"), None).unwrap());
    assert!(empty.contains("listing layer):\n  (none)\n"), "{empty}");
}
