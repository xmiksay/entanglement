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
//! `Deny`, so a hard policy floor (agent profile or config ceiling, #172) stands
//! regardless of what the user once approved. Matching is **exact**: a grant for
//! `bash(git status)` re-allows only that command, never `git status -s` — the
//! issue is repeated prompts for the *same* call, not a pattern grant.
//!
//! # Scopes
//!
//! - **Session** — kept in memory, keyed by [`SessionId`]. Gone when the process
//!   exits; a child session never inherits a parent's session grants (least
//!   privilege, mirroring the permission clamp).
//! - **Always** — persisted to a **managed** grants file in the config dir
//!   (`${config_dir}/entanglement/grants.yml`, override `ENTANGLEMENT_GRANTS_FILE`),
//!   a sibling of the provider-key env file (#220) rather than a section of the
//!   hand-edited `config.yml`: the runtime rewrites it freely, so it stays out of
//!   the commented user config the way secrets do. Loaded at startup, re-written
//!   on each new `Always` grant. Best-effort: a write failure is logged, never
//!   fatal.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use entanglement_core::{ApprovalScope, SessionId};
use serde::{Deserialize, Serialize};

/// Env var overriding the managed grants file path (tests + non-XDG setups).
const GRANTS_FILE_ENV: &str = "ENTANGLEMENT_GRANTS_FILE";

/// A single granted tool call: the tool name plus the optional argument
/// (command/path, #173) the grant was recorded against. `arg == None` grants
/// every call to a tool that carries no permission argument (e.g. `grep`).
/// Matched by exact equality — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GrantKey {
    pub tool: String,
    pub arg: Option<String>,
}

impl GrantKey {
    fn new(tool: &str, arg: Option<&str>) -> Self {
        Self {
            tool: tool.to_string(),
            arg: arg.map(str::to_string),
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

    /// Parse a grants-file rule key back into a [`GrantKey`]: `bash(git status)`
    /// ⇒ `{ bash, Some("git status") }`, `grep` ⇒ `{ grep, None }`. A key with a
    /// `(` but no closing `)` is treated as a bare tool name (no argument).
    fn from_rule(key: &str) -> Self {
        if let Some(open) = key.find('(') {
            if key.ends_with(')') {
                return Self {
                    tool: key[..open].to_string(),
                    arg: Some(key[open + 1..key.len() - 1].to_string()),
                };
            }
        }
        Self {
            tool: key.to_string(),
            arg: None,
        }
    }
}

/// On-disk shape of the managed grants file. A top-level `grants:` list of rule
/// keys keeps room for future keys and lets `deny_unknown_fields` flag typos.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantsFile {
    #[serde(default)]
    grants: Vec<String>,
}

/// The runtime's grant set: per-session (in-memory) plus persisted "always"
/// grants, with the file path to re-write on an `Always` grant.
#[derive(Debug, Default)]
pub struct FileGrantStore {
    session: HashMap<SessionId, HashSet<GrantKey>>,
    always: HashSet<GrantKey>,
    path: Option<PathBuf>,
}

impl FileGrantStore {
    /// Load the persisted `Always` grants from the managed file, resolving its
    /// path from `ENTANGLEMENT_GRANTS_FILE` or `${config_dir}/entanglement/`. A
    /// missing file is an empty store; a malformed one is logged and treated as
    /// empty (a corrupt grants file must never wedge startup, and grants only
    /// *widen* access — dropping them is the safe failure).
    pub fn load() -> Self {
        let path = grants_file_path();
        let always = match &path {
            Some(p) => read_grants(p),
            None => HashSet::new(),
        };
        Self {
            session: HashMap::new(),
            always,
            path,
        }
    }

    /// Whether a call `(tool, arg)` from `session` is already granted — an active
    /// session grant or a persisted `Always` grant. The executor consults this
    /// only when a call resolves to `Ask`, upgrading it to `Allow`.
    pub fn is_granted(&self, session: &SessionId, tool: &str, arg: Option<&str>) -> bool {
        let key = GrantKey::new(tool, arg);
        self.always.contains(&key)
            || self
                .session
                .get(session)
                .is_some_and(|set| set.contains(&key))
    }

