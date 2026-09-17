//! Permission modes (#560, ADR-0207): the mode table and its rule engine,
//! built on the capability vocabulary ([`crate::capability`]). Wired into
//! the dispatch ladder via `crate::policy::ProfileResolver` (stage 4): a
//! session's mode, folded from `OutEvent::ModeChanged`, resolves every
//! call's grade here instead of through `AgentProfile`.
//!
//! A [`Mode`] is a resolved grade table: a `default` grade, [`Rules`], run
//! [`Limits`], and an optional sandbox posture. [`ModeTable`] holds a named
//! set of them — the runtime's own four built-ins ([`ModeTable::builtin`]),
//! or an embedder's own table via [`ModeTable::new`] (ADR-0207 §2: "An
//! embedder building on the library supplies its own table").
//!
//! The table is **code, not configuration**: `skutter` compiles in
//! `research`/`plan`/`build`/`auto` and reads no `modes/` directory, ever. A
//! `config.yml` `modes:` block only *tunes* one of them ([`tune::apply`]) —
//! it can never define, add, or remove a mode, and it can only ever **add**
//! rules: [`Rules`] resolves by longest matching key (ADR-0207 §4, revised
//! from an earlier tier-based precedence), so a shipped rule is overridden
//! by adding a longer, more specific one rather than by removing it.

mod builtin;
mod limits;
mod rules;
mod tune;

pub use limits::{Limits, OnTimeout};
pub use rules::Rules;
pub use tune::{apply as apply_tuning, ModeTuning};

use anyhow::{bail, Result};
use entanglement_core::Permission;
use serde::Deserialize;

use crate::capability::Capability;

/// Deserialize the config-surface spelling of a grade: `allow`/`deny`/
/// `prompt`. `prompt` is the YAML word for core's `Permission::Ask`
/// (ADR-0207 §4: "the config surface uses the word a user thinks in, the
/// enum keeps core's name") — used for the `default:` field in both a
/// built-in mode's YAML ([`builtin::RawMode`]) and nowhere else, since
/// `deny`/`allow`/`prompt` are list *names*, not scalar values, everywhere
/// else they appear. Any other spelling (including the enum's own `ask`) is
/// a config error, not a silent fallback.
pub(crate) fn deserialize_grade<'de, D>(deserializer: D) -> Result<Permission, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    match raw.as_str() {
        "allow" => Ok(Permission::Allow),
        "deny" => Ok(Permission::Deny),
        "prompt" => Ok(Permission::Ask),
        other => Err(serde::de::Error::custom(format!(
            "invalid grade '{other}': expected 'allow', 'deny', or 'prompt'"
        ))),
    }
}

/// One resolved permission mode (ADR-0207 §2/§4): a name, the grade used
/// when nothing else matches, its rule table, its run limits, and its
/// sandbox posture — `sandbox`: `Some("bwrap")`/`Some("bubblewrap")` confines
/// every `bash`/`call` the whole spawn sub-tree makes, `None` leaves it
/// unsandboxed; `sandbox_network` shares the host network namespace with a
/// confined command (ignored when `sandbox` is `None`). Stage 5b moved
/// confinement here from the old per-profile `AgentProfile::sandbox` — a
/// mode applies to its whole spawn sub-tree (ADR-0207 §6), so there is no
/// more per-session ancestor floor to freeze at spawn: every session under
/// one mode gets the identical policy.
#[derive(Debug, Clone, PartialEq)]
pub struct Mode {
    pub name: String,
    pub default: Permission,
    pub rules: Rules,
    pub limits: Limits,
    pub sandbox: Option<String>,
    pub sandbox_network: bool,
}

impl Mode {
    /// This mode's confinement policy for `bash`/`call` (ADR-0207 §6, stage
    /// 5b): `sandbox`/`sandbox_network` translated into a
    /// [`crate::host::SandboxPolicy`]. `ENTANGLEMENT_SANDBOX`/
    /// `ENTANGLEMENT_SANDBOX_NETWORK` are then layered on top via
    /// [`SandboxPolicy::most_confined`] — env may only tighten what the mode
    /// declares, never loosen it, which `most_confined` gives for free since
    /// it always picks the more-confined of the two.
    pub fn sandbox_policy(&self) -> crate::host::SandboxPolicy {
        use crate::host::{SandboxBackend, SandboxPolicy};
        let backend = match self.sandbox.as_deref() {
            Some("bwrap") | Some("bubblewrap") => SandboxBackend::Bubblewrap,
            _ => SandboxBackend::None,
        };
        SandboxPolicy {
            backend,
            network: self.sandbox_network,
        }
    }
}

