//! The status line's git facts, read straight off the filesystem (#762).
//!
//! No `git` subprocess and no tree walk: an ancestor search for `.git`, then
//! one or two tiny file reads. `skutter` often runs from a huge workspace
//! root, so anything that scales with the tree (or forks per refresh) is out.
//!
//! - `.git` **directory** → a plain repo rooted at that ancestor.
//! - `.git` **file** with `gitdir: <path>` → a linked worktree when that
//!   gitdir holds a `commondir` file (the repo name then comes from the main
//!   repo, the parent of the common dir); a submodule otherwise (no
//!   `commondir`), named after its own checkout.
//! - `HEAD`: `ref: refs/heads/<b>` → `<b>`; a bare object id → its 7-char
//!   short sha; anything else — including reftable's `refs/heads/.invalid`
//!   stub, which only the reftable backend can resolve — → `?`.
//!
//! No dirty flag: that needs an index/tree comparison, i.e. a walk.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// What the status line knows about where the TUI works.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct GitInfo {
    /// Repo name — or, outside git, the `~`-abbreviated working path.
    pub repo: String,
    /// The `~`-abbreviated absolute working path.
    pub cwd: String,
    /// The working path below the repo root (`""` at the root or outside git).
    pub cwd_rel: String,
    /// Branch, short sha, or `?`; `""` outside git.
    pub branch: String,
    /// The linked worktree's name, when the cwd is one.
    pub worktree: Option<String>,
    /// The `HEAD` file the 2 s poll stats for an mtime change.
    pub head: Option<PathBuf>,
    /// The checkout's top-level directory (the status command's `root`).
    pub toplevel: Option<PathBuf>,
}

/// Probe `cwd`. `home` abbreviates paths to `~` (injected so tests never
/// depend on the real home directory).
pub(crate) fn probe(cwd: &Path, home: Option<&Path>) -> GitInfo {
    let cwd_display = tilde(cwd, home);
    let Some(repo) = find_repo(cwd) else {
        return GitInfo {
            repo: cwd_display.clone(),
            cwd: cwd_display,
            ..GitInfo::default()
        };
    };
    let cwd_rel = cwd
        .strip_prefix(&repo.toplevel)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    GitInfo {
        repo: repo.name,
        cwd: cwd_display,
        cwd_rel,
        branch: repo
            .gitdir
            .as_deref()
            .map_or_else(|| "?".to_string(), read_branch),
        worktree: repo.worktree,
        head: repo.gitdir.map(|g| g.join("HEAD")),
        toplevel: Some(repo.toplevel),
    }
}

/// The `HEAD` file's mtime — the cheap "did anything move?" check between
/// full probes.
pub(crate) fn head_stamp(info: &GitInfo) -> Option<SystemTime> {
    let head = info.head.as_deref()?;
    std::fs::metadata(head).and_then(|m| m.modified()).ok()
}

struct Repo {
    toplevel: PathBuf,
    name: String,
    /// `None` when a `.git` file is unreadable or malformed.
    gitdir: Option<PathBuf>,
    worktree: Option<String>,
}

fn find_repo(cwd: &Path) -> Option<Repo> {
    cwd.ancestors().find_map(|dir| {
        let dotgit = dir.join(".git");
        let meta = std::fs::metadata(&dotgit).ok()?;
        let own_name = basename(dir);
        if meta.is_dir() {
            return Some(Repo {
                toplevel: dir.to_path_buf(),
                name: own_name,
                gitdir: Some(dotgit),
                worktree: None,
            });
        }
        let gitdir = read_small(&dotgit).and_then(|s| {
            let target = s
                .lines()
                .next()?
                .strip_prefix("gitdir:")?
                .trim()
                .to_string();
            (!target.is_empty()).then(|| dir.join(target))
        });
        let common = gitdir.as_deref().and_then(|g| {
            let rel = read_small(&g.join("commondir"))?;
            let common = g.join(rel.trim());
            Some(std::fs::canonicalize(&common).unwrap_or(common))
        });
        let (name, worktree) = match (&gitdir, common) {
            (Some(g), Some(common)) => (main_repo_name(&common), Some(basename(g))),
            _ => (own_name, None),
        };
        Some(Repo {
            toplevel: dir.to_path_buf(),
            name,
            gitdir,
            worktree,
        })
    })
}

