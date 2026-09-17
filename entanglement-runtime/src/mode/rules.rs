//! Grade-keyed rule resolution (ADR-0207 §4).
//!
//! A mode's rule list mixes two kinds of entry: a bare capability-class name
//! (`read`/`write`/`exec`/`plan`/`control`, matching every tool that
//! declares that [`Capability`]) and a tool rule (a bare tool name, or the
//! `tool(pattern)`/`tool{pattern}` argument-/workdir-scoped grammar,
//! ADR-0051/ADR-0116). [`Rules::from_lists`] classifies each raw string once
//! at construction so [`resolve`] never re-parses a key on the hot path.
//!
//! The `tool(pattern)`/`tool{pattern}` glob grammar itself is
//! `entanglement_core::protocol`'s (`split_rule_key`/`glob_match`), but both
//! are private to that module — a separate crate can't call them directly.
//! Reimplementing the `*`/`?` wildcard semantics here would be exactly the
//! second matcher ADR-0207 exists to remove, so [`tool_rule_matches`]
//! borrows the tested matcher through the one door core does expose it
//! behind: a single-rule [`PermissionProfile`], whose public
//! `resolve_scoped` already *is* that matcher. Only the trivial "does this
//! key carry a `(...)`/`{...}` suffix" classification (needed to rank scoped
//! rules above bare ones, tier 1 below) is duplicated locally — it decides
//! nothing about *whether* a pattern matches, only which tier a key
//! competes in.

use entanglement_core::{Permission, PermissionProfile};

use crate::capability::Capability;

/// One mode's rule table, already split into the four buckets a call
/// resolves against (ADR-0207 §4): capability-class allow/deny (bare
/// `read`/`write`/`exec`/`plan`/`control` entries) and tool-rule allow/deny
/// (a bare tool name, `tool(pattern)`, or `tool{pattern}`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rules {
    pub deny_classes: Vec<Capability>,
    pub allow_classes: Vec<Capability>,
    pub deny_tools: Vec<String>,
    pub allow_tools: Vec<String>,
}

impl Rules {
    /// Build from the raw YAML `deny`/`allow` lists (ADR-0207 §4's flat
    /// grade-keyed shape), classifying each entry once: a bare key spelled
    /// exactly like one of the five capability classes is the class, never
    /// the same-named literal tool.
    pub fn from_lists(deny: &[String], allow: &[String]) -> Self {
        let mut rules = Rules::default();
        for key in deny {
            match capability_class(key) {
                Some(class) => rules.deny_classes.push(class),
                None => rules.deny_tools.push(key.clone()),
            }
        }
        for key in allow {
            match capability_class(key) {
                Some(class) => rules.allow_classes.push(class),
                None => rules.allow_tools.push(key.clone()),
            }
        }
        rules
    }
}

/// A bare (unscoped) key spelled exactly like one of the five capability
/// classes always names the class, never the same-named literal tool
/// (`read`, `write` are both class names *and* real registered tools) —
/// mirroring the old ADR-0114 capability-key fan-out convention, so a
/// class-wide rule doesn't need to be spelled once per tool it covers. A
/// *scoped* key (`write(pattern)`) always names the literal tool instead,
/// since scoping is only defined for tools here (ADR-0207 §4 lists no
/// class-scoped grammar) — that's the one way to write a rule for the
/// literal `write`/`read`/`exec`/`plan`/`control`-named tool, and it's what
/// the built-in `plan` mode uses for its plans-folder carve-out.
pub(super) fn capability_class(key: &str) -> Option<Capability> {
    match key {
        "read" => Some(Capability::Read),
        "write" => Some(Capability::Write),
        "exec" => Some(Capability::Exec),
        "plan" => Some(Capability::Plan),
        "control" => Some(Capability::Control),
        _ => None,
    }
}

/// Whether `key` carries an argument or workdir scope
/// (`tool(pattern)`/`tool{pattern}`) rather than matching bare/`*`. Mirrors
/// only the classification half of core's private `split_rule_key` — see
/// the module doc for why the matching half stays delegated.
fn is_scoped(key: &str) -> bool {
    if key.find('(').is_some() && key.ends_with(')') {
        return true;
    }
    if key.find('{').is_some() && key.ends_with('}') {
        return true;
    }
    false
}

