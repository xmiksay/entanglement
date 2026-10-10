//! File-based agent definitions (#112, ADR-0034).
//!
//! An agent is a markdown file with YAML frontmatter: the frontmatter is the
//! identity bundle (`name`/`description`/`model`/…), the body below the
//! closing `---` is the agent's system-prompt body. Definitions are discovered
//! at startup and folded into a core [`AgentCatalog`].
//!
//! The body is not stored raw: as each definition is parsed it is composed into
//! the final `system_prompt` by [`crate::system_prompt::assemble`] (shared
//! preamble + body + project brief + env block + skill index, #113). Baking the
//! assembled prompt into the registry here keeps every downstream consumer
//! (session start, spawn) a pass-through.
//!
//! # Layers & precedence
//!
//! Three layers, later wins on a `name` collision:
//!
//! 1. **built-in** — embedded [`include_str!`] files (`general`, `plan`,
//!    `debug` — ADR-0207 stage 6a collapsed the five-persona roster:
//!    `build` renamed to `general` (unchanged body, the default worker
//!    persona), `explore`/`research` retired since their read-only posture is
//!    a permission **mode** now, not a persona), parsed through the *same*
//!    loader. Editing a built-in is just dropping a same-`name` file in a
//!    higher layer; there is no special "edit built-ins" code path.
//! 2. **user** — `~/.claude/agents/*.md` (cross-vendor, lenient), then
//!    `${config_dir}/entanglement/agents/*.md` (native, strict).
//! 3. **project** — `.claude/agents` then `.agents/agents` (both lenient), then
//!    `<root>/.entanglement/agents/*.md` (native, strict — highest).
//!
//! Same defaults+override shape as the provider catalog (#118): a malformed
//! *native* user/project file is a loud error, never a silent fallback; the
//! embedded built-ins are guarded by a unit test so their parse is provably
//! infallible. Foreign (cross-vendor) dirs are parsed leniently per ADR-0074:
//! only `name` + `description` are read (unknown keys like Claude Code's
//! `tools: Read, Grep` string, `model`, `color` are ignored) and a malformed
//! file is warned and skipped — it must not abort the load.
//!
//! # Authority left the agent (ADR-0207)
//!
//! `tools`/`disallowed_tools` (the tool mask, #116/ADR-0038), `permission`
//! (#59), `can_spawn`/`spawnable_agents` (#119, ADR-0040), `sandbox`
//! (ADR-0134) and `mode` (primary/subagent/all, ADR-0034) are no longer
//! agent frontmatter keys: authority is a second, independent session axis
//! now — the permission **mode** — not anything an `Agent` carries,
//! and any agent may be a session root or a spawn target (ADR-0207 §4/§6).
//! A definition naming any of those fails to parse (`deny_unknown_fields`),
//! same as any other unrecognized key.
//!
//! # `SetAgent` is gone (ADR-0207 §9, stage 6a)
//!
//! An agent is chosen once, when a session starts, and is fixed for that
//! session's life — a spawned sub-agent is a fresh session with its own
//! system prompt, which is what delegating to a different persona actually
//! needs; there is no live "switch agent" message any more.
//!
//! # Migrating a legacy native-layer file
//!
//! A user/project **native** definition (`${config_dir}/entanglement/agents`,
//! `<root>/.entanglement/agents`) authored before ADR-0207 may still carry the
//! retired authority keys (`migrate::RETIRED_AGENT_KEYS`). Rather than bricking the
//! load on every one of them forever, [`migrate::migrate_legacy_agent_file`] self-heals
//! a **strict**-layer parse failure that is caused by exactly those keys: it
//! backs the file up to `<file>.bak`, rewrites it without them, and warns
//! naming the file and the closest replacement permission **mode** inferred
//! from what the dropped rules allowed. A parse failure the retired keys
//! don't explain (a genuine typo) still aborts loudly — this must never paper
//! over a mistake by guessing. Foreign (lenient) files are unaffected: they
//! never carried these keys as anything but ignored noise (ADR-0074).

mod discover;
mod migrate;
mod parse;

use std::path::Path;

use anyhow::{Context, Result};
use entanglement_core::{Agent, AgentCatalog};

use crate::mcp::McpCapabilityIndex;
use crate::skills::SkillRegistry;
use crate::system_prompt::{assemble_parts, PromptContext, PromptPart};
use discover::{discover, BUILT_INS};
use parse::{build_agent, parse_definition, parse_raw, resolve_preload, AgentDefinition};

