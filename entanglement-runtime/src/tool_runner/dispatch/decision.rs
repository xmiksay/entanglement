//! The approval park (issue #451, re-cut by ADR-0227): emit the
//! `ToolRequest`, wait for the head's decision (bounded by the mode's
//! `question_timeout` unless a head watches the session, ADR-0247), record
//! what the approval earned — a per-host `http` approval, a wider-scope
//! grant, a script's per-run cache entry — and clear the call to run, or
//! refuse.

use entanglement_core::{AgentState, ApprovalKind, ApprovalScope, CardReason};

use crate::policy::AskReason;
use crate::run_limits;
use crate::seam::ApprovalDecision;

use super::gates::Cleared;
use super::grade::{graded_name, grant_key};
use super::{Call, Dispatcher, Via};

/// The card reason for a write the trusted-script floor carded (ADR-0228).
const FLOOR_NOTE: &str =
    "writes a trusted script or a skill's SKILL.md/lib.rhai — such a write always asks";

/// Why a call is parked — decides the card text and what approval records.
pub(super) enum Prompt {
    /// The resolver graded it `Ask` — with its reason when it gave one
    /// (#968: a path outside the project root).
    Grade(Option<AskReason>),
    /// A credentialed `http` call to a host not yet approved this session.
    Host(String),
}

/// The card body and its reason line for a parked call. A floored write
/// says so even when a resolver's reason is what carded it, so the user
/// knows a wider grant is not on offer.
fn card_text(
    prompt: &Prompt,
    body: &str,
    floored: bool,
    mode: &str,
    tool: &str,
) -> (String, String) {
    match prompt {
        Prompt::Grade(Some(why)) => {
            let mut reason = why.text.clone();
            if floored {
                reason.push_str(&format!("; {FLOOR_NOTE}"));
            }
            (format!("{body}\n\n⚠ {}", why.text), reason)
        }
        Prompt::Host(host) => (
            format!("{body}\n\nauthenticated for {host}"),
            format!("authenticated for {host}"),
        ),
        Prompt::Grade(None) if floored => (body.to_string(), FLOOR_NOTE.to_string()),
        Prompt::Grade(None) => (
            body.to_string(),
            format!("mode `{mode}` grades `{tool}` Ask"),
        ),
    }
}

/// The structured reason a head decides on (ADR-0230). The floor wins over
/// a resolver's reason: it is what makes a wider approval worthless.
fn card_reason(prompt: &Prompt, floored: bool) -> Option<CardReason> {
    match prompt {
        _ if floored => Some(CardReason::FlooredWrite),
        Prompt::Grade(Some(why)) => Some(why.card),
        Prompt::Grade(None) | Prompt::Host(_) => None,
    }
}

impl Dispatcher {
    pub(super) async fn park(
        &self,
        call: &Call,
        via: Via<'_>,
        arg: Option<String>,
        prompt: Prompt,
        // ADR-0228: a floored write always asks — a wider grant would never
        // be consulted, so none is recorded.
        floored: bool,
    ) -> Option<Cleared> {
        let body = &call.input;
        // The card names what was graded (ADR-0249): an envelope tool's
        // target, never the envelope, so the approver sees what an `Always`
        // grant will be recorded under.
        let graded = graded_name(&self.tools, &call.tool, &call.input);
        let (shown, reason) = card_text(&prompt, body, floored, &self.mode, &graded);
        // A script's nested call is carded as such; the script source rode
        // the outer `rhai` launch card.
        let (kind, card_tool, detached) = match via {
            Via::Model => (ApprovalKind::Tool, graded.clone(), false),
            Via::Script(sc) => (ApprovalKind::Rhai, format!("{graded} (rhai)"), sc.detached),
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
        let rx =
            crate::pending::park_approval_as(&self.holly, &self.pending, req, card_tool, detached);
        // A background script's Ask arrives outside any live turn (#637):
        // flipping the session's state would strand a stale status.
        let set_state = |state| {
            if !detached {
                self.holly.emit_status(&self.session, state);
            }
        };
        set_state(AgentState::WaitingApproval);
        let expiry = run_limits::expiry(
            &self.limits,
            &self.attendance,
            &self.chain,
            &call.request_id,
        );
        let secs = expiry.secs();
        let decision = self
            .pending
            .await_timed(&self.session, &call.request_id, rx, expiry)
            .await;
        let Some(decision) = decision else {
            // Silence is never consent (ADR-0207 §11).
            set_state(AgentState::Thinking);
            return Some(Cleared::Refused(format!(
                "tool `{}` denied: no response within {secs}s (mode `{}`)",
                call.tool, self.mode
            )));
        };
        match decision.into_approval() {
            ApprovalDecision::Approve { scope, .. } => {
                set_state(AgentState::Thinking);
                // A host approval is the per-session unlock and, like a
                // plain grade approval, also earns the script cache entry
                // and any wider grant the user chose (ADR-0228).
                let earner = self.earner(via);
                if let Prompt::Host(host) = &prompt {
                    self.http_gate.approve(&earner, host);
                }
                if let Via::Script(sc) = via {
                    sc.approve(&call.tool, &call.input);
                }
                if scope != ApprovalScope::Once && !floored {
                    let why = match &prompt {
                        Prompt::Grade(why) => why.as_ref(),
                        Prompt::Host(_) => None,
                    };
                    let (tool, arg) = grant_key(why, &graded, arg.as_deref());
                    self.grants
                        .record(&earner, tool, arg, scope, &self.mode)
                        .await;
                }
                Some(Cleared::Run)
            }
            ApprovalDecision::Reject { reason } => {
                set_state(AgentState::Thinking);
                Some(Cleared::Refused(format!(
                    "tool `{}` rejected: {}",
                    call.tool,
                    reason.as_deref().unwrap_or("user")
                )))
            }
            // `Stop` (and a closed inbox) unwind.
            ApprovalDecision::Stop => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn escape() -> Prompt {
        Prompt::Grade(Some(AskReason {
            text: "accesses a path outside the project root: /elsewhere/s/x.rhai".into(),
            card: CardReason::RootEscape,
            grant: None,
        }))
    }

    #[test]
    fn a_floored_reasoned_card_carries_both_reasons() {
        let (shown, reason) = card_text(&escape(), "{}", true, "build", "write");
        assert!(shown.contains("⚠ accesses a path outside"), "{shown}");
        assert_eq!(
            reason,
            format!("accesses a path outside the project root: /elsewhere/s/x.rhai; {FLOOR_NOTE}")
        );
        let (_, plain) = card_text(&escape(), "{}", false, "build", "write");
        assert!(!plain.contains(FLOOR_NOTE), "{plain}");
        let (_, graded) = card_text(&Prompt::Grade(None), "{}", true, "build", "write");
        assert_eq!(graded, FLOOR_NOTE);
        let (_, graded) = card_text(&Prompt::Grade(None), "{}", false, "build", "write");
        assert_eq!(graded, "mode `build` grades `write` Ask");
    }

    #[test]
    fn the_floor_outranks_a_resolver_reason_in_the_card_reason() {
        assert_eq!(card_reason(&escape(), true), Some(CardReason::FlooredWrite));
        assert_eq!(card_reason(&escape(), false), Some(CardReason::RootEscape));
        assert_eq!(
            card_reason(&Prompt::Grade(None), true),
            Some(CardReason::FlooredWrite)
        );
        assert_eq!(card_reason(&Prompt::Grade(None), false), None);
        assert_eq!(card_reason(&Prompt::Host("h".into()), false), None);
    }
}
