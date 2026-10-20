//! Each compaction lineage's overlay epoch, as the executor folded its modes
//! (ADR-0243). The overlay is mode-scoped (ADR-0207 §8): the executor drops
//! a session's overlay on every mode switch, and core hands a compaction
//! successor its predecessor's overlay only when it starts in the
//! predecessor's mode. The epoch counts both, across every member of the
//! lineage, so a running script drops a snapshot entry once its session's
//! lineage epoch moved, through any number of compactions and whichever
//! member switched. Comparing modes alone misses a switch and its reversal.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use entanglement_core::{Holly, SessionId};

/// Each overlay entry's lineage head and that lineage's epoch when the
/// entry was taken, keyed like the entry (re-keyed with it on a move).
pub(crate) type Snapshot = HashMap<SessionId, (SessionId, u64)>;

type Modes = Mutex<HashMap<SessionId, String>>;
type Lineage = Arc<dyn Fn(&SessionId) -> SessionId + Send + Sync>;

/// Per lineage head: the overlay epoch and the mode its latest member
/// folded. A lineage's first folded mode is epoch 1; 0 means never folded.
#[derive(Clone)]
pub(crate) struct ModeEpochs {
    lines: Arc<Mutex<HashMap<SessionId, (u64, String)>>>,
    /// A session's lineage head (`Holly::lineage_head`).
    head: Lineage,
}

