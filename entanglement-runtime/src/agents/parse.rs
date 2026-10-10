//! Parsing one discovered definition (strict vs. lenient frontmatter, the
//! retired-key self-heal) and composing it into a core [`Agent`].

use anyhow::{bail, Context, Result};
use entanglement_core::Agent;
use serde::Deserialize;

use super::discover::RawAgent;
use super::migrate::migrate_legacy_agent_file;
use crate::layers::Strictness;
use crate::mcp::McpCapabilityIndex;
use crate::mode::{ladder_rank, BUILTIN_LADDER};
use crate::skills::SkillRegistry;
use crate::system_prompt::{assemble, PromptContext};

/// One parsed agent definition (frontmatter + body). The `deny_unknown_fields`
/// makes a typo'd key a loud error rather than a silently-ignored field.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AgentDefinition {
    /// Unique id; what `agent { agent }` spawns by.
    pub(super) name: String,
    /// One-line summary; the only field disclosed to a spawning model.
    pub(super) description: String,
    /// Provider model override, or `inherit` / omitted for the session default.
    #[serde(default)]
    pub(super) model: Option<String>,
    /// Provider this agent pins `model` to (#323, ADR-0081). Set alongside
    /// `model` to form a *model pin*: the session re-binds to `(provider, model)`
    /// at session start. `inherit`/omitted ⇒ no provider pin; `model`
    /// alone stays the legacy request-level fallback. `provider` without `model`
    /// is a loud load error (a provider with nothing to run is meaningless).
    #[serde(default)]
    pub(super) provider: Option<String>,
    /// Fold the project brief into this agent's system prompt (#113). Opt-in:
    /// omitted ⇒ the brief is not included even when a brief file exists.
    #[serde(default)]
    pub(super) include_brief: bool,
    /// Skills to **preload** into this agent's system prompt (#117): the listed
    /// skills' full bodies are injected at load (paths substituted, same pipeline
    /// as `load_skill`). Preload only — *not* an allowlist: runtime `load_skill`
    /// access is governed by the session's permission mode, not the agent.
    #[serde(default)]
    pub(super) skills: Option<Vec<String>>,
    /// The permission mode a fresh session under this agent starts in when
    /// neither `--mode` nor config `mode:` names one (#763, ADR-0218) — a
    /// starting hint, never authority. An unknown mode warns and is dropped.
    #[serde(default)]
    pub(super) default_mode: Option<String>,
}

/// Lenient frontmatter for cross-vendor agents (ADR-0074): only the identity
/// pair is read, every other key is ignored — Claude Code agent files carry
/// keys entanglement's strict schema rejects (`tools` as a comma-separated
/// *string*, `model: sonnet`, `color`).
#[derive(Debug, Deserialize)]
struct ForeignAgentFrontmatter {
    name: String,
    description: String,
}

impl ForeignAgentFrontmatter {
    /// Map onto the native definition: no brief/preload, no model/provider
    /// pin. No authority to drop any more (ADR-0207) — a foreign agent's
    /// posture is whatever session mode it runs under, same as any native
    /// one, and it is a spawn target like any other agent (ADR-0207 §6) with
    /// zero mapping needed.
    fn into_definition(self) -> AgentDefinition {
        AgentDefinition {
            name: self.name,
            description: self.description,
            model: None,
            provider: None,
            include_brief: false,
            skills: None,
            default_mode: None,
        }
    }
}

/// Split + parse one discovered definition's frontmatter honoring its layer's
/// strictness (ADR-0074): strict → the current loud behavior (malformed
/// aborts); lenient → `Ok(None)` after a `warn!`, so a foreign file
/// entanglement doesn't own is skipped instead of bricking startup. Returns the
/// definition plus the markdown body.
pub(super) fn parse_raw(raw: &RawAgent) -> Result<Option<(AgentDefinition, String)>> {
    match raw.strictness {
        Strictness::Strict => {
            let (frontmatter, body) = crate::frontmatter::split(&raw.content)
                .with_context(|| format!("parsing agent `{}`", raw.source))?;
            match serde_yaml::from_str::<AgentDefinition>(&frontmatter) {
                Ok(def) => Ok(Some((def, body))),
                Err(e) => {
                    // A real on-disk file (not an embedded built-in, which
                    // never carries the retired keys) gets one self-heal
                    // attempt before the parse error aborts the load.
                    let Some(path) = raw.path.as_deref() else {
                        return Err(e).with_context(|| {
                            format!("invalid frontmatter in agent `{}`", raw.source)
                        });
                    };
                    match migrate_legacy_agent_file(path, &frontmatter, &body)? {
                        Some(cleaned) => {
                            let def: AgentDefinition = serde_yaml::from_str(&cleaned)
                                .with_context(|| {
                                    format!(
                                        "invalid frontmatter in agent `{}` even after dropping \
                                         retired keys",
                                        raw.source
                                    )
                                })?;
                            Ok(Some((def, body)))
                        }
                        // No retired key explains the failure — a genuine
                        // typo, so the original error stands (no guessing).
                        None => Err(e).with_context(|| {
                            format!("invalid frontmatter in agent `{}`", raw.source)
                        }),
                    }
                }
            }
        }
        Strictness::Lenient => {
            let parsed = crate::frontmatter::split(&raw.content).and_then(|(frontmatter, body)| {
                let fm: ForeignAgentFrontmatter =
                    serde_yaml::from_str(&frontmatter).context("invalid agent frontmatter")?;
                if fm.name.trim().is_empty() {
                    bail!("agent frontmatter `name` must not be empty");
                }
                Ok((fm.into_definition(), body))
            });
            match parsed {
                Ok(pair) => Ok(Some(pair)),
                Err(e) => {
                    tracing::warn!(
                        source = %raw.source,
                        error = %format!("{e:#}"),
                        "skipping malformed foreign agent definition",
                    );
                    Ok(None)
                }
            }
        }
    }
}

