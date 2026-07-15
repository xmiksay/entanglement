//! `skutter inspect skills [name] [--disclosures]` (#186).
//!
//! Closes the skill-authoring blind spot: skill selection is "description quality
//! is the contract," yet the author could not see the exact tier-1
//! `disclosures()` block the model gets, whether `user_only` withheld a skill,
//! which layer won a collision, or whether a `${SKILL_DIR}` payload path resolves.
//! Three views over the engine-free skill registry:
//! - `--disclosures`: the exact tier-1 block the model receives (takes priority);
//! - a `name`: a dry-run of the `load_skill` path substitution for that skill;
//! - neither: a table of every resolved skill with its winning layer + provenance.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use crate::skills::{self, SkillResolution};
use crate::system_prompt;

/// Dispatch the three inspection views (see the module doc for precedence).
pub fn inspect_skills(cwd: &Path, name: Option<&str>, disclosures: bool) -> Result<()> {
    if disclosures {
        let registry = skills::load_registry(cwd).context("loading skill definitions")?;
        print!("{}", render_disclosures(&registry.disclosures()));
        return Ok(());
    }

    let resolved = skills::resolve_registry(cwd).context("resolving skill registry")?;
    match name {
        Some(name) => {
            // Resolve with provenance so the dry-run shows the layer winner (the
            // same `SkillMeta` `load_skill` would pick) plus what it overrode.
            let entry = resolved
                .iter()
                .find(|r| r.meta.name == name)
                .with_context(|| {
                    format!("unknown skill `{name}` (no matching SKILL.md found in any layer)")
                })?;
            print!("{}", render_skill_dry_run(entry));
        }
        None => print!("{}", render_skill_table(&resolved)),
    }
    Ok(())
}

/// Render the exact tier-1 disclosure block the model is handed — the same
/// [`system_prompt::render_skills`] output the assembled prompt embeds, so what
/// the author sees here is byte-for-byte what drives selection.
pub(super) fn render_disclosures(disclosures: &[crate::system_prompt::SkillDisclosure]) -> String {
    if disclosures.is_empty() {
        return "(no skills disclosed to the model — none discovered, or all are user_only)\n"
            .to_string();
    }
    format!("{}\n", system_prompt::render_skills(disclosures))
}

/// One-line-per-skill table: name, whether it is `user_only` (withheld from the
/// model), winning layer, its `root_dir`, and the description that drives
/// selection. Includes `user_only` skills — the author must see what was withheld.
pub(super) fn render_skill_table(resolved: &[SkillResolution]) -> String {
    if resolved.is_empty() {
        return "no skill definitions found\n".to_string();
    }

    let rows: Vec<Vec<String>> = resolved
        .iter()
        .map(|r| {
            vec![
                r.meta.name.clone(),
                if r.meta.user_only { "yes" } else { "no" }.to_string(),
                r.layer.label().to_string(),
                r.meta
                    .root_dir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(built-in)".into()),
                r.meta.description.clone(),
            ]
        })
        .collect();

    super::render_table(
        &["NAME", "USER_ONLY", "LAYER", "ROOT_DIR", "DESCRIPTION"],
        &rows,
    )
}

/// Dry-run the `load_skill` resolution for one skill without a model: identity +
/// layer provenance (winner + what it overrode), then the exact `load_skill`
/// output (path-substituted body + `available_refs`). A `${SKILL_DIR}` or
/// relative-path bug surfaces right here. Shared with the TUI overlay's
/// per-skill detail pane (#331).
pub(super) fn render_skill_dry_run(entry: &SkillResolution) -> String {
    let skill = &entry.meta;
    let mut out = String::new();
    let _ = writeln!(out, "name:        {}", skill.name);
    let _ = writeln!(out, "description: {}", skill.description);
    let _ = writeln!(out, "user_only:   {}", skill.user_only);
    let _ = writeln!(out, "layer:       {}", entry.layer.label());
    let _ = writeln!(out, "source:      {}", entry.source);
    match &skill.root_dir {
        Some(dir) => {
            let _ = writeln!(out, "root_dir:    {}", dir.display());
        }
        None => {
            let _ = writeln!(
                out,
                "root_dir:    (built-in, single-file — no payload paths)"
            );
        }
    }

    if entry.shadowed.is_empty() {
        let _ = writeln!(out, "overrides:   (none — no lower-layer definition)");
    } else {
        let _ = writeln!(out, "overrides:");
        for (layer, source) in &entry.shadowed {
            let _ = writeln!(out, "  - {} ({source})", layer.label());
        }
    }

    if skill.user_only {
        // `load_skill` refuses a `user_only` skill to a model; this authoring
        // dry-run renders it anyway — call out that the model never sees it.
        let _ = writeln!(
            out,
            "\nnote: user_only — the model can never load this via `load_skill`; \
             shown here for authoring only."
        );
    }

    let _ = writeln!(
        out,
        "\n─── load_skill output (path substitution applied) ───\n"
    );
    let _ = writeln!(out, "{}", skills::load_skill::render_skill(skill));
    out
}
