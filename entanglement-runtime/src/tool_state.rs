//! Per-tool **dispatch state** — the three-state truth every profile-posture
//! UI renders (`skutter inspect agents`, the TUI `/agent` tools checklist).
//!
//! Advertisement is decoupled from enforcement: core advertises every spec the
//! config provides and the masks are enforced at dispatch ([`crate::decline`]).
//! So "the tool is absent from this profile" is no longer a state a UI can
//! show — every tool is present, and what varies is *what happens when the
//! model calls it*:
//!
//! - **allowed** — runs, no prompt;
//! - **asks** — emits `ToolRequest` and waits for the user;
//! - **declines** — refused outright, either by the permission grade (`Deny`)
//!   or because a gate withholds the tool (the profile mask, or the
//!   spawn/plan-authorship gate on the profile-defining specs).
//!
//! A bare grade isn't always the whole truth: an argument- or workdir-scoped
//! rule (`write(.entanglement/plans/*.md): allow`) makes a concrete call land
//! somewhere the tool's bare grade never shows. Rather than pick one and lie,
//! a [`ToolState`] carries the bare outcome **plus** the grades reachable only
//! through such a rule (`PermissionProfile::scoped_grades`), rendered as a
//! suffix: `declines (allowed by argument)`. The same vocabulary and the same
//! suffix are used by both surfaces, so a state read in one reads identically
//! in the other.

use entanglement_core::{AgentProfile, Permission, PermissionProfile};

use crate::tool_names::{AGENT_SEND_TOOL, AGENT_TOOL, PROPOSE_PLAN_TOOL};

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

/// One tool's state under `profile` — the mask, the profile-defining
/// advertisement gates, and the permission grade folded into one answer.
pub fn for_profile(profile: &AgentProfile, tool: &str) -> ToolState {
    if !profile.advertises_tool(tool) || !profile_gate_admits(profile, tool) {
        return ToolState::declines();
    }
    graded(&profile.permission, tool)
}

/// One tool's state from its **grade alone**, for a caller holding the mask
/// separately — the TUI checklist, whose checkbox *is* the mask being edited,
/// so it recombines the two itself (unchecked ⇒ [`ToolState::declines`]).
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

/// The two profile-defining specs whose advertisement is gated by something
/// other than the mask (design decision 2 — they vary per profile, never
/// within a session): the `agent`/`agent_send` spawn family behind
/// `may_spawn()`, and `propose_plan` behind explicit mask membership. Both
/// gates also refuse the *call*, so folding them into `declines` is honest.
fn profile_gate_admits(profile: &AgentProfile, tool: &str) -> bool {
    match tool {
        AGENT_TOOL | AGENT_SEND_TOOL => profile.may_spawn(),
        PROPOSE_PLAN_TOOL => crate::plan_tasks::explicitly_allowlists(profile, PROPOSE_PLAN_TOOL),
        _ => true,
    }
}

/// The roster an **engine-free** surface grades a profile against
/// (`skutter inspect agents`): the compile-time literal tool vocabulary, minus
/// `read_raw` — an internal alias of `read` the model is never shown
/// (ADR-0098). MCP tools are necessarily absent: no server connects in a view
/// that spawns no engine, so their names are unknowable there.
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
    use entanglement_core::AgentMode;

    fn profile(
        default: Permission,
        rules: &[(&str, Permission)],
        tools: Option<Vec<&str>>,
    ) -> AgentProfile {
        let mut permission = PermissionProfile::new(default);
        for (k, p) in rules {
            permission = permission.with(*k, *p);
        }
        AgentProfile {
            name: "t".into(),
            description: String::new(),
            mode: AgentMode::Primary,
            system_prompt: String::new(),
            model: None,
            provider: None,
            permission,
            tools: tools.map(|v| v.into_iter().map(String::from).collect()),
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
        }
    }

    #[test]
    fn grade_drives_the_three_states() {
        let p = profile(
            Permission::Ask,
            &[("read", Permission::Allow), ("write", Permission::Deny)],
            None,
        );
        assert_eq!(for_profile(&p, "read").label(), "allowed");
        assert_eq!(for_profile(&p, "edit").label(), "asks");
        assert_eq!(for_profile(&p, "write").label(), "declines");
    }

    #[test]
    fn a_masked_out_tool_declines_whatever_its_grade_says() {
        let p = profile(Permission::Allow, &[], Some(vec!["read"]));
        assert_eq!(for_profile(&p, "read").label(), "allowed");
        assert_eq!(
            for_profile(&p, "edit"),
            ToolState::declines(),
            "the mask wins over an Allow grade — and says nothing about arguments"
        );
    }

    #[test]
    fn argument_scoped_escape_hatch_is_spelled_out() {
        // The `plan` profile's shape: bare `write` denies, a plans-folder path
        // is allowed.
        let p = profile(
            Permission::Ask,
            &[
                ("write", Permission::Deny),
                ("write(.entanglement/plans/*.md)", Permission::Allow),
            ],
            None,
        );
        assert_eq!(
            for_profile(&p, "write").label(),
            "declines (allowed by argument)"
        );
        // A scoped rule agreeing with the bare grade adds no noise.
        let quiet = profile(Permission::Ask, &[("call(*)", Permission::Ask)], None);
        assert_eq!(for_profile(&quiet, "call").label(), "asks");
    }

    #[test]
    fn spawn_and_plan_gates_decline_past_the_mask() {
        // Mask + grade both wide open, but neither spec is advertised to this
        // profile — the call would be refused, so it must not read `allowed`.
        let mut p = profile(Permission::Allow, &[], None);
        p.mode = AgentMode::Subagent; // may_spawn() defaults closed
        assert_eq!(for_profile(&p, AGENT_TOOL), ToolState::declines());
        assert_eq!(for_profile(&p, AGENT_SEND_TOOL), ToolState::declines());
        assert_eq!(for_profile(&p, PROPOSE_PLAN_TOOL), ToolState::declines());

        // A profile that explicitly allowlists `propose_plan` authors plans.
        let author = profile(
            Permission::Allow,
            &[],
            Some(vec!["read", PROPOSE_PLAN_TOOL, AGENT_TOOL]),
        );
        assert_eq!(for_profile(&author, PROPOSE_PLAN_TOOL).label(), "allowed");
        assert_eq!(for_profile(&author, AGENT_TOOL).label(), "allowed");
    }

    #[test]
    fn graded_ignores_the_mask_for_the_checklist_recombination() {
        let p = profile(Permission::Allow, &[], Some(vec!["read"]));
        assert_eq!(graded(&p.permission, "edit").label(), "allowed");
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
