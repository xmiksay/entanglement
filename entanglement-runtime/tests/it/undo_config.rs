//! `config.yml` `undo:` end to end (#787): limits resolved from a layered
//! config file drive what the real `write` tool backs up, and a live limit
//! change (what the definitions watcher applies on reload) takes effect for
//! the next write while existing backups stay restorable.

use entanglement_core::{InMsg, LlmResponse, OutEvent, SessionId};
use entanglement_runtime::config::Config;
use entanglement_runtime::file_backup::{BackupLimits, FileBackups};
use entanglement_runtime::session_store::SessionStore;

use crate::revert_files::{is_done, is_reverted, send_until, spawn_with, write_call};

fn text(t: &str) -> LlmResponse {
    LlmResponse {
        text: t.into(),
        ..Default::default()
    }
}

fn revert(sid: &SessionId) -> InMsg {
    InMsg::RevertFiles {
        session: sid.clone(),
        to_turn: None,
    }
}

fn is_reverted_or_error(ev: &OutEvent) -> bool {
    is_reverted(ev) || matches!(ev, OutEvent::Error { .. })
}

#[tokio::test]
async fn config_caps_and_live_disable_drive_real_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let user = tmp.path().join("user.yml");
    std::fs::write(&user, "undo:\n  max_file_bytes: 4B\n").unwrap();
    let config = Config::load_with(&work, Some(&user)).unwrap();

    let store = SessionStore::at(tmp.path().join("sessions"));
    let backups = FileBackups::with_backup_limits(store.clone(), config.undo);
    let holly = spawn_with(
        &work,
        &store,
        backups.clone(),
        vec![
            write_call("w1", "big.txt", "x"),
            text("one"),
            write_call("w2", "small.txt", "y"),
            text("two"),
            write_call("w3", "other.txt", "z"),
        ],
    );
    std::fs::write(work.join("big.txt"), "0123456789").unwrap();
    std::fs::write(work.join("small.txt"), "abc").unwrap();
    let sid = SessionId::new("s-undo-config");

    // Turn 1: the prior `big.txt` is over the 4-byte config cap.
    send_until(&holly, &sid, InMsg::prompt(sid.clone(), "big"), is_done).await;
    // Turn 2: `small.txt` fits, so it is backed up.
    send_until(&holly, &sid, InMsg::prompt(sid.clone(), "small"), is_done).await;

    // Disable live: turn 3's write is not recorded at all.
    backups.set_limits(BackupLimits {
        enabled: false,
        ..config.undo
    });
    send_until(&holly, &sid, InMsg::prompt(sid.clone(), "more"), is_done).await;
    assert_eq!(
        std::fs::read_to_string(work.join("other.txt")).unwrap(),
        "z"
    );

    // Undo skips untracked turn 3 and restores turn 2's backup, recorded
    // before the switch-off.
    let ev = send_until(&holly, &sid, revert(&sid), is_reverted).await;
    assert!(
        matches!(&ev, OutEvent::FilesReverted { to_turn: 1, restored, skipped, .. }
            if restored == &["small.txt"] && skipped.is_empty()),
        "{ev:?}"
    );
    assert_eq!(
        std::fs::read_to_string(work.join("small.txt")).unwrap(),
        "abc"
    );

    // Turn 1's over-cap change is reported as having no backup.
    let ev = send_until(&holly, &sid, revert(&sid), is_reverted_or_error).await;
    assert!(
        matches!(&ev, OutEvent::FilesReverted { skipped, .. }
            if skipped.len() == 1 && skipped[0].reason.contains("per-file backup cap")),
        "{ev:?}"
    );
    assert_eq!(std::fs::read_to_string(work.join("big.txt")).unwrap(), "x");

    // Nothing recorded is left: the disabled state is named, not a silent no-op.
    let ev = send_until(&holly, &sid, revert(&sid), is_reverted_or_error).await;
    assert!(
        matches!(&ev, OutEvent::Error { message, .. } if message.contains("undo disabled")),
        "{ev:?}"
    );
}