/// Load the agent registry for `root`: embedded built-ins, then the user dir,
/// then the project dir — later layers replace earlier ones on a `name`
/// collision (project > user > built-in). A malformed file in any layer aborts.
///
/// `ctx` carries the deterministic system-prompt inputs (shared preamble,
/// project brief, environment block, skill index): each agent's body is
/// composed into a final `system_prompt` via [`assemble`] as it is parsed
/// (#113). Pass [`PromptContext::default`] for the raw, un-composed bodies.
///
/// `mcp` is the config-side MCP capability index (#426,
/// `entanglement_runtime::mcp::capability_index`): a bare `read`/`write`/`call`
/// permission key fans out to these namespaced MCP tool names in addition to
/// the fixed built-in set. Pass an empty index when no MCP capability fan-out
/// applies (e.g. `McpCapabilityIndex::new()`).
pub fn load_registry(
    root: &Path,
    ctx: &PromptContext,
    skills: &SkillRegistry,
    mcp: &McpCapabilityIndex,
) -> Result<AgentCatalog> {
    let mut reg = AgentCatalog::default();
    // Track the winning (layer, source) per name so a later-wins collision is no
    // longer silent (#185): emit a `replaces=<prior source>` debug at the
    // overwrite, matching the provenance `inspect agents` surfaces.
    let mut winning: std::collections::HashMap<String, (AgentLayer, String)> =
        std::collections::HashMap::new();
    for raw in discover(root)? {
        // Later layers replace earlier ones on a `name` collision because
        // `discover` yields them in precedence order and `insert` overwrites.
        let Some((def, body)) = parse_raw(&raw)? else {
            continue;
        };
        let agent = build_agent(def, &body, ctx, skills, mcp)
            .with_context(|| format!("parsing agent `{}`", raw.source))?;
        if let Some((prior_layer, prior_source)) =
            winning.insert(agent.name.clone(), (raw.layer, raw.source.clone()))
        {
            tracing::debug!(
                agent = %agent.name,
                layer = raw.layer.label(),
                replaces = %format!("{} ({})", prior_layer.label(), prior_source),
                source = %raw.source,
                "agent definition overrides a lower layer",
            );
        }
        reg.insert(agent);
    }
    Ok(reg)
}

/// Parse *only* the embedded built-in set (`general`/`plan`/`debug`)
/// into a [`AgentCatalog`], skipping the user/project layers
/// [`load_registry`] consults. The runtime is the single source of the
/// built-ins (#201): core carries only the `general` fallback
/// [`AgentCatalog::new`] synthesizes, so callers that need the full set
/// without touching the filesystem parse the embedded markdown here. Prompts
/// are composed with an identity [`PromptContext`] (no brief/env/skills),
/// matching the raw built-in bodies.
///
/// The embedded definitions are exercised by
/// [`tests::built_ins_parse_with_expected_shape`], but a passing test suite is
/// not something this function's own callers can rely on at runtime (#585):
/// embedders call this directly as a public seam, bypassing [`load_registry`]
/// entirely. So a parse failure here is surfaced as a `Result` — same as
/// every other layer — rather than an unconditional panic baked into a
/// library function.
pub fn built_in_registry() -> Result<AgentCatalog> {
    let ctx = PromptContext::default();
    let skills = SkillRegistry::default();
    let mcp = McpCapabilityIndex::new();
    let mut reg = AgentCatalog::default();
    for (file, contents) in BUILT_INS {
        let agent = parse_definition(contents, &ctx, &skills, &mcp)
            .with_context(|| format!("embedded built-in agent `{file}` must parse"))?;
        reg.insert(agent);
    }
    Ok(reg)
}

/// One resolved agent for `skutter inspect agents` (#185): the winning
/// definition plus the provenance the silent `insert` used to swallow — which
/// layer/source won, and every lower-layer definition of the same name it
/// overrode.
pub struct AgentResolution {
    /// The fully assembled winning agent (mode/model pin/spawn posture + prompt).
    pub agent: Agent,
    /// Which precedence layer the winner came from.
    pub layer: AgentLayer,
    /// The winner's origin (`built-in (general.md)` or a file path).
    pub source: String,
    /// Lower-layer definitions of the same name the winner overrode, in
    /// precedence order — `(layer, source)` each. Empty when nothing was shadowed.
    pub shadowed: Vec<(AgentLayer, String)>,
}

