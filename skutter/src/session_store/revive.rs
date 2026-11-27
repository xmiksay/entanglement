//! Which descendants a resumed root brings back with it (ADR-0247), on every
//! resume path ([`super::resume_with_mode`]: the TUI, `run --resume`, the
//! `serve` wake): core's resume cascade revives only a sub-agent the log
//! still shows live, so a child the root's own hibernate put down —
//! `SessionHibernated` logged for it too — would stay down, its parked card
//! never re-offered.

use std::collections::HashMap;

use entanglement_core::{InMsg, OutEvent, SessionId};

use crate::session_store::{LogPayload, LogRecord};

/// The descendants of `root` to resume after it, parents first: each one
/// whose last lifecycle record is `SessionHibernated` and whose hibernate
/// was the root's cascade. That is the nearest `HibernateSession` logged
/// since the child last started, naming the child or one of its ancestors,
/// names `root` — or none does (the idle sweep, which hibernates only
/// roots). A child hibernated on its own, or under an intermediate sub-agent
/// hibernated on its own, stays down, as does a closed one.
pub fn revived_with(records: &[LogRecord], root: &SessionId) -> Vec<SessionId> {
    let mut parent: HashMap<SessionId, SessionId> = HashMap::new();
    // Per session: down by a hibernate, and whose hibernate put it down.
    let mut down: HashMap<SessionId, Option<SessionId>> = HashMap::new();
    let mut targets: Vec<SessionId> = Vec::new();
    let mut started_at: HashMap<SessionId, usize> = HashMap::new();
    for record in records {
        match &record.payload {
            LogPayload::In(InMsg::HibernateSession { session }) => targets.push(session.clone()),
            // A hibernated id closes with no `SessionEnded`: only its inbound
            // `CloseSession` is logged. Ids are single-use, so it is final;
            // its sub-tree is closed with it and never reached below.
            LogPayload::In(InMsg::CloseSession { session }) => {
                down.remove(session);
            }
            LogPayload::Out(OutEvent::SessionStarted {
                session, parent: p, ..
            }) => {
                if let Some(p) = p {
                    parent.insert(session.clone(), p.clone());
                }
                down.remove(session);
                started_at.insert(session.clone(), targets.len());
            }
            LogPayload::Out(OutEvent::ParentChanged { session, parent: p }) => {
                parent.insert(session.clone(), p.clone());
            }
            LogPayload::Out(OutEvent::SessionHibernated { session, .. }) => {
                let since = started_at.get(session).copied().unwrap_or_default();
                let line = ancestors_or_self(&parent, session);
                let cause = targets
                    .get(since..)
                    .unwrap_or_default()
                    .iter()
                    .rev()
                    .find(|t| line.contains(t))
                    .cloned();
                down.insert(session.clone(), cause);
            }
            LogPayload::Out(OutEvent::SessionEnded { session, .. }) => {
                down.remove(session);
            }
            _ => {}
        }
    }
    let mut revived = Vec::new();
    let mut frontier = vec![root.clone()];
    while let Some(at) = frontier.pop() {
        let mut children: Vec<&SessionId> = parent
            .iter()
            .filter(|(_, p)| **p == at)
            .map(|(c, _)| c)
            .collect();
        children.sort_by(|a, b| a.0.cmp(&b.0));
        for child in children {
            let by_root =
                matches!(down.get(child), Some(cause) if cause.as_ref().is_none_or(|c| c == root));
            if by_root {
                revived.push(child.clone());
                frontier.push(child.clone());
            }
        }
    }
    revived
}

