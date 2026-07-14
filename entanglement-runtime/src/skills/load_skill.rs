//! `load_skill` — tier-2 skill loading (#115, epic #111, ADR-0037).
//!
//! One generic runtime-owned tool (not one tool per skill): the model calls
//! `load_skill { skill_name }` after picking a skill from the tier-1 disclosure
//! list (`name: description` lines in its system prompt, [`super::SkillRegistry::disclosures`]).
//! The handler resolves it **deterministically** — never model reasoning:
//!
//! 1. Look the [`SkillMeta`] up by `name` in the startup index.
//! 2. Reject a `user_only` skill: it is withheld from disclosure and only an
//!    explicit user command may trigger it, a channel the headless engine has no
//!    model-driven equivalent for — so a model-issued `load_skill` is refused.
//! 3. Read the `SKILL.md` body and **substitute every relative payload path to an
//!    absolute one** before the text reaches the model. This closes Claude Code's
//!    known bug class (anthropics/claude-code#17741, #11011): the *model* resolving
//!    `references/x.md` against the wrong base and guessing. `SKILL_DIR` and the
//!    project root stay two strictly separate coordinate systems — a ref that does
//!    **not** resolve under the skill dir (a project-root path) is left untouched;
//!    there is no implicit CWD fallback.
//!
//! Unlike `agent_spawn`/`ask_user`, `load_skill` is a **real host tool** in the
//! [`ToolRegistry`](crate::tools::ToolRegistry): it reads the
//! filesystem, so it goes through the *same* per-call permission gate as `read`
//! (no special exemption). Permission (`Allow`/`Ask`/`Deny`) is the executor's
//! job; this handler owns only resolution + substitution and returns the body as
//! an ordinary `tool_result` — never a spoofed user message, so the authorship
//! trail stays honest.
//!
//! **Provenance** (`skill_id` carried onto tool calls made while a skill is
//! "active", to scope its `allowed_tools` mask and feed the audit trail) is a
//! runtime tool-execution-record field; it lands with mask *enforcement* (#116).
//! This handler surfaces `skill_id` in the result so the trail is already visible.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::tools::Tool;
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;

use super::SkillRegistry;
use crate::tool_names::LOAD_SKILL_TOOL;

/// The `load_skill` host tool. Holds the shared startup [`SkillRegistry`] and
/// resolves a `skill_name` against it deterministically.
pub struct LoadSkillTool {
    registry: Arc<SkillRegistry>,
}

impl LoadSkillTool {
    pub fn new(registry: Arc<SkillRegistry>) -> Self {
        Self { registry }
    }
}

#[derive(Deserialize)]
struct LoadSkillInput {
    skill_name: String,
}

#[async_trait]
impl Tool for LoadSkillTool {
    fn name(&self) -> &'static str {
        LOAD_SKILL_TOOL
    }
    fn description(&self) -> &str {
        "Load a skill's full instructions by name (pick one from the skill index \
         in your system prompt). Returns the skill body with every file path \
         resolved to an absolute path, plus a list of its supporting reference \
         files — read those on demand with the `read` tool. Deterministic: the \
         skill_name must match an indexed skill exactly."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill_name": {
                    "type": "string",
                    "description": "Exact `name` of a skill from the skill index."
                }
            },
            "required": ["skill_name"]
        })
    }
    async fn run(&self, input: &str) -> Result<String> {
        let parsed: LoadSkillInput = serde_json::from_str(input)
            .context("invalid input to load_skill: expected {\"skill_name\": string}")?;
        load(&self.registry, &parsed.skill_name)
    }
}

/// Deterministic resolution of `skill_name` against `registry`. Pure over the
/// registry + filesystem so it is unit-tested directly (unknown / user_only /
/// path substitution). Separated from [`Tool::run`] for exactly that reason.
fn load(registry: &SkillRegistry, skill_name: &str) -> Result<String> {
    let skill = registry
        .get(skill_name)
        .ok_or_else(|| anyhow!("unknown skill `{skill_name}`: it is not in the skill index"))?;
    if skill.user_only {
        bail!(
            "skill `{skill_name}` is user_only and can only be triggered by an \
             explicit user command, not a model-issued load_skill"
        );
    }
    Ok(render_skill(skill))
}