/// Does tool-rule key `key` match this call? Delegates to a single-rule
/// [`PermissionProfile`] so the real glob/scope matcher (ADR-0051/ADR-0116)
/// runs unmodified: a profile whose only rule is `key => Allow` under a
/// `Deny` default resolves to `Allow` iff `key` matches.
fn tool_rule_matches(key: &str, name: &str, arg: Option<&str>, workdir: Option<&str>) -> bool {
    PermissionProfile::new(Permission::Deny)
        .with(key, Permission::Allow)
        .resolve_scoped(name, arg, workdir)
        == Permission::Allow
}

/// Resolve the grade for one call under a mode's `default` + [`Rules`]
/// (ADR-0207 §4). Order:
///
/// 1. an explicit tool rule naming the tool wins — scoped
///    (`tool(...)`/`tool{...}`) before bare, and at equal specificity `deny`
///    before `allow` (`deny` is absolute, ADR-0207 §4);
/// 2. else a capability-class `deny`;
/// 3. else a capability-class `allow`;
/// 4. else the mode's `default`.
pub fn resolve(
    rules: &Rules,
    default: Permission,
    tool_name: &str,
    capabilities: &[Capability],
    arg: Option<&str>,
    workdir: Option<&str>,
) -> Permission {
    // Tier 1: explicit tool rules, scoped tier first, deny before allow.
    for scoped_tier in [true, false] {
        let deny_hit = rules.deny_tools.iter().any(|key| {
            is_scoped(key) == scoped_tier && tool_rule_matches(key, tool_name, arg, workdir)
        });
        if deny_hit {
            return Permission::Deny;
        }
        let allow_hit = rules.allow_tools.iter().any(|key| {
            is_scoped(key) == scoped_tier && tool_rule_matches(key, tool_name, arg, workdir)
        });
        if allow_hit {
            return Permission::Allow;
        }
    }

    // Tier 2/3: capability class, deny absolute over allow.
    if capabilities.iter().any(|c| rules.deny_classes.contains(c)) {
        return Permission::Deny;
    }
    if capabilities.iter().any(|c| rules.allow_classes.contains(c)) {
        return Permission::Allow;
    }

    default
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_scoped_tool_rule_beats_bare_class_deny() {
        let rules = Rules::from_lists(
            &["write".to_string()],
            &["write(.entanglement/plans/*.md)".to_string()],
        );
        assert_eq!(
            resolve(
                &rules,
                Permission::Ask,
                "write",
                &[Capability::Write],
                Some(".entanglement/plans/phase1.md"),
                None
            ),
            Permission::Allow,
            "the scoped allow must win over the class deny"
        );
        // Same tool, an arg the scoped rule doesn't cover: falls through to
        // the bare class deny.
        assert_eq!(
            resolve(
                &rules,
                Permission::Ask,
                "write",
                &[Capability::Write],
                Some("README.md"),
                None
            ),
            Permission::Deny
        );
    }

    #[test]
    fn scoped_beats_bare_at_the_explicit_tool_tier() {
        let rules = Rules::from_lists(&["bash(rm *)".to_string()], &["bash".to_string()]);
        // `bash` alone is bare-allowed, but `bash(rm *)` is scoped-denied —
        // scoped must win regardless of declaration order.
        assert_eq!(
            resolve(
                &rules,
                Permission::Ask,
                "bash",
                &[Capability::Exec],
                Some("rm -rf x"),
                None
            ),
            Permission::Deny
        );
        assert_eq!(
            resolve(
                &rules,
                Permission::Ask,
                "bash",
                &[Capability::Exec],
                Some("ls"),
                None
            ),
            Permission::Allow
        );
    }

    #[test]
    fn class_deny_beats_class_allow() {
        let rules = Rules::from_lists(&["write".to_string()], &["write".to_string()]);
        assert_eq!(
            resolve(
                &rules,
                Permission::Ask,
                "edit",
                &[Capability::Write],
                None,
                None
            ),
            Permission::Deny
        );
    }

    #[test]
    fn unmatched_call_falls_through_to_default() {
        let rules = Rules::default();
        assert_eq!(
            resolve(
                &rules,
                Permission::Ask,
                "bash",
                &[Capability::Exec],
                Some("curl x"),
                None
            ),
            Permission::Ask
        );
    }

    #[test]
    fn bare_class_key_never_matches_the_same_named_literal_tool_as_a_tool_rule() {
        // "write" in a deny list is the class, not a scoped-vs-bare tool
        // rule — so it must show up in deny_classes, not deny_tools.
        let rules = Rules::from_lists(&["write".to_string()], &[]);
        assert_eq!(rules.deny_classes, vec![Capability::Write]);
        assert!(rules.deny_tools.is_empty());
    }
}
