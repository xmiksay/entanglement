//! Grade-keyed rule resolution (ADR-0207 §4, revised): **longest match
//! wins**. Not a tier order, not first-or-last-declared — among every rule
//! that matches a call, the one with the longest key (measured in
//! characters) decides the grade, so `write(docs/*)` (13 chars) out-ranks
//! bare `write` (5 chars) with no notion of "scoped beats bare" needed as a
//! separate rule. Capability-class entries (`read`/`write`/`exec`/`plan`/
//! `control`) compete on the same footing as tool rules — that's the whole
//! point: a reader can determine the outcome by inspection without knowing
//! an evaluation order.
//!
//! **Tie-break** (equal key length): the more restrictive grade wins —
//! `deny` > `prompt` (core's `Permission::Ask`) > `allow`. The task this
//! module implements didn't pin a tiebreak; this file does, deliberately,
//! so two rules of equal specificity never depend on declaration order.
//!
//! **`bash`/`call` share one rule set** — they're two spellings of the same
//! `Exec` capability, so a rule written for either grades both
//! ([`canonicalize_key`]/[`canonical_tool`]). Compound commands
//! (`&&`/`||`/`;`/`|`/`&`, ADR-0197) are split and graded per segment for
//! both tools; a command the splitter can't fully account for
//! ([`crate::shell_split::SplitOutcome::Opaque`]) grades at the mode's
//! `default` outright — no whole-string fallback guess, unlike the
//! `bash`-only legacy resolver in `permission_bash.rs`.
//!
//! The `tool(pattern)`/`tool{pattern}` glob grammar itself is
//! `entanglement_core::protocol`'s (`split_rule_key`/`glob_match`), private
//! to that module — a separate crate can't call it directly. Reimplementing
//! the `*`/`?` wildcard semantics here would be exactly the second matcher
//! ADR-0207 exists to remove, so [`tool_rule_matches`] borrows the tested
//! matcher through the one door core does expose it behind: a single-rule
//! [`PermissionProfile`], whose public `resolve_scoped` already *is* that
//! matcher.

use entanglement_core::{Permission, PermissionProfile};

use crate::capability::Capability;
use crate::permission::min_permission;
use crate::shell_split::{self, SplitOutcome};

/// One graded entry: the raw key exactly as written (its character count is
/// its specificity), the grade it carries, and — for a bare capability-class
/// key — which class it names, so [`resolve_single`] can test it against a
/// call's capability slice instead of its tool name.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    key: String,
    grade: Permission,
    class: Option<Capability>,
}

/// One mode's rule table: every `deny`/`allow`/`prompt` entry from the YAML,
/// in one flat list ranked at resolution time by key length (ADR-0207 §4).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rules {
    entries: Vec<Entry>,
}

impl Rules {
    /// Build from the raw YAML `deny`/`allow`/`prompt` lists (ADR-0207 §4's
    /// three grade-keyed lists).
    pub fn from_lists(deny: &[String], allow: &[String], prompt: &[String]) -> Self {
        let mut rules = Rules::default();
        for key in deny {
            rules.push(key, Permission::Deny);
        }
        for key in allow {
            rules.push(key, Permission::Allow);
        }
        for key in prompt {
            rules.push(key, Permission::Ask);
        }
        rules
    }

    /// Add one rule. Tuning only ever calls this — there is no removal
    /// syntax (ADR-0207 §5): longest-match already makes a shipped rule
    /// overridable by adding a longer, more specific one.
    pub(super) fn push(&mut self, key: &str, grade: Permission) {
        self.entries.push(Entry {
            key: key.to_string(),
            grade,
            class: capability_class(key),
        });
    }

    /// Whether `class` carries an exact (unscoped) `deny` entry — the
    /// tuning guard's question (ADR-0207 §5): is this mode's class-deny
    /// still standing, not what the longest match currently resolves to.
    pub(super) fn class_is_denied(&self, class: Capability) -> bool {
        self.entries
            .iter()
            .any(|e| e.class == Some(class) && e.grade == Permission::Deny)
    }

    /// Every rule as `(key, grade)`, in declaration order — display-only
    /// (`skutter inspect modes`, ADR-0207 stage 6c): longest-match doesn't
    /// care about this order at resolution time ([`resolve`]), but a reader
    /// comparing a mode's built-in shape against its tuned one wants to see
    /// what was actually added, in the order it was added.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, Permission)> {
        self.entries.iter().map(|e| (e.key.as_str(), e.grade))
    }
}

/// A bare (unscoped) key spelled exactly like one of the five capability
/// classes always names the class, never the same-named literal tool
/// (`read`, `write` are both class names *and* real registered tools) —
/// mirroring the old ADR-0114 capability-key fan-out convention. A *scoped*
/// key (`write(pattern)`) always names the literal tool instead, since
/// scoping is only defined for tools here (ADR-0207 §4 lists no
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

