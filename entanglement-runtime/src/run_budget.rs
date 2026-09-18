//! Wall-clock + turn-count budget enforcement for a mode's `max_turns`/
//! `max_duration` (ADR-0207 §11, stage 5c). Runtime-side only: core sees
//! nothing new, just the ordinary trusted `InMsg::Stop` any other
//! cancellation already uses (ADR-0207 §2 — core carries no notion of a
//! "budget"). A breach also emits a stated-reason `OutEvent::Error` first,
//! so the user learns *why* the run stopped rather than just that it did —
//! reusing that existing variant instead of inventing a new one.
//!
//! Tracks, per session: its mode (folded from `OutEvent::ModeChanged`,
//! mirroring `tool_runner`'s own `perm_modes` fold), its start time
//! (`SessionStarted`), and its turn count (one `OutEvent::Usage { purpose:
//! Turn, .. }` per model round-trip — a compaction summary's own `Usage`
//! carries `purpose: Compaction` and is not counted). A session whose mode
//! sets neither limit (every built-in mode but a tuned `auto`) costs one map
//! entry and a handful of `None` comparisons — undefined still means
//! unlimited (ADR-0207 §6).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use entanglement_core::{Holly, InMsg, OutEvent, SessionId, UsagePurpose};
use tokio::sync::broadcast::error::RecvError;

use crate::mode::ModeTable;

struct Budget {
    mode: String,
    started: Instant,
    turns: u32,
}

/// Run until the engine's outbox closes, ending any session whose mode's
/// `max_turns`/`max_duration` is breached. Spawned once per executor
/// (inside `tool_runner::spawn_tool_executor_with_policy`, alongside its
/// other background tasks) so every caller — `skutter` included — gets this
/// for free without wiring it up itself. Tracks each session's mode itself
/// (off `OutEvent::ModeChanged`) rather than sharing `tool_runner`'s own
/// `perm_modes` map — this watcher's mode fold only ever needs to be read
/// back here, so a private copy is simpler than a shared one.
///
/// `sub` is taken as a parameter, not subscribed internally: the caller
/// must subscribe *synchronously*, before handing off to this (eventually
/// scheduled) async task, so a `SessionStarted`/`Usage` broadcast right
/// after spawning can't race ahead of this watcher's subscription and be
/// silently missed — the same discipline `tool_runner`'s own main loop
/// documents for its `sub`/`inbound` handles.
pub(crate) async fn watch(
    holly: Holly,
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    mode_table: Arc<ModeTable>,
) {
    let mut sessions: HashMap<SessionId, Budget> = HashMap::new();
    // Sweeps for a wall-clock breach even across a long silent stretch (a
    // slow tool call, a slow model response) that the event-driven arms
    // below would otherwise miss entirely.
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let breaches: Vec<(SessionId, String)> = sessions
                    .iter()
                    .filter_map(|(session, budget)| {
                        duration_breach(&mode_table, budget).map(|reason| (session.clone(), reason))
                    })
                    .collect();
                for (session, reason) in breaches {
                    sessions.remove(&session);
                    end_session(&holly, &session, reason).await;
                }
            }
            ev = sub.recv() => {
                match ev {
                    // Mode isn't known yet at `SessionStarted` (it arrives
                    // via its own `ModeChanged`, mirroring `tool_runner`'s
                    // `perm_modes` fold) — `or_insert_with` seeds an entry so
                    // the start time is pinned to session start even if
                    // `ModeChanged` is still in flight.
                    Ok(OutEvent::SessionStarted { session, .. }) => {
                        sessions.entry(session).or_insert_with(|| Budget {
                            mode: String::new(),
                            started: Instant::now(),
                            turns: 0,
                        });
                    }
                    Ok(OutEvent::ModeChanged { session, mode }) => {
                        sessions
                            .entry(session)
                            .or_insert_with(|| Budget {
                                mode: String::new(),
                                started: Instant::now(),
                                turns: 0,
                            })
                            .mode = mode;
                    }
                    Ok(OutEvent::Usage { session, purpose: UsagePurpose::Turn, .. }) => {
                        let breach = sessions.get_mut(&session).and_then(|budget| {
                            budget.turns += 1;
                            turns_breach(&mode_table, budget)
                        });
                        if let Some(reason) = breach {
                            sessions.remove(&session);
                            end_session(&holly, &session, reason).await;
                        }
                    }
                    Ok(OutEvent::SessionEnded { session, .. })
                    | Ok(OutEvent::SessionHibernated { session, .. }) => {
                        sessions.remove(&session);
                    }
                    Ok(_) => {}
                    // Best-effort (mirrors `tool_runner`'s own lagged arms):
                    // a missed `Usage` under-counts turns and a missed
                    // `SessionStarted` skips tracking entirely — both fail
                    // toward "no budget enforced" rather than a false
                    // breach, matching this watcher's advisory role.
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break,
                }
            }
        }
    }
}

