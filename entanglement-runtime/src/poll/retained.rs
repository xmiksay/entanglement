//! The retained-output (`o-`) handle path of `poll` (#608, ADR-0161 §7): a
//! completed operation whose result overflowed its cap. Split out of `mod.rs`
//! (file-cap, issue #451) since it is a self-contained dispatch arm with no
//! dependency on the job/agent paths beyond the shared `PollInput`/
//! `unknown_handle`/`kill_refused_message` helpers those still own.

use entanglement_core::SessionId;

use super::PollInput;
use crate::retained_output::{Page, RetainedOutputRegistry};

/// A retained-output id (ADR-0164's `o-` prefix, #608) — a completed
/// operation whose result overflowed its cap.
pub(super) fn is_retained_handle(handle: &str) -> bool {
    handle.starts_with("o-")
}

/// A retained-output poll never waits — the operation it pages already
/// finished — and refuses `kill` like an agent handle (#608).
pub(super) fn resolve_retained(
    retained: &RetainedOutputRegistry,
    session: &SessionId,
    parsed: &PollInput,
) -> String {
    if parsed.kill {
        return super::kill_refused_message(&parsed.handle);
    }
    match retained.page(&parsed.handle, session, parsed.offset, parsed.tail) {
        Some(page) => format_retained_page(&parsed.handle, page),
        None => super::unknown_handle(&parsed.handle),
    }
}

/// Render a retained-output page: an `output_file`d operation just names its
/// path (the one place a path still appears — ADR-0161 §7); otherwise the
/// requested line slice, byte-capped, with the range/more-remaining and any
/// retention-cap overflow reported so an operation's own retention never
/// silently drops text.
fn format_retained_page(id: &str, page: Page) -> String {
    if let Some(path) = page.output_file {
        return format!("[retained output {id}: full output at {path}]\n");
    }
    if page.total_lines == 0 {
        return format!("[retained output {id}: empty]\n");
    }
    if page.text.is_empty() {
        return format!(
            "[retained output {id}: offset {} is past the end ({} lines total)]\n",
            page.start_line, page.total_lines
        );
    }
    let end_line = page.start_line + page.text.lines().count();
    let mut header = format!(
        "[retained output {id}: lines {}-{} of {}{}]\n",
        page.start_line + 1,
        end_line,
        page.total_lines,
        if page.has_more {
            ", more available"
        } else {
            ""
        },
    );
    if page.dropped_bytes > 0 {
        header.push_str(&format!(
            "[{} bytes evicted from retention past the per-operation cap — \
             unrecoverable]\n",
            page.dropped_bytes
        ));
    }
    crate::host::bounded_result(&header, page.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_registry::AgentRegistry;
    use crate::host::jobs::JobRegistry;
    use crate::script_ops::ScriptRegistry;

    #[test]
    fn is_retained_handle_matches_the_o_prefix_only() {
        assert!(is_retained_handle("o-6a708af0a1002"));
        assert!(!is_retained_handle("j-6a708af0a1002"));
        assert!(!is_retained_handle("s-6a708af0a1002"));
    }

    /// #608: a retained-output handle pages the text with `offset`/`tail`,
    /// defaulting to 30 lines from 0, and reports when more remains.
    #[tokio::test]
    async fn retained_handle_pages_with_offset_and_tail() {
        let session = SessionId::new("s4");
        let retained = RetainedOutputRegistry::new();
        let text: String = (1..=50)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let id = retained.register_text(Some(session.clone()), text);

        let first = super::super::resolve(
            &JobRegistry::new(),
            &AgentRegistry::default(),
            &retained,
            &ScriptRegistry::new(),
            &session,
            &serde_json::json!({"handle": id, "tail": 10}).to_string(),
        )
        .await;
        assert!(
            first.contains("line1") && first.contains("line10"),
            "{first}"
        );
        assert!(!first.contains("line11"), "{first}");
        assert!(first.contains("more available"), "{first}");

        let second = super::super::resolve(
            &JobRegistry::new(),
            &AgentRegistry::default(),
            &retained,
            &ScriptRegistry::new(),
            &session,
            &serde_json::json!({"handle": id, "offset": 10, "tail": 10}).to_string(),
        )
        .await;
        assert!(
            second.contains("line11") && second.contains("line20"),
            "{second}"
        );
        assert!(!second.contains("line21"), "{second}");
    }

    /// #608: an operation registered with an explicit `output_file` is named,
    /// not paged — the one place a path still appears.
    #[tokio::test]
    async fn retained_handle_with_output_file_names_the_path_not_text() {
        let session = SessionId::new("s5");
        let retained = RetainedOutputRegistry::new();
        let id = retained.register_file(Some(session.clone()), "out/result.txt".to_string());

        let out = super::super::resolve(
            &JobRegistry::new(),
            &AgentRegistry::default(),
            &retained,
            &ScriptRegistry::new(),
            &session,
            &serde_json::json!({"handle": id}).to_string(),
        )
        .await;
        assert!(out.contains("out/result.txt"), "{out}");
    }

    /// #608: `kill` is refused on a retained-output handle — nothing running.
    #[tokio::test]
    async fn kill_is_refused_on_a_retained_output_handle() {
        let session = SessionId::new("s6");
        let retained = RetainedOutputRegistry::new();
        let id = retained.register_text(Some(session.clone()), "hi".to_string());

        let out = super::super::resolve(
            &JobRegistry::new(),
            &AgentRegistry::default(),
            &retained,
            &ScriptRegistry::new(),
            &session,
            &serde_json::json!({"handle": id, "kill": true}).to_string(),
        )
        .await;
        assert!(
            out.contains("not supported") && out.contains("retained-output"),
            "{out}"
        );
    }

    /// #608: a retained-output handle from a different session is unknown —
    /// the same ownership scoping job/agent handles enforce.
    #[tokio::test]
    async fn retained_handle_from_a_different_session_is_unknown() {
        let owner = SessionId::new("owner2");
        let stranger = SessionId::new("stranger2");
        let retained = RetainedOutputRegistry::new();
        let id = retained.register_text(Some(owner), "hi".to_string());

        let out = super::super::resolve(
            &JobRegistry::new(),
            &AgentRegistry::default(),
            &retained,
            &ScriptRegistry::new(),
            &stranger,
            &serde_json::json!({"handle": id}).to_string(),
        )
        .await;
        assert!(out.contains("unknown handle"), "{out}");
    }
}
