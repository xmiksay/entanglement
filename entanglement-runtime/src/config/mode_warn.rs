//! Startup warning for a `config.yml` rule — a `modes:` tuning rule or a
//! `permissions:` ceiling rule — whose tool name or scope can never match a
//! call.
//!
//! ADR-0166 shipped this exact posture for the old agent-borne `tools:`
//! mask and `permission:` rule keys; ADR-0207 stage 4c deleted the mask
//! (and with it `is_recognized_mask_entry`'s only caller,
//! `warn_unrecognized_mask_entries`) but not the failure mode: a rule naming
//! a renamed or nonexistent tool still loads silently and grades nothing — a
//! typo is an invisible no-op. This module restores the warning for the rule
//! language that replaced the mask, reusing `is_recognized_mask_entry`
//! unchanged rather than inventing a second vocabulary check. A *known* name
//! dispatch never grades under its own name (`mcp`, the discovery tools, the
//! script-only read variants `read_raw`/`*_json`, the bookkeeping Control
//! tools — `tool_names::NON_MASKABLE_TOOLS`, #843/#849) is the same
//! invisible no-op and warns with the reason instead. For `propose_plan`
//! the no-op is enforced, not incidental: the matcher skips a rule naming a
//! non-maskable tool (`mode::rules`), so only its `plan` class grades it.
//! So are keys whose scope the matcher never tests:
//! - an argument scope on a tool with no argument (#878), `mcp__gh__x(foo)`;
//! - a workdir scope on any tool but `bash`/`call` (#881), `read{/tmp/*}`;
//! - a scope the matcher can't read at all, `bash(git *){/x}` (one scope
//!   per key, closed at its end).

use entanglement_core::{Permission, PermissionProfile};
use serde_yaml::{Mapping, Value};

use super::RawLayer;
use crate::mode::{capability_class, rule_tool_name};
use crate::permission::{takes_permission_arg, takes_workdir_scope};
use crate::tool_names::{is_recognized_mask_entry, non_maskable_reason};

/// The three rule-list keys a `modes:` mode entry (`mode::ModeTuning`) and
/// the `permissions:` ceiling (`config::ceiling::RawCeiling`) both carry.
/// Kept as a local constant instead of deserializing through either type —
/// this module works on the raw pre-merge YAML of each layer, not the final
/// merged config.
const GRADE_KEYS: [&str; 3] = ["allow", "deny", "prompt"];

/// The `scope` a `permissions:` ceiling rule is reported under (a `modes:`
/// rule reports its mode name).
pub(super) const CEILING_SCOPE: &str = "permissions";

/// `(source, scope, entry)` for every rule in any layer's own `modes:` block
/// (scope = the mode name) or `permissions:` block (scope =
/// [`CEILING_SCOPE`]) whose tool name can never match: one the compile-time
/// vocabulary can't vouch for, or a non-maskable one. Checked against each
/// layer's *own* YAML — not the final merged config — so the warning names
/// the file that actually wrote the offending line, mirroring
/// `ceiling_warn`'s per-layer approach. The rule's tool part (everything
/// before an argument/workdir scope, [`rule_tool_name`]) is what's checked
/// against the vocabulary: an exact-argument scoped rule like
/// `bash(cargo check)` carries no wildcard of its own and must not
/// false-positive just because the whole entry isn't a known literal. The
/// scope itself only matters for a tool with no argument or workdir to
/// scope (`takes_permission_arg`, #878; `takes_workdir_scope`, #881) or
/// when it is malformed ([`key_scope`]). An unscoped `mcp__<server>__<tool>`
/// name, a bare capability class key, or a bare `*` is never flagged (MCP tools
/// register after config load); a tool-name glob like `mcp__gh__*` is,
/// since rule keys never glob the tool part ([`no_match_reason`]).
pub(super) fn stale_rule_names(layers: &[RawLayer]) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for layer in layers {
        let modes = layer
            .doc
            .get("modes")
            .and_then(Value::as_mapping)
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| Some((k.as_str()?, v.as_mapping()?)));
        let ceiling = layer
            .doc
            .get(CEILING_SCOPE)
            .and_then(Value::as_mapping)
            .map(|m| (CEILING_SCOPE, m));
        for (scope, rules) in modes.chain(ceiling) {
            for entry in rule_entries(rules).filter(|e| is_stale(e)) {
                out.push((layer.source.clone(), scope.to_string(), entry.to_string()));
            }
        }
    }
    out
}

