//! Dedicated plans-folder watch (#627, ADR-0145 "Consequences"): notifies a
//! plan session's transcript live when its bound file changes **out of
//! band** — through an editor, not this session's own `edit`/`write`/
//! `apply_patch` — instead of only failing at the *next*
//! `propose_plan(path=...)` call.
//!
//! Deliberately not folded into `watch.rs`'s #329 definitions watcher: that
//! one reloads agent/skill/config *definitions*, a different reload action
//! entirely — bolting a plans-folder watch onto it would conflate two
//! unrelated semantics in one module. This reuses only its
//! `spawn_debounced_watcher` primitive, which is already
//! definitions-decoupled.
//!
//! Detection reuses [`PlanFileRegistry`], the same registry `propose_plan`'s
//! staleness guard and the tool executor's `FileChange` listener already keep
//! current (`plan_files.rs`): a debounced firing re-hashes every currently
//! bound file and compares it against the registry's last-known hash. A
//! mismatch means the file changed since the registry last heard about it
//! from *this* runtime's own tool calls — i.e. an out-of-band edit — so it's
//! both notice-worthy and, crucially, refreshed into the registry immediately
//! (mirroring `note_file_change`), so the *next* `propose_plan(path=...)`
//! doesn't also refuse it as stale: the head was already told live, repeating
//! the same fact as a hard tool error would be noise, not new information.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use entanglement_core::{Holly, OutEvent, SessionId};
use sha2::{Digest, Sha256};

use crate::plan_files::{PlanBinding, PlanFileRegistry};
use crate::propose_plan::PLANS_DIR;
use crate::watch::spawn_debounced_watcher;

/// Same debounce window as the definitions watcher — collapses an editor's
/// save-as-two-writes into one notice.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// Start the plans-folder watch for `root`'s `.entanglement/plans/` dir.
/// `plan_files` must be the *same* registry `propose_plan` and the tool
/// executor's `FileChange` listener share, so a detected out-of-band change
/// both notifies and self-heals the registry. Returns `None` when the folder
/// doesn't exist yet (no plan has ever been proposed in this project) —
/// mirrors `spawn_debounced_watcher`'s own "nothing to watch yet" behavior; a
/// later `content` submission creates the folder but, like the definitions
/// watcher's own known v1 limitation, this watch needs a restart to pick up a
/// directory that didn't exist at watch-start.
pub fn spawn_plans_watcher(
    holly: &Holly,
    root: PathBuf,
    plan_files: Arc<PlanFileRegistry>,
) -> Option<tokio::task::JoinHandle<()>> {
    let plans_dir = root.join(PLANS_DIR);
    let holly = holly.clone();
    spawn_debounced_watcher(vec![plans_dir], DEBOUNCE, move || {
        for (session, changed) in out_of_band_changes(&root, plan_files.snapshot()) {
            plan_files.record(
                session.clone(),
                changed.rel_path.clone(),
                changed.hash.clone(),
            );
            holly.emit_for_session(&session, |seq| OutEvent::PlanChanged {
                session: session.clone(),
                seq,
                path: changed.rel_path.clone(),
                hash: changed.hash.clone(),
            });
        }
    })
}

/// Re-hash every binding's file under `root` and return the ones whose
/// on-disk content no longer matches the registry's last-known hash — i.e.
/// changed by something other than this runtime's own tracked writes,
/// carrying the freshly-computed hash. A binding whose file is gone (deleted,
/// or a stale entry for a session that never wrote it) is skipped, not
/// reported: nothing a head can usefully render. Pure and independently
/// unit-testable, mirroring `watch.rs`'s `fingerprint`/`definitions_changed`
/// split.
fn out_of_band_changes(
    root: &Path,
    bindings: Vec<(SessionId, PlanBinding)>,
) -> Vec<(SessionId, PlanBinding)> {
    bindings
        .into_iter()
        .filter_map(|(session, binding)| {
            let content = std::fs::read(root.join(&binding.rel_path)).ok()?;
            let hash = format!("{:x}", Sha256::digest(&content));
            if hash == binding.hash {
                return None;
            }
            Some((
                session,
                PlanBinding {
                    rel_path: binding.rel_path,
                    hash,
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("entanglement-plan-watch-unit-")
            .tempdir()
            .unwrap()
    }

    fn hash(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn unchanged_file_is_not_reported() {
        let dir = tempdir();
        std::fs::write(dir.path().join("s1.md"), "# v1").unwrap();
        let bindings = vec![(
            SessionId::new("s1"),
            PlanBinding {
                rel_path: "s1.md".into(),
                hash: hash(b"# v1"),
            },
        )];
        assert!(out_of_band_changes(dir.path(), bindings).is_empty());
    }

    #[test]
    fn a_changed_file_is_reported_with_its_new_hash() {
        let dir = tempdir();
        std::fs::write(dir.path().join("s1.md"), "# v2, edited by the user").unwrap();
        let bindings = vec![(
            SessionId::new("s1"),
            PlanBinding {
                rel_path: "s1.md".into(),
                hash: hash(b"# v1"),
            },
        )];
        let changed = out_of_band_changes(dir.path(), bindings);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].0, SessionId::new("s1"));
        assert_eq!(changed[0].1.hash, hash(b"# v2, edited by the user"));
        assert_eq!(changed[0].1.rel_path, "s1.md");
    }

    #[test]
    fn a_missing_file_is_skipped_not_reported() {
        let dir = tempdir();
        let bindings = vec![(
            SessionId::new("s1"),
            PlanBinding {
                rel_path: "gone.md".into(),
                hash: "deadbeef".into(),
            },
        )];
        assert!(out_of_band_changes(dir.path(), bindings).is_empty());
    }

    #[test]
    fn multiple_bindings_are_diffed_independently() {
        let dir = tempdir();
        std::fs::write(dir.path().join("a.md"), "# a v1").unwrap();
        std::fs::write(dir.path().join("b.md"), "# b v2, edited").unwrap();
        let bindings = vec![
            (
                SessionId::new("a"),
                PlanBinding {
                    rel_path: "a.md".into(),
                    hash: hash(b"# a v1"),
                },
            ),
            (
                SessionId::new("b"),
                PlanBinding {
                    rel_path: "b.md".into(),
                    hash: hash(b"# b v1"),
                },
            ),
        ];
        let changed = out_of_band_changes(dir.path(), bindings);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].0, SessionId::new("b"));
    }

    #[tokio::test]
    async fn missing_plans_dir_spawns_nothing() {
        let dir = tempdir();
        let holly = Holly::spawn(entanglement_core::EngineConfig::default());
        assert!(spawn_plans_watcher(
            &holly,
            dir.path().to_path_buf(),
            Arc::new(PlanFileRegistry::new())
        )
        .is_none());
    }
}
