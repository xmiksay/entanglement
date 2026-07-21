//! Skill discovery + registry (#114, tier-1 progressive disclosure, epic #111).
//!
//! A **skill** is a directory holding a `SKILL.md` (YAML frontmatter + markdown
//! body) plus optional supporting files (`references/*.md`, `scripts/*`). The
//! frontmatter is the tier-1 contract (`name` + `description`); the body and its
//! payload are tier-2, loaded on demand — never preloaded into the prompt.
//!
//! Discovery mirrors the file-based agent loader (#112) and the provider catalog
//! (#118): embedded stock skills, then user, then project — later layers replace
//! earlier ones on a `name` collision (**project > user > built-in**). A stock
//! skill is edited by dropping a same-`name` `SKILL.md` in a higher layer.
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
//!
//! # Disclosure
//!
//! Only `name` + `description` reach the model (a compact list in the assembled
//! system prompt — see [`crate::system_prompt`]). Selection stays LLM reasoning:
//! the model matches its task against the `description` in its own forward pass.
//! There is no keyword router or embedding gate — description quality is the
//! contract. `user_only` skills are withheld from the disclosure list so the
//! model cannot self-trigger them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::layers::Strictness;
use crate::system_prompt::SkillDisclosure;

mod discovery;
pub mod load_skill;
pub use load_skill::LoadSkillTool;

/// One discovered skill: the tier-1 metadata plus the loaded body. `root_dir` is
/// resolved **once** here at discovery (the directory holding `SKILL.md` and its
/// `references/`/`scripts/` payload) and never re-derived downstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMeta {
    /// Unique id; what an invocation / tier-2 load references.
    pub name: String,
    /// One-line summary; the only field (with `name`) disclosed to the model.
    pub description: String,
    /// `true` ⇒ only explicit user invocation can trigger it (destructive/deploy
    /// skills). Withheld from the model's disclosure list.
    pub user_only: bool,
    /// Tool mask active while the skill is loaded, enforced by
    /// `permission::skill_masked` (ADR-0106). `None` ⇒ inherit the session's
    /// tools.
    pub allowed_tools: Option<Vec<String>>,
    /// The skill directory (holds `SKILL.md` + payload). `None` for embedded
    /// built-ins, which have no on-disk home.
    pub root_dir: Option<PathBuf>,
    /// The markdown body below the frontmatter (tier-2 content).
    pub body: String,
}

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

/// Lenient frontmatter for cross-vendor skills (ADR-0074): only the tier-1
/// contract is read, every other key (Claude Code's `allowed-tools`, `license`,
/// `argument-hint`, …) is ignored. Claude's `disable-model-invocation` maps to
/// [`SkillMeta::user_only`] — same semantics, the model must not self-trigger.
/// Claude's `allowed-tools` is deliberately dropped: its tool names don't map
/// onto entanglement's, and `allowed_tools` enforcement (ADR-0106) keys off
/// `SkillMeta::allowed_tools`, which a foreign skill never populates.
#[derive(Debug, Deserialize)]
struct ForeignSkillFrontmatter {
    name: String,
    description: String,
    #[serde(default, rename = "disable-model-invocation")]
    disable_model_invocation: bool,
}

/// Named set of discovered [`SkillMeta`], keyed by `name`. [`insert`][Self::insert]
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
    /// a skill's body into the agent's assembled system prompt at load time. Two
    /// deliberate differences from the model-facing `load_skill`:
    ///
    /// - a `user_only` skill **is** preloadable — the author opted in explicitly,
    ///   so the "model cannot self-trigger" guard does not apply;
    /// - an unknown name is a hard error, surfaced loudly at load time (agent
    ///   definitions validate loudly, never silently drop a typo'd skill).
    pub fn preload_body(&self, name: &str) -> Result<String> {
        let skill = self.get(name).ok_or_else(|| {
            anyhow::anyhow!("unknown preload skill `{name}`: it is not in the skill index")
        })?;
        Ok(load_skill::render_skill(skill))
    }

    /// Tier-1 disclosure lines for the system prompt: `name` + `description`
    /// only, name-sorted. `user_only` skills are excluded — the model must not
    /// see a skill it cannot self-trigger.
    pub fn disclosures(&self) -> Vec<SkillDisclosure> {
        self.iter()
            .filter(|s| !s.user_only)
            .map(|s| SkillDisclosure {
                name: s.name.clone(),
                description: s.description.clone(),
            })
            .collect()
    }
}

