//! ADR-0228: `rhai` runs a predefined script by `path` and binds `args` as a
//! constant map for both run kinds; `script` XOR `path` is validated before
//! the launch; the launch card shows the source read once. Plus the skill
//! side: a native `tools:` key is refused without aborting the load, and
//! `load_skill` lists `lib.rhai` signatures with their doc comments.

use std::path::Path;
use std::sync::Arc;

use entanglement_core::Permission;
use entanglement_runtime::skills::{load_registry_in, LoadSkillTool};
use entanglement_runtime::Tool;

use super::launch_harness::{fixture, run, table, Launch, Setup, ADD};
use super::no_user;

fn allow(launch: Launch) -> Setup {
    Setup::new(table(Permission::Allow, &[], &[], 0), launch)
}

/// The card shows the very source the runtime read — the model never reads
/// the file — and the approved run evaluates it with its `args`.
#[tokio::test]
async fn a_path_run_cards_the_source_and_runs_it_with_args() {
    let (_tmp, root) = fixture();
    let ask = Setup::new(table(Permission::Ask, &[], &[], 0), Launch::AskPerCommand);
    let seen = run(&root, &[ADD], ask).await;
    assert_eq!(
        seen.launch_cards,
        vec!["// path: \"scripts/add.rhai\"\n// args: {\"a\":40,\"b\":2}\nargs.a + args.b"]
    );
    assert!(seen.outputs[0].contains("42"), "{:?}", seen.outputs);
}

/// A non-trusted path outside the root keeps the escape-root warning — on
/// the same launch card, never a second prompt.
#[tokio::test]
async fn an_out_of_root_path_warns_on_the_one_launch_card() {
    let (_tmp, root) = fixture();
    let seen = run(
        &root,
        &[r#"{"path":"../evil.rhai","args":{"a":40,"b":2}}"#],
        allow(Launch::AskPerCommand),
    )
    .await;
    let evil = root.parent().unwrap().join("evil.rhai");
    assert_eq!(seen.launch_cards.len(), 1);
    assert!(
        seen.launch_cards[0].ends_with(&format!("OUTSIDE the project root: {}", evil.display())),
        "{:?}",
        seen.launch_cards
    );
    assert!(seen.nested_cards.is_empty());
    assert!(seen.outputs[0].contains("42"), "{:?}", seen.outputs);
}

#[tokio::test]
async fn args_are_a_constant_map_in_both_run_kinds() {
    let (_tmp, root) = fixture();
    std::fs::write(root.join("scripts/mutate.rhai"), "args.a = 0; args.a").unwrap();
    let seen = run(
        &root,
        &[
            r#"{"script":"args.n * 2","args":{"n":21}}"#,
            ADD,
            r#"{"script":"args = #{}; 1","args":{"n":1}}"#,
            r#"{"path":"scripts/mutate.rhai","args":{"a":5}}"#,
            r#"{"script":"args.len()"}"#,
        ],
        allow(Launch::AskPerCommand),
    )
    .await;
    assert!(seen.outputs[0].contains("42"), "{:?}", seen.outputs);
    assert!(seen.outputs[1].contains("42"), "{:?}", seen.outputs);
    for out in &seen.outputs[2..4] {
        assert!(out.to_lowercase().contains("constant"), "{out}");
    }
    assert!(seen.outputs[4].contains('0'), "args is #{{}} when absent");
}

#[tokio::test]
async fn xor_and_type_violations_are_declined_before_the_launch() {
    let (_tmp, root) = fixture();
    let seen = run(
        &root,
        &[
            r#"{"script":"1","path":"scripts/add.rhai"}"#,
            r#"{"args":{"a":1}}"#,
            r#"{"path":"scripts/add.rhai","args":[1]}"#,
            r#"{"path":"scripts/add.rhai","arguments":{"a":1}}"#,
        ],
        allow(Launch::RunAll),
    )
    .await;
    assert!(seen.launch_cards.is_empty(), "no card for a bad call");
    assert!(seen.outputs[0].contains("not both"), "{}", seen.outputs[0]);
    assert!(
        seen.outputs[1].contains("exactly one of"),
        "{}",
        seen.outputs[1]
    );
    assert!(seen.outputs[2].contains("`args` must be an object"));
    assert!(seen.outputs[3].contains("unknown key `arguments`"));
}

fn write_skill(dir: &Path, name: &str, front: &str) -> std::path::PathBuf {
    let skill = dir.join(name);
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: d\n{front}---\nBody of {name}.\n"),
    )
    .unwrap();
    skill
}

/// A native `tools:` key rejects that skill with the replacement named in
/// the log — never the whole load.
#[test]
fn a_native_tools_key_rejects_only_that_skill() {
    let tmp = tempfile::tempdir().unwrap();
    let skills = tmp.path().join(".entanglement/skills");
    write_skill(
        &skills,
        "old",
        "tools:\n  - kind: alias\n    name: x\n    target: read\n",
    );
    write_skill(&skills, "fine", "");
    let reg = load_registry_in(tmp.path(), &no_user(tmp.path())).unwrap();
    assert!(reg.get("old").is_none());
    assert!(reg.get("fine").is_some());
}

/// `load_skill` = body, the lib's signatures with their doc comments, then
/// the refs (the lib itself among them, runnable by `path`).
#[tokio::test]
async fn load_skill_lists_lib_signatures_with_doc_comments() {
    let tmp = tempfile::tempdir().unwrap();
    let skills = tmp.path().join(".entanglement/skills");
    let dir = write_skill(&skills, "git-tools", "");
    std::fs::write(
        dir.join("lib.rhai"),
        "/// Files changed since `rev`.\n/// One path per entry.\nfn changed(rev) { [] }\n\
         /** Count commits. */\nfn count() { 0 }\nprivate fn hidden() { 1 }",
    )
    .unwrap();
    let reg = load_registry_in(tmp.path(), &no_user(tmp.path())).unwrap();
    let tool = LoadSkillTool::new(Arc::new(std::sync::RwLock::new(Arc::new(reg))));
    let out = tool.run(r#"{"skill_name":"git-tools"}"#).await.unwrap();
    let expected = "Body of git-tools.\n\nscript functions (call from a `rhai` script):\n  \
         skill::git_tools::changed(rev)\n      Files changed since `rev`.\n      One path per \
         entry.\n  skill::git_tools::count()\n      Count commits.\n\navailable_refs";
    assert!(out.contains(expected), "{out}");
    assert!(!out.contains("hidden"), "{out}");
    assert!(out.contains(&dir.join("lib.rhai").display().to_string()));
}
