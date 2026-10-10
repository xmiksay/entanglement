//! Persisted + in-memory "always allow" tool grants (#174).
//!
//! An [`InMsg::Approve`][entanglement_core::InMsg::Approve] carries an
//! [`ApprovalScope`]: [`Once`][ApprovalScope::Once] is the historical one-shot
//! approval (the next identical call asks again); [`Session`][ApprovalScope::Session]
//! and [`Always`][ApprovalScope::Always] widen it so an *identical* later call —
//! same tool **and** the argument [`grading_arg`][crate::permission_path::grading_arg]
//! extracts (command/path, #173; root-relativized for path tools, #485,
//! ADR-0125) — skips the prompt. This module owns the grant set that makes
//! that decision.
//!
//! A grant only ever upgrades a resolved `Ask` to `Allow`; it never touches a
//! `Deny`, so a hard policy floor (the session's permission mode or the config
//! ceiling, #172) stands regardless of what the user once approved. Matching
//! is **exact** for `Session`/`Always`: a grant for `bash(git status)`
//! re-allows only that command, never `git status -s` — the issue is repeated
//! prompts for the *same* call, not a pattern grant. Since ADR-0207 §8, a
//! `Session`/`Always` grant also carries the **mode** it was earned in and
//! matches only that mode exactly — a grant from `build` must not fire in
//! `research` — so [`GrantKey`] gained a `mode` field; a pre-ADR-0207 grants
//! file has no mode on its entries, loaded as `mode: None`, which by
//! construction never equals a live call's `Some(mode)` (never matches, the
//! safe direction). [`SessionDir`][ApprovalScope::SessionDir]
//! (#486, ADR-0126) is a deliberate exception to both the exact-match and the
//! mode-scoping rule — see below.
//!
//! # Scopes
//!
//! - **Session** — kept in memory, keyed by [`SessionId`]. Gone when the process
//!   exits; a child session never inherits a parent's session grants (least
//!   privilege, mirroring the permission clamp).
//! - **Always** — persisted to a **managed** grants file in the config dir
//!   (`<state dir>/grants.yml`, relocated by `ENTANGLEMENT_STATE_DIR`),
//!   a sibling of the provider-key env file (#220) rather than a section of the
//!   hand-edited `config.yml`: the runtime rewrites it freely, so it stays out of
//!   the commented user config the way secrets do. Loaded at startup, re-written
//!   on each new `Always` grant. Best-effort: a write failure is logged, never
//!   fatal.
//! - **SessionDir** (#486, ADR-0126) — session-only like `Session`, but widens
//!   to every call whose grading argument falls under the approved call's
//!   directory ([`dir_for`], [`dir_covers`]) instead of matching one exact
//!   call. Restricted to the read-only triad (`read`/`grep`/`glob`, the
//!   ADR-0114 `read` capability's members) — the tools a repeated-prompt
//!   nuisance actually comes from; any other tool degrades this to an exact
//!   `Session` grant rather than widening it. Never persisted (no
//!   `Always`-directory scope) — the TUI `/allow <path>` command
//!   (`grant_session_dir`) is the other way to add one, beside approving a
//!   prompted call with `[d]`. **Mode-scoped**, like every other scope (#634:
//!   a prior draft of ADR-0207 §8 carved this one out as an exception —
//!   "every mode with a human present to approve it allows the read-only
//!   triad anyway" — but that leaves one grant scope unscoped for no
//!   functional gain, and the user chose symmetry: a directory grant earned
//!   in `research` matches only `research`, exactly like `GrantKey`).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use entanglement_core::{ApprovalScope, SessionId};

/// A single granted tool call: the tool name, the optional argument
/// (command/path, #173) the grant was recorded against, and the permission
/// **mode** it was earned in (ADR-0207 §8) — `None` only for a legacy entry
/// loaded from a pre-ADR-0207 grants file, which never matches a live call
/// (every real call carries `Some(mode)`), so an old grant is treated as
/// non-matching rather than silently re-honored across modes. `arg == None`
/// grants every call to a tool that carries no permission argument (e.g.
/// `grep`). Matched by exact equality — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GrantKey {
    pub tool: String,
    pub arg: Option<String>,
    pub mode: Option<String>,
}

impl GrantKey {
    fn new(tool: &str, arg: Option<&str>, mode: &str) -> Self {
        Self {
            tool: tool.to_string(),
            arg: arg.map(str::to_string),
            mode: Some(mode.to_string()),
        }
    }