/// Every file the probe reads is a few bytes. Only a regular file is opened
/// (opening a FIFO named `HEAD` blocks forever, wedging the blocking-pool
/// thread and the TUI's exit), and at most [`READ_CAP`] bytes are read.
const READ_CAP: u64 = 4096;

fn read_small(path: &Path) -> Option<String> {
    use std::io::Read;
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut out = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(READ_CAP)
        .read_to_string(&mut out)
        .ok()?;
    Some(out)
}

/// The main repo's name from its common dir: the parent of a `.git`, or a
/// bare repo's own name minus a `.git` suffix.
fn main_repo_name(common: &Path) -> String {
    if common.file_name().is_some_and(|n| n == ".git") {
        if let Some(parent) = common.parent() {
            return basename(parent);
        }
    }
    let name = basename(common);
    name.strip_suffix(".git").unwrap_or(&name).to_string()
}

fn read_branch(gitdir: &Path) -> String {
    let Some(head) = read_small(&gitdir.join("HEAD")) else {
        return "?".to_string();
    };
    let head = head.trim();
    if let Some(target) = head.strip_prefix("ref:") {
        return match target.trim().strip_prefix("refs/heads/") {
            Some(branch) if !branch.is_empty() && branch != ".invalid" => branch.to_string(),
            _ => "?".to_string(),
        };
    }
    let is_oid = matches!(head.len(), 40 | 64) && head.chars().all(|c| c.is_ascii_hexdigit());
    if is_oid {
        head[..7].to_string()
    } else {
        "?".to_string()
    }
}

