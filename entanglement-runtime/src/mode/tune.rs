//! Apply a `config.yml` `modes:` tuning entry over a built-in mode
//! (ADR-0207 §5): it may add or remove individual tool and argument-scoped
//! rules, but may never change the mode's `default` grade and may never
//! weaken a capability-class `deny`. That second guard is what makes
//! "research cannot write" a fact about the binary rather than a fact about
//! whatever a machine's `config.yml` happens to say today.
//!
//! Scope note: the guard below is a literal reading of ADR-0207 §5's two
//! examples — it rejects a bare capability-class name (`write`) landing in
//! `allow` (or being pulled out of `deny`) when that class is already
//! class-denied. It does **not** trace a *scoped* tool rule
//! (`write(pattern)`) back to the literal tool's own capabilities — doing
//! that needs the [`crate::tools::ToolRegistry`], which this self-contained
//! module doesn't have. The built-in `plan` mode relies on exactly this
//! shape (`deny: [write]` + `allow: ["write(.entanglement/plans/*.md)"]`),
//! so closing the gap for user tuning too is left to whichever later stage
//! wires config loading through the registry — flagged here rather than
//! silently assumed closed.

use anyhow::{bail, Result};
use entanglement_core::Permission;
use serde::Deserialize;

use super::rules::{capability_class, Rules};
use super::Mode;

/// One mode's tuning entry from `config.yml`'s `modes:` block — the same
/// `deny`/`allow` list shape as [`super::builtin::RawMode`], plus the
/// remove-lists a tuning entry alone needs, minus `default`'s being
/// mandatory (kept optional here purely so [`apply`] can name-and-reject an
/// attempt to set it, instead of serde silently accepting and ignoring the
/// key).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeTuning {
    #[serde(default)]
    pub default: Option<Permission>,
    /// Rules to add to the mode's allow list.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Rules to remove from the mode's allow list, by exact key.
    #[serde(default)]
    pub allow_remove: Vec<String>,
    /// Rules to add to the mode's deny list.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Rules to remove from the mode's deny list, by exact key.
    #[serde(default)]
    pub deny_remove: Vec<String>,
}

/// Apply `tuning` over `mode` (an already-resolved built-in), returning the
/// tuned mode or a load error naming `mode.name` and the offending key.
pub fn apply(mode: &Mode, tuning: &ModeTuning) -> Result<Mode> {
    if tuning.default.is_some() {
        bail!(
            "mode '{}': tuning cannot change the default grade",
            mode.name
        );
    }
    for key in &tuning.allow {
        reject_if_weakens_class_deny(mode, key)?;
    }
    for key in &tuning.deny_remove {
        reject_if_weakens_class_deny(mode, key)?;
    }

    let mut rules = mode.rules.clone();
    for key in &tuning.allow {
        add_rule(&mut rules, key, Permission::Allow);
    }
    for key in &tuning.deny {
        add_rule(&mut rules, key, Permission::Deny);
    }
    remove_rules(&mut rules, &tuning.allow_remove, Permission::Allow);
    remove_rules(&mut rules, &tuning.deny_remove, Permission::Deny);

    Ok(Mode {
        name: mode.name.clone(),
        default: mode.default,
        rules,
        limits: mode.limits,
        sandbox: mode.sandbox.clone(),
    })
}

/// A capability-class key is a weakening attempt iff the class it names is
/// already in `mode`'s built-in `deny_classes` — whether it arrives by
/// widening `allow` or by shrinking `deny`.
fn reject_if_weakens_class_deny(mode: &Mode, key: &str) -> Result<()> {
    if let Some(class) = capability_class(key) {
        if mode.rules.deny_classes.contains(&class) {
            bail!(
                "mode '{}': tuning cannot weaken the class-deny on '{key}'",
                mode.name
            );
        }
    }
    Ok(())
}