    /// The rule-key spelling used in the grants file: `tool(arg)` when scoped,
    /// bare `tool` otherwise — the same syntax the permission rules use (#173).
    fn to_rule(&self) -> String {
        match &self.arg {
            Some(a) => format!("{}({a})", self.tool),
            None => self.tool.clone(),
        }
    }

    /// Parse a grants-file rule key into a [`GrantKey`] at `mode`:
    /// `bash(git status)` ⇒ `{ bash, Some("git status"), mode }`, `grep` ⇒
    /// `{ grep, None, mode }`. A key with a `(` but no closing `)` is treated
    /// as a bare tool name (no argument).
    fn from_rule(key: &str, mode: Option<String>) -> Self {
        let (tool, arg) = match key.find('(') {
            Some(open) if key.ends_with(')') => (
                key[..open].to_string(),
                Some(key[open + 1..key.len() - 1].to_string()),
            ),
            _ => (key.to_string(), None),
        };
        Self { tool, arg, mode }
    }
}

/// The runtime's grant set: per-session (in-memory) plus persisted "always"
/// grants, with the file path to re-write on an `Always` grant. `session_dirs`
/// is the [`ApprovalScope::SessionDir`] store (#486, ADR-0126; mode-scoped
/// since #634): a session-only, never-persisted set of directories
/// (root-relative, #485), keyed by the mode they were earned in, that widen
/// the read-only triad (`read`/`grep`/`glob`) instead of matching one exact
/// call.
#[derive(Debug, Default)]
pub struct FileGrantStore {
    session: HashMap<SessionId, HashSet<GrantKey>>,
    session_dirs: HashMap<SessionId, HashMap<String, BTreeSet<String>>>,
    always: HashSet<GrantKey>,
    path: Option<PathBuf>,
}

impl FileGrantStore {
    /// Load the persisted `Always` grants from the managed file, resolving its
    /// path in the managed-state dir (`ENTANGLEMENT_STATE_DIR`). A
    /// missing file is an empty store; a malformed one is logged and treated as
    /// empty (a corrupt grants file must never wedge startup, and grants only
    /// *widen* access — dropping them is the safe failure).
    pub fn load() -> Self {
        Self::load_from(grants_file_path())
    }

    /// [`load`](Self::load) from an explicit grants file — or, with `None`, a
    /// purely in-memory store whose `Always` grants live only as long as it
    /// does. The injection seam (#747): a test pins a temp file (or none)
    /// rather than reading the developer's real `grants.yml` or steering
    /// `ENTANGLEMENT_STATE_DIR` through process env.
    pub fn load_from(path: Option<PathBuf>) -> Self {
        let always = match &path {
            Some(p) => read_grants(p),
            None => HashSet::new(),
        };
        Self {
            session: HashMap::new(),
            session_dirs: HashMap::new(),
            always,
            path,
        }
    }

    /// Whether a call `(tool, arg)` from `session` under `mode` is already
    /// granted — an active session grant, a persisted `Always` grant, or a
    /// [`ApprovalScope::SessionDir`] directory grant covering `arg` (#486),
    /// earned under this same `mode`. The executor consults this only when a
    /// call resolves to `Ask`, upgrading it to `Allow`. `mode` is matched
    /// exactly (ADR-0207 §8, extended to `SessionDir` by #634): a grant
    /// earned in one mode never fires in another.
    pub fn is_granted(
        &self,
        session: &SessionId,
        tool: &str,
        arg: Option<&str>,
        mode: &str,
    ) -> bool {
        let key = GrantKey::new(tool, arg, mode);
        if self.always.contains(&key)
            || self
                .session
                .get(session)
                .is_some_and(|set| set.contains(&key))
        {
            return true;
        }
        if crate::tool_names::is_read_capability_member(tool) {
            if let (Some(by_mode), Some(arg)) = (self.session_dirs.get(session), arg) {
                if let Some(dirs) = by_mode.get(mode) {
                    return dirs.iter().any(|dir| dir_covers(dir, arg));
                }
            }
        }
        false
    }

