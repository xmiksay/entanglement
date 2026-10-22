//! The roots this process hibernated, as `serve`'s wake sees them
//! (ADR-0247): which may be woken, which a wake is running for, and the
//! parent links that map a listed sub-agent to its root. Bounded: past
//! [`MAX_TRACKED`] the oldest sleeping root is forgotten (warned once),
//! and an ended or forgotten root's links go with it.

use std::collections::HashMap;

use entanglement_core::SessionId;
use tokio::sync::watch;

/// The most sleeping roots tracked at once.
pub(super) const MAX_TRACKED: usize = 4096;

enum Phase {
    /// Hibernated, in arrival order (for eviction).
    Asleep(u64),
    /// A wake runs; `true` on the channel once it finished.
    Waking(watch::Receiver<bool>),
}

/// What a listing must do before it goes out.
pub(super) enum Begin {
    /// Not asleep: list now.
    Now,
    /// Run the wake, then report it on this sender.
    Lead(watch::Sender<bool>),
    /// Wait for the running wake.
    Follow(watch::Receiver<bool>),
}

#[derive(Default)]
pub(super) struct Sleepers {
    phases: HashMap<SessionId, Phase>,
    parents: HashMap<SessionId, SessionId>,
    next: u64,
    warned: bool,
}

impl Sleepers {
    /// A `SessionStarted`: its parent link, and — unless it is its own
    /// wake's start — it is awake.
    pub(super) fn started(&mut self, session: &SessionId, parent: Option<&SessionId>) {
        if let Some(parent) = parent {
            self.parents.insert(session.clone(), parent.clone());
        }
        if matches!(self.phases.get(session), Some(Phase::Asleep(_))) {
            self.phases.remove(session);
        }
    }

    pub(super) fn reparented(&mut self, session: &SessionId, parent: &SessionId) {
        self.parents.insert(session.clone(), parent.clone());
    }

    /// A `SessionHibernated`: a root becomes wakeable; a sub-agent comes
    /// back with its root.
    pub(super) fn hibernated(&mut self, session: &SessionId) {
        if self.parents.contains_key(session) || self.phases.contains_key(session) {
            return;
        }
        if self.phases.len() >= MAX_TRACKED {
            self.evict_oldest();
        }
        self.next += 1;
        self.phases
            .insert(session.clone(), Phase::Asleep(self.next));
    }

    /// A `SessionEnded` or `CloseSession`: gone for good, with the links of
    /// its sub-tree (closed with it).
    pub(super) fn ended(&mut self, session: &SessionId) {
        self.phases.remove(session);
        self.forget_tree(session);
    }

    /// Drop the parent links of `top` and every session under it.
    fn forget_tree(&mut self, top: &SessionId) {
        let doomed: Vec<SessionId> = self
            .parents
            .keys()
            .filter(|s| self.line_of(s).contains(&top))
            .cloned()
            .collect();
        for s in doomed {
            self.parents.remove(&s);
        }
    }

    /// `session` and its ancestors, cycle-safe.
    fn line_of<'a>(&'a self, session: &'a SessionId) -> Vec<&'a SessionId> {
        let mut line = vec![session];
        while let Some(parent) = line.last().and_then(|s| self.parents.get(*s)) {
            if line.contains(&parent) {
                break;
            }
            line.push(parent);
        }
        line
    }

    /// The root `session` belongs to (itself for a root), cycle-safe.
    pub(super) fn root_of(&self, session: &SessionId) -> SessionId {
        self.line_of(session)
            .last()
            .map_or_else(|| session.clone(), |root| (*root).clone())
    }

    /// Claim a listing's wake of `root`: the first caller leads it, a later
    /// one follows it. A root `live` again elsewhere is awake.
    pub(super) fn begin(&mut self, root: &SessionId, live: bool) -> Begin {
        match self.phases.get(root) {
            Some(Phase::Waking(done)) => Begin::Follow(done.clone()),
            Some(Phase::Asleep(_)) if !live => {
                let (finished, done) = watch::channel(false);
                self.phases.insert(root.clone(), Phase::Waking(done));
                Begin::Lead(finished)
            }
            Some(Phase::Asleep(_)) => {
                self.phases.remove(root);
                Begin::Now
            }
            None => Begin::Now,
        }
    }

    /// A wake finished: a root still not live goes back to sleep so a later
    /// listing retries it.
    pub(super) fn finish(&mut self, root: &SessionId, live: bool) {
        if live {
            self.phases.remove(root);
        } else {
            self.next += 1;
            self.phases.insert(root.clone(), Phase::Asleep(self.next));
        }
    }