fn turns_breach(table: &ModeTable, budget: &Budget) -> Option<String> {
    let max = table.get(&budget.mode)?.limits.max_turns?;
    (budget.turns >= max).then(|| {
        format!(
            "session ended: mode `{}` max_turns ({max}) exceeded",
            budget.mode
        )
    })
}

fn duration_breach(table: &ModeTable, budget: &Budget) -> Option<String> {
    let max = table.get(&budget.mode)?.limits.max_duration?;
    (budget.started.elapsed() >= Duration::from_secs(max)).then(|| {
        format!(
            "session ended: mode `{}` max_duration ({max}s) exceeded",
            budget.mode
        )
    })
}

async fn end_session(holly: &Holly, session: &SessionId, reason: String) {
    holly.emit_for_session(session, |seq| OutEvent::Error {
        session: session.clone(),
        seq,
        message: reason,
    });
    let _ = holly
        .send(InMsg::Stop {
            session: session.clone(),
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::{Limits, Mode, Rules};
    use entanglement_core::Permission;

    fn mode_with_limits(name: &str, limits: Limits) -> Mode {
        Mode {
            name: name.to_string(),
            default: Permission::Deny,
            rules: Rules::default(),
            limits,
            sandbox: None,
            sandbox_network: false,
        }
    }

    fn table(limits: Limits) -> ModeTable {
        ModeTable::new(vec![mode_with_limits("auto", limits)]).expect("single mode is valid")
    }

    #[test]
    fn turns_breach_none_when_max_turns_unset() {
        let t = table(Limits::default());
        let budget = Budget {
            mode: "auto".to_string(),
            started: Instant::now(),
            turns: 1000,
        };
        assert_eq!(turns_breach(&t, &budget), None);
    }

    #[test]
    fn turns_breach_fires_at_the_limit() {
        let t = table(Limits {
            max_turns: Some(3),
            ..Limits::default()
        });
        let mut budget = Budget {
            mode: "auto".to_string(),
            started: Instant::now(),
            turns: 2,
        };
        assert_eq!(turns_breach(&t, &budget), None);
        budget.turns = 3;
        assert!(turns_breach(&t, &budget).is_some());
    }

    #[test]
    fn duration_breach_none_when_max_duration_unset() {
        let t = table(Limits::default());
        let budget = Budget {
            mode: "auto".to_string(),
            started: Instant::now() - Duration::from_secs(1_000_000),
            turns: 0,
        };
        assert_eq!(duration_breach(&t, &budget), None);
    }

    #[test]
    fn duration_breach_fires_past_the_limit() {
        let t = table(Limits {
            max_duration: Some(60),
            ..Limits::default()
        });
        let fresh = Budget {
            mode: "auto".to_string(),
            started: Instant::now(),
            turns: 0,
        };
        assert_eq!(duration_breach(&t, &fresh), None);
        let stale = Budget {
            mode: "auto".to_string(),
            started: Instant::now() - Duration::from_secs(61),
            turns: 0,
        };
        assert!(duration_breach(&t, &stale).is_some());
    }

    #[test]
    fn unseen_mode_never_breaches() {
        let t = table(Limits {
            max_turns: Some(1),
            max_duration: Some(1),
            ..Limits::default()
        });
        let budget = Budget {
            mode: "no-such-mode".to_string(),
            started: Instant::now() - Duration::from_secs(1_000_000),
            turns: 1000,
        };
        assert_eq!(turns_breach(&t, &budget), None);
        assert_eq!(duration_breach(&t, &budget), None);
    }
}
