//! ADR-0228's write floor: a script inside a `rhai.trusted` folder runs
//! force-allowed, so a `write`/`edit`/`apply_patch` of a `*.rhai` file there
//! always asks — even in a mode that allows the tool outright. Other files
//! in the folder, and `.rhai` files elsewhere, grade normally (a non-script
//! can never run by `path`, so it can never run trusted); a `Deny` stays a
//! `Deny`.

use std::path::Path;

use entanglement_core::Permission;
use serde_json::json;

use super::launch_harness::{fixture, run, run_calls, table, Launch, Setup};

/// A mode allowing the three file mutations outright, with `scripts/`
/// trusted.
fn allow_all(root: &Path, deny: &[&str]) -> Setup {
    let allow = ["write", "edit", "apply_patch"];
    let mut setup = Setup::new(table(Permission::Ask, &allow, deny, 0), Launch::Reject);
    setup.trusted = vec![root.join("scripts")].into();
    setup
}

/// The three mutating calls, each aimed at `path`: `write` creates `1`,
/// `edit` turns it into `2`, `apply_patch` into `3`.
fn mutations(path: &str) -> Vec<(&'static str, String)> {
    vec![
        ("write", json!({"path": path, "content": "1\n"}).to_string()),
        (
            "edit",
            json!({"path": path, "oldString": "1", "newString": "2"}).to_string(),
        ),
        (
            "apply_patch",
            json!({"path": path, "patch": "@@ -1 +1 @@\n-2\n+3\n"}).to_string(),
        ),
    ]
}

async fn cards_for(root: &Path, path: &str, setup: Setup) -> Vec<String> {
    let calls = mutations(path);
    let calls: Vec<(&str, &str)> = calls.iter().map(|(t, i)| (*t, i.as_str())).collect();
    run_calls(root, &calls, setup).await.nested_cards
}

#[tokio::test]
async fn mutating_a_trusted_script_always_asks() {
    let (_tmp, root) = fixture();
    let cards = cards_for(&root, "scripts/new.rhai", allow_all(&root, &[])).await;
    let tools: Vec<&str> = cards
        .iter()
        .map(|c| c.split(':').next().unwrap_or(""))
        .collect();
    assert_eq!(tools, ["write", "edit", "apply_patch"], "{cards:?}");
    assert_eq!(
        std::fs::read_to_string(root.join("scripts/new.rhai")).unwrap(),
        "3\n",
        "each approved call ran"
    );
}

/// A non-script file in a trusted folder is written with no card — and that
/// is safe only because trust needs a `.rhai` script: running it by `path`
/// is refused before any card (B, #820 review).
#[tokio::test]
async fn other_files_in_a_trusted_folder_write_freely_but_never_run() {
    let (_tmp, root) = fixture();
    let write = json!({"path": "scripts/x.txt", "content": "40 + 2"}).to_string();
    let run = json!({"path": "scripts/x.txt"}).to_string();
    let seen = run_calls(
        &root,
        &[("write", write.as_str()), ("rhai", run.as_str())],
        allow_all(&root, &[]),
    )
    .await;
    assert!(seen.nested_cards.is_empty(), "{:?}", seen.nested_cards);
    assert!(seen.launch_cards.is_empty(), "{:?}", seen.launch_cards);
    assert_eq!(
        std::fs::read_to_string(root.join("scripts/x.txt")).unwrap(),
        "40 + 2"
    );
    assert_eq!(seen.outputs.len(), 2, "{:?}", seen.outputs);
    assert!(
        seen.outputs[1].contains("`.rhai` script file") && !seen.outputs[1].contains("42"),
        "{:?}",
        seen.outputs
    );
}

