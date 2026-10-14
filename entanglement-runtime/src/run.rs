//! One-shot run mode: send a prompt and stream events until `Done`.
//!
//! Supports `--format json` (NDJSON events) or `--format text` (human-friendly).

use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use entanglement_core::{CardReason, Holly, InMsg, OutEvent, SessionId};
use tokio::sync::broadcast::error::RecvError;

pub(crate) mod render;
pub(crate) mod summary;

/// Send one prompt and stream events until `Done` (or timeout).
///
/// Follows a **compaction successor** (ADR-0205): when the context overflows,
/// the engine compacts this session into a fresh one and retires the original,
/// so the turn — and its `Done` — land on the successor. The `SessionStarted`
/// that announces it carries `predecessor`, which is how this loop learns to
/// keep listening there instead of waiting out the timeout on a session that
/// will never speak again.
///
/// A generic (non-`propose_plan`) `ToolRequest` reaching this head (e.g. the
/// escape-root gate forcing an approval prompt even under a mode's
/// `Allow`, ADR-0109) is auto-rejected with a reason the model can act on:
/// there is no interactive user here to answer it, and left unhandled it
/// would park until the 60s `recv` timeout below kills the whole run.
/// `--yes` (#554) used to offer an auto-approve escape hatch for this; it is
/// retired (ADR-0207 §11) — `--mode auto` is the real unattended posture now
/// (allow-but-deny-list posture per ADR-0228, run/turn/duration budgets,
/// no silent widening), so this
/// head no longer takes a flag to override its default.
///
/// `session` must already exist — resumed, or bound by the caller's own
/// `InMsg::Spawn` (ADR-0207 §9: an agent is chosen once, at spawn, so that
/// choice is the caller's job, not this loop's) — this only ever sends the
/// plain follow-up `Prompt` and drives the turn to `Done`.
pub async fn run_one(holly: &Holly, session: &SessionId, prompt: &str, format: &str) -> Result<()> {
    let json = format == "json";
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::prompt(session.clone(), prompt.to_string()))
        .await?;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    // Rebound when a compaction forks this session away (see the doc above).
    let mut session = session.clone();
    loop {
        let ev = match tokio::time::timeout(Duration::from_secs(60), sub.recv()).await {
            Ok(Ok(ev)) => ev,
            // A broadcast lag is a dropped-events gap, not end-of-stream: log and
            // keep relaying instead of silently killing the turn mid-conversation.
            Ok(Err(RecvError::Lagged(n))) => {
                tracing::warn!("run relay lagged, skipped {n} engine events");
                continue;
            }
            Ok(Err(RecvError::Closed)) => break,
            Err(_) => anyhow::bail!("timed out waiting for engine event"),
        };
        // Checked *before* the session filter: the announcement belongs to the
        // successor, so filtering first would drop the very event that says
        // where this run continues.
        if let OutEvent::SessionStarted {
            session: successor,
            predecessor: Some(source),
            ..
        } = &ev
        {
            if *source == session {
                tracing::debug!(%source, %successor, "run follows the compaction successor");
                session = successor.clone();
            }
        }
        // A session-log outage (#767) names no session, so it is printed
        // before the filter below — whatever the root: everything this
        // one-shot process logs belongs to this run.
        if let Some(line) = render::persistence_notice(&ev) {
            eprintln!("{line}");
            continue;
        }
        if !relays(&ev, &session) {
            continue;
        }
        if json {
            writeln!(out, "{}", serde_json::to_string(&ev)?)?;
        } else {
            render::render_text(&mut out, &ev)?;
        }
        out.flush()?;
        // No interactive user on the one-shot head: auto-answer every `ask_user`
        // question (its first option, or a canned note when it has none) so the
        // turn proceeds instead of parking forever (ADR-0027 fallback).
        if let OutEvent::UserQuestion {
            request_id,
            questions,
            ..
        } = &ev
        {
            let answers = questions
                .0
                .iter()
                .map(|q| {
                    vec![q
                        .options
                        .first()
                        .map(|o| o.label.clone())
                        .unwrap_or_else(|| "(no interactive user available)".to_string())]
                })
                .collect();
            holly
                .send(InMsg::answer_question(
                    session.clone(),
                    request_id.clone(),
                    answers,
                ))
                .await?;
        }
        // `propose_plan` force-parks on approval (#141, ADR-0042); a one-shot head
        // has no interactive user to accept it, so auto-reject with a clear reason
        // (the plan agent learns the outcome in-band and can end its turn) —
        // unconditionally, regardless of `auto_approve`: accepting a plan hands off
        // to a `build` child with its own review loop, which a headless run can't
        // drive either way.
        //
        // Any other `ToolRequest` (#554) — most commonly the escape-root gate
        // forcing a prompt for an out-of-root path even under an `Allow` profile —
        // has no such special handling and would otherwise park until this loop's
        // 60s `recv` timeout kills the whole run. Settle it immediately with a
        // reason the model can act on (retry inside the root, run `--mode auto`
        // for an unattended posture, or rerun interactively via `tui`) instead.
        //
        // The one exception is an in-root `rhai` launch card (ADR-0228, told
        // by its `reason`, ADR-0230): with nobody to
        // pick **Run all**, the script starts at once **Ask per command** —
        // each nested call is still graded, and a nested `Ask` lands here as
        // its own `<tool> (rhai)` card and is refused like any other.
        if let OutEvent::ToolRequest {
            request_id,
            tool,
            reason,
            ..
        } = &ev
        {
            let answer = headless_answer(&session, request_id, tool, *reason);
            holly.send(answer).await?;
        }
        if matches!(ev, OutEvent::Done { .. }) {
            break;
        }
    }
    Ok(())
}

