//! Undo/rewind end to end (#731, ADR-0212): a scripted model writes a file
//! across two turns through the real executor + host tools (with backups) +
//! persistence tap; `InMsg::RevertFiles` then rolls the edits back through
//! the runtime's `FileBackups::reverter` seam, and a resume of the resulting
//! log never touches the disk again.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, SessionId, ToolCall,
};
use entanglement_runtime::extra_roots::ExtraRootStore;
use entanglement_runtime::file_backup::FileBackups;
use entanglement_runtime::host::host_tools_with_backups;
use entanglement_runtime::persistence::spawn_persistence_subscriber;
use entanglement_runtime::policy::{DefaultGrantStore, ModeResolver};
use entanglement_runtime::session_store::{pair_records, SessionStore};
use entanglement_runtime::tool_runner::ToolExecutor;

/// Pops scripted responses (shared across sessions), then plain text.
struct Scripted(Arc<Mutex<Vec<LlmResponse>>>);

#[async_trait]
impl Llm for Scripted {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let resp = self.0.lock().unwrap().pop().unwrap_or_else(|| LlmResponse {
            text: "done".into(),
            ..Default::default()
        });
        Ok(stream_from_response(resp))
    }
}

pub(crate) fn write_call(id: &str, path: &str, content: &str) -> LlmResponse {
    LlmResponse {
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "write".into(),
            input: serde_json::json!({"path": path, "content": content}).to_string(),
            provider_meta: None,
        }],
        ..Default::default()
    }
}

/// An engine wired the way `skutter` wires it: backups on the file tools,
/// the reverter seam on core, lineage tracking, and the persistence tap.
fn spawn(work: &Path, store: &SessionStore, script: Vec<LlmResponse>) -> Holly {
    spawn_with(work, store, FileBackups::new(store.clone()), script)
}

/// [`spawn`] around a caller-built `backups` (e.g. with config limits).
pub(crate) fn spawn_with(
    work: &Path,
    store: &SessionStore,
    backups: FileBackups,
    script: Vec<LlmResponse>,
) -> Holly {
    let mut script = script;
    script.reverse();
    let script = Arc::new(Mutex::new(script));
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || Box::new(Scripted(script.clone())) as Box<dyn Llm>),
        agents: entanglement_runtime::agents::built_in_registry().unwrap(),
        file_reverter: Some(backups.reverter()),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    backups.track_roots(&holly);
    spawn_persistence_subscriber(&holly, store.clone());
    let tools = host_tools_with_backups(
        work.to_path_buf(),
        Some(Arc::new(ExtraRootStore::ephemeral())),
        Some(backups),
        None,
    )
    .shared();
    let base = entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow);
    let perm_modes = crate::mode_support::perm_modes();
    let table = crate::mode_support::allow_all_table();
    let resolver = Arc::new(ModeResolver::new(
        perm_modes.clone(),
        table.clone(),
        tools.clone(),
        base.clone(),
        Some(work.to_path_buf()),
    ));
    let _executor = ToolExecutor::builder(&holly, tools)
        .base(base)
        .perm_modes(perm_modes)
        .resolver(resolver)
        .grants(Arc::new(DefaultGrantStore::load_from(None)))
        .mode_table(table)
        .spawn();
    holly
}

/// Send `msg` and wait for the first event matching `done` for `sid`.
pub(crate) async fn send_until(
    holly: &Holly,
    sid: &SessionId,
    msg: InMsg,
    done: impl Fn(&OutEvent) -> bool,
) -> OutEvent {
    let mut sub = holly.subscribe();
    holly.send(msg).await.unwrap();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("timed out")
            .expect("event");
        if ev.session() == Some(sid) && done(&ev) {
            return ev;
        }
    }
}

pub(crate) fn is_done(ev: &OutEvent) -> bool {
    matches!(ev, OutEvent::Done { .. })
}

pub(crate) fn is_reverted(ev: &OutEvent) -> bool {
    matches!(ev, OutEvent::FilesReverted { .. })
}

#[tokio::test]
async fn undo_and_rewind_restore_files_and_resume_never_reverts_again() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let store = SessionStore::at(tmp.path().join("sessions"));
    let sid = SessionId::new("s-undo");
    let holly = spawn(
        &work,
        &store,
        vec![
            write_call("w1", "a.txt", "one"),
            LlmResponse {
                text: "created".into(),
                ..Default::default()
            },
            write_call("w2", "a.txt", "two"),
        ],
    );
    let file = work.join("a.txt");

    send_until(
        &holly,
        &sid,
        InMsg::prompt(sid.clone(), "create a"),
        is_done,
    )
    .await;
    send_until(
        &holly,
        &sid,
        InMsg::prompt(sid.clone(), "change a"),
        is_done,
    )
    .await;
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "two");

    // `/undo`: the latest turn with edits (2) goes back.
    let ev = send_until(
        &holly,
        &sid,
        InMsg::RevertFiles {
            session: sid.clone(),
            to_turn: None,
        },
        is_reverted,
    )
    .await;
    assert!(
        matches!(&ev, OutEvent::FilesReverted { to_turn: 1, restored, skipped, .. }
            if restored == &["a.txt"] && skipped.is_empty()),
        "{ev:?}"
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "one");

    // `/rewind 0`: the create goes too.
    send_until(
        &holly,
        &sid,
        InMsg::RevertFiles {
            session: sid.clone(),
            to_turn: Some(0),
        },
        is_reverted,
    )
    .await;
    assert!(
        !file.exists(),
        "the agent created it, so a full rewind removes it"
    );

    // Resume the logged session on a fresh engine: replay folds the two
    // `FilesReverted` records into notes but never re-runs a revert, so a
    // file the user wrote since stays exactly as it is.
    std::fs::write(&file, "user wrote this").unwrap();
    holly.shutdown().await;
    let reverts_in = |records: &[entanglement_runtime::session_store::LogRecord]| {
        records
            .iter()
            .filter(|r| {
                matches!(
                    r.payload,
                    entanglement_runtime::session_store::LogPayload::Out(
                        OutEvent::FilesReverted { .. }
                    )
                )
            })
            .count()
    };
    // The persistence tap writes on its own task: wait for the fact (#815).
    crate::eventually(|| store.read(&sid).is_ok_and(|r| reverts_in(&r) == 2)).await;
    let records = store.read(&sid).unwrap();
    assert_eq!(reverts_in(&records), 2, "both reverts are logged");
    let resumed = spawn(&work, &store, vec![]);
    let mut resumed_sub = resumed.subscribe();
    resumed
        .resume(sid.clone(), pair_records(&records))
        .await
        .unwrap();
    // Barrier (#815): the resumed `SessionStarted` is emitted only after the
    // replay that would have re-run a revert has finished.
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), resumed_sub.recv())
            .await
            .expect("the resumed session starts")
            .unwrap();
        if matches!(&ev, OutEvent::SessionStarted { session, .. } if *session == sid) {
            break;
        }
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "user wrote this");
}