fn basename(p: &Path) -> String {
    p.file_name().map_or_else(
        || p.to_string_lossy().into_owned(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn tilde(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.to_string_lossy()),
        None => path.to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn repo_dir(root: &Path, name: &str, head: &str) -> PathBuf {
        let top = root.join(name);
        fs::create_dir_all(top.join(".git")).unwrap();
        fs::write(top.join(".git/HEAD"), head).unwrap();
        top
    }

    #[test]
    fn plain_repo_at_its_root() {
        let t = tempfile::tempdir().unwrap();
        let top = repo_dir(t.path(), "proj", "ref: refs/heads/main\n");
        let info = probe(&top, None);
        assert_eq!(info.repo, "proj");
        assert_eq!(info.cwd_rel, "");
        assert_eq!(info.branch, "main");
        assert_eq!(info.worktree, None);
        assert_eq!(info.head, Some(top.join(".git/HEAD")));
        assert_eq!(info.toplevel, Some(top.clone()));
        assert!(head_stamp(&info).is_some());
    }

    #[test]
    fn subdirectory_reports_the_relative_path() {
        let t = tempfile::tempdir().unwrap();
        let top = repo_dir(t.path(), "proj", "ref: refs/heads/feat/x\n");
        let sub = top.join("src/deep");
        fs::create_dir_all(&sub).unwrap();
        let info = probe(&sub, None);
        assert_eq!(
            (info.repo.as_str(), info.cwd_rel.as_str()),
            ("proj", "src/deep")
        );
        assert_eq!(info.branch, "feat/x");
    }

    #[test]
    fn detached_head_shows_a_short_sha_and_odd_heads_show_a_question_mark() {
        let t = tempfile::tempdir().unwrap();
        let top = repo_dir(t.path(), "p", "0123456789abcdef0123456789abcdef01234567\n");
        assert_eq!(probe(&top, None).branch, "0123456");
        // Reftable leaves this stub in HEAD; the real ref lives in tables.
        fs::write(top.join(".git/HEAD"), "ref: refs/heads/.invalid\n").unwrap();
        assert_eq!(probe(&top, None).branch, "?");
        fs::write(top.join(".git/HEAD"), "garbage").unwrap();
        assert_eq!(probe(&top, None).branch, "?");
        fs::remove_file(top.join(".git/HEAD")).unwrap();
        assert_eq!(probe(&top, None).branch, "?");
    }

    #[test]
    fn linked_worktree_is_named_after_the_main_repo() {
        let t = tempfile::tempdir().unwrap();
        let main = repo_dir(t.path(), "mainrepo", "ref: refs/heads/master\n");
        let admin = main.join(".git/worktrees/wt1");
        fs::create_dir_all(&admin).unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        fs::write(admin.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
        let wt = t.path().join("elsewhere/wt1");
        fs::create_dir_all(wt.join("sub")).unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();

        let info = probe(&wt.join("sub"), None);
        assert_eq!(info.repo, "mainrepo");
        assert_eq!(info.cwd_rel, "sub");
        assert_eq!(info.branch, "feature");
        assert_eq!(info.worktree.as_deref(), Some("wt1"));
        assert_eq!(info.head, Some(admin.join("HEAD")));
    }

    #[test]
    fn submodule_gitdir_without_commondir_is_not_a_worktree() {
        let t = tempfile::tempdir().unwrap();
        let sup = repo_dir(t.path(), "super", "ref: refs/heads/main\n");
        let modules = sup.join(".git/modules/lib");
        fs::create_dir_all(&modules).unwrap();
        fs::write(modules.join("HEAD"), "ref: refs/heads/dev\n").unwrap();
        let sub = sup.join("lib");
        fs::create_dir_all(&sub).unwrap();
        // Relative gitdir, as git writes it for submodules.
        fs::write(sub.join(".git"), "gitdir: ../.git/modules/lib\n").unwrap();

        let info = probe(&sub, None);
        assert_eq!(info.repo, "lib");
        assert_eq!(info.branch, "dev");
        assert_eq!(info.worktree, None);
    }

    #[test]
    fn an_oversized_head_is_read_capped_and_unrecognized() {
        let t = tempfile::tempdir().unwrap();
        let huge = format!("ref: refs/heads/{}", "b".repeat(1 << 20));
        let top = repo_dir(t.path(), "p", &huge);
        assert_eq!(read_small(&top.join(".git/HEAD")).unwrap().len(), 4096);
        // Truncated mid-name still parses as a (long) branch — the point is
        // the read is bounded, not that the result is pretty.
        assert_eq!(
            probe(&top, None).branch.len(),
            4096 - "ref: refs/heads/".len()
        );
    }

    /// A FIFO named `HEAD` (or `.git`) must never be opened: `open` on a
    /// writer-less FIFO blocks forever.
    #[cfg(unix)]
    #[test]
    fn a_fifo_head_or_dotgit_is_skipped_not_opened() {
        use std::os::unix::ffi::OsStrExt;
        fn mkfifo(p: &Path) {
            let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
            // SAFETY: `c` is a valid NUL-terminated path for the call's duration.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
        }
        let t = tempfile::tempdir().unwrap();
        let top = t.path().join("p");
        fs::create_dir_all(top.join(".git")).unwrap();
        mkfifo(&top.join(".git/HEAD"));
        assert_eq!(probe(&top, None).branch, "?");

        let other = t.path().join("q");
        fs::create_dir_all(&other).unwrap();
        mkfifo(&other.join(".git"));
        let info = probe(&other, None);
        assert_eq!((info.repo.as_str(), info.branch.as_str()), ("q", "?"));
    }

    #[test]
    fn a_malformed_git_file_still_marks_a_repo_with_unknown_head() {
        let t = tempfile::tempdir().unwrap();
        let top = t.path().join("odd");
        fs::create_dir_all(&top).unwrap();
        fs::write(top.join(".git"), "not a gitdir line").unwrap();
        let info = probe(&top, None);
        assert_eq!((info.repo.as_str(), info.branch.as_str()), ("odd", "?"));
    }

    #[test]
    fn outside_git_the_path_is_home_abbreviated() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let cwd = home.join("notes/x");
        fs::create_dir_all(&cwd).unwrap();
        let info = probe(&cwd, Some(&home));
        // `cwd` is abbreviated whether or not a repo encloses the tempdir.
        assert_eq!(info.cwd, "~/notes/x");
        if find_repo(t.path()).is_some() {
            eprintln!(
                "SKIP outside-git half: TMPDIR {:?} is inside a checkout",
                t.path()
            );
            return;
        }
        assert_eq!(info.repo, "~/notes/x");
        assert_eq!((info.cwd_rel.as_str(), info.branch.as_str()), ("", ""));
        assert_eq!(info.head, None);
        assert_eq!(probe(&home, Some(&home)).repo, "~");
    }

    #[test]
    fn main_repo_name_handles_a_bare_common_dir() {
        assert_eq!(main_repo_name(Path::new("/srv/foo.git")), "foo");
        assert_eq!(main_repo_name(Path::new("/w/bar/.git")), "bar");
    }
}
