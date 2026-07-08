//! One-shot run mode: send a prompt and stream events until `Done`.
//!
//! Supports `--format json` (NDJSON events) or `--format text` (human-friendly).

use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use entanglement_core::{AgentState, Holly, InMsg, OutEvent, SessionId, TaskStatus};

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
        OutEvent::ToolOutput { output, .. } => writeln!(out, "= {output}")?,
        OutEvent::TaskList { tasks, .. } => {
            writeln!(out, "▢ tasks:")?;
            for t in tasks {
                writeln!(out, "  [{}] {}", task_symbol(t.status), t.content)?;
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

fn task_symbol(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Pending => "○",
        TaskStatus::InProgress => "▶",
        TaskStatus::Completed => "✓",
        TaskStatus::Cancelled => "✗",
    }
}