/// The tool name a rule key names: everything before an argument (`(`) or
/// workdir (`{`) scope, or the whole key if unscoped. Used by the tuning
/// guard (`tune.rs`) to resolve a scoped key like `write(*)` back to the
/// literal tool it grants, and by `config::mode_warn`'s stale-tuning-name
/// check (crate-visible for that second, cross-module consumer) to extract
/// the same tool part from a rule key like `bash(cargo check)` before
/// validating it against the compile-time vocabulary.
pub(crate) fn rule_tool_name(key: &str) -> &str {
    let cut = key.find(['(', '{']).unwrap_or(key.len());
    &key[..cut]
}

/// `bash` and `call` are two spellings of the same `Exec` capability
/// (ADR-0207 §4) — canonicalize `call` to `bash` in both a rule key's tool
/// part and the incoming tool name so a rule written for either grades a
/// call to both.
fn canonical_tool(name: &str) -> &str {
    if name == "call" {
        "bash"
    } else {
        name
    }
}

fn canonicalize_key(key: &str) -> String {
    let tool = rule_tool_name(key);
    if tool == "call" {
        format!("bash{}", &key[tool.len()..])
    } else {
        key.to_string()
    }
}

/// Does tool-rule key `key` match this call? Delegates to a single-rule
/// [`PermissionProfile`] so the real glob/scope matcher (ADR-0051/ADR-0116)
/// runs unmodified: a profile whose only rule is `key => Allow` under a
/// `Deny` default resolves to `Allow` iff `key` matches.
fn tool_rule_matches(key: &str, name: &str, arg: Option<&str>, workdir: Option<&str>) -> bool {
    let key = canonicalize_key(key);
    let name = canonical_tool(name);
    PermissionProfile::new(Permission::Deny)
        .with(&key, Permission::Allow)
        .resolve_scoped(name, arg, workdir)
        == Permission::Allow
}

/// Most restrictive of two grades — the equal-length tiebreak (module doc):
/// `deny` beats everything, `prompt`/`Ask` beats `allow`.
fn most_restrictive(a: Permission, b: Permission) -> Permission {
    match (a, b) {
        (Permission::Deny, _) | (_, Permission::Deny) => Permission::Deny,
        (Permission::Ask, _) | (_, Permission::Ask) => Permission::Ask,
        _ => Permission::Allow,
    }
}

/// Resolve one call (no compound-command splitting) against `rules` +
/// `default`: the longest matching key wins, ties break to the more
/// restrictive grade, no match falls through to `default`.
fn resolve_single(
    rules: &Rules,
    default: Permission,
    tool_name: &str,
    capabilities: &[Capability],
    arg: Option<&str>,
    workdir: Option<&str>,
) -> Permission {
    let mut best: Option<(usize, Permission)> = None;
    for entry in &rules.entries {
        let matches = match entry.class {
            Some(class) => capabilities.contains(&class),
            None => tool_rule_matches(&entry.key, tool_name, arg, workdir),
        };
        if !matches {
            continue;
        }
        let len = entry.key.chars().count();
        best = Some(match best {
            None => (len, entry.grade),
            Some((best_len, _)) if len > best_len => (len, entry.grade),
            Some((best_len, best_grade)) if len == best_len => {
                (best_len, most_restrictive(best_grade, entry.grade))
            }
            Some(existing) => existing,
        });
    }
    best.map_or(default, |(_, grade)| grade)
}

/// Per-segment grading for `bash`/`call` (ADR-0197, extended to `call` by
/// ADR-0207 §4): split on top-level `&&`/`||`/`;`/`|`/`&`, grade each
/// segment independently and fold to the most restrictive result — any
/// segment `Deny` denies the whole command, all-`Allow` is `Allow`,
/// otherwise the fold lands on the most restrictive non-deny grade among
/// the segments. A simple command splits into exactly one segment equal to
/// the whole string, so this reduces to a single [`resolve_single`] call
/// for every non-compound call.
///
/// [`SplitOutcome::Opaque`] grades at `default` directly — a construct the
/// splitter can't fully account for is never graded on a guess (ADR-0207
/// §4), so unlike the bash-only legacy resolver in `permission_bash.rs`
/// (which re-resolved with the argument dropped, still consulting bare/
/// workdir rules), a mode's `default` already *is* the safe fallback here.
fn resolve_compound(
    rules: &Rules,
    default: Permission,
    tool_name: &str,
    capabilities: &[Capability],
    command: &str,
    workdir: Option<&str>,
) -> Permission {
    match shell_split::split(command) {
        SplitOutcome::Opaque => default,
        SplitOutcome::Segments(segments) => segments
            .iter()
            .try_fold(Permission::Allow, |acc, seg| {
                let grade =
                    resolve_single(rules, default, tool_name, capabilities, Some(seg), workdir);
                if grade == Permission::Deny {
                    None
                } else {
                    Some(min_permission(acc, grade))
                }
            })
            .unwrap_or(Permission::Deny),
    }
}

