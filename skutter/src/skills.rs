//! File-based skill discovery (#114, #932) over the library's skill registry
//! (`entanglement_runtime::skills`).
//!
//! A skill on disk is a directory holding a `SKILL.md` (YAML frontmatter +
//! markdown body) plus optional supporting files (`references/*.md`,
//! `scripts/*`, a `lib.rhai`). Discovery mirrors the file-based agent loader
//! (#112) and the provider catalog (#118): embedded stock skills, then user,
//! then project — later layers replace earlier ones on a `name` collision
//! (**project > user > built-in**). A stock skill is edited by dropping a
//! same-`name` `SKILL.md` in a higher layer.
//!
//! # Layers & precedence
//!
//! 1. **built-in** — embedded [`include_str!`] `SKILL.md` files, parsed through
//!    the *same* loader. Stock skills are single-file (body only, no on-disk
//!    `references/`/`scripts/` payload); anything needing supporting files lives
//!    on disk.
//! 2. **user** — `~/.claude/skills/**/SKILL.md` (cross-vendor, lenient), then
//!    `${config_dir}/entanglement/skills/**/SKILL.md` (native, strict).
//! 3. **project** — `.claude/skills` then `.agents/skills` (both lenient), then
//!    `<root>/.entanglement/skills/**/SKILL.md` (native, strict — highest).
//!
//! Foreign (cross-vendor) dirs are parsed leniently per ADR-0074: unknown
//! frontmatter keys are ignored and a malformed file is warned and skipped —
//! a file entanglement doesn't own must not abort the load.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

use entanglement_runtime::skills::*;

use crate::layers::UserLayer;

mod discovery;
// `SKILL.md` frontmatter parsing, strict and lenient (ADR-0074/0228).
mod parse;

pub use discovery::skill_dirs;

use parse::parse_raw;

/// Which precedence layer a skill definition came from (#186). The shared
/// [`crate::layers::Layer`] — `built-in < user < project`, later wins on a
/// `name` collision — re-exported under the skills-facing name.
pub use crate::layers::Layer as SkillLayer;

/// Load the skill registry for `root`: embedded stock skills, then the user
/// dirs, then the project dirs — later layers replace earlier ones on a `name`
/// collision (project > user > built-in). A malformed file in a native layer
/// aborts; a malformed foreign file is warned and skipped.
///
/// `user` is where the user layer lives — the embedder's answer (#773).
pub fn load_registry_in(root: &Path, user: &UserLayer) -> Result<SkillRegistry> {
    let mut reg = SkillRegistry::default();
    // Track the winning (layer, source) per name so a later-wins collision is no
    // longer silent (#186): emit a `replaces=<prior source>` debug at the
    // overwrite, matching the provenance `inspect skills` surfaces.
    let mut winning: HashMap<String, (SkillLayer, String)> = HashMap::new();
    for raw in discovery::discover(root, user)? {
        let Some(skill) = parse_raw(&raw)? else {
            continue;
        };
        if let Some((prior_layer, prior_source)) =
            winning.insert(skill.name.clone(), (raw.layer, raw.source.clone()))
        {
            tracing::debug!(
                skill = %skill.name,
                layer = raw.layer.label(),
                replaces = %format!("{} ({})", prior_layer.label(), prior_source),
                source = %raw.source,
                "skill definition overrides a lower layer",
            );
        }
        reg.insert(skill);
    }
    Ok(reg)
}

/// One resolved skill for `skutter inspect skills` (#186): the winning definition
/// plus the provenance the silent `insert` used to swallow — which layer/source
/// won, and every lower-layer `SKILL.md` of the same name it overrode.
pub struct SkillResolution {
    /// The winning skill metadata (name/description/root_dir).
    pub meta: SkillMeta,
    /// Which precedence layer the winner came from.
    pub layer: SkillLayer,
    /// The winner's origin (`built-in (rhai.md)` or a file path).
    pub source: String,
    /// Lower-layer definitions of the same name the winner overrode, in
    /// precedence order — `(layer, source)` each. Empty when nothing was shadowed.
    pub shadowed: Vec<(SkillLayer, String)>,
}

/// Resolve every skill for `root` with full provenance (#186), applying the same
/// layer precedence as [`load_registry_in`] but keeping *which* layer won and what
/// it shadowed. Sorted by name for a stable table. Malformed files behave
/// exactly as at load (native aborts, foreign warns and skips) — mirrors
/// [`crate::agents::resolve_registry_in`].
pub fn resolve_registry_in(root: &Path, user: &UserLayer) -> Result<Vec<SkillResolution>> {
    let mut order: Vec<String> = Vec::new();
    let mut by_name: HashMap<String, Vec<(SkillLayer, String, SkillMeta)>> = HashMap::new();
    for raw in discovery::discover(root, user)? {
        let Some(meta) = parse_raw(&raw)? else {
            continue;
        };
        let name = meta.name.clone();
        let entry = by_name.entry(name.clone()).or_default();
        if entry.is_empty() {
            order.push(name);
        }
        entry.push((raw.layer, raw.source, meta));
    }

    let mut resolved: Vec<SkillResolution> = order
        .into_iter()
        .map(|name| {
            let mut defs = by_name
                .remove(&name)
                .expect("name recorded on first insert");
            let (layer, source, meta) = defs.pop().expect("at least one definition per name");
            let shadowed = defs.into_iter().map(|(l, s, _)| (l, s)).collect();
            SkillResolution {
                meta,
                layer,
                source,
                shadowed,
            }
        })
        .collect();
    resolved.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
    Ok(resolved)
}

#[cfg(test)]
mod tests;
