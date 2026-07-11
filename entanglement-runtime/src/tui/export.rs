//! Session-transcript → Markdown export (ADR-0029).
//!
//! Reconstructs a readable Markdown document from the head's accumulated
//! [`TranscriptEntry`] stream. Pure over its inputs so it unit-tests without a
//! terminal; the `$EDITOR` launch that opens the result lives in
//! [`crate::tui::editor`].

use entanglement_core::SessionId;

use crate::tui::session_view::TranscriptEntry;

/// Filesystem-safe export filename `<session>-<unix_secs>.md`. The session id can
/// be an arbitrary string, so non-`[A-Za-z0-9_-]` characters are folded to `_`.
pub fn export_filename(session: &SessionId, unix_secs: u64) -> String {
    let safe: String = session
        .0
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{safe}-{unix_secs}.md")
}

/// Renders the session's visible transcript as a standalone Markdown document.
/// Consecutive streamed deltas are coalesced into one block (the engine emits
/// text/reasoning token-by-token), mirroring how the TUI renders them.
pub fn transcript_to_markdown(
    session: &SessionId,
    plan: Option<&str>,
    tasks: Option<&str>,
    transcript: &[TranscriptEntry],
    unix_secs: u64,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Session `{session}`\n\n"));
    out.push_str(&format!("_Exported at {unix_secs} (Unix time)_\n\n"));

    if let Some(plan) = plan.filter(|p| !p.trim().is_empty()) {
        out.push_str("## Plan\n\n");
        out.push_str(plan.trim_end());
        out.push_str("\n\n");
    }

    if let Some(tasks) = tasks.filter(|t| !t.trim().is_empty()) {
        out.push_str("## Tasks\n\n");
        out.push_str(tasks.trim_end());
        out.push_str("\n\n");
    }

    // Coalesce consecutive deltas, flushing the *other* accumulator on a switch
    // and both at any non-delta boundary — the same ordering discipline the TUI
    // renderer uses so text and thinking blocks stay in arrival order.
    let mut text = String::new();
    let mut reasoning = String::new();
    for entry in transcript {
        match entry {
            TranscriptEntry::TextDelta { text: t } => {
                flush_reasoning(&mut out, &mut reasoning);
                text.push_str(t);
            }
            TranscriptEntry::ReasoningDelta { text: t } => {
                flush_text(&mut out, &mut text);
                reasoning.push_str(t);
            }
            other => {
                flush_text(&mut out, &mut text);
                flush_reasoning(&mut out, &mut reasoning);
                append_entry(&mut out, other);
            }
        }
    }
    flush_text(&mut out, &mut text);
    flush_reasoning(&mut out, &mut reasoning);

    out
}

fn flush_text(out: &mut String, buf: &mut String) {
    if !buf.trim().is_empty() {
        out.push_str("## Assistant\n\n");
        out.push_str(buf.trim_end());
        out.push_str("\n\n");
    }
    buf.clear();
}

fn flush_reasoning(out: &mut String, buf: &mut String) {
    if !buf.trim().is_empty() {
        out.push_str("### Reasoning\n\n");
        push_blockquote(out, buf.trim_end());
    }
    buf.clear();
}

fn append_entry(out: &mut String, entry: &TranscriptEntry) {
    match entry {
        TranscriptEntry::User { text, .. } => {
            out.push_str("## User\n\n");
            out.push_str(text.trim_end());
            out.push_str("\n\n");
        }
        TranscriptEntry::ToolCall { tool, input } => {
            out.push_str(&format!("### Tool call: `{tool}`\n\n"));
            out.push_str(&fenced(&pretty_json(input), "json"));
        }
        TranscriptEntry::ToolOutput { tool, output } => {
            match tool {
                Some(t) => out.push_str(&format!("**Output** (`{t}`):\n\n")),
                None => out.push_str("**Output:**\n\n"),
            }
            out.push_str(&fenced(output.trim_end(), ""));
        }
        TranscriptEntry::Error { message } => {
            out.push_str("### Error\n\n");
            push_blockquote(out, message.trim_end());
        }
        TranscriptEntry::Done => {
            out.push_str("---\n\n");
        }
        // Deltas are coalesced by the caller and never reach here.
        TranscriptEntry::TextDelta { .. } | TranscriptEntry::ReasoningDelta { .. } => {}
    }
}