fn add_rule(rules: &mut Rules, key: &str, perm: Permission) {
    let class = capability_class(key);
    match (class, perm) {
        (Some(class), Permission::Allow) => push_unique(&mut rules.allow_classes, class),
        (Some(class), Permission::Deny) => push_unique(&mut rules.deny_classes, class),
        (None, Permission::Allow) => rules.allow_tools.push(key.to_string()),
        (None, Permission::Deny) => rules.deny_tools.push(key.to_string()),
        (_, Permission::Ask) => unreachable!("add_rule is only ever called with Allow or Deny"),
    }
}

fn push_unique<T: PartialEq>(list: &mut Vec<T>, item: T) {
    if !list.contains(&item) {
        list.push(item);
    }
}

fn remove_rules(rules: &mut Rules, keys: &[String], perm: Permission) {
    for key in keys {
        match (capability_class(key), perm) {
            (Some(class), Permission::Allow) => rules.allow_classes.retain(|c| *c != class),
            (Some(class), Permission::Deny) => rules.deny_classes.retain(|c| *c != class),
            (None, Permission::Allow) => rules.allow_tools.retain(|k| k != key),
            (None, Permission::Deny) => rules.deny_tools.retain(|k| k != key),
            (_, Permission::Ask) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capability;
    use crate::mode::builtin;

    fn research() -> Mode {
        builtin::modes()
            .expect("built-ins parse")
            .into_iter()
            .find(|m| m.name == "research")
            .expect("research exists")
    }

    #[test]
    fn adding_a_scoped_exec_allow_is_accepted() {
        let tuning = ModeTuning {
            allow: vec!["bash(cargo check)".to_string()],
            ..Default::default()
        };
        let tuned = apply(&research(), &tuning).expect("this is exactly the ADR §5 example");
        assert!(tuned
            .rules
            .allow_tools
            .iter()
            .any(|k| k == "bash(cargo check)"));
    }

    #[test]
    fn widening_allow_with_a_class_denied_class_is_rejected() {
        let tuning = ModeTuning {
            allow: vec!["write".to_string()],
            ..Default::default()
        };
        let err = apply(&research(), &tuning)
            .expect_err("research denies the write class; tuning must not undo that");
        assert!(err.to_string().contains("write"));
    }

    #[test]
    fn removing_a_class_deny_is_rejected() {
        let tuning = ModeTuning {
            deny_remove: vec!["write".to_string()],
            ..Default::default()
        };
        apply(&research(), &tuning).expect_err("deny_remove on a class-denied key must fail");
    }

    #[test]
    fn changing_default_is_rejected() {
        let tuning = ModeTuning {
            default: Some(Permission::Allow),
            ..Default::default()
        };
        apply(&research(), &tuning).expect_err("default is not tunable");
    }

    #[test]
    fn removing_a_non_class_deny_entry_is_allowed() {
        let tuning = ModeTuning {
            deny_remove: vec!["bash(rm -rf /*)".to_string()],
            ..Default::default()
        };
        // research's built-in deny list has no such entry; removal of a
        // tool-rule (not a class) must never be guarded, present or not.
        apply(&research(), &tuning).expect("removing a non-class deny entry is always allowed");
    }

    #[test]
    fn adding_and_removing_a_tool_allow_round_trips() {
        let add = ModeTuning {
            allow: vec!["bash(cargo check)".to_string()],
            ..Default::default()
        };
        let tuned = apply(&research(), &add).expect("add accepted");
        let remove = ModeTuning {
            allow_remove: vec!["bash(cargo check)".to_string()],
            ..Default::default()
        };
        let untuned = apply(&tuned, &remove).expect("remove accepted");
        assert!(!untuned
            .rules
            .allow_tools
            .iter()
            .any(|k| k == "bash(cargo check)"));
    }

    #[test]
    fn adding_an_already_present_class_allow_does_not_duplicate() {
        let tuning = ModeTuning {
            allow: vec!["read".to_string()],
            ..Default::default()
        };
        let tuned = apply(&research(), &tuning).expect("read is already class-allowed");
        assert_eq!(
            tuned
                .rules
                .allow_classes
                .iter()
                .filter(|c| **c == Capability::Read)
                .count(),
            1
        );
    }
}
