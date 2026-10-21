//! A resumed session's ancestors (ADR-0246): the parent chain up to the
//! nearest live ancestor, read from the supervisor's links and the replayed
//! log (the whole tree folds into its root's file, ADR-0020), and the mode
//! that chain imposes on the session.

use super::starting_mode::narrow_to;
use super::Supervisor;
use crate::holly::DEFAULT_MODE;
use crate::protocol::{InMsg, OutEvent, SessionId};

type Log = [(Option<InMsg>, OutEvent)];

/// `session`'s last mode as the log records it.
fn logged_mode(records: &Log, session: &SessionId) -> Option<String> {
    records.iter().rev().find_map(|(_, ev)| match ev {
        OutEvent::ModeChanged { session: s, mode } if s == session => Some(mode.clone()),
        _ => None,
    })
}

/// `session`'s parent as the log records it: its last `ParentChanged`
/// (ADR-0234), else its `SessionStarted`.
pub(super) fn logged_parent(records: &Log, session: &SessionId) -> Option<SessionId> {
    records.iter().rev().find_map(|(_, ev)| match ev {
        OutEvent::ParentChanged { session: s, parent } if s == session => {
            Some(Some(parent.clone()))
        }
        OutEvent::SessionStarted {
            session: s, parent, ..
        } if s == session => Some(parent.clone()),
        _ => None,
    })?
}

impl Supervisor {
    /// The ancestors of a session whose parent is `parent`, nearest first,
    /// up to and including the nearest live one (or the root). A compacted
    /// ancestor stands for its successor (ADR-0205), which took over its
    /// children.
    pub(super) fn ancestors(&self, parent: Option<SessionId>, records: &Log) -> Vec<SessionId> {
        let mut chain: Vec<SessionId> = Vec::new();
        let mut next = parent;
        while let Some(id) = next {
            let id = self.session_meta.successor_of(&id).unwrap_or(id);
            if chain.contains(&id) {
                break;
            }
            let live = self.sessions.contains_key(&id);
            chain.push(id.clone());
            if live {
                break;
            }
            next = match self.parent_links.get(&id) {
                Some(parent) => parent.clone(),
                None => logged_parent(records, &id),
            };
        }
        chain
    }

    /// The mode `session` last ran in, from the log. While its old task is
    /// still tearing down the log can lag it, and the supervisor's own cache
    /// (written by every `SetMode` routed to that task) is the newer word;
    /// it replaces the log only when provably stricter, so neither a stale
    /// log nor a head's narrowed replay (ADR-0216) is widened.
    fn last_mode(&self, session: &SessionId, logged: Option<String>) -> Option<String> {
        let cached = self
            .modes
            .get(session)
            .filter(|_| self.retiring.contains_key(session));
        match (logged, cached) {
            (Some(logged), Some(cached)) => Some(narrow_to(&logged, cached, &self.cfg.mode_ladder)),
            (logged, cached) => logged.or_else(|| cached.cloned()),
        }
    }

    /// `ancestor`'s own current mode: its live one when it runs, else its
    /// last logged one.
    fn own_mode(&self, ancestor: &SessionId, records: &Log) -> Option<String> {
        if self.sessions.contains_key(ancestor) {
            let live = self
                .modes
                .get(ancestor)
                .map_or(DEFAULT_MODE, String::as_str);
            return Some(live.to_string());
        }
        self.last_mode(ancestor, logged_mode(records, ancestor))
    }

    /// The mode a resumed `target`, replayed in `replayed`, starts in: that,
    /// unless its nearest ancestor's current mode is provably stricter
    /// (ADR-0246). A non-live ancestor's current mode is its logged one,
    /// narrowed the same way by its own ancestors: the mode that ancestor
    /// would itself resume in.
    pub(super) fn resumed_mode(
        &self,
        target: &SessionId,
        replayed: &str,
        parent: Option<SessionId>,
        records: &Log,
    ) -> String {
        let ladder = &self.cfg.mode_ladder;
        let mut ceiling: Option<String> = None;
        for ancestor in self.ancestors(parent, records).iter().rev() {
            ceiling = match (self.own_mode(ancestor, records), ceiling) {
                (Some(own), Some(above)) => Some(narrow_to(&own, &above, ladder)),
                (own, above) => own.or(above),
            };
        }
        let saved = self
            .last_mode(target, Some(replayed.to_string()))
            .unwrap_or_else(|| replayed.to_string());
        match ceiling {
            Some(ceiling) => narrow_to(&saved, &ceiling, ladder),
            None => saved,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(session: &str, parent: Option<&str>) -> (Option<InMsg>, OutEvent) {
        let ev = OutEvent::SessionStarted {
            session: SessionId::new(session),
            parent: parent.map(SessionId::new),
            predecessor: None,
            agent: "general".into(),
            model: None,
            root: parent.is_none(),
            ts: 0,
            user: None,
        };
        (None, ev)
    }

    fn mode(session: &str, mode: &str) -> (Option<InMsg>, OutEvent) {
        let ev = OutEvent::ModeChanged {
            session: SessionId::new(session),
            mode: mode.into(),
        };
        (None, ev)
    }

    #[test]
    fn the_log_gives_the_last_mode_and_parent() {
        let moved = OutEvent::ParentChanged {
            session: SessionId::new("kid"),
            parent: SessionId::new("succ"),
        };
        let log = [
            started("root", None),
            started("kid", Some("root")),
            mode("kid", "build"),
            mode("root", "plan"),
            mode("kid", "research"),
            (None, moved),
        ];
        let kid = SessionId::new("kid");
        assert_eq!(logged_mode(&log, &kid).as_deref(), Some("research"));
        assert_eq!(logged_parent(&log, &kid), Some(SessionId::new("succ")));
        assert_eq!(logged_parent(&log, &SessionId::new("root")), None);
        assert_eq!(logged_mode(&log, &SessionId::new("nobody")), None);
    }
}
