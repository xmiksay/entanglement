//! Human-friendly rendering of one `OutEvent` for the text-format one-shot
//! head. Split out of `run.rs` (400-line cap): the relay loop and the
//! per-variant rendering are independent concerns, and only this half grows
//! as the protocol gains variants.

use std::io::Write;

use anyhow::Result;
use entanglement_core::{AgentState, CompactionMode, OutEvent};

use super::summary;

/// Human-friendly rendering of a single event.
pub(crate) fn render_text<W: Write>(out: &mut W, ev: &OutEvent) -> Result<()> {
    match ev {
        OutEvent::SessionStarted { .. } => {}
        OutEvent::SessionEnded { .. } => {}
        // Memory eviction (#318); the one-shot head never hibernates, so nothing
        // to render.
        OutEvent::SessionHibernated { .. } => {}
        OutEvent::SessionList { .. } => {}
        // `ListQuestions` reply (#515): a session-less snapshot query; the
        // one-shot head never issues it, so nothing to render.
        OutEvent::QuestionList { .. } => {}
        // `ListOperations` reply (#607, ADR-0161 §6): same shape, and the
        // one-shot head never issues it either.
        OutEvent::OperationList { .. } => {}
        // MCP ops (#375) are engine-global queries/commands; the one-shot
        // head never issues them, so nothing to render.
        OutEvent::McpList { .. } => {}
        OutEvent::McpChanged { .. } => {}
        // MCP OAuth progress (ADR-0153). The authorize URL is always printed —
        // a one-shot/headless run may have no browser to open, and the URL is
        // the only way to complete the flow there.
        OutEvent::McpAuthChanged { status } => {
            if let Some(url) = &status.authorize_url {
                match &status.user_code {
                    Some(code) => writeln!(
                        out,
                        "→ {} · open {url} and enter code {code} to authorize",
                        status.name
                    )?,
                    None => writeln!(out, "→ {} · open to authorize: {url}", status.name)?,
                }
            } else if let Some(err) = &status.error {
                writeln!(out, "✗ {} · {err}", status.name)?
            } else if let Some(state) = &status.state {
                writeln!(out, "✓ {} · {state}", status.name)?
            }
        }
        // History is a late-subscriber query reply (#160); the one-shot head
        // never issues `ReplayFrom`, so nothing to render.
        OutEvent::History { .. } => {}
        OutEvent::Status { state, .. } => match state {
            AgentState::Thinking => writeln!(out, "… thinking")?,
            AgentState::Working => writeln!(out, "… working")?,
            AgentState::WaitingAgent => writeln!(out, "… waiting for sub-agent")?,
            AgentState::WaitingApproval => writeln!(out, "… waiting for approval")?,
            AgentState::WaitingAnswer => writeln!(out, "… waiting for answer")?,
            AgentState::Paused => writeln!(out, "‖ paused")?,
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
        OutEvent::SessionMetaChanged { name, action, .. } => writeln!(
            out,
            "# session: name={} action={}",
            name.as_deref().unwrap_or("-"),
            action.as_deref().unwrap_or("-")
        )?,
        // Live tool overlay (#539): render the effective pattern list so a
        // one-shot run driven by an embedder shows what was injected.
        OutEvent::ToolOverlayChanged { entries, .. } => {
            let list: Vec<String> = entries
                .iter()
                .map(|e| {
                    if e.allow {
                        format!("{} (allow)", e.pattern)
                    } else {
                        e.pattern.clone()
                    }
                })
                .collect();
            writeln!(out, "# tools enabled: {}", list.join(", "))?
        }
        OutEvent::Plan { content, .. } => writeln!(out, "▸ plan:\n{content}")?,
        OutEvent::TextDelta { text, .. } => writeln!(out, "> {text}")?,
        OutEvent::ReasoningDelta { text, .. } => writeln!(out, "· {text}")?,
        // Streaming tool-arg fragment (#194): the batch renderer prints the whole
        // call on `ToolCall`, so the per-fragment delta is display-only noise here.
        OutEvent::ToolCallDelta { .. } => {}
        // The real tool name plus a one-line readable summary, shared with the
        // TUI header (ADR-0204 §6) — never the raw JSON input.
        OutEvent::ToolCall { tool, input, .. } => {
            writeln!(out, "→ {}", summary::call_line(tool, input))?
        }
        OutEvent::ToolRequest { tool, input, .. } => {
            writeln!(out, "? {}", summary::call_line(tool, input))?
        }
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
        // `is_error` (#636, ADR-0176) picks the sigil — the text itself is
        // unchanged, so a denied/failed call was always readable, just not
        // visually distinct from a success at a glance.
        OutEvent::ToolOutput {
            output, is_error, ..
        } => {
            let sigil = if *is_error { '✗' } else { '=' };
            let lines = summary::readable_output(output);
            writeln!(out, "{sigil} {}", lines.first().map_or("", String::as_str))?;
            for line in lines.iter().skip(1) {
                writeln!(out, "  {line}")?;
            }
        }
        OutEvent::TaskList { content, .. } => {
            writeln!(out, "▢ tasks:")?;
            for line in content.lines() {
                writeln!(out, "  {line}")?;
            }
        }
        // `input_tokens` is only the uncached portion (ADR-0202); the billed
        // prompt adds cache reads and writes.
        OutEvent::Usage {
            input_tokens,
            output_tokens,
            cached_input_tokens,
            cache_write_tokens,
            cost_usd,
            ..
        } => {
            let prompt = input_tokens + cached_input_tokens + cache_write_tokens;
            let line = format!(
                "$ usage: {prompt} in ({cached_input_tokens} cached) / {output_tokens} out"
            );
            match cost_usd {
                Some(cost) => writeln!(out, "{line} (${cost:.4})")?,
                None => writeln!(out, "{line}")?,
            }
        }
        OutEvent::Error { message, .. } => writeln!(out, "! {message}")?,
        OutEvent::Done { .. } => writeln!(out, "✓ done")?,
        // Every compaction forks a successor and retires this session
        // (ADR-0205); `auto`/`kind` only say which path got there.
        OutEvent::Compacted {
            summary,
            auto,
            mode,
            ..
        } => {
            let how = match (auto, mode) {
                (false, _) => "compacted",
                (true, CompactionMode::Summary) => {
                    "auto-compacted: context overflowed the model's window, summarized"
                }
                (true, CompactionMode::Prune) => {
                    "auto-compacted: context overflowed the model's window and no summary \
                     was available, so the oldest tool output was pruned"
                }
            };
            writeln!(
                out,
                "▸ {how} — continuing in a successor session (this one is retired, \
                 its log preserved):\n{summary}"
            )?
        }
        OutEvent::FileChange {
            path, change_kind, ..
        } => writeln!(out, "✓ {change_kind:?}: {path}")?,
        // Watcher-driven out-of-band plan-file edit notice (#627, ADR-0145
        // "Consequences"): a non-TUI head sees this live too, not only as a
        // refusal at the next `propose_plan(path=...)` call.
        OutEvent::PlanChanged { path, .. } => writeln!(out, "◆ plan file changed on disk: {path}")?,
        // Skill-active posture (#400, ADR-0106; posture-only since ADR-0194): a
        // wire-facing audit event for a head to render, not required for the
        // one-shot text render.
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
        // Captured extended-thinking block: the persistence rail for reasoning
        // already rendered live via `ReasoningDelta` — nothing new to show.
        OutEvent::ReasoningBlock { .. } => {}
        // LLM endpoint throttle transition (#517, ADR-0141): the wire-visible
        // counterpart to the TUI's `throttle_status()` poll — this is the
        // signal that reaches a non-TUI head, so it renders in full.
        OutEvent::Throttle {
            endpoint,
            throttled,
            in_flight,
            cap,
            retry_in_ms,
            pacing_in_ms,
            waiters,
            shared_leases,
        } => {
            if *throttled {
                let detail = match (retry_in_ms, pacing_in_ms) {
                    (Some(ms), _) => format!("retry {:.1}s", *ms as f64 / 1000.0),
                    (None, Some(ms)) => format!("pacing · next {:.1}s", *ms as f64 / 1000.0),
                    (None, None) => "busy".to_string(),
                };
                // `shared_leases` (#552) is surfaced only when it disagrees
                // with this process's own `in_flight` — otherwise it's just
                // noise repeating the same number.
                let shared = shared_leases
                    .filter(|leases| leases != in_flight)
                    .map(|leases| format!(" · shared {leases}/{cap}"))
                    .unwrap_or_default();
                let queued = if *waiters > 0 {
                    format!(" · {waiters} queued")
                } else {
                    String::new()
                };
                writeln!(
                    out,
                    "⚠ {endpoint} throttled · {detail} · {in_flight}/{cap}{shared}{queued}"
                )?
            } else {
                writeln!(out, "✓ {endpoint} throttle cleared")?
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::SessionId;

    fn render(ev: &OutEvent) -> String {
        let mut buf = Vec::new();
        render_text(&mut buf, ev).expect("render into a Vec never fails");
        String::from_utf8(buf).expect("rendered text is UTF-8")
    }

    #[test]
    fn tool_call_prints_the_real_name_and_a_readable_summary() {
        let text = render(&OutEvent::ToolCall {
            session: SessionId::new("s"),
            seq: 1,
            request_id: "c1".to_string(),
            tool: "mcp__chess__move".to_string(),
            input: r#"{"move":"e2e4"}"#.to_string(),
            envelope: None,
            provider_meta: None,
        });
        assert_eq!(text, "→ chess › move  e2e4\n");
    }

    #[test]
    fn json_tool_output_prints_key_value_lines() {
        let text = render(&OutEvent::ToolOutput {
            session: SessionId::new("s"),
            seq: 2,
            request_id: "c1".to_string(),
            tool: "mcp__chess__move".to_string(),
            output: r#"{"moves":["e4"],"ok":true}"#.to_string(),
            content: vec![],
            is_error: false,
            duration_ms: None,
            exit_code: None,
            envelope: None,
        });
        assert_eq!(text, "= moves:\n    - e4\n  ok: true\n");
    }
}