/// Resolve every agent for `root` with full provenance (#185), applying the same
/// layer precedence as [`load_registry`] but keeping *which* layer won and what
/// it shadowed. Sorted by name for a stable table. Malformed files behave
/// exactly as at load (native aborts, foreign warns and skips).
///
/// `skutter inspect agents` (unlike [`load_registry`]) doesn't already resolve
/// the user config's MCP servers, so this parses with an empty
/// [`McpCapabilityIndex`] — harmless now that agent parsing carries no
/// permission fan-out of its own (ADR-0207); kept for
/// [`build_agent`]'s shared signature.
pub fn resolve_registry(
    root: &Path,
    ctx: &PromptContext,
    skills: &SkillRegistry,
) -> Result<Vec<AgentResolution>> {
    let mcp = McpCapabilityIndex::new();
    // Preserve first-seen order of names, then sort at the end; group each name's
    // definitions in precedence order so the last is the winner and the rest are
    // what it shadowed.
    let mut order: Vec<String> = Vec::new();
    let mut by_name: std::collections::HashMap<String, Vec<(AgentLayer, String, Agent)>> =
        std::collections::HashMap::new();
    for raw in discover(root)? {
        let Some((def, body)) = parse_raw(&raw)? else {
            continue;
        };
        let agent = build_agent(def, &body, ctx, skills, &mcp)
            .with_context(|| format!("parsing agent `{}`", raw.source))?;
        let name = agent.name.clone();
        let entry = by_name.entry(name.clone()).or_default();
        if entry.is_empty() {
            order.push(name);
        }
        entry.push((raw.layer, raw.source, agent));
    }

    let mut resolved: Vec<AgentResolution> = order
        .into_iter()
        .map(|name| {
            let mut defs = by_name
                .remove(&name)
                .expect("name recorded on first insert");
            let (layer, source, agent) = defs.pop().expect("at least one definition per name");
            let shadowed = defs.into_iter().map(|(l, s, _)| (l, s)).collect();
            AgentResolution {
                agent,
                layer,
                source,
                shadowed,
            }
        })
        .collect();
    resolved.sort_by(|a, b| a.agent.name.cmp(&b.agent.name));
    Ok(resolved)
}

/// Everything `skutter inspect prompt` needs for one agent (#184): the winning
/// definition's source, the assembled agent, and the per-part breakdown.
pub struct AgentPromptReport {
    /// Where the winning definition came from (`built-in (general.md)` or a path).
    pub source: String,
    /// The fully assembled agent (its `system_prompt` is the resolved prompt).
    pub agent: Agent,
    /// The included prompt slices with their sources, in prompt order.
    pub parts: Vec<PromptPart>,
    /// Whether the definition opted into the project brief (`include_brief`).
    pub include_brief: bool,
    /// Whether a brief slice actually made it into the prompt (set *and* found).
    pub brief_included: bool,
}

/// Resolve the winning definition for `agent` (same precedence as
/// [`load_registry`]) and report its assembled prompt plus per-part breakdown,
/// without spawning the engine (#184). `Ok(None)` if no such agent exists;
/// malformed definitions behave exactly as at load (native aborts, foreign
/// warns and skips). Parses with an empty [`McpCapabilityIndex`], kept for
/// [`build_agent`]'s shared signature (#426; harmless now, ADR-0207).
pub fn prompt_report(
    root: &Path,
    agent: &str,
    ctx: &PromptContext,
    skills: &SkillRegistry,
) -> Result<Option<AgentPromptReport>> {
    // Scan in precedence order, keeping the *last* definition whose name matches
    // — the same "later layer wins" rule `load_registry` gets from `insert`.
    let mut winner: Option<(String, AgentDefinition, String)> = None;
    for raw in discover(root)? {
        let Some((def, body)) = parse_raw(&raw)? else {
            continue;
        };
        if def.name == agent {
            winner = Some((raw.source, def, body));
        }
    }
    let Some((source, def, body)) = winner else {
        return Ok(None);
    };

    let include_brief = def.include_brief;
    let preloaded = resolve_preload(def.skills.as_deref().unwrap_or(&[]), &def.name, skills)?;
    let mut parts = assemble_parts(&body, include_brief, ctx, &preloaded);
    // `assemble_parts` labels the body with a generic source; here we know the
    // actual winning file, so point the body part at it.
    for p in parts.iter_mut().filter(|p| p.label == "agent body") {
        p.source = source.clone();
    }
    let brief_included = parts.iter().any(|p| p.label == "project brief");
    let agent = build_agent(def, &body, ctx, skills, &McpCapabilityIndex::new())?;
    Ok(Some(AgentPromptReport {
        source,
        agent,
        parts,
        include_brief,
        brief_included,
    }))
}

