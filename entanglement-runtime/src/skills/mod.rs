//! The skill registry (#114, tier-1 progressive disclosure, epic #111).
//!
//! A **skill** is a name + one-line description (tier 1), a markdown body
//! loaded on demand (tier 2, never preloaded into the prompt), an optional
//! home directory with supporting files, and an optional `lib.rhai` whose
//! functions every script reaches as `skill::<skill>` (ADR-0227). An embedder
//! builds [`SkillMeta`]s in code ([`SkillMeta::new`]) or loads them from
//! disk — `skutter` discovers `SKILL.md` files across its layered dirs (#932).
//!
//! # Disclosure
//!
//! Only `name` + `description` reach the model, through `list_skills`.
//! Selection stays LLM reasoning: the model matches its task against the
//! `description` in its own forward pass. There is no keyword router or
//! embedding gate — description quality is the contract. Every skill is
//! disclosed — there is no model-hidden skill (ADR-0229 removed `user_only`).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;

// `lib.rhai` doc comments for the `load_skill` listing (ADR-0228).
mod lib_docs;
pub mod lib_rhai;
pub mod load_skill;
// The embedder's per-session registries + code-built skills (ADR-0249).
mod session;

pub use load_skill::LoadSkillTool;
pub use session::{SessionSkills, SkillResolver};

/// The file that marks a skill directory on disk; `load_skill` leaves it out
/// of the directory's reference listing.
pub const SKILL_FILE: &str = "SKILL.md";

/// A skill disclosure: the two fields exposed to the model (`name` + one-line
/// `description`) — what `list_skills` lists. Generated from the skill
/// registry (every skill, ADR-0229), never authored into an agent body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDisclosure {
    pub name: String,
    pub description: String,
}

/// One skill: the tier-1 metadata plus the body. `root_dir` (the directory
/// holding its `references/`/`scripts/` payload) is resolved once, by whoever
/// built the skill, and never re-derived downstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMeta {
    /// Unique id; what an invocation / tier-2 load references.
    pub name: String,
    /// One-line summary; the only field (with `name`) disclosed to the model.
    pub description: String,
    /// The skill directory (holds `SKILL.md` + payload). `None` for embedded
    /// built-ins, which have no on-disk home.
    pub root_dir: Option<PathBuf>,
    /// The markdown body below the frontmatter (tier-2 content).
    pub body: String,
    /// The compiled `lib.rhai` beside `SKILL.md` (ADR-0227), registered in
    /// every script as `skill::<skill>`; `None` when absent or refused.
    pub lib: Option<lib_rhai::SkillLib>,
}

/// Named set of [`SkillMeta`], keyed by `name`. [`insert`][Self::insert]
/// replaces on collision so a higher layer overrides a lower one.
#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: HashMap<String, SkillMeta>,
}

impl SkillRegistry {
    /// Look a skill up by `name`. Tier-2 loading (`load_skill`, #115) resolves
    /// the body + `root_dir` through it.
    pub fn get(&self, name: &str) -> Option<&SkillMeta> {
        self.skills.get(name)
    }

    /// Every skill, name-sorted for a stable roster (the order the disclosure
    /// list is rendered in), independent of `HashMap` iteration order.
    pub fn iter(&self) -> impl Iterator<Item = &SkillMeta> {
        let mut skills: Vec<&SkillMeta> = self.skills.values().collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills.into_iter()
    }

    pub fn insert(&mut self, skill: SkillMeta) {
        self.skills.insert(skill.name.clone(), skill);
    }

    /// Resolve a **preload** skill (#117) to its full rendered body, using the
    /// same substitution pipeline as `load_skill` ([`load_skill::render_skill`]).
    /// Preload (`skills:` in an agent definition) is agent-author config, distinct
    /// from the `load_skill` tool mask that controls runtime *access*: it injects
    /// a skill's body into the agent's assembled system prompt at load time.
    /// Unlike the model-facing `load_skill`, an unknown name is a hard error,
    /// surfaced loudly at load time (agent definitions validate loudly, never
    /// silently drop a typo'd skill).
    pub fn preload_body(&self, name: &str) -> Result<String> {
        let skill = self.get(name).ok_or_else(|| {
            anyhow::anyhow!("unknown preload skill `{name}`: it is not in the skill index")
        })?;
        Ok(load_skill::render_skill_in(self, skill))
    }

    /// Why `name`'s `lib.rhai` has no `skill::` namespace — the skill that
    /// claimed the segment first — or `None` (ADR-0227's shared naming pass,
    /// the one script registration uses).
    pub fn lib_collision(&self, name: &str) -> Option<String> {
        let with_lib: Vec<String> = self
            .iter()
            .filter(|s| s.lib.is_some())
            .map(|s| s.name.clone())
            .collect();
        match crate::rhai_ident::plan_skills(&with_lib).remove(name)? {
            crate::rhai_ident::RhaiName::Collides(other) => Some(other),
            crate::rhai_ident::RhaiName::Path(_) => None,
        }
    }

    /// Tier-1 disclosure lines for the `list_skills` roster (ADR-0226; the
    /// system prompt carries none): `name` + `description` only, name-sorted
    /// — every skill (ADR-0229).
    pub fn disclosures(&self) -> Vec<SkillDisclosure> {
        self.iter()
            .map(|s| SkillDisclosure {
                name: s.name.clone(),
                description: s.description.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disclosures_are_name_sorted() {
        let mut reg = SkillRegistry::default();
        for name in ["zebra", "alpha"] {
            reg.insert(SkillMeta {
                name: name.into(),
                description: "d".into(),
                root_dir: None,
                body: String::new(),
                lib: None,
            });
        }
        let d = reg.disclosures();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].name, "alpha");
        assert_eq!(d[1].name, "zebra");
    }
}
