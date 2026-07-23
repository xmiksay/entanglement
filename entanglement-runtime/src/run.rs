//! One-shot run mode: send a prompt and stream events until `Done`.
//!
//! Supports `--format json` (NDJSON events) or `--format text` (human-friendly).

use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use entanglement_core::{AgentState, Holly, InMsg, OutEvent, SessionId};
use tokio::sync::broadcast::error::RecvError;

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
        .send(InMsg::prompt(session.clone(), prompt.to_string()))
        .await?;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
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
        if ev.session() != Some(session) {
            continue;
        }
        if json {
            writeln!(out, "{}", serde_json::to_string(&ev)?)?;
        } else {
            render_text(&mut out, &ev)?;
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
        // (the plan agent learns the outcome in-band and can end its turn).
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
        // Memory eviction (#318); the one-shot head never hibernates, so nothing
        // to render.
        OutEvent::SessionHibernated { .. } => {}
        OutEvent::SessionList { .. } => {}
        // MCP ops (#375) and the bash-live ops (#498) are engine-global
        // queries/commands; the one-shot head never issues them, so nothing to
        // render.
        OutEvent::McpList { .. } => {}
        OutEvent::McpChanged { .. } => {}
        OutEvent::BashChanged { .. } => {}
        // History is a late-subscriber query reply (#160); the one-shot head
        // never issues `ReplayFrom`, so nothing to render.
        OutEvent::History { .. } => {}
        OutEvent::Status { state, .. } => match state {
            AgentState::Thinking => writeln!(out, "… thinking")?,
            AgentState::WaitingApproval => writeln!(out, "… waiting for approval")?,
            AgentState::WaitingAnswer => writeln!(out, "… waiting for answer")?,
            AgentState::Error => writeln!(out, "! turn ended in error")?,
            _ => {}
        },
        OutEvent::AgentChanged { agent, .. } => writeln!(out, "# agent: {agent}")?,
        OutEvent::ModelChanged {
            provider, model, ..
        } => writeln!(out, "# model: {provider}/{model}")?,
        OutEvent::GenerationChanged { generation, .. } => {
            writeln!(out, "# generation: {generation:?}")?
        }
        OutEvent::Plan { content, .. } => writeln!(out, "▸ plan:\n{content}")?,
        OutEvent::TextDelta { text, .. } => writeln!(out, "> {text}")?,
        OutEvent::ReasoningDelta { text, .. } => writeln!(out, "· {text}")?,
        // Streaming tool-arg fragment (#194): the batch renderer prints the whole
        // call on `ToolCall`, so the per-fragment delta is display-only noise here.
        OutEvent::ToolCallDelta { .. } => {}
        OutEvent::ToolCall { tool, input, .. } => writeln!(out, "→ {tool}: {input}")?,
        OutEvent::ToolRequest { tool, input, .. } => writeln!(out, "? {tool}: {input}")?,
        OutEvent::UserQuestion { questions, .. } => {
            for q in &questions.0 {
                writeln!(out, "? {}", q.question)?;
                for opt in &q.options {
                    writeln!(out, "  - {}", opt.label)?;
                }
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
        OutEvent::Usage {
            input_tokens,
            output_tokens,
            cost_usd,
            ..
        } => match cost_usd {
            Some(cost) => writeln!(
                out,
                "$ usage: {input_tokens} in / {output_tokens} out (${cost:.4})"
            )?,
            None => writeln!(out, "$ usage: {input_tokens} in / {output_tokens} out")?,
        },
        OutEvent::Error { message, .. } => writeln!(out, "! {message}")?,
        OutEvent::Done { .. } => writeln!(out, "✓ done")?,
        OutEvent::Compacted { summary, auto, .. } => {
            if *auto {
                writeln!(
                    out,
                    "▸ auto-compacted: context overflowed the model's window, \
                     summarized in place to keep the turn going:\n{summary}"
                )?
            } else {
                writeln!(
                    out,
                    "▸ compacted: summary ready — fork into a new session to continue \
                     from it (the original is preserved):\n{summary}"
                )?
            }
        }
        OutEvent::FileChange {
            path, change_kind, ..
        } => writeln!(out, "✓ {change_kind:?}: {path}")?,
        // Skill-scoped tool mask posture (#400, ADR-0106): a wire-facing audit
        // event for a head to render, not required for the one-shot text render.
        OutEvent::SkillActive { skill_id, .. } => match skill_id {
            Some(id) => writeln!(out, "◆ skill active: {id}")?,
            None => writeln!(out, "◆ skill cleared")?,
        },
        // Ambiguous-stop bounded retry (#ADR-0118): the model's stream ended
        // without a confident finish signal, so the turn is retrying in place.
        // Render a one-line notice; its non-delta arrival also flushes the
        // preceding partial `TextDelta` line so the retry's text stays separate.
        OutEvent::AmbiguousRetry { .. } => writeln!(out, "↻ model stop was ambiguous — retrying")?,
        // Persisted provider-side web-search block (#481): already rendered
        // live via `ReasoningDelta`'s query/source lines — nothing new to show.
        OutEvent::SearchResult { .. } => {}
    }
    Ok(())
}
