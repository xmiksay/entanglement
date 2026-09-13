//! Attributed **autodecline** messages for a tool call the dispatch gate
//! refuses.
//!
//! Advertisement is decoupled from enforcement: core advertises every spec the
//! config provides, so the model can (and will) call a tool its profile mask,
//! the session tool overlay, or an active skill's `allowed_tools` withholds.
//! Each such call is answered with a terse, **attributed** refusal — the model
//! must learn *who* declined it, or it retries the same call forever — carried
//! on the ADR-0176 structured side channel with `is_error: true`.
//!
//! The wording family is one table, here, so the executor's dispatch ladder and
//! the mask walk in [`crate::permission`] can never drift apart:
//!
//! - `Declined by agent profile ...`
//! - `Declined by ancestor agent ...`
//! - `Declined by session tool overlay ...`
//! - `Declined by skill ...`
//! - `` tool `bash` is disabled — enable with /enable tool bash ``

use entanglement_core::SessionId;

/// Which authority at a chain link withheld the tool — the mask walk
/// ([`crate::permission::tool_mask_source`]) reports it alongside the link, so
/// a refusal can name the overlay rather than blaming the agent definition the
/// user never edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskAuthority {
    /// The link's `tools`/`disallowed_tools` profile mask (#116, ADR-0038).
    Profile,
    /// A deny entry in the link's live session tool overlay (#539, ADR-0149).
    Overlay,
    /// The link was never seen by the executor's lifecycle fold — fail-closed
    /// (#156), so it masks everything.
    Unseen,
}

/// Which link in the ancestor chain declined the call, and on whose authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskSource {
    pub session: SessionId,
    pub authority: MaskAuthority,
}

impl MaskSource {
    pub fn profile(session: SessionId) -> Self {
        Self {
            session,
            authority: MaskAuthority::Profile,
        }
    }

    pub fn overlay(session: SessionId) -> Self {
        Self {
            session,
            authority: MaskAuthority::Overlay,
        }
    }

    pub fn unseen(session: SessionId) -> Self {
        Self {
            session,
            authority: MaskAuthority::Unseen,
        }
    }
}

/// The refusal for a masked `tool` call in `own_session`. `agent_name` is the
/// profile name active at the declining link (`None` when the link is unseen),
/// used only to attribute an *ancestor*'s decision — a self-inflicted mask
/// names the session's own profile.
pub fn mask_decline(
    source: &MaskSource,
    own_session: &SessionId,
    agent_name: Option<&str>,
    tool: &str,
) -> String {
    let own = source.session == *own_session;
    let agent = agent_name.unwrap_or("unknown");
    match (source.authority, own) {
        (MaskAuthority::Profile, true) => {
            format!("Declined by agent profile `{agent}` — tool `{tool}` is not in its tool mask")
        }
        (MaskAuthority::Profile, false) => format!(
            "Declined by ancestor agent `{agent}`'s profile — tool `{tool}` is not in its tool mask"
        ),
        (MaskAuthority::Overlay, true) => {
            format!(
                "Declined by session tool overlay — tool `{tool}` is withdrawn for this session"
            )
        }
        (MaskAuthority::Overlay, false) => format!(
            "Declined by ancestor agent `{agent}`'s session tool overlay — tool `{tool}` is \
             withdrawn for its sub-tree"
        ),
        // Fail-closed (#156): a lifecycle event was dropped, so the deciding
        // profile is unknown. Say so rather than blaming a named agent.
        (MaskAuthority::Unseen, _) => format!(
            "Declined: tool `{tool}`'s agent profile is not yet known to the executor \
             (fail-closed)"
        ),
    }
}

/// The refusal for a call an active skill's `allowed_tools` withholds (#400,
/// ADR-0106) — layered after the agent mask, so this only fires for a tool the
/// profile itself admits.
pub fn skill_decline(skill_id: &str, tool: &str) -> String {
    format!("Declined by skill `{skill_id}`'s allowed_tools — tool `{tool}` is not listed")
}

/// The refusal for a lazily-registrable built-in that is advertised but not yet
/// registered (`bash`, ADR-0163 §2). Advertisement is unconditional now, so
/// this is the *only* signal the model gets that the tool exists but is off —
/// it names the exact command that turns it on.
pub fn disabled_builtin_decline(tool: &str) -> String {
    format!("tool `{tool}` is disabled — enable with /enable tool {tool}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: &str) -> SessionId {
        SessionId::new(id)
    }

    #[test]
    fn own_profile_names_the_session_s_own_agent() {
        let msg = mask_decline(
            &MaskSource::profile(s("s1")),
            &s("s1"),
            Some("research"),
            "edit",
        );
        assert_eq!(
            msg,
            "Declined by agent profile `research` — tool `edit` is not in its tool mask"
        );
    }

    #[test]
    fn ancestor_profile_names_the_clamping_agent() {
        let msg = mask_decline(
            &MaskSource::profile(s("parent")),
            &s("child"),
            Some("plan"),
            "write",
        );
        assert_eq!(
            msg,
            "Declined by ancestor agent `plan`'s profile — tool `write` is not in its tool mask"
        );
    }

    #[test]
    fn overlay_is_attributed_to_the_overlay_not_the_profile() {
        let own = mask_decline(
            &MaskSource::overlay(s("s1")),
            &s("s1"),
            Some("build"),
            "bash",
        );
        assert_eq!(
            own,
            "Declined by session tool overlay — tool `bash` is withdrawn for this session"
        );
        let ancestor = mask_decline(
            &MaskSource::overlay(s("parent")),
            &s("child"),
            Some("build"),
            "bash",
        );
        assert!(
            ancestor.contains("ancestor agent `build`'s session tool overlay"),
            "{ancestor}"
        );
    }

    #[test]
    fn unseen_link_blames_nobody() {
        let msg = mask_decline(&MaskSource::unseen(s("s1")), &s("s1"), None, "edit");
        assert!(msg.contains("not yet known"), "{msg}");
        assert!(!msg.contains("unknown`"), "no fake agent name: {msg}");
    }

    #[test]
    fn skill_and_disabled_builtin_wording() {
        assert_eq!(
            skill_decline("restricted", "edit"),
            "Declined by skill `restricted`'s allowed_tools — tool `edit` is not listed"
        );
        assert_eq!(
            disabled_builtin_decline("bash"),
            "tool `bash` is disabled — enable with /enable tool bash"
        );
    }
}
