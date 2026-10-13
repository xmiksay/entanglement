//! `SKILL.md` frontmatter parsing (split out of `skills/mod.rs`): the strict
//! native schema, the lenient cross-vendor one (ADR-0074), and the retired
//! `tools:` key's skip-not-abort handling (ADR-0228).

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::layers::Strictness;

use super::discovery::RawSkill;
use super::{lib_rhai, SkillMeta};

/// Skill frontmatter: `name` + `description` required, the rest optional.
/// `deny_unknown_fields` makes a typo'd key a loud error, not a silent drop.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    user_only: bool,
    #[serde(default)]
    allowed_tools: Option<Vec<String>>,
}

/// A native skill still declaring the retired `tools:` frontmatter
/// (ADR-0228). [`parse_raw`] skips such a skill with an error-level log
/// instead of aborting the whole registry load: a repo carrying an old
/// `SKILL.md` must not brick startup for everyone who opens it.
#[derive(Debug)]
pub(super) struct RetiredToolsKey(String);

impl std::fmt::Display for RetiredToolsKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "skill `{}`: the `tools:` frontmatter key was removed (ADR-0228) — move each \
             tool into a function in a `lib.rhai` beside SKILL.md (called as \
             `skill::<skill>::<fn>(...)` from a script), or keep the script in the skill \
             dir and run it with `rhai` `{{\"path\": \"<skill_dir>/x.rhai\", \"args\": {{...}}}}`",
            self.0
        )
    }
}

impl std::error::Error for RetiredToolsKey {}

/// Lenient frontmatter for cross-vendor skills (ADR-0074): only the tier-1
/// contract is read, every other key (Claude Code's `allowed-tools`, `license`,
/// `argument-hint`, …) is ignored. Claude's `disable-model-invocation` maps to
/// [`SkillMeta::user_only`] — same semantics, the model must not self-trigger.
/// Claude's `allowed-tools` is deliberately dropped: its tool names don't map
/// onto entanglement's, and `SkillMeta::allowed_tools` is parsed-but-ignored
/// even on a native skill (ADR-0194), so there is nothing for a foreign skill
/// to populate it for.
#[derive(Debug, Deserialize)]
struct ForeignSkillFrontmatter {
    name: String,
    description: String,
    #[serde(default, rename = "disable-model-invocation")]
    disable_model_invocation: bool,
}

/// Parse one discovered `SKILL.md` honoring its layer's strictness (ADR-0074):
/// strict → the current loud behavior (a malformed file aborts the load),
/// except the retired `tools:` key, which skips the skill with an error-level
/// log (ADR-0228); lenient → `Ok(None)` after a `warn!`, so a foreign file
/// entanglement doesn't own is skipped instead of bricking startup.
pub(super) fn parse_raw(raw: &RawSkill) -> Result<Option<SkillMeta>> {
    match raw.strictness {
        Strictness::Strict => match parse_skill(&raw.content, raw.root_dir.clone()) {
            Ok(meta) => Ok(Some(meta)),
            Err(e) if e.is::<RetiredToolsKey>() => {
                tracing::error!(source = %raw.source, "skipping skill: {e}");
                Ok(None)
            }
            Err(e) => Err(e.context(format!("parsing skill {}", raw.source))),
        },
        Strictness::Lenient => match parse_foreign_skill(&raw.content, raw.root_dir.clone()) {
            Ok(meta) => Ok(Some(meta)),
            Err(e) => {
                tracing::warn!(
                    source = %raw.source,
                    error = %format!("{e:#}"),
                    "skipping malformed foreign skill definition",
                );
                Ok(None)
            }
        },
    }
}

/// Why a native `raw` skill is refused at load — the retired `tools:` key
/// (ADR-0228) — as a `source: reason` line, or `None` when it loads (or
/// fails some other way, which aborts the load loudly on its own).
pub(super) fn rejection(raw: &RawSkill) -> Option<String> {
    if raw.strictness != Strictness::Strict {
        return None;
    }
    match parse_skill(&raw.content, raw.root_dir.clone()) {
        Err(e) if e.is::<RetiredToolsKey>() => Some(format!("{}: {e}", raw.source)),
        _ => None,
    }
}

/// Parse a `SKILL.md`: split frontmatter from body, deserialize the frontmatter,
/// and build a [`SkillMeta`] carrying the pre-resolved `root_dir`. A frontmatter
/// carrying `allowed_tools` gets a one-time (per load) warning — ADR-0194
/// retired its enforcement, so the field is parsed-but-ignored; permission
/// profiles are the control now. A `tools:` key is a [`RetiredToolsKey`].
pub(super) fn parse_skill(content: &str, root_dir: Option<PathBuf>) -> Result<SkillMeta> {
    let (frontmatter, body) = crate::frontmatter::split(content)?;
    let mut raw: serde_yaml::Value =
        serde_yaml::from_str(&frontmatter).context("invalid skill frontmatter")?;
    // The retired key is taken out by *presence* — an empty `tools:` or
    // `tools: ~` is still a stale declaration — so it is refused with the
    // replacement named, not with serde's bare "unknown field".
    let had_tools = raw
        .as_mapping_mut()
        .is_some_and(|m| m.remove("tools").is_some());
    let fm: SkillFrontmatter = serde_yaml::from_value(raw).context("invalid skill frontmatter")?;
    if fm.name.trim().is_empty() {
        bail!("skill frontmatter `name` must not be empty");
    }
    if had_tools {
        return Err(RetiredToolsKey(fm.name).into());
    }
    if fm.allowed_tools.is_some() {
        tracing::warn!(
            skill = %fm.name,
            "skill's `allowed_tools` is no longer enforced (ADR-0194: skills are \
             additive-only) — permission profiles are the control now; remove the field",
        );
    }
    let lib = lib_rhai::load(&fm.name, root_dir.as_deref());
    Ok(SkillMeta {
        name: fm.name,
        description: fm.description,
        user_only: fm.user_only,
        allowed_tools: fm.allowed_tools,
        lib,
        root_dir,
        body,
    })
}

/// Parse a cross-vendor `SKILL.md` via the lenient [`ForeignSkillFrontmatter`]
/// (unknown keys ignored, ADR-0074) — a stray `tools:` included.
fn parse_foreign_skill(content: &str, root_dir: Option<PathBuf>) -> Result<SkillMeta> {
    let (frontmatter, body) = crate::frontmatter::split(content)?;
    let fm: ForeignSkillFrontmatter =
        serde_yaml::from_str(&frontmatter).context("invalid skill frontmatter")?;
    if fm.name.trim().is_empty() {
        bail!("skill frontmatter `name` must not be empty");
    }
    let lib = lib_rhai::load(&fm.name, root_dir.as_deref());
    Ok(SkillMeta {
        name: fm.name,
        description: fm.description,
        user_only: fm.disable_model_invocation,
        allowed_tools: None,
        lib,
        root_dir,
        body,
    })
}
