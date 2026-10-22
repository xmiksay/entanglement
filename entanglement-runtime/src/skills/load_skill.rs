//! `load_skill` — tier-2 skill loading (#115, epic #111, ADR-0037).
//!
//! One generic runtime-owned tool (not one tool per skill): the model calls
//! `load_skill { skill_name }` after picking a skill from `list_skills`
//! (ADR-0226; [`super::SkillRegistry::disclosures`] — the roster left the
//! system prompt).
//! The handler resolves it **deterministically** — never model reasoning:
//!
//! 1. Look the [`SkillMeta`] up by `name` in the startup index.
//! 2. Read the `SKILL.md` body and **substitute every relative payload path to an
//!    absolute one** before the text reaches the model. This closes Claude Code's
//!    known bug class (anthropics/claude-code#17741, #11011): the *model* resolving
//!    `references/x.md` against the wrong base and guessing. `SKILL_DIR` and the
//!    project root stay two strictly separate coordinate systems — a ref that does
//!    **not** resolve under the skill dir (a project-root path) is left untouched;
//!    there is no implicit CWD fallback.
//!
//! Unlike `agent`/`ask_user`, `load_skill` is a **real host tool** in the
//! [`ToolRegistry`](crate::tools::ToolRegistry), but `Capability::Control`
//! like the other discovery tools (ADR-0226): it reads only trusted skill
//! definitions, never an arbitrary path, so it is never graded. This handler
//! owns only resolution + substitution and returns the body as
//! an ordinary `tool_result` — never a spoofed user message, so the authorship
//! trail stays honest.
//!
//! **Provenance** (`skill_id`, tracked while a skill is "active" to feed the
//! `OutEvent::SkillActive` posture event and the audit trail — no longer a
//! tool mask, ADR-0194) is a runtime tool-execution-record field. This
//! handler surfaces `skill_id` in the result so the trail is already visible.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use crate::capability::Capability;
use crate::tools::Tool;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;

use super::SkillRegistry;
use crate::tool_names::LOAD_SKILL_TOOL;

/// The `load_skill` host tool. Holds the shared, live-reloadable
/// [`SkillRegistry`] (#329: an `Arc<RwLock<..>>` so a definitions-dir change
/// swaps in a fresh registry without restarting the engine) and resolves a
/// `skill_name` against its *current* value deterministically.
pub struct LoadSkillTool {
    registry: Arc<RwLock<Arc<SkillRegistry>>>,
}

impl LoadSkillTool {
    pub fn new(registry: Arc<RwLock<Arc<SkillRegistry>>>) -> Self {
        Self { registry }
    }
}

#[derive(Deserialize)]
struct LoadSkillInput {
    skill_name: String,
}

#[async_trait]
impl Tool for LoadSkillTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(LOAD_SKILL_TOOL)
    }
    fn capabilities(&self) -> &'static [Capability] {
        &[Capability::Control]
    }
    fn description(&self) -> &str {
        "Load a skill's full instructions by name (list_skills shows them). \
         Returns the skill body with every file path \
         resolved to an absolute path, the script functions its lib.rhai adds \
         (signatures with their docs), plus a list of its supporting reference \
         files — read those on demand with the `read` tool, run a .rhai one with \
         `rhai` `path`. Deterministic: the skill_name must match an indexed skill \
         exactly."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill_name": {
                    "type": "string",
                    "description": "Exact `name` of a skill from list_skills."
                }
            },
            "required": ["skill_name"]
        })
    }
    async fn run(&self, input: &str) -> Result<String> {
        let parsed: LoadSkillInput = serde_json::from_str(input)
            .context("invalid input to load_skill: expected {\"skill_name\": string}")?;
        // Snapshot the current registry `Arc` under the lock (cheap — a refcount
        // bump) rather than holding the lock across the lookup below, so a
        // concurrent reload's write lock is never contended by this async call.
        let registry = self.registry.read().unwrap().clone();
        load(&registry, &parsed.skill_name)
    }
}

/// Parse the `skill_id: <name>\n` header a successful [`render`] result begins
/// with. `None` for anything else — in particular the "tool `load_skill`
/// failed: …" text [`crate::tools::ToolRegistry::execute`] synthesizes for an
/// `Err` (unknown skill) — so the runtime skill-active posture
/// (#400, ADR-0106; posture-only since ADR-0194) only activates on an actual
/// load, never a failed attempt.
pub fn parse_skill_id(content: &str) -> Option<&str> {
    content.strip_prefix("skill_id: ")?.lines().next()
}