/// Resolve the grade for one call under a mode's `default` + [`Rules`]
/// (ADR-0207 §4). `bash`/`call` calls with a command argument grade per
/// compound segment ([`resolve_compound`]); everything else resolves in one
/// shot ([`resolve_single`]).
pub fn resolve(
    rules: &Rules,
    default: Permission,
    tool_name: &str,
    capabilities: &[Capability],
    arg: Option<&str>,
    workdir: Option<&str>,
) -> Permission {
    if matches!(tool_name, "bash" | "call") {
        if let Some(command) = arg {
            return resolve_compound(rules, default, tool_name, capabilities, command, workdir);
        }
    }
    resolve_single(rules, default, tool_name, capabilities, arg, workdir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longer_scoped_tool_rule_beats_shorter_class_deny() {
        let rules = Rules::from_lists(
            &["write".to_string()],
            &["write(.entanglement/plans/*.md)".to_string()],
            &[],
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
            "the longer scoped allow must win over the shorter class deny"
        );
        // Same tool, an arg the scoped rule doesn't cover: only the class
        // deny matches, so it applies.
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
    fn longer_scoped_deny_beats_shorter_bare_allow() {
        let rules = Rules::from_lists(&["bash(rm *)".to_string()], &["bash".to_string()], &[]);
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
    fn equal_length_tie_deny_beats_allow() {
        let rules = Rules::from_lists(&["write".to_string()], &["write".to_string()], &[]);
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
    fn equal_length_tie_deny_beats_prompt() {
        let rules = Rules::from_lists(&["write".to_string()], &[], &["write".to_string()]);
        assert_eq!(
            resolve(
                &rules,
                Permission::Allow,
                "edit",
                &[Capability::Write],
                None,
                None
            ),
            Permission::Deny
        );
    }

    #[test]
    fn equal_length_tie_prompt_beats_allow() {
        let rules = Rules::from_lists(&[], &["write".to_string()], &["write".to_string()]);
        assert_eq!(
            resolve(
                &rules,
                Permission::Deny,
                "edit",
                &[Capability::Write],
                None,
                None
            ),
            Permission::Ask
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
    fn bare_class_key_denies_every_tool_with_that_capability_not_just_the_literal_name() {
        // "write" in a deny list must be the class, matching any tool that
        // declares Capability::Write (here: "edit"), not a tool-rule keyed
        // to a literal tool named "write".
        let rules = Rules::from_lists(&["write".to_string()], &[], &[]);
        assert_eq!(
            resolve(
                &rules,
                Permission::Allow,
                "edit",
                &[Capability::Write],
                None,
                None
            ),
            Permission::Deny
        );
    }

    #[test]
    fn call_is_graded_by_a_bash_written_rule() {
        let rules = Rules::from_lists(&[], &["bash(rg *)".to_string()], &[]);
        assert_eq!(
            resolve(
                &rules,
                Permission::Ask,
                "call",
                &[Capability::Exec],
                Some("rg foo"),
                None
            ),
            Permission::Allow,
            "bash and call are one rule set (ADR-0207 §4)"
        );
    }

    #[test]
    fn bash_is_denied_by_a_call_written_rule() {
        let rules = Rules::from_lists(&["call(rm *)".to_string()], &[], &[]);
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
    }

    #[test]
    fn compound_command_grades_per_segment_for_call_too() {
        let rules = Rules::from_lists(
            &[],
            &["bash(find *)".to_string(), "bash(grep *)".to_string()],
            &[],
        );
        assert_eq!(
            resolve(
                &rules,
                Permission::Ask,
                "call",
                &[Capability::Exec],
                Some("find . | grep x"),
                None
            ),
            Permission::Allow
        );
        assert_eq!(
            resolve(
                &rules,
                Permission::Ask,
                "call",
                &[Capability::Exec],
                Some("find . && curl x"),
                None
            ),
            Permission::Ask,
            "the unmatched segment must not ride the find rule through"
        );
    }

    #[test]
    fn deny_on_any_segment_denies_the_whole_compound_command() {
        let rules = Rules::from_lists(
            &["bash(rm *)".to_string()],
            &["bash(find *)".to_string()],
            &[],
        );
        assert_eq!(
            resolve(
                &rules,
                Permission::Ask,
                "bash",
                &[Capability::Exec],
                Some("find . && rm x"),
                None
            ),
            Permission::Deny
        );
    }

    #[test]
    fn unparseable_compound_command_grades_at_default_not_a_guess() {
        let rules = Rules::from_lists(&[], &["bash(find *)".to_string()], &[]);
        assert_eq!(
            resolve(
                &rules,
                Permission::Ask,
                "bash",
                &[Capability::Exec],
                Some("find . > out.txt"),
                None
            ),
            Permission::Ask,
            "output redirection is Opaque to the splitter; must not ride the find allow"
        );
        assert_eq!(
            resolve(
                &rules,
                Permission::Deny,
                "bash",
                &[Capability::Exec],
                Some("find . > out.txt"),
                None
            ),
            Permission::Deny,
            "Opaque grades at whatever `default` the mode has, not a fixed fallback"
        );
    }
}
