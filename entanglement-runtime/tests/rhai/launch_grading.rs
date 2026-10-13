//! ADR-0228: the `rhai` launch is graded like any tool — the overlay (deny
//! and enable), mode rules on the script's canonical `path`, grants — and
//! `auto` (`default: allow`) runs a script with no card while its explicit
//! denies still win.

use std::path::Path;
use std::sync::Arc;

use entanglement_core::{ApprovalScope, Permission, ToolOverlayEntry};
use entanglement_runtime::mode::ModeTable;

use super::launch_harness::{fixture, run, run_calls, table, Launch, Setup, ADD};

/// Writes `made.txt` — a nested call an `Ask` mode cards.
const WRITE: &str = r#"write("made.txt", "x"); "wrote""#;

/// `scripts/w.rhai` writes `args.f`; returns a `rhai` input per target file
/// (so a nested approval's grant never covers the next run's write).
fn write_script(root: &Path) -> impl Fn(&str) -> String {
    std::fs::write(
        root.join("scripts/w.rhai"),
        r#"write(args.f, "x"); "wrote""#,
    )
    .unwrap();
    |f: &str| serde_json::json!({"path": "scripts/w.rhai", "args": {"f": f}}).to_string()
}

fn ask(launch: Launch) -> Setup {
    Setup::new(table(Permission::Ask, &[], &[], 0), launch)
}

fn inline(script: &str) -> String {
    serde_json::json!({ "script": script }).to_string()
}

#[tokio::test]
async fn an_overlay_deny_of_rhai_refuses_the_launch_trusted_path_included() {
    let (_tmp, root) = fixture();
    let mut setup = ask(Launch::RunAll);
    setup.trusted = vec![root.join("scripts")].into();
    setup.overlay = vec![ToolOverlayEntry::deny("rhai")];
    let seen = run(&root, &[ADD, &inline("40 + 2")], setup).await;
    assert!(seen.launch_cards.is_empty(), "{:?}", seen.launch_cards);
    assert_eq!(seen.outputs.len(), 2, "{:?}", seen.outputs);
    for out in &seen.outputs {
        assert!(out.contains("withdrawn") && !out.contains("42"), "{out}");
    }
}

/// An overlay enable is an Allow: no card, and the script runs Ask per
/// command — its nested write still cards.
#[tokio::test]
async fn an_overlay_enable_of_rhai_runs_without_a_card_asking_per_command() {
    let (_tmp, root) = fixture();
    let mut setup = ask(Launch::Reject);
    setup.overlay = vec![ToolOverlayEntry::allow("rhai")];
    let seen = run(&root, &[&inline(WRITE)], setup).await;
    assert!(seen.launch_cards.is_empty(), "{:?}", seen.launch_cards);
    assert_eq!(seen.nested_cards.len(), 1, "{:?}", seen.nested_cards);
    assert!(root.join("made.txt").exists(), "{:?}", seen.outputs);
}

/// A wider approval grants the exact path: the next launch grades Allow —
/// no card — and still asks per command.
#[tokio::test]
async fn a_path_grant_lifts_the_next_launch_to_ask_per_command() {
    let (_tmp, root) = fixture();
    let input = write_script(&root);
    let mut setup = ask(Launch::AskPerCommand);
    setup.scope = ApprovalScope::Session;
    let seen = run(&root, &[&input("a.txt"), &input("b.txt")], setup).await;
    assert_eq!(seen.launch_cards.len(), 1, "{:?}", seen.launch_cards);
    assert_eq!(seen.nested_cards.len(), 2, "{:?}", seen.nested_cards);
}

/// **Run all** is per run: the grant it came with lifts the launch card, not
/// the force-allow — the second run asks per command.
#[tokio::test]
async fn run_all_is_never_part_of_a_grant() {
    let (_tmp, root) = fixture();
    let input = write_script(&root);
    let mut setup = ask(Launch::RunAll);
    setup.scope = ApprovalScope::Session;
    let seen = run(&root, &[&input("a.txt"), &input("b.txt")], setup).await;
    assert_eq!(seen.launch_cards.len(), 1, "{:?}", seen.launch_cards);
    assert_eq!(
        seen.nested_cards.len(),
        1,
        "only the granted run asks: {:?}",
        seen.nested_cards
    );
}

/// An inline script has no path to key a grant on: approval is once only.
#[tokio::test]
async fn an_inline_approval_is_once_only() {
    let (_tmp, root) = fixture();
    let mut setup = ask(Launch::AskPerCommand);
    setup.scope = ApprovalScope::Session;
    let script = inline("40 + 2");
    let seen = run(&root, &[&script, &script], setup).await;
    assert_eq!(seen.launch_cards.len(), 2, "{:?}", seen.launch_cards);
    assert!(seen.outputs.iter().all(|o| o.contains("42")));
}

