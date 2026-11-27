//! `SKILL.md` frontmatter parsing (split out of `skills/mod.rs`): the strict
//! native schema and the lenient cross-vendor one (ADR-0074).

use std::path::{Path, PathBuf};

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
}

/// Lenient frontmatter for cross-vendor skills (ADR-0074): only the tier-1
/// contract is read, every other key (Claude Code's `allowed-tools`, `license`,
/// `argument-hint`, …) is ignored — `disable-model-invocation` too, since
/// every skill is model-visible (ADR-0229). Claude's `allowed-tools` is
/// dropped too: skills never restrict the tool set (ADR-0194).
#[derive(Debug, Deserialize)]
struct ForeignSkillFrontmatter {
    name: String,
    description: String,
}

/// Parse one discovered `SKILL.md` honoring its layer's strictness (ADR-0074):
/// strict → a malformed file aborts the load (a pre-0.8 `tools:`/`user_only`/
/// `allowed_tools` key included, #920); lenient → `Ok(None)` after a `warn!`, so a foreign
/// file entanglement doesn't own is skipped instead of bricking startup.
pub(super) fn parse_raw(raw: &RawSkill) -> Result<Option<SkillMeta>> {
    match raw.strictness {
        Strictness::Strict => parse_skill(&raw.content, raw.root_dir.clone())
            .map(Some)
            .with_context(|| format!("parsing skill {}", raw.source)),
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

/// Parse a `SKILL.md`: split frontmatter from body, deserialize the frontmatter,
/// and build a [`SkillMeta`] carrying the pre-resolved `root_dir`.
pub(super) fn parse_skill(content: &str, root_dir: Option<PathBuf>) -> Result<SkillMeta> {
    let (frontmatter, body) = crate::frontmatter::split(content)?;
    let fm: SkillFrontmatter =
        serde_yaml::from_str(&frontmatter).context("invalid skill frontmatter")?;
    if fm.name.trim().is_empty() {
        bail!("skill frontmatter `name` must not be empty");
    }
    let lib = load_lib(&fm.name, root_dir.as_deref());
    Ok(SkillMeta {
        name: fm.name,
        description: fm.description,
        lib,
        root_dir,
        body,
    })
}

/// Parse a cross-vendor `SKILL.md` via the lenient [`ForeignSkillFrontmatter`]
/// (unknown keys ignored, ADR-0074).
fn parse_foreign_skill(content: &str, root_dir: Option<PathBuf>) -> Result<SkillMeta> {
    let (frontmatter, body) = crate::frontmatter::split(content)?;
    let fm: ForeignSkillFrontmatter =
        serde_yaml::from_str(&frontmatter).context("invalid skill frontmatter")?;
    if fm.name.trim().is_empty() {
        bail!("skill frontmatter `name` must not be empty");
    }
    let lib = load_lib(&fm.name, root_dir.as_deref());
    Ok(SkillMeta {
        name: fm.name,
        description: fm.description,
        lib,
        root_dir,
        body,
    })
}

/// Load and compile `<root_dir>/lib.rhai`, or `None` when there is none, it
/// fails to compile, or it carries a top-level statement (each warned).
fn load_lib(skill: &str, root_dir: Option<&Path>) -> Option<lib_rhai::SkillLib> {
    let path = root_dir?.join(lib_rhai::LIB_FILE);
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(skill, path = %path.display(), "skipping unreadable skill lib.rhai: {e}");
            return None;
        }
    };
    match lib_rhai::compile(skill, &source) {
        Ok(lib) => Some(lib),
        Err(why) => {
            tracing::warn!(skill, path = %path.display(), "skipping skill lib.rhai: {why}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signatures(lib: &lib_rhai::SkillLib) -> Vec<&str> {
        lib.functions.iter().map(|f| f.signature.as_str()).collect()
    }

    #[test]
    fn load_reads_lib_rhai_beside_skill_md_and_skips_a_bad_one() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load_lib("s", Some(dir.path())).is_none(),
            "no lib.rhai, no lib"
        );
        std::fs::write(dir.path().join(lib_rhai::LIB_FILE), "fn hi() { \"hi\" }").unwrap();
        assert_eq!(
            signatures(&load_lib("s", Some(dir.path())).unwrap()),
            vec!["skill::s::hi()"]
        );
        std::fs::write(dir.path().join(lib_rhai::LIB_FILE), "print(1);").unwrap();
        assert!(load_lib("s", Some(dir.path())).is_none());
        assert!(load_lib("s", None).is_none(), "a built-in has no directory");
    }
}
