//! Apply a `config.yml` `modes:` tuning entry over a built-in mode
//! (ADR-0207 §5). Tuning only ever **adds** rules — there is deliberately no
//! removal syntax: longest-match ([`super::rules`]) already makes one
//! unnecessary, since a longer, more specific rule simply out-ranks a
//! shorter shipped one. To stop `bash(wc *)` being pre-allowed, add a longer
//! `deny` that covers the case you care about — the shipped rule stays
//! visible and the override reads as an override.
//!
//! Tuning may never change `default`, and may never weaken a
//! capability-class `deny`. Because longest-match lets a long scoped rule
//! out-rank a short class name, that guard can't be a string comparison:
//! `research: allow: ["write(*)"]` names no class, yet grants exactly what
//! `deny: [write]` forbids, and under longest-match that 13-character rule
//! would out-rank the 5-character class deny. [`apply`] closes this by
//! taking a capability resolver and rejecting any rule whose named tool
//! carries a capability that's already class-denied in this mode — which
//! keeps this module free of a `ToolRegistry` dependency; the caller
//! supplies the resolver, and stage 4 will pass one backed by the real
//! registry.

use anyhow::{bail, Result};
use serde::Deserialize;

use super::rules::{capability_class, rule_tool_name};
use super::Mode;
use crate::capability::Capability;
use entanglement_core::Permission;

/// One mode's tuning entry from `config.yml`'s `modes:` block — the same
/// `deny`/`allow`/`prompt` list shape as [`super::builtin::RawMode`]
/// (ADR-0207 §4/§5). `default` is kept as a raw, never-parsed string purely
/// so [`apply`] can name-and-reject an attempt to set it, instead of serde
/// silently accepting and ignoring the key.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeTuning {
    #[serde(default)]
    pub default: Option<String>,
    /// Rules to add to the mode's allow list.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Rules to add to the mode's deny list.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Rules to add to the mode's prompt list — the config spelling of
    /// `Permission::Ask` (ADR-0207 §4).
    #[serde(default)]
    pub prompt: Vec<String>,
}

/// Apply `tuning` over `mode` (an already-resolved built-in), returning the
/// tuned mode or a load error naming `mode.name` and the offending key.
/// `capability_of` resolves a tool name to its declared capabilities; a name
/// it doesn't recognize is treated as carrying none, since an unknown tool
/// name is a dispatch-time error, never a reason to reject a tuning rule
/// here.
pub fn apply(
    mode: &Mode,
    tuning: &ModeTuning,
    capability_of: &dyn Fn(&str) -> Option<&'static [Capability]>,
) -> Result<Mode> {
    if tuning.default.is_some() {
        bail!(
            "mode '{}': tuning cannot change the default grade",
            mode.name
        );
    }
    for key in tuning.allow.iter().chain(tuning.prompt.iter()) {
        reject_if_weakens_class_deny(mode, key, capability_of)?;
    }

    let mut rules = mode.rules.clone();
    for key in &tuning.deny {
        rules.push(key, Permission::Deny);
    }
    for key in &tuning.allow {
        rules.push(key, Permission::Allow);
    }
    for key in &tuning.prompt {
        rules.push(key, Permission::Ask);
    }

    Ok(Mode {
        name: mode.name.clone(),
        default: mode.default,
        rules,
        limits: mode.limits,
        sandbox: mode.sandbox.clone(),
        sandbox_network: mode.sandbox_network,
    })
}