fn push_blockquote(out: &mut String, body: &str) {
    for line in body.lines() {
        out.push_str("> ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
}

/// Wraps `content` in a fenced code block whose fence is longer than any
/// backtick run inside it, so tool output containing ``` never breaks out.
fn fenced(content: &str, lang: &str) -> String {
    let fence = "`".repeat(max_backtick_run(content).max(2) + 1);
    format!("{fence}{lang}\n{content}\n{fence}\n\n")
}

fn max_backtick_run(s: &str) -> usize {
    let mut max = 0;
    let mut cur = 0;
    for c in s.chars() {
        if c == '`' {
            cur += 1;
            max = max.max(cur);
        } else {
            cur = 0;
        }
    }
    max
}

fn pretty_json(input: &str) -> String {
    serde_json::from_str::<serde_json::Value>(input)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| input.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid() -> SessionId {
        SessionId::new("s1")
    }

    #[test]
    fn filename_sanitizes_unsafe_chars() {
        let s = SessionId::new("a/b:c d");
        assert_eq!(export_filename(&s, 42), "a_b_c_d-42.md");
        // A uuid-shaped id passes through untouched.
        let uuid = SessionId::new("3f2b1a-9c8d");
        assert_eq!(export_filename(&uuid, 7), "3f2b1a-9c8d-7.md");
    }

    #[test]
    fn coalesces_deltas_and_orders_reasoning_before_text() {
        let transcript = vec![
            TranscriptEntry::User {
                text: "hi".into(),
                pending: false,
            },
            TranscriptEntry::ReasoningDelta {
                text: "think ".into(),
            },
            TranscriptEntry::ReasoningDelta {
                text: "hard".into(),
            },
            TranscriptEntry::TextDelta {
                text: "Hello ".into(),
            },
            TranscriptEntry::TextDelta {
                text: "world".into(),
            },
            TranscriptEntry::Done,
        ];
        let md = transcript_to_markdown(&sid(), None, None, &transcript, 100);

        let user = md.find("## User").unwrap();
        let reasoning = md.find("### Reasoning").unwrap();
        let assistant = md.find("## Assistant").unwrap();
        assert!(user < reasoning && reasoning < assistant);
        assert!(md.contains("> think hard"));
        assert!(md.contains("Hello world"));
        assert!(md.contains("\n---\n"));
    }

    #[test]
    fn tool_call_pretty_prints_json_and_output_is_fenced() {
        let transcript = vec![
            TranscriptEntry::ToolCall {
                tool: "read".into(),
                input: r#"{"path":"a.rs"}"#.into(),
            },
            TranscriptEntry::ToolOutput {
                tool: Some("read".into()),
                output: "fn main() {}".into(),
            },
        ];
        let md = transcript_to_markdown(&sid(), None, None, &transcript, 0);
        assert!(md.contains("### Tool call: `read`"));
        // Pretty-printed JSON spans multiple lines.
        assert!(md.contains("  \"path\": \"a.rs\""));
        assert!(md.contains("**Output** (`read`):"));
        assert!(md.contains("fn main() {}"));
    }

    #[test]
    fn output_with_backticks_gets_a_longer_fence() {
        let transcript = vec![TranscriptEntry::ToolOutput {
            tool: None,
            output: "```rust\nlet x = 1;\n```".into(),
        }];
        let md = transcript_to_markdown(&sid(), None, None, &transcript, 0);
        // Inner ``` must be wrapped by a four-backtick fence so it doesn't escape.
        assert!(md.contains("````\n```rust"));
    }

    #[test]
    fn plan_and_tasks_render_when_present() {
        let tasks = "- [x] done thing\n- [ ] todo thing";
        let md = transcript_to_markdown(&sid(), Some("The plan."), Some(tasks), &[], 0);
        assert!(md.contains("## Plan\n\nThe plan."));
        assert!(md.contains("- [x] done thing"));
        assert!(md.contains("- [ ] todo thing"));
    }
}
