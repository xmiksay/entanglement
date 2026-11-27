//! What a [`PermissionResolver`][super::PermissionResolver] answers for one
//! call (#964): a [`Permission`] whose `Deny` may carry the reason the model
//! is told — so a resolver that vetoes for its own reasons (`skutter`'s
//! `pre_tool_use` hook, an embedder's tenant policy) is not reported as a
//! mode deny with `/mode` advice that cannot help — and whose `Ask` may
//! carry the reason the approval card shows (#968: `skutter`'s escape-root
//! check asks for a path outside the project root).

use entanglement_core::{CardReason, Permission};

/// One call's grade from a resolver. A resolver that grades by mode alone
/// returns `permission.into()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grade {
    /// Run immediately, no approval request.
    Allow,
    /// Park an approval card and wait for the user. `Some(reason)` is why
    /// the resolver asks beyond the mode's own grade: the card says so, and
    /// silence never approves it.
    Ask(Option<AskReason>),
    /// Refuse outright. `Some(reason)` reaches the model verbatim as the
    /// call's error result; `None` gets the executor's mode-deny text.
    Deny(Option<String>),
}

/// Why a resolver asks: the card's reason line and the structured reason a
/// head decides on (ADR-0230).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskReason {
    pub text: String,
    pub card: CardReason,
}

impl Grade {
    /// A `Deny` the model is told about in `reason`'s words.
    pub fn deny(reason: impl Into<String>) -> Self {
        Grade::Deny(Some(reason.into()))
    }

    /// An `Ask` whose card says `text` and carries `card`.
    pub fn ask(text: impl Into<String>, card: CardReason) -> Self {
        Grade::Ask(Some(AskReason {
            text: text.into(),
            card,
        }))
    }

    /// The grade without its reason.
    pub fn permission(&self) -> Permission {
        match self {
            Grade::Allow => Permission::Allow,
            Grade::Ask(_) => Permission::Ask,
            Grade::Deny(_) => Permission::Deny,
        }
    }
}

impl From<Permission> for Grade {
    fn from(permission: Permission) -> Self {
        match permission {
            Permission::Allow => Grade::Allow,
            Permission::Ask => Grade::Ask(None),
            Permission::Deny => Grade::Deny(None),
        }
    }
}

/// The refusal text the model gets for a `Deny` of `tool` under `mode`: the
/// resolver's own reason when it gave one, else the mode-deny text that
/// names the way out (ADR-0207 §4).
pub(crate) fn deny_text(tool: &str, mode: &str, reason: Option<String>) -> String {
    reason.unwrap_or_else(|| format!("tool `{tool}` denied by mode `{mode}` — use /mode to switch"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_permission_round_trips_without_a_reason() {
        for p in [Permission::Allow, Permission::Ask, Permission::Deny] {
            assert_eq!(Grade::from(p).permission(), p);
        }
        assert_eq!(Grade::deny("no").permission(), Permission::Deny);
        assert_eq!(
            Grade::ask("why", CardReason::RootEscape).permission(),
            Permission::Ask
        );
        assert_eq!(Grade::from(Permission::Deny), Grade::Deny(None));
        assert_eq!(Grade::from(Permission::Ask), Grade::Ask(None));
    }

    #[test]
    fn a_reason_replaces_the_mode_deny_text() {
        assert_eq!(
            deny_text("sh", "plan", None),
            "tool `sh` denied by mode `plan` — use /mode to switch"
        );
        assert_eq!(deny_text("sh", "plan", Some("vetoed".into())), "vetoed");
    }
}
