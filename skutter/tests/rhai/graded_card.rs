//! ADR-0249 in `skutter`: the script-only structured and raw variants grade
//! as their base tool through `Tool::call_grading`, so their nested cards
//! name that tool — the name a `prompt` rule and an `Always` grant use (#934).

use entanglement_core::{OutEvent, Permission, PermissionProfile, SessionId};

use super::{
    collect_auto_approving, mode_table_for, one_profile, prompt, rhai_output, spawn_with_rhai_mode,
    TempDir,
};

#[tokio::test]
async fn a_structured_or_raw_variant_is_carded_as_its_base_tool() {
    let dir = TempDir::new("graded-card");
    std::fs::write(dir.path.join("a.rs"), "fn a() {}\n").unwrap();
    let permission = PermissionProfile::new(Permission::Allow)
        .with("glob", Permission::Ask)
        .with("grep", Permission::Ask)
        .with("read", Permission::Ask);
    let holly = spawn_with_rhai_mode(
        r#"
        let g = glob_json("*.rs").files.len();
        let m = grep_json("fn a").matches.len();
        let r = read_raw("a.rs");
        `${g}|${m}|${r.len()}`
        "#,
        &dir.path,
        one_profile("askread", permission.clone()),
        mode_table_for(&permission),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    prompt(&holly, &sid, "askread").await;
    let events = collect_auto_approving(&holly, sub, &sid).await;

    let cards: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolRequest { tool, .. } => Some(tool.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        cards,
        ["glob (rhai)", "grep (rhai)", "read (rhai)"],
        "{events:#?}"
    );
    let out = rhai_output(&events).expect("expected rhai output");
    assert!(out.contains("1|1|"), "every call ran once approved: {out}");
}
