//! One-shot run mode: send a prompt and stream events until `Done`.
//!
//! Supports `--format json` (NDJSON events) or `--format text` (human-friendly).

use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use entanglement_core::{AgentState, Holly, InMsg, OutEvent, SessionId};

/// Send one prompt and stream events until `Done` (or timeout).
pub async fn run_one(
    holly: &Holly,
    session: &SessionId,
    agent: Option<&str>,
    prompt: &str,
    format: &str,
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
        .send(InMsg::Prompt {
            session: session.clone(),
            text: prompt.to_string(),
        })
        .await?;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    loop {
        let ev = match tokio::time::timeout(Duration::from_secs(60), sub.recv()).await {
            Ok(Ok(ev)) => ev,
            Ok(Err(_)) => break,
            Err(_) => anyhow::bail!("timed out waiting for engine event"),
        };
        if ev.session() != session {
            continue;
        }
        if json {
            writeln!(out, "{}", serde_json::to_string(&ev)?)?;
        } else {
            render_text(&mut out, &ev)?;
        }
        out.flush()?;
        // No interactive user on the one-shot head: auto-answer an `ask_user`
        // prompt (first option, or a canned note when only free-form) so the
        // turn proceeds instead of parking forever (ADR-0027 fallback).
        if let OutEvent::UserQuestion {
            request_id,
            options,
            ..
        } = &ev
        {
            let answer = options
                .first()
                .map(|o| o.label.clone())
                .unwrap_or_else(|| "(no interactive user available)".to_string());
            holly
                .send(InMsg::AnswerQuestion {
                    session: session.clone(),
                    request_id: request_id.clone(),
                    answer,
                })
                .await?;
        }
        // `propose_plan` force-parks on approval (#141, ADR-0042); a one-shot head
        // has no interactive user to accept it, so auto-reject with a clear reason
        // (the plan agent learns the outcome in-band and can end its turn).
        if let OutEvent::ToolRequest {
            request_id, tool, ..
        } = &ev
        {
            if tool == crate::propose_plan::PROPOSE_PLAN_TOOL {
                holly
                    .send(InMsg::Reject {
                        session: session.clone(),
                        request_id: request_id.clone(),
                        reason: Some(
                            "non-interactive head cannot accept a plan; run interactively (tui) to accept".to_string(),
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

/// Human-friendly rendering of a single event.
fn render_text<W: Write>(out: &mut W, ev: &OutEvent) -> Result<()> {
    match ev {
        OutEvent::SessionStarted { .. } => {}
        OutEvent::SessionEnded { .. } => {}
        OutEvent::SessionList { .. } => {}
        OutEvent::Status { state, .. } => match state {
            AgentState::Thinking => writeln!(out, "… thinking")?,
            AgentState::WaitingApproval => writeln!(out, "… waiting for approval")?,
            AgentState::Error => writeln!(out, "! turn ended in error")?,
            _ => {}
        },
        OutEvent::AgentChanged { agent, .. } => writeln!(out, "# agent: {agent}")?,
        OutEvent::Plan { content, .. } => writeln!(out, "▸ plan:\n{content}")?,
        OutEvent::TextDelta { text, .. } => writeln!(out, "> {text}")?,
        OutEvent::ReasoningDelta { text, .. } => writeln!(out, "· {text}")?,
        OutEvent::ToolCall { tool, input, .. } => writeln!(out, "→ {tool}: {input}")?,
        OutEvent::ToolRequest { tool, input, .. } => writeln!(out, "? {tool}: {input}")?,
        OutEvent::UserQuestion {
            question, options, ..
        } => {
            writeln!(out, "? {question}")?;
            for opt in options {
                writeln!(out, "  - {}", opt.label)?;
            }
        }
        // Runtime plumbing (#58): execution round-trip, not user-facing.
        OutEvent::ToolExec { .. } => {}
        OutEvent::ToolOutput { output, .. } => writeln!(out, "= {output}")?,
        OutEvent::TaskList { content, .. } => {
            writeln!(out, "▢ tasks:")?;
            for line in content.lines() {
                writeln!(out, "  {line}")?;
            }
        }
        OutEvent::Error { message, .. } => writeln!(out, "! {message}")?,
        OutEvent::Done { .. } => writeln!(out, "✓ done")?,
        OutEvent::FileChange {
            path, change_kind, ..
        } => writeln!(out, "✓ {change_kind:?}: {path}")?,
    }
    Ok(())
}
