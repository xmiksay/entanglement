//! Per-tool **dispatch state** — the three-state truth (`allowed`/`asks`/
//! `declines`) a permission grade produces, shared vocabulary for any surface
//! that renders one.
//!
//! - **allowed** — runs, no prompt;
//! - **asks** — emits `ToolRequest` and waits for the user;
//! - **declines** — refused outright (`Deny`).
//!
//! A bare grade isn't always the whole truth: an argument- or workdir-scoped
//! rule (`write(.entanglement/plans/*.md): allow`) makes a concrete call land
//! somewhere the tool's bare grade never shows. Rather than pick one and lie,
//! a [`ToolState`] carries the bare outcome **plus** the grades reachable only
//! through such a rule (`PermissionProfile::scoped_grades`), rendered as a
//! suffix: `declines (allowed by argument)`.
//!
//! ADR-0207 moved the actual dispatch grade off `AgentProfile` and onto the
//! session's permission mode (`crate::policy::ProfileResolver`), so this module
//! no longer grades a *profile* — [`graded`] grades a bare [`PermissionProfile`]
//! a caller already has in hand (e.g. the config `permissions:` ceiling).

use entanglement_core::{Permission, PermissionProfile};

/// What a dispatch of one tool does under a given profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchState {
    Allowed,
    Asks,
    Declines,
}

impl DispatchState {
    /// The single word both surfaces print for this state.
    pub fn label(self) -> &'static str {
        match self {
            DispatchState::Allowed => "allowed",
            DispatchState::Asks => "asks",
            DispatchState::Declines => "declines",
        }
    }

    fn from_grade(grade: Permission) -> Self {
        match grade {
            Permission::Allow => DispatchState::Allowed,
            Permission::Ask => DispatchState::Asks,
            Permission::Deny => DispatchState::Declines,
        }
    }
}

/// One tool's dispatch state under one profile: the outcome of a bare call,
/// plus any *other* outcome an argument-/workdir-scoped rule can reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolState {
    pub state: DispatchState,
    /// Outcomes reachable only through a `tool(pattern)`/`tool{pattern}` rule,
    /// deduped and never containing [`state`][Self::state]. Empty ⇒ the bare
    /// state is the whole truth.
    pub by_argument: Vec<DispatchState>,
}

impl ToolState {
    /// A withheld tool: nothing to say about arguments, since the gate that
    /// declines it never looks at them.
    pub fn declines() -> Self {
        Self {
            state: DispatchState::Declines,
            by_argument: Vec::new(),
        }
    }

    /// `allowed` / `asks` / `declines`, suffixed with the argument-scoped
    /// escape hatches when the bare grade hides one:
    /// `declines (allowed by argument)`.
    pub fn label(&self) -> String {
        if self.by_argument.is_empty() {
            return self.state.label().to_string();
        }
        let variants: Vec<&str> = self.by_argument.iter().map(|s| s.label()).collect();
        format!(
            "{} ({} by argument)",
            self.state.label(),
            variants.join("/")
        )
    }
}

/// One tool's state from its grade under a bare [`PermissionProfile`] a
/// caller already has in hand (e.g. the config `permissions:` ceiling).
pub fn graded(permission: &PermissionProfile, tool: &str) -> ToolState {
    ToolState {
        state: DispatchState::from_grade(permission.for_tool(tool)),
        by_argument: permission
            .scoped_grades(tool)
            .into_iter()
            .map(DispatchState::from_grade)
            .collect(),
    }
}

/// The roster an **engine-free** surface grades against: the compile-time
/// literal tool vocabulary, minus `read_raw` — an internal alias of `read` the
/// model is never shown (ADR-0098). MCP tools are necessarily absent: no
/// server connects in a view that spawns no engine, so their names are
/// unknowable there.
pub fn static_roster() -> Vec<&'static str> {
    crate::tool_names::known_tool_names()
        .iter()
        .copied()
        .filter(|t| *t != "read_raw")
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_names::{AGENT_TOOL, PROPOSE_PLAN_TOOL};

    #[test]
    fn grade_drives_the_three_states() {
        let p = PermissionProfile::new(Permission::Ask)
            .with("read", Permission::Allow)
            .with("write", Permission::Deny);
        assert_eq!(graded(&p, "read").label(), "allowed");
        assert_eq!(graded(&p, "edit").label(), "asks");
        assert_eq!(graded(&p, "write").label(), "declines");
    }

    #[test]
    fn argument_scoped_escape_hatch_is_spelled_out() {
        // The `plan` profile's shape: bare `write` denies, a plans-folder path
        // is allowed.
        let p = PermissionProfile::new(Permission::Ask)
            .with("write", Permission::Deny)
            .with("write(.entanglement/plans/*.md)", Permission::Allow);
        assert_eq!(
            graded(&p, "write").label(),
            "declines (allowed by argument)"
        );
        // A scoped rule agreeing with the bare grade adds no noise.
        let quiet = PermissionProfile::new(Permission::Ask).with("call(*)", Permission::Ask);
        assert_eq!(graded(&quiet, "call").label(), "asks");
    }

    #[test]
    fn static_roster_covers_the_built_ins_and_hides_read_raw() {
        let roster = static_roster();
        for expected in [
            "read",
            "write",
            "bash",
            "poll",
            AGENT_TOOL,
            PROPOSE_PLAN_TOOL,
        ] {
            assert!(roster.contains(&expected), "{expected} missing: {roster:?}");
        }
        assert!(!roster.contains(&"read_raw"), "{roster:?}");
    }
}
