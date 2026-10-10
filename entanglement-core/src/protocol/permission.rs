//! Permission grades, rule matching and approval scope.

use serde::{Deserialize, Serialize};

#[cfg(doc)]
use crate::protocol::{Agent, InMsg, OutEvent, ToolOverlayEntry};

/// What the engine does when the model asks to run a host tool. Driven by the
/// session's active [`Agent`] permission profile — e.g. a `plan` profile
/// denies edits, a `build` profile allows everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Run immediately, no approval request.
    Allow,
    /// Emit [`OutEvent::ToolRequest`] and wait for the user.
    Ask,
    /// Refuse outright; the model is told the tool was denied by policy.
    Deny,
}

/// Per-tool permission rules. Evaluated against a tool name **and** an optional
/// tool-specific argument (the command for `bash`/`call`, the target path for
/// `edit`/`write`/`read`, #173) — and, for `bash`/`call`, an optional `workdir`
/// (#425); later matching rules win (so put `"*"` first, specifics after —
/// same semantics as opencode). Every tool — including the runtime's
/// `update_plan`/`update_tasks` state tools (#231, ADR-0049) — resolves
/// through this: there are no engine built-ins that bypass it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionProfile {
    /// `(pattern, permission)` pairs. `pattern` is a tool name, `"*"`, a
    /// tool-with-argument glob `tool(pattern)` (#173), or a tool-with-workdir
    /// glob `tool{pattern}` (#425) — e.g. `bash(git *)`, `edit(src/*)`,
    /// `bash{/tmp/*}`. See [`PermissionProfile::resolve`].
    pub rules: Vec<(String, Permission)>,
    /// Used when no rule matches.
    pub default: Permission,
}

impl PermissionProfile {
    pub fn new(default: Permission) -> Self {
        Self {
            rules: Vec::new(),
            default,
        }
    }

    /// Add a rule (evaluated after previously-added rules on conflict).
    pub fn with(mut self, pattern: impl Into<String>, perm: Permission) -> Self {
        self.rules.push((pattern.into(), perm));
        self
    }

    /// Resolve the permission for a tool call, matching each rule key against
    /// the tool `name` **and** an optional tool-specific `arg` (#173).
    /// Equivalent to [`resolve_scoped`][Self::resolve_scoped] with
    /// `workdir = None` — every `tool{pattern}` workdir-scoped rule (#425)
    /// simply never matches through this entry point. A rule key is one of:
    ///
    /// - `*` or a bare tool name — matches any call to that tool, ignoring the
    ///   argument (the pre-#173 behaviour);
    /// - `tool(pattern)` — matches only when `arg` is `Some` and the `*`/`?`
    ///   glob `pattern` matches it, e.g. `bash(git *)`, `edit(src/*)`.
    ///
    /// Last matching rule wins, so an argument-scoped rule placed after a
    /// coarse one refines it (`bash: ask` then `bash(git status): allow`).
    pub fn resolve(&self, name: &str, arg: Option<&str>) -> Permission {
        self.resolve_scoped(name, arg, None)
    }

    /// Resolve the permission for a tool call, matching each rule key against
    /// the tool `name`, an optional tool-specific `arg` (#173, `tool(pattern)`),
    /// **and** an optional `workdir` (#425, `tool{pattern}`) — the working
    /// directory a `bash`/`call` invocation runs in, distinct from its command
    /// line. Both scopes are independent single-pattern clauses (not a
    /// compound key): a profile mixes `tool(cmd-pattern)` and `tool{workdir-
    /// pattern}` rules freely in one ordered list, and last-matching-rule-wins
    /// applies across the whole list exactly as it does for `resolve`. A tool
    /// with no workdir concept (anything but `bash`/`call`) simply never
    /// matches a `tool{pattern}` rule, since callers pass `workdir = None` for
    /// it — safe by construction, not by convention.
    pub fn resolve_scoped(
        &self,
        name: &str,
        arg: Option<&str>,
        workdir: Option<&str>,
    ) -> Permission {
        let mut result = self.default;
        for (key, p) in &self.rules {
            if rule_matches(key, name, arg, workdir) {
                result = *p;
            }
        }
        result
    }

