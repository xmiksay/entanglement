//! File-based agent definitions (#112, ADR-0034).
//!
//! An agent is a markdown file with YAML frontmatter: the frontmatter is the
//! config bundle (`name`/`description`/`mode`/`model`/`permission`/…), the body
//! below the closing `---` is the agent's system-prompt body. Definitions are
//! discovered at startup and folded into a core [`ProfileRegistry`].
//!
//! The body is not stored raw: as each definition is parsed it is composed into
//! the final `system_prompt` by [`crate::system_prompt::assemble`] (shared
//! preamble + body + project brief + env block + skill index, #113). Baking the
//! assembled prompt into the registry here keeps every downstream consumer
//! (session start, `SetAgent`, spawn) a pass-through.
//!
//! # Layers & precedence
//!
//! Three layers, later wins on a `name` collision:
//!
//! 1. **built-in** — embedded [`include_str!`] files ([`build`], [`plan`],
//!    [`explore`]), parsed through the *same* loader. Editing a built-in is just
//!    dropping a same-`name` file in a higher layer; there is no special
//!    "edit built-ins" code path.
//! 2. **user** — `${config_dir}/entanglement/agents/*.md`.
//! 3. **project** — `<root>/.entanglement/agents/*.md`.
//!
//! Same defaults+override shape as the provider catalog (#118): a malformed
//! user/project file is a loud error, never a silent fallback; the embedded
//! built-ins are guarded by a unit test so their parse is provably infallible.
//!
//! # Tool mask (#116) and deferred frontmatter
//!
//! `tools`/`disallowed_tools` (the tool mask) now reach the core
//! [`AgentProfile`] and are **enforced** (#116, ADR-0038): core filters the
//! advertised specs by the mask at turn time and the runtime executor refuses a
//! masked call at dispatch, so a restricted agent's model never sees the schema
//! and a hallucinated call is still refused.
//!
//! `can_spawn`/`spawnable_agents` (fine-grained spawn control) now reach the core
//! [`AgentProfile`] and are **enforced** (#119, ADR-0040): `can_spawn` gates the
//! whole `agent_*` family (withheld from the model + refused at dispatch when a
//! profile may not spawn) and `spawnable_agents` scopes which profiles it may
//! spawn — both layered in front of the ADR-0023 budget and the ADR-0024 clamp.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use entanglement_core::{AgentMode, AgentProfile, Permission, PermissionProfile, ProfileRegistry};
use serde::Deserialize;

use crate::skills::SkillRegistry;
use crate::system_prompt::{assemble, assemble_parts, PromptContext, PromptPart};

/// Embedded built-in definitions, parsed through the same loader as user/project
/// files. `(filename, contents)` — the filename only feeds parse-error messages;
/// the agent's identity is its frontmatter `name`.
const BUILT_INS: &[(&str, &str)] = &[
    ("build.md", include_str!("build.md")),
    ("plan.md", include_str!("plan.md")),
    ("explore.md", include_str!("explore.md")),
];

/// Env var overriding the user agents directory (tests + non-XDG setups).
const AGENTS_DIR_ENV: &str = "ENTANGLEMENT_AGENTS_DIR";

