//! ADR-0228's launch model: a `rhai.trusted` script runs with no card and
//! every nested call force-allowed (explicit denies still win); any other
//! script raises one card — Run all, Ask per command, Reject; unattended,
//! no card and nested `Ask`s collapse. Plus the workdir rewrite of `path`, a
//! background `path` run, the no-escape-policy refusal, and doc-style
//! comments that must not break a script.

use std::path::PathBuf;
use std::sync::Arc;

use entanglement_core::Permission;
use entanglement_runtime::config::Config;
use entanglement_runtime::http_gate::HttpGate;

use super::launch_harness::{fixture, run, table, Launch, Setup, ADD};

/// A script that writes inside the root (graded `Ask` below), reads a file
/// outside it (escape-root) and makes a credentialed `http()` call (the
/// per-host approval) — each result joined, a refusal caught as its text.
const EVERYTHING: &str = r#"let w = write("made.txt", "x");
let r = read("../out.txt");
let h = ""; try { h = http("GET", "https://127.0.0.1:9/x"); } catch (e) { h = e; }
`${w}|${r}|${h}`"#;

fn credentialed() -> Arc<HttpGate> {
    Arc::new(HttpGate::new(
        serde_yaml::from_str("127.0.0.1: {headers: {X-Key: s3cret}}").unwrap(),
    ))
}

fn ask(launch: Launch) -> Setup {
    let mut setup = Setup::new(table(Permission::Ask, &[], &[], 0), launch);
    setup.gate = credentialed();
    setup
}

/// `scripts/everything.rhai` under a project whose `scripts/` is trusted.
fn trusted_fixture() -> (tempfile::TempDir, PathBuf, Setup) {
    let (tmp, root) = fixture();
    std::fs::write(root.join("scripts/everything.rhai"), EVERYTHING).unwrap();
    let mut setup = ask(Launch::Reject);
    setup.trusted = vec![root.join("scripts")].into();
    (tmp, root, setup)
}

fn assert_everything_ran(root: &std::path::Path, out: &str) {
    assert!(
        root.join("made.txt").exists(),
        "the Ask-graded write ran: {out}"
    );
    assert!(out.contains("outside text"), "escape-root passed: {out}");
    assert!(
        !out.contains("authenticated"),
        "no per-host approval: {out}"
    );
    assert!(
        !out.contains("denied") && !out.contains("rejected"),
        "{out}"
    );
}

