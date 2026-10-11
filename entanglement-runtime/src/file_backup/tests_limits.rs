//! Configurable backup limits (#787): the `undo:` block's `enabled` switch
//! and caps, including live changes through [`FileBackups::set_limits`].

use super::tests::Fixture;
use super::*;

fn off() -> BackupLimits {
    BackupLimits {
        enabled: false,
        ..BackupLimits::default()
    }
}

#[test]
fn disabled_records_nothing_and_undo_says_so() {
    let f = Fixture::with(|store| FileBackups::with_backup_limits(store, off()));
    std::fs::write(f.path("a.txt"), "v0").unwrap();
    f.prompt(&f.sid, "edit");
    f.change(&f.sid, "a.txt", Some(b"v1"));

    let dir = f.store.backups_dir(&f.sid).unwrap();
    assert!(!dir.exists(), "nothing is written while disabled");
    let err = f.backups.revert(&f.sid, None).unwrap_err();
    assert!(format!("{err:#}").contains("undo disabled"), "{err:#}");
    let err = f.backups.revert(&f.sid, Some(0)).unwrap_err();
    assert!(format!("{err:#}").contains("undo disabled"), "{err:#}");
    assert_eq!(f.read("a.txt").unwrap(), b"v1", "nothing touched");
}

#[test]
fn backups_recorded_before_disabling_still_restore() {
    let f = Fixture::new();
    std::fs::write(f.path("a.txt"), "v0").unwrap();
    f.prompt(&f.sid, "edit");
    f.change(&f.sid, "a.txt", Some(b"v1"));

    f.backups.set_limits(off());
    f.prompt(&f.sid, "edit untracked");
    f.change(&f.sid, "b.txt", Some(b"new"));

    let report = f.backups.revert(&f.sid, None).unwrap();
    assert_eq!(report.restored, vec!["a.txt"]);
    assert_eq!(f.read("a.txt").unwrap(), b"v0");
    assert_eq!(
        f.read("b.txt").unwrap(),
        b"new",
        "never recorded, never reverted"
    );
    // Everything recorded is now reverted: the next undo explains why
    // there is nothing left rather than silently doing nothing.
    let err = f.backups.revert(&f.sid, None).unwrap_err();
    assert!(format!("{err:#}").contains("undo disabled"), "{err:#}");
}

#[test]
fn changed_caps_apply_to_the_next_write_only() {
    let f = Fixture::new();
    std::fs::write(f.path("a.txt"), "0123456789").unwrap();
    std::fs::write(f.path("b.txt"), "0123456789").unwrap();
    f.prompt(&f.sid, "edit");
    f.change(&f.sid, "a.txt", Some(b"x"));
    f.backups.set_limits(BackupLimits {
        max_file_bytes: 4,
        ..BackupLimits::default()
    });
    f.change(&f.sid, "b.txt", Some(b"y"));

    let report = f.backups.revert(&f.sid, None).unwrap();
    assert_eq!(report.restored, vec!["a.txt"], "recorded under the old cap");
    assert_eq!(report.skipped.len(), 1, "{report:?}");
    assert_eq!(report.skipped[0].path, "b.txt");
    assert!(
        report.skipped[0]
            .reason
            .contains("4 bytes per-file backup cap"),
        "{report:?}"
    );
}

#[test]
fn re_enabling_resumes_recording() {
    let f = Fixture::with(|store| FileBackups::with_backup_limits(store, off()));
    f.prompt(&f.sid, "edit");
    f.change(&f.sid, "a.txt", Some(b"untracked"));
    f.backups.set_limits(BackupLimits::default());
    f.prompt(&f.sid, "edit again");
    f.change(&f.sid, "b.txt", Some(b"tracked"));

    let report = f.backups.revert(&f.sid, None).unwrap();
    assert_eq!(report.restored, vec!["b.txt"]);
    assert_eq!(f.read("b.txt"), None);
    assert_eq!(f.read("a.txt").unwrap(), b"untracked");
}