    /// Name-only resolution: matches `*` and bare-name rules, treating every
    /// argument-scoped `tool(pattern)`/`tool{pattern}` rule as a non-match.
    /// Equivalent to [`resolve`][Self::resolve] with `arg = None`. Kept for
    /// callers that render a profile's coarse per-tool posture without a
    /// concrete call in hand (inspect views, the TUI panel).
    pub fn for_tool(&self, name: &str) -> Permission {
        self.resolve(name, None)
    }

    /// Whether `name` carries an **explicit, unscoped** `Deny` rule keyed to
    /// its own bare name — the profile author's deliberate "never" for this
    /// specific tool, as opposed to falling through to the ambient
    /// [`default`][Self::default] every tool the profile never mentions
    /// resolves to, or to a blanket `*` catch-all. Neither the default nor a
    /// `*`/`tool(pattern)`/`tool{pattern}` rule counts — only a literal
    /// `name` key. This is ADR-0198's hard floor for an out-of-mask call: a
    /// tool the profile never mentions still gets the mask-miss approval
    /// offer even when its ambient default is `deny` (every unlisted tool
    /// would otherwise silently lose the offer); a tool the author
    /// specifically wrote `deny` for does not.
    pub fn explicit_bare_deny(&self, name: &str) -> bool {
        self.rules
            .iter()
            .any(|(pattern, perm)| pattern == name && *perm == Permission::Deny)
    }

    /// The grades a *concrete* call to `name` can reach that its bare grade
    /// ([`for_tool`][Self::for_tool]) does not show, because they sit behind an
    /// argument- or workdir-scoped rule (`tool(pattern)`/`tool{pattern}`,
    /// #173/#425) — e.g. the `plan` profile's `write: deny` +
    /// `write(.entanglement/plans/*.md): allow`, whose bare grade `Deny` is a
    /// half-truth. A UI rendering a profile's per-tool posture needs this to
    /// stay honest; nothing on the dispatch path consults it.
    ///
    /// Each scoped rule is probed by resolving with **its own pattern as the
    /// value** — a `*`/`?` glob always matches itself — so the probe runs the
    /// whole ordered rule list and a later bare rule overriding a scoped one is
    /// reflected. Deduped, in rule order, with the bare grade removed: an empty
    /// result means the bare grade is the whole truth for this tool.
    pub fn scoped_grades(&self, name: &str) -> Vec<Permission> {
        let bare = self.for_tool(name);
        let mut out = Vec::new();
        for (key, _) in &self.rules {
            let (tool_pat, scope) = split_rule_key(key);
            if tool_pat != "*" && tool_pat != name {
                continue;
            }
            let grade = match scope {
                RuleScope::None => continue,
                RuleScope::Arg(pat) => self.resolve(name, Some(pat)),
                RuleScope::Workdir(pat) => self.resolve_scoped(name, None, Some(pat)),
            };
            if grade != bare && !out.contains(&grade) {
                out.push(grade);
            }
        }
        out
    }
}

