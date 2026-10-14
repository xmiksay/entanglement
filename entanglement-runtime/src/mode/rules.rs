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
//! **Precedence** (ADR-0228): a rule naming the tool outranks a
//! capability-class rule; a `*` rule competes with a named rule by key
//! length and with a class rule by strictness (the stricter wins); within
//! each kind the longest key wins; ties go to the stricter grade. Named over
//! class because a multi-capability tool — `rhai` is read+write+exec —
//! would otherwise be decided by whichever class key happens to be longer
//! (`write`, 5 chars, beats a bare `rhai`, 4), so no mode could ask for
//! scripts while allowing writes. `*` vs class by strictness because the
//! tuning guard cannot see what a wildcard reaches, so a long `*(…)` allow
//! must not out-rank a class deny; `*` vs named keeps length, so
//! `deny: ["*(*credentials*)"]` still beats `bash(cat*)`. A key spelling a
//! tool's own name counts as naming it even when it is also a class (`read`
//! for the `read` tool).
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

    /// The keys of every class-`deny` rule (`write`, `plan`, …).
    pub(super) fn denied_class_keys(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter(|e| e.class.is_some() && e.grade == Permission::Deny)
            .map(|e| e.key.as_str())
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
pub(crate) fn capability_class(key: &str) -> Option<Capability> {
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
/// `default`: the best matching key wins ([`best_match`]), ties break to the more
/// restrictive grade, no match falls through to `default`.
fn resolve_single(
    rules: &Rules,
    default: Permission,
    tool_name: &str,
    capabilities: &[Capability],
    arg: Option<&str>,
    workdir: Option<&str>,
) -> Permission {
    best_match(rules, tool_name, capabilities, arg, workdir).unwrap_or(default)
}

/// The grade of the longest rule for literal `tool` that is argument- or
/// workdir-scoped and matches `arg`; `None` when none does. ADR-0227's
/// "explicit `http(...)` rule" test: a bare tool or class entry never counts.
/// `skip` drops entries by key (the project layer's, which must not unlock).
pub(crate) fn scoped_grade(
    rules: &Rules,
    tool: &str,
    arg: Option<&str>,
    skip: impl Fn(&str) -> bool,
) -> Option<Permission> {
    let scoped = Rules {
        entries: rules
            .entries
            .iter()
            .filter(|e| {
                e.class.is_none()
                    && rule_tool_name(&e.key) == tool
                    && e.key != tool
                    && !skip(&e.key)
            })
            .cloned()
            .collect(),
    };
    best_match(&scoped, tool, &[], arg, None)
}

/// The precedence in one sentence — what `skutter inspect modes` prints.
pub const PRECEDENCE: &str = "A rule naming the tool outranks a capability-class rule; a `*` \
    rule competes with a named rule by key length and with a class rule by strictness (the \
    stricter wins); within each kind the longest key wins; ties go to the stricter grade.";

/// Which kind of key matched, for ranking (module doc).
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

/// The winning grade (module doc): with a rule naming the tool, it and the
/// best `*` rule compete by key length (ties to the stricter grade) and
/// class rules are ignored; without one, the best class rule and the best
/// `*` rule compete by strictness. Within a kind the longest key wins, ties
/// to the stricter grade.
fn best_match(
    rules: &Rules,
    tool_name: &str,
    capabilities: &[Capability],
    arg: Option<&str>,
    workdir: Option<&str>,
) -> Option<Permission> {
    let mut best: [Option<(usize, Permission)>; 3] = [None; 3];
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
        let len = entry.key.chars().count();
        *slot = Some(match *slot {
            None => (len, entry.grade),
            Some((best_len, _)) if len > best_len => (len, entry.grade),
            Some((best_len, best_grade)) if len == best_len => {
                (best_len, most_restrictive(best_grade, entry.grade))
            }
            Some(existing) => existing,
        });
    }
    let [named, class, glob] = best;
    match (named, glob) {
        // Named vs `*`: by key length, ties to the stricter grade. A class
        // rule never decides once a rule names the tool.
        (Some((nl, ng)), Some((gl, gg))) => Some(match nl.cmp(&gl) {
            std::cmp::Ordering::Greater => ng,
            std::cmp::Ordering::Less => gg,
            std::cmp::Ordering::Equal => most_restrictive(ng, gg),
        }),
        (Some((_, ng)), None) => Some(ng),
        // No named rule: class vs `*` by strictness.
        (None, glob) => match (class, glob) {
            (Some((_, cg)), Some((_, gg))) => Some(most_restrictive(cg, gg)),
            (c, g) => c.or(g).map(|(_, grade)| grade),
        },
    }
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
    fn scoped_grade_sees_only_argument_scoped_rules_for_the_tool() {
        let rules = Rules::from_lists(
            &["http(DELETE *)".to_string()],
            &[
                "exec".to_string(),
                "http".to_string(),
                "http(GET https://a.io/*)".to_string(),
            ],
            &[],
        );
        let g = |arg: &str| scoped_grade(&rules, "http", Some(arg), |_| false);
        assert_eq!(g("GET https://a.io/x"), Some(Permission::Allow));
        assert_eq!(g("DELETE https://a.io/x"), Some(Permission::Deny));
        assert_eq!(
            g("POST https://a.io/x"),
            None,
            "bare `http`/`exec` never count"
        );
    }

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

    /// ADR-0228: a rule naming the tool outranks a longer class rule, so a
    /// mode can ask for scripts while allowing writes; a tool-name glob
    /// still competes by length, and a scoped key keeps beating a bare one.
    #[test]
    fn a_rule_naming_the_tool_outranks_a_class_rule() {
        let caps = [Capability::Read, Capability::Write, Capability::Exec];
        let rules = Rules::from_lists(
            &[],
            &[
                "write".to_string(),
                "*".to_string(),
                "rhai(ok/*)".to_string(),
            ],
            &["rhai".to_string()],
        );
        let g = |arg| resolve(&rules, Permission::Deny, "rhai", &caps, arg, None);
        assert_eq!(g(None), Permission::Ask);
        assert_eq!(g(Some("ok/x.rhai")), Permission::Allow);
        assert_eq!(g(Some("other/x.rhai")), Permission::Ask);
        let glob = Rules::from_lists(&["write".to_string()], &["*".to_string()], &[]);
        assert_eq!(
            resolve(
                &glob,
                Permission::Allow,
                "edit",
                &[Capability::Write],
                None,
                None
            ),
            Permission::Deny,
            "a bare `*` never outranks a class deny"
        );
    }

    /// ADR-0228's precedence for `*` rules: against a named rule by key
    /// length (both directions), against a class rule by strictness.
    #[test]
    fn a_star_rule_competes_by_length_with_named_and_by_strictness_with_class() {
        let exec = [Capability::Exec];
        let r = |deny: &[&str], allow: &[&str]| {
            let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            Rules::from_lists(&l(deny), &l(allow), &[])
        };
        // Regression (master behaviour): a long `*` deny beats a shorter
        // named allow.
        let creds = r(&["*(*credentials*)"], &["bash(cat*)"]);
        let g = |rules: &Rules, tool, caps: &[Capability], arg| {
            resolve(rules, Permission::Ask, tool, caps, arg, None)
        };
        assert_eq!(
            g(&creds, "bash", &exec, Some("cat ~/.aws/credentials")),
            Permission::Deny
        );
        assert_eq!(
            g(&creds, "bash", &exec, Some("cat notes")),
            Permission::Allow
        );
        // The other direction: a longer named rule beats a shorter `*`.
        let named = r(&["*(cat*)"], &["bash(cat notes*)"]);
        assert_eq!(
            g(&named, "bash", &exec, Some("cat notes.md")),
            Permission::Allow
        );
        // `deny: ["*"]` + `allow: [read]`: the `read` tool is named by `read`
        // (4 > 1) and runs; `grep` only shares the class, which loses to the
        // stricter `*` deny.
        let star = r(&["*"], &["read"]);
        assert_eq!(
            g(&star, "read", &[Capability::Read], None),
            Permission::Allow
        );
        assert_eq!(
            g(&star, "grep", &[Capability::Read], None),
            Permission::Deny
        );
        // A long scoped `*` allow loses to a class deny.
        let class = r(&["write"], &["*(scripts/*)", "exec"]);
        assert_eq!(
            g(&class, "edit", &[Capability::Write], Some("scripts/x")),
            Permission::Deny
        );
        // And a `*` deny still beats a class allow.
        let curl = r(&["*(curl *)"], &["exec"]);
        assert_eq!(g(&curl, "call", &exec, Some("curl x")), Permission::Deny);
        assert_eq!(g(&curl, "call", &exec, Some("ls")), Permission::Allow);
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

    /// ADR-0232: a rule naming a non-maskable tool never matches — the plan
    /// class stays `propose_plan`'s only gate, both ways, and a `*` rule
    /// still reaches it.
    #[test]
    fn a_rule_naming_propose_plan_is_inert_its_class_decides() {
        let plan = [Capability::Plan];
        let g = |deny: &[&str], allow: &[&str]| {
            let s = |l: &[&str]| l.iter().map(|k| k.to_string()).collect::<Vec<_>>();
            let rules = Rules::from_lists(&s(deny), &s(allow), &[]);
            resolve(&rules, Permission::Ask, "propose_plan", &plan, None, None)
        };
        assert_eq!(g(&["plan"], &["propose_plan"]), Permission::Deny);
        assert_eq!(g(&["propose_plan"], &["plan"]), Permission::Allow);
        assert_eq!(
            g(&["propose_plan"], &[]),
            Permission::Ask,
            "falls to default"
        );
        assert_eq!(
            g(&["*"], &["plan"]),
            Permission::Deny,
            "`*` still reaches it"
        );
        // Every other tool keeps its named rules.
        let rules = Rules::from_lists(&["agent".to_string()], &["control".to_string()], &[]);
        let control = [Capability::Control];
        assert_eq!(
            resolve(&rules, Permission::Ask, "agent", &control, None, None),
            Permission::Deny
        );
    }

    /// ADR-0232: the built-ins' `control` allow must not widen a tool that
    /// also carries another class — it follows that class instead.
    #[test]
    fn the_control_class_only_matches_a_pure_control_tool() {
        let research = Rules::from_lists(
            &["write".to_string(), "plan".to_string()],
            &["read".to_string(), "control".to_string()],
            &[],
        );
        let g = |rules: &Rules, caps: &[Capability]| {
            resolve(rules, Permission::Ask, "mixed", caps, None, None)
        };
        let write_control = [Capability::Write, Capability::Control];
        let exec_control = [Capability::Exec, Capability::Control];
        assert_eq!(g(&research, &write_control), Permission::Deny);
        assert_eq!(
            g(&research, &exec_control),
            Permission::Ask,
            "exec: default"
        );
        let plan = Rules::from_lists(
            &["write".to_string()],
            &[
                "read".to_string(),
                "plan".to_string(),
                "control".to_string(),
            ],
            &[],
        );
        assert_eq!(g(&plan, &exec_control), Permission::Ask);
        assert_eq!(g(&plan, &[Capability::Control]), Permission::Allow);
        let build = Rules::from_lists(&[], &["exec".to_string(), "control".to_string()], &[]);
        assert_eq!(g(&build, &exec_control), Permission::Allow);
    }
}