/// One parsed agent definition (frontmatter + body). The `deny_unknown_fields`
/// makes a typo'd key a loud error rather than a silently-ignored field.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentDefinition {
    /// Unique id; what `agent_spawn { agent }` / `SetAgent` reference.
    name: String,
    /// One-line summary; the only field disclosed to a spawning model.
    description: String,
    /// `primary` / `subagent` / `all`. Defaults to `primary`.
    #[serde(default = "default_mode")]
    mode: AgentMode,
    /// Provider model override, or `inherit` / omitted for the session default.
    #[serde(default)]
    model: Option<String>,
    /// Per-tool `Allow | Ask | Deny` rules. Omitted ⇒ allow-all.
    #[serde(default)]
    permission: Option<serde_yaml::Value>,
    /// Fold the project brief into this agent's system prompt (#113). Opt-in:
    /// omitted ⇒ the brief is not included even when a brief file exists.
    #[serde(default)]
    include_brief: bool,
    /// Tool allowlist; omitted ⇒ inherit all. Enforced (#116, ADR-0038): masks
    /// both the advertised specs and dispatch.
    #[serde(default)]
    tools: Option<Vec<String>>,
    /// Tool denylist, applied after the allowlist (#116, ADR-0038).
    #[serde(default)]
    disallowed_tools: Vec<String>,
    /// Whether this profile may spawn sub-agents (#119, ADR-0040). Omitted ⇒
    /// derive from `mode` (`subagent` closed, otherwise open).
    #[serde(default)]
    can_spawn: Option<bool>,
    /// Which agents this profile may spawn, by name (#119, ADR-0040). Omitted ⇒
    /// any registered profile whose `mode` permits sub-agent use.
    #[serde(default)]
    spawnable_agents: Option<Vec<String>>,
    /// Skills to **preload** into this agent's system prompt (#117): the listed
    /// skills' full bodies are injected at load (paths substituted, same pipeline
    /// as `load_skill`). Preload only — *not* an allowlist: runtime skill access
    /// is the orthogonal `load_skill` tool mask (`tools`/`disallowed_tools`).
    #[serde(default)]
    skills: Option<Vec<String>>,
}

fn default_mode() -> AgentMode {
    AgentMode::Primary
}

/// Load the agent registry for `root`: embedded built-ins, then the user dir,
/// then the project dir — later layers replace earlier ones on a `name`
/// collision (project > user > built-in). A malformed file in any layer aborts.
///
/// `ctx` carries the deterministic system-prompt inputs (shared preamble,
/// project brief, environment block, skill index): each profile's body is
/// composed into a final `system_prompt` via [`assemble`] as it is parsed
/// (#113). Pass [`PromptContext::default`] for the raw, un-composed bodies.
pub fn load_registry(
    root: &Path,
    ctx: &PromptContext,
    skills: &SkillRegistry,
) -> Result<ProfileRegistry> {
    let mut reg = ProfileRegistry::default();
    // Track the winning layer per name so a later-wins collision is no longer
    // silent (#185): emit a `replaces=<prior layer>` debug at the overwrite,
    // matching the provenance `inspect agents` surfaces.
    let mut winning: std::collections::HashMap<String, AgentLayer> =
        std::collections::HashMap::new();
    for raw in discover(root)? {
        // Later layers replace earlier ones on a `name` collision because
        // `discover` yields them in precedence order and `insert` overwrites.
        let profile = parse_definition(&raw.content, ctx, skills)
            .with_context(|| format!("parsing agent `{}`", raw.source))?;
        if let Some(prior) = winning.insert(profile.name.clone(), raw.layer) {
            tracing::debug!(
                agent = %profile.name,
                layer = raw.layer.label(),
                replaces = prior.label(),
                source = %raw.source,
                "agent definition overrides a lower layer",
            );
        }
        reg.insert(profile);
    }
    Ok(reg)
}

/// One resolved agent for `skutter inspect agents` (#185): the winning
/// definition plus the provenance the silent `insert` used to swallow — which
/// layer/source won, and every lower-layer definition of the same name it
/// overrode.
pub struct AgentResolution {
    /// The fully assembled winning profile (mode/model/permission/mask + prompt).
    pub profile: AgentProfile,
    /// Which precedence layer the winner came from.
    pub layer: AgentLayer,
    /// The winner's origin (`built-in (build.md)` or a file path).
    pub source: String,
    /// Lower-layer definitions of the same name the winner overrode, in
    /// precedence order — `(layer, source)` each. Empty when nothing was shadowed.
    pub shadowed: Vec<(AgentLayer, String)>,
}

