//! Session tool overlay (#539, ADR-0149; live bash enablement folded in, ADR-0163).

use serde::{Deserialize, Serialize};

use super::permission::glob_match;
#[cfg(doc)]
use crate::protocol::InMsg;

/// One entry of a session's live tool overlay (#539, ADR-0149): a `*`/`?`
/// wildcard `pattern` (`mcp__chessbase__*` for a server, `bash` for one tool)
/// a head injected via [`InMsg::SetToolOverlay`] — the user's own direct
/// voice, layered over the session's permission mode (ADR-0207). An
/// **enable** entry (trusted frames only) makes matching tools usable for
/// **this session** even where the mode grades them `Deny`: `allow: false`
/// (the default) still routes every matching call through the approval
/// prompt; `allow: true` grants permission outright — both runtime-side and
/// still clamped by the config ceiling (#172). A **deny** entry withdraws
/// matching tools from the session (its `allow`/`arg_pattern` are
/// meaningless and ignored); deny beats enable beats the mode. The overlay
/// is mode-scoped: a live mode change drops it wholesale.
///
/// `arg_pattern` (ADR-0163, #611) narrows an enable entry's grant to matching
/// **command arguments** rather than the whole tool — the live-bash grade
/// this ADR absorbed (formerly `BashGrade::Allow { pattern }`). Do not
/// confuse it with `pattern` above: `pattern` is a **tool-name** glob
/// (e.g. `bash`, `mcp__chessbase__*`); `arg_pattern`
/// is a **command-argument** pattern (e.g. `git *`) materializing an
/// argument-scoped `tool(arg_pattern): allow` rule (ADR-0114/#173's
/// `tool(pattern)` syntax) on top of a flat `Ask` default, fanned out over
/// every tool `pattern` matches. `None` (the default for every existing
/// entry, `#[serde(default)]`) leaves an enable entry's grant flat — `allow`
/// alone decides it, as before this field existed. Ignored on a deny entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOverlayEntry {
    pub pattern: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deny: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arg_pattern: Option<String>,
}

impl ToolOverlayEntry {
    pub fn ask(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            allow: false,
            deny: false,
            arg_pattern: None,
        }
    }

    pub fn deny(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            allow: false,
            deny: true,
            arg_pattern: None,
        }
    }

    /// A flat **enable** entry that also grants outright (`allow: true`, no
    /// `arg_pattern`): the tool is usable for the session *and* skips the
    /// permission prompt on every later call — the "stop asking me"
    /// semantics an ordinary Session grant (#174) gives an `Ask` tool.
    pub fn allow(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            allow: true,
            deny: false,
            arg_pattern: None,
        }
    }

    /// Whether this entry's pattern matches `tool` (`*`/`?` wildcards).
    pub fn matches(&self, tool: &str) -> bool {
        glob_match(&self.pattern, tool)
    }

    /// The overlay's opinion on `tool`: `Some(false)` — a deny entry matches
    /// (the tool is withdrawn for this session, whatever the mode grades);
    /// `Some(true)` — an enable entry matches (usable even where the mode
    /// denies it); `None` — no opinion, the session's permission mode stands.
    /// Deny entries win over enable entries regardless of list order.
    /// Consulted by the runtime at **dispatch** only — the overlay does not
    /// narrow what core advertises, so toggling it leaves the model's tools
    /// array untouched.
    pub fn disposition(entries: &[Self], tool: &str) -> Option<bool> {
        if entries.iter().any(|e| e.deny && e.matches(tool)) {
            return Some(false);
        }
        entries
            .iter()
            .any(|e| !e.deny && e.matches(tool))
            .then_some(true)
    }

    /// The first **enable** entry matching `tool`, if any — the grade-override
    /// lookup (a deny entry has no grade; it removes the tool ahead of any
    /// permission decision).
    pub fn find<'a>(entries: &'a [Self], tool: &str) -> Option<&'a Self> {
        entries.iter().find(|e| !e.deny && e.matches(tool))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_overlay_entry_allow_grants_outright_with_no_arg_pattern() {
        let entry = ToolOverlayEntry::allow("edit");
        assert_eq!(entry.pattern, "edit");
        assert!(entry.allow);
        assert!(!entry.deny);
        assert!(entry.arg_pattern.is_none());
        assert_eq!(
            ToolOverlayEntry::disposition(std::slice::from_ref(&entry), "edit"),
            Some(true)
        );
        assert!(ToolOverlayEntry::find(&[entry], "edit").is_some());
    }

    #[test]
    fn overlay_disposition_deny_beats_enable_beats_none() {
        // #539: deny entries win regardless of list order; enable entries win
        // over "no opinion"; an unmatched tool leaves the profile mask to
        // decide (None).
        let entries = vec![
            ToolOverlayEntry::ask("mcp__*"),
            ToolOverlayEntry::deny("mcp__jira__*"),
        ];
        assert_eq!(
            ToolOverlayEntry::disposition(&entries, "mcp__docs__search"),
            Some(true)
        );
        assert_eq!(
            ToolOverlayEntry::disposition(&entries, "mcp__jira__create"),
            Some(false),
            "deny beats the broader enable"
        );
        assert_eq!(ToolOverlayEntry::disposition(&entries, "edit"), None);
        // The grade lookup skips deny entries entirely.
        assert!(ToolOverlayEntry::find(&entries, "mcp__docs__search").is_some());
        assert!(
            ToolOverlayEntry::find(&entries, "mcp__jira__create").is_some(),
            "find returns the enable entry; existence was already refused by disposition"
        );
    }
}
