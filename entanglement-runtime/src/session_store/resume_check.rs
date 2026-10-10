//! The pre-resume checks every in-tree resume path shares (`skutter run
//! --resume` and both TUI resume entry points): ADR-0214, refined per session
//! with read-only narrowing by ADR-0215.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use entanglement_core::{Holly, InMsg, OutEvent, SessionId, DEFAULT_AGENT};

use super::{integrity_gap, read, LogPayload, LogRecord};

/// Agent names ADR-0207 stage 6a retired from the built-in roster: `build`
/// was renamed `general`; `explore`/`research` became the `research`
/// permission mode rather than a persona.
const RETIRED_AGENTS: &[&str] = &["build", "explore", "research"];

/// The retired agents that were read-only. A session that last ran under one
/// resumes in this mode unless the user chose one explicitly (ADR-0215): the
/// fallback persona must not silently widen what the session may do.
const READ_ONLY_RETIRED: &[&str] = &["explore", "research"];
const READ_ONLY_MODE: &str = "research";

/// One resumed session whose effective agent was a retired one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fallback {
    pub session: SessionId,
    pub root: bool,
    /// The retired name the session last ran under.
    pub retired: String,
    /// What replay actually resumes it as: the last agent it named that the
    /// catalog still carries, else `general` (replay's base seed).
    pub resumed_as: String,
}

impl Fallback {
    /// The mode this session is forced into: `research` for a read-only
    /// retired agent, unless the user passed `--mode` (explicit wins).
    pub fn forced_mode(&self, explicit_mode: bool) -> Option<&'static str> {
        let read_only = READ_ONLY_RETIRED.contains(&self.retired.as_str());
        (read_only && !explicit_mode).then_some(READ_ONLY_MODE)
    }

    /// The one line a resume head prints for this session.
    pub fn note(&self, explicit_mode: bool) -> String {
        let who = if self.root {
            "session".to_string()
        } else {
            format!("session `{}`", self.session)
        };
        let mode = self
            .forced_mode(explicit_mode)
            .map(|m| format!(" in `{m}` mode"))
            .unwrap_or_default();
        format!(
            "note: {who} used retired agent `{}`; resuming as `{}`{mode} (ADR-0215)",
            self.retired, self.resumed_as
        )
    }
}

/// What resuming a log entails beyond the replay itself.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ResumeCheck {
    /// Sessions resume will rebuild under a fallback agent, in log order.
    pub fallbacks: Vec<Fallback>,
    /// Every session resume will rebuild, with its parent, in log order.
    pub resumed: Vec<(SessionId, Option<SessionId>)>,
}

impl ResumeCheck {
    /// The rebuilt sessions forced into `research`: each read-only fallback
    /// plus its rebuilt descendants, since a mode covers the whole spawn
    /// sub-tree (ADR-0207 §6). Empty under an explicit `--mode`.
    fn forced(&self, explicit_mode: bool) -> HashSet<&SessionId> {
        let roots: HashSet<&SessionId> = self
            .fallbacks
            .iter()
            .filter(|f| f.forced_mode(explicit_mode).is_some())
            .map(|f| &f.session)
            .collect();
        if roots.is_empty() {
            return roots;
        }
        let parents: HashMap<&SessionId, Option<&SessionId>> = self
            .resumed
            .iter()
            .map(|(sid, parent)| (sid, parent.as_ref()))
            .collect();
        let under_forced = |sid: &SessionId| {
            // Bounded by the map's size, so a malformed cyclic log can't spin.
            let mut cur = Some(sid);
            for _ in 0..=parents.len() {
                match cur {
                    Some(c) if roots.contains(c) => return true,
                    Some(c) => cur = parents.get(c).copied().flatten(),
                    None => return false,
                }
            }
            false
        };
        self.resumed
            .iter()
            .map(|(sid, _)| sid)
            .filter(|sid| under_forced(sid))
            .collect()
    }

    /// `records` paired for `Holly::resume`, with a synthetic `ModeChanged`
    /// appended per rebuilt session when any session is forced (ADR-0215):
    /// `research` inside a forced sub-tree, `mode` (the head's `--mode` /
    /// config `mode:`, when set) everywhere else.
    ///
    /// WHY in the replay stream and not as a later `SetMode`: a resumed
    /// session announces its replayed mode and re-offers a parked batch
    /// *before* it reads its inbox, so a `SetMode` sent after
    /// `Holly::resume` would let a re-offered call be graded under the wider
    /// mode first. And `mode` can't follow as a `SetMode` on the root either:
    /// that cascades over the whole tree and would widen a forced child until
    /// a re-narrowing landed (#757). Appended last so replay's
    /// last-write-wins fold keeps it.
    pub fn paired_for_resume(
        &self,
        records: &[LogRecord],
        explicit_mode: bool,
        mode: Option<&str>,
    ) -> Vec<(Option<InMsg>, OutEvent)> {
        let mut paired = super::pair_records(records);
        let forced = self.forced(explicit_mode);
        if forced.is_empty() {
            return paired;
        }
        for (sid, _) in &self.resumed {
            let target = if forced.contains(sid) {
                Some(READ_ONLY_MODE)
            } else {
                mode
            };
            if let Some(target) = target {
                paired.push((
                    None,
                    OutEvent::ModeChanged {
                        session: sid.clone(),
                        mode: target.to_string(),
                    },
                ));
            }
        }
        paired
    }
}