/// Resolve every agent for `root` with full provenance (#185), applying the same
/// layer precedence as [`load_registry`] but keeping *which* layer won and what
/// it shadowed. Sorted by name for a stable table. A malformed file in any layer
/// is a loud error, exactly as at load.
pub fn resolve_registry(
    root: &Path,
    ctx: &PromptContext,
    skills: &SkillRegistry,
) -> Result<Vec<AgentResolution>> {
    // Preserve first-seen order of names, then sort at the end; group each name's
    // definitions in precedence order so the last is the winner and the rest are
    // what it shadowed.
    let mut order: Vec<String> = Vec::new();
    let mut by_name: std::collections::HashMap<String, Vec<(AgentLayer, String, AgentProfile)>> =
        std::collections::HashMap::new();
    for raw in discover(root)? {
        let profile = parse_definition(&raw.content, ctx, skills)
            .with_context(|| format!("parsing agent `{}`", raw.source))?;
        let name = profile.name.clone();
        let entry = by_name.entry(name.clone()).or_default();
        if entry.is_empty() {
            order.push(name);
        }
        entry.push((raw.layer, raw.source, profile));
    }

    let mut resolved: Vec<AgentResolution> = order
        .into_iter()
        .map(|name| {
            let mut defs = by_name
                .remove(&name)
                .expect("name recorded on first insert");
            let (layer, source, profile) = defs.pop().expect("at least one definition per name");
            let shadowed = defs.into_iter().map(|(l, s, _)| (l, s)).collect();
            AgentResolution {
                profile,
                layer,
                source,
                shadowed,
            }
        })
        .collect();
    resolved.sort_by(|a, b| a.profile.name.cmp(&b.profile.name));
    Ok(resolved)
}

/// Everything `skutter inspect prompt` needs for one agent (#184): the winning
/// definition's source, the assembled profile, and the per-part breakdown.
pub struct AgentPromptReport {
    /// Where the winning definition came from (`built-in (build.md)` or a path).
    pub source: String,
    /// The fully assembled profile (its `system_prompt` is the resolved prompt).
    pub profile: AgentProfile,
    /// The included prompt slices with their sources, in prompt order.
    pub parts: Vec<PromptPart>,
    /// Whether the definition opted into the project brief (`include_brief`).
    pub include_brief: bool,
    /// Whether a brief slice actually made it into the prompt (set *and* found).
    pub brief_included: bool,
}

/// Resolve the winning definition for `agent` (same precedence as
/// [`load_registry`]) and report its assembled prompt plus per-part breakdown,
/// without spawning the engine (#184). `Ok(None)` if no such agent exists; a
/// malformed definition in any layer is a loud error, exactly as at load.
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
        let (frontmatter, body) = crate::frontmatter::split(&raw.content)
            .with_context(|| format!("parsing agent `{}`", raw.source))?;
        let def: AgentDefinition = serde_yaml::from_str(&frontmatter)
            .with_context(|| format!("invalid frontmatter in agent `{}`", raw.source))?;
        if def.name == agent {
            winner = Some((raw.source, def, body));
        }
    }
    let Some((source, def, body)) = winner else {
        return Ok(None);
    };

    let include_brief = def.include_brief;
    let mode = def.mode;
    let preloaded = resolve_preload(def.skills.as_deref().unwrap_or(&[]), &def.name, skills)?;
    let mut parts = assemble_parts(&body, include_brief, mode, ctx, &preloaded);
    // `assemble_parts` labels the body with a generic source; here we know the
    // actual winning file, so point the body part at it.
    for p in parts.iter_mut().filter(|p| p.label == "agent body") {
        p.source = source.clone();
    }
    let brief_included = parts.iter().any(|p| p.label == "project brief");
    let profile = build_profile(def, &body, ctx, skills)?;
    Ok(Some(AgentPromptReport {
        source,
        profile,
        parts,
        include_brief,
        brief_included,
    }))
}

/// Which of the three precedence layers a definition came from (#185). The
/// shared [`crate::layers::Layer`] — `built-in < user < project`, later wins on
/// a `name` collision — re-exported under the agents-facing name.
pub use crate::layers::Layer as AgentLayer;

/// A discovered agent definition file *before* parsing: which layer it came from
/// (#185), a display label for its origin (`built-in (build.md)` or the file
/// path), and the raw file content.
struct RawAgent {
    layer: AgentLayer,
    source: String,
    content: String,
}