/// Deterministic resolution of `skill_name` against `registry`. Pure over the
/// registry + filesystem so it is unit-tested directly (unknown skill /
/// path substitution). Separated from [`Tool::run`] for exactly that reason.
fn load(registry: &SkillRegistry, skill_name: &str) -> Result<String> {
    let skill = registry
        .get(skill_name)
        .ok_or_else(|| anyhow!("unknown skill `{skill_name}`: it is not in the skill index"))?;
    Ok(render_skill_in(registry, skill))
}

/// Render one skill's full instructions the way `load_skill` returns them
/// (ADR-0228): under a `skill_id` header, the path-substituted body, its
/// `lib.rhai` function signatures with their doc comments, then the
/// `available_refs` listing.
/// Shared by the model-facing [`load`] and the agent-definition **preload**
/// (#117), so a preloaded skill reads identically to one the model loads itself.
pub(crate) fn render_skill(skill: &super::SkillMeta) -> String {
    render_skill_with(skill, None, None)
}

/// [`render_skill`] with `registry`'s naming pass applied: a lib whose
/// `skill::` segment another skill claimed first is listed without callable
/// signatures — the same pass script namespaces are registered with.
/// The `rhai` guide also lists the importable modules (ADR-0248) — the one
/// place they are disclosed, so the tool spec stays static.
pub(crate) fn render_skill_in(registry: &SkillRegistry, skill: &super::SkillMeta) -> String {
    let modules = (skill.name == RHAI_GUIDE)
        .then(|| registry.modules.listing())
        .flatten();
    render_skill_with(skill, registry.lib_collision(&skill.name), modules)
}

/// The built-in guide skill that lists the modules.
const RHAI_GUIDE: &str = "rhai";

fn render_skill_with(
    skill: &super::SkillMeta,
    collision: Option<String>,
    modules: Option<String>,
) -> String {
    // Built-ins have no on-disk home (`root_dir == None`): they are single-file,
    // so there are no relative payload paths to resolve and no refs to list.
    let (content, refs) = match skill.root_dir.as_deref() {
        Some(dir) => (substitute_paths(&skill.body, dir), list_refs(dir)),
        None => (skill.body.clone(), Vec::new()),
    };
    let functions = skill
        .lib
        .as_ref()
        .filter(|l| !l.functions.is_empty())
        .map(|lib| lib_listing(&lib.functions, collision));
    let functions = match (functions, modules) {
        (Some(f), Some(m)) => Some(format!("{f}\n\n{m}")),
        (f, m) => f.or(m),
    };
    render(
        &skill.name,
        skill.root_dir.as_deref(),
        &content,
        functions.as_deref(),
        &refs,
    )
}

/// The `script functions` block: one signature per line, its doc comment
/// indented below it — signature first, as #826 found that listing shape
/// the most accurate. A lib whose `skill::` segment another skill claimed
/// first lists nothing callable.
fn lib_listing(functions: &[super::lib_rhai::LibFn], collision: Option<String>) -> String {
    if let Some(other) = collision {
        return format!(
            "script functions: no rhai name (collides with {other}) — not callable from a script"
        );
    }
    let mut out = String::from("script functions (call from a `rhai` script):");
    for f in functions {
        out.push_str(&format!("\n  {}", f.signature));
        for line in f.doc.lines() {
            out.push_str(&format!("\n      {line}"));
        }
    }
    out
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
                && path.file_name().and_then(|n| n.to_str()) == Some(super::discovery::SKILL_FILE);
            if !is_marker {
                refs.push(path);
            }
        }
    }
}

