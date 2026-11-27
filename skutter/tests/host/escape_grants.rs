//! The escape-root ask is answered by its own grant (#968 review): an
//! in-root grant on `bash`'s command does not run that command outside the
//! root, and the approval of an out-of-root run is not a grant on the bare
//! command. A script's out-of-root `read_raw` raises no escape ask at all.

use std::sync::Arc;

use entanglement_core::{ApprovalScope, CardReason, OutEvent, Permission};
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};
use entanglement_runtime::policy::GrantStore;
use serde_json::json;
use skutter::host::escape::escape_grant;

use crate::mode_support::allow_all_table;
use crate::session_workdir::{call, harness, output, requests};

/// One mode, `build`, that asks for everything.
fn ask_all() -> Arc<ModeTable> {
    let mode = Mode {
        name: "build".into(),
        default: Permission::Ask,
        rules: Rules::default(),
        limits: Limits::default(),
        sandbox: None,
        sandbox_network: false,
    };
    Arc::new(ModeTable::new(vec![mode]).unwrap())
}

#[tokio::test]
async fn an_in_root_bash_grant_does_not_cover_an_out_of_root_workdir() {
    let h = harness(
        vec![
            call("in", "bash", serde_json::json!({"command": "pwd"})),
            call(
                "out",
                "bash",
                serde_json::json!({"command": "pwd", "workdir": "../outside"}),
            ),
        ],
        ask_all(),
    );
    let outside = h.outside.display().to_string();
    h.grants
        .record(&h.sid, "bash", Some("pwd"), ApprovalScope::Session, "build")
        .await;
    let events = h.turn().await;
    let (inside, inside_err) = output(&events, "in");
    assert!(
        !inside_err && inside.contains(&h.root.display().to_string()),
        "{inside}"
    );
    let asked = requests(&events);
    assert!(
        asked.len() == 1 && asked[0].contains("outside the project root"),
        "only the out-of-root run asks: {asked:?}"
    );
    let (out, out_err) = output(&events, "out");
    assert!(
        out_err && !out.contains(&outside),
        "rejected, never run: {out}"
    );
}

/// What an out-of-root approval records answers that command in that
/// directory — not another command there, and not the command at the root.
#[tokio::test]
async fn an_escape_grant_covers_its_command_in_its_directory_only() {
    let outside = |cmd: &str| serde_json::json!({"command": cmd, "workdir": "../outside"});
    let h = harness(
        vec![
            call("same", "bash", outside("pwd")),
            call("other", "bash", outside("echo hi")),
            call("root", "bash", serde_json::json!({"command": "pwd"})),
        ],
        ask_all(),
    );
    let key = escape_grant("bash", &h.outside, Some("pwd".into()));
    h.grants
        .record(
            &h.sid,
            &key.tool,
            Some(&key.arg),
            ApprovalScope::Session,
            "build",
        )
        .await;
    let events = h.turn().await;
    let (same, same_err) = output(&events, "same");
    assert!(
        !same_err && same.contains(&h.outside.display().to_string()),
        "{same}"
    );
    let asked = requests(&events);
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert!(output(&events, "other").1, "another command asks");
    assert!(output(&events, "root").1, "the bare command is not granted");
}

/// The `reason` of every approval card in `events`.
fn card_reasons(events: &[OutEvent]) -> Vec<Option<CardReason>> {
    events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolRequest { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect()
}

/// A script's out-of-root `read_raw` is refused as in 0.7 — directly or
/// through the binding: no escape-root card it could not honour.
#[tokio::test]
async fn an_out_of_root_read_raw_fails_without_an_escape_card() {
    let script = r#"read_raw("../outside/o.txt")"#;
    let h = harness(
        vec![
            call("raw", "read_raw", json!({"path": "../outside/o.txt"})),
            call("script", "rhai", json!({ "script": script })),
        ],
        allow_all_table(),
    );
    let events = h.turn().await;
    assert!(card_reasons(&events).is_empty(), "{events:?}");
    let (raw, raw_err) = output(&events, "raw");
    assert!(
        raw_err && raw.contains("escapes working directory"),
        "{raw}"
    );
    // The binding reports the failed nested call as the script's value.
    let (script, _) = output(&events, "script");
    assert!(
        script.contains("escapes working directory") && !script.contains("outer"),
        "{script}"
    );
}

/// Approving whatever a script's out-of-root `read_raw` parks never
/// records a `read#outside-root` grant: the user approved no `read`, so a
/// later plain `read` of that path still asks.
#[tokio::test]
async fn approving_an_out_of_root_read_raw_grants_no_read_escape() {
    let script = r#"read_raw("../outside/o.txt")"#;
    let h = harness(
        vec![call("script", "rhai", json!({ "script": script }))],
        ask_all(),
    );
    let events = h.turn_answering(Some(ApprovalScope::Session)).await;
    assert!(
        !card_reasons(&events).contains(&Some(CardReason::RootEscape)),
        "{events:?}"
    );
    let (out, _) = output(&events, "script");
    assert!(out.contains("escapes working directory"), "{out}");
    let key = escape_grant("read", &h.outside.join("o.txt"), None);
    assert!(!h
        .grants
        .is_granted(&h.sid, &key.tool, Some(&key.arg), "build"));
}
