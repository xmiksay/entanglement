//! Startup warning for a `config.yml` rule — a `modes:` tuning rule or a
//! `permissions:` ceiling rule — whose tool name can never match a call.
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

use serde_yaml::{Mapping, Value};

use super::RawLayer;
use crate::mode::{capability_class, rule_tool_name};
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
/// `ceiling_warn`'s per-layer approach. Only the rule's tool part
/// (everything before an argument/workdir scope, [`rule_tool_name`]) is
/// checked: an exact-argument scoped rule like `bash(cargo check)` carries
/// no wildcard of its own and must not false-positive just because the
/// whole entry isn't a known literal. A `mcp__<server>__<tool>` name, a
/// bare capability class key, or a bare `*` is never flagged (MCP tools
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
/// non-maskable name, or a tool-name glob — rule keys match the tool part
/// exactly, only a bare `*` is a wildcard (the core matcher's
/// `rule_matches`; name globs are the session overlay's alone).
fn no_match_reason(entry: &str) -> Option<String> {
    let tool = rule_tool_name(entry);
    if tool != "*" && tool.contains(['*', '?']) {
        return Some(format!(
            "rule tool names match exactly — `{tool}` is no glob here (only a bare `*` is); \
             name each tool, e.g. `mcp__<server>__<tool>`"
        ));
    }
    non_maskable_reason(tool)
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
                "config.yml rule names a tool no rule can match — it has no effect: {reason}"
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
}
