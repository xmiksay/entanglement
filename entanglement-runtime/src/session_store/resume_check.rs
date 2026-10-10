//! The pre-resume checks every in-tree resume path shares (`skutter run
//! --resume` and both TUI resume entry points), ADR-0214.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use entanglement_core::{Holly, InMsg, OutEvent, SessionId, DEFAULT_AGENT};

use super::{integrity_gap, read, LogPayload, LogRecord};

/// Agent names ADR-0207 stage 6a retired from the built-in roster: `build`
/// was renamed `general`; `explore`/`research` became the `research`
/// permission mode rather than a persona.
const RETIRED_AGENTS: &[&str] = &["build", "explore", "research"];

/// The retired agents that were read-only. A session that last ran under one
/// resumes in this mode unless the user chose one explicitly (ADR-0214): the
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
            "note: {who} used retired agent `{}`; resuming as `{}`{mode} (ADR-0214)",
            self.retired, self.resumed_as
        )
    }
}

/// What resuming a log entails beyond the replay itself.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ResumeCheck {
    /// Sessions resume will rebuild under a fallback agent, in log order.
    pub fallbacks: Vec<Fallback>,
}

#[derive(Default)]
struct Seen {
    root: bool,
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
        if let OutEvent::SessionStarted { root, .. } = ev {
            s.root = *root;
            s.live = true;
        }
        s.last_named = Some(name.clone());
        if known(name) {
            s.effective = Some(name.clone());
        }
    }
    let fallbacks = order
        .into_iter()
        .filter_map(|sid| {
            let s = seen.remove(&sid)?;
            // Resume rebuilds the root plus every still-live child (ADR-0112).
            let resumed = s.root || s.live;
            let retired = s.last_named?;
            let is_fallback = RETIRED_AGENTS.contains(&retired.as_str()) && !known(&retired);
            (resumed && is_fallback).then(|| Fallback {
                session: sid,
                root: s.root,
                retired,
                resumed_as: s.effective.unwrap_or_else(|| DEFAULT_AGENT.to_string()),
            })
        })
        .collect();
    Ok(ResumeCheck { fallbacks })
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

/// After `Holly::resume` (and after the head applied its own `--mode` /
/// config mode, which cascades over the tree), narrow each read-only
/// fallback session to `research`. Sent per session, so it cascades only
/// over that session's own sub-tree.
pub async fn apply_forced_modes(
    holly: &Holly,
    check: &ResumeCheck,
    explicit_mode: bool,
) -> Result<()> {
    for fallback in &check.fallbacks {
        if let Some(mode) = fallback.forced_mode(explicit_mode) {
            holly
                .send(InMsg::SetMode {
                    session: fallback.session.clone(),
                    mode: mode.to_string(),
                })
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "resume_check_tests.rs"]
mod tests;
