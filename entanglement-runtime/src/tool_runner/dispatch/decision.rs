//! The approval park (issue #451, re-cut by ADR-0227): emit the
//! `ToolRequest`, wait for the head's decision (bounded by the mode's
//! `question_timeout`), record what the approval earned — an escape-root
//! grant, a per-host `http` approval, a wider-scope grant, a script's
//! per-run cache entry — and run, or refuse.

use std::path::PathBuf;

use entanglement_core::{AgentState, ApprovalKind, ApprovalScope, ToolOverlayEntry};

use crate::run_limits;
use crate::seam::Decision;
use crate::tools::ToolRegistry;

use super::grade::graded_name;
use super::{Call, Dispatcher, Outcome, Via};

/// Why a call is parked — decides the card text and what approval records.
pub(super) enum Prompt {
    /// The mode graded it `Ask`.
    Grade,
    /// It reaches outside the project root (ADR-0109).
    Escape(PathBuf),
    /// A credentialed `http` call to a host not yet approved this session.
    Host(String),
}

impl Dispatcher {
    pub(super) async fn park(
        &self,
        tools: &ToolRegistry,
        call: Call,
        via: Via<'_>,
        arg: Option<String>,
        prompt: Prompt,
        overlay_entry: Option<ToolOverlayEntry>,
    ) -> Option<Outcome> {
        let (shown, reason) = match &prompt {
            Prompt::Escape(abs) => {
                let abs = abs.display();
                (
                    format!(
                        "{}\n\n⚠ accesses a path OUTSIDE the project root: {abs}",
                        call.input
                    ),
                    format!("accesses a path outside the project root: {abs}"),
                )
            }
            Prompt::Host(host) => (
                format!("{}\n\nauthenticated for {host}", call.input),
                format!("authenticated for {host}"),
            ),
            Prompt::Grade => (
                call.input.clone(),
                format!("mode `{}` grades `{}` Ask", self.mode, call.tool),
            ),
        };
        // A script's nested call is carded as such; the script source rode
        // the outer `rhai` approval.
        let (kind, card_tool, detached) = match via {
            Via::Model => (ApprovalKind::Tool, call.tool.clone(), false),
            Via::Script(sc) => (
                ApprovalKind::Rhai,
                format!("{} (rhai)", call.tool),
                sc.detached,
            ),
        };
        let req = crate::pending::approval(
            &self.session,
            &call.request_id,
            kind,
            &card_tool,
            shown,
            reason,
        );
        let rx = crate::pending::park_approval(&self.holly, &self.pending, req, card_tool);
        // A background script's Ask arrives outside any live turn (#637):
        // flipping the session's state would strand a stale status.
        let set_state = |state| {
            if !detached {
                self.holly.emit_status(&self.session, state);
            }
        };
        set_state(AgentState::WaitingApproval);
        let timeout = run_limits::timeout(&self.limits);
        let decision = self
            .pending
            .await_timed(&self.session, &call.request_id, rx, timeout)
            .await;
        let Some(decision) = decision else {
            // Silence is never consent (ADR-0207 §11).
            set_state(AgentState::Thinking);
            return Some(Outcome::Refused(format!(
                "tool `{}` denied: no response within {}s (mode `{}`)",
                call.tool,
                timeout.map(|d| d.as_secs()).unwrap_or_default(),
                self.mode
            )));
        };
        match decision {
            Decision::Approve { scope, .. } => {
                set_state(AgentState::Thinking);
                // Every escape scope is recorded: a `Once` is the single-use
                // token bound to this exact request id (#449). A host
                // approval is the per-session unlock and, like a plain grade
                // approval, also earns the script cache entry and any wider
                // grant the user chose.
                if let Prompt::Escape(abs) = &prompt {
                    if let Some(er) = &self.escape_root {
                        er.store.record(&call.tool, abs, scope, &call.request_id);
                    }
                } else {
                    if let Prompt::Host(host) = &prompt {
                        self.http_gate.approve(&self.session, host);
                    }
                    if let Via::Script(sc) = via {
                        sc.approve(&call.tool, &call.input);
                    }
                    if scope != ApprovalScope::Once {
                        self.grants
                            .record(
                                &self.session,
                                graded_name(&call.tool),
                                arg.as_deref(),
                                scope,
                                &self.mode,
                            )
                            .await;
                    }
                }
                Some(self.execute(tools, call, via, overlay_entry).await)
            }
            Decision::Reject { reason } => {
                set_state(AgentState::Thinking);
                Some(Outcome::Refused(format!(
                    "tool `{}` rejected: {}",
                    call.tool,
                    reason.as_deref().unwrap_or("user")
                )))
            }
            // `Stop` (and a closed inbox) unwind; `Answer`/`Retract`/
            // `Replace` never target an approval id (`ask_user`-only, #515).
            Decision::Stop
            | Decision::Answer { .. }
            | Decision::Retract
            | Decision::Replace { .. } => None,
        }
    }
}
