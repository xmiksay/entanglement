//! Ranking the rules that match one call (ADR-0207 §4, ADR-0228 precedence,
//! [`super::PRECEDENCE`]): which single entry wins, so the caller learns
//! both the grade and the rule behind it (ADR-0247).

use entanglement_core::Permission;

use super::{canonical_tool, canonicalize_key, rule_tool_name, tool_rule_matches, Entry};
use crate::capability::Capability;

/// Which kind of key matched, for ranking (module doc of `rules`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The key's tool part is the call's tool itself (bare or scoped).
    Named,
    /// A capability class (`read`/`write`/...).
    Class,
    /// The `*` tool wildcard, bare or scoped (`*`, `*(scripts/*)`).
    Glob,
}

/// A key spelling the tool's own name counts as naming it even when it is
/// also a class (`read` for the `read` tool); for any other tool it is the
/// class.
fn kind(entry: &Entry, tool: &str) -> Kind {
    if canonical_tool(rule_tool_name(&canonicalize_key(&entry.key))) == canonical_tool(tool) {
        Kind::Named
    } else if entry.class.is_some() {
        Kind::Class
    } else {
        Kind::Glob
    }
}

/// Whether `entry`'s key names `tool` (bare or scoped) — not a class or `*`.
pub(super) fn names_tool(entry: &Entry, tool: &str) -> bool {
    kind(entry, tool) == Kind::Named
}

/// Most restrictive of two grades — the equal-length tiebreak:
/// `deny` beats everything, `prompt`/`Ask` beats `allow`.
fn most_restrictive(a: Permission, b: Permission) -> Permission {
    match (a, b) {
        (Permission::Deny, _) | (_, Permission::Deny) => Permission::Deny,
        (Permission::Ask, _) | (_, Permission::Ask) => Permission::Ask,
        _ => Permission::Allow,
    }
}

/// The stricter of two entries (`a` on an equal grade).
fn stricter<'a>(a: &'a Entry, b: &'a Entry) -> &'a Entry {
    if most_restrictive(a.grade, b.grade) == a.grade {
        a
    } else {
        b
    }
}

/// The longer key of two entries, ties to the stricter.
fn longer<'a>(a: &'a Entry, b: &'a Entry) -> &'a Entry {
    match a.key.chars().count().cmp(&b.key.chars().count()) {
        std::cmp::Ordering::Greater => a,
        std::cmp::Ordering::Less => b,
        std::cmp::Ordering::Equal => stricter(a, b),
    }
}

/// The winning entry: with a rule naming the tool, it and the best `*` rule
/// compete by key length (ties to the stricter grade) and class rules are
/// ignored; without one, the best class rule and the best `*` rule compete
/// by strictness. Within a kind the longest key wins, ties to the stricter
/// grade.
pub(super) fn best_entry<'a>(
    rules: &'a super::Rules,
    tool_name: &str,
    capabilities: &[Capability],
    arg: Option<&str>,
    workdir: Option<&str>,
) -> Option<&'a Entry> {
    let mut best: [Option<&Entry>; 3] = [None; 3];
    for entry in &rules.entries {
        let matches = match entry.class {
            // `control` reaches only a pure orchestration tool (ADR-0232):
            // as the longest class key it would otherwise out-rank `write`/
            // `exec` for a mixed tool and widen it past the mode's deny.
            Some(Capability::Control) => capabilities == [Capability::Control],
            Some(class) => capabilities.contains(&class),
            None => tool_rule_matches(&entry.key, tool_name, arg, workdir),
        };
        if !matches {
            continue;
        }
        let entry_kind = kind(entry, tool_name);
        // A rule naming a non-maskable tool is inert (ADR-0232): only its
        // class (`propose_plan`'s `plan`) or a `*` rule grades it, which is
        // what the config warning (`config::mode_warn`) promises.
        if entry_kind == Kind::Named && crate::tool_names::is_non_maskable(tool_name) {
            continue;
        }
        let slot = &mut best[entry_kind as usize];
        *slot = Some(slot.map_or(entry, |current| longer(current, entry)));
    }
    let [named, class, glob] = best;
    match (named, glob) {
        // Named vs `*`: by key length, ties to the stricter grade. A class
        // rule never decides once a rule names the tool.
        (Some(n), Some(g)) => Some(longer(n, g)),
        (Some(n), None) => Some(n),
        // No named rule: class vs `*` by strictness.
        (None, glob) => match (class, glob) {
            (Some(c), Some(g)) => Some(stricter(c, g)),
            (c, g) => c.or(g),
        },
    }
}