    fn evict_oldest(&mut self) {
        if !self.warned {
            self.warned = true;
            tracing::warn!(
                cap = MAX_TRACKED,
                "serve: too many hibernated roots tracked; forgetting the oldest"
            );
        }
        let oldest = self
            .phases
            .iter()
            .filter_map(|(s, p)| match p {
                Phase::Asleep(n) => Some((*n, s)),
                Phase::Waking(_) => None,
            })
            .min_by_key(|(n, _)| *n)
            .map(|(n, s)| (n, s.clone()));
        if let Some((_, session)) = oldest {
            self.phases.remove(&session);
            self.forget_tree(&session);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(s: &str) -> SessionId {
        SessionId::new(s)
    }

    #[test]
    fn only_roots_sleep_and_a_child_maps_to_its_root() {
        let mut s = Sleepers::default();
        s.started(&sid("r"), None);
        s.started(&sid("c"), Some(&sid("r")));
        s.started(&sid("g"), Some(&sid("c")));
        s.hibernated(&sid("g"));
        s.hibernated(&sid("c"));
        s.hibernated(&sid("r"));
        assert_eq!(s.root_of(&sid("g")), sid("r"));
        assert!(
            matches!(s.begin(&sid("c"), false), Begin::Now),
            "a child is no target"
        );
        assert!(matches!(s.begin(&sid("r"), false), Begin::Lead(_)));
        assert!(matches!(s.begin(&sid("r"), false), Begin::Follow(_)));
    }

    #[test]
    fn a_failed_wake_sleeps_again_and_a_live_one_is_forgotten() {
        let mut s = Sleepers::default();
        s.hibernated(&sid("r"));
        assert!(matches!(s.begin(&sid("r"), false), Begin::Lead(_)));
        s.finish(&sid("r"), false);
        assert!(
            matches!(s.begin(&sid("r"), false), Begin::Lead(_)),
            "retried"
        );
        s.finish(&sid("r"), true);
        assert!(matches!(s.begin(&sid("r"), false), Begin::Now));
    }

    #[test]
    fn the_cap_forgets_the_oldest_sleeper() {
        let mut s = Sleepers::default();
        for i in 0..=MAX_TRACKED {
            s.hibernated(&sid(&format!("r{i}")));
        }
        assert!(
            matches!(s.begin(&sid("r0"), false), Begin::Now),
            "the oldest went"
        );
        let newest = format!("r{MAX_TRACKED}");
        assert!(matches!(s.begin(&sid(&newest), false), Begin::Lead(_)));
        assert!(s.warned);
    }

    #[test]
    fn an_end_or_close_forgets_it() {
        let mut s = Sleepers::default();
        s.hibernated(&sid("r"));
        s.ended(&sid("r"));
        assert!(matches!(s.begin(&sid("r"), false), Begin::Now));
    }

    /// Review F2: an ended or evicted root takes its sub-tree's links along.
    #[test]
    fn an_end_or_eviction_drops_the_trees_links() {
        let mut s = Sleepers::default();
        s.started(&sid("r"), None);
        s.started(&sid("c"), Some(&sid("r")));
        s.started(&sid("g"), Some(&sid("c")));
        s.started(&sid("other"), Some(&sid("q")));
        s.ended(&sid("r"));
        assert_eq!(s.parents.len(), 1, "only the other tree's link is left");
        assert_eq!(s.root_of(&sid("g")), sid("g"));

        let mut s = Sleepers::default();
        s.hibernated(&sid("r0"));
        s.started(&sid("c0"), Some(&sid("r0")));
        for i in 1..=MAX_TRACKED {
            s.hibernated(&sid(&format!("r{i}")));
        }
        assert!(s.parents.is_empty(), "the evicted root's link went with it");
    }

    /// A `ParentChanged` moves a sub-agent under another root, and its
    /// listing then wakes that root.
    #[test]
    fn a_reparented_child_maps_to_its_new_root() {
        let mut s = Sleepers::default();
        s.started(&sid("old"), None);
        s.started(&sid("new"), None);
        s.started(&sid("c"), Some(&sid("old")));
        s.reparented(&sid("c"), &sid("new"));
        assert_eq!(s.root_of(&sid("c")), sid("new"));
    }

    /// A root resumed by another route is awake twice over: its start
    /// forgets it, and a listing that finds it live (a start the tracker
    /// missed) does not wake it either.
    #[test]
    fn a_root_resumed_elsewhere_is_awake() {
        let mut s = Sleepers::default();
        s.hibernated(&sid("r"));
        s.started(&sid("r"), None);
        assert!(
            matches!(s.begin(&sid("r"), false), Begin::Now),
            "its start forgot it"
        );

        let mut s = Sleepers::default();
        s.hibernated(&sid("r"));
        assert!(
            matches!(s.begin(&sid("r"), true), Begin::Now),
            "live: no wake"
        );
        assert!(
            matches!(s.begin(&sid("r"), false), Begin::Now),
            "and forgotten"
        );
    }
}