/// `session` and its ancestors, nearest first, cycle-safe.
fn ancestors_or_self(
    parent: &HashMap<SessionId, SessionId>,
    session: &SessionId,
) -> Vec<SessionId> {
    let mut line = vec![session.clone()];
    while let Some(p) = line.last().and_then(|s| parent.get(s)) {
        if line.contains(p) {
            break;
        }
        line.push(p.clone());
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(s: &str) -> SessionId {
        SessionId::new(s)
    }

    fn started(s: &str, parent: Option<&str>) -> LogRecord {
        LogRecord::new(
            sid(s),
            LogPayload::Out(OutEvent::SessionStarted {
                session: sid(s),
                parent: parent.map(sid),
                predecessor: None,
                agent: "general".into(),
                model: None,
                root: parent.is_none(),
                ts: 0,
                user: None,
            }),
        )
    }

    fn hibernate(s: &str) -> LogRecord {
        LogRecord::new(
            sid(s),
            LogPayload::In(InMsg::HibernateSession { session: sid(s) }),
        )
    }

    fn hibernated(s: &str) -> LogRecord {
        LogRecord::new(
            sid(s),
            LogPayload::Out(OutEvent::SessionHibernated {
                session: sid(s),
                ts: 0,
            }),
        )
    }

    fn ended(s: &str) -> LogRecord {
        LogRecord::new(
            sid(s),
            LogPayload::Out(OutEvent::SessionEnded {
                session: sid(s),
                ts: 0,
            }),
        )
    }

    /// The root's hibernate put its sub-tree down: all of it comes back,
    /// parents first.
    #[test]
    fn the_roots_cascade_is_revived() {
        let log = [
            started("r", None),
            started("c", Some("r")),
            started("g", Some("c")),
            hibernate("r"),
            hibernated("g"),
            hibernated("r"),
            hibernated("c"),
        ];
        assert_eq!(revived_with(&log, &sid("r")), vec![sid("c"), sid("g")]);
    }

    /// The idle sweep logs no `HibernateSession`: still the root's cascade.
    #[test]
    fn a_sweep_without_a_logged_target_is_revived() {
        let log = [
            started("r", None),
            started("c", Some("r")),
            hibernated("r"),
            hibernated("c"),
        ];
        assert_eq!(revived_with(&log, &sid("r")), vec![sid("c")]);
    }

    /// A child hibernated on its own stays down, and so does the sub-tree
    /// of an intermediate one; a closed child never comes back.
    #[test]
    fn own_hibernates_and_closed_children_stay_down() {
        let log = [
            started("r", None),
            started("own", Some("r")),
            started("mid", Some("r")),
            started("under", Some("mid")),
            started("gone", Some("r")),
            hibernate("own"),
            hibernated("own"),
            hibernate("mid"),
            hibernated("under"),
            hibernated("mid"),
            ended("gone"),
            hibernate("r"),
            hibernated("r"),
        ];
        assert!(revived_with(&log, &sid("r")).is_empty());
    }

    /// An own hibernate before a later restart does not taint the child's
    /// next hibernate by the root.
    #[test]
    fn only_hibernates_since_the_last_start_count() {
        let log = [
            started("r", None),
            started("c", Some("r")),
            hibernate("c"),
            hibernated("c"),
            started("c", Some("r")),
            hibernate("r"),
            hibernated("c"),
            hibernated("r"),
        ];
        assert_eq!(revived_with(&log, &sid("r")), vec![sid("c")]);
    }

    fn close(s: &str) -> LogRecord {
        LogRecord::new(
            sid(s),
            LogPayload::In(InMsg::CloseSession { session: sid(s) }),
        )
    }

    /// Review F1: a child closed while hibernated logs only its inbound
    /// `CloseSession` — it never comes back, nor does its sub-tree.
    #[test]
    fn a_child_closed_after_its_hibernate_stays_closed() {
        let log = [
            started("r", None),
            started("c", Some("r")),
            started("g", Some("c")),
            started("k", Some("r")),
            hibernate("r"),
            hibernated("g"),
            hibernated("c"),
            hibernated("k"),
            hibernated("r"),
            close("c"),
        ];
        assert_eq!(revived_with(&log, &sid("r")), vec![sid("k")]);
    }

    /// A sweep (no logged target) after a restart: an own hibernate from
    /// before that restart is stale and must not keep the child down.
    #[test]
    fn a_sweep_after_a_restart_ignores_a_stale_own_target() {
        let log = [
            started("r", None),
            started("c", Some("r")),
            hibernate("c"),
            hibernated("c"),
            started("c", Some("r")),
            hibernated("c"),
            hibernated("r"),
        ];
        assert_eq!(revived_with(&log, &sid("r")), vec![sid("c")]);
    }

    /// An end after a hibernate is final.
    #[test]
    fn an_end_after_a_hibernate_is_final() {
        let log = [
            started("r", None),
            started("c", Some("r")),
            hibernated("c"),
            ended("c"),
            hibernated("r"),
        ];
        assert!(revived_with(&log, &sid("r")).is_empty());
    }

    /// An intermediate hibernated on its own, then resumed on its own,
    /// leaves its own child down (core's cascade skipped it); the root's
    /// later hibernate brings back only the intermediate.
    #[test]
    fn a_grandchild_down_under_an_own_hibernate_stays_down() {
        let log = [
            started("r", None),
            started("mid", Some("r")),
            started("c", Some("mid")),
            hibernate("mid"),
            hibernated("c"),
            hibernated("mid"),
            started("mid", Some("r")),
            hibernate("r"),
            hibernated("mid"),
            hibernated("r"),
        ];
        assert_eq!(revived_with(&log, &sid("r")), vec![sid("mid")]);
    }
}
