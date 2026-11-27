//! Where `config.yml` `rhai.trusted` folders and the skill layer dirs
//! (ADR-0228) meet the file system.
//!
//! A script inside a trusted folder runs force-allowed with no card, and a
//! skill's `lib.rhai`/`SKILL.md` is code and instructions every later
//! session loads — so writing one is as good as running it.
//! [`floored_write`] lets the dispatch grade floor a `write`/`edit`/
//! `apply_patch` of such a file at `Ask`: the agent cannot author trusted
//! code unseen. `bash` and MCP filesystem servers are not covered (neither
//! names its target in a form that can be checked reliably), which is why a
//! trusted folder must not be agent-writable.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

/// The path-taking file-mutation host tools.
const MUTATING_TOOLS: &[&str] = &["write", "edit", "apply_patch"];

/// The files that make up a skill's behavior: its instructions and its
/// script library.
const SKILL_FILES: &[&str] = &["SKILL.md", "lib.rhai"];

/// Symlink hops followed before giving up (the kernel's own `MAXSYMLINKS`).
const MAX_HOPS: usize = 40;

/// `config.yml` `rhai.trusted` folders by the layer that listed them,
/// canonical. A folder both layers list counts as the project's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrustedFolders {
    /// Always honored: the project layer is trusted code (ADR-0047).
    pub project: Vec<PathBuf>,
    pub user: Vec<PathBuf>,
    /// Modes the project config gives `prompt` rules. The user's trust is
    /// dropped there for the whole script: the repository asked to be
    /// consulted in that mode, and a user folder must not wave its rules
    /// through unseen.
    pub user_suppressed: BTreeSet<String>,
    /// The project's `permissions:` ceiling asks for prompts (`prompt`
    /// rules or `default: prompt`): the same, in every mode.
    pub user_suppressed_everywhere: bool,
}

/// An embedder's (or a test's) plain list: trusted unconditionally, like
/// the project layer.
impl From<Vec<PathBuf>> for TrustedFolders {
    fn from(project: Vec<PathBuf>) -> Self {
        TrustedFolders {
            project,
            ..Self::default()
        }
    }
}

impl TrustedFolders {
    /// Every trusted folder, both layers — what the write floor protects.
    pub fn all(&self) -> impl Iterator<Item = &PathBuf> {
        self.project.iter().chain(&self.user)
    }

    /// Whether a script at canonical `canon` runs trusted in `mode`.
    pub fn runs_trusted(&self, canon: &Path, mode: &str) -> bool {
        is_script(canon)
            && (contains(&self.project, canon)
                || (!self.user_suppressed_in(mode) && contains(&self.user, canon)))
    }

    /// Whether the user layer's trust is dropped in `mode`.
    pub fn user_suppressed_in(&self, mode: &str) -> bool {
        self.user_suppressed_everywhere || self.user_suppressed.contains(mode)
    }
}

/// What the write floor and the launch's trust check read, built once per
/// executor.
#[derive(Debug, Default)]
pub(crate) struct Trust {
    pub folders: TrustedFolders,
    /// The user and project skill layer dirs, canonical — a missing one by
    /// its nearest existing ancestor, so a skill created later is covered.
    pub skill_dirs: Vec<PathBuf>,
}

impl Trust {
    pub(crate) fn new(folders: TrustedFolders, skill_dirs: &[PathBuf]) -> Self {
        let skill_dirs = skill_dirs
            .iter()
            .filter_map(|d| canonical_target(&lexical(d)))
            .collect();
        Trust {
            folders,
            skill_dirs,
        }
    }
}

/// Whether `path` names a Rhai script file: a `.rhai` extension, any case.
/// Trust is a property of scripts only — the `rhai` launch refuses any
/// other `path` (judged on its canonical target), and only such a file's
/// write is floored — so a `.txt` in a trusted folder can never run trusted.
pub(crate) fn is_script(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("rhai"))
}

fn is_skill_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| SKILL_FILES.iter().any(|f| f.eq_ignore_ascii_case(n)))
}

