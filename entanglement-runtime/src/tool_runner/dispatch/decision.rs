//! The approval park (issue #451, re-cut by ADR-0227): emit the
//! `ToolRequest`, wait for the head's decision (bounded by the mode's
//! `question_timeout`), record what the approval earned — an escape-root
//! grant, a per-host `http` approval, a wider-scope grant, a script's
//! per-run cache entry — and clear the call to run, or refuse.

use std::path::PathBuf;

use entanglement_core::{AgentState, ApprovalKind, ApprovalScope, CardReason, ToolOverlayEntry};

use crate::run_limits;
use crate::seam::Decision;

use super::gates::Cleared;
use super::grade::graded_name;
use super::{Call, Dispatcher, Via};

/// The card reason for a write the trusted-script floor carded (ADR-0228).
const FLOOR_NOTE: &str =
    "writes a trusted script or a skill's SKILL.md/lib.rhai — such a write always asks";

/// Why a call is parked — decides the card text and what approval records.
pub(super) enum Prompt {
    /// The mode graded it `Ask`.
    Grade,
    /// It reaches outside the project root (ADR-0109); `graded_ask` when the
    /// mode would have asked anyway, so a wider approval also earns the
    /// grade's grant (ADR-0228).
    Escape { abs: PathBuf, graded_ask: bool },
    /// A credentialed `http` call to a host not yet approved this session.
    Host(String),
}

/// The card body and its reason line for a parked call. A floored write
/// says so even when the escape-root warning is what carded it, so the user
/// knows a wider grant is not on offer.
fn card_text(
    prompt: &Prompt,
    body: &str,
    floored: bool,
    mode: &str,
    tool: &str,
) -> (String, String) {
    match prompt {
        Prompt::Escape { abs, .. } => {
            let abs = abs.display();
            let mut reason = format!("accesses a path outside the project root: {abs}");
            if floored {
                reason.push_str(&format!("; {FLOOR_NOTE}"));
            }
            (
                format!("{body}\n\n⚠ accesses a path OUTSIDE the project root: {abs}"),
                reason,
            )
        }
        Prompt::Host(host) => (
            format!("{body}\n\nauthenticated for {host}"),
            format!("authenticated for {host}"),
        ),
        Prompt::Grade if floored => (body.to_string(), FLOOR_NOTE.to_string()),
        Prompt::Grade => (
            body.to_string(),
            format!("mode `{mode}` grades `{tool}` Ask"),
        ),
    }
}

/// The structured reason a head decides on (ADR-0230). The floor wins over
/// the escape: it is what makes a wider approval worthless.
fn card_reason(prompt: &Prompt, floored: bool) -> Option<CardReason> {
    match prompt {
        _ if floored => Some(CardReason::FlooredWrite),
        Prompt::Escape { .. } => Some(CardReason::RootEscape),
        Prompt::Grade | Prompt::Host(_) => None,
    }
}

impl Dispatcher {
    pub(super) async fn park(
        &self,
        call: &Call,
        via: Via<'_>,
        arg: Option<String>,
        prompt: Prompt,
        overlay_entry: Option<ToolOverlayEntry>,
        // ADR-0228: a floored write always asks — a wider grant would never
        // be consulted, so none is recorded.
        floored: bool,
    ) -> Option<Cleared> {
        let body = &call.input;
        let (shown, reason) = card_text(&prompt, body, floored, &self.mode, &call.tool);
        // A script's nested call is carded as such; the script source rode
        // the outer `rhai` launch card.
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
            card_reason(&prompt, floored),
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
            return Some(Cleared::Refused(format!(
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
                // grant the user chose — as does an escape card that also
                // cleared a graded `Ask`, or the next run would ask again
                // for the grade alone (ADR-0228).
                let graded_ask = match &prompt {
                    Prompt::Escape { abs, graded_ask } => {
                        if let Some(er) = &self.escape_root {
                            er.store.record(&call.tool, abs, scope, &call.request_id);
                        }
                        *graded_ask
                    }
                    _ => true,
                };
                if graded_ask {
                    if let Prompt::Host(host) = &prompt {
                        self.http_gate.approve(&self.session, host);
                    }
                    if let Via::Script(sc) = via {
                        sc.approve(&call.tool, &call.input);
                    }
                    if scope != ApprovalScope::Once && !floored {
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
                Some(Cleared::Run(overlay_entry, None))
            }
            Decision::Reject { reason } => {
                set_state(AgentState::Thinking);
                Some(Cleared::Refused(format!(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_floored_escape_card_carries_both_reasons() {
        let escape = Prompt::Escape {
            abs: PathBuf::from("/elsewhere/s/x.rhai"),
            graded_ask: false,
        };
        let (shown, reason) = card_text(&escape, "{}", true, "build", "write");
        assert!(shown.contains("OUTSIDE the project root"), "{shown}");
        assert_eq!(
            reason,
            format!("accesses a path outside the project root: /elsewhere/s/x.rhai; {FLOOR_NOTE}")
        );
        let (_, plain) = card_text(&escape, "{}", false, "build", "write");
        assert!(!plain.contains(FLOOR_NOTE), "{plain}");
        let (_, graded) = card_text(&Prompt::Grade, "{}", true, "build", "write");
        assert_eq!(graded, FLOOR_NOTE);
        let (_, graded) = card_text(&Prompt::Grade, "{}", false, "build", "write");
        assert_eq!(graded, "mode `build` grades `write` Ask");
    }

    #[test]
    fn the_floor_outranks_the_escape_in_the_card_reason() {
        let escape = Prompt::Escape {
            abs: PathBuf::from("/elsewhere/x"),
            graded_ask: true,
        };
        assert_eq!(card_reason(&escape, true), Some(CardReason::FlooredWrite));
        assert_eq!(card_reason(&escape, false), Some(CardReason::RootEscape));
        assert_eq!(
            card_reason(&Prompt::Grade, true),
            Some(CardReason::FlooredWrite)
        );
        assert_eq!(card_reason(&Prompt::Grade, false), None);
        assert_eq!(card_reason(&Prompt::Host("h".into()), false), None);
    }
}