/// A rule is a weakening attempt iff it grants (via `allow` or `prompt`) a
/// capability the mode already class-denies: either the key names the class
/// directly (bare `write`), or it's a tool rule whose named tool carries
/// that capability (`write(*)` names the literal tool `write`, resolved
/// through `capability_of` — ADR-0207 §4 has no class-scoped grammar, so a
/// scoped key never names a class).
fn reject_if_weakens_class_deny(
    mode: &Mode,
    key: &str,
    capability_of: &dyn Fn(&str) -> Option<&'static [Capability]>,
) -> Result<()> {
    if let Some(class) = capability_class(key) {
        if mode.rules.class_is_denied(class) {
            bail!(
                "mode '{}': tuning cannot weaken the class-deny on '{key}'",
                mode.name
            );
        }
        return Ok(());
    }
    if let Some(capabilities) = capability_of(rule_tool_name(key)) {
        if capabilities.iter().any(|c| mode.rules.class_is_denied(*c)) {
            bail!(
                "mode '{}': tuning rule '{key}' grants a capability class-denied in this mode",
                mode.name
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::builtin;

    fn research() -> Mode {
        builtin::modes()
            .expect("built-ins parse")
            .into_iter()
            .find(|m| m.name == "research")
            .expect("research exists")
    }

    /// A minimal stand-in for stage 4's registry-backed resolver: enough to
    /// exercise the guard without a `ToolRegistry` in this self-contained
    /// module.
    fn capability_of(name: &str) -> Option<&'static [Capability]> {
        match name {
            "write" | "edit" => Some(&[Capability::Write]),
            "bash" | "call" => Some(&[Capability::Exec]),
            "read" => Some(&[Capability::Read]),
            _ => None,
        }
    }

    #[test]
    fn adding_a_scoped_exec_allow_is_accepted() {
        let tuning = ModeTuning {
            allow: vec!["bash(cargo check)".to_string()],
            ..Default::default()
        };
        let tuned = apply(&research(), &tuning, &capability_of)
            .expect("this is exactly the ADR §5 example");
        assert_eq!(
            tuned.resolve("bash", &[Capability::Exec], Some("cargo check"), None),
            Permission::Allow
        );
    }

    #[test]
    fn widening_allow_with_a_class_denied_class_name_is_rejected() {
        let tuning = ModeTuning {
            allow: vec!["write".to_string()],
            ..Default::default()
        };
        let err = apply(&research(), &tuning, &capability_of)
            .expect_err("research denies the write class; tuning must not undo that");
        assert!(err.to_string().contains("write"));
    }

    #[test]
    fn widening_allow_with_a_scoped_rule_over_the_class_deny_is_rejected() {
        // The hole this guard exists for: "write(*)" names no class, but
        // resolves (via capability_of) to the same Capability::Write that
        // research's built-in `deny: [write]` already forbids — and under
        // longest-match a 13-character scoped rule would out-rank the
        // 5-character class deny if this weren't rejected.
        let tuning = ModeTuning {
            allow: vec!["write(*)".to_string()],
            ..Default::default()
        };
        let err = apply(&research(), &tuning, &capability_of)
            .expect_err("research: allow: [\"write(*)\"] must be rejected");
        assert!(err.to_string().contains("write(*)"));
    }

    #[test]
    fn widening_prompt_with_a_class_denied_capability_is_rejected() {
        let tuning = ModeTuning {
            prompt: vec!["write(*)".to_string()],
            ..Default::default()
        };
        apply(&research(), &tuning, &capability_of)
            .expect_err("prompt widening must be guarded exactly like allow widening");
    }

    #[test]
    fn changing_default_is_rejected() {
        let tuning = ModeTuning {
            default: Some("allow".to_string()),
            ..Default::default()
        };
        apply(&research(), &tuning, &capability_of).expect_err("default is not tunable");
    }

    #[test]
    fn adding_a_deny_rule_is_never_guarded() {
        let tuning = ModeTuning {
            deny: vec!["bash(curl *)".to_string()],
            ..Default::default()
        };
        let tuned = apply(&research(), &tuning, &capability_of).expect("narrowing always allowed");
        assert_eq!(
            tuned.resolve("bash", &[Capability::Exec], Some("curl x"), None),
            Permission::Deny
        );
    }

    #[test]
    fn a_rule_naming_an_unrecognized_tool_is_never_guarded() {
        let tuning = ModeTuning {
            allow: vec!["mcp__foo__bar".to_string()],
            ..Default::default()
        };
        apply(&research(), &tuning, &capability_of)
            .expect("an unresolvable tool name carries no known capability to weaken");
    }

    #[test]
    fn adding_two_tool_allows_round_trips_through_resolve() {
        let tuning = ModeTuning {
            allow: vec![
                "bash(cargo check)".to_string(),
                "bash(cargo test *)".to_string(),
            ],
            ..Default::default()
        };
        let tuned = apply(&research(), &tuning, &capability_of).expect("both accepted");
        assert_eq!(
            tuned.resolve("bash", &[Capability::Exec], Some("cargo test --lib"), None),
            Permission::Allow
        );
    }
}