/// `rhai(scripts/*)` matches on the script's canonical root-relative path:
/// a script elsewhere — or a link in `scripts/` pointing elsewhere — cards.
#[tokio::test]
async fn a_scoped_rhai_rule_grades_on_the_canonical_path() {
    let (_tmp, root) = fixture();
    let setup = || {
        Setup::new(
            table(Permission::Ask, &["rhai(scripts/*)"], &[], 0),
            Launch::AskPerCommand,
        )
    };
    let other = r#"{"path":"other.rhai","args":{"a":1,"b":1}}"#;
    let seen = run(&root, &[ADD, other], setup()).await;
    assert_eq!(seen.launch_cards.len(), 1, "{:?}", seen.launch_cards);
    assert!(seen.launch_cards[0].contains("other.rhai"));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("other.rhai"), root.join("scripts/l.rhai")).unwrap();
        let link = r#"{"path":"scripts/l.rhai","args":{"a":1,"b":1}}"#;
        let seen = run(&root, &[link], setup()).await;
        assert_eq!(seen.launch_cards.len(), 1, "graded as other.rhai");
    }
}

/// The built-in table in `auto`.
fn auto() -> Setup {
    let mut setup = Setup::new(
        Arc::new(ModeTable::builtin().expect("built-in modes")),
        Launch::Reject,
    );
    setup.mode = Some("auto");
    setup
}

/// `auto` lets everything run: no launch card, no nested card.
#[tokio::test]
async fn auto_runs_a_script_with_no_card_and_its_nested_calls_pass() {
    let (_tmp, root) = fixture();
    let seen = run(&root, &[&inline(WRITE)], auto()).await;
    assert!(seen.launch_cards.is_empty(), "{:?}", seen.launch_cards);
    assert!(seen.nested_cards.is_empty(), "{:?}", seen.nested_cards);
    assert!(root.join("made.txt").exists(), "{:?}", seen.outputs);
}

/// A `default: allow` mode's deny rule still wins inside a script (a
/// catchable error), and an overlay deny of `rhai` still refuses the launch
/// in `auto`.
#[tokio::test]
async fn denies_still_win_where_everything_else_runs() {
    let (_tmp, root) = fixture();
    let try_write = r#"let r = "ran"; try { write("no.txt", "x"); } catch (e) { r = e; } r"#;
    let allow_but_write = Setup::new(
        table(Permission::Allow, &[], &["write(no.txt)"], 60),
        Launch::Reject,
    );
    let seen = run(&root, &[&inline(try_write)], allow_but_write).await;
    assert!(seen.launch_cards.is_empty(), "{:?}", seen.launch_cards);
    assert!(
        seen.outputs[0].contains("denied by mode"),
        "{:?}",
        seen.outputs
    );
    assert!(!root.join("no.txt").exists());
    let mut setup = auto();
    setup.overlay = vec![ToolOverlayEntry::deny("rhai")];
    let seen = run(&root, &[&inline("1")], setup).await;
    assert!(seen.outputs[0].contains("withdrawn"), "{:?}", seen.outputs);
}

/// The escape-root gate still asks in `auto` (Run all / trust pass it).
#[tokio::test]
async fn auto_still_asks_before_a_script_reads_outside_the_root() {
    let (_tmp, root) = fixture();
    let seen = run(&root, &[&inline(r#"read("../out.txt")"#)], auto()).await;
    assert!(seen.launch_cards.is_empty(), "{:?}", seen.launch_cards);
    assert_eq!(seen.nested_cards.len(), 1, "{:?}", seen.nested_cards);
}

/// The write floor applies in `auto` too: unattended, a write of a trusted
/// script collapses to a refusal first, and an identical repeat parks a card.
#[tokio::test]
async fn auto_floors_a_trusted_script_write() {
    let (_tmp, root) = fixture();
    let mut setup = auto();
    setup.trusted = vec![root.join("scripts")].into();
    let write = serde_json::json!({"path": "scripts/new.rhai", "content": "1"}).to_string();
    let seen = run_calls(&root, &[("write", &write), ("write", &write)], setup).await;
    assert!(seen.outputs[0].contains("unattended"), "{:?}", seen.outputs);
    assert_eq!(seen.nested_cards.len(), 1, "{:?}", seen.nested_cards);
    assert!(root.join("scripts/new.rhai").exists(), "{:?}", seen.outputs);
}