/// Every string rule under `rules`' grade lists.
fn rule_entries(rules: &Mapping) -> impl Iterator<Item = &str> {
    GRADE_KEYS
        .iter()
        .filter_map(|grade| rules.get(*grade).and_then(Value::as_sequence))
        .flatten()
        .filter_map(Value::as_str)
}

fn is_stale(entry: &str) -> bool {
    capability_class(entry).is_none()
        && (no_match_reason(entry).is_some() || !is_recognized_mask_entry(rule_tool_name(entry)))
}

/// Why a rule whose tool part is a *known* spelling still never matches: a
/// non-maskable name, a tool-name glob — rule keys match the tool part
/// exactly, only a bare `*` is a wildcard (the core matcher's
/// `rule_matches`; name globs are the session overlay's alone) — or an
/// argument scope on a tool that never has an argument (#878: the matcher
/// sees `None`, so `mcp__gh__x(foo)` is as inert as a typo), a workdir scope
/// on a tool that never has a workdir (#881, `read{/tmp/*}`), or a scope the
/// matcher can't read ([`KeyScope::Malformed`]).
fn no_match_reason(entry: &str) -> Option<String> {
    let tool = rule_tool_name(entry);
    if tool != "*" && tool.contains(['*', '?']) {
        return Some(format!(
            "rule tool names match exactly — `{tool}` is no glob here (only a bare `*` is); \
             name each tool, e.g. `mcp__<server>__<tool>`"
        ));
    }
    if let Some(reason) = non_maskable_reason(tool) {
        return Some(reason);
    }
    // An unrecognized tool part keeps the stale-name warning, which is the
    // likelier mistake (`bassh(x)` is a typo, not a misplaced scope).
    if !is_recognized_mask_entry(tool) {
        return None;
    }
    match key_scope(entry) {
        KeyScope::Malformed => Some(format!(
            "`{entry}` is no rule key the matcher reads — a key carries at most one \
             scope, closed at its very end; {}",
            scope_forms(tool)
        )),
        KeyScope::Arg if tool != "*" && !takes_permission_arg(tool) => Some(format!(
            "`{tool}` has no argument to scope — `{entry}` never matches a call; {}",
            instead(tool)
        )),
        KeyScope::Workdir if tool != "*" && !takes_workdir_scope(tool) => Some(format!(
            "`{tool}` has no workdir to scope (only `bash`/`call` do) — `{entry}` \
             never matches a call; {}",
            instead(tool)
        )),
        _ => None,
    }
}

/// The scopes `tool` really takes, so a malformed key's hint never points at
/// a form that would only trip another of these warnings.
fn scope_forms(tool: &str) -> String {
    match (takes_permission_arg(tool), takes_workdir_scope(tool)) {
        (true, true) => format!("write `{tool}(pattern)` or `{tool}{{workdir}}`"),
        (true, false) => format!("write `{tool}(pattern)`"),
        (false, true) => format!("write `{tool}{{workdir}}`"),
        (false, false) => format!("`{tool}` takes no scope"),
    }
}