/// A rule key's argument scope, once split from its tool/capability part
/// (#173, #425): unscoped, a `tool(pattern)` command/path glob, or a
/// `tool{pattern}` workdir glob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleScope<'a> {
    None,
    Arg(&'a str),
    Workdir(&'a str),
}

/// Whether a rule `key` matches a tool `name` + optional `arg`/`workdir`
/// (#173/#425). `key` is `*`, a bare tool name, `tool(pattern)`, or
/// `tool{pattern}`; a scoped rule matches only when the corresponding value is
/// present and its `*`/`?` glob matches.
fn rule_matches(key: &str, name: &str, arg: Option<&str>, workdir: Option<&str>) -> bool {
    let (tool_pat, scope) = split_rule_key(key);
    if tool_pat != "*" && tool_pat != name {
        return false;
    }
    match scope {
        RuleScope::None => true,
        RuleScope::Arg(pat) => arg.is_some_and(|a| glob_match(pat, a)),
        RuleScope::Workdir(pat) => workdir.is_some_and(|w| glob_match(pat, w)),
    }
}

/// Split a rule key into its tool part and scope: `bash(git *)` ⇒
/// `("bash", Arg("git *"))`, `bash{/tmp/*}` ⇒ `("bash", Workdir("/tmp/*"))`,
/// `bash` ⇒ `("bash", None)`. A key without a matching trailing `)`/`}` is
/// treated as a plain tool name (no pattern).
fn split_rule_key(key: &str) -> (&str, RuleScope<'_>) {
    if let Some(open) = key.find('(') {
        if key.ends_with(')') {
            return (&key[..open], RuleScope::Arg(&key[open + 1..key.len() - 1]));
        }
    }
    if let Some(open) = key.find('{') {
        if key.ends_with('}') {
            return (
                &key[..open],
                RuleScope::Workdir(&key[open + 1..key.len() - 1]),
            );
        }
    }
    (key, RuleScope::None)
}

/// Minimal `*`/`?` wildcard match for argument-scoped permission rules (#173)
/// and the session tool overlay's [`ToolOverlayEntry::pattern`] (#537,
/// ADR-0148 — the agent tool mask this once also served is retired,
/// ADR-0207): `*` matches any run of characters (including `/` and the empty
/// string), `?` matches exactly one, everything else is literal. Deliberately
/// separator-agnostic and free of `**`/character-classes — so `bash(git *)`,
/// `edit(src/*)` and `mcp__docs__*` all read naturally and core stays
/// dependency-free (ADR-0006).
pub(super) fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    // Backtrack point: the last `*` seen and the text index it was matched from.
    let (mut star, mut resume) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            resume = ti;
            pi += 1;
        } else if let Some(s) = star {
            // Mismatch under a prior `*`: let it swallow one more char and retry.
            pi = s + 1;
            resume += 1;
            ti = resume;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// How long an [`InMsg::Approve`] grant lasts (#174). A bare `Approve` is
/// [`Once`][ApprovalScope::Once] — the historical one-shot behavior — so the
/// field defaults to it and older heads that omit it are unaffected. The runtime
/// records the wider scopes in a grant set so an *identical* later call (same
/// tool + argument, [`crate::PermissionProfile::resolve`]'s `arg`) skips the
/// prompt: [`Session`][ApprovalScope::Session] for the life of the session (in
/// memory), [`Always`][ApprovalScope::Always] persisted to the user's managed
/// grants file, or [`SessionDir`][ApprovalScope::SessionDir] (#486, ADR-0126) —
/// session-only like `Session`, but widened to every call under the approved
/// call's directory instead of an exact match, and restricted to the read-only
/// triad (`read`/`grep`/`glob`); on any other tool the runtime degrades it to
/// an exact `Session` grant rather than widening it. A grant only ever
/// upgrades an `Ask` to `Allow`; it never overrides a `Deny` (policy stays a
/// hard floor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
    /// Approve just this one call — the next identical call asks again.
    #[default]
    Once,
    /// Approve every identical call for the rest of this session (in-memory).
    Session,
    /// Approve every identical call, now and in future sessions — persisted to
    /// the runtime's managed grants file.
    Always,
    /// Approve every call under this call's directory for the rest of this
    /// session (#486) — the read-only triad (`read`/`grep`/`glob`) only; any
    /// other tool degrades this to an exact [`Session`][ApprovalScope::Session]
    /// grant instead of widening it. Never persisted (no `Always`-directory
    /// scope).
    SessionDir,
}