/// Split frontmatter from body, parse the frontmatter as YAML, and build a core
/// [`Agent`]. The body is composed with `ctx` into the final
/// `system_prompt` via [`assemble`]: shared preamble + body + brief (if
/// `include_brief`) + env + skills — unconditional for every agent now
/// (ADR-0207 §4 retires the old `Subagent`-mode reduced form, #113).
pub(super) fn parse_definition(
    content: &str,
    ctx: &PromptContext,
    skills: &SkillRegistry,
    mcp: &McpCapabilityIndex,
) -> Result<Agent> {
    let (frontmatter, body) = crate::frontmatter::split(content)?;
    let def: AgentDefinition =
        serde_yaml::from_str(&frontmatter).context("invalid agent frontmatter")?;
    build_agent(def, &body, ctx, skills, mcp)
}

/// Build a core [`Agent`] from an already-parsed definition + body,
/// composing the final `system_prompt` via [`assemble`]. Split out from
/// [`parse_definition`] so `inspect` can reuse it after it has the definition in
/// hand (to also render the per-part breakdown from the same inputs).
///
/// `_mcp` is dead weight since ADR-0207 (agent parsing carries no permission
/// fan-out any more) — kept only so every call site in this module shares one
/// signature; [`permission_from_value`]/`expand_capabilities` still need a real
/// [`McpCapabilityIndex`] for the config `permissions:` ceiling
/// ([`crate::config`]).
pub(super) fn build_agent(
    def: AgentDefinition,
    body: &str,
    ctx: &PromptContext,
    skills: &SkillRegistry,
    _mcp: &McpCapabilityIndex,
) -> Result<Agent> {
    if def.name.trim().is_empty() {
        bail!("agent frontmatter `name` must not be empty");
    }
    let preloaded = resolve_preload(def.skills.as_deref().unwrap_or(&[]), &def.name, skills)?;
    let include_brief = def.include_brief;
    // `inherit` is the "no pin" sentinel on both model and provider (matching
    // `model`'s existing filter); drop it before it reaches the agent.
    let model = def.model.filter(|m| m != "inherit");
    let provider = def.provider.filter(|p| p != "inherit");
    // A provider pin needs a model to run (#323, ADR-0081): `provider:` without
    // `model:` is a loud load error, never a silent no-op.
    if provider.is_some() && model.is_none() {
        bail!(
            "agent `{}` sets `provider` without `model`: a provider pin needs a model to run",
            def.name
        );
    }
    // Unknown → warn and ignore rather than abort: the hint only picks a
    // starting posture, so a stale name must not brick the whole catalog.
    let default_mode = def.default_mode.filter(|mode| {
        let known = ladder_rank(mode).is_some();
        if !known {
            tracing::warn!(
                agent = %def.name,
                default_mode = %mode,
                "ignoring unknown `default_mode` (expected one of {})",
                BUILTIN_LADDER.join("/"),
            );
        }
        known
    });
    let agent = Agent {
        name: def.name,
        description: def.description,
        system_prompt: assemble(body, include_brief, ctx, &preloaded),
        model,
        provider,
        default_mode,
    };
    // The one observability point at load (#184): the assembled prompt is
    // otherwise invisible. `brief`/`skills` report what actually reached this
    // prompt — `brief` is `none` unless the agent opts in *and* a brief exists.
    let brief = if include_brief {
        ctx.brief_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "none".to_string())
    } else {
        "none".to_string()
    };
    let skills_in_prompt = ctx.skills.len();
    tracing::debug!(
        agent = %agent.name,
        prompt_len = agent.system_prompt.len(),
        brief = %brief,
        skills = skills_in_prompt,
        "assembled agent system prompt",
    );
    Ok(agent)
}

/// Resolve a definition's `skills:` preload (#117) to rendered bodies via the
/// skill registry. An unknown skill is a loud error (agent definitions never
/// silently drop a typo'd field); orthogonal to the `load_skill` access mask.
pub(super) fn resolve_preload(
    names: &[String],
    agent: &str,
    skills: &SkillRegistry,
) -> Result<Vec<String>> {
    names
        .iter()
        .map(|name| {
            skills
                .preload_body(name)
                .with_context(|| format!("preloading skill for agent `{agent}`"))
        })
        .collect()
}