/// Which of the three precedence layers a definition came from (#185). The
/// shared [`crate::layers::Layer`] — `built-in < user < project`, later wins on
/// a `name` collision — re-exported under the agents-facing name.
pub use crate::layers::Layer as AgentLayer;

#[cfg(test)]
mod tests {
    use super::discover::RawAgent;
    use super::*;
    use crate::layers::Strictness;

    /// Parse with an identity context + empty skill registry so tests assert the
    /// raw body verbatim (no preload injection).
    fn parse(content: &str) -> Result<Agent> {
        parse_definition(
            content,
            &PromptContext::default(),
            &SkillRegistry::default(),
            &McpCapabilityIndex::new(),
        )
    }

    /// Parse against a supplied skill registry, to exercise `skills:` preload.
    fn parse_with_skills(content: &str, skills: &SkillRegistry) -> Result<Agent> {
        parse_definition(
            content,
            &PromptContext::default(),
            skills,
            &McpCapabilityIndex::new(),
        )
    }

    /// A one-off registry holding a single embedded (built-in shape) skill.
    fn skill_registry(name: &str, user_only: bool, body: &str) -> SkillRegistry {
        use crate::skills::SkillMeta;
        let mut reg = SkillRegistry::default();
        reg.insert(SkillMeta {
            name: name.into(),
            description: "d".into(),
            user_only,
            allowed_tools: None,
            root_dir: None,
            body: body.into(),
            tools: Vec::new(),
        });
        reg
    }

    #[test]
    fn built_in_registry_resolves_the_three_agents() {
        // Exercises the public seam itself (#585): a parse failure here comes
        // back as an `Err` an embedder can handle, not a panic baked into the
        // function.
        let reg = built_in_registry().expect("embedded built-ins must parse");
        for name in ["general", "plan", "debug"] {
            assert!(reg.get(name).is_some(), "missing built-in `{name}`");
        }
    }

    #[test]
    fn built_ins_parse_with_expected_identity() {
        // The embedded built-ins must parse — this is what lets `load_registry`
        // treat their parse as infallible. ADR-0207 left each built-in with no
        // permission or spawn posture of its own (identity only: name,
        // description, system prompt, model/provider pin); read-only/
        // read-write behavior and spawn bounds are both runtime permission-
        // mode facts now, and any agent is a valid spawn target.
        let mut reg = AgentCatalog::default();
        for (file, contents) in BUILT_INS {
            let p = parse(contents).unwrap_or_else(|e| panic!("{file}: {e}"));
            reg.insert(p);
        }

        let general = reg.get("general").expect("general built-in");
        assert!(general.system_prompt.starts_with("You are a coding agent"));

        assert!(reg.get("plan").is_some());
        assert!(reg.get("debug").is_some());

        // #763: only `plan` carries a starting-mode hint.
        assert_eq!(
            reg.get("plan").unwrap().default_mode.as_deref(),
            Some("plan")
        );
        assert_eq!(general.default_mode, None);
        assert_eq!(reg.get("debug").unwrap().default_mode, None);
    }

    #[test]
    fn a_known_default_mode_is_kept() {
        let p = parse("---\nname: x\ndescription: d\ndefault_mode: research\n---\nbody")
            .expect("parses");
        assert_eq!(p.default_mode.as_deref(), Some("research"));
    }

    #[test]
    fn an_unknown_default_mode_is_ignored_not_fatal() {
        // #763: a stale/typo'd mode name warns at load and is dropped, so the
        // agent still loads and simply starts in the usual mode.
        let p = parse("---\nname: x\ndescription: d\ndefault_mode: yolo\n---\nbody")
            .expect("an unknown default_mode must not abort the load");
        assert_eq!(p.default_mode, None);
    }

    #[test]
    fn auto_is_never_a_default_mode() {
        // ADR-0218: an agent file alone can never start a session unattended.
        let p = parse("---\nname: x\ndescription: d\ndefault_mode: auto\n---\nbody")
            .expect("a rejected default_mode must not abort the load");
        assert_eq!(p.default_mode, None);
        let p =
            parse("---\nname: x\ndescription: d\ndefault_mode: build\n---\nbody").expect("parses");
        assert_eq!(p.default_mode.as_deref(), Some("build"));
    }

