//! Containment of the `glob`/`grep` walk (ADR-0054/ADR-0132): the boundary
//! is the root, widened per match only by a durable `read` entry in the
//! escape-root store — never by where the pattern's own walk starts.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use entanglement_core::ApprovalScope;

use super::*;
use crate::host::{GlobTool, GrepTool};
use crate::state::ExtraRootStore;
use crate::tools::Tool;

/// A canonical `root/` (with `src/in.txt`) and a sibling `outside/` (with
/// `secret.txt`), two levels apart so `../outside` reaches it from `src`.
struct Fx {
    _dir: tempfile::TempDir,
    root: PathBuf,
    outside: PathBuf,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (root, outside) = (base.join("root"), base.join("outside"));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(root.join("src/in.txt"), "needle in\n").unwrap();
    std::fs::write(outside.join("secret.txt"), "needle secret\n").unwrap();
    Fx {
        _dir: dir,
        root,
        outside,
    }
}

fn names(list: &FileList) -> Vec<String> {
    list.files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

fn read_grant(dir: &Path) -> ExtraRootStore {
    let store = ExtraRootStore::ephemeral();
    store.record("read", dir, ApprovalScope::Session);
    store
}

/// The #968 review's bypass: a brace alternative reaching outside the root
/// must not move the walk's boundary there.
#[test]
fn a_brace_alternative_out_of_the_root_is_dropped() {
    let f = fx();
    for pattern in ["{src,../outside}/*", "{src/*,../outside/*}", "../outside/*"] {
        let list = list_files(&f.root, pattern, &[]).unwrap();
        let got = names(&list);
        assert!(
            !got.iter().any(|n| n.contains("secret")),
            "{pattern}: {got:?}"
        );
        assert!(list.out_of_root > 0, "{pattern}: counted, not silent");
    }
    let list = list_files(&f.root, "{src,../outside}/*", &[]).unwrap();
    assert!(names(&list).iter().any(|n| n.ends_with("in.txt")));
}

/// An absolute directory outside the root is not expanded into a walk.
#[test]
fn an_ungranted_external_directory_is_not_searched() {
    let f = fx();
    let list = list_files(&f.root, &f.outside.display().to_string(), &[]).unwrap();
    assert!(list.files.is_empty(), "{:?}", list.files);
    let pattern = format!("{}/**/*", f.outside.display());
    let store = ExtraRootStore::ephemeral();
    let list = list_files_with_extra_roots(&f.root, &pattern, &[], Some(&store)).unwrap();
    assert!(list.files.is_empty(), "{:?}", list.files);
}

/// A durable `read` entry (ADR-0132) admits matches under it, expands a
/// bare directory and ignores that directory's own `.gitignore`.
#[test]
fn a_durable_read_entry_admits_its_directory() {
    let f = fx();
    std::fs::write(f.outside.join(".gitignore"), "secret.txt\n").unwrap();
    let store = read_grant(&f.outside);
    let bare = f.outside.display().to_string();
    let list = list_files_with_extra_roots(&f.root, &bare, &[], Some(&store)).unwrap();
    assert!(
        names(&list).iter().any(|n| n.ends_with("secret.txt")),
        "{:?}",
        list.files
    );
    let other = ExtraRootStore::ephemeral();
    other.record("write", &f.outside, ApprovalScope::Session);
    let list = list_files_with_extra_roots(&f.root, &bare, &[], Some(&other)).unwrap();
    assert!(list.files.is_empty(), "a write entry is not a read one");
}

/// The trusted scratch dir counts for every tool, `read` included.
#[test]
fn the_scratch_dir_is_searchable() {
    let f = fx();
    let store = ExtraRootStore::ephemeral().with_scratch(f.outside.clone());
    let pattern = format!("{}/*", f.outside.display());
    let list = list_files_with_extra_roots(&f.root, &pattern, &[], Some(&store)).unwrap();
    assert!(names(&list).iter().any(|n| n.ends_with("secret.txt")));
}

/// A symlink inside a granted directory pointing elsewhere does not leak.
#[cfg(unix)]
#[test]
fn a_symlink_out_of_a_granted_directory_does_not_leak() {
    let f = fx();
    let granted = f.outside.join("granted");
    std::fs::create_dir_all(&granted).unwrap();
    std::fs::write(granted.join("ok.txt"), "x\n").unwrap();
    std::os::unix::fs::symlink(&f.outside, granted.join("escape")).unwrap();
    let store = read_grant(&granted);
    let pattern = format!("{}/**/*", granted.display());
    let list = list_files_with_extra_roots(&f.root, &pattern, &[], Some(&store)).unwrap();
    let got = names(&list);
    assert!(got.iter().any(|n| n.ends_with("ok.txt")), "{got:?}");
    assert!(!got.iter().any(|n| n.ends_with("secret.txt")), "{got:?}");
}

/// The tools themselves: a brace pattern or `path` filter reaching outside
/// the root finds nothing there, and a durable entry makes it searchable.
#[tokio::test]
async fn glob_and_grep_keep_to_the_root_unless_a_read_entry_covers_it() {
    let f = fx();
    let glob = GlobTool::new(f.root.clone());
    let out = glob
        .run(&serde_json::json!({"pattern": "{src,../outside}/*"}).to_string())
        .await
        .unwrap();
    assert!(!out.contains("secret"), "{out}");
    let grep = GrepTool::new(f.root.clone());
    let filter = serde_json::json!({"pattern": "needle", "path": "{src,../outside}/*"});
    let out = grep.run(&filter.to_string()).await.unwrap();
    assert!(
        out.contains("needle in") && !out.contains("secret"),
        "{out}"
    );

    let store = Arc::new(read_grant(&f.outside));
    let grep = GrepTool::new(f.root.clone()).with_extra_roots(store.clone());
    let out = grep.run(&filter.to_string()).await.unwrap();
    assert!(out.contains("needle secret"), "{out}");
    let glob = GlobTool::new(f.root.clone()).with_extra_roots(store);
    let pattern = serde_json::json!({"pattern": format!("{}/*", f.outside.display())});
    let out = glob.run(&pattern.to_string()).await.unwrap();
    assert!(out.contains("secret.txt"), "{out}");
}