/// Render one skill's full instructions the way `load_skill` returns them:
/// path-substituted body + `available_refs` listing, under a `skill_id` header.
/// Shared by the model-facing [`load`] and the agent-definition **preload**
/// (#117), so a preloaded skill reads identically to one the model loads itself.
/// Carries no `user_only` gate — that policy belongs to the caller (`load`
/// enforces it; preload is author config and does not).
pub(crate) fn render_skill(skill: &super::SkillMeta) -> String {
    // Built-ins have no on-disk home (`root_dir == None`): they are single-file,
    // so there are no relative payload paths to resolve and no refs to list.
    let (content, refs) = match skill.root_dir.as_deref() {
        Some(dir) => (substitute_paths(&skill.body, dir), list_refs(dir)),
        None => (skill.body.clone(), Vec::new()),
    };
    render(&skill.name, skill.root_dir.as_deref(), &content, &refs)
}

/// Rewrite relative payload paths in `body` to absolute paths under `skill_dir`.
///
/// Two mechanisms, both deterministic against the filesystem at load time:
/// - the explicit `${SKILL_DIR}` / `$SKILL_DIR` placeholder → the absolute skill
///   directory (an author's unambiguous escape hatch);
/// - any relative path token that **resolves to an existing entry under the skill
///   dir** → its absolute path. A token that does not resolve there (a
///   project-root ref like `src/main.rs`) is a different coordinate system and is
///   left untouched — no implicit CWD fallback.
fn substitute_paths(body: &str, skill_dir: &Path) -> String {
    let dir_str = skill_dir.to_string_lossy();
    let body = body
        .replace("${SKILL_DIR}", &dir_str)
        .replace("$SKILL_DIR", &dir_str);

    static PATH_TOKEN: OnceLock<Regex> = OnceLock::new();
    // A path-ish token: starts with a word char or `.`, may contain `/._-`. The
    // existence probe below is what actually decides a rewrite, so the regex only
    // needs to bound candidates loosely.
    let re = PATH_TOKEN.get_or_init(|| Regex::new(r"[A-Za-z0-9_.][A-Za-z0-9_./-]*").unwrap());

    re.replace_all(&body, |caps: &regex::Captures| {
        let tok = &caps[0];
        // Strip trailing sentence punctuation so `references/x.md.` at a line end
        // still probes `references/x.md` and keeps the `.` as a literal suffix.
        let path = tok.trim_end_matches(['.', ',', ';', ':', ')']);
        let suffix = &tok[path.len()..];
        // Only relative, slash-bearing tokens are payload-path candidates; a bare
        // word (`read`) or an already-absolute path is never rewritten.
        if path.is_empty() || Path::new(path).is_absolute() || !path.contains('/') {
            return tok.to_string();
        }
        let candidate = skill_dir.join(path);
        if candidate.exists() {
            format!("{}{suffix}", candidate.display())
        } else {
            tok.to_string()
        }
    })
    .into_owned()
}

/// Absolute paths of every supporting file under `skill_dir` (recursively),
/// excluding the top-level `SKILL.md` marker. Sorted for a stable listing.
fn list_refs(skill_dir: &Path) -> Vec<PathBuf> {
    let mut refs = Vec::new();
    collect_refs(skill_dir, skill_dir, &mut refs);
    refs.sort();
    refs
}

fn collect_refs(root: &Path, dir: &Path, refs: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            collect_refs(root, &path, refs);
        } else if meta.is_file() {
            // The SKILL.md at the skill root is the marker, not a reference.
            let is_marker = path.parent() == Some(root)
                && path.file_name().and_then(|n| n.to_str()) == Some(super::SKILL_FILE);
            if !is_marker {
                refs.push(path);
            }
        }
    }
}

