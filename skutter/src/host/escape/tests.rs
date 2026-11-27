use super::*;
use entanglement_core::ApprovalScope;

/// An inner resolver that answers one fixed grade.
struct Fixed(Grade);

#[async_trait]
impl PermissionResolver for Fixed {
    async fn resolve(&self, _session: &SessionId, _tool: &str, _input: &str) -> Grade {
        self.0.clone()
    }
}

struct Fx {
    _dir: tempfile::TempDir,
    root: PathBuf,
    outside: PathBuf,
    store: Arc<ExtraRootStore>,
}

/// A canonical `root/` with a sibling `outside/`.
fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (root, outside) = (base.join("root"), base.join("outside"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    Fx {
        _dir: dir,
        root,
        outside,
        store: Arc::new(ExtraRootStore::ephemeral()),
    }
}

impl Fx {
    fn resolver(&self, inner: Grade) -> EscapeResolver {
        let tools = super::super::every_tool(self.root.clone()).shared();
        EscapeResolver::new(
            Arc::new(Fixed(inner)),
            self.root.clone(),
            self.store.clone(),
            tools,
        )
    }

    async fn grade(&self, inner: Grade, tool: &str, input: serde_json::Value) -> Grade {
        self.resolver(inner)
            .resolve(&SessionId::new("s"), tool, &input.to_string())
            .await
    }
}

fn reason(grade: &Grade) -> Option<(CardReason, String)> {
    match grade {
        Grade::Ask(Some(why)) => Some((why.card, why.text.clone())),
        _ => None,
    }
}

#[tokio::test]
async fn an_in_root_call_keeps_its_grade() {
    let f = fx();
    let read = serde_json::json!({"path": "a.txt"});
    assert_eq!(
        f.grade(Grade::Allow, "read", read.clone()).await,
        Grade::Allow
    );
    assert_eq!(
        f.grade(Grade::Ask(None), "read", read).await,
        Grade::Ask(None)
    );
    let bash = serde_json::json!({"command": "ls"});
    assert_eq!(f.grade(Grade::Allow, "bash", bash).await, Grade::Allow);
}

/// Whatever the mode allowed, a path outside the root asks, saying where.
#[tokio::test]
async fn an_out_of_root_path_asks_with_the_reason() {
    let f = fx();
    let target = f.outside.join("x.txt");
    let input = serde_json::json!({"path": target});
    let graded = f.grade(Grade::Allow, "write", input).await;
    let (card, text) = reason(&graded).expect("a reasoned ask");
    assert_eq!(card, CardReason::RootEscape);
    assert_eq!(
        text,
        format!(
            "accesses a path outside the project root: {}",
            target.display()
        )
    );
    let workdir = serde_json::json!({"command": "ls", "workdir": f.outside});
    let graded = f.grade(Grade::Allow, "bash", workdir).await;
    assert_eq!(reason(&graded).unwrap().0, CardReason::RootEscape);
}

/// A search never asks (ADR-0132): its walk drops what lies outside the
/// root, so an out-of-root or brace pattern keeps the mode's grade.
#[tokio::test]
async fn a_search_never_asks() {
    let f = fx();
    let outside = format!("{}/*", f.outside.display());
    for pattern in [outside.as_str(), "{src,../outside}/*", "../outside/*"] {
        let glob = serde_json::json!({"pattern": pattern});
        assert_eq!(f.grade(Grade::Allow, "glob", glob).await, Grade::Allow);
        let grep = serde_json::json!({"pattern": "x", "path": pattern});
        assert_eq!(f.grade(Grade::Allow, "grep", grep).await, Grade::Allow);
    }
}

/// A deny wins over the escape: there is nothing to ask about.
#[tokio::test]
async fn a_deny_stays_a_deny() {
    let f = fx();
    let input = serde_json::json!({"path": f.outside.join("x")});
    let denied = f.grade(Grade::deny("no"), "read", input).await;
    assert_eq!(denied, Grade::deny("no"));
}

/// A script `path` outside the root asks as a launch escape.
#[tokio::test]
async fn a_script_outside_the_root_is_a_launch_escape() {
    let f = fx();
    let input = serde_json::json!({"path": f.outside.join("s.rhai")});
    let graded = f.grade(Grade::Allow, RHAI_TOOL, input).await;
    let (card, text) = reason(&graded).unwrap();
    assert_eq!(card, CardReason::LaunchEscape);
    assert!(
        text.starts_with("runs a script outside the project root"),
        "{text}"
    );
    let inline = serde_json::json!({"script": "1"});
    assert_eq!(f.grade(Grade::Allow, RHAI_TOOL, inline).await, Grade::Allow);
}

/// A durable store entry (a `/cd`, the scratch dir) skips the ask — for
/// the tool it names only.
#[tokio::test]
async fn a_durable_store_entry_skips_the_ask() {
    let f = fx();
    f.store.record("bash", &f.outside, ApprovalScope::Session);
    let workdir = serde_json::json!({"command": "ls", "workdir": f.outside});
    assert_eq!(f.grade(Grade::Allow, "bash", workdir).await, Grade::Allow);
    let read = serde_json::json!({"path": f.outside});
    assert!(reason(&f.grade(Grade::Allow, "read", read).await).is_some());
}

/// A symlink under the root that points out of it is judged by its target.
#[cfg(unix)]
#[tokio::test]
async fn a_symlink_out_of_the_root_asks() {
    let f = fx();
    std::os::unix::fs::symlink(&f.outside, f.root.join("link")).unwrap();
    let input = serde_json::json!({"path": "link/x.txt"});
    let graded = f.grade(Grade::Allow, "read", input).await;
    let (_, text) = reason(&graded).unwrap();
    assert!(
        text.ends_with(&f.outside.join("x.txt").display().to_string()),
        "{text}"
    );
}

fn grant_of(grade: &Grade) -> AskGrant {
    match grade {
        Grade::Ask(Some(why)) => why.grant.clone().expect("an escape ask names its grant"),
        other => panic!("not a reasoned ask: {other:?}"),
    }
}

/// The #968 review: an escape ask is answered by its own key, never by a
/// grant on the bare argument — `bash`'s keys on its command *and* the
/// escaped workdir, a path tool's on the escaped path.
#[tokio::test]
async fn an_escape_ask_is_keyed_on_what_escapes() {
    let f = fx();
    let other = f.outside.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let bash = |cmd: &str, dir: &Path| serde_json::json!({"command": cmd, "workdir": dir});
    let key = grant_of(
        &f.grade(Grade::Allow, "bash", bash("make", &f.outside))
            .await,
    );
    assert_eq!(key.tool, "bash#outside-root");
    let expected = serde_json::json!([f.outside.to_string_lossy(), "make"]).to_string();
    assert_eq!(key.arg, expected);
    assert_ne!(key.arg, "make", "never the bare command");
    let elsewhere = grant_of(&f.grade(Grade::Allow, "bash", bash("make", &other)).await);
    assert_ne!(key, elsewhere, "another workdir is another grant");
    let other_cmd = grant_of(
        &f.grade(Grade::Allow, "bash", bash("rm x", &f.outside))
            .await,
    );
    assert_ne!(key, other_cmd, "another command is another grant");

    let target = f.outside.join("x.txt");
    let read = serde_json::json!({"path": target});
    let key = grant_of(&f.grade(Grade::Ask(None), "read", read).await);
    assert_eq!(key.tool, "read#outside-root");
    assert_eq!(key.arg, target.to_string_lossy());
}