    #[test]
    fn built_in_bodies_leave_the_exec_rule_to_the_bash_spec() {
        // #740: the bash-vs-call-vs-rhai rule lives once, in the `bash` tool
        // description (cached tools block, visible in every advertising
        // mode). A per-agent copy is re-sent per request and drifts — the old
        // `general` one still called `call` "the" exec after ADR-0195.
        for (file, contents) in BUILT_INS {
            for phrase in ["`rhai`", "sh -c", "Exec is", "not a shell"] {
                assert!(
                    !contents.contains(phrase),
                    "{file} re-explains exec-tool choice ({phrase:?})"
                );
            }
        }
    }

    #[test]
    fn missing_frontmatter_is_an_error() {
        let err = parse("no frontmatter here").unwrap_err();
        assert!(err.to_string().contains("frontmatter"), "got: {err}");
    }

    /// Wrap `content` as a lenient (foreign-dir) raw definition.
    fn foreign_raw(content: &str) -> RawAgent {
        RawAgent {
            layer: AgentLayer::User,
            strictness: Strictness::Lenient,
            source: "~/.claude/agents/test.md".into(),
            content: content.into(),
            path: None,
        }
    }

    #[test]
    fn foreign_frontmatter_ignores_claude_specific_keys() {
        // Claude Code style: `tools` is a comma-separated *string* the strict
        // schema rejects, plus `model`/`color` keys entanglement doesn't know.
        let raw = foreign_raw(
            "---\nname: helper\ndescription: d\ntools: Read, Grep\nmodel: sonnet\ncolor: blue\n---\nbody",
        );
        let (def, body) = parse_raw(&raw).unwrap().expect("foreign agent parses");
        assert_eq!(def.name, "helper");
        assert_eq!(body, "body");
    }

    #[test]
    fn malformed_foreign_definition_is_skipped_not_fatal() {
        assert!(parse_raw(&foreign_raw("no frontmatter")).unwrap().is_none());
        assert!(parse_raw(&foreign_raw("---\nname: x\n---\nno description"))
            .unwrap()
            .is_none());
        assert!(
            parse_raw(&foreign_raw("---\nname: '  '\ndescription: d\n---\nb"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn malformed_strict_definition_still_aborts() {
        let raw = RawAgent {
            strictness: Strictness::Strict,
            ..foreign_raw("---\nname: x\n---\nno description")
        };
        assert!(parse_raw(&raw).is_err());
    }

    #[test]
    fn unterminated_frontmatter_is_an_error() {
        let err = parse("---\nname: x\ndescription: y\n").unwrap_err();
        assert!(err.to_string().contains("unterminated"), "got: {err}");
    }

    #[test]
    fn missing_required_field_is_an_error() {
        // `description` is required; the serde detail rides the error's cause chain.
        let err = parse("---\nname: x\n---\nbody").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("description"), "got: {msg}");
    }

    #[test]
    fn unknown_field_is_rejected() {
        let err = parse("---\nname: x\ndescription: y\ntypo_field: 1\n---\nbody").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("typo_field"), "got: {msg}");
    }

    #[test]
    fn bad_yaml_is_an_error() {
        let err = parse("---\nname: [unclosed\n---\nbody").unwrap_err();
        assert!(err.to_string().contains("frontmatter"), "got: {err}");
    }

    #[test]
    fn body_becomes_system_prompt_and_model_inherit_is_none() {
        let p =
            parse("---\nname: x\ndescription: d\nmodel: inherit\n---\nDo the thing.\n").unwrap();
        assert_eq!(p.system_prompt, "Do the thing.");
        assert_eq!(p.model, None);
    }

    #[test]
    fn retired_authority_frontmatter_keys_are_rejected() {
        // ADR-0207: authority left the agent entirely, in two stages — the
        // tool mask/`permission` first, then (stage 5b) spawn control,
        // sandbox, and the primary/subagent/all `mode` distinction. A
        // definition naming any of them is now a plain unknown-field load
        // error, same as any other typo — never silently ignored or warned.
        for frontmatter in [
            "---\nname: x\ndescription: d\ntools: [read]\n---\nbody",
            "---\nname: x\ndescription: d\ndisallowed_tools: [bash]\n---\nbody",
            "---\nname: x\ndescription: d\npermission:\n  default: ask\n---\nbody",
            "---\nname: x\ndescription: d\nmode: primary\n---\nbody",
            "---\nname: x\ndescription: d\ncan_spawn: true\n---\nbody",
            "---\nname: x\ndescription: d\nspawnable_agents: [explore]\n---\nbody",
            "---\nname: x\ndescription: d\nsandbox: bwrap\n---\nbody",
        ] {
            let err = parse(frontmatter).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("unknown field"), "got: {msg}");
        }
    }

    /// End-to-end through [`parse_raw`] against a real file: a native-layer
    /// definition still carrying `tools`/`permission` self-heals instead of
    /// bricking the load — backed up, rewritten, and re-parsed transparently.
    #[test]
    fn legacy_native_file_self_heals_with_backup_and_warning() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("legacy.md");
        std::fs::write(
            &path,
            "---\nname: reviewer\ndescription: d\ntools: [read, glob, grep]\npermission:\n  default: ask\n---\nBody text.\n",
        )
        .unwrap();
        let raw = RawAgent {
            layer: AgentLayer::User,
            strictness: Strictness::Strict,
            source: path.display().to_string(),
            content: std::fs::read_to_string(&path).unwrap(),
            path: Some(path.clone()),
        };

        let (def, body) = parse_raw(&raw).unwrap().expect("self-healed and parsed");
        assert_eq!(def.name, "reviewer");
        assert_eq!(body, "Body text.");

        // The backup keeps the original bytes with the retired keys intact.
        let bak = path.with_extension("md.bak");
        let backed_up = std::fs::read_to_string(&bak).expect("backup written");
        assert!(backed_up.contains("tools:"));

        // The file itself was rewritten without the retired keys, and a
        // second parse (no more raw-content caching) succeeds directly —
        // proving the rewrite, not just an in-memory patch, is what's live.
        let rewritten = std::fs::read_to_string(&path).unwrap();
        assert!(!rewritten.contains("tools:"));
        assert!(!rewritten.contains("permission:"));
        assert!(rewritten.contains("Body text."));
        let raw2 = RawAgent {
            content: rewritten,
            ..raw
        };
        assert!(parse_raw(&raw2).unwrap().is_some());
    }