/// This head's answer to a `ToolRequest`: a `rhai` launch card starts the
/// script **Ask per command** (a `Once` approval, never `run_all`) — unless
/// the script lies outside the project root, which no one approved; every
/// other card — a nested `<tool> (rhai)` one included — is rejected. Decided
/// on the card's structured `reason` (ADR-0230), never its text.
fn headless_answer(
    session: &SessionId,
    request_id: &str,
    tool: &str,
    reason: Option<CardReason>,
) -> InMsg {
    let (session, request_id) = (session.clone(), request_id.to_string());
    if entanglement_runtime::pending::headless_starts(reason) {
        InMsg::Approve {
            session,
            request_id,
            scope: entanglement_core::ApprovalScope::Once,
            mode: None,
            run_all: false,
        }
    } else {
        InMsg::Reject {
            session,
            request_id,
            reason: Some(reject_reason(tool).to_string()),
        }
    }
}

/// Whether the relay loop prints `ev` for the run bound to `session`: its own
/// events, plus an endpoint `Throttle` transition (#794) — engine-global
/// (`session()` is `None`, the pool is per-endpoint, ADR-0141), yet it is the
/// only signal telling a headless run why its turn has gone quiet. It rides
/// the normal output (a text line, or an NDJSON frame under `--format json`)
/// like `Status`, unlike a persistence outage, which is an operational fault
/// and goes to stderr.
fn relays(ev: &OutEvent, session: &SessionId) -> bool {
    matches!(ev, OutEvent::Throttle { .. }) || ev.session() == Some(session)
}

/// The auto-reject reason for a `ToolRequest` this one-shot head has no
/// interactive user to answer. `propose_plan` gets its own message (accepting
/// a plan is a mode switch this head can't observe or drive either way);
/// every other tool gets the generic pointer to `--mode auto`, a
/// `rhai.trusted` folder (a script there runs with every nested call
/// allowed, ADR-0228) or `tui`.
///
/// Deliberately takes no mode/flag input (#560, ADR-0207 §7 extension): this
/// head auto-rejects a plan **unconditionally**, `--mode auto` included — an
/// unattended run must never silently start auto-*accepting* its own plans,
/// only running the *tools* `--mode auto`'s bounded posture already allows.
fn reject_reason(tool: &str) -> &'static str {
    if tool == crate::tool_names::PROPOSE_PLAN_TOOL {
        "non-interactive head cannot accept a plan; run interactively (tui) to accept"
    } else {
        "non-interactive head auto-rejects tool approval requests; \
         rerun with --mode auto for an unattended run, put a script in a `rhai.trusted` \
         folder (config.yml) to run it unasked, or run interactively (tui) to decide"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn propose_plan_gets_its_own_reason_regardless_of_any_mode() {
        // No `mode`/`--yes`-style parameter exists on this function at all —
        // proof by signature that the reject is unconditional, not gated on
        // `--mode auto` (#560).
        let reason = reject_reason(crate::tool_names::PROPOSE_PLAN_TOOL);
        assert!(reason.contains("cannot accept a plan"), "{reason}");
        assert!(!reason.contains("--mode auto"), "{reason}");
    }

    /// ADR-0228: the generic reason names `rhai.trusted` beside `--mode
    /// auto` — a trusted script runs here with no card at all.
    #[test]
    fn the_generic_reason_names_the_unattended_ways_out() {
        let reason = reject_reason("bash");
        assert!(
            reason.contains("--mode auto") && reason.contains("rhai.trusted"),
            "{reason}"
        );
    }

    /// ADR-0228/ADR-0230: a launch card starts the script Ask per command;
    /// a nested card, an out-of-root launch and a card with no launch
    /// reason are refused — whatever the card text says.
    #[test]
    fn a_rhai_launch_runs_ask_per_command_and_a_nested_card_is_rejected() {
        let sid = SessionId::new("s");
        for reason in [CardReason::ScriptLaunch, CardReason::InlineLaunch] {
            match headless_answer(&sid, "r1", "rhai", Some(reason)) {
                InMsg::Approve { scope, run_all, .. } => {
                    assert_eq!(scope, entanglement_core::ApprovalScope::Once);
                    assert!(!run_all);
                }
                other => panic!("{other:?}"),
            }
        }
        for (tool, reason) in [
            ("write (rhai)", None),
            ("write (rhai)", Some(CardReason::FlooredWrite)),
            ("bash", Some(CardReason::RootEscape)),
            ("rhai", None),
            ("rhai", Some(CardReason::LaunchEscape)),
        ] {
            assert!(
                matches!(
                    headless_answer(&sid, "r2", tool, reason),
                    InMsg::Reject { .. }
                ),
                "{tool} {reason:?}"
            );
        }
    }

    #[test]
    fn throttle_is_relayed_though_it_names_no_session() {
        let mine = SessionId::new("s-1");
        let throttle = OutEvent::Throttle {
            endpoint: "https://api.example/v1".into(),
            throttled: true,
            in_flight: 1,
            cap: 1,
            retry_in_ms: Some(2000),
            pacing_in_ms: None,
            waiters: 0,
            shared_leases: None,
        };
        assert_eq!(throttle.session(), None);
        assert!(relays(&throttle, &mine));
        let other = OutEvent::Status {
            session: SessionId::new("s-2"),
            state: entanglement_core::AgentState::Thinking,
        };
        assert!(!relays(&other, &mine), "another session stays filtered");
    }

    #[test]
    fn every_other_tool_gets_the_generic_unattended_pointer() {
        let reason = reject_reason("bash");
        assert!(reason.contains("--mode auto"), "{reason}");
        assert_ne!(reason, reject_reason(crate::tool_names::PROPOSE_PLAN_TOOL));
    }
}
