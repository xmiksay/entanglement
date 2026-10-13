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
//! contract. Every skill is disclosed — there is no model-hidden skill
//! (ADR-0229 removed `user_only`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::layers::UserLayer;
use crate::system_prompt::SkillDisclosure;

mod discovery;
// `lib.rhai` doc comments for the `load_skill` listing (ADR-0228).
#[cfg_attr(not(feature = "rhai"), allow(dead_code))]
mod lib_docs;
pub mod lib_rhai;
pub mod load_skill;
// `SKILL.md` frontmatter parsing, strict and lenient (ADR-0074/0228).
mod parse;

pub use discovery::skill_dirs;
pub use load_skill::LoadSkillTool;

use parse::parse_raw;

/// One discovered skill: the tier-1 metadata plus the loaded body. `root_dir` is
/// resolved **once** here at discovery (the directory holding `SKILL.md` and its
/// `references/`/`scripts/` payload) and never re-derived downstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMeta {
    /// Unique id; what an invocation / tier-2 load references.
    pub name: String,
    /// One-line summary; the only field (with `name`) disclosed to the model.
    pub description: String,
    /// Parsed but **ignored** (ADR-0194 retired the ADR-0106 enforcement
    /// this once drove): skills are additive-only, never a tool-set
    /// restriction. Kept in the struct only because `SkillFrontmatter` is
    /// `deny_unknown_fields` — dropping the key would break every existing
    /// skill file that still carries it. A skill parsed with a non-`None`
    /// value gets a one-time load warning (see `parse::parse_skill`) pointing the
    /// author at deleting it. Still populated onto the vestigial
    /// `OutEvent::SkillActive.allowed_tools` wire field for log-replay
    /// compatibility.
    pub allowed_tools: Option<Vec<String>>,
    /// The skill directory (holds `SKILL.md` + payload). `None` for embedded
    /// built-ins, which have no on-disk home.
    pub root_dir: Option<PathBuf>,
    /// The markdown body below the frontmatter (tier-2 content).
    pub body: String,
    /// The compiled `lib.rhai` beside `SKILL.md` (ADR-0227), registered in
    /// every script as `skill::<skill>`; `None` when absent or refused.
    pub lib: Option<lib_rhai::SkillLib>,
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
    #[cfg(feature = "rhai")]
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

    #[cfg(not(feature = "rhai"))]
    pub fn lib_collision(&self, _name: &str) -> Option<String> {
        None
    }

    /// Tier-1 disclosure lines for the system prompt: `name` + `description`
    /// only, name-sorted — every skill (ADR-0229).
    pub fn disclosures(&self) -> Vec<SkillDisclosure> {
        self.iter()
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

/// Load the skill registry for `root`: embedded stock skills, then the user
/// dirs, then the project dirs — later layers replace earlier ones on a `name`
/// collision (project > user > built-in). A malformed file in a native layer
/// aborts; a malformed foreign file is warned and skipped.
pub fn load_registry(root: &Path) -> Result<SkillRegistry> {
    load_registry_in(root, &UserLayer::FromEnv)
}

/// [`load_registry`] with an explicit user layer (#773).
pub fn load_registry_in(root: &Path, user: &UserLayer) -> Result<SkillRegistry> {
    let mut reg = SkillRegistry::default();
    // Track the winning (layer, source) per name so a later-wins collision is no
    // longer silent (#186): emit a `replaces=<prior source>` debug at the
    // overwrite, matching the provenance `inspect skills` surfaces.
    let mut winning: HashMap<String, (SkillLayer, String)> = HashMap::new();
    let mut user_only = Vec::new();
    for raw in discovery::discover(root, user)? {
        let Some((skill, declares_user_only)) = parse_raw(&raw)? else {
            continue;
        };
        if declares_user_only {
            user_only.push((skill.name.clone(), raw.source.clone()));
        }
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
    parse::warn_user_only(format!("skills\0{}\0{user:?}", root.display()), &user_only);
    Ok(reg)
}

/// Native skills refused at load — the retired `tools:` key (ADR-0228) —
/// as `source: reason` lines, so `skutter inspect skills` shows what the log
/// only warned about.
pub fn rejected_skills(root: &Path) -> Result<Vec<String>> {
    rejected_skills_in(root, &UserLayer::FromEnv)
}

/// [`rejected_skills`] with an explicit user layer.
pub fn rejected_skills_in(root: &Path, user: &UserLayer) -> Result<Vec<String>> {
    Ok(discovery::discover(root, user)?
        .iter()
        .filter_map(parse::rejection)
        .collect())
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
/// layer precedence as [`load_registry`] but keeping *which* layer won and what
/// it shadowed. Sorted by name for a stable table. Malformed files behave
/// exactly as at load (native aborts, foreign warns and skips) — mirrors
/// [`crate::agents::resolve_registry`].
pub fn resolve_registry(root: &Path) -> Result<Vec<SkillResolution>> {
    resolve_registry_in(root, &UserLayer::FromEnv)
}

/// [`resolve_registry`] with an explicit user layer (#773).
pub fn resolve_registry_in(root: &Path, user: &UserLayer) -> Result<Vec<SkillResolution>> {
    let mut order: Vec<String> = Vec::new();
    let mut by_name: HashMap<String, Vec<(SkillLayer, String, SkillMeta)>> = HashMap::new();
    for raw in discovery::discover(root, user)? {
        let Some((meta, _)) = parse_raw(&raw)? else {
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
mod tests {
    use std::collections::HashSet;

    use super::discovery::{walk, BUILT_INS, SKILL_FILE};
    use super::parse::{parse_skill, RetiredToolsKey};
    use super::*;

    /// An empty user layer: the injected stand-in for the real user dirs.
    fn no_user(root: &Path) -> UserLayer {
        UserLayer::Dir(root.join("no-such-user-dir"))
    }

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
        let rhai = reg.get("rhai").expect("rhai built-in");
        assert_eq!(rhai.root_dir, None);
        assert!(rhai.body.contains("Rhai"));
    }

    /// A minimal [`tracing::Subscriber`] that counts WARN-level events whose
    /// message contains the needle (`""` counts every warning) — enough to
    /// assert a one-time warning fires exactly once, without pulling in a
    /// test-only tracing crate or tripping over unrelated load warnings.
    struct WarnCounter(std::sync::Arc<std::sync::atomic::AtomicUsize>, &'static str);

    struct Message(String);

    impl tracing::field::Visit for Message {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = format!("{value:?}");
            }
        }
    }

    impl tracing::Subscriber for WarnCounter {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            if *event.metadata().level() != tracing::Level::WARN {
                return;
            }
            let mut msg = Message(String::new());
            event.record(&mut msg);
            if msg.0.contains(self.1) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// ADR-0194: `allowed_tools` still parses (wire/log-replay compat), but a
    /// skill carrying it gets exactly one load warning naming the field as
    /// no-longer-enforced; a skill without it never warns.
    #[test]
    fn allowed_tools_frontmatter_is_parsed_but_ignored_with_one_time_warning() {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let _guard = tracing::subscriber::set_default(WarnCounter(count.clone(), ""));

        let s =
            parse("---\nname: x\ndescription: d\nallowed_tools: [bash, read]\n---\nbody").unwrap();
        assert_eq!(s.allowed_tools, Some(vec!["bash".into(), "read".into()]));
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one warning for a skill carrying allowed_tools"
        );

        let s2 = parse("---\nname: y\ndescription: d\n---\nbody").unwrap();
        assert_eq!(s2.allowed_tools, None);
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no additional warning for a skill without allowed_tools"
        );
    }

    #[test]
    fn required_fields_and_defaults() {
        let s = parse("---\nname: x\ndescription: d\n---\nbody").unwrap();
        assert_eq!(s.name, "x");
        assert_eq!(s.description, "d");
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
        // Point the user layer at a non-existent dir so a real ~/.config skill
        // can't leak into the assertion.
        let reg = load_registry_in(root, &no_user(root)).unwrap();

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
        let reg = load_registry_in(root, &UserLayer::Dir(user.clone())).unwrap();

        assert_eq!(reg.get("dup").unwrap().description, "from project");
    }

    #[test]
    fn user_skill_overrides_built_in() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let user = root.join("user-skills");
        write_skill(
            &user,
            "rhai",
            "---\nname: rhai\ndescription: my override\n---\nx",
        );
        let reg = load_registry_in(root, &UserLayer::Dir(user.clone())).unwrap();

        assert_eq!(reg.get("rhai").unwrap().description, "my override");
    }

    #[test]
    fn resolve_registry_surfaces_layer_winner_and_shadowed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A user override of the built-in `rhai` skill: project wins nothing
        // here, so `user` is the winner and the built-in is shadowed.
        let user = root.join("user-skills");
        write_skill(
            &user,
            "rhai",
            "---\nname: rhai\ndescription: user rhai\n---\nx",
        );
        let resolved = resolve_registry_in(root, &UserLayer::Dir(user.clone())).unwrap();

        let rhai = resolved
            .iter()
            .find(|r| r.meta.name == "rhai")
            .expect("rhai resolved");
        assert_eq!(rhai.layer, SkillLayer::User);
        assert_eq!(rhai.meta.description, "user rhai");
        // The built-in it overrode is recorded, not swallowed.
        assert_eq!(rhai.shadowed.len(), 1, "built-in should be shadowed");
        assert_eq!(rhai.shadowed[0].0, SkillLayer::BuiltIn);
    }

    #[test]
    fn resolve_registry_is_name_sorted_and_records_no_shadow_when_unique() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let project = root.join(".entanglement").join("skills");
        write_skill(&project, "zzz", "---\nname: zzz\ndescription: last\n---\nb");
        let resolved = resolve_registry_in(root, &no_user(root)).unwrap();

        // Name-sorted: the built-in `rhai` sorts before the project `zzz`.
        let names: Vec<&str> = resolved.iter().map(|r| r.meta.name.as_str()).collect();
        assert_eq!(names, vec!["rhai", "zzz"]);
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
        let err = load_registry_in(root, &no_user(root)).unwrap_err();
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
        let reg = load_registry_in(root, &no_user(root)).unwrap();

        let s = reg.get("arch").expect("foreign skill discovered");
        assert_eq!(s.description, "from claude");
        // Claude's `allowed-tools` is deliberately dropped (ADR-0074).
        assert_eq!(s.allowed_tools, None);
    }

    #[test]
    fn foreign_layer_tools_key_is_dropped_like_claude_allowed_tools() {
        // A `tools:` key in a foreign (cross-vendor) SKILL.md is just another
        // key entanglement doesn't own — dropped like Claude's `allowed-tools`.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_skill(
            &root.join(".claude").join("skills"),
            "with-tools",
            "---\nname: with-tools\ndescription: d\ntools:\n  - kind: alias\n    name: x\n    target: read\n---\nbody",
        );
        let reg = load_registry_in(root, &no_user(root)).unwrap();
        assert!(reg.get("with-tools").is_some(), "foreign skill still loads");
    }

    /// ADR-0228: a native skill's `tools:` key is refused with the
    /// replacement named — the skill is skipped, every other skill (in the
    /// user and the project layer) still loads.
    #[test]
    fn a_native_tools_key_skips_the_skill_and_names_the_replacement() {
        let md = |name: &str| {
            format!(
                "---\nname: {name}\ndescription: d\ntools:\n  - kind: rhai\n    name: w\n    \
                 script: x.rhai\n---\nbody"
            )
        };
        for empty in ["tools:\n", "tools: ~\n", "tools: []\n"] {
            let e = parse(&format!("---\nname: e\ndescription: d\n{empty}---\nb")).unwrap_err();
            assert!(e.is::<RetiredToolsKey>(), "{empty:?}: {e:#}");
        }
        let err = parse(&md("t")).unwrap_err();
        let text = format!("{err:#}");
        assert!(err.is::<RetiredToolsKey>(), "{text}");
        for hint in ["lib.rhai", "skill::<skill>::<fn>", "\"path\"", "args"] {
            assert!(text.contains(hint), "missing `{hint}`: {text}");
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let user = root.join("user-skills");
        write_skill(&user, "u", &md("u"));
        let project = root.join(".entanglement").join("skills");
        write_skill(&project, "p", &md("p"));
        write_skill(&project, "ok", "---\nname: ok\ndescription: fine\n---\nb");
        let reg = load_registry_in(root, &UserLayer::Dir(user.clone())).unwrap();
        assert!(reg.get("u").is_none() && reg.get("p").is_none());
        assert_eq!(reg.get("ok").unwrap().description, "fine");
        let resolved = resolve_registry_in(root, &UserLayer::Dir(user.clone())).unwrap();
        assert!(resolved.iter().any(|r| r.meta.name == "ok"));
        let rejected = rejected_skills_in(root, &UserLayer::Dir(user)).unwrap();
        assert_eq!(rejected.len(), 2, "{rejected:?}");
        assert!(
            rejected.iter().all(|r| r.contains("SKILL.md: skill `")),
            "{rejected:?}"
        );
    }

    /// ADR-0227: a foreign (cross-vendor) skill dir's `lib.rhai` loads too.
    #[cfg(feature = "rhai")]
    #[test]
    fn a_foreign_skill_dir_lib_rhai_is_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let skill_dir = write_skill(
            &root.join(".claude").join("skills"),
            "foreign",
            "---\nname: foreign\ndescription: d\n---\nbody",
        );
        std::fs::write(skill_dir.join(lib_rhai::LIB_FILE), "fn hi() { 1 }").unwrap();
        let reg = load_registry_in(root, &no_user(root)).unwrap();
        let lib = reg
            .get("foreign")
            .unwrap()
            .lib
            .as_ref()
            .expect("lib loaded");
        assert_eq!(lib.functions[0].signature, "skill::foreign::hi()");
    }

    /// ADR-0229: a foreign `disable-model-invocation` is an ignored unknown
    /// key — the skill loads and is disclosed like any other, with no warning
    /// (foreign files are not entanglement's to correct).
    #[test]
    fn foreign_disable_model_invocation_is_ignored() {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let _guard = tracing::subscriber::set_default(WarnCounter(
            count.clone(),
            "disable-model-invocation",
        ));
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_skill(
            &root.join(".claude").join("skills"),
            "deploy",
            "---\nname: deploy\ndescription: d\ndisable-model-invocation: true\n---\nb",
        );
        let reg = load_registry_in(root, &no_user(root)).unwrap();

        assert!(reg.get("deploy").is_some());
        assert!(reg.disclosures().iter().any(|d| d.name == "deploy"));
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// ADR-0229: a native skill still declaring `user_only` loads, is
    /// disclosed, and warns once — a reload re-parsing the same file stays
    /// quiet, but a key removed and later restored warns again.
    #[test]
    fn native_user_only_loads_disclosed_and_warns_once_per_sighting() {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let _guard = tracing::subscriber::set_default(WarnCounter(count.clone(), "user_only"));
        let warned = || count.load(std::sync::atomic::Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let skills = root.join(".entanglement").join("skills");
        let with_key = "---\nname: ship\ndescription: d\nuser_only: true\n---\nb";
        write_skill(&skills, "ship", with_key);
        let reg = load_registry_in(root, &no_user(root)).unwrap();
        assert!(reg.disclosures().iter().any(|d| d.name == "ship"));
        assert_eq!(warned(), 1);

        load_registry_in(root, &no_user(root)).unwrap();
        assert_eq!(warned(), 1, "a reload must not repeat the removal warning");

        write_skill(&skills, "ship", "---\nname: ship\ndescription: d\n---\nb");
        load_registry_in(root, &no_user(root)).unwrap();
        assert_eq!(warned(), 1, "a clean load never warns");

        write_skill(&skills, "ship", with_key);
        load_registry_in(root, &no_user(root)).unwrap();
        assert_eq!(warned(), 2, "a reintroduced key warns again");
    }

    #[test]
    fn malformed_foreign_skill_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let claude = root.join(".claude").join("skills");
        write_skill(&claude, "bad", "no frontmatter at all");
        write_skill(&claude, "good", "---\nname: good\ndescription: ok\n---\nb");
        let reg = load_registry_in(root, &no_user(root)).unwrap();

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
        let reg = load_registry_in(root, &no_user(root)).unwrap();
        // Native `.entanglement` beats both foreign project dirs.
        write_skill(
            &root.join(".entanglement").join("skills"),
            "dup",
            "---\nname: dup\ndescription: from native\n---\nn",
        );
        let reg2 = load_registry_in(root, &no_user(root)).unwrap();

        assert_eq!(reg.get("only-agents").unwrap().description, "cross-vendor");
        // `.agents` (cross-vendor) beats `.claude` (vendor-specific)…
        assert_eq!(reg.get("dup").unwrap().description, "from .agents");
        // …and `.entanglement` (native) beats both.
        assert_eq!(reg2.get("dup").unwrap().description, "from native");
        let resolved = resolve_registry_in(root, &no_user(root)).unwrap();
        let dup = resolved.iter().find(|r| r.meta.name == "dup").unwrap();
        assert_eq!(dup.layer, SkillLayer::Project);
        assert_eq!(dup.shadowed.len(), 2, "both foreign defs recorded");
    }

    #[test]
    fn disclosures_are_name_sorted() {
        let mut reg = SkillRegistry::default();
        for name in ["zebra", "alpha"] {
            reg.insert(SkillMeta {
                name: name.into(),
                description: "d".into(),
                allowed_tools: None,
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