    /// A parse failure the retired keys don't explain (a genuine typo) still
    /// aborts loudly — the self-heal must never paper over a real mistake.
    #[test]
    fn a_typo_unrelated_to_retired_keys_still_aborts() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("typo.md");
        std::fs::write(
            &path,
            "---\nname: x\ndescription: d\ntypo_field: 1\n---\nbody",
        )
        .unwrap();
        let raw = RawAgent {
            layer: AgentLayer::User,
            strictness: Strictness::Strict,
            source: path.display().to_string(),
            content: std::fs::read_to_string(&path).unwrap(),
            path: Some(path.clone()),
        };

        let err = parse_raw(&raw).unwrap_err();
        assert!(format!("{err:#}").contains("typo_field"));
        // No migration side effect: the file is untouched, no backup written.
        assert!(!path.with_extension("md.bak").exists());
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("typo_field"));
    }

    #[test]
    fn explicit_model_is_kept() {
        let p = parse("---\nname: x\ndescription: d\nmodel: glm-4.7\n---\nbody").unwrap();
        assert_eq!(p.model.as_deref(), Some("glm-4.7"));
    }

    #[test]
    fn skills_preload_injects_body_into_system_prompt() {
        // `skills:` preloads the full body; `load_skill` access is a runtime
        // permission-mode fact now, not anything the agent masks (#117).
        let skills = skill_registry("git", false, "Run `git commit` carefully.");
        let p = parse_with_skills(
            "---\nname: x\ndescription: d\nskills: [git]\n---\nBody.",
            &skills,
        )
        .unwrap();
        assert!(
            p.system_prompt.contains("Preloaded skills"),
            "{}",
            p.system_prompt
        );
        assert!(
            p.system_prompt.contains("skill_id: git"),
            "{}",
            p.system_prompt
        );
        assert!(
            p.system_prompt.contains("Run `git commit` carefully."),
            "{}",
            p.system_prompt
        );
    }

    #[test]
    fn preload_accepts_user_only_skills() {
        // Preload is author config, so a `user_only` skill (withheld from the
        // model-facing `load_skill`) is still preloadable (#117).
        let skills = skill_registry("deploy", true, "deploy steps");
        let p = parse_with_skills(
            "---\nname: x\ndescription: d\nskills: [deploy]\n---\nBody.",
            &skills,
        )
        .unwrap();
        assert!(
            p.system_prompt.contains("deploy steps"),
            "{}",
            p.system_prompt
        );
    }

    #[test]
    fn unknown_preload_skill_is_a_loud_error() {
        let skills = SkillRegistry::default();
        let err = parse_with_skills(
            "---\nname: x\ndescription: d\nskills: [nope]\n---\nBody.",
            &skills,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("nope"), "got: {msg}");
    }
}
