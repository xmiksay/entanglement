//! `config.yml`'s `permissions:` ceiling grammar (#172, ADR-0207 stage 6c):
//! the exact same `default`/`allow`/`deny`/`prompt` shape a mode body uses
//! (§4/§5), not the old free-form `tool: allow|ask|deny` map
//! ([`crate::agents::permission_from_value`], now dead for this call site).
//!
//! The parsed [`RawCeiling`] is turned into a plain
//! [`entanglement_core::PermissionProfile`] — the type every dispatch call
//! site already threads (`spawn_tool_executor*`, `BindingPolicy`, ~30 test
//! fixtures) — by pushing each `deny`/`allow`/`prompt` entry as a rule.
//! Declaration order no longer matters the way it used to: grading now goes
//! through [`crate::mode::Mode::from_permission_profile`] at every real use
//! site ([`crate::permission::clamp_to_base`]), which re-sorts by
//! longest-match, not by insertion order — so building the profile is a
//! trivial bucket-and-flatten with no capability-class expansion needed
//! here at all (a bare `write`/`read`/`exec`/`plan`/`control` key rides
//! through unmodified and is recognized as a class by
//! `mode::rules::capability_class` the moment it's graded).

use anyhow::Result;
use entanglement_core::{Permission, PermissionProfile};
use serde::Deserialize;

/// The raw `permissions:` block shape. Unlike a built-in mode's
/// [`crate::mode::builtin`] YAML, `default` is optional here (absent ⇒
/// `Permission::Allow`, matching the pre-6c ceiling's allow-all no-op
/// default) — a ceiling that only ever adds `deny`/`prompt` narrowing
/// shouldn't have to repeat `default: allow` to spell "otherwise unchanged".
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawCeiling {
    #[serde(default, deserialize_with = "deserialize_grade_opt")]
    default: Option<Permission>,
    #[serde(default)]
    deny: Vec<String>,
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    prompt: Vec<String>,
}

fn deserialize_grade_opt<'de, D>(deserializer: D) -> Result<Option<Permission>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    crate::mode::deserialize_grade(deserializer).map(Some)
}

/// Build the ceiling [`PermissionProfile`] from a parsed [`RawCeiling`].
/// `None` (`permissions:` absent from every layer) is the pre-existing
/// allow-all no-op.
pub(super) fn build_ceiling_profile(raw: Option<&RawCeiling>) -> PermissionProfile {
    let Some(raw) = raw else {
        return PermissionProfile::new(Permission::Allow);
    };
    let mut profile = PermissionProfile::new(raw.default.unwrap_or(Permission::Allow));
    for key in &raw.deny {
        profile = profile.with(key.clone(), Permission::Deny);
    }
    for key in &raw.allow {
        profile = profile.with(key.clone(), Permission::Allow);
    }
    for key in &raw.prompt {
        profile = profile.with(key.clone(), Permission::Ask);
    }
    profile
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> RawCeiling {
        serde_yaml::from_str(yaml).expect("valid ceiling YAML")
    }

    #[test]
    fn absent_permissions_is_allow_all() {
        let profile = build_ceiling_profile(None);
        assert_eq!(profile.default, Permission::Allow);
        assert!(profile.rules.is_empty());
    }

    #[test]
    fn default_defaults_to_allow_when_omitted() {
        let raw = parse("deny: [write]\n");
        let profile = build_ceiling_profile(Some(&raw));
        assert_eq!(profile.default, Permission::Allow);
    }

    #[test]
    fn prompt_spells_ask() {
        let raw = parse("default: prompt\n");
        let profile = build_ceiling_profile(Some(&raw));
        assert_eq!(profile.default, Permission::Ask);
    }

    #[test]
    fn ask_is_not_a_valid_default_spelling() {
        let err = serde_yaml::from_str::<RawCeiling>("default: ask\n")
            .expect_err("the ceiling spells Permission::Ask as `prompt`, like a mode body");
        assert!(err.to_string().contains("prompt"));
    }

    #[test]
    fn an_old_grammar_key_is_rejected_as_unknown() {
        // `bash: ask` (the old free-form shape) has no home in the new
        // struct — `deny_unknown_fields` rejects it outright, which is
        // exactly the signal `migrate_permissions` uses to detect a legacy
        // file that needs rewriting.
        assert!(serde_yaml::from_str::<RawCeiling>("bash: ask\n").is_err());
    }

    #[test]
    fn deny_allow_prompt_lists_become_profile_rules() {
        let raw = parse("deny: [write]\nallow: [\"bash(cargo check)\"]\nprompt: [call]\n");
        let profile = build_ceiling_profile(Some(&raw));
        assert!(profile
            .rules
            .contains(&("write".to_string(), Permission::Deny)));
        assert!(profile
            .rules
            .contains(&("bash(cargo check)".to_string(), Permission::Allow)));
        assert!(profile
            .rules
            .contains(&("call".to_string(), Permission::Ask)));
    }
}
