//! One-shot run mode: send a prompt and stream events until `Done`.
//!
//! Supports `--format json` (NDJSON events) or `--format text` (human-friendly).

use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use entanglement_core::{Holly, InMsg, OutEvent, SessionId};
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
/// `auto_approve` (`--yes`, #554) controls how a generic (non-`propose_plan`)
/// `ToolRequest` is settled: there is no interactive user to answer it, and
/// left unhandled it parks until the 60s `recv` timeout below kills the whole
/// run — reachable on stock defaults because the escape-root gate forces an
/// approval prompt even under a profile's `Allow` (ADR-0109). `false` (the
/// default) auto-rejects with a reason the model can act on instead of
/// silently dying; `true` auto-approves so a trusted unattended run proceeds.
pub async fn run_one(
    holly: &Holly,
    session: &SessionId,
    agent: Option<&str>,
    prompt: &str,
    format: &str,
    auto_approve: bool,
) -> Result<()> {
    let json = format == "json";
    let mut sub = holly.subscribe();

    if let Some(a) = agent {
        holly
            .send(InMsg::SetAgent {
                session: session.clone(),
                agent: a.to_string(),
            })
            .await?;
    }
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
        if ev.session() != Some(&session) {
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
        // 60s `recv` timeout kills the whole run. Settle it immediately instead:
        // `--yes` approves once, the default rejects with a reason the model can
        // act on (retry inside the root, or ask the user to rerun interactively).
        if let OutEvent::ToolRequest {
            request_id, tool, ..
        } = &ev
        {
            if tool == crate::tool_names::PROPOSE_PLAN_TOOL {
                holly
                    .send(InMsg::Reject {
                        session: session.clone(),
                        request_id: request_id.clone(),
                        reason: Some(
                            "non-interactive head cannot accept a plan; run interactively (tui) to accept".to_string(),
                        ),
                    })
                    .await?;
            } else if auto_approve {
                holly
                    .send(InMsg::Approve {
                        session: session.clone(),
                        request_id: request_id.clone(),
                        scope: Default::default(),
                    })
                    .await?;
            } else {
                holly
                    .send(InMsg::Reject {
                        session: session.clone(),
                        request_id: request_id.clone(),
                        reason: Some(
                            "non-interactive head auto-rejects tool approval requests by default; \
                             rerun with --yes to auto-approve, or interactively (tui) to decide"
                                .to_string(),
                        ),
                    })
                    .await?;
            }
        }
        if matches!(ev, OutEvent::Done { .. }) {
            break;
        }
    }
    Ok(())
}