/// Render the tool result: `skill_id` + skill dir header, the path-substituted
/// body, the lib's script functions, then the (loaded-on-demand) reference
/// listing.
fn render(
    name: &str,
    skill_dir: Option<&Path>,
    content: &str,
    functions: Option<&str>,
    refs: &[PathBuf],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("skill_id: {name}\n"));
    if let Some(dir) = skill_dir {
        out.push_str(&format!("skill_dir: {}\n", dir.display()));
    }
    out.push('\n');
    out.push_str(content.trim_end());
    out.push_str("\n\n");
    if let Some(functions) = functions {
        out.push_str(functions);
        out.push_str("\n\n");
    }
    if refs.is_empty() {
        out.push_str("available_refs: none");
    } else {
        out.push_str(
            "available_refs (read on demand with the `read` tool; run a `.rhai` script \
             with `rhai` {\"path\": ..., \"args\": {...}}):\n",
        );
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

    /// ADR-0227/0228: a skill's `lib.rhai` functions are listed after its
    /// body, each with its doc comment, before the refs.
    #[cfg(feature = "rhai")]
    #[test]
    fn a_loaded_skill_lists_its_lib_signatures() {
        let mut s = skill("git", None, "Body.");
        s.lib = Some(
            crate::skills::lib_rhai::compile("git", "/// Last n commits.\nfn log(n) { n }")
                .unwrap(),
        );
        let out = render_skill(&s);
        assert!(
            out.contains(
                "Body.\n\nscript functions (call from a `rhai` script):\n  skill::git::log(n)\n      \
                 Last n commits.\n\navailable_refs: none"
            ),
            "{out}"
        );
        assert!(!render_skill(&skill("git", None, "Body.")).contains("script functions"));
        // A lib that lost its `skill::` segment lists no callable names.
        let mut reg = SkillRegistry::default();
        let mut other = skill("git-x", None, "B.");
        other.lib = Some(crate::skills::lib_rhai::compile("git-x", "fn a() { 1 }").unwrap());
        let mut loser = skill("git.x", None, "B.");
        loser.lib = Some(crate::skills::lib_rhai::compile("git.x", "fn b() { 1 }").unwrap());
        reg.insert(other);
        reg.insert(loser.clone());
        let out = render_skill_in(&reg, &loser);
        assert!(out.contains("collides with skill `git-x`"), "{out}");
        assert!(!out.contains("skill::git_x::b()"), "{out}");
    }

    /// ADR-0248: the `rhai` guide — and only it — ends with the modules,
    /// each with its layer and its functions' docs, before the refs.
    #[cfg(feature = "rhai")]
    #[test]
    fn the_rhai_guide_lists_the_modules() {
        let mut reg = registry(vec![skill("rhai", None, "Guide."), skill("x", None, "X.")]);
        reg.modules = Arc::new(crate::skills::rhai_modules::load_from(&[]));
        let out = load(&reg, "rhai").unwrap();
        assert!(
            out.contains("Guide.\n\nmodules (`import \"<name>\" as m;`"),
            "{out}"
        );
        assert!(out.contains(
            "\n  json_schema (built-in)\n    validate(value, schema)\n        Validate `value`"
        ));
        assert!(out.ends_with("available_refs: none"), "{out}");
        assert!(!load(&reg, "x").unwrap().contains("json_schema"));
    }

    fn skill(name: &str, root_dir: Option<PathBuf>, body: &str) -> SkillMeta {
        SkillMeta {
            name: name.into(),
            description: "d".into(),
            allowed_tools: None,
            root_dir,
            body: body.into(),
            lib: None,
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
    fn capability_is_control() {
        let reg = Arc::new(RwLock::new(Arc::new(SkillRegistry::default())));
        assert_eq!(
            LoadSkillTool::new(reg).capabilities(),
            &[Capability::Control]
        );
    }

    #[test]
    fn parse_skill_id_reads_the_header_of_a_successful_load() {
        let reg = registry(vec![skill("commit", None, "run `git commit`")]);
        let out = load(&reg, "commit").unwrap();
        assert_eq!(parse_skill_id(&out), Some("commit"));
    }

    #[test]
    fn parse_skill_id_is_none_for_a_failed_load() {
        // Mirrors the `tool `load_skill` failed: …` text `ToolRegistry::execute`
        // synthesizes for an `Err` — no `skill_id:` header to parse.
        let reg = registry(vec![]);
        let err = load(&reg, "nope").unwrap_err();
        let synthesized = format!("tool `load_skill` failed: {err:#}");
        assert_eq!(parse_skill_id(&synthesized), None);
    }

    #[test]
    fn unknown_skill_is_an_error() {
        let reg = registry(vec![]);
        let err = load(&reg, "nope").unwrap_err();
        assert!(format!("{err:#}").contains("unknown skill"), "got: {err:#}");
    }

    /// ADR-0229: a skill still declaring the removed `user_only` key loads
    /// for the model like any other.
    #[test]
    fn formerly_user_only_skill_loads() {
        let deploy = crate::skills::parse::parse_skill(
            "---\nname: deploy\ndescription: d\nuser_only: true\n---\ndanger",
            None,
        )
        .unwrap();
        let out = load(&registry(vec![deploy]), "deploy").unwrap();
        assert_eq!(parse_skill_id(&out), Some("deploy"));
        assert!(out.contains("danger"), "{out}");
    }

    #[test]
    fn built_in_body_returned_verbatim_with_skill_id() {
        // No root_dir ⇒ no substitution, no refs.
        let reg = registry(vec![skill("commit", None, "run `git commit`")]);
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
        let reg = registry(vec![skill("myskill", Some(skill_dir.clone()), body)]);
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

        let reg = registry(vec![skill("myskill", Some(skill_dir.clone()), "body")]);
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