/// Whether the canonical `path` lies in one of the canonical `dirs`.
/// Canonical on both sides, so a symlink inside a trusted folder that points
/// out of it is judged by its target.
pub(crate) fn contains<'a>(dirs: impl IntoIterator<Item = &'a PathBuf>, canon: &Path) -> bool {
    dirs.into_iter().any(|dir| canon.starts_with(dir))
}

/// Whether `(tool, input)` mutates a `*.rhai` file inside a trusted folder,
/// or a `SKILL.md`/`lib.rhai` inside a skill layer dir. A relative `path`
/// resolves against `root`. A target that cannot be resolved counts as
/// inside (fail closed) — it only ever costs a prompt.
pub(crate) fn floored_write(tool: &str, input: &str, root: &Path, trust: &Trust) -> bool {
    if !MUTATING_TOOLS.contains(&tool) {
        return false;
    }
    let Some(path) = serde_json::from_str::<serde_json::Value>(input)
        .ok()
        .and_then(|v| v.get("path")?.as_str().map(PathBuf::from))
    else {
        return false;
    };
    let abs = lexical(&root.join(path));
    let (target, resolved) = match canonical_target(&abs) {
        Some(canon) => (canon, true),
        None => (abs, false),
    };
    let inside = |dirs: Vec<&PathBuf>| !dirs.is_empty() && (!resolved || contains(dirs, &target));
    (is_script(&target) && inside(trust.folders.all().collect()))
        || (is_skill_file(&target) && inside(trust.skill_dirs.iter().collect()))
}

/// `path` with `.`/`..` folded lexically (it is absolute).
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// The canonical file a write to `path` lands on, whether or not it exists:
/// an existing file (or a symlink chain, dangling included) resolves to its
/// target; a missing one to its nearest existing ancestor, canonicalized,
/// plus the missing tail. `None` when it cannot be resolved.
fn canonical_target(path: &Path) -> Option<PathBuf> {
    let mut path = path.to_path_buf();
    for _ in 0..MAX_HOPS {
        if let Ok(canon) = path.canonicalize() {
            return Some(canon);
        }
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = std::fs::read_link(&path).ok()?;
                path = lexical(&path.parent()?.join(target));
            }
            Ok(_) => return None,
            Err(_) => return canonical_ancestor(&path),
        }
    }
    None
}

