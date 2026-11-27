use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::discovery::{walk, BUILT_INS};
use super::parse::parse_skill;
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

#[test]
fn required_fields_and_defaults() {
    let s = parse("---\nname: x\ndescription: d\n---\nbody").unwrap();
    assert_eq!(s.name, "x");
    assert_eq!(s.description, "d");
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

/// #920: a native skill still declaring a pre-0.8 key (`tools:`, ADR-0228;
/// `user_only`, ADR-0229; `allowed_tools`, ADR-0194) is an unknown field
/// like any typo — the load aborts naming the file, no skip-and-warn shim.
#[test]
fn a_native_pre_0_8_key_aborts_the_load() {
    for key in [
        "tools: []\n",
        "tools: ~\n",
        "user_only: true\n",
        "allowed_tools: [bash, read]\n",
    ] {
        let err = parse(&format!("---\nname: e\ndescription: d\n{key}---\nb")).unwrap_err();
        assert!(
            format!("{err:#}").contains("unknown field"),
            "{key:?}: {err:#}"
        );

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let project = root.join(".entanglement").join("skills");
        write_skill(
            &project,
            "e",
            &format!("---\nname: e\ndescription: d\n{key}---\nb"),
        );
        let err = load_registry_in(root, &no_user(root)).unwrap_err();
        assert!(format!("{err:#}").contains("SKILL.md"), "{key:?}: {err:#}");
    }
}

/// ADR-0227: a foreign (cross-vendor) skill dir's `lib.rhai` loads too.
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
    let _guard =
        tracing::subscriber::set_default(WarnCounter(count.clone(), "disable-model-invocation"));
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