    /// Record an approval per its [`ApprovalScope`], tagged with the mode it
    /// was earned in (ADR-0207 §8). `Once` records nothing; `Session` adds an
    /// in-memory grant for `session`; `Always` adds a persisted grant and
    /// re-writes the managed file (best-effort); `SessionDir` (#486) derives
    /// the directory `(tool, arg)` implies (see [`dir_for`]) and widens the
    /// read-only triad under it, for this `mode` only (#634), for the rest of
    /// the session — on any other tool, or a call `dir_for` can't derive a
    /// directory from, it degrades to an exact `Session` grant instead of
    /// widening. Returns whether a new grant was stored (an already-known
    /// grant is a no-op).
    pub fn record(
        &mut self,
        session: &SessionId,
        tool: &str,
        arg: Option<&str>,
        scope: ApprovalScope,
        mode: &str,
    ) -> bool {
        match scope {
            ApprovalScope::Once => false,
            ApprovalScope::Session => {
                let key = GrantKey::new(tool, arg, mode);
                self.session.entry(session.clone()).or_default().insert(key)
            }
            ApprovalScope::Always => {
                let key = GrantKey::new(tool, arg, mode);
                let inserted = self.always.insert(key.clone());
                if inserted {
                    self.persist(&key);
                }
                inserted
            }
            ApprovalScope::SessionDir => {
                if crate::tool_names::is_read_capability_member(tool) {
                    if let Some(dir) = dir_for(tool, arg) {
                        return self
                            .session_dirs
                            .entry(session.clone())
                            .or_default()
                            .entry(mode.to_string())
                            .or_default()
                            .insert(dir);
                    }
                }
                let key = GrantKey::new(tool, arg, mode);
                self.session.entry(session.clone()).or_default().insert(key)
            }
        }
    }

    /// Grant `dir` to `session` for the read-only triad (`read`/`grep`/`glob`)
    /// under `mode` only (#634) — the TUI `/allow <path>` command's entry
    /// point (#486, ADR-0126). `dir` is lexically normalized
    /// ([`crate::permission_path::normalize_lexical`], #485) before storage;
    /// returns the normalized form for the caller's confirmation status line.
    /// Never persisted — a directory grant is session-only by design, unlike
    /// the exact-match `Always` scope above.
    pub fn grant_session_dir(&mut self, session: &SessionId, dir: &str, mode: &str) -> String {
        let normalized = crate::permission_path::normalize_lexical(dir);
        self.session_dirs
            .entry(session.clone())
            .or_default()
            .entry(mode.to_string())
            .or_default()
            .insert(normalized.clone());
        normalized
    }

    /// Drop a session's in-memory grants when it closes, so a reused id (there are
    /// none today, but the store outlives sessions) never sees stale approvals.
    pub fn forget_session(&mut self, session: &SessionId) {
        self.session.remove(session);
        self.session_dirs.remove(session);
    }

    /// Re-write the managed file from the current `Always` set, merged against
    /// whatever is on disk under an exclusive lock (#329) — a concurrent skutter
    /// instance's own `Always` grant, added between this store's `load()` and
    /// now, must survive rather than being clobbered by a write from stale
    /// in-memory state. Best-effort: a write failure is logged, never
    /// propagated — a lost persisted grant only means the user is asked again,
    /// the safe direction.
    fn persist(&mut self, new_key: &GrantKey) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let result = crate::config::lock::with_locked_file(&path, || {
            let mut merged = read_grants(&path);
            merged.insert(new_key.clone());
            write_grants(&path, &merged)?;
            Ok(merged)
        });
        match result {
            Ok(merged) => self.always = merged,
            Err(e) => tracing::warn!("could not persist tool grants to {}: {e:#}", path.display()),
        }
    }

    /// Re-read the persisted `Always` grants from disk (#329) — picks up a grant
    /// another skutter instance recorded, without disturbing this process's
    /// in-memory `Session`-scoped grants (those are never shared across
    /// processes by design).
    pub fn reload(&mut self) {
        if let Some(path) = &self.path {
            self.always = read_grants(path);
        }
    }
}

// SessionDir directory derivation/coverage (#486, ADR-0126) — split out of
// this (previously grandfathered-over-cap) file into its own module: a
// self-contained unit with no dependency on `GrantKey`/mode-scoping.
mod session_dir;
use session_dir::{dir_covers, dir_for};

// The managed grants file: path resolution, on-disk shape, read/write.
mod file;
use file::{grants_file_path, read_grants, write_grants};

#[cfg(test)]
mod tests {
    use super::*;