/// Enumerate every agent definition in precedence order — embedded built-ins,
/// then the user dir, then the project dir — without parsing them. Later entries
/// win on a `name` collision, so consumers keep the last match. A missing dir is
/// fine; an unreadable dir or file is an error. A missing *explicit*
/// `ENTANGLEMENT_AGENTS_DIR` override is warned by [`crate::layers::load_layers`].
fn discover(root: &Path) -> Result<Vec<RawAgent>> {
    let built_ins: Vec<RawAgent> = BUILT_INS
        .iter()
        .map(|(file, contents)| RawAgent {
            layer: AgentLayer::BuiltIn,
            source: format!("built-in ({file})"),
            content: (*contents).to_string(),
        })
        .collect();
    crate::layers::load_layers(root, "agents", AGENTS_DIR_ENV, built_ins, read_dir_raws)
}

/// Append every `*.md` file in `dir` (if it exists) to `raws`, tagged with
/// `layer` and sorted for deterministic collision resolution within the
/// directory.
fn read_dir_raws(layer: AgentLayer, dir: &Path, raws: &mut Vec<RawAgent>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("reading agents dir {}", dir.display()))?;
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("md"))
        .collect();
    files.sort();
    for path in files {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading agent definition {}", path.display()))?;
        raws.push(RawAgent {
            layer,
            source: path.display().to_string(),
            content,
        });
    }
    Ok(())
}

/// Split frontmatter from body, parse the frontmatter as YAML, and build a core
/// [`AgentProfile`]. The body is composed with `ctx` into the final
/// `system_prompt` via [`assemble`]: shared preamble + body + brief (if
/// `include_brief`) + env + skills, with subagents getting the reduced form
/// (#113).
fn parse_definition(
    content: &str,
    ctx: &PromptContext,
    skills: &SkillRegistry,
) -> Result<AgentProfile> {
    let (frontmatter, body) = crate::frontmatter::split(content)?;
    let def: AgentDefinition =
        serde_yaml::from_str(&frontmatter).context("invalid agent frontmatter")?;
    build_profile(def, &body, ctx, skills)
}

/// Build a core [`AgentProfile`] from an already-parsed definition + body,
/// composing the final `system_prompt` via [`assemble`]. Split out from
/// [`parse_definition`] so `inspect` can reuse it after it has the definition in
/// hand (to also render the per-part breakdown from the same inputs).
fn build_profile(
    def: AgentDefinition,
    body: &str,
    ctx: &PromptContext,
    skills: &SkillRegistry,
) -> Result<AgentProfile> {
    if def.name.trim().is_empty() {
        bail!("agent frontmatter `name` must not be empty");
    }
    let permission = match &def.permission {
        Some(v) => permission_from_value(v)?,
        None => PermissionProfile::new(Permission::Allow),
    };
    let preloaded = resolve_preload(def.skills.as_deref().unwrap_or(&[]), &def.name, skills)?;
    let include_brief = def.include_brief;
    let mode = def.mode;
    let profile = AgentProfile {
        name: def.name,
        description: def.description,
        mode,
        system_prompt: assemble(body, include_brief, mode, ctx, &preloaded),
        model: def.model.filter(|m| m != "inherit"),
        permission,
        tools: def.tools,
        disallowed_tools: def.disallowed_tools,
        can_spawn: def.can_spawn,
        spawnable_agents: def.spawnable_agents,
    };
    // The one observability point at load (#184): the assembled prompt is
    // otherwise invisible. `brief`/`skills` report what actually reached this
    // prompt — `brief` is `none` unless the agent opts in *and* a brief exists;
    // `skills` is 0 for a subagent (the tier-1 index is withheld for it).
    let brief = if include_brief {
        ctx.brief_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "none".to_string())
    } else {
        "none".to_string()
    };
    let skills_in_prompt = if mode != AgentMode::Subagent {
        ctx.skills.len()
    } else {
        0
    };
    tracing::debug!(
        agent = %profile.name,
        prompt_len = profile.system_prompt.len(),
        brief = %brief,
        skills = skills_in_prompt,
        "assembled agent system prompt",
    );
    Ok(profile)
}

/// Resolve a definition's `skills:` preload (#117) to rendered bodies via the
/// skill registry. An unknown skill is a loud error (agent definitions never
/// silently drop a typo'd field); orthogonal to the `load_skill` access mask.
fn resolve_preload(names: &[String], agent: &str, skills: &SkillRegistry) -> Result<Vec<String>> {
    names
        .iter()
        .map(|name| {
            skills
                .preload_body(name)
                .with_context(|| format!("preloading skill for agent `{agent}`"))
        })
        .collect()
}