#[tokio::test]
async fn a_trusted_script_runs_without_any_card() {
    let (_tmp, root, setup) = trusted_fixture();
    let seen = run(&root, &[r#"{"path":"scripts/everything.rhai"}"#], setup).await;
    assert!(seen.launch_cards.is_empty() && seen.nested_cards.is_empty());
    assert_everything_ran(&root, &seen.outputs[0]);
}

/// Explicit denies still win inside a trusted script — a catchable error.
#[tokio::test]
async fn an_explicit_deny_inside_a_trusted_script_is_catchable() {
    let (_tmp, root, mut setup) = trusted_fixture();
    setup.table = table(Permission::Ask, &[], &["write"], 0);
    std::fs::write(
        root.join("scripts/deny.rhai"),
        r#"let r = "ran"; try { write("no.txt", "x"); } catch (e) { r = e; } r"#,
    )
    .unwrap();
    let seen = run(&root, &[r#"{"path":"scripts/deny.rhai"}"#], setup).await;
    assert!(
        seen.outputs[0].contains("denied by mode"),
        "{:?}",
        seen.outputs
    );
    assert!(!root.join("no.txt").exists());
}

/// Trust comes from `config.yml` `rhai.trusted` in either layer: the
/// project's `scripts/` and the user's own folder (`~`-free, relative to the
/// user config file).
#[tokio::test]
async fn project_and_user_layer_trusted_folders_both_work() {
    let (tmp, root) = fixture();
    let user_dir = tmp.path().canonicalize().unwrap().join("user");
    std::fs::create_dir_all(user_dir.join("mine")).unwrap();
    std::fs::write(user_dir.join("mine/u.rhai"), "args.a + args.b").unwrap();
    std::fs::write(user_dir.join("config.yml"), "rhai:\n  trusted: [mine]\n").unwrap();
    std::fs::create_dir_all(root.join(".entanglement")).unwrap();
    std::fs::write(
        root.join(".entanglement/config.yml"),
        "rhai:\n  trusted: [scripts]\n",
    )
    .unwrap();
    let config = Config::load_with(&root, Some(&user_dir.join("config.yml"))).unwrap();
    assert_eq!(
        config.rhai_trusted.all().count(),
        2,
        "{:?}",
        config.rhai_trusted
    );
    let mut setup = ask(Launch::Reject);
    setup.trusted = config.rhai_trusted;
    let user_script = serde_json::json!({
        "path": user_dir.join("mine/u.rhai"), "args": {"a": 40, "b": 2}
    })
    .to_string();
    let seen = run(&root, &[ADD, &user_script], setup).await;
    assert!(seen.launch_cards.is_empty(), "{:?}", seen.launch_cards);
    assert!(
        seen.outputs.iter().all(|o| o.contains("42")),
        "{:?}",
        seen.outputs
    );
}

/// The project config adding `prompt` rules for the session's mode drops
/// the user layer's trust for the whole script there — it is graded (and
/// carded) like any other; the project's own trusted folder still runs.
#[tokio::test]
async fn project_prompt_rules_suppress_user_layer_trust_in_that_mode() {
    let (tmp, root) = fixture();
    let user_dir = tmp.path().canonicalize().unwrap().join("user");
    std::fs::create_dir_all(user_dir.join("mine")).unwrap();
    std::fs::write(user_dir.join("mine/u.rhai"), "args.a + args.b").unwrap();
    std::fs::write(user_dir.join("config.yml"), "rhai:\n  trusted: [mine]\n").unwrap();
    std::fs::create_dir_all(root.join(".entanglement")).unwrap();
    std::fs::write(
        root.join(".entanglement/config.yml"),
        "rhai:\n  trusted: [scripts]\nmodes:\n  build:\n    prompt: [\"bash(make *)\"]\n",
    )
    .unwrap();
    let config = Config::load_with(&root, Some(&user_dir.join("config.yml"))).unwrap();
    let mut setup = ask(Launch::Reject);
    setup.trusted = config.rhai_trusted;
    let user_script = serde_json::json!({
        "path": user_dir.join("mine/u.rhai"), "args": {"a": 40, "b": 2}
    })
    .to_string();
    let seen = run(&root, &[ADD, &user_script], setup).await;
    assert_eq!(seen.launch_cards.len(), 1, "{:?}", seen.launch_cards);
    assert!(
        seen.launch_cards[0].contains("u.rhai"),
        "{:?}",
        seen.launch_cards
    );
    assert!(
        seen.outputs[0].contains("42"),
        "project trust: {:?}",
        seen.outputs
    );
    assert!(seen.outputs[1].contains("rejected"), "{:?}", seen.outputs);
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_out_of_a_trusted_folder_is_not_trusted() {
    let (_tmp, root, setup) = trusted_fixture();
    let evil = root.parent().unwrap().join("evil.rhai");
    std::os::unix::fs::symlink(&evil, root.join("scripts/link.rhai")).unwrap();
    let seen = run(
        &root,
        &[r#"{"path":"scripts/link.rhai","args":{"a":1,"b":1}}"#],
        setup,
    )
    .await;
    assert_eq!(seen.launch_cards.len(), 1, "carded, not trusted");
    assert!(seen.outputs[0].contains("rejected"), "{:?}", seen.outputs);
}

#[tokio::test]
async fn run_all_force_allows_the_nested_calls() {
    let (_tmp, root) = fixture();
    let input = serde_json::json!({ "script": EVERYTHING }).to_string();
    let seen = run(&root, &[&input], ask(Launch::RunAll)).await;
    assert_eq!(seen.launch_cards.len(), 1);
    assert!(
        seen.launch_cards[0].starts_with("// inline script\nlet w = write"),
        "the source"
    );
    assert!(seen.nested_cards.is_empty(), "{:?}", seen.nested_cards);
    assert_everything_ran(&root, &seen.outputs[0]);
}

#[tokio::test]
async fn ask_per_command_prompts_each_nested_ask() {
    let (_tmp, root) = fixture();
    let seen = run(
        &root,
        &[r#"{"script":"write(\"a.txt\", \"x\"); 42"}"#],
        ask(Launch::AskPerCommand),
    )
    .await;
    assert_eq!(seen.launch_cards.len(), 1);
    assert_eq!(seen.nested_cards.len(), 1, "{:?}", seen.nested_cards);
    assert!(seen.nested_cards[0].starts_with("write (rhai)"));
    assert!(seen.outputs[0].contains("42"));
}

#[tokio::test]
async fn reject_refuses_the_launch() {
    let (_tmp, root) = fixture();
    let seen = run(
        &root,
        &[r#"{"script":"write(\"a.txt\", \"x\")"}"#],
        ask(Launch::Reject),
    )
    .await;
    assert!(seen.outputs[0].contains("rejected"), "{:?}", seen.outputs);
    assert!(!root.join("a.txt").exists());
}

/// Unattended: no launch card; a nested `Ask` collapses per the mode's
/// rules — refused here — and an out-of-root non-trusted path is refused.
/// No answerer (point 6): an `Ask` launch card waits only the mode's
/// `question_timeout`, then the script runs Ask per command — its nested
/// `Ask`s collapsing per the mode (a first one refused, catchably). An
/// out-of-root script is refused on the timeout instead: silence never
/// approves an escape. Nothing hangs.
#[tokio::test]
async fn an_unanswered_launch_card_times_out_into_ask_per_command() {
    let (_tmp, root) = fixture();
    let setup = Setup::new(table(Permission::Ask, &[], &[], 1), Launch::Silent);
    let seen = run(
        &root,
        &[
            r#"{"script":"let r = \"ran\"; try { write(\"a.txt\", \"x\"); } catch (e) { r = e; } r"}"#,
            r#"{"path":"../evil.rhai","args":{"a":1,"b":1}}"#,
            ADD,
        ],
        setup,
    )
    .await;
    assert_eq!(seen.launch_cards.len(), 3, "{:?}", seen.launch_cards);
    assert!(seen.nested_cards.is_empty(), "{:?}", seen.nested_cards);
    assert!(seen.outputs[0].contains("unattended"), "{:?}", seen.outputs);
    assert!(!root.join("a.txt").exists());
    assert!(
        seen.outputs[1].contains("no response") && seen.outputs[1].contains("outside the project"),
        "{:?}",
        seen.outputs
    );
    assert!(seen.outputs[2].contains("42"), "{:?}", seen.outputs);
}

/// `skutter run` (point 6): a launch card starts the script Ask per command
/// at once; a nested `Ask` card is refused at once — a catchable error.
#[tokio::test]
async fn the_headless_head_runs_ask_per_command_and_refuses_nested_asks() {
    let (_tmp, root) = fixture();
    let setup = Setup::new(table(Permission::Ask, &[], &[], 0), Launch::Headless);
    let seen = run(
        &root,
        &[r#"{"script":"let r = \"ran\"; try { write(\"a.txt\", \"x\"); } catch (e) { r = e; } r"}"#],
        setup,
    )
    .await;
    assert_eq!(seen.launch_cards.len(), 1, "{:?}", seen.launch_cards);
    assert_eq!(seen.nested_cards.len(), 1, "{:?}", seen.nested_cards);
    assert!(seen.outputs[0].contains("rejected"), "{:?}", seen.outputs);
    assert!(!root.join("a.txt").exists());
}

#[tokio::test]
async fn a_pre_tool_use_hook_vetoes_the_launch() {
    let (_tmp, root, mut setup) = trusted_fixture();
    setup.hooks = serde_yaml::from_str(
        "pre_tool_use:\n  - command: \"echo no scripts today >&2; exit 1\"\n    tools: [rhai]\n",
    )
    .unwrap();
    let seen = run(&root, &[ADD], setup).await;
    assert!(
        seen.outputs[0].contains("no scripts today"),
        "{:?}",
        seen.outputs
    );
}

/// Without Rhai's `metadata` feature a `///` or `/** */` that is not
/// followed by `fn` is an ordinary comment — in an inline script and a file.
#[tokio::test]
async fn doc_style_comments_never_break_a_script() {
    let (_tmp, root) = fixture();
    std::fs::write(
        root.join("scripts/doc.rhai"),
        "/** A header. */\n/// stray\nlet x = 40; x + 2 /// trailing",
    )
    .unwrap();
    let seen = run(
        &root,
        &[
            r#"{"script":"/** header */\n40 + 2"}"#,
            r#"{"script":"/// hi\nlet x = 42; x"}"#,
            r#"{"script":"fn f() { /// inner\n 42 }\nf()"}"#,
            r#"{"path":"scripts/doc.rhai"}"#,
        ],
        ask(Launch::RunAll),
    )
    .await;
    assert_eq!(seen.outputs.len(), 4);
    for out in &seen.outputs {
        assert!(out.contains("42"), "{out}");
    }
}

#[tokio::test]
async fn without_an_escape_policy_an_out_of_root_path_is_refused() {
    let (_tmp, root) = fixture();
    let mut setup = ask(Launch::RunAll);
    setup.escape_root = false;
    let seen = run(
        &root,
        &[r#"{"path":"../evil.rhai","args":{"a":1,"b":1}}"#],
        setup,
    )
    .await;
    assert!(seen.outputs[0].contains("escapes"), "{:?}", seen.outputs);
}

/// A relative `path` joins the session workdir: `add.rhai` from inside
/// `scripts/` is the trusted `scripts/add.rhai`.
#[tokio::test]
async fn a_relative_path_resolves_against_the_session_workdir() {
    let (_tmp, root, mut setup) = trusted_fixture();
    setup.workdir = Some("scripts");
    let seen = run(
        &root,
        &[r#"{"path":"add.rhai","args":{"a":40,"b":2}}"#],
        setup,
    )
    .await;
    assert!(seen.launch_cards.is_empty(), "{:?}", seen.launch_cards);
    assert!(seen.outputs[0].contains("42"), "{:?}", seen.outputs);
}

/// A background `path` run detaches with a handle — only after the launch
/// and the read: a missing file is reported, never detached.
#[tokio::test]
async fn a_background_path_run_detaches_after_the_launch_and_read() {
    let (_tmp, root, mut setup) = trusted_fixture();
    // A missing file has no canonical path to trust: it is carded.
    setup.launch = Launch::AskPerCommand;
    let seen = run(
        &root,
        &[
            r#"{"path":"scripts/add.rhai","args":{"a":1,"b":1},"background":true}"#,
            r#"{"path":"scripts/missing.rhai","background":true}"#,
        ],
        setup,
    )
    .await;
    assert!(
        seen.outputs[0].contains("[background script x-"),
        "{:?}",
        seen.outputs
    );
    assert!(seen.outputs[1].contains("cannot"), "{:?}", seen.outputs);
}
