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

/// The read-only exec allowlist shared by `research`, `plan` and `auto`
/// (not `build`, which class-allows `exec` outright). A plain YAML list —
/// not a mapping merge-keyed into each mode file — because YAML's `<<`
/// merge grammar only merges mappings; splicing a *sequence* into another
/// mode's `allow` list still has to happen somewhere, and doing it here in
/// Rust after each mode's own YAML parses is the whole mechanism, with no
/// anchor/alias indirection for a reader of the mode files to untangle.
const READONLY_EXEC_YML: &str = include_str!("builtin/readonly_exec.yml");

/// The `prompt`-list counterpart of [`READONLY_EXEC_YML`]: mutating
/// `gh api`/`glab api` spellings that must out-rank a broader allow under
/// longest-match — the shared `bash(gh api *)`/`bash(glab api *)` allow in
/// `research`/`plan` (via `READONLY_EXEC_YML`) and the bare `exec` class
/// allow in `build`. Spliced into `prompt`, never `allow` — see the file's
/// own header for why `auto` doesn't use it.
const READONLY_EXEC_PROMPT_YML: &str = include_str!("builtin/readonly_exec_prompt.yml");

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

/// Parse `yaml`, extending its `allow` list with `extra_allow` and its
/// `prompt` list with `extra_prompt` — the two shared splices (empty slices
/// for a mode that needs none of its own).
fn parse(name: &str, yaml: &str, extra_allow: &[String], extra_prompt: &[String]) -> Result<Mode> {
    let raw: RawMode = serde_yaml::from_str(yaml)
        .with_context(|| format!("mode '{name}': invalid built-in YAML"))?;
    let mut allow = raw.allow;
    allow.extend(extra_allow.iter().cloned());
    let mut prompt = raw.prompt;
    prompt.extend(extra_prompt.iter().cloned());
    Ok(Mode {
        name: name.to_string(),
        default: raw.default,
        rules: Rules::from_lists(&raw.deny, &allow, &prompt),
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
    let readonly_exec: Vec<String> = serde_yaml::from_str(READONLY_EXEC_YML)
        .context("built-in readonly_exec.yml: invalid YAML")?;
    let readonly_exec_prompt: Vec<String> = serde_yaml::from_str(READONLY_EXEC_PROMPT_YML)
        .context("built-in readonly_exec_prompt.yml: invalid YAML")?;
    Ok(vec![
        parse(
            "research",
            RESEARCH_YML,
            &readonly_exec,
            &readonly_exec_prompt,
        )?,
        parse("plan", PLAN_YML, &readonly_exec, &readonly_exec_prompt)?,
        parse("build", BUILD_YML, &[], &readonly_exec_prompt)?,
        parse("auto", AUTO_YML, &readonly_exec, &[])?,
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
    fn research_denies_write_and_plan_and_allows_read() {
        let research = parse("research", RESEARCH_YML, &[], &[]).expect("parses");
        assert_eq!(research.default, Permission::Ask);
        assert_eq!(
            research.resolve("edit", &[Capability::Write], None, None),
            Permission::Deny
        );
        assert_eq!(
            research.resolve("write", &[Capability::Write], None, None),
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
    }

    /// The set from `readonly_exec.yml` resolves to `Allow` in all three
    /// modes that include it. None of these commands appear in
    /// `research.yml`/`plan.yml`/`auto.yml` themselves (those files carry no
    /// exec allow list of their own any more), so an `Allow` here can only
    /// come from the one spliced-in shared list — proof it is genuinely
    /// shared, not three independent copies: editing `readonly_exec.yml`
    /// changes all three at once.
    #[test]
    fn shared_readonly_exec_set_resolves_allow_in_research_plan_and_auto() {
        let modes = modes().expect("built-ins parse");
        let commands = [
            "ls", // bare invocation — no arguments
            "git status",
            "git log --oneline",
            "git config --get user.name",
            "sed -i s/a/b/ file.rs", // write-capable, knowingly allowed
            "jq .foo file.json",
        ];
        for mode_name in ["research", "plan", "auto"] {
            let mode = modes
                .iter()
                .find(|m| m.name == mode_name)
                .expect("mode exists");
            for cmd in commands {
                assert_eq!(
                    mode.resolve("bash", &[Capability::Exec], Some(cmd), None),
                    Permission::Allow,
                    "mode '{mode_name}': bash {cmd:?} should be shared-allowed"
                );
                assert_eq!(
                    mode.resolve("call", &[Capability::Exec], Some(cmd), None),
                    Permission::Allow,
                    "mode '{mode_name}': call {cmd:?} should be allowed too"
                );
            }
        }
    }

    /// The shared set enumerates read-only git subcommands rather than
    /// allowing bare `git *`, so `push`/`reset --hard` never resolve
    /// `Allow` — `research`/`plan` have no rule for them at all (falling
    /// through to `default: prompt`), and `auto`'s own destructive deny
    /// list still catches them explicitly.
    #[test]
    fn shared_set_does_not_allow_git_push_or_reset_hard() {
        let modes = modes().expect("built-ins parse");
        let research = modes.iter().find(|m| m.name == "research").expect("exists");
        let plan = modes.iter().find(|m| m.name == "plan").expect("exists");
        let auto = modes.iter().find(|m| m.name == "auto").expect("exists");
        for cmd in ["git push origin main", "git reset --hard HEAD~1"] {
            assert_eq!(
                research.resolve("bash", &[Capability::Exec], Some(cmd), None),
                Permission::Ask,
                "research: {cmd:?} must not be shared-allowed"
            );
            assert_eq!(
                plan.resolve("bash", &[Capability::Exec], Some(cmd), None),
                Permission::Ask,
                "plan: {cmd:?} must not be shared-allowed"
            );
            assert_eq!(
                auto.resolve("bash", &[Capability::Exec], Some(cmd), None),
                Permission::Deny,
                "auto: {cmd:?} must still be explicitly denied"
            );
        }
    }

    /// The full gh/glab + network-mutating matrix, run through the real
    /// `Mode::resolve` (never eyeballed against the YAML) — a pattern list
    /// that looks right and matches wrong is exactly how the git-branch
    /// widening broke a test this same session (#0 of this change).
    #[test]
    fn gh_glab_and_network_mutating_commands_resolve_as_specified() {
        let modes = modes().expect("built-ins parse");
        let mode = |name: &str| modes.iter().find(|m| m.name == name).expect("mode exists");
        let (research, plan, build, auto) =
            (mode("research"), mode("plan"), mode("build"), mode("auto"));
        let resolve = |m: &Mode, cmd: &str| m.resolve("bash", &[Capability::Exec], Some(cmd), None);

        // Read-only gh/glab lookups: curated-allowed everywhere, including
        // the plain (non-mutating) `gh api` spelling.
        for cmd in ["gh issue list", "gh api repos/o/r", "glab mr list"] {
            for m in [research, plan, auto] {
                assert_eq!(resolve(m, cmd), Permission::Allow, "{}: {cmd:?}", m.name);
            }
            // `build` reaches the same Allow through its bare `exec` class
            // allow, not the curated set it doesn't have.
            assert_eq!(resolve(build, cmd), Permission::Allow, "build: {cmd:?}");
        }

        // A mutating `gh api`/`glab api` spelling must not ride the broad
        // `api *` allow through — it must escalate (research/plan/build ask,
        // auto denies outright), in both the `-X` and `--method` spellings.
        for cmd in [
            "gh api -X DELETE repos/o/r",
            "gh api --method DELETE repos/o/r",
            "glab api -X POST projects/1/issues",
        ] {
            assert_ne!(
                resolve(research, cmd),
                Permission::Allow,
                "research: {cmd:?}"
            );
            assert_eq!(resolve(research, cmd), Permission::Ask, "research: {cmd:?}");
            assert_eq!(resolve(plan, cmd), Permission::Ask, "plan: {cmd:?}");
            assert_eq!(resolve(build, cmd), Permission::Ask, "build: {cmd:?}");
            assert_eq!(resolve(auto, cmd), Permission::Deny, "auto: {cmd:?}");
        }

        // Plain `git push`: prompts in build (out-ranking its bare `exec`
        // allow), denied outright in auto.
        assert_eq!(resolve(build, "git push"), Permission::Ask);
        assert_eq!(resolve(auto, "git push"), Permission::Deny);
        // The destructive force-push spelling stays a hard `deny` in build
        // too — a longer `prompt` rule could never win against it anyway.
        assert_eq!(resolve(build, "git push --force"), Permission::Deny);

        // `cargo publish`: prompts in build, denied in auto even though
        // auto's own `allow` has a broad `bash(cargo *)`.
        assert_eq!(resolve(build, "cargo publish"), Permission::Ask);
        assert_eq!(resolve(auto, "cargo publish"), Permission::Deny);

        // A plain read-only command is unaffected by any of the above.
        for m in [research, plan, auto, build] {
            assert_eq!(
                resolve(m, "ls -la"),
                Permission::Allow,
                "{}: ls -la",
                m.name
            );
        }
    }

    #[test]
    fn plan_allows_plan_class_and_the_plans_folder_carve_out() {
        let plan = parse("plan", PLAN_YML, &[], &[]).expect("parses");
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
        let build = parse("build", BUILD_YML, &[], &[]).expect("parses");
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
        let auto = parse("auto", AUTO_YML, &[], &[]).expect("parses");
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
