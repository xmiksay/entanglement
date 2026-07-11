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
//! 2. **user** — `${config_dir}/entanglement/skills/**/SKILL.md`.
//! 3. **project** — `<root>/.entanglement/skills/**/SKILL.md`.
//!
//! # Disclosure
//!
//! Only `name` + `description` reach the model (a compact list in the assembled
//! system prompt — see [`crate::system_prompt`]). Selection stays LLM reasoning:
//! the model matches its task against the `description` in its own forward pass.
//! There is no keyword router or embedding gate — description quality is the
//! contract. `user_only` skills are withheld from the disclosure list so the
//! model cannot self-trigger them.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::system_prompt::SkillDisclosure;

pub mod load_skill;
pub use load_skill::LoadSkillTool;

/// Embedded stock skills, parsed through the same loader as user/project files.
/// `(filename, contents)` — the filename only feeds parse-error messages; the
/// skill's identity is its frontmatter `name`.
const BUILT_INS: &[(&str, &str)] = &[("commit.md", include_str!("commit.md"))];

/// Env var overriding the user skills directory (tests + non-XDG setups).
const SKILLS_DIR_ENV: &str = "ENTANGLEMENT_SKILLS_DIR";

/// The `SKILL.md` marker file that makes a directory a skill.
const SKILL_FILE: &str = "SKILL.md";

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
    /// Tool mask active while the skill is loaded (tier-2 enforcement, deferred).
    /// `None` ⇒ inherit the session's tools.
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

/// Load the skill registry for `root`: embedded stock skills, then the user dir,
/// then the project dir — later layers replace earlier ones on a `name`
/// collision (project > user > built-in). A malformed file in any layer aborts.
pub fn load_registry(root: &Path) -> Result<SkillRegistry> {
    let mut reg = SkillRegistry::default();
    for (file, contents) in BUILT_INS {
        // Embedded built-ins are guarded by `built_ins_parse`, so a failure here
        // is a build-time bug, not a runtime condition — surface it loudly.
        let skill = parse_skill(contents, None)
            .with_context(|| format!("parsing built-in skill `{file}`"))?;
        reg.insert(skill);
    }
    if let Some(dir) = user_skills_dir() {
        load_dir(&dir, &mut reg)?;
    }
    load_dir(&root.join(".entanglement").join("skills"), &mut reg)?;
    Ok(reg)
}

/// Discover every `SKILL.md` under `dir` (recursively) and fold it into `reg`,
/// replacing any same-`name` entry. A missing dir is fine; an unreadable dir or
/// a malformed `SKILL.md` is an error. Symlinked duplicates (a link to an
/// already-seen file, or a directory cycle) are deduped by canonical path.
fn load_dir(dir: &Path, reg: &mut SkillRegistry) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let mut found: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    walk(dir, &mut found, &mut seen)?;
    // Sort for deterministic collision resolution within a single layer.
    found.sort();
    for skill_md in found {
        let contents = std::fs::read_to_string(&skill_md)
            .with_context(|| format!("reading skill file {}", skill_md.display()))?;
        // `root_dir` is the SKILL.md's directory — resolved once, here.
        let root_dir = skill_md.parent().map(Path::to_path_buf);
        let skill = parse_skill(&contents, root_dir)
            .with_context(|| format!("parsing skill {}", skill_md.display()))?;
        reg.insert(skill);
    }
    Ok(())
}

/// Recursively collect `SKILL.md` paths under `dir`. `seen` holds canonicalized
/// paths of already-visited files *and* directories: it dedupes symlinked
/// duplicate files and breaks symlink directory cycles. `std::fs::metadata`
/// follows symlinks so a linked directory is still traversed once.
fn walk(dir: &Path, found: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>) -> Result<()> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("reading skills dir {}", dir.display()))?;
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("reading an entry under {}", dir.display()))?
            .path();
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            // A broken symlink (or a racing removal) is skipped, not fatal.
            Err(_) => continue,
        };
        if meta.is_dir() {
            subdirs.push(path);
        } else if meta.is_file() && path.file_name().and_then(|n| n.to_str()) == Some(SKILL_FILE) {
            let canon = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if seen.insert(canon) {
                found.push(path);
            }
        }
    }
    subdirs.sort();
    for sub in subdirs {
        let canon = std::fs::canonicalize(&sub).unwrap_or_else(|_| sub.clone());
        // A directory already seen (a symlink pointing back into the tree) is
        // not re-entered, so a cycle cannot loop forever.
        if seen.insert(canon) {
            walk(&sub, found, seen)?;
        }
    }
    Ok(())
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

/// The user skills dir: `${config_dir}/entanglement/skills`, overridable via
/// `ENTANGLEMENT_SKILLS_DIR` (which tests point at a temp dir).
fn user_skills_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(SKILLS_DIR_ENV) {
        return Some(PathBuf::from(p));
    }
    dirs::config_dir().map(|d| d.join("entanglement").join("skills"))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

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