/// Convert a `permission` mapping into a core [`PermissionProfile`]. Keys are
/// tool patterns — `"*"`, a tool name, or an argument-scoped `tool(pattern)`
/// glob (e.g. `bash(git *)`, `edit(src/*)`, #173); the reserved `default` key
/// sets the fallback permission. Rules preserve file order (last match wins, ADR-0003).
/// An omitted `default` ⇒ allow. Shared with the user config's `permissions`
/// section (#172), which uses the identical shape.
pub(crate) fn permission_from_value(value: &serde_yaml::Value) -> Result<PermissionProfile> {
    let map = value
        .as_mapping()
        .context("`permission` must be a mapping of tool → allow|ask|deny")?;
    let mut default = Permission::Allow;
    let mut rules = Vec::new();
    for (key, val) in map {
        let key = key
            .as_str()
            .context("`permission` keys must be strings (a tool name or `*`)")?;
        let perm: Permission = serde_yaml::from_value(val.clone())
            .with_context(|| format!("invalid permission for `{key}` (expected allow|ask|deny)"))?;
        if key == "default" {
            default = perm;
        } else {
            rules.push((key.to_string(), perm));
        }
    }
    Ok(PermissionProfile { rules, default })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse with an identity context + empty skill registry so tests assert the
    /// raw body verbatim (no preload injection).
    fn parse(content: &str) -> Result<AgentProfile> {
        parse_definition(
            content,
            &PromptContext::default(),
            &SkillRegistry::default(),
        )
    }

    /// Parse against a supplied skill registry, to exercise `skills:` preload.
    fn parse_with_skills(content: &str, skills: &SkillRegistry) -> Result<AgentProfile> {
        parse_definition(content, &PromptContext::default(), skills)
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
        });
        reg
    }

    #[test]
    fn built_ins_parse_with_expected_shape() {
        // The embedded built-ins must parse — this is what lets `load_registry`
        // treat their parse as infallible.
        let mut reg = ProfileRegistry::default();
        for (file, contents) in BUILT_INS {
            let p = parse(contents).unwrap_or_else(|e| panic!("{file}: {e}"));
            reg.insert(p);
        }
        let build = reg.get("build").expect("build built-in");
        assert_eq!(build.mode, AgentMode::Primary);
        assert_eq!(build.permission.for_tool("edit"), Permission::Allow);
        assert!(build.system_prompt.starts_with("You are a coding agent"));
        // Plan authorship is default-closed (#231, ADR-0049): inherit-all `build`
        // does not explicitly allowlist the plan tools, so it authors no plan.
        assert!(!build.advertises_tool("update_plan") || build.tools.is_none());
        assert!(!crate::plan_tasks::explicitly_allowlists(
            build,
            "update_plan"
        ));
        assert!(!crate::plan_tasks::explicitly_allowlists(
            build,
            "propose_plan"
        ));

        let plan = reg.get("plan").expect("plan built-in");
        assert_eq!(plan.permission.for_tool("read"), Permission::Allow);
        assert_eq!(plan.permission.for_tool("edit"), Permission::Ask);
        // Plan authors the plan (#231, ADR-0049) and is physically read-only: its
        // tool mask carries the read trio + delegation/skill tools + the plan
        // tools, no `edit`/`write`/`bash`. Children spawned under it inherit the
        // clamp. Its allowlist explicitly opts into plan authorship.
        assert!(crate::plan_tasks::explicitly_allowlists(
            plan,
            "update_plan"
        ));
        assert!(crate::plan_tasks::explicitly_allowlists(
            plan,
            "propose_plan"
        ));
        assert!(plan.advertises_tool("read"));
        assert!(plan.advertises_tool("agent_spawn"));
        assert!(plan.advertises_tool("load_skill"));
        assert!(plan.advertises_tool("update_plan"));
        assert!(plan.advertises_tool("propose_plan"));
        assert!(!plan.advertises_tool("edit"));
        assert!(!plan.advertises_tool("write"));
        assert!(!plan.advertises_tool("bash"));

        let explore = reg.get("explore").expect("explore built-in");
        assert_eq!(explore.mode, AgentMode::Subagent);
        assert_eq!(explore.permission.for_tool("read"), Permission::Allow);
        assert_eq!(explore.permission.for_tool("edit"), Permission::Deny);
        // Read-only `explore` never authors a plan and cannot mutate tasks (#175):
        // its allowlist omits `update_plan`/`update_tasks` and permission denies.
        assert!(!explore.advertises_tool("update_plan"));
        assert!(!explore.advertises_tool("update_tasks"));
        assert_eq!(
            explore.permission.for_tool("update_tasks"),
            Permission::Deny
        );
        // Reference read-only agent (#116): its tool mask is the read trio only.
        assert!(explore.advertises_tool("read"));
        assert!(explore.advertises_tool("grep"));
        assert!(!explore.advertises_tool("edit"));
        assert!(!explore.advertises_tool("agent_spawn"));
    }

    #[test]
    fn missing_frontmatter_is_an_error() {
        let err = parse("no frontmatter here").unwrap_err();
        assert!(err.to_string().contains("frontmatter"), "got: {err}");
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
        // Omitted permission ⇒ allow-all.
        assert_eq!(p.permission.for_tool("edit"), Permission::Allow);
    }

    #[test]
    fn explicit_model_is_kept() {
        let p = parse("---\nname: x\ndescription: d\nmodel: glm-4.7\n---\nbody").unwrap();
        assert_eq!(p.model.as_deref(), Some("glm-4.7"));
    }

    #[test]
    fn mode_all_and_tool_mask_reach_the_profile() {
        let p = parse(
            "---\nname: x\ndescription: d\nmode: all\ntools: [read, grep]\n\
             disallowed_tools: [bash]\ncan_spawn: true\nspawnable_agents: [explore]\n---\nbody",
        )
        .unwrap();
        assert_eq!(p.mode, AgentMode::All);
        // `tools`/`disallowed_tools` now reach the core profile and drive the
        // advertised-set mask (#116).
        assert_eq!(
            p.tools.as_deref(),
            Some(&["read".to_string(), "grep".to_string()][..])
        );
        assert_eq!(p.disallowed_tools, vec!["bash".to_string()]);
        assert!(p.advertises_tool("read"));
        assert!(!p.advertises_tool("edit"));
        assert!(!p.advertises_tool("bash"));
        // `can_spawn`/`spawnable_agents` now reach the core profile too (#119).
        assert!(p.may_spawn());
        assert!(p.spawn_target_allowed("explore"));
        assert!(!p.spawn_target_allowed("build"));
    }

    #[test]
    fn skills_preload_injects_body_into_system_prompt() {
        // `skills:` preloads the full body; the tool mask is untouched (preload is
        // not an allowlist), so `load_skill` stays advertised for the rest (#117).
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
        // Preload does not touch the tool mask — `load_skill` still advertised.
        assert!(p.advertises_tool("load_skill"));
    }

    #[test]
    fn preload_and_mask_are_independent_mechanisms() {
        // The "preload X but block everything else" corner case (#117): preload a
        // skill body *and* mask `load_skill` out so no other skill is loadable.
        let skills = skill_registry("git", false, "git body");
        let p = parse_with_skills(
            "---\nname: x\ndescription: d\nskills: [git]\n\
             disallowed_tools: [load_skill]\n---\nBody.",
            &skills,
        )
        .unwrap();
        // Body is preloaded...
        assert!(p.system_prompt.contains("git body"), "{}", p.system_prompt);
        // ...but runtime access to *any* skill is masked off.
        assert!(!p.advertises_tool("load_skill"));
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

    #[test]
    fn spawn_fields_default_from_mode_when_omitted() {
        // A subagent leaf with no `can_spawn` defaults closed; a primary opens.
        let leaf = parse("---\nname: x\ndescription: d\nmode: subagent\n---\nbody").unwrap();
        assert!(!leaf.may_spawn());
        let primary = parse("---\nname: y\ndescription: d\n---\nbody").unwrap();
        assert!(primary.may_spawn());
        // An omitted allowlist is open to any target.
        assert!(primary.spawn_target_allowed("anything"));
    }
}