#[derive(Default)]
struct Seen {
    root: bool,
    parent: Option<SessionId>,
    live: bool,
    last_named: Option<String>,
    effective: Option<String>,
}

/// Checks a root log before resume: refuses a log with a gap tombstone
/// (#104 — replay would fold an incomplete conversation), and lists every
/// session resume will rebuild whose effective agent is retired. `known`
/// says whether the agent catalog carries a name — it mirrors replay, which
/// folds a `SessionStarted`/`AgentChanged` only for a known agent, so a user
/// agent still named `build` is not a fallback at all.
///
/// #720 (ADR-0214) reversed ADR-0207 stage 6a's refusal: such a log now
/// resumes rather than stranding nearly every 0.6.0 log (`build` was its
/// default agent), and the change is announced, never hidden.
pub fn check_for_resume(
    records: &[LogRecord],
    known: impl Fn(&str) -> bool,
) -> Result<ResumeCheck> {
    if let Some(dropped) = integrity_gap(records) {
        anyhow::bail!(
            "its session log is missing {dropped} record(s) dropped during recording, so \
             replay would reconstruct an incomplete conversation. Start a fresh session instead."
        );
    }
    let mut order: Vec<SessionId> = Vec::new();
    let mut seen: HashMap<SessionId, Seen> = HashMap::new();
    for record in records {
        let LogPayload::Out(ev) = &record.payload else {
            continue;
        };
        let (sid, name) = match ev {
            OutEvent::SessionStarted { session, agent, .. }
            | OutEvent::AgentChanged { session, agent } => (session, agent),
            OutEvent::SessionEnded { session, .. }
            | OutEvent::SessionHibernated { session, .. } => {
                if let Some(s) = seen.get_mut(session) {
                    s.live = false;
                }
                continue;
            }
            _ => continue,
        };
        if !seen.contains_key(sid) {
            order.push(sid.clone());
        }
        let s = seen.entry(sid.clone()).or_default();
        if let OutEvent::SessionStarted { root, parent, .. } = ev {
            s.root = *root;
            s.parent = parent.clone();
            s.live = true;
        }
        s.last_named = Some(name.clone());
        if known(name) {
            s.effective = Some(name.clone());
        }
    }
    let mut check = ResumeCheck::default();
    for sid in order {
        let Some(s) = seen.remove(&sid) else {
            continue;
        };
        // Resume rebuilds the root plus every still-live child (ADR-0112).
        if !(s.root || s.live) {
            continue;
        }
        check.resumed.push((sid.clone(), s.parent));
        let Some(retired) = s.last_named else {
            continue;
        };
        if RETIRED_AGENTS.contains(&retired.as_str()) && !known(&retired) {
            check.fallbacks.push(Fallback {
                session: sid,
                root: s.root,
                retired,
                resumed_as: s.effective.unwrap_or_else(|| DEFAULT_AGENT.to_string()),
            });
        }
    }
    Ok(check)
}

/// [`read`] + [`check_for_resume`] for root session `id`.
pub fn load_for_resume(
    cwd: &Path,
    id: &SessionId,
    known: impl Fn(&str) -> bool,
) -> Result<(Vec<LogRecord>, ResumeCheck)> {
    let records =
        read(cwd, id).with_context(|| format!("Failed to read session records for {id}"))?;
    let check =
        check_for_resume(&records, known).with_context(|| format!("Refusing to resume {id}"))?;
    Ok((records, check))
}

/// Resumes `root` from `records` under `mode` (`--mode` > config `mode:`).
///
/// With a forced session in the tree, every mode rides the replay
/// ([`ResumeCheck::paired_for_resume`]) so the cascading switch never reaches
/// it (#757). Otherwise `mode` goes out as an ordinary `SetMode` on the root,
/// cascading as usual and keeping the model's "changed from" mode notice.
pub async fn resume_with_mode(
    holly: &Holly,
    root: &SessionId,
    records: &[LogRecord],
    check: &ResumeCheck,
    explicit_mode: bool,
    mode: Option<String>,
) -> Result<()> {
    holly
        .resume(
            root.clone(),
            check.paired_for_resume(records, explicit_mode, mode.as_deref()),
        )
        .await?;
    if let Some(mode) = mode.filter(|_| check.forced(explicit_mode).is_empty()) {
        holly
            .send(InMsg::SetMode {
                session: root.clone(),
                mode,
            })
            .await?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "resume_check_tests.rs"]
mod tests;
