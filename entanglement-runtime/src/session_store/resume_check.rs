//! The pre-resume checks every in-tree resume path shares (`skutter run
//! --resume` and both TUI resume entry points).

use std::path::Path;

use anyhow::{Context, Result};
use entanglement_core::{OutEvent, SessionId, DEFAULT_AGENT};

use super::{integrity_gap, read, LogPayload, LogRecord};

/// Agent names ADR-0207 stage 6a retired from the built-in roster: `build`
/// was renamed `general`; `explore`/`research` became the `research`
/// permission mode rather than a persona.
const RETIRED_AGENTS: &[&str] = &["build", "explore", "research"];

/// The first retired agent name this log names, if any — checked across every
/// `SessionStarted`/`AgentChanged` record in the whole root file (a cascaded
/// resume rebuilds every spawned child from the same file).
///
/// #720 (ADR-0214) reversed ADR-0207 stage 6a's refusal: such a log now **resumes**,
/// under the base agent (`general`) — exactly what `Holly::resume` already
/// does for an embedder, since replay keeps the base seed for a name the
/// catalog doesn't carry. Refusing stranded nearly every 0.6.0 log (`build`
/// was its default agent); resuming under a different persona is announced
/// via [`retired_agent_note`] rather than hidden.
pub fn retired_agent(records: &[LogRecord]) -> Option<&'static str> {
    records.iter().find_map(|record| {
        let name = match &record.payload {
            LogPayload::Out(OutEvent::SessionStarted { agent, .. })
            | LogPayload::Out(OutEvent::AgentChanged { agent, .. }) => agent.as_str(),
            _ => return None,
        };
        RETIRED_AGENTS.iter().copied().find(|r| *r == name)
    })
}

/// The one-line note a resume head prints when the log names a retired agent.
pub fn retired_agent_note(records: &[LogRecord]) -> Option<String> {
    retired_agent(records).map(|agent| {
        format!(
            "note: session used retired agent `{agent}`; resuming as `{DEFAULT_AGENT}` (ADR-0207)"
        )
    })
}

/// Reads root session `id`'s log for resume: refuses a log with a gap
/// tombstone (#104 — replay would fold an incomplete conversation) and
/// returns the records plus the [`retired_agent_note`], if any, to show.
pub fn load_for_resume(cwd: &Path, id: &SessionId) -> Result<(Vec<LogRecord>, Option<String>)> {
    let records =
        read(cwd, id).with_context(|| format!("Failed to read session records for {id}"))?;
    if let Some(dropped) = integrity_gap(&records) {
        anyhow::bail!(
            "Refusing to resume {id}: its session log is missing {dropped} record(s) dropped \
             during recording, so replay would reconstruct an incomplete conversation. Start a \
             fresh session instead."
        );
    }
    let note = retired_agent_note(&records);
    Ok((records, note))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(agent: &str) -> LogRecord {
        let sid = SessionId::new("s");
        LogRecord::new(
            sid.clone(),
            LogPayload::Out(OutEvent::SessionStarted {
                session: sid,
                parent: None,
                predecessor: None,
                agent: agent.to_string(),
                model: None,
                root: true,
                ts: 0,
                user: None,
            }),
        )
    }

    #[test]
    fn a_retired_session_started_agent_yields_the_fallback_note() {
        let records = [started("build")];
        assert_eq!(retired_agent(&records), Some("build"));
        assert_eq!(
            retired_agent_note(&records).as_deref(),
            Some("note: session used retired agent `build`; resuming as `general` (ADR-0207)")
        );
    }

    /// A legacy `AgentChanged` (a pre-ADR-0207 `SetAgent` switch) counts too.
    #[test]
    fn a_retired_agent_changed_record_is_found() {
        let sid = SessionId::new("s");
        let records = [
            started("general"),
            LogRecord::new(
                sid.clone(),
                LogPayload::Out(OutEvent::AgentChanged {
                    session: sid,
                    agent: "explore".to_string(),
                }),
            ),
        ];
        assert_eq!(retired_agent(&records), Some("explore"));
    }

    #[test]
    fn a_current_roster_name_needs_no_note() {
        assert_eq!(retired_agent_note(&[started("general")]), None);
    }
}