/// The fix for a scope `tool` can't carry. A bare class name (`read`,
/// `write`) names the whole capability class, so suggesting it would widen a
/// rule that matched nothing into one that matches every tool in the class;
/// both class-named tools take an argument, so that scope is offered instead.
fn instead(tool: &str) -> String {
    match takes_permission_arg(tool).then(|| format!("`{tool}(pattern)`")) {
        Some(arg) if capability_class(tool).is_some() => format!(
            "scope its argument, {arg}, instead (a bare `{tool}` names its whole capability class)"
        ),
        Some(arg) => format!("write bare `{tool}` or scope its argument, {arg}, instead"),
        None => format!("write bare `{tool}` instead"),
    }
}

/// Which scope a rule key carries, as core's matcher reads it.
#[derive(Debug, PartialEq)]
enum KeyScope {
    None,
    Arg,
    Workdir,
    /// A scope the matcher folds into the tool part (`bash(x){/y}`, an
    /// unclosed `read{x`), so the key matches no call at all.
    Malformed,
}

/// Probed through the public single-rule [`PermissionProfile`] — the same
/// door `mode::rules` grades through — rather than re-parsing core's private
/// `split_rule_key`: a glob always matches itself, so a well-formed key
/// matches its own scope text offered as the argument or as the workdir,
/// and a malformed one matches neither.
fn key_scope(entry: &str) -> KeyScope {
    let tool = rule_tool_name(entry);
    if tool.len() == entry.len() {
        return KeyScope::None;
    }
    // `None` when the key's last char isn't a one-byte closer — core reads
    // no scope there either.
    let Some(inner) = entry.get(tool.len() + 1..entry.len() - 1) else {
        return KeyScope::Malformed;
    };
    let matches = |arg, workdir| {
        PermissionProfile::new(Permission::Deny)
            .with(entry, Permission::Allow)
            .resolve_scoped(tool, arg, workdir)
            == Permission::Allow
    };
    match (matches(Some(inner), None), matches(None, Some(inner))) {
        (true, _) => KeyScope::Arg,
        (false, true) => KeyScope::Workdir,
        (false, false) => KeyScope::Malformed,
    }
}

/// Log one warning per stale rule. Called once per config load. Non-fatal,
/// exactly like ADR-0166's original: a rule this check can't vouch for is
/// *suspicious*, not *provably wrong* (the compile-time vocabulary itself
/// can lag a brand-new tool), so it degrades the one rule loudly rather than
/// aborting startup.
///
/// Logs only a finding new since this root's previous load
/// ([`super::warn_once`], #785/#813). Returns how many it logged.
pub(super) fn warn_stale_rule_names(layers: &[RawLayer], root: Option<&std::path::Path>) -> usize {
    let stale = stale_rule_names(layers);
    let keys: Vec<String> = stale
        .iter()
        .map(|(s, m, e)| format!("{s}\0{m}\0{e}"))
        .collect();
    let fresh = super::warn_once::fresh(super::warn_once::scope("mode", root, layers), &keys);
    let mut logged = 0;
    for ((source, scope, entry), _) in stale.into_iter().zip(fresh).filter(|(_, f)| *f) {
        logged += 1;
        match no_match_reason(&entry) {
            Some(reason) => tracing::warn!(
                %source,
                %scope,
                %entry,
                "config.yml rule can never match a call — it has no effect: {reason}"
            ),
            None => tracing::warn!(
                %source,
                %scope,
                %entry,
                "config.yml rule names an unrecognized tool — likely a stale or \
                 typo'd name; it will silently never match anything"
            ),
        }
    }
    logged
}

#[cfg(test)]
mod tests {
    use super::super::ConfigLayer;
    use super::*;

    fn layer(layer: ConfigLayer, yaml: &str) -> RawLayer {
        RawLayer {
            layer,
            source: format!("test ({})", layer.label()),
            doc: serde_yaml::from_str(yaml).unwrap(),
        }
    }

    #[test]
    fn a_nonsense_tool_name_warns() {
        let layers = vec![layer(
            ConfigLayer::Project,
            "modes:\n  research:\n    allow: [\"totally_bogus_tool\"]\n",
        )];
        let stale = stale_rule_names(&layers);
        assert_eq!(
            stale,
            vec![(
                "test (project)".to_string(),
                "research".to_string(),
                "totally_bogus_tool".to_string(),
            )]
        );
    }