/// Which precedence layer a skill definition came from (#186). The shared
/// [`crate::layers::Layer`] — `built-in < user < project`, later wins on a
/// `name` collision — re-exported under the skills-facing name.
pub use crate::layers::Layer as SkillLayer;

use discovery::RawSkill;

/// Parse one discovered `SKILL.md` honoring its layer's strictness (ADR-0074):
/// strict → the current loud behavior (a malformed file aborts the load);
/// lenient → `Ok(None)` after a `warn!`, so a foreign file entanglement doesn't
/// own is skipped instead of bricking startup.
fn parse_raw(raw: &RawSkill) -> Result<Option<SkillMeta>> {
    match raw.strictness {
        Strictness::Strict => parse_skill(&raw.content, raw.root_dir.clone())
            .with_context(|| format!("parsing skill {}", raw.source))
            .map(Some),
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

/// Load the skill registry for `root`: embedded stock skills, then the user
/// dirs, then the project dirs — later layers replace earlier ones on a `name`
/// collision (project > user > built-in). A malformed file in a native layer
/// aborts; a malformed foreign file is warned and skipped.
pub fn load_registry(root: &Path) -> Result<SkillRegistry> {
    let mut reg = SkillRegistry::default();
    // Track the winning (layer, source) per name so a later-wins collision is no
    // longer silent (#186): emit a `replaces=<prior source>` debug at the
    // overwrite, matching the provenance `inspect skills` surfaces.
    let mut winning: HashMap<String, (SkillLayer, String)> = HashMap::new();
    for raw in discovery::discover(root)? {
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
    /// The winning skill metadata (name/description/user_only/root_dir).
    pub meta: SkillMeta,
    /// Which precedence layer the winner came from.
    pub layer: SkillLayer,
    /// The winner's origin (`built-in (commit.md)` or a file path).
    pub source: String,
    /// Lower-layer definitions of the same name the winner overrode, in
    /// precedence order — `(layer, source)` each. Empty when nothing was shadowed.
    pub shadowed: Vec<(SkillLayer, String)>,
}

/// Resolve every skill for `root` with full provenance (#186), applying the same
/// layer precedence as [`load_registry`] but keeping *which* layer won and what
/// it shadowed. Sorted by name for a stable table. Malformed files behave
/// exactly as at load (native aborts, foreign warns and skips) — mirrors
/// [`crate::agents::resolve_registry`].
pub fn resolve_registry(root: &Path) -> Result<Vec<SkillResolution>> {
    let mut order: Vec<String> = Vec::new();
    let mut by_name: HashMap<String, Vec<(SkillLayer, String, SkillMeta)>> = HashMap::new();
    for raw in discovery::discover(root)? {
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

/// Parse a `SKILL.md`: split frontmatter from body, deserialize the frontmatter,
/// and build a [`SkillMeta`] carrying the pre-resolved `root_dir`.
fn parse_skill(content: &str, root_dir: Option<PathBuf>) -> Result<SkillMeta> {
    let (frontmatter, body) = crate::frontmatter::split(content)?;
    let fm: SkillFrontmatter =
        serde_yaml::from_str(&frontmatter).context("invalid skill frontmatter")?;
    if fm.name.trim().is_empty() {
        bail!("skill frontmatter `name` must not be empty");
    }
    Ok(SkillMeta {
        name: fm.name,
        description: fm.description,
        user_only: fm.user_only,
        allowed_tools: fm.allowed_tools,
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
    Ok(SkillMeta {
        name: fm.name,
        description: fm.description,
        user_only: fm.disable_model_invocation,
        allowed_tools: None,
        root_dir,
        body,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Mutex;

    use super::discovery::{walk, BUILT_INS, SKILLS_DIR_ENV, SKILL_FILE};
    use super::*;

    /// `load_registry` reads a process-global env var (`SKILLS_DIR_ENV`); tests
    /// that set it must not race under cargo's parallel test threads.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn parse(content: &str) -> Result<SkillMeta> {
        parse_skill(content, None)
    }

    /// Write a `<dir>/<name>/SKILL.md` with the given frontmatter/body.
    fn write_skill(base: &Path, dir: &str, contents: &str) -> PathBuf {
        let skill_dir = base.join(dir);
        std::fs::create_dir_all(&skill_dir).unwrap();
        let path = skill_dir.join(SKILL_FILE);
        std::fs::write(&path, contents).unwrap();
        skill_dir
    }

    #[test]
    fn built_ins_parse() {
        // The embedded stock skills must parse — this is what lets
        // `load_registry` treat their parse as infallible.
        let mut reg = SkillRegistry::default();
        for (file, contents) in BUILT_INS {
            let s = parse(contents).unwrap_or_else(|e| panic!("{file}: {e}"));
            reg.insert(s);
        }
        let commit = reg.get("commit").expect("commit built-in");
        assert!(!commit.user_only);
        assert_eq!(commit.root_dir, None);
        assert!(commit.body.contains("Conventional Commit"));
        assert_eq!(
            commit.allowed_tools.as_deref(),
            Some(&["bash".to_string(), "read".to_string(), "grep".to_string()][..])
        );
    }

    #[test]
    fn required_fields_and_defaults() {
        let s = parse("---\nname: x\ndescription: d\n---\nbody").unwrap();
        assert_eq!(s.name, "x");
        assert_eq!(s.description, "d");
        assert!(!s.user_only);
        assert_eq!(s.allowed_tools, None);
        assert_eq!(s.body, "body");
    }

    #[test]
    fn missing_description_is_an_error() {
        let err = parse("---\nname: x\n---\nbody").unwrap_err();
        assert!(format!("{err:#}").contains("description"), "got: {err:#}");
    }

    #[test]
    fn unknown_field_is_rejected() {
        let err = parse("---\nname: x\ndescription: d\ntypo: 1\n---\nb").unwrap_err();
        assert!(format!("{err:#}").contains("typo"), "got: {err:#}");
    }

    #[test]
    fn missing_frontmatter_is_an_error() {
        let err = parse("just a body").unwrap_err();
        assert!(err.to_string().contains("frontmatter"), "got: {err}");
    }

    #[test]
    fn empty_name_is_an_error() {
        let err = parse("---\nname: '   '\ndescription: d\n---\nb").unwrap_err();
        assert!(err.to_string().contains("name"), "got: {err}");
    }

    #[test]
    fn discovery_walks_nested_dirs_and_resolves_root_dir_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Nested one level deeper than <skills>/<name>/SKILL.md.
        let skills = root.join(".entanglement").join("skills");
        let deep = write_skill(
            &skills,
            "group/nested",
            "---\nname: nested\ndescription: a nested skill\n---\nbody",
        );
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Point the user layer at a non-existent dir so a real ~/.config skill
        // can't leak into the assertion.
        std::env::set_var(SKILLS_DIR_ENV, root.join("no-such-user-dir"));
        let reg = load_registry(root).unwrap();
        std::env::remove_var(SKILLS_DIR_ENV);

        let s = reg.get("nested").expect("nested skill discovered");
        assert_eq!(s.description, "a nested skill");
        assert_eq!(s.root_dir.as_deref(), Some(deep.as_path()));
    }

    #[test]
    fn project_wins_over_user_on_name_collision() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let user = root.join("user-skills");
        write_skill(
            &user,
            "dup",
            "---\nname: dup\ndescription: from user\n---\nu",
        );
        let project = root.join(".entanglement").join("skills");
        write_skill(
            &project,
            "dup",
            "---\nname: dup\ndescription: from project\n---\np",
        );
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(SKILLS_DIR_ENV, &user);
        let reg = load_registry(root).unwrap();
        std::env::remove_var(SKILLS_DIR_ENV);

        assert_eq!(reg.get("dup").unwrap().description, "from project");
    }

    #[test]
    fn user_skill_overrides_built_in() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let user = root.join("user-skills");
        write_skill(
            &user,
            "commit",
            "---\nname: commit\ndescription: my override\n---\nx",
        );
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(SKILLS_DIR_ENV, &user);
        let reg = load_registry(root).unwrap();
        std::env::remove_var(SKILLS_DIR_ENV);

        assert_eq!(reg.get("commit").unwrap().description, "my override");
    }

    #[test]
    fn resolve_registry_surfaces_layer_winner_and_shadowed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A user override of the built-in `commit` skill: project wins nothing
        // here, so `user` is the winner and the built-in is shadowed.
        let user = root.join("user-skills");
        write_skill(
            &user,
            "commit",
            "---\nname: commit\ndescription: user commit\n---\nx",
        );
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(SKILLS_DIR_ENV, &user);
        let resolved = resolve_registry(root).unwrap();
        std::env::remove_var(SKILLS_DIR_ENV);

        let commit = resolved
            .iter()
            .find(|r| r.meta.name == "commit")
            .expect("commit resolved");
        assert_eq!(commit.layer, SkillLayer::User);
        assert_eq!(commit.meta.description, "user commit");
        // The built-in it overrode is recorded, not swallowed.
        assert_eq!(commit.shadowed.len(), 1, "built-in should be shadowed");
        assert_eq!(commit.shadowed[0].0, SkillLayer::BuiltIn);
    }

    #[test]
    fn resolve_registry_is_name_sorted_and_records_no_shadow_when_unique() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let project = root.join(".entanglement").join("skills");
        write_skill(&project, "zzz", "---\nname: zzz\ndescription: last\n---\nb");
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(SKILLS_DIR_ENV, root.join("no-such-user-dir"));
        let resolved = resolve_registry(root).unwrap();
        std::env::remove_var(SKILLS_DIR_ENV);

        // Name-sorted: the built-in `commit` sorts before the project `zzz`.
        let names: Vec<&str> = resolved.iter().map(|r| r.meta.name.as_str()).collect();
        assert_eq!(names, vec!["commit", "zzz"]);
        let zzz = resolved.iter().find(|r| r.meta.name == "zzz").unwrap();
        assert_eq!(zzz.layer, SkillLayer::Project);
        assert!(zzz.shadowed.is_empty(), "unique skill shadows nothing");
    }

    #[test]
    fn symlinked_duplicate_is_deduped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let skills = root.join(".entanglement").join("skills");
        let real = write_skill(
            &skills,
            "real",
            "---\nname: real\ndescription: the one skill\n---\nb",
        );
        // A sibling directory symlinked to the real skill dir surfaces the same
        // SKILL.md a second time; canonical-path dedup collapses it.
        std::os::unix::fs::symlink(&real, skills.join("linked")).unwrap();

        let mut found = Vec::new();
        let mut seen = HashSet::new();
        walk(&skills, &mut found, &mut seen).unwrap();
        assert_eq!(found.len(), 1, "symlinked duplicate not deduped: {found:?}");
    }

    #[test]
    fn malformed_skill_file_aborts_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let skills = root.join(".entanglement").join("skills");
        write_skill(&skills, "bad", "no frontmatter at all");
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(SKILLS_DIR_ENV, root.join("no-such-user-dir"));
        let err = load_registry(root).unwrap_err();
        std::env::remove_var(SKILLS_DIR_ENV);
        assert!(format!("{err:#}").contains("bad"), "got: {err:#}");
    }

    #[test]
    fn foreign_skill_with_unknown_keys_loads_leniently() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Claude-Code-style frontmatter: keys the strict schema would reject.
        write_skill(
            &root.join(".claude").join("skills"),
            "arch",
            "---\nname: arch\ndescription: from claude\nallowed-tools: Bash(git:*), Read\nlicense: MIT\n---\nbody",
        );
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(SKILLS_DIR_ENV, root.join("no-such-user-dir"));
        let reg = load_registry(root).unwrap();
        std::env::remove_var(SKILLS_DIR_ENV);

        let s = reg.get("arch").expect("foreign skill discovered");
        assert_eq!(s.description, "from claude");
        // Claude's `allowed-tools` is deliberately dropped (ADR-0074).
        assert_eq!(s.allowed_tools, None);
        assert!(!s.user_only);
    }

    #[test]
    fn foreign_disable_model_invocation_maps_to_user_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_skill(
            &root.join(".claude").join("skills"),
            "deploy",
            "---\nname: deploy\ndescription: d\ndisable-model-invocation: true\n---\nb",
        );
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(SKILLS_DIR_ENV, root.join("no-such-user-dir"));
        let reg = load_registry(root).unwrap();
        std::env::remove_var(SKILLS_DIR_ENV);

        assert!(reg.get("deploy").unwrap().user_only);
    }

    #[test]
    fn malformed_foreign_skill_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let claude = root.join(".claude").join("skills");
        write_skill(&claude, "bad", "no frontmatter at all");
        write_skill(&claude, "good", "---\nname: good\ndescription: ok\n---\nb");
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(SKILLS_DIR_ENV, root.join("no-such-user-dir"));
        let reg = load_registry(root).unwrap();
        std::env::remove_var(SKILLS_DIR_ENV);

        assert!(reg.get("bad").is_none(), "malformed foreign skill skipped");
        assert_eq!(reg.get("good").unwrap().description, "ok");
    }

    #[test]
    fn agents_dir_skill_discovered_and_native_project_wins() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_skill(
            &root.join(".claude").join("skills"),
            "dup",
            "---\nname: dup\ndescription: from .claude\n---\nc",
        );
        write_skill(
            &root.join(".agents").join("skills"),
            "dup",
            "---\nname: dup\ndescription: from .agents\n---\na",
        );
        write_skill(
            &root.join(".agents").join("skills"),
            "only-agents",
            "---\nname: only-agents\ndescription: cross-vendor\n---\nb",
        );
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(SKILLS_DIR_ENV, root.join("no-such-user-dir"));
        let reg = load_registry(root).unwrap();
        // Native `.entanglement` beats both foreign project dirs.
        write_skill(
            &root.join(".entanglement").join("skills"),
            "dup",
            "---\nname: dup\ndescription: from native\n---\nn",
        );
        std::env::set_var(SKILLS_DIR_ENV, root.join("no-such-user-dir"));
        let reg2 = load_registry(root).unwrap();
        std::env::remove_var(SKILLS_DIR_ENV);

        assert_eq!(reg.get("only-agents").unwrap().description, "cross-vendor");
        // `.agents` (cross-vendor) beats `.claude` (vendor-specific)…
        assert_eq!(reg.get("dup").unwrap().description, "from .agents");
        // …and `.entanglement` (native) beats both.
        assert_eq!(reg2.get("dup").unwrap().description, "from native");
        let resolved = {
            std::env::set_var(SKILLS_DIR_ENV, root.join("no-such-user-dir"));
            let r = resolve_registry(root).unwrap();
            std::env::remove_var(SKILLS_DIR_ENV);
            r
        };
        let dup = resolved.iter().find(|r| r.meta.name == "dup").unwrap();
        assert_eq!(dup.layer, SkillLayer::Project);
        assert_eq!(dup.shadowed.len(), 2, "both foreign defs recorded");
    }

    #[test]
    fn disclosures_exclude_user_only_and_are_name_sorted() {
        let mut reg = SkillRegistry::default();
        reg.insert(SkillMeta {
            name: "zebra".into(),
            description: "z".into(),
            user_only: false,
            allowed_tools: None,
            root_dir: None,
            body: String::new(),
        });
        reg.insert(SkillMeta {
            name: "deploy".into(),
            description: "danger".into(),
            user_only: true,
            allowed_tools: None,
            root_dir: None,
            body: String::new(),
        });
        reg.insert(SkillMeta {
            name: "alpha".into(),
            description: "a".into(),
            user_only: false,
            allowed_tools: None,
            root_dir: None,
            body: String::new(),
        });
        let d = reg.disclosures();
        assert_eq!(d.len(), 2, "user_only skill must be withheld: {d:?}");
        assert_eq!(d[0].name, "alpha");
        assert_eq!(d[1].name, "zebra");
    }
}