    /// A grants file in a fresh temp dir — each test its own, injected via
    /// `load_from` (#747), so no process env is shared or mutated.
    fn tmp_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("grants.yml");
        (dir, path)
    }

    #[test]
    fn grant_key_rule_roundtrips() {
        for key in [
            GrantKey::new("bash", Some("git status"), "build"),
            GrantKey::new("edit", Some("src/main.rs"), "build"),
            GrantKey::new("grep", None, "build"),
        ] {
            assert_eq!(GrantKey::from_rule(&key.to_rule(), key.mode.clone()), key);
        }
        // A malformed key (no closing paren) degrades to a bare tool name.
        assert_eq!(
            GrantKey::from_rule("bash(oops", Some("build".to_string())),
            GrantKey::new("bash(oops", None, "build")
        );
    }

    #[test]
    fn once_records_nothing() {
        let mut store = FileGrantStore::default();
        let s = SessionId::new("s");
        assert!(!store.record(&s, "bash", Some("ls"), ApprovalScope::Once, "build"));
        assert!(!store.is_granted(&s, "bash", Some("ls"), "build"));
    }

    #[test]
    fn session_grant_is_scoped_to_its_session() {
        let mut store = FileGrantStore::default();
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        assert!(store.record(
            &a,
            "bash",
            Some("git status"),
            ApprovalScope::Session,
            "build"
        ));
        // The granting session skips the prompt for the identical call...
        assert!(store.is_granted(&a, "bash", Some("git status"), "build"));
        // ...a different command still asks, and a different session never inherits.
        assert!(!store.is_granted(&a, "bash", Some("git log"), "build"));
        assert!(!store.is_granted(&b, "bash", Some("git status"), "build"));
        // Re-recording the same grant is a no-op.
        assert!(!store.record(
            &a,
            "bash",
            Some("git status"),
            ApprovalScope::Session,
            "build"
        ));
        store.forget_session(&a);
        assert!(!store.is_granted(&a, "bash", Some("git status"), "build"));
    }

    /// ADR-0207 §8: a grant earned in one mode never fires in another, even
    /// for the same session/tool/argument.
    #[test]
    fn session_grant_is_scoped_to_its_mode() {
        let mut store = FileGrantStore::default();
        let s = SessionId::new("s");
        assert!(store.record(
            &s,
            "bash",
            Some("git status"),
            ApprovalScope::Session,
            "build"
        ));
        assert!(store.is_granted(&s, "bash", Some("git status"), "build"));
        assert!(
            !store.is_granted(&s, "bash", Some("git status"), "research"),
            "a grant earned in build must not fire in research"
        );
    }

    #[test]
    fn argless_grant_covers_the_whole_tool() {
        let mut store = FileGrantStore::default();
        let s = SessionId::new("s");
        store.record(&s, "grep", None, ApprovalScope::Session, "build");
        assert!(store.is_granted(&s, "grep", None, "build"));
    }

    // --- #486, ADR-0126: SessionDir directory grants ------------------------
    // Mode-scoped since #634, symmetric with `GrantKey` -- a `SessionDir`
    // grant only ever fires under the mode it was earned in.

    #[test]
    fn session_dir_grant_covers_repeated_reads_under_one_directory() {
        let mut store = FileGrantStore::default();
        let s = SessionId::new("s");
        assert!(store.record(
            &s,
            "read",
            Some("src/a.rs"),
            ApprovalScope::SessionDir,
            "build"
        ));
        // The approved file itself, a sibling, and a nested subdirectory all
        // fall under the granted "src" directory.
        assert!(store.is_granted(&s, "read", Some("src/a.rs"), "build"));
        assert!(store.is_granted(&s, "read", Some("src/b.rs"), "build"));
        assert!(store.is_granted(&s, "read", Some("src/b/c.rs"), "build"));
        // grep/glob under the same directory are covered too (the triad).
        assert!(store.is_granted(&s, "grep", Some("src"), "build"));
        assert!(store.is_granted(&s, "grep", Some("src/sub"), "build"));
        assert!(store.is_granted(&s, "glob", Some("src/*.rs"), "build"));
    }

    /// #634: symmetric with `session_grant_is_scoped_to_its_mode` — a
    /// `SessionDir` grant earned under `build` must not fire under `research`,
    /// even for the same session/tool/directory.
    #[test]
    fn session_dir_grant_is_scoped_to_its_mode() {
        let mut store = FileGrantStore::default();
        let s = SessionId::new("s");
        store.record(
            &s,
            "read",
            Some("src/a.rs"),
            ApprovalScope::SessionDir,
            "build",
        );
        assert!(store.is_granted(&s, "read", Some("src/a.rs"), "build"));
        assert!(
            !store.is_granted(&s, "read", Some("src/a.rs"), "research"),
            "a directory grant earned in build must not fire in research"
        );
    }

    #[test]
    fn session_dir_grant_does_not_cover_a_sibling_directory() {
        let mut store = FileGrantStore::default();
        let s = SessionId::new("s");
        store.record(
            &s,
            "read",
            Some("src/a.rs"),
            ApprovalScope::SessionDir,
            "build",
        );
        assert!(!store.is_granted(&s, "read", Some("src2/x"), "build"));
    }

    #[test]
    fn session_dir_grant_does_not_widen_a_mutation_tool() {
        let mut store = FileGrantStore::default();
        let s = SessionId::new("s");
        // `edit` is not in the read-only triad, so this degrades to an exact
        // `Session` grant -- the identical call is covered, a sibling isn't.
        assert!(store.record(
            &s,
            "edit",
            Some("src/a.rs"),
            ApprovalScope::SessionDir,
            "build"
        ));
        assert!(store.is_granted(&s, "edit", Some("src/a.rs"), "build"));
        assert!(!store.is_granted(&s, "edit", Some("src/b.rs"), "build"));
    }

    #[test]
    fn session_dir_grant_does_not_leak_to_bash() {
        let mut store = FileGrantStore::default();
        let s = SessionId::new("s");
        // `bash` has no directory concept (`dir_for` returns `None`), so this
        // also degrades to an exact `Session` grant on the literal command.
        store.record(
            &s,
            "bash",
            Some("git status"),
            ApprovalScope::SessionDir,
            "build",
        );
        assert!(store.is_granted(&s, "bash", Some("git status"), "build"));
        assert!(!store.is_granted(&s, "bash", Some("git log"), "build"));
    }

    #[test]
    fn session_dir_grant_is_scoped_to_its_session() {
        let mut store = FileGrantStore::default();
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        store.record(
            &a,
            "read",
            Some("src/a.rs"),
            ApprovalScope::SessionDir,
            "build",
        );
        assert!(store.is_granted(&a, "read", Some("src/b.rs"), "build"));
        assert!(!store.is_granted(&b, "read", Some("src/b.rs"), "build"));
    }

    #[test]
    fn forget_session_clears_dir_grants() {
        let mut store = FileGrantStore::default();
        let s = SessionId::new("s");
        store.record(
            &s,
            "read",
            Some("src/a.rs"),
            ApprovalScope::SessionDir,
            "build",
        );
        assert!(store.is_granted(&s, "read", Some("src/b.rs"), "build"));
        store.forget_session(&s);
        assert!(!store.is_granted(&s, "read", Some("src/b.rs"), "build"));
    }

    #[test]
    fn grant_session_dir_normalizes_and_covers_the_triad() {
        let mut store = FileGrantStore::default();
        let s = SessionId::new("s");
        assert_eq!(
            store.grant_session_dir(&s, "./src/", "build"),
            "src".to_string()
        );
        assert!(store.is_granted(&s, "read", Some("src/a.rs"), "build"));
        assert!(store.is_granted(&s, "grep", Some("src/a.rs"), "build"));
        assert!(store.is_granted(&s, "glob", Some("src/*.rs"), "build"));
        assert!(!store.is_granted(&s, "edit", Some("src/a.rs"), "build"));
        // #634: the `/allow` grant is scoped to the mode it was made in too.
        assert!(!store.is_granted(&s, "read", Some("src/a.rs"), "research"));
    }

    #[test]
    fn grant_session_dir_dot_covers_every_relative_arg() {
        let mut store = FileGrantStore::default();
        let s = SessionId::new("s");
        store.grant_session_dir(&s, ".", "build");
        assert!(store.is_granted(&s, "read", Some("anything/at/all.rs"), "build"));
        assert!(store.is_granted(&s, "read", Some("top_level.rs"), "build"));
    }

    #[test]
    fn always_grant_persists_and_reloads_across_stores() {
        let (_dir, path) = tmp_path();

        let s = SessionId::new("s");
        let mut store = FileGrantStore::load_from(Some(path.clone()));
        assert!(store.record(
            &s,
            "bash",
            Some("git status"),
            ApprovalScope::Always,
            "build"
        ));

        // A freshly loaded store (new process) sees the persisted grant, and it is
        // global -- any session under the same mode skips the prompt.
        let reloaded = FileGrantStore::load_from(Some(path.clone()));
        assert!(reloaded.is_granted(
            &SessionId::new("other"),
            "bash",
            Some("git status"),
            "build"
        ));
        assert!(!reloaded.is_granted(&SessionId::new("other"), "bash", Some("git log"), "build"));
        // A different mode never inherits it (ADR-0207 §8).
        assert!(!reloaded.is_granted(
            &SessionId::new("other"),
            "bash",
            Some("git status"),
            "research"
        ));
    }

    #[test]
    fn concurrent_always_grants_from_two_stores_both_survive() {
        // Two "processes" (threads, each with its own `FileGrantStore::load_from`)
        // race to record *different* `Always` grants against the same on-disk
        // file (#329). Without the lock's read-current-then-merge, the second
        // writer's `std::fs::write` of its own stale `self.always` would clobber
        // the first writer's grant -- a lost update. A freshly loaded third store
        // must see both.
        let (_dir, path) = tmp_path();

        let (pa, pb) = (path.clone(), path.clone());
        let a = std::thread::spawn(move || {
            let mut store = FileGrantStore::load_from(Some(pa));
            store.record(
                &SessionId::new("a"),
                "bash",
                Some("git status"),
                ApprovalScope::Always,
                "build",
            );
        });
        let b = std::thread::spawn(move || {
            let mut store = FileGrantStore::load_from(Some(pb));
            store.record(
                &SessionId::new("b"),
                "bash",
                Some("git log"),
                ApprovalScope::Always,
                "build",
            );
        });
        a.join().unwrap();
        b.join().unwrap();

        let reloaded = FileGrantStore::load_from(Some(path.clone()));
        let any = SessionId::new("other");
        assert!(
            reloaded.is_granted(&any, "bash", Some("git status"), "build"),
            "grant recorded by the first store must survive a concurrent write"
        );
        assert!(
            reloaded.is_granted(&any, "bash", Some("git log"), "build"),
            "grant recorded by the second store must survive a concurrent write"
        );
    }

    #[test]
    fn reload_picks_up_another_process_grant() {
        let (_dir, path) = tmp_path();

        let mut store = FileGrantStore::load_from(Some(path.clone()));
        let session = SessionId::new("s");
        store.record(
            &session,
            "bash",
            Some("echo hi"),
            ApprovalScope::Session,
            "build",
        );

        // Another instance persists an `Always` grant directly on disk.
        let mut other = FileGrantStore::load_from(Some(path.clone()));
        other.record(
            &SessionId::new("other"),
            "grep",
            None,
            ApprovalScope::Always,
            "build",
        );

        assert!(
            !store.is_granted(&session, "grep", None, "build"),
            "stale before reload"
        );
        store.reload();
        assert!(
            store.is_granted(&session, "grep", None, "build"),
            "reload must pick up the new Always grant"
        );
        // The session-scoped grant recorded before reload is untouched.
        assert!(store.is_granted(&session, "bash", Some("echo hi"), "build"));
    }

    #[test]
    fn malformed_grants_file_loads_empty() {
        let (_dir, path) = tmp_path();
        std::fs::write(&path, "grants: [oops\n").unwrap();
        let store = FileGrantStore::load_from(Some(path.clone()));
        assert!(!store.is_granted(&SessionId::new("s"), "bash", Some("x"), "build"));
    }

    /// ADR-0207 §8: a grants file written before this stage has no `mode` on
    /// its entries -- loaded as `mode: None`, which never matches a live
    /// call's `Some(mode)`, so a legacy `Always` grant is silently inert
    /// rather than re-honored under whatever mode a call happens to run in.
    #[test]
    fn legacy_mode_less_grant_never_matches_a_live_call() {
        let (_dir, path) = tmp_path();
        std::fs::write(&path, "grants:\n  - bash(git status)\n").unwrap();
        let store = FileGrantStore::load_from(Some(path.clone()));
        assert!(!store.is_granted(&SessionId::new("s"), "bash", Some("git status"), "build"));
        assert!(!store.is_granted(&SessionId::new("s"), "bash", Some("git status"), "research"));
    }

    /// A grant recorded by this stage round-trips through the on-disk
    /// `{mode, rule}` shape and is readable back by a fresh store.
    #[test]
    fn scoped_grant_round_trips_through_the_file() {
        let (_dir, path) = tmp_path();

        let mut store = FileGrantStore::load_from(Some(path.clone()));
        store.record(
            &SessionId::new("s"),
            "bash",
            Some("git status"),
            ApprovalScope::Always,
            "research",
        );
        let reloaded = FileGrantStore::load_from(Some(path.clone()));
        assert!(reloaded.is_granted(
            &SessionId::new("other"),
            "bash",
            Some("git status"),
            "research"
        ));
        assert!(!reloaded.is_granted(
            &SessionId::new("other"),
            "bash",
            Some("git status"),
            "build"
        ));
    }
}