// Both maps hold plain values, valid whatever a panicking holder did. Lock
// order everywhere: `perm_modes`, then the epochs; a lineage head is looked
// up before either is taken.
pub(crate) fn lock_modes(perm_modes: &Modes) -> MutexGuard<'_, HashMap<SessionId, String>> {
    perm_modes.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ModeEpochs {
    /// Lineages as the engine's session directory records them. The
    /// supervisor records a successor before its `SessionStarted`, so the
    /// successor's first `ModeChanged` already folds into its lineage.
    pub(crate) fn of(holly: Holly) -> Self {
        Self::with_lineage(Arc::new(move |s| holly.lineage_head(s)))
    }

    fn with_lineage(head: Lineage) -> Self {
        Self {
            lines: Arc::default(),
            head,
        }
    }

    /// Fold one `ModeChanged` into `perm_modes`, returning whether the
    /// session's mode actually changed (its overlay is then dropped). The
    /// lineage epoch moves on a switch, and on a successor's first mode
    /// when it differs from the mode the lineage was last in: core carried
    /// no overlay to it then. A resume re-announces the same mode and moves
    /// nothing. The epoch moves under the same `perm_modes` guard as the
    /// mode, so a reader holding that guard ([`read`](Self::read)) sees both
    /// or neither.
    pub(crate) fn fold(&self, perm_modes: &Modes, session: &SessionId, mode: String) -> bool {
        let head = (self.head)(session);
        let mut modes = lock_modes(perm_modes);
        let previous = modes.get(session);
        if previous == Some(&mode) {
            return false;
        }
        let switched = previous.is_some();
        let mut lines = self.lines();
        match lines.get_mut(&head) {
            Some((epoch, last)) => {
                if switched || *last != mode {
                    *epoch += 1;
                }
                last.clone_from(&mode);
            }
            // A lineage starts at 1, never at the 0 a snapshot records for a
            // lineage it has not seen fold: an entry taken while the first
            // `ModeChanged` was lost to lag then goes on this fold, like the
            // overlay the executor dropped.
            None => {
                lines.insert(head, (1 + u64::from(switched), mode.clone()));
            }
        }
        modes.insert(session.clone(), mode);
        true
    }

    /// The snapshot of `sessions` as of now.
    pub(crate) fn snapshot<'a>(
        &self,
        perm_modes: &Modes,
        sessions: impl Iterator<Item = &'a SessionId>,
    ) -> Snapshot {
        let heads: Vec<_> = sessions.map(|s| (s.clone(), (self.head)(s))).collect();
        let _modes = lock_modes(perm_modes);
        let lines = self.lines();
        heads
            .into_iter()
            .map(|(s, head)| {
                let at = epoch(&lines, &head);
                (s, (head, at))
            })
            .collect()
    }

    /// Under one `perm_modes` guard, as [`fold`](Self::fold) writes them:
    /// drop the `snapshot` entries whose lineage epoch moved, then read
    /// `session`'s mode, so a caller never pairs one moment's mode with
    /// another moment's overlay.
    pub(crate) fn read(
        &self,
        perm_modes: &Modes,
        session: &SessionId,
        snapshot: &mut Snapshot,
    ) -> Option<String> {
        let modes = lock_modes(perm_modes);
        let lines = self.lines();
        snapshot.retain(|_, (head, at)| epoch(&lines, head) == *at);
        modes.get(session).cloned()
    }

    fn lines(&self) -> MutexGuard<'_, HashMap<SessionId, (u64, String)>> {
        self.lines.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn epoch(lines: &HashMap<SessionId, (u64, String)>, head: &SessionId) -> u64 {
    lines.get(head).map_or(0, |(epoch, _)| *epoch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(s: &str) -> SessionId {
        SessionId::new(s)
    }

    /// Every session named `<head>` or `<head>.<n>` is in `<head>`'s lineage.
    fn epochs() -> (ModeEpochs, Modes) {
        let head: Lineage =
            Arc::new(|s: &SessionId| SessionId::new(s.0.split('.').next().unwrap_or_default()));
        (ModeEpochs::with_lineage(head), Mutex::default())
    }

    /// Fold `before`, snapshot `p`'s entry, fold `after`: whether it survives.
    fn survives(before: &[(&str, &str)], after: &[(&str, &str)]) -> bool {
        let (epochs, modes) = epochs();
        for (s, m) in before {
            epochs.fold(&modes, &sid(s), (*m).into());
        }
        let mut snapshot = epochs.snapshot(&modes, [&sid("p")].into_iter());
        for (s, m) in after {
            epochs.fold(&modes, &sid(s), (*m).into());
        }
        epochs.read(&modes, &sid("p"), &mut snapshot);
        !snapshot.is_empty()
    }

    #[test]
    fn a_switch_moves_the_epoch_and_a_duplicate_does_not() {
        let (epochs, modes) = epochs();
        let p = sid("p");
        assert!(epochs.fold(&modes, &p, "build".into()), "first mode");
        assert!(!epochs.fold(&modes, &p, "build".into()), "a duplicate");
        let mut snapshot = epochs.snapshot(&modes, [&p].into_iter());
        assert_eq!(snapshot[&p], (p.clone(), 1), "a folded lineage starts at 1");
        assert!(epochs.fold(&modes, &p, "plan".into()));
        let mode = epochs.read(&modes, &p, &mut snapshot);
        assert_eq!(mode.as_deref(), Some("plan"));
        assert!(snapshot.is_empty());
    }

    #[test]
    fn an_entry_survives_only_an_unswitched_lineage() {
        let start = [("p", "build")];
        assert!(survives(&start, &[]), "nothing moved");
        assert!(
            survives(&start, &[("p.1", "build")]),
            "same-mode compaction"
        );
        assert!(
            survives(&start, &[("p.1", "build"), ("p.2", "build")]),
            "two same-mode compactions"
        );
        assert!(!survives(&start, &[("p", "plan"), ("p", "build")]), "A→B→A");
        assert!(
            !survives(&start, &[("p", "plan"), ("p", "build"), ("p.1", "build")]),
            "A→B→A, then a compaction"
        );
        assert!(
            !survives(&start, &[("p.1", "plan")]),
            "S starts in another mode"
        );
        assert!(
            !survives(
                &start,
                &[
                    ("p.1", "build"),
                    ("p.1", "plan"),
                    ("p.1", "build"),
                    ("p.2", "build")
                ]
            ),
            "the middle link switched and back"
        );
        assert!(
            !survives(
                &start,
                &[("p.1", "plan"), ("p.1", "build"), ("p.2", "build")]
            ),
            "the middle link started in another mode, then switched into the entry's"
        );
        assert!(survives(&start, &[("q", "plan")]), "another lineage");
    }

    /// A snapshot taken after earlier switches (a non-zero epoch) still
    /// survives a compaction that carried the overlay.
    #[test]
    fn a_snapshot_at_a_later_epoch_survives_an_unswitched_compaction() {
        let before = [("p", "build"), ("p", "plan"), ("p", "build")];
        assert!(survives(&before, &[("p.1", "build")]));
        assert!(!survives(&before, &[("p.1", "plan")]));
    }

    /// The first `ModeChanged` was lost to lag but the overlay was folded:
    /// the snapshot records the unseen lineage at 0, and the next fold (a
    /// switch, which drops the executor's overlay) must make it stale.
    #[test]
    fn an_unseen_lineages_first_fold_is_a_switch() {
        let (epochs, modes) = epochs();
        let p = sid("p");
        let mut snapshot = epochs.snapshot(&modes, [&p].into_iter());
        assert_eq!(snapshot[&p], (p.clone(), 0));
        epochs.fold(&modes, &p, "plan".into());
        epochs.read(&modes, &p, &mut snapshot);
        assert!(snapshot.is_empty());
    }
}