/// The nearest existing ancestor of `path`, canonicalized, joined with the
/// components below it.
fn canonical_ancestor(path: &Path) -> Option<PathBuf> {
    let mut tail = Vec::new();
    let mut cur = path;
    loop {
        tail.push(cur.file_name()?);
        cur = cur.parent()?;
        if let Ok(canon) = cur.canonicalize() {
            return Some(tail.iter().rev().fold(canon, |acc, c| acc.join(c)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fx {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        trust: Trust,
    }

    /// `scripts/` trusted; `.entanglement/skills` an existing skill dir and
    /// `.agents/skills` a not-yet-created one.
    fn fx() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::create_dir_all(root.join("other")).unwrap();
        std::fs::create_dir_all(root.join(".entanglement/skills/git")).unwrap();
        std::fs::write(root.join("scripts/ok.rhai"), "1").unwrap();
        let trust = Trust::new(
            vec![root.join("scripts")].into(),
            &[
                root.join(".entanglement/skills"),
                root.join(".agents/skills"),
            ],
        );
        Fx {
            _tmp: tmp,
            root,
            trust,
        }
    }

    fn writes(f: &Fx, tool: &str, path: &str) -> bool {
        let input = serde_json::json!({ "path": path, "content": "x" }).to_string();
        floored_write(tool, &input, &f.root, &f.trust)
    }

    #[test]
    fn a_rhai_file_inside_a_trusted_folder_is_caught_for_every_mutating_tool() {
        let f = fx();
        for tool in ["write", "edit", "apply_patch"] {
            assert!(writes(&f, tool, "scripts/ok.rhai"), "{tool} existing");
            assert!(writes(&f, tool, "scripts/new.RHAI"), "{tool} new");
            assert!(writes(&f, tool, "scripts/deep/er/new.rhai"), "{tool} deep");
            let abs = f.root.join("scripts/./x/../ok.rhai");
            assert!(writes(&f, tool, abs.to_str().unwrap()), "{tool} absolute");
        }
        assert!(!writes(&f, "read", "scripts/ok.rhai"), "read is no write");
    }

    #[test]
    fn other_files_and_other_folders_are_not() {
        let f = fx();
        assert!(!writes(&f, "write", "scripts/notes.md"));
        assert!(!writes(&f, "write", "other/x.rhai"));
        assert!(!writes(&f, "write", "scripts/../other/x.rhai"));
        let none = Fx {
            trust: Trust::default(),
            ..fx()
        };
        assert!(!writes(&none, "write", "scripts/ok.rhai"));
        assert!(!writes(&none, "write", ".entanglement/skills/git/SKILL.md"));
    }

    #[test]
    fn skill_files_in_any_skill_dir_are_caught() {
        let f = fx();
        for path in [
            ".entanglement/skills/git/SKILL.md",
            ".entanglement/skills/git/lib.rhai",
            ".entanglement/skills/new/deep/SKILL.md",
            ".agents/skills/x/lib.rhai",
            ".agents/skills/x/skill.md",
        ] {
            assert!(writes(&f, "edit", path), "{path}");
        }
        for path in [
            ".entanglement/skills/git/notes.md",
            ".entanglement/skills/git/run.rhai",
            "docs/SKILL.md",
            "lib.rhai",
        ] {
            assert!(!writes(&f, "write", path), "{path}");
        }
    }

    #[test]
    fn user_trust_is_dropped_in_a_mode_the_project_prompts_in() {
        let f = fx();
        let script = f.root.join("scripts/ok.rhai");
        let user = TrustedFolders {
            user: vec![f.root.join("scripts")],
            user_suppressed: ["build".to_string()].into(),
            ..TrustedFolders::default()
        };
        assert!(user.runs_trusted(&script, "auto"));
        assert!(!user.runs_trusted(&script, "build"));
        let everywhere = TrustedFolders {
            user: vec![f.root.join("scripts")],
            user_suppressed_everywhere: true,
            ..TrustedFolders::default()
        };
        assert!(!everywhere.runs_trusted(&script, "auto"));
        let project = TrustedFolders::from(vec![f.root.join("scripts")]);
        assert!(
            project.runs_trusted(&script, "build"),
            "project trust stays"
        );
        assert!(!project.runs_trusted(&f.root.join("scripts/x.txt"), "build"));
    }

    #[test]
    fn is_script_is_the_rhai_extension_in_any_case() {
        for yes in ["a.rhai", "dir/b.RHAI", "/x/c.Rhai"] {
            assert!(is_script(Path::new(yes)), "{yes}");
        }
        for no in ["a.txt", "rhai", "a.rhai.txt", "dir.rhai/x", ".rhai"] {
            assert!(!is_script(Path::new(no)), "{no}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_judged_by_where_the_write_lands() {
        let f = fx();
        let link = |target: &Path, name: &str| {
            std::os::unix::fs::symlink(target, f.root.join(name)).unwrap();
        };
        // Outside → into the trusted folder (existing and dangling targets).
        link(&f.root.join("scripts/ok.rhai"), "other/in.rhai");
        link(&f.root.join("scripts/later.rhai"), "other/dangling.rhai");
        link(&f.root.join("scripts"), "other/dir");
        assert!(writes(&f, "write", "other/in.rhai"));
        assert!(writes(&f, "write", "other/dangling.rhai"));
        assert!(writes(&f, "write", "other/dir/new.rhai"));
        // Inside → out of it.
        link(&f.root.join("other/out.rhai"), "scripts/out.rhai");
        std::fs::write(f.root.join("other/out.rhai"), "1").unwrap();
        assert!(!writes(&f, "write", "scripts/out.rhai"));
    }

    #[test]
    fn contains_compares_canonical_prefixes() {
        let f = fx();
        let trusted = &f.trust.folders.project;
        assert!(contains(trusted, &f.root.join("scripts/ok.rhai")));
        assert!(!contains(trusted, &f.root.join("scripts-x/ok.rhai")));
        assert!(!contains(&[], &f.root.join("scripts/ok.rhai")));
    }
}
