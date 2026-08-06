//! `skutter inspect session <id>` (#558).
//!
//! Closes the "session tool overlay" blind spot: `InMsg::SetToolOverlay`
//! (ADR-0149) is session-scoped state that lives only in that session's own
//! event log (`Session.tool_overlay`, folded on resume) — there is no managed
//! file for it, and it cannot be reconstructed by re-running load-time
//! discovery the way `inspect prompt`/`agents`/`skills`/`config` do. This view
//! instead reads the persisted `.jsonl` log directly and folds the same
//! "last write wins" events [`crate::session_store::list_sessions`] already
//! folds for `name` — no engine, no LLM, just the log.
//!
//! Only a **root** session id resolves: a spawned child's records live inside
//! its root's log file, not a file of their own ([`crate::session_store::session_path`]),
//! so its tool overlay is visible by inspecting the root and reading the
//! child's own `ToolOverlayChanged` records within it — a future refinement,
//! not needed by #558's scope.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use entanglement_core::{OutEvent, SessionId, ToolOverlayEntry};

use crate::session_store::{self, LogPayload, LogRecord};

/// Read `id`'s persisted log and print its resolved agent/model/name plus its
/// live tool overlay (folded from the log's events, last write wins).
pub fn inspect_session(cwd: &Path, id: &str) -> Result<()> {
    let session_id = SessionId::new(id);
    let records = session_store::read(cwd, &session_id).with_context(|| {
        format!(
            "no session log for `{id}` in this directory — run `skutter sessions` for root \
             session ids (a spawned child has no log file of its own)"
        )
    })?;
    print!("{}", render_session(&session_id, &records));
    Ok(())
}

/// State folded from `target`'s own records in a (possibly multi-session) log —
/// mirrors the "last write wins" scan [`session_store::list_sessions`] runs for
/// `name`, extended to agent/model/tool-overlay.
#[derive(Default)]
struct FoldedState {
    started: bool,
    agent: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    name: Option<String>,
    tool_overlay: Vec<ToolOverlayEntry>,
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
                profile,
                model,
                ..
            } if session == target => {
                state.started = true;
                state.agent = Some(profile.clone());
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
            OutEvent::ToolOverlayChanged { session, entries } if session == target => {
                state.tool_overlay = entries.clone();
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

    let _ = writeln!(
        out,
        "\ntool overlay (ADR-0149 — live, per-session mask override past the agent profile):"
    );
    if state.tool_overlay.is_empty() {
        let _ = writeln!(out, "  (none — using the agent profile's own tool mask)");
    } else {
        for entry in &state.tool_overlay {
            let disposition = if entry.deny {
                "deny"
            } else if entry.allow {
                "allow (no approval prompt)"
            } else {
                "ask (bypasses the agent mask, still prompts)"
            };
            let _ = writeln!(out, "  {}: {disposition}", entry.pattern);
        }
    }
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
    fn folds_started_agent_model_name_and_overlay_last_write_wins() {
        let target = SessionId::new("root-1");
        let records = vec![
            out(
                &target,
                OutEvent::SessionStarted {
                    session: target.clone(),
                    parent: None,
                    predecessor: None,
                    profile: "build".to_string(),
                    model: Some("glm-4.5".to_string()),
                    root: true,
                    ts: 0,
                    user: None,
                    sponsored: false,
                },
            ),
            out(
                &target,
                OutEvent::AgentChanged {
                    session: target.clone(),
                    agent: "plan".to_string(),
                    profile_detail: None,
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
            out(
                &target,
                OutEvent::ToolOverlayChanged {
                    session: target.clone(),
                    entries: vec![ToolOverlayEntry::deny("bash")],
                },
            ),
        ];

        let rendered = render_session(&target, &records);
        assert!(rendered.contains("agent:   plan"));
        assert!(rendered.contains("model:   zai/glm-5.2"));
        assert!(rendered.contains("name:    my session"));
        assert!(rendered.contains("bash: deny"));
        assert!(!rendered.contains("warning:"));
    }

    #[test]
    fn empty_log_reports_no_started_record_and_an_empty_overlay() {
        let target = SessionId::new("orphan");
        let rendered = render_session(&target, &[]);
        assert!(rendered.contains("warning: no SessionStarted"));
        assert!(rendered.contains("agent:   (unknown)"));
        assert!(rendered.contains("(none — using the agent profile's own tool mask)"));
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
                    profile: "build".to_string(),
                    model: None,
                    root: true,
                    ts: 0,
                    user: None,
                    sponsored: false,
                },
            ),
            out(
                &sibling,
                OutEvent::ToolOverlayChanged {
                    session: sibling.clone(),
                    entries: vec![ToolOverlayEntry::ask("mcp__docs__*")],
                },
            ),
        ];
        let rendered = render_session(&target, &records);
        assert!(rendered.contains("(none — using the agent profile's own tool mask)"));
    }
}
