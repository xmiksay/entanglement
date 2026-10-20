//! What a background script that followed its session's compaction
//! (ADR-0241) reads per call: the session's current mode (a grant earned
//! under an older mode no longer clears it), the mode-scoped overlay (a
//! launch-mode enable lapses), the switch's wait for the executor's mode
//! fold, and an undo backup placed by the successor's clock rather than the
//! predecessor's `ToolExec` seq.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use entanglement_core::{
    ApprovalScope, EngineConfig, Holly, InMsg, Llm, OutEvent, Permission, PermissionProfile,
    SessionId, ToolOverlayEntry,
};
use entanglement_runtime::file_backup::FileBackups;
use entanglement_runtime::host::{host_tools_with, BashTool, HostToolOptions};
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};
use entanglement_runtime::policy::{DefaultGrantStore, ModeResolver};
use entanglement_runtime::session_store::SessionStore;
use entanglement_runtime::tool_runner::ToolExecutor;
use serde_json::json;
use tokio::sync::broadcast::Receiver;

use super::background_follow::{
    any, approve, compact, edit_card_on, executor_caught_up, nested_edit_on, no_card, until, Follow,
};
use super::{answer_launch_cards, TempDir};

const WAIT: Duration = Duration::from_secs(5);

type Modes = Arc<Mutex<HashMap<SessionId, String>>>;

/// `build`, `strict` and `unattended` grade alike: everything allowed but
/// `edit`, which asks. `strict` differs only by name, which is all a grant's
/// mode scope sees; `unattended` also has a `question_timeout`, so a first
/// `Ask` collapses to a denial (ADR-0207 §11).
fn table() -> Arc<ModeTable> {
    let mode = |name: &str, question_timeout| Mode {
        name: name.to_string(),
        default: Permission::Allow,
        rules: Rules::from_lists(&[], &[], &["edit".to_string()]),
        limits: Limits {
            question_timeout,
            ..Limits::default()
        },
        sandbox: None,
        sandbox_network: false,
    };
    let modes = vec![mode("build", 0), mode("strict", 0), mode("unattended", 30)];
    Arc::new(ModeTable::new(modes).expect("valid table"))
}

/// The engine over [`Follow`], with `bash` and optional undo backups; also
/// the executor's folded mode map.
pub(super) fn engine(root: &Path, script: String, backups: Option<FileBackups>) -> (Holly, Modes) {
    let direct = json!({"path": "f.txt", "oldString": "", "newString": "zero\n"});
    let agents =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(Follow {
                script: script.clone(),
                direct: direct.clone(),
            }) as Box<dyn Llm>
        }),
        agents: agents.clone(),
        ..EngineConfig::default()
    });
    answer_launch_cards(&holly);
    let base = PermissionProfile::new(Permission::Allow);
    let options = HostToolOptions {
        backups,
        ..HostToolOptions::default()
    };
    let mut tools = host_tools_with(root.to_path_buf(), options);
    tools.register(BashTool::new(root.to_path_buf()));
    let tools = tools.shared();
    let modes: Modes = Arc::default();
    let resolver = Arc::new(ModeResolver::new(
        modes.clone(),
        table(),
        tools.clone(),
        base.clone(),
        None,
    ));
    let _executor = ToolExecutor::builder(&holly, tools)
        .agents(agents)
        .base(base)
        .perm_modes(modes.clone())
        .resolver(resolver)
        .grants(Arc::new(DefaultGrantStore::load_from(None)))
        .mode_table(table())
        .spawn();
    (holly, modes)
}

/// A script line that blocks until `gate` exists, first touching `mark`.
pub(super) fn wait_on(gate: &Path, mark: Option<&Path>) -> String {
    let touch = mark.map_or(String::new(), |m| format!("touch '{}'; ", m.display()));
    format!(
        r#"bash("{touch}while [ ! -f '{}' ]; do sleep 0.05; done");"#,
        gate.display()
    )
}