/// A `.rhai` name in a trusted folder pointing at a non-script is judged by
/// its target: refused, never run trusted.
#[cfg(unix)]
#[tokio::test]
async fn a_rhai_symlink_to_a_text_file_never_runs_trusted() {
    let (_tmp, root) = fixture();
    std::fs::write(root.join("scripts/x.txt"), "40 + 2").unwrap();
    std::os::unix::fs::symlink(root.join("scripts/x.txt"), root.join("scripts/x.rhai")).unwrap();
    // Even **Run all** on a card cannot make it run.
    let mut setup = allow_all(&root, &[]);
    setup.table = table(Permission::Ask, &[], &[], 0);
    setup.launch = Launch::RunAll;
    let seen = run(&root, &[r#"{"path":"scripts/x.rhai"}"#], setup).await;
    assert_eq!(seen.launch_cards.len(), 1, "carded, not trusted");
    assert_eq!(seen.outputs.len(), 1, "{:?}", seen.outputs);
    assert!(
        seen.outputs[0].contains("does not resolve to a `.rhai`")
            && !seen.outputs[0].contains("42"),
        "{:?}",
        seen.outputs
    );
}

/// The extension check is case-insensitive: `.RHAI` in a trusted folder is
/// a trusted script — it runs with no card.
#[tokio::test]
async fn an_upper_case_rhai_extension_is_still_trusted() {
    let (_tmp, root) = fixture();
    std::fs::write(root.join("scripts/UP.RHAI"), "args.a + args.b").unwrap();
    let seen = run(
        &root,
        &[r#"{"path":"scripts/UP.RHAI","args":{"a":40,"b":2}}"#],
        allow_all(&root, &[]),
    )
    .await;
    assert!(seen.launch_cards.is_empty(), "{:?}", seen.launch_cards);
    assert!(seen.outputs[0].contains("42"), "{:?}", seen.outputs);
}

#[tokio::test]
async fn a_rhai_file_outside_the_trusted_folders_grades_normally() {
    let (_tmp, root) = fixture();
    let cards = cards_for(&root, "lib/new.rhai", allow_all(&root, &[])).await;
    assert!(cards.is_empty(), "{cards:?}");
}

#[tokio::test]
async fn a_deny_rule_still_denies_a_trusted_script_write() {
    let (_tmp, root) = fixture();
    let setup = allow_all(&root, &["write", "edit", "apply_patch"]);
    let calls = mutations("scripts/add.rhai");
    let calls: Vec<(&str, &str)> = calls.iter().map(|(t, i)| (*t, i.as_str())).collect();
    let seen = run_calls(&root, &calls, setup).await;
    assert!(seen.nested_cards.is_empty(), "{:?}", seen.nested_cards);
    assert_eq!(seen.outputs.len(), 3, "{:?}", seen.outputs);
    assert!(
        seen.outputs.iter().all(|o| o.contains("denied by mode")),
        "{:?}",
        seen.outputs
    );
    assert_eq!(
        std::fs::read_to_string(root.join("scripts/add.rhai")).unwrap(),
        "args.a + args.b"
    );
}

/// Writes a trusted script and a plain file, each result caught.
const WRITES_TRUSTED: &str = r#"let a = "ok"; try { write("scripts/gen.rhai", "1"); } catch (e) { a = e; }
write("plain.txt", "x"); a"#;

/// **Run all** passes every nested `Ask` but a floored write: that gets its
/// own card (the plain write does not).
#[tokio::test]
async fn run_all_still_cards_a_floored_write() {
    let (_tmp, root) = fixture();
    let mut setup = Setup::new(table(Permission::Ask, &[], &[], 0), Launch::RunAll);
    setup.trusted = vec![root.join("scripts")].into();
    let input = json!({ "script": WRITES_TRUSTED }).to_string();
    let seen = run(&root, &[&input], setup).await;
    assert_eq!(seen.launch_cards.len(), 1);
    assert_eq!(seen.nested_cards.len(), 1, "{:?}", seen.nested_cards);
    assert!(
        seen.nested_cards[0].starts_with("write (rhai)"),
        "{:?}",
        seen.nested_cards
    );
    assert!(root.join("scripts/gen.rhai").exists() && root.join("plain.txt").exists());
}

/// Unattended, the floored write's `Ask` collapses (a catchable refusal) —
/// it never hangs.
#[tokio::test]
async fn an_unattended_floored_write_collapses() {
    let (_tmp, root) = fixture();
    let mut setup = Setup::new(table(Permission::Ask, &[], &[], 60), Launch::RunAll);
    setup.trusted = vec![root.join("scripts")].into();
    let input = json!({ "script": WRITES_TRUSTED }).to_string();
    let seen = run(&root, &[&input], setup).await;
    assert!(seen.nested_cards.is_empty(), "{:?}", seen.nested_cards);
    assert!(seen.outputs[0].contains("unattended"), "{:?}", seen.outputs);
    assert!(!root.join("scripts/gen.rhai").exists());
    assert!(root.join("plain.txt").exists());
}

/// A trusted script's own writes are its owner's: not floored.
#[tokio::test]
async fn a_trusted_script_writing_a_trusted_script_is_not_floored() {
    let (_tmp, root) = fixture();
    std::fs::write(root.join("scripts/writer.rhai"), WRITES_TRUSTED).unwrap();
    let mut setup = Setup::new(table(Permission::Ask, &[], &[], 0), Launch::Reject);
    setup.trusted = vec![root.join("scripts")].into();
    let seen = run(&root, &[r#"{"path":"scripts/writer.rhai"}"#], setup).await;
    assert!(seen.launch_cards.is_empty() && seen.nested_cards.is_empty());
    assert!(root.join("scripts/gen.rhai").exists(), "{:?}", seen.outputs);
}

/// A skill's `SKILL.md`/`lib.rhai` in any skill layer dir is floored like a
/// trusted script; its other files grade normally.
#[tokio::test]
async fn skill_files_are_floored_like_trusted_scripts() {
    let (_tmp, root) = fixture();
    let mut setup = allow_all(&root, &[]);
    setup.skill_dirs = vec![
        root.join(".entanglement/skills"),
        root.join(".claude/skills"),
    ];
    let write = |path: &str| json!({"path": path, "content": "x"}).to_string();
    let (skill, lib, claude, notes) = (
        write(".entanglement/skills/git/SKILL.md"),
        write(".entanglement/skills/git/lib.rhai"),
        write(".claude/skills/x/SKILL.md"),
        write(".entanglement/skills/git/notes.md"),
    );
    let calls = [
        ("write", skill.as_str()),
        ("write", lib.as_str()),
        ("write", claude.as_str()),
        ("write", notes.as_str()),
    ];
    let seen = run_calls(&root, &calls, setup).await;
    assert_eq!(seen.nested_cards.len(), 3, "{:?}", seen.nested_cards);
    assert!(seen.nested_cards.iter().all(|c| !c.contains("notes.md")));
    assert!(root.join(".entanglement/skills/git/notes.md").exists());
}
