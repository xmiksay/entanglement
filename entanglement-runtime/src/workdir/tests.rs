use super::*;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    outside: PathBuf,
}

/// A canonical project root with `sub/` and a sibling `outside/` directory.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = dir.path().canonicalize().expect("canonical tempdir");
    let root = base.join("root");
    let outside = base.join("outside");
    std::fs::create_dir_all(root.join("sub/deep")).expect("mkdir sub");
    std::fs::create_dir_all(&outside).expect("mkdir outside");
    std::fs::write(root.join("file.txt"), "x").expect("write file");
    Fixture {
        _dir: dir,
        root,
        outside,
    }
}

#[test]
fn a_directory_inside_the_root_resolves_canonical() {
    let f = fixture();
    let got = resolve(&f.root, None, None, "sub", None).expect("inside");
    assert_eq!(got, f.root.join("sub"));
    let current = f.root.join("sub").to_string_lossy().into_owned();
    let got = resolve(&f.root, None, Some(&current), "deep/..", None).expect("relative");
    assert_eq!(got, f.root.join("sub"), "relative to the current workdir");
    let abs = f.root.to_string_lossy().into_owned();
    assert_eq!(resolve(&f.root, None, None, &abs, None), Ok(f.root.clone()));
}

#[test]
fn a_file_a_missing_path_or_nothing_is_refused() {
    let f = fixture();
    let err = resolve(&f.root, None, None, "file.txt", None).unwrap_err();
    assert!(err.contains("not a directory"), "{err}");
    assert!(resolve(&f.root, None, None, "nope", None).is_err());
    assert!(resolve(&f.root, None, None, "  ", None).is_err());
}

#[test]
fn outside_the_root_needs_an_approval() {
    let f = fixture();
    let store = ExtraRootStore::ephemeral();
    let outside = f.outside.to_string_lossy().into_owned();
    let err = resolve(&f.root, Some(&store), None, &outside, None).unwrap_err();
    assert!(err.contains("outside the project root"), "{err}");
    assert!(err.contains("/cd --session"), "{err}");
    // Strict containment with no store at all.
    assert!(resolve(&f.root, None, None, &outside, Some(ApprovalScope::Session)).is_err());
    // A one-shot cannot back a directory every later command runs in.
    assert!(resolve(
        &f.root,
        Some(&store),
        None,
        &outside,
        Some(ApprovalScope::Once)
    )
    .is_err());
    assert!(!store.is_durably_allowed("bash", &f.outside));
}

#[test]
fn an_approval_records_the_exec_grant_and_is_remembered() {
    let f = fixture();
    let store = ExtraRootStore::ephemeral();
    let outside = f.outside.to_string_lossy().into_owned();
    let got = resolve(
        &f.root,
        Some(&store),
        None,
        &outside,
        Some(ApprovalScope::Session),
    );
    assert_eq!(got, Ok(f.outside.clone()));
    assert!(store.is_durably_allowed("bash", &f.outside));
    assert!(store.is_durably_allowed("call", &f.outside));
    assert!(
        !store.is_durably_allowed("read", &f.outside),
        "file tools keep their own per-path escape-root rules"
    );
    // Asked once: the same directory now resolves with no approval.
    assert_eq!(
        resolve(&f.root, Some(&store), None, &outside, None),
        Ok(f.outside.clone())
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_out_of_the_root_resolves_to_its_target() {
    let f = fixture();
    std::os::unix::fs::symlink(&f.outside, f.root.join("link")).expect("symlink");
    let err = resolve(&f.root, None, None, "link", None).unwrap_err();
    assert!(err.contains("outside the project root"), "{err}");
}

#[test]
fn the_resolver_seam_returns_the_absolute_path() {
    let f = fixture();
    let seam = resolver(f.root.clone(), None);
    let got = seam(None, "sub", None).expect("inside");
    assert_eq!(got, f.root.join("sub").to_string_lossy());
}

#[test]
fn session_workdirs_track_and_forget() {
    let w = SessionWorkdirs::default();
    let s = SessionId::new("s1");
    let root = Path::new("/p");
    assert_eq!(SessionWorkdirs::display_root(Some(&w), &s, root), root);
    w.observe(&s, Some("/p/sub"));
    assert_eq!(w.get(&s), Some(PathBuf::from("/p/sub")));
    assert_eq!(
        SessionWorkdirs::display_root(Some(&w), &s, root),
        PathBuf::from("/p/sub")
    );
    w.observe(&s, None);
    assert_eq!(w.get(&s), None);
    w.observe(&s, Some("/p/sub"));
    w.forget(&s);
    assert_eq!(w.get(&s), None);
    assert_eq!(SessionWorkdirs::display_root(None, &s, root), root);
}