/// Render the tool result: `skill_id` + skill dir header, the path-substituted
/// body, then the (loaded-on-demand) reference listing.
fn render(name: &str, skill_dir: Option<&Path>, content: &str, refs: &[PathBuf]) -> String {
    let mut out = String::new();
    out.push_str(&format!("skill_id: {name}\n"));
    if let Some(dir) = skill_dir {
        out.push_str(&format!("skill_dir: {}\n", dir.display()));
    }
    out.push('\n');
    out.push_str(content.trim_end());
    out.push_str("\n\n");
    if refs.is_empty() {
        out.push_str("available_refs: none");
    } else {
        out.push_str("available_refs (read on demand with the `read` tool):\n");
        for r in refs {
            out.push_str(&format!("- {}\n", r.display()));
        }
        // Trim the trailing newline for a tidy result.
        out.truncate(out.trim_end().len());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillMeta;

    fn skill(name: &str, user_only: bool, root_dir: Option<PathBuf>, body: &str) -> SkillMeta {
        SkillMeta {
            name: name.into(),
            description: "d".into(),
            user_only,
            allowed_tools: None,
            root_dir,
            body: body.into(),
        }
    }

    fn registry(skills: Vec<SkillMeta>) -> SkillRegistry {
        let mut reg = SkillRegistry::default();
        for s in skills {
            reg.insert(s);
        }
        reg
    }

    #[test]
    fn unknown_skill_is_an_error() {
        let reg = registry(vec![]);
        let err = load(&reg, "nope").unwrap_err();
        assert!(format!("{err:#}").contains("unknown skill"), "got: {err:#}");
    }

    #[test]
    fn user_only_skill_is_rejected() {
        let reg = registry(vec![skill("deploy", true, None, "danger")]);
        let err = load(&reg, "deploy").unwrap_err();
        assert!(format!("{err:#}").contains("user_only"), "got: {err:#}");
    }

    #[test]
    fn built_in_body_returned_verbatim_with_skill_id() {
        // No root_dir ⇒ no substitution, no refs.
        let reg = registry(vec![skill("commit", false, None, "run `git commit`")]);
        let out = load(&reg, "commit").unwrap();
        assert!(out.contains("skill_id: commit"), "got: {out}");
        assert!(out.contains("run `git commit`"), "got: {out}");
        assert!(out.contains("available_refs: none"), "got: {out}");
    }

    #[test]
    fn relative_refs_become_absolute_project_refs_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("myskill");
        std::fs::create_dir_all(skill_dir.join("references")).unwrap();
        std::fs::write(skill_dir.join("references/guide.md"), "g").unwrap();

        // Body mixes a skill-dir ref (exists under the skill dir), a project-root
        // ref (does not), and a placeholder.
        let body = "See references/guide.md for details. Edit src/main.rs. Dir is ${SKILL_DIR}.";
        let reg = registry(vec![skill("myskill", false, Some(skill_dir.clone()), body)]);
        let out = load(&reg, "myskill").unwrap();

        let abs_ref = skill_dir.join("references/guide.md");
        assert!(
            out.contains(&abs_ref.display().to_string()),
            "relative ref not made absolute; got: {out}"
        );
        // The trailing period after the ref is preserved as a literal.
        assert!(
            out.contains(&format!("{} for details", abs_ref.display())),
            "trailing punctuation dropped; got: {out}"
        );
        // Project-root ref lives in a separate coordinate system — untouched.
        assert!(
            out.contains("src/main.rs"),
            "project-root ref must not be rewritten; got: {out}"
        );
        assert!(
            !out.contains(&skill_dir.join("src/main.rs").display().to_string()),
            "project-root ref wrongly resolved under skill dir; got: {out}"
        );
        // ${SKILL_DIR} placeholder resolved to the absolute skill dir.
        assert!(
            out.contains(&format!("Dir is {}", skill_dir.display())),
            "placeholder not substituted; got: {out}"
        );
    }

    #[test]
    fn available_refs_list_supporting_files_not_skill_md() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("myskill");
        std::fs::create_dir_all(skill_dir.join("references")).unwrap();
        std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "marker").unwrap();
        std::fs::write(skill_dir.join("references/a.md"), "a").unwrap();
        std::fs::write(skill_dir.join("scripts/run.sh"), "s").unwrap();

        let reg = registry(vec![skill(
            "myskill",
            false,
            Some(skill_dir.clone()),
            "body",
        )]);
        let out = load(&reg, "myskill").unwrap();

        assert!(out.contains("available_refs"), "got: {out}");
        assert!(
            out.contains(&skill_dir.join("references/a.md").display().to_string()),
            "got: {out}"
        );
        assert!(
            out.contains(&skill_dir.join("scripts/run.sh").display().to_string()),
            "got: {out}"
        );
        // The marker file is not a reference.
        assert!(
            !out.contains(&skill_dir.join("SKILL.md").display().to_string()),
            "SKILL.md must not be listed as a ref; got: {out}"
        );
    }
}