impl ApprovalScope {
    /// Whether this is the default one-shot scope. Drives `skip_serializing_if`
    /// so a bare `Approve` stays wire-identical to the pre-#174 shape.
    pub fn is_once(&self) -> bool {
        matches!(self, ApprovalScope::Once)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #486: `SessionDir` is an additive variant — pin its exact wire spelling
    /// (`snake_case`, matching the existing `session`/`always`) so a client
    /// implementation has a stable literal to target.
    #[test]
    fn approve_scope_session_dir_serializes_as_session_dir() {
        let json = serde_json::to_string(&ApprovalScope::SessionDir).unwrap();
        assert_eq!(json, "\"session_dir\"");
    }

    #[test]
    fn permission_last_match_wins() {
        let p = PermissionProfile::new(Permission::Deny)
            .with("*", Permission::Ask)
            .with("read", Permission::Allow);
        assert_eq!(p.for_tool("read"), Permission::Allow);
        assert_eq!(p.for_tool("bash"), Permission::Ask);
        assert_eq!(p.for_tool("edit"), Permission::Ask);
    }

    #[test]
    fn permission_defaults_when_no_rule() {
        let p = PermissionProfile::new(Permission::Allow);
        assert_eq!(p.for_tool("anything"), Permission::Allow);
    }

    #[test]
    fn argument_scoped_rule_refines_the_coarse_grade() {
        // Every bash asks, but `git *` is pre-approved and `rm *` hard-denied.
        let p = PermissionProfile::new(Permission::Allow)
            .with("bash", Permission::Ask)
            .with("bash(git *)", Permission::Allow)
            .with("bash(rm *)", Permission::Deny);
        assert_eq!(p.resolve("bash", Some("git status")), Permission::Allow);
        assert_eq!(p.resolve("bash", Some("rm -rf /")), Permission::Deny);
        // A command matching neither refined rule falls back to the coarse `bash`.
        assert_eq!(p.resolve("bash", Some("ls -la")), Permission::Ask);
    }

    #[test]
    fn argument_scoped_rule_ignored_without_an_argument() {
        // Name-only resolution (`for_tool`, and any tool with no arg) never sees
        // an argument-scoped rule — it falls through to the coarse grade.
        let p = PermissionProfile::new(Permission::Ask).with("bash(git *)", Permission::Allow);
        assert_eq!(p.for_tool("bash"), Permission::Ask);
        assert_eq!(p.resolve("bash", None), Permission::Ask);
        assert_eq!(p.resolve("bash", Some("git status")), Permission::Allow);
    }

    #[test]
    fn path_scoped_rule_matches_the_edit_target() {
        // Edits under `src/` are allowed; everything else asks.
        let p = PermissionProfile::new(Permission::Ask).with("edit(src/*)", Permission::Allow);
        assert_eq!(p.resolve("edit", Some("src/main.rs")), Permission::Allow);
        assert_eq!(p.resolve("edit", Some("src/a/b.rs")), Permission::Allow);
        assert_eq!(p.resolve("edit", Some("Cargo.toml")), Permission::Ask);
    }

    #[test]
    fn wildcard_star_matches_across_separators_and_question_matches_one() {
        assert!(glob_match("git *", "git status"));
        // The space before `*` is literal, so a bare `git` (no subcommand) misses.
        assert!(!glob_match("git *", "git"));
        assert!(glob_match("git*", "git")); // trailing `*` may match empty
        assert!(glob_match("src/*", "src/a/b/c.rs"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(!glob_match("git *", "cargo build"));
        assert!(glob_match("*", "anything at all"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exacts"));
    }

    #[test]
    fn split_rule_key_parses_tool_and_pattern() {
        assert_eq!(split_rule_key("bash"), ("bash", RuleScope::None));
        assert_eq!(split_rule_key("*"), ("*", RuleScope::None));
        assert_eq!(
            split_rule_key("bash(git *)"),
            ("bash", RuleScope::Arg("git *"))
        );
        assert_eq!(
            split_rule_key("edit(src/*)"),
            ("edit", RuleScope::Arg("src/*"))
        );
        assert_eq!(
            split_rule_key("bash{/tmp/*}"),
            ("bash", RuleScope::Workdir("/tmp/*"))
        );
        // A malformed key with no closing paren/brace stays a plain name.
        assert_eq!(split_rule_key("bash(oops"), ("bash(oops", RuleScope::None));
        assert_eq!(split_rule_key("bash{oops"), ("bash{oops", RuleScope::None));
    }

    #[test]
    fn workdir_scoped_rule_matches_the_bash_call_workdir() {
        // A workdir-scoped rule is independent of the command-scoped rule and
        // composes with it via ordinary last-match-wins.
        let p = PermissionProfile::new(Permission::Allow)
            .with("bash", Permission::Ask)
            .with("bash{/tmp/*}", Permission::Allow)
            .with("bash{/etc/*}", Permission::Deny);
        assert_eq!(
            p.resolve_scoped("bash", Some("ls"), Some("/tmp/scratch")),
            Permission::Allow
        );
        assert_eq!(
            p.resolve_scoped("bash", Some("ls"), Some("/etc/cron.d")),
            Permission::Deny
        );
        // Falls through to the coarse rule for a workdir matching neither.
        assert_eq!(
            p.resolve_scoped("bash", Some("ls"), Some("/home/x")),
            Permission::Ask
        );
        // The plain `resolve` entry point never sees `workdir` (equivalent to
        // `workdir = None`), so a workdir-scoped rule never matches through it.
        assert_eq!(p.resolve("bash", Some("ls")), Permission::Ask);
    }

    #[test]
    fn scoped_grades_expose_what_the_bare_grade_hides() {
        // The `plan` profile's shape: bare `write` is Deny, but a plans-folder
        // path resolves Allow — a per-tool UI that showed only the bare grade
        // would be lying.
        let p = PermissionProfile::new(Permission::Ask)
            .with("read", Permission::Allow)
            .with("write", Permission::Deny)
            .with("write(.entanglement/plans/*.md)", Permission::Allow);
        assert_eq!(p.for_tool("write"), Permission::Deny);
        assert_eq!(p.scoped_grades("write"), vec![Permission::Allow]);
        // A tool with no scoped rule at all — the bare grade is the whole truth.
        assert!(p.scoped_grades("read").is_empty());
    }

    #[test]
    fn scoped_grades_drop_variants_a_later_bare_rule_overrides() {
        // `research`'s shape: `call(*)` agrees with the `ask` default, so there
        // is nothing extra to report. And a scoped rule a *later* bare rule
        // overrides can never be reached, so it must not be reported either.
        let agrees = PermissionProfile::new(Permission::Ask).with("call(*)", Permission::Ask);
        assert!(agrees.scoped_grades("call").is_empty());

        let overridden = PermissionProfile::new(Permission::Ask)
            .with("write(plans/*)", Permission::Allow)
            .with("write", Permission::Deny);
        assert_eq!(overridden.for_tool("write"), Permission::Deny);
        assert!(overridden.scoped_grades("write").is_empty());
    }

    #[test]
    fn scoped_grades_cover_workdir_rules_and_wildcard_tool_keys() {
        let p = PermissionProfile::new(Permission::Ask).with("bash{/tmp/*}", Permission::Allow);
        assert_eq!(p.scoped_grades("bash"), vec![Permission::Allow]);
        // A `*(pattern)` key scopes every tool, so it reports for each of them.
        let all = PermissionProfile::new(Permission::Deny).with("*(git *)", Permission::Ask);
        assert_eq!(all.scoped_grades("bash"), vec![Permission::Ask]);
        assert_eq!(all.scoped_grades("call"), vec![Permission::Ask]);
        // Deduped: two scoped rules resolving to the same grade report once.
        let dup = PermissionProfile::new(Permission::Deny)
            .with("edit(src/*)", Permission::Allow)
            .with("edit(docs/*)", Permission::Allow);
        assert_eq!(dup.scoped_grades("edit"), vec![Permission::Allow]);
    }

    #[test]
    fn explicit_bare_deny_ignores_the_ambient_default_and_the_wildcard() {
        // The ambient default alone (`explore`'s shape: `default: deny`, no
        // rule ever mentions `edit`) is not an explicit floor.
        let ambient = PermissionProfile::new(Permission::Deny).with("read", Permission::Allow);
        assert!(!ambient.explicit_bare_deny("edit"));
        // Neither is a `*` catch-all.
        let wildcard = PermissionProfile::new(Permission::Allow).with("*", Permission::Deny);
        assert!(!wildcard.explicit_bare_deny("edit"));
        // Nor an argument-/workdir-scoped rule — it narrows, it doesn't deny
        // the bare tool.
        let scoped = PermissionProfile::new(Permission::Allow).with("bash(rm *)", Permission::Deny);
        assert!(!scoped.explicit_bare_deny("bash"));
        // Only a literal bare-name `deny` rule counts.
        let explicit = PermissionProfile::new(Permission::Allow).with("edit", Permission::Deny);
        assert!(explicit.explicit_bare_deny("edit"));
        assert!(!explicit.explicit_bare_deny("write"));
    }
}
