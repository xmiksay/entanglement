//! The wire-side half of #843: a `SetToolOverlay` entry naming a tool no
//! overlay can match (`tool_names::NON_MASKABLE_TOOLS`) is accepted — core
//! holds no tool policy to refuse it with — but the runtime, which owns that
//! policy, logs that the entry has no effect when it folds the
//! `ToolOverlayChanged` reply.

use entanglement_core::{SessionId, ToolOverlayEntry};

use crate::tool_names::non_maskable_reason;

/// `(pattern, reason)` for each entry of `next` naming a non-maskable tool
/// that `prev` did not already carry — a full-replacement update re-sends
/// every standing entry, so only a new one is worth a warning.
fn new_inert_entries<'a>(
    prev: &[ToolOverlayEntry],
    next: &'a [ToolOverlayEntry],
) -> Vec<(&'a str, String)> {
    next.iter()
        .filter(|e| !prev.iter().any(|p| p.pattern == e.pattern))
        .filter_map(|e| non_maskable_reason(&e.pattern).map(|why| (e.pattern.as_str(), why)))
        .collect()
}

/// Log one warning per newly applied no-effect overlay entry.
pub(super) fn warn_inert_entries(
    session: &SessionId,
    prev: Option<&Vec<ToolOverlayEntry>>,
    next: &[ToolOverlayEntry],
) {
    for (pattern, reason) in new_inert_entries(prev.map_or(&[], Vec::as_slice), next) {
        tracing::warn!(
            %session,
            %pattern,
            "tool overlay entry names a tool no overlay can match — it has no effect: {reason}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_new_non_maskable_entry_is_reported() {
        let next = vec![
            ToolOverlayEntry::deny("mcp"),
            ToolOverlayEntry::deny("list_skills"),
            ToolOverlayEntry::deny("mcp__gh__*"),
            ToolOverlayEntry::deny("bash"),
        ];
        let fresh = new_inert_entries(&[], &next);
        let names: Vec<&str> = fresh.iter().map(|(p, _)| *p).collect();
        assert_eq!(names, ["mcp", "list_skills"]);
        assert!(fresh[0].1.contains("mcp__<server>__"), "{}", fresh[0].1);

        // Re-sent alongside a new entry, a standing one is not re-reported.
        let prev = next.clone();
        let mut again = next.clone();
        again.push(ToolOverlayEntry::ask("glob_json"));
        let names: Vec<&str> = new_inert_entries(&prev, &again)
            .iter()
            .map(|(p, _)| *p)
            .collect();
        assert_eq!(names, ["glob_json"]);
    }

    /// #849: a wire overlay entry naming a bookkeeping Control tool warns;
    /// one naming `agent`/`agent_send` is a real entry (graded, ADR-0232).
    #[test]
    fn bookkeeping_control_tool_entries_are_reported_spawning_is_not() {
        let next: Vec<ToolOverlayEntry> = [
            "poll",
            "ask_user",
            "update_tasks",
            "request_mode",
            "propose_plan",
            "agent",
            "agent_send",
        ]
        .map(ToolOverlayEntry::deny)
        .to_vec();
        let names: Vec<&str> = new_inert_entries(&[], &next)
            .iter()
            .map(|(p, _)| *p)
            .collect();
        assert_eq!(
            names,
            [
                "poll",
                "ask_user",
                "update_tasks",
                "request_mode",
                "propose_plan"
            ]
        );
    }
}
