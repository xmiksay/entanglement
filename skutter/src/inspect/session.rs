//! `skutter inspect session <id>` (#558).
//!
//! A session's live agent/model/name is session-scoped state that lives only
//! in its own event log — it cannot be reconstructed by re-running load-time
//! discovery the way `inspect prompt`/`agents`/`skills`/`config` do. This view
//! instead reads the persisted `.jsonl` log directly and folds the same
//! "last write wins" events
//! [`crate::session_store::SessionStore::list_sessions`] already folds for
//! `name` — no engine, no LLM, just the log.
//!
//! Only a **root** session id resolves: a spawned child's records live inside
//! its root's log file, not a file of their own
//! ([`crate::session_store::SessionStore::session_path`]).

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use entanglement_core::{OutEvent, SessionId};

use entanglement_runtime::session_store::{LogPayload, LogRecord};

/// Read `id`'s persisted log and print its resolved agent/model/name
/// (folded from the log's events, last write wins).
pub fn inspect_session(cwd: &Path, id: &str) -> Result<()> {
    let session_id = SessionId::new(id);
    let records = crate::user_dirs::session_store(cwd)
        .read(&session_id)
        .with_context(|| {
            format!(
                "no session log for `{id}` in this directory — run `skutter sessions` for root \
             session ids (a spawned child has no log file of its own)"
            )
        })?;
    print!("{}", render_session(&session_id, &records));
    Ok(())
}

/// State folded from `target`'s own records in a (possibly multi-session) log —
/// mirrors the "last write wins" scan [`session_store::SessionStore::list_sessions`] runs for
/// `name`, extended to agent/model.
#[derive(Default)]
struct FoldedState {
    started: bool,
    agent: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    name: Option<String>,
}

fn fold(target: &SessionId, records: &[LogRecord]) -> FoldedState {
    let mut state = FoldedState::default();
    for r in records {
        let LogPayload::Out(ev) = &r.payload else {
            continue;
        };
        match ev {
            OutEvent::SessionStarted {
                session,
                agent,
                model,
                ..
            } if session == target => {
                state.started = true;
                state.agent = Some(agent.clone());
                state.model = model.clone();
            }
            OutEvent::AgentChanged { session, agent, .. } if session == target => {
                state.agent = Some(agent.clone());
            }
            OutEvent::ModelChanged {
                session,
                provider,
                model,
                ..
            } if session == target => {
                state.provider = Some(provider.clone());
                state.model = Some(model.clone());
            }
            OutEvent::SessionMetaChanged { session, name, .. } if session == target => {
                state.name = name.clone();
            }
            _ => {}
        }
    }
    state
}

fn render_session(target: &SessionId, records: &[LogRecord]) -> String {
    let state = fold(target, records);
    let mut out = String::new();
    if !state.started {
        let _ = writeln!(
            out,
            "warning: no SessionStarted record for `{}` in this log — it may be a spawned \
             child, not a root session\n",
            target.0
        );
    }
    let _ = writeln!(out, "session: {}", target.0);
    let _ = writeln!(
        out,
        "agent:   {}",
        state.agent.as_deref().unwrap_or("(unknown)")
    );
    let model_line = match (&state.provider, &state.model) {
        (Some(p), Some(m)) => format!("{p}/{m}"),
        (None, Some(m)) => m.clone(),
        _ => "(provider default)".to_string(),
    };
    let _ = writeln!(out, "model:   {model_line}");
    let _ = writeln!(
        out,
        "name:    {}",
        state.name.as_deref().unwrap_or("(unset)")
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(session: &SessionId, ev: OutEvent) -> LogRecord {
        LogRecord {
            ts: 0,
            session: session.clone(),
            payload: LogPayload::Out(ev),
        }
    }

    #[test]
    fn folds_started_agent_model_name_last_write_wins() {
        let target = SessionId::new("root-1");
        let records = vec![
            out(
                &target,
                OutEvent::SessionStarted {
                    session: target.clone(),
                    parent: None,
                    predecessor: None,
                    agent: "build".to_string(),
                    model: Some("glm-4.5".to_string()),
                    root: true,
                    ts: 0,
                    user: None,
                },
            ),
            out(
                &target,
                OutEvent::AgentChanged {
                    session: target.clone(),
                    agent: "plan".to_string(),
                },
            ),
            out(
                &target,
                OutEvent::ModelChanged {
                    session: target.clone(),
                    provider: "zai".to_string(),
                    model: "glm-5.2".to_string(),
                    context_window: None,
                },
            ),
            out(
                &target,
                OutEvent::SessionMetaChanged {
                    session: target.clone(),
                    name: Some("my session".to_string()),
                    action: None,
                },
            ),
        ];

        let rendered = render_session(&target, &records);
        assert!(rendered.contains("agent:   plan"));
        assert!(rendered.contains("model:   zai/glm-5.2"));
        assert!(rendered.contains("name:    my session"));
        assert!(!rendered.contains("warning:"));
    }

    #[test]
    fn empty_log_reports_no_started_record() {
        let target = SessionId::new("orphan");
        let rendered = render_session(&target, &[]);
        assert!(rendered.contains("warning: no SessionStarted"));
        assert!(rendered.contains("agent:   (unknown)"));
    }

    #[test]
    fn a_sibling_sessions_records_are_ignored() {
        let target = SessionId::new("root-1");
        let sibling = SessionId::new("child-1");
        let records = vec![
            out(
                &target,
                OutEvent::SessionStarted {
                    session: target.clone(),
                    parent: None,
                    predecessor: None,
                    agent: "build".to_string(),
                    model: None,
                    root: true,
                    ts: 0,
                    user: None,
                },
            ),
            out(
                &sibling,
                OutEvent::SessionMetaChanged {
                    session: sibling.clone(),
                    name: Some("the child".to_string()),
                    action: None,
                },
            ),
        ];
        let rendered = render_session(&target, &records);
        assert!(rendered.contains("name:    (unset)"));
    }
}