    /// #785: a reload re-running the same load logs nothing new; a changed
    /// finding still warns.
    #[test]
    fn a_repeated_load_warns_once_per_finding() {
        let yaml = "modes:\n  research:\n    allow: [\"dedupe_785_bogus\"]\n";
        let layers = vec![layer(ConfigLayer::Project, yaml)];
        assert_eq!(warn_stale_rule_names(&layers, None), 1);
        assert_eq!(warn_stale_rule_names(&layers, None), 0);
        let changed = vec![layer(
            ConfigLayer::Project,
            "modes:\n  research:\n    allow: [\"dedupe_785_other\"]\n",
        )];
        assert_eq!(warn_stale_rule_names(&changed, None), 1);
    }

    #[test]
    fn a_renamed_tool_name_still_warns_like_adr_0166s_original_examples() {
        let layers = vec![layer(
            ConfigLayer::User,
            "modes:\n  build:\n    deny: [\"agent_spawn\"]\n",
        )];
        let stale = stale_rule_names(&layers);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].2, "agent_spawn");
    }

    #[test]
    fn a_legitimate_mcp_tool_does_not_warn() {
        let layers = vec![layer(
            ConfigLayer::Project,
            "modes:\n  research:\n    allow: [\"mcp__docs__search\"]\n",
        )];
        assert!(stale_rule_names(&layers).is_empty());
    }

    #[test]
    fn a_legitimate_scoped_bash_rule_does_not_warn() {
        let layers = vec![layer(
            ConfigLayer::Project,
            "modes:\n  research:\n    allow: [\"bash(cargo *)\"]\n",
        )];
        assert!(stale_rule_names(&layers).is_empty());
    }

    /// The tool part (`bash`) is what's checked, not the whole entry — an
    /// exact-argument scoped rule with no glob character must not
    /// false-positive just because it lacks a `*`.
    #[test]
    fn an_exact_scoped_rule_with_no_wildcard_does_not_warn() {
        let layers = vec![layer(
            ConfigLayer::Project,
            "modes:\n  research:\n    allow: [\"bash(cargo check)\"]\n",
        )];
        assert!(stale_rule_names(&layers).is_empty());
    }

    #[test]
    fn a_capability_class_name_does_not_warn() {
        let layers = vec![layer(
            ConfigLayer::Project,
            "modes:\n  build:\n    deny: [\"write\"]\n",
        )];
        assert!(stale_rule_names(&layers).is_empty());
    }

    /// ADR-0228 retired skill-declared `skill__*` tools: a rule naming one
    /// is stale now.
    #[test]
    fn a_retired_skill_tool_warns() {
        let layers = vec![layer(
            ConfigLayer::Project,
            "modes:\n  build:\n    prompt: [\"skill__deploy__ship\"]\n",
        )];
        assert_eq!(stale_rule_names(&layers).len(), 1);
    }

    /// ADR-0228: the launch grades on its script `path`, so a scoped
    /// `rhai(...)` rule is as valid as a bare `rhai`.
    #[test]
    fn scoped_and_bare_rhai_rules_do_not_warn() {
        let layers = vec![layer(
            ConfigLayer::Project,
            "modes:\n  build:\n    allow: [\"rhai(scripts/*)\", \"rhai\"]\n",
        )];
        assert!(stale_rule_names(&layers).is_empty());
    }

    #[test]
    fn no_modes_block_reports_nothing() {
        let layers = vec![layer(ConfigLayer::User, "verbose: true\n")];
        assert!(stale_rule_names(&layers).is_empty());
    }

    /// #843: a known name dispatch never grades under its own name is as
    /// inert as a typo — in a mode rule and in the ceiling alike.
    #[test]
    fn non_maskable_names_warn_in_mode_and_ceiling_rules() {
        let layers = vec![layer(
            ConfigLayer::User,
            "modes:\n  build:\n    deny: [\"mcp\", \"list_mcp\"]\n\
             permissions:\n  deny: [\"glob_json\", \"mcp(*)\", \"bash\"]\n",
        )];
        let stale: Vec<(String, String)> = stale_rule_names(&layers)
            .into_iter()
            .map(|(_, scope, entry)| (scope, entry))
            .collect();
        let want = |scope: &str, entry: &str| (scope.to_string(), entry.to_string());
        assert_eq!(
            stale,
            vec![
                want("build", "mcp"),
                want("build", "list_mcp"),
                want(CEILING_SCOPE, "glob_json"),
                want(CEILING_SCOPE, "mcp(*)"),
            ]
        );
    }

    /// #849: a rule naming a bookkeeping Control tool warns, in a mode rule
    /// and in the ceiling; the builtin-style `plan` class rule that actually
    /// gates `propose_plan` does not, nor does a rule naming `agent` (graded).
    #[test]
    fn bookkeeping_control_tool_rules_warn_but_their_class_and_agent_do_not() {
        let tools = [
            "poll",
            "ask_user",
            "update_tasks",
            "request_mode",
            "propose_plan",
        ];
        let list = tools.map(|t| format!("\"{t}\"")).join(", ");
        let layers = vec![layer(
            ConfigLayer::User,
            &format!(
                "modes:\n  plan:\n    deny: [{list}, \"plan\", \"agent\", \"agent_send\"]\n\
                 permissions:\n  prompt: [\"propose_plan(*)\", \"control\"]\n"
            ),
        )];
        let stale: Vec<(String, String)> = stale_rule_names(&layers)
            .into_iter()
            .map(|(_, scope, entry)| (scope, entry))
            .collect();
        let mut want: Vec<(String, String)> = tools
            .iter()
            .map(|t| ("plan".to_string(), t.to_string()))
            .collect();
        want.push((CEILING_SCOPE.to_string(), "propose_plan(*)".to_string()));
        assert_eq!(stale, want);
        let why = no_match_reason("propose_plan").unwrap();
        assert!(why.contains("name `plan` instead"), "{why}");
    }

    /// ADR-0235: the spawning pair grades on its target, so a scoped rule
    /// on either is live; a scoped rule on a tool dispatch never grades
    /// still warns.
    #[test]
    fn scoped_spawning_rules_do_not_warn_but_a_scoped_non_maskable_one_does() {
        let layers = vec![layer(
            ConfigLayer::User,
            "modes:\n  build:\n    deny: [\"agent(plan)\", \"agent_send(acme:*)\", \"poll(x)\"]\n",
        )];
        let stale: Vec<String> = stale_rule_names(&layers)
            .into_iter()
            .map(|(_, _, entry)| entry)
            .collect();
        assert_eq!(stale, vec!["poll(x)".to_string()]);
    }

    #[test]
    fn a_per_tool_mcp_rule_and_class_keys_do_not_warn() {
        let layers = vec![layer(
            ConfigLayer::User,
            "modes:\n  build:\n    deny: [\"mcp__gh__x\", \"exec\", \"control\", \"*(*.pem)\"]\n\
             permissions:\n  default: allow\n  prompt: [\"plan\", \"mcp__gh__create\"]\n",
        )];
        assert!(stale_rule_names(&layers).is_empty());
    }

    /// #878: a scoped key on a tool that never has an argument warns — an
    /// MCP tool (connected or not), and in the ceiling too — naming the fix.
    #[test]
    fn a_scoped_rule_on_an_argument_less_tool_warns() {
        let layers = vec![layer(
            ConfigLayer::User,
            "modes:\n  build:\n    deny: [\"mcp__gh__create(foo)\"]\n\
             permissions:\n  prompt: [\"mcp__docs__search(*)\"]\n",
        )];
        let stale: Vec<(String, String)> = stale_rule_names(&layers)
            .into_iter()
            .map(|(_, scope, entry)| (scope, entry))
            .collect();
        assert_eq!(
            stale,
            vec![
                ("build".to_string(), "mcp__gh__create(foo)".to_string()),
                (
                    CEILING_SCOPE.to_string(),
                    "mcp__docs__search(*)".to_string()
                ),
            ]
        );
        let why = no_match_reason("mcp__gh__create(foo)").unwrap();
        assert!(why.contains("no argument to scope"), "{why}");
        assert!(why.contains("write bare `mcp__gh__create`"), "{why}");
        // A non-maskable tool keeps its own, more specific reason.
        let why = no_match_reason("load_skill(x)").unwrap();
        assert!(why.contains("discovery tool"), "{why}");
        let discovery = layer(
            ConfigLayer::User,
            "modes:\n  build:\n    deny: [\"load_skill(x)\", \"mcp_tools(gh)\"]\n",
        );
        assert_eq!(stale_rule_names(&[discovery]).len(), 2);
    }

    /// #878: every tool that does take an argument keeps its scoped rules
    /// silent — the wildcard tool and a workdir scope too.
    #[test]
    fn scoped_rules_on_argument_taking_tools_do_not_warn() {
        let keys = [
            "bash(cargo *)",
            "call(git *)",
            "agent(plan)",
            "agent_send(acme:*)",
            "read(src/*)",
            "write(docs/*)",
            "edit(src/*)",
            "apply_patch(src/*)",
            "glob(src/**)",
            "grep(src/*)",
            "rhai(scripts/*)",
            "web_fetch(https://docs.rs/*)",
            "http(GET https://api.github.com/*)",
            "*(*.pem)",
            "bash{/tmp/*}",
            "mcp__gh__create",
        ];
        let list = keys.map(|k| format!("\"{k}\"")).join(", ");
        let layers = vec![layer(
            ConfigLayer::User,
            &format!("modes:\n  build:\n    allow: [{list}]\n"),
        )];
        assert!(
            stale_rule_names(&layers).is_empty(),
            "{:?}",
            stale_rule_names(&layers)
        );
    }

    /// An unrecognized tool part keeps the stale-name warning rather than
    /// blaming the scope.
    #[test]
    fn a_scoped_typo_warns_as_unrecognized() {
        assert_eq!(no_match_reason("bassh(cargo *)"), None);
        let layers = vec![layer(
            ConfigLayer::User,
            "modes:\n  build:\n    deny: [\"bassh(cargo *)\"]\n",
        )];
        assert_eq!(stale_rule_names(&layers).len(), 1);
    }

    #[test]
    fn a_typo_in_the_ceiling_warns() {
        let layers = vec![layer(
            ConfigLayer::User,
            "permissions:\n  deny: [\"bassh\"]\n",
        )];
        let stale = stale_rule_names(&layers);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].1, CEILING_SCOPE);
    }

    /// #843: rule keys match the tool part exactly, so a server-wide
    /// `mcp__gh__*` rule denies nothing — only the overlay globs names.
    #[test]
    fn a_tool_name_glob_rule_warns() {
        let layers = vec![layer(
            ConfigLayer::User,
            "modes:\n  build:\n    deny: [\"mcp__gh__*\", \"list_?\"]\n",
        )];
        let stale = stale_rule_names(&layers);
        assert_eq!(stale.len(), 2, "{stale:?}");
        let why = no_match_reason("mcp__gh__*").unwrap();
        assert!(why.contains("no glob here"), "{why}");
    }

    /// A hint never suggests a bare class name (it would match the whole
    /// class) nor a scope the tool can't carry (it would only warn again).
    #[test]
    fn hints_suggest_only_forms_that_match() {
        let why = no_match_reason("read{/tmp/*}").unwrap();
        assert!(why.contains("`read(pattern)`"), "{why}");
        assert!(!why.contains("write bare `read`"), "{why}");
        let why = no_match_reason("read(a){b}").unwrap();
        assert!(why.contains("`read(pattern)`"), "{why}");
        assert!(!why.contains("read{workdir}"), "{why}");
        let why = no_match_reason("bash(git *){/x}").unwrap();
        assert!(why.contains("`bash(pattern)` or `bash{workdir}`"), "{why}");
    }

    fn stale_entries(yaml: &str) -> Vec<String> {
        stale_rule_names(&[layer(ConfigLayer::User, yaml)])
            .into_iter()
            .map(|(_, _, entry)| entry)
            .collect()
    }

    /// #881: a workdir scope on any tool but `bash`/`call` warns — a known
    /// tool and an MCP tool alike, in a mode rule and in the ceiling.
    #[test]
    fn a_workdir_scope_on_a_tool_without_a_workdir_warns() {
        let stale = stale_entries(
            "modes:\n  build:\n    deny: [\"read{/tmp/*}\", \"mcp__x__y{/a}\"]\n\
             permissions:\n  prompt: [\"grep{src}\"]\n",
        );
        assert_eq!(stale, ["read{/tmp/*}", "mcp__x__y{/a}", "grep{src}"]);
        let why = no_match_reason("read{/tmp/*}").unwrap();
        assert!(why.contains("no workdir to scope"), "{why}");
        assert!(why.contains("`read(pattern)`"), "{why}");
        let why = no_match_reason("mcp__x__y{/a}").unwrap();
        assert!(why.contains("write bare `mcp__x__y` instead"), "{why}");
    }

    /// #881: `bash`/`call` and the `*` wildcard keep their workdir scopes
    /// silent, as do braces or parens nested inside a well-formed scope.
    #[test]
    fn well_formed_workdir_scopes_do_not_warn() {
        let keys = [
            "bash{/tmp/*}",
            "call{/srv/*}",
            "*{/tmp/*}",
            "bash{a(b)}",
            "read(a{b})",
            "bash()",
        ];
        let list = keys.map(|k| format!("\"{k}\"")).join(", ");
        let stale = stale_entries(&format!("modes:\n  build:\n    allow: [{list}]\n"));
        assert!(stale.is_empty(), "{stale:?}");
    }

    /// #881: core's matcher reads one closing scope per key, so a combined,
    /// reordered, unclosed or trailing-text key matches nothing and warns —
    /// a multi-byte last char included, without panicking on the slice.
    #[test]
    fn a_malformed_scope_warns() {
        let keys = [
            "bash(git *){/x}",
            "bash{/x}(git *)",
            "read{/tmp/*",
            "bash(git *",
            "bash(",
            "bash{",
            "bash(x)y",
            "read(é",
            "*(x){y}",
        ];
        let list = keys.map(|k| format!("\"{k}\"")).join(", ");
        let stale = stale_entries(&format!("modes:\n  build:\n    deny: [{list}]\n"));
        assert_eq!(stale, keys);
        let why = no_match_reason("bash(git *){/x}").unwrap();
        assert!(why.contains("at most one scope"), "{why}");
    }

    /// The probe agrees with core's matcher on every shape above.
    #[test]
    fn key_scope_reads_keys_like_the_matcher() {
        assert_eq!(key_scope("bash"), KeyScope::None);
        assert_eq!(key_scope("bash(git *)"), KeyScope::Arg);
        assert_eq!(key_scope("read(a{b})"), KeyScope::Arg);
        assert_eq!(key_scope("bash{/tmp/*}"), KeyScope::Workdir);
        assert_eq!(key_scope("bash{a(b)}"), KeyScope::Workdir);
        assert_eq!(key_scope("*{/x}"), KeyScope::Workdir);
        assert_eq!(key_scope("bash(git *){/x}"), KeyScope::Malformed);
        assert_eq!(key_scope("read(é"), KeyScope::Malformed);
    }
}
