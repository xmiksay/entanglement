//! Registry tests: layering, override provenance, cross-vendor leniency,
//! namespacing, built-in clashes, and expansion through the live handle.
//! Every test builds its raws from explicit temp dirs via `discover_dir`, so
//! none reads the developer's real `~/.claude`/`${config_dir}` or mutates the
//! process env.

use std::path::Path;

use super::discovery::{discover_dir, RawCommand};
use super::*;

fn write(dir: &Path, rel: &str, content: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// Read `dirs` in order (lowest precedence first) as `discover` would.
fn raws(dirs: &[(Layer, &Path, Strictness)]) -> Vec<RawCommand> {
    let mut out = Vec::new();
    for (layer, dir, strictness) in dirs {
        discover_dir(*layer, dir, *strictness, &mut out).unwrap();
    }
    out
}

#[test]
fn project_overrides_user_and_records_what_it_shadowed() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        user.path(),
        "review.md",
        "---\ndescription: user\n---\nU $1",
    );
    write(user.path(), "only-user.md", "just user");
    write(
        project.path(),
        "review.md",
        "---\ndescription: project\n---\nP $1",
    );

    let resolved = resolve(raws(&[
        (Layer::User, user.path(), Strictness::Strict),
        (Layer::Project, project.path(), Strictness::Strict),
    ]))
    .unwrap();
    let names: Vec<&str> = resolved.iter().map(|r| r.command.name.as_str()).collect();
    assert_eq!(names, vec!["only-user", "review"]);
    let review = &resolved[1];
    assert_eq!(review.command.description, "project");
    assert_eq!(review.command.layer, Layer::Project);
    assert_eq!(review.shadowed.len(), 1);
    assert_eq!(review.shadowed[0].0, Layer::User);
    assert!(review.shadowed[0].1.ends_with("review.md"));
}

#[test]
fn native_beats_foreign_within_the_project_layer() {
    let claude = tempfile::tempdir().unwrap();
    let native = tempfile::tempdir().unwrap();
    write(
        claude.path(),
        "fix.md",
        "---\nallowed-tools: Bash\n---\nclaude",
    );
    write(native.path(), "fix.md", "native");
    let reg = registry_from(
        resolve(raws(&[
            (Layer::Project, claude.path(), Strictness::Lenient),
            (Layer::Project, native.path(), Strictness::Strict),
        ]))
        .unwrap(),
    );
    assert_eq!(reg.get("fix").unwrap().body, "native");
}

#[test]
fn malformed_foreign_file_is_skipped_but_native_aborts() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "bad.md", "---\ndescription: [\n---\nx");
    write(dir.path(), "good.md", "fine");
    let lenient = resolve(raws(&[(Layer::Project, dir.path(), Strictness::Lenient)])).unwrap();
    assert_eq!(lenient.len(), 1);
    assert_eq!(lenient[0].command.name, "good");

    let err = resolve(raws(&[(Layer::Project, dir.path(), Strictness::Strict)])).unwrap_err();
    assert!(format!("{err:#}").contains("bad.md"), "{err:#}");
}

#[test]
fn subdirectories_namespace_with_colons() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "frontend/review.md", "FE review");
    write(dir.path(), "review.md", "plain review");
    let reg =
        registry_from(resolve(raws(&[(Layer::Project, dir.path(), Strictness::Strict)])).unwrap());
    assert_eq!(reg.get("frontend:review").unwrap().body, "FE review");
    assert_eq!(reg.get("review").unwrap().body, "plain review");
}

#[test]
fn builtin_names_are_flagged_and_dropped() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "help.md", "mine");
    write(dir.path(), "compact.md", "mine too");
    write(dir.path(), "ok.md", "kept");
    let resolved = resolve(raws(&[(Layer::Project, dir.path(), Strictness::Strict)])).unwrap();
    assert_eq!(resolved.iter().filter(|r| r.builtin_clash).count(), 2);

    let reg = registry_from(resolved);
    assert_eq!(reg.len(), 1);
    assert!(reg.get("help").is_none());
    assert!(
        reg.expand("/help me").is_none(),
        "a built-in is never expanded"
    );
}

#[test]
fn registry_expands_invocations_and_leaves_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "review.md",
        "---\nargument-hint: <file> [focus]\n---\nReview $1 for $2.",
    );
    let reg =
        registry_from(resolve(raws(&[(Layer::Project, dir.path(), Strictness::Strict)])).unwrap());
    let (cmd, prompt) = reg.expand("/review src/a.rs \"error paths\"").unwrap();
    assert_eq!(prompt, "Review src/a.rs for error paths.");
    assert_eq!(cmd.usage(), "/review <file> [focus]");
    assert!(reg.expand("/unknown x").is_none());
    assert!(reg.expand("review x").is_none());
}

#[test]
fn expand_or_literal_reads_the_live_handle() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "hi.md", "Hello $ARGUMENTS!");
    let live: LiveCommands = Arc::new(RwLock::new(Arc::new(CommandRegistry::default())));
    assert_eq!(expand_or_literal(&live, "/hi bob".into()), "/hi bob");

    // A reload swaps the registry behind the same handle.
    let fresh =
        registry_from(resolve(raws(&[(Layer::Project, dir.path(), Strictness::Strict)])).unwrap());
    *live.write().unwrap() = Arc::new(fresh);
    assert_eq!(expand_or_literal(&live, "/hi bob".into()), "Hello bob!");
    assert_eq!(expand_or_literal(&live, "plain".into()), "plain");
}

/// The real `discover` path under an injected user layer: proves the user +
/// project dirs are wired through `layers::load_layers`. The user layer is a
/// temp dir, so the real user dirs are never read.
#[test]
fn load_registry_reads_override_and_project_dirs() {
    let root = tempfile::tempdir().unwrap();
    let user = root.path().join("user-commands");
    write(&user, "a.md", "user a");
    write(
        root.path(),
        ".opencode/command/b.md",
        "---\nagent: x\nsubtask: true\n---\noc b",
    );
    write(root.path(), ".entanglement/commands/a.md", "project a");

    let reg = registry_from(resolve_registry_in(root.path(), &UserLayer::Dir(user)).unwrap());

    assert_eq!(reg.get("a").unwrap().body, "project a");
    assert_eq!(reg.get("b").unwrap().body, "oc b");
}
