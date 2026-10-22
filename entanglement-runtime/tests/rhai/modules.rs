//! ADR-0248: an imported module's code is the calling script's code — every
//! binding it makes, however deep the import, is graded in the caller's
//! posture; `load_skill rhai` lists the modules; a write into a module dir
//! always asks.

use entanglement_core::{Permission, PermissionProfile};
use serde_json::json;

use super::launch_harness::{fixture, run_calls, table, Launch, Setup};

/// `deep` writes, `mid` imports `deep`: the script imports only `mid`.
fn write_modules(root: &std::path::Path) {
    let dir = root.join(".entanglement/rhai/modules");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("deep.rhai"),
        r#"fn put() { write("out.txt", "x") }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("mid.rhai"),
        "import \"deep\" as d;\n/// Writes out.txt.\nfn put() { d::put() }",
    )
    .unwrap();
}

fn setup_for(root: &std::path::Path) -> Setup {
    let mut setup = Setup::new(table(Permission::Allow, &[], &[], 0), Launch::RunAll);
    setup.skills =
        entanglement_runtime::skills::load_registry_in(root, &super::no_user(root)).unwrap();
    setup
}

const PUT: &str = r#"{"script":"import \"mid\" as m; m::put()"}"#;

#[tokio::test]
async fn a_denied_binding_stays_denied_inside_an_imported_module() {
    let (_tmp, root) = fixture();
    write_modules(&root);
    let mut setup = setup_for(&root);
    // Arg-scoped: a bare `write` rule would clamp the `rhai` launch itself.
    setup.ceiling =
        PermissionProfile::new(Permission::Allow).with("write(out.txt)", Permission::Deny);
    let seen = run_calls(&root, &[("rhai", PUT)], setup).await;
    let out = &seen.outputs[0];
    assert!(out.contains("denied by mode"), "{out}");
    assert!(
        !out.contains("tool `rhai` denied"),
        "the launch itself ran: {out}"
    );
    assert!(!root.join("out.txt").exists());
}

#[tokio::test]
async fn an_allowed_binding_runs_from_an_imported_module() {
    let (_tmp, root) = fixture();
    write_modules(&root);
    let seen = run_calls(&root, &[("rhai", PUT)], setup_for(&root)).await;
    assert!(root.join("out.txt").exists(), "{:?}", seen.outputs);
}

#[tokio::test]
async fn load_skill_rhai_lists_the_modules() {
    let (_tmp, root) = fixture();
    write_modules(&root);
    let call = json!({"skill_name": "rhai"}).to_string();
    let seen = run_calls(&root, &[("load_skill", &call)], setup_for(&root)).await;
    let out = &seen.outputs[0];
    assert!(
        out.contains("\n  json_schema (built-in)\n    validate(value, schema)"),
        "{out}"
    );
    assert!(
        out.contains("\n  mid (project; imports deep)\n    put()\n        Writes out.txt."),
        "{out}"
    );
}

/// Every mutating tool aimed at a `*.rhai` in a module dir asks, the dir
/// not existing yet included; other files there grade normally.
#[tokio::test]
async fn a_write_into_a_module_dir_always_asks() {
    let (_tmp, root) = fixture();
    let allow = ["write", "edit", "apply_patch"];
    let mut setup = Setup::new(table(Permission::Ask, &allow, &[], 0), Launch::Reject);
    setup.module_dirs = vec![root.join(".entanglement/rhai/modules")];
    let write = |path: &str| json!({"path": path, "content": "fn f() { 1 }"}).to_string();
    let (module, nested, notes) = (
        write(".entanglement/rhai/modules/m.rhai"),
        write(".entanglement/rhai/modules/a/b.rhai"),
        write(".entanglement/rhai/modules/notes.md"),
    );
    assert!(!root.join(".entanglement/rhai").exists());
    let calls = [
        ("write", module.as_str()),
        ("write", nested.as_str()),
        ("write", notes.as_str()),
    ];
    let seen = run_calls(&root, &calls, setup).await;
    assert_eq!(seen.nested_cards.len(), 2, "{:?}", seen.nested_cards);
    assert!(seen.nested_cards.iter().all(|c| !c.contains("notes.md")));
    assert!(root.join(".entanglement/rhai/modules/notes.md").exists());
}