    /// Record an approval per its [`ApprovalScope`]. `Once` records nothing;
    /// `Session` adds an in-memory grant for `session`; `Always` adds a persisted
    /// grant and re-writes the managed file (best-effort). Returns whether a new
    /// grant was stored (an already-known grant is a no-op).
    pub fn record(
        &mut self,
        session: &SessionId,
        tool: &str,
        arg: Option<&str>,
        scope: ApprovalScope,
    ) -> bool {
        let key = GrantKey::new(tool, arg);
        match scope {
            ApprovalScope::Once => false,
            ApprovalScope::Session => self.session.entry(session.clone()).or_default().insert(key),
            ApprovalScope::Always => {
                let inserted = self.always.insert(key.clone());
                if inserted {
                    self.persist(&key);
                }
                inserted
            }
        }
    }

    /// Drop a session's in-memory grants when it closes, so a reused id (there are
    /// none today, but the store outlives sessions) never sees stale approvals.
    pub fn forget_session(&mut self, session: &SessionId) {
        self.session.remove(session);
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

/// Resolve the managed grants file path: `ENTANGLEMENT_GRANTS_FILE` wins,
/// otherwise `${config_dir}/entanglement/grants.yml`. `None` when neither is
/// available (persistence then silently no-ops).
fn grants_file_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(GRANTS_FILE_ENV) {
        return Some(PathBuf::from(p));
    }
    dirs::config_dir().map(|d| d.join("entanglement").join("grants.yml"))
}

/// Read + parse the grants file at `path` into a key set. A missing file, or any
/// read/parse error, yields an empty set (logged) — grants only widen access, so
/// a corrupt file failing closed is the safe outcome.
fn read_grants(path: &Path) -> HashSet<GrantKey> {
    if !path.exists() {
        return HashSet::new();
    }
    let parsed = std::fs::read_to_string(path)
        .map_err(|e| format!("{e}"))
        .and_then(|t| serde_yaml::from_str::<GrantsFile>(&t).map_err(|e| format!("{e}")));
    match parsed {
        Ok(file) => file.grants.iter().map(|k| GrantKey::from_rule(k)).collect(),
        Err(e) => {
            tracing::warn!("ignoring malformed grants file {}: {e}", path.display());
            HashSet::new()
        }
    }
}

/// Write `grants` to `path` as the managed YAML file, creating the config dir if
/// needed. Keys are sorted so the file is stable across writes (readable diffs,
/// no churn).
fn write_grants(path: &Path, grants: &HashSet<GrantKey>) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut keys: Vec<String> = grants.iter().map(GrantKey::to_rule).collect();
    keys.sort();
    let doc = GrantsFile { grants: keys };
    let body = serde_yaml::to_string(&doc)?;
    let header = "# entanglement — persisted \"always allow\" tool grants (#174).\n\
                  # Managed by skutter: a line is appended when you approve a tool with the\n\
                  # \"always\" scope. Each entry upgrades a matching Ask to Allow (exact match on\n\
                  # tool + argument); it never overrides a Deny. Delete a line to revoke.\n";
    crate::config::atomic::atomic_write(path, &format!("{header}{body}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `GRANTS_FILE_ENV` is process-global; the tests that set it serialize here.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn tmp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("entanglement-grants-test-{name}.yml"))
    }

    #[test]
    fn grant_key_rule_roundtrips() {
        for key in [
            GrantKey::new("bash", Some("git status")),
            GrantKey::new("edit", Some("src/main.rs")),
            GrantKey::new("grep", None),
        ] {
            assert_eq!(GrantKey::from_rule(&key.to_rule()), key);
        }
        // A malformed key (no closing paren) degrades to a bare tool name.
        assert_eq!(
            GrantKey::from_rule("bash(oops"),
            GrantKey::new("bash(oops", None)
        );
    }

    #[test]
    fn once_records_nothing() {
        let mut store = FileGrantStore::default();
        let s = SessionId::new("s");
        assert!(!store.record(&s, "bash", Some("ls"), ApprovalScope::Once));
        assert!(!store.is_granted(&s, "bash", Some("ls")));
    }

    #[test]
    fn session_grant_is_scoped_to_its_session() {
        let mut store = FileGrantStore::default();
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        assert!(store.record(&a, "bash", Some("git status"), ApprovalScope::Session));
        // The granting session skips the prompt for the identical call…
        assert!(store.is_granted(&a, "bash", Some("git status")));
        // …a different command still asks, and a different session never inherits.
        assert!(!store.is_granted(&a, "bash", Some("git log")));
        assert!(!store.is_granted(&b, "bash", Some("git status")));
        // Re-recording the same grant is a no-op.
        assert!(!store.record(&a, "bash", Some("git status"), ApprovalScope::Session));
        store.forget_session(&a);
        assert!(!store.is_granted(&a, "bash", Some("git status")));
    }

    #[test]
    fn argless_grant_covers_the_whole_tool() {
        let mut store = FileGrantStore::default();
        let s = SessionId::new("s");
        store.record(&s, "grep", None, ApprovalScope::Session);
        assert!(store.is_granted(&s, "grep", None));
    }

    #[test]
    fn always_grant_persists_and_reloads_across_stores() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_path("persist");
        let _ = std::fs::remove_file(&path);
        // SAFETY: single-threaded test guarded by ENV_LOCK; scopes this store's path.
        unsafe { std::env::set_var(GRANTS_FILE_ENV, &path) };

        let s = SessionId::new("s");
        let mut store = FileGrantStore::load();
        assert!(store.record(&s, "bash", Some("git status"), ApprovalScope::Always));

        // A freshly loaded store (new process) sees the persisted grant, and it is
        // global — any session skips the prompt.
        let reloaded = FileGrantStore::load();
        assert!(reloaded.is_granted(&SessionId::new("other"), "bash", Some("git status")));
        assert!(!reloaded.is_granted(&SessionId::new("other"), "bash", Some("git log")));

        unsafe { std::env::remove_var(GRANTS_FILE_ENV) };
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn concurrent_always_grants_from_two_stores_both_survive() {
        // Two "processes" (threads, each with its own `FileGrantStore::load()`)
        // race to record *different* `Always` grants against the same on-disk
        // file (#329). Without the lock's read-current-then-merge, the second
        // writer's `std::fs::write` of its own stale `self.always` would clobber
        // the first writer's grant — a lost update. A freshly loaded third store
        // must see both.
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_path("concurrent");
        let _ = std::fs::remove_file(&path);
        unsafe { std::env::set_var(GRANTS_FILE_ENV, &path) };

        let a = std::thread::spawn(|| {
            let mut store = FileGrantStore::load();
            store.record(
                &SessionId::new("a"),
                "bash",
                Some("git status"),
                ApprovalScope::Always,
            );
        });
        let b = std::thread::spawn(|| {
            let mut store = FileGrantStore::load();
            store.record(
                &SessionId::new("b"),
                "bash",
                Some("git log"),
                ApprovalScope::Always,
            );
        });
        a.join().unwrap();
        b.join().unwrap();

        let reloaded = FileGrantStore::load();
        let any = SessionId::new("other");
        assert!(
            reloaded.is_granted(&any, "bash", Some("git status")),
            "grant recorded by the first store must survive a concurrent write"
        );
        assert!(
            reloaded.is_granted(&any, "bash", Some("git log")),
            "grant recorded by the second store must survive a concurrent write"
        );

        unsafe { std::env::remove_var(GRANTS_FILE_ENV) };
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reload_picks_up_another_process_grant() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_path("reload");
        let _ = std::fs::remove_file(&path);
        unsafe { std::env::set_var(GRANTS_FILE_ENV, &path) };

        let mut store = FileGrantStore::load();
        let session = SessionId::new("s");
        store.record(&session, "bash", Some("echo hi"), ApprovalScope::Session);

        // Another instance persists an `Always` grant directly on disk.
        let mut other = FileGrantStore::load();
        other.record(
            &SessionId::new("other"),
            "grep",
            None,
            ApprovalScope::Always,
        );

        assert!(
            !store.is_granted(&session, "grep", None),
            "stale before reload"
        );
        store.reload();
        assert!(
            store.is_granted(&session, "grep", None),
            "reload must pick up the new Always grant"
        );
        // The session-scoped grant recorded before reload is untouched.
        assert!(store.is_granted(&session, "bash", Some("echo hi")));

        unsafe { std::env::remove_var(GRANTS_FILE_ENV) };
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn malformed_grants_file_loads_empty() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_path("malformed");
        std::fs::write(&path, "grants: [oops\n").unwrap();
        // SAFETY: single-threaded test guarded by ENV_LOCK.
        unsafe { std::env::set_var(GRANTS_FILE_ENV, &path) };
        let store = FileGrantStore::load();
        assert!(!store.is_granted(&SessionId::new("s"), "bash", Some("x")));
        unsafe { std::env::remove_var(GRANTS_FILE_ENV) };
        let _ = std::fs::remove_file(&path);
    }
}