pub(super) async fn wait_for_file(path: &Path) {
    tokio::time::timeout(WAIT, async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{} never appeared", path.display()));
}

/// Launch on `pred`, wait for the handle, compact into `succ`, and wait for
/// the executor to fold the successor's start.
pub(super) async fn launch_and_compact(
    holly: &Holly,
    sub: &mut Receiver<OutEvent>,
    pred: &SessionId,
    succ: &SessionId,
) {
    holly.send(InMsg::prompt(pred.clone(), "go")).await.unwrap();
    until(sub, "the launch's handle", no_card, |ev| {
        matches!(ev, OutEvent::ToolOutput { session, tool, .. } if session == pred && tool == "rhai")
    })
    .await;
    compact(holly, sub, pred, succ).await;
    executor_caught_up(holly, sub, succ).await;
}

pub(super) async fn set_mode(
    holly: &Holly,
    sub: &mut Receiver<OutEvent>,
    session: &SessionId,
    mode: &str,
) {
    holly
        .send(InMsg::SetMode {
            session: session.clone(),
            mode: mode.into(),
        })
        .await
        .unwrap();
    executor_caught_up(holly, sub, session).await;
}

pub(super) fn ids() -> (SessionId, SessionId) {
    (SessionId::new("pred"), SessionId::new("succ"))
}

pub(super) fn gate(dir: &TempDir, name: &str) -> PathBuf {
    dir.path.join(name)
}

/// ADR-0207 §8: a grant matches only the mode it was earned in. The script
/// switches to the successor while it is in `build`, the model earns a
/// `build` session grant for `f.txt`, the user moves the successor to
/// `strict` — the script's next edit of `f.txt` is still asked.
#[tokio::test]
async fn a_grant_earned_under_the_old_mode_no_longer_clears_the_script() {
    let dir = TempDir::new("bg-follow-mode");
    let (g1, g2, switched) = (gate(&dir, "g1"), gate(&dir, "g2"), gate(&dir, "sw"));
    let script = format!(
        r#"{} {} edit("f.txt", "zero\n", "one\n"); "finished""#,
        wait_on(&g1, None),
        wait_on(&g2, Some(&switched))
    );
    let (holly, _modes) = engine(&dir.path, script, None);
    let (pred, succ) = ids();
    let mut sub = holly.subscribe();
    launch_and_compact(&holly, &mut sub, &pred, &succ).await;

    holly
        .send(InMsg::prompt(succ.clone(), "edit"))
        .await
        .unwrap();
    let card = until(&mut sub, "the direct edit's card", any, |ev| {
        edit_card_on(ev, &succ)
    })
    .await;
    let OutEvent::ToolRequest { request_id, .. } = card else {
        unreachable!("matched a ToolRequest")
    };
    approve(&holly, &succ, &request_id, ApprovalScope::Session).await;
    until(&mut sub, "the direct edit's output", no_card, |ev| {
        matches!(ev, OutEvent::ToolOutput { session, tool, .. } if session == &succ && tool == "edit")
    })
    .await;

    std::fs::write(&g1, "").unwrap();
    wait_for_file(&switched).await;
    set_mode(&holly, &mut sub, &succ, "strict").await;
    std::fs::write(&g2, "").unwrap();
    until(&mut sub, "the script's edit asked again", no_card, |ev| {
        edit_card_on(ev, &succ)
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(dir.path.join("f.txt")).unwrap(),
        "zero\n"
    );
}

/// The overlay is mode-scoped (ADR-0207 §8): an `allow` enable set on the
/// predecessor in `build` is in the script's launch snapshot, but once the
/// successor is in `strict` the executor dropped it, and so does the script.
#[tokio::test]
async fn a_launch_mode_overlay_enable_lapses_after_the_successors_mode_changes() {
    let dir = TempDir::new("bg-follow-overlay");
    let g1 = gate(&dir, "g1");
    let script = format!(
        r#"{} edit("f.txt", "", "one\n"); "finished""#,
        wait_on(&g1, None)
    );
    let (holly, _modes) = engine(&dir.path, script, None);
    let (pred, succ) = ids();
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::SetToolOverlay {
            session: pred.clone(),
            entries: vec![ToolOverlayEntry::allow("edit")],
        })
        .await
        .unwrap();
    launch_and_compact(&holly, &mut sub, &pred, &succ).await;
    set_mode(&holly, &mut sub, &succ, "strict").await;

    std::fs::write(&g1, "").unwrap();
    until(&mut sub, "the script's edit asked", no_card, |ev| {
        edit_card_on(ev, &succ)
    })
    .await;
    assert!(!dir.path.join("f.txt").exists(), "the edit has not run");
}

/// Until the executor folded the successor's mode the script stays on the
/// predecessor: switching earlier would grade under an unseen session,
/// which the resolver fails closed. Its grant stays where it looks, on the
/// predecessor, so the next edit of the file runs unasked. The executor's
/// fold is simulated as lagging by removing the successor's entry.
#[tokio::test]
async fn the_switch_waits_for_the_successors_mode_fold() {
    let dir = TempDir::new("bg-follow-fold");
    let (g1, started) = (gate(&dir, "g1"), gate(&dir, "started"));
    let script = format!(
        r#"{} edit("f.txt", "", "one\n"); edit("f.txt", "one\n", "two\n"); "finished""#,
        wait_on(&g1, Some(&started))
    );
    let (holly, modes) = engine(&dir.path, script, None);
    let (pred, succ) = ids();
    let mut sub = holly.subscribe();
    // The first call must already be on the bridge (as the predecessor)
    // before the compaction, so the edit is the first chance to switch.
    holly.send(InMsg::prompt(pred.clone(), "go")).await.unwrap();
    wait_for_file(&started).await;
    compact(&holly, &mut sub, &pred, &succ).await;
    executor_caught_up(&holly, &mut sub, &succ).await;
    modes.lock().unwrap().remove(&succ);

    std::fs::write(&g1, "").unwrap();
    let card = until(
        &mut sub,
        "the script's edit card",
        no_card,
        |ev| matches!(ev, OutEvent::ToolRequest { tool, .. } if tool.contains("edit")),
    )
    .await;
    let OutEvent::ToolRequest { request_id, .. } = card else {
        unreachable!("matched a ToolRequest")
    };
    // Parked as the predecessor, so the predecessor's id answers it.
    approve(&holly, &pred, &request_id, ApprovalScope::Session).await;
    until(&mut sub, "the second edit, unasked", no_card, |ev| {
        nested_edit_on(ev, &pred, 3)
    })
    .await;
    let f = dir.path.join("f.txt");
    assert_eq!(std::fs::read_to_string(f).unwrap(), "two\n");
}

/// ADR-0212's backup of a write the switched script makes is the
/// successor's, without the launch's `ToolExec` seq: that seq is a record
/// of the predecessor's log, and would name an unrelated one in the
/// successor's.
#[tokio::test]
async fn a_switched_scripts_backup_carries_no_launch_exec_seq() {
    let dir = TempDir::new("bg-follow-backup");
    let g1 = gate(&dir, "g1");
    let store = SessionStore::at(dir.path.join("sessions"));
    let script = format!(
        r#"{} edit("f.txt", "", "one\n"); "finished""#,
        wait_on(&g1, None)
    );
    let (holly, _modes) = engine(&dir.path, script, Some(FileBackups::new(store.clone())));
    let (pred, succ) = ids();
    let mut sub = holly.subscribe();
    launch_and_compact(&holly, &mut sub, &pred, &succ).await;

    std::fs::write(&g1, "").unwrap();
    let card = until(&mut sub, "the script's edit card", no_card, |ev| {
        edit_card_on(ev, &succ)
    })
    .await;
    let OutEvent::ToolRequest { request_id, .. } = card else {
        unreachable!("matched a ToolRequest")
    };
    approve(&holly, &succ, &request_id, ApprovalScope::Once).await;
    until(&mut sub, "the edit's report", no_card, |ev| {
        nested_edit_on(ev, &succ, 2)
    })
    .await;

    let manifest = store.backups_dir(&succ).unwrap().join("manifest.jsonl");
    let text = std::fs::read_to_string(&manifest).expect("the successor's manifest");
    let change: serde_json::Value = text
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|v| v["kind"] == "change")
        .expect("one recorded change");
    assert_eq!(change["session"], "succ", "{change}");
    assert!(change.get("exec_seq").is_none(), "{change}");
}
