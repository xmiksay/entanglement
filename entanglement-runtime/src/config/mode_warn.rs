//! Startup warning for a `config.yml` `modes:` tuning rule whose tool name
//! nothing in the compile-time vocabulary recognizes.
//!
//! ADR-0166 shipped this exact posture for the old agent-borne `tools:`
//! mask and `permission:` rule keys; ADR-0207 stage 4c deleted the mask
//! (and with it `is_recognized_mask_entry`'s only caller,
//! `warn_unrecognized_mask_entries`) but not the failure mode: a `modes:`
//! tuning rule naming a renamed or nonexistent tool still loads silently and
//! grades nothing — a typo is an invisible no-op. This module restores the
//! warning for the rule language that replaced the mask, reusing
//! `is_recognized_mask_entry` unchanged rather than inventing a second
//! vocabulary check.

use serde_yaml::Value;

use super::RawLayer;
use crate::mode::rule_tool_name;
use crate::tool_names::is_recognized_mask_entry;

/// The three tuning-list keys a `modes:` mode entry can carry
/// (`mode::ModeTuning`'s own field names). Kept as a local constant instead
/// of deserializing through `ModeTuning` itself — this module works on the
/// raw pre-merge YAML of each layer, not the final merged config.
const GRADE_KEYS: [&str; 3] = ["allow", "deny", "prompt"];

/// `(source, mode, entry)` for every rule in any layer's own `modes:` block
/// whose tool name the compile-time vocabulary can't vouch for. Checked
/// against each layer's *own* YAML — not the final merged config — so the
/// warning names the file that actually wrote the offending line, mirroring
/// `ceiling_warn`'s per-layer approach. Only the rule's tool part (everything
/// before an argument/workdir scope, [`rule_tool_name`]) is checked: an
/// exact-argument scoped rule like `bash(cargo check)` carries no wildcard of
/// its own and must not false-positive just because the whole entry isn't a
/// known literal. `is_recognized_mask_entry` already narrows the check to
/// the compile-time built-in vocabulary — a `mcp__*`/`endpoint__*`/
/// `skill__*` name, a capability class, or a glob pattern is never flagged,
/// since MCP/endpoint/skill tools register after config load.
pub(super) fn stale_mode_tuning_names(layers: &[RawLayer]) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for layer in layers {
        let Some(modes) = layer.doc.get("modes").and_then(Value::as_mapping) else {
            continue;
        };
        for (mode_key, tuning) in modes {
            let Some(mode_name) = mode_key.as_str() else {
                continue;
            };
            let Some(tuning_map) = tuning.as_mapping() else {
                continue;
            };
            for grade in GRADE_KEYS {
                let Some(list) = tuning_map.get(grade).and_then(Value::as_sequence) else {
                    continue;
                };
                for entry in list {
                    let Some(entry) = entry.as_str() else {
                        continue;
                    };
                    if !is_recognized_mask_entry(rule_tool_name(entry)) {
                        out.push((
                            layer.source.clone(),
                            mode_name.to_string(),
                            entry.to_string(),
                        ));
                    }
                }
            }
        }
    }
    out
}

/// Log one warning per stale tuning rule. Called once per config load.
/// Non-fatal, exactly like ADR-0166's original: a rule this check can't
/// vouch for is *suspicious*, not *provably wrong* (the compile-time
/// vocabulary itself can lag a brand-new tool), so it degrades the one rule
/// loudly rather than aborting startup.
pub(super) fn warn_stale_mode_tuning_names(layers: &[RawLayer]) {
    for (source, mode, entry) in stale_mode_tuning_names(layers) {
        tracing::warn!(
            %source,
            %mode,
            %entry,
            "config.yml `modes:` tuning rule names an unrecognized tool — likely a \
             stale or typo'd name; it will silently never match anything"
        );
    }
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
        let stale = stale_mode_tuning_names(&layers);
        assert_eq!(
            stale,
            vec![(
                "test (project)".to_string(),
                "research".to_string(),
                "totally_bogus_tool".to_string(),
            )]
        );
    }

    #[test]
    fn a_renamed_tool_name_still_warns_like_adr_0166s_original_examples() {
        let layers = vec![layer(
            ConfigLayer::User,
            "modes:\n  build:\n    deny: [\"agent_spawn\"]\n",
        )];
        let stale = stale_mode_tuning_names(&layers);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].2, "agent_spawn");
    }

    #[test]
    fn a_legitimate_mcp_tool_does_not_warn() {
        let layers = vec![layer(
            ConfigLayer::Project,
            "modes:\n  research:\n    allow: [\"mcp__docs__search\"]\n",
        )];
        assert!(stale_mode_tuning_names(&layers).is_empty());
    }

    #[test]
    fn a_legitimate_scoped_bash_rule_does_not_warn() {
        let layers = vec![layer(
            ConfigLayer::Project,
            "modes:\n  research:\n    allow: [\"bash(cargo *)\"]\n",
        )];
        assert!(stale_mode_tuning_names(&layers).is_empty());
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
        assert!(stale_mode_tuning_names(&layers).is_empty());
    }

    #[test]
    fn a_capability_class_name_does_not_warn() {
        let layers = vec![layer(
            ConfigLayer::Project,
            "modes:\n  build:\n    deny: [\"write\"]\n",
        )];
        assert!(stale_mode_tuning_names(&layers).is_empty());
    }

    #[test]
    fn an_endpoint_or_skill_tool_does_not_warn() {
        let layers = vec![layer(
            ConfigLayer::Project,
            "modes:\n  build:\n    prompt: [\"endpoint__docs\", \"skill__deploy__ship\"]\n",
        )];
        assert!(stale_mode_tuning_names(&layers).is_empty());
    }

    #[test]
    fn no_modes_block_reports_nothing() {
        let layers = vec![layer(ConfigLayer::User, "verbose: true\n")];
        assert!(stale_mode_tuning_names(&layers).is_empty());
    }
}
