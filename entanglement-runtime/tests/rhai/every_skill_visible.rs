//! ADR-0229: `user_only` is gone — a skill still declaring it is an ordinary
//! model-visible skill. Through the real executor, the model lists it,
//! loads it, and calls its `lib.rhai` from a script.

use entanglement_core::Permission;
use entanglement_runtime::skills::load_registry_in;

use super::launch_harness::{fixture, run_calls, table, Launch, Setup};
use super::no_user;

#[tokio::test]
async fn a_formerly_user_only_skill_is_listed_loaded_and_scriptable() {
    let (_tmp, root) = fixture();
    let dir = root.join(".entanglement/skills/deploy");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: deploy\ndescription: ship it\nuser_only: true\n---\nDeploy steps.\n",
    )
    .unwrap();
    std::fs::write(dir.join("lib.rhai"), r#"fn ship() { "shipped" }"#).unwrap();
    let mut setup = Setup::new(table(Permission::Allow, &[], &[], 0), Launch::RunAll);
    setup.skills = load_registry_in(&root, &no_user(&root)).unwrap();

    let seen = run_calls(
        &root,
        &[
            ("list_skills", "{}"),
            ("load_skill", r#"{"skill_name":"deploy"}"#),
            ("rhai", r#"{"script":"skill::deploy::ship()"}"#),
        ],
        setup,
    )
    .await;

    assert_eq!(seen.outputs.len(), 3, "{:?}", seen.outputs);
    assert!(
        seen.outputs[0].contains("deploy — ship it"),
        "{:?}",
        seen.outputs
    );
    assert!(
        seen.outputs[1].contains("skill_id: deploy"),
        "{:?}",
        seen.outputs
    );
    assert!(
        seen.outputs[1].contains("skill::deploy::ship()"),
        "{:?}",
        seen.outputs
    );
    assert!(seen.outputs[2].contains("shipped"), "{:?}", seen.outputs);
}
