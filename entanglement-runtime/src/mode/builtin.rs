//! The four built-in modes (ADR-0207 §2/§4/§11), embedded as YAML
//! (`include_str!`) so their shape matches the `config.yml` `modes:` tuning
//! block exactly (ADR-0207 §5, see [`super::tune`]) — `skutter` compiles
//! these in and reads no `modes/` directory, ever: a user tunes a mode, but
//! can never define, add or remove one.

use anyhow::{Context, Result};
use entanglement_core::Permission;
use serde::Deserialize;

use super::limits::Limits;
use super::rules::Rules;
use super::{deserialize_grade, Mode};

const RESEARCH_YML: &str = include_str!("builtin/research.yml");
const PLAN_YML: &str = include_str!("builtin/plan.yml");
const BUILD_YML: &str = include_str!("builtin/build.yml");
const AUTO_YML: &str = include_str!("builtin/auto.yml");

/// The on-disk shape of one mode's YAML — identical to a `config.yml`
/// `modes:` tuning entry ([`super::tune::ModeTuning`]) except `default` is
/// mandatory here and optional (reject-if-present) there. No
/// `deny_unknown_fields` here: serde doesn't support it alongside
/// `#[serde(flatten)]` (the `limits` field below) — [`Limits`] itself still
/// rejects an unknown *limit* key, and the four embedded files are pinned by
/// the parse tests, not user input.
#[derive(Debug, Deserialize)]
pub(super) struct RawMode {
    #[serde(deserialize_with = "deserialize_grade")]
    pub default: Permission,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub prompt: Vec<String>,
    #[serde(default)]
    pub sandbox: Option<String>,
    /// Share the host network namespace with a confined `bash`/`call`
    /// (ADR-0207 §6, stage 5b) — ignored when `sandbox` is unset. None of
    /// the four built-ins opt in, matching the ADR-0104 default-closed
    /// egress policy.
    #[serde(default)]
    pub sandbox_network: bool,
    #[serde(flatten)]
    pub limits: Limits,
}

fn parse(name: &str, yaml: &str) -> Result<Mode> {
    let raw: RawMode = serde_yaml::from_str(yaml)
        .with_context(|| format!("mode '{name}': invalid built-in YAML"))?;
    Ok(Mode {
        name: name.to_string(),
        default: raw.default,
        rules: Rules::from_lists(&raw.deny, &raw.allow, &raw.prompt),
        limits: raw.limits,
        sandbox: raw.sandbox,
        sandbox_network: raw.sandbox_network,
    })
}

/// Parse the four embedded built-ins. Only fails on a broken embedded YAML
/// file — i.e. a bug in this crate, never user input — so callers are free
/// to treat it as effectively infallible (stage 4's dispatch wiring can
/// still propagate the `Result` rather than unwrap it).
pub(super) fn modes() -> Result<Vec<Mode>> {
    Ok(vec![
        parse("research", RESEARCH_YML)?,
        parse("plan", PLAN_YML)?,
        parse("build", BUILD_YML)?,
        parse("auto", AUTO_YML)?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capability;

    #[test]
    fn all_four_builtins_parse() {
        let modes = modes().expect("built-in YAML must parse");
        let names: Vec<&str> = modes.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["research", "plan", "build", "auto"]);
    }

    #[test]
    fn research_denies_write_and_allows_read_and_curated_exec() {
        let research = parse("research", RESEARCH_YML).expect("parses");
        assert_eq!(research.default, Permission::Ask);
        assert_eq!(
            research.resolve("edit", &[Capability::Write], None, None),
            Permission::Deny
        );
        assert_eq!(
            research.resolve("propose_plan", &[Capability::Plan], None, None),
            Permission::Deny
        );
        assert_eq!(
            research.resolve("read", &[Capability::Read], None, None),
            Permission::Allow
        );
        for verb in ["find", "grep", "rg", "ls", "cat", "head", "tail", "wc"] {
            assert_eq!(
                research.resolve(
                    "bash",
                    &[Capability::Exec],
                    Some(&format!("{verb} x")),
                    None
                ),
                Permission::Allow,
                "bash {verb} must be curated-allowed"
            );
            assert_eq!(
                research.resolve(
                    "call",
                    &[Capability::Exec],
                    Some(&format!("{verb} x")),
                    None
                ),
                Permission::Allow,
                "call {verb} must be allowed too — bash/call share one rule set"
            );
        }
    }

    #[test]
    fn plan_allows_plan_class_and_the_plans_folder_carve_out() {
        let plan = parse("plan", PLAN_YML).expect("parses");
        assert_eq!(
            plan.resolve("propose_plan", &[Capability::Plan], None, None),
            Permission::Allow
        );
        assert_eq!(
            plan.resolve(
                "write",
                &[Capability::Write],
                Some(".entanglement/plans/phase1.md"),
                None
            ),
            Permission::Allow
        );
        assert_eq!(
            plan.resolve("write", &[Capability::Write], Some("README.md"), None),
            Permission::Deny
        );
    }

    #[test]
    fn build_defaults_to_prompt_unlike_the_old_build_agent() {
        let build = parse("build", BUILD_YML).expect("parses");
        assert_eq!(build.default, Permission::Ask);
        assert_eq!(
            build.resolve("edit", &[Capability::Write], None, None),
            Permission::Allow
        );
        assert_eq!(
            build.resolve("propose_plan", &[Capability::Plan], None, None),
            Permission::Deny
        );
        // bash and call share the destructive deny list even though the
        // YAML only spells it as `bash(...)` (ADR-0207 §4).
        assert_eq!(
            build.resolve("call", &[Capability::Exec], Some("rm -rf /"), None),
            Permission::Deny
        );
    }

    #[test]
    fn auto_defaults_to_deny_with_a_bounded_question_timeout() {
        let auto = parse("auto", AUTO_YML).expect("parses");
        assert_eq!(auto.default, Permission::Deny);
        assert_eq!(auto.limits.question_timeout, 60);
    }

    #[test]
    fn ask_is_no_longer_a_valid_default_spelling() {
        let err = serde_yaml::from_str::<RawMode>("default: ask\n")
            .expect_err("the config surface spells Permission::Ask as 'prompt', not 'ask'");
        assert!(err.to_string().contains("prompt"));
    }
}