impl Mode {
    /// Resolve the grade for one call under this mode. `capabilities` is the
    /// tool's own [`Capability`] slice ([`crate::capability::capability_of`]);
    /// `arg`/`workdir` are the same per-call scoping inputs
    /// `PermissionProfile::resolve_scoped` already takes (ADR-0051/ADR-0116).
    pub fn resolve(
        &self,
        tool_name: &str,
        capabilities: &[Capability],
        arg: Option<&str>,
        workdir: Option<&str>,
    ) -> Permission {
        rules::resolve(
            &self.rules,
            self.default,
            tool_name,
            capabilities,
            arg,
            workdir,
        )
    }
}

/// A named set of modes. Construction validates only that names are unique
/// — two modes named `research` would make [`ModeTable::get`] silently pick
/// one, which is a worse failure than refusing to build the table at all.
#[derive(Debug, Clone, PartialEq)]
pub struct ModeTable {
    modes: Vec<Mode>,
}

impl ModeTable {
    /// The runtime's own four built-ins: `research`, `plan`, `build`,
    /// `auto` (ADR-0207 §2).
    pub fn builtin() -> Result<Self> {
        Self::new(builtin::modes()?)
    }

    /// Build a table from an arbitrary mode list — the seam an embedder uses
    /// to supply its own table instead of the built-in four (ADR-0207 §2).
    pub fn new(modes: Vec<Mode>) -> Result<Self> {
        for (i, mode) in modes.iter().enumerate() {
            if modes[..i].iter().any(|m| m.name == mode.name) {
                bail!("duplicate mode name '{}'", mode.name);
            }
        }
        Ok(Self { modes })
    }

    pub fn get(&self, name: &str) -> Option<&Mode> {
        self.modes.iter().find(|m| m.name == name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.modes.iter().map(|m| m.name.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The task's own worked example (ADR-0207 §4), pinned against the real
    /// built-in `research` mode rather than a synthetic one.
    #[test]
    fn research_matches_the_worked_example() {
        let table = ModeTable::builtin().expect("built-ins parse");
        let research = table.get("research").expect("research exists");

        assert_eq!(
            research.resolve("edit", &[Capability::Write], None, None),
            Permission::Deny,
            "edit -> Deny (Write is class-denied)"
        );
        assert_eq!(
            research.resolve("read", &[Capability::Read], None, None),
            Permission::Allow,
            "read -> Allow (Read is class-allowed)"
        );
        assert_eq!(
            research.resolve("bash", &[Capability::Exec], Some("find ."), None),
            Permission::Allow,
            "bash find . -> Allow (bash(find *) scoped rule)"
        );
        assert_eq!(
            research.resolve("bash", &[Capability::Exec], Some("curl x"), None),
            Permission::Ask,
            "bash curl x -> Ask (the default)"
        );
    }

    #[test]
    fn builtin_table_has_exactly_the_four_modes() {
        let table = ModeTable::builtin().expect("built-ins parse");
        let mut names: Vec<&str> = table.names().collect();
        names.sort_unstable();
        assert_eq!(names, vec!["auto", "build", "plan", "research"]);
    }

    #[test]
    fn duplicate_mode_names_are_rejected() {
        let base = ModeTable::builtin().expect("built-ins parse");
        let research = base.get("research").expect("research exists").clone();
        let dup = research.clone();
        assert!(ModeTable::new(vec![research, dup]).is_err());
    }

    #[test]
    fn embedder_can_supply_its_own_table() {
        let custom = Mode {
            name: "custom".to_string(),
            default: Permission::Deny,
            rules: Rules::default(),
            limits: Limits::default(),
            sandbox: None,
            sandbox_network: false,
        };
        let table = ModeTable::new(vec![custom]).expect("single-mode table is valid");
        assert!(table.get("custom").is_some());
        assert!(table.get("research").is_none());
    }
}
