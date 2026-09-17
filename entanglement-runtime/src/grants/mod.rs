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
//!   (`${config_dir}/entanglement/grants.yml`, override `ENTANGLEMENT_GRANTS_FILE`),
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
//!   prompted call with `[d]`. Deliberately **not** mode-scoped (ADR-0207
//!   §8 names `GrantKey`, not this store): every mode with a human present
//!   to approve it allows the read-only triad by `default` or `allow`
//!   (`auto`'s unattended `default: deny` never reaches an approval prompt
//!   to widen in the first place, per §11's question-timeout-as-denial), so
//!   the cross-mode leak this ADR closes for `write`-capable grants doesn't
//!   apply here. Revisit if a custom mode ever wants its own read-only
//!   posture.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use entanglement_core::{ApprovalScope, SessionId};
use serde::{Deserialize, Serialize};

/// Env var overriding the managed grants file path (tests + non-XDG setups).
const GRANTS_FILE_ENV: &str = "ENTANGLEMENT_GRANTS_FILE";

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

/// On-disk shape of the managed grants file. A top-level `grants:` list keeps
/// room for future keys and lets `deny_unknown_fields` flag typos.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantsFile {
    #[serde(default)]
    grants: Vec<GrantEntry>,
}

/// One line of the grants file: a **scoped** entry (mode + rule, written by
/// every grant this stage records) or a bare rule **string** — a pre-ADR-0207
/// file, kept parseable so an existing user's file doesn't error out, but
/// loaded with `mode: None` so it never matches a live call (ADR-0207 §8:
/// "existing grants.yml entries have no mode; treat them as non-matching").
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum GrantEntry {
    Scoped { mode: String, rule: String },
    Legacy(String),
}

/// The runtime's grant set: per-session (in-memory) plus persisted "always"
/// grants, with the file path to re-write on an `Always` grant. `session_dirs`
/// is the [`ApprovalScope::SessionDir`] store (#486, ADR-0126): a session-only,
/// never-persisted set of directories (root-relative, #485) that widen the
/// read-only triad (`read`/`grep`/`glob`) instead of matching one exact call.
#[derive(Debug, Default)]
pub struct FileGrantStore {
    session: HashMap<SessionId, HashSet<GrantKey>>,
    session_dirs: HashMap<SessionId, BTreeSet<String>>,
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
            session_dirs: HashMap::new(),
            always,
            path,
        }
    }

    /// Whether a call `(tool, arg)` from `session` under `mode` is already
    /// granted — an active session grant, a persisted `Always` grant, or (for
    /// the read-only triad, unscoped by mode — see the module docs) a
    /// [`ApprovalScope::SessionDir`] directory grant covering `arg` (#486).
    /// The executor consults this only when a call resolves to `Ask`,
    /// upgrading it to `Allow`. `mode` is matched exactly (ADR-0207 §8): a
    /// grant earned in one mode never fires in another.
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
            if let (Some(dirs), Some(arg)) = (self.session_dirs.get(session), arg) {
                return dirs.iter().any(|dir| dir_covers(dir, arg));
            }
        }
        false
    }

    /// Record an approval per its [`ApprovalScope`], tagged with the mode it
    /// was earned in (ADR-0207 §8). `Once` records nothing; `Session` adds an
    /// in-memory grant for `session`; `Always` adds a persisted grant and
    /// re-writes the managed file (best-effort); `SessionDir` (#486) derives
    /// the directory `(tool, arg)` implies (see [`dir_for`]) and widens the
    /// read-only triad under it for the rest of the session, unscoped by mode
    /// (see the module docs) — on any other tool, or a call `dir_for` can't
    /// derive a directory from, it degrades to an exact `Session` grant
    /// instead of widening. Returns whether a new grant was stored (an
    /// already-known grant is a no-op).
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
                            .insert(dir);
                    }
                }
                let key = GrantKey::new(tool, arg, mode);
                self.session.entry(session.clone()).or_default().insert(key)
            }
        }
    }

    /// Grant `dir` to `session` for the read-only triad (`read`/`grep`/`glob`)
    /// — the TUI `/allow <path>` command's entry point (#486, ADR-0126). `dir`
    /// is lexically normalized ([`crate::permission_path::normalize_lexical`],
    /// #485) before storage; returns the normalized form for the caller's
    /// confirmation status line. Never persisted — a directory grant is
    /// session-only by design, unlike the exact-match `Always` scope above.
    pub fn grant_session_dir(&mut self, session: &SessionId, dir: &str) -> String {
        let normalized = crate::permission_path::normalize_lexical(dir);
        self.session_dirs
            .entry(session.clone())
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
        Ok(file) => file
            .grants
            .iter()
            .map(|entry| match entry {
                GrantEntry::Scoped { mode, rule } => GrantKey::from_rule(rule, Some(mode.clone())),
                // Pre-ADR-0207: no mode was ever recorded — `mode: None`
                // never matches a live call (module docs).
                GrantEntry::Legacy(rule) => GrantKey::from_rule(rule, None),
            })
            .collect(),
        Err(e) => {
            tracing::warn!("ignoring malformed grants file {}: {e}", path.display());
            HashSet::new()
        }
    }
}

/// Write `grants` to `path` as the managed YAML file, creating the config dir if
/// needed. Keys are sorted so the file is stable across writes (readable diffs,
/// no churn). A key with `mode: None` (a pre-ADR-0207 entry this process never
/// re-earned) round-trips as its original bare-string spelling rather than
/// being force-upgraded to a fabricated mode.
fn write_grants(path: &Path, grants: &HashSet<GrantKey>) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut keys: Vec<(String, GrantEntry)> = grants
        .iter()
        .map(|key| {
            let rule = key.to_rule();
            let entry = match &key.mode {
                Some(mode) => GrantEntry::Scoped {
                    mode: mode.clone(),
                    rule: rule.clone(),
                },
                None => GrantEntry::Legacy(rule.clone()),
            };
            (rule, entry)
        })
        .collect();
    keys.sort_by(|a, b| a.0.cmp(&b.0));
    let doc = GrantsFile {
        grants: keys.into_iter().map(|(_, e)| e).collect(),
    };
    let body = serde_yaml::to_string(&doc)?;
    let header = "# entanglement — persisted \"always allow\" tool grants (#174).\n\
                  # Managed by skutter: a line is appended when you approve a tool with the\n\
                  # \"always\" scope. Each entry upgrades a matching Ask to Allow (exact match on\n\
                  # tool + argument); it never overrides a Deny. Delete a line to revoke.\n";
    crate::config::atomic::atomic_write(path, &format!("{header}{body}"))
}

// SessionDir directory derivation/coverage (#486, ADR-0126) — split out of
// this (previously grandfathered-over-cap) file into its own module: a
// self-contained unit with no dependency on `GrantKey`/mode-scoping.
mod session_dir;
use session_dir::{dir_covers, dir_for};

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
    // Deliberately unscoped by mode (module docs) -- a `SessionDir` grant
    // covers the read-only triad under any mode.

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
        // Unscoped by mode: the same grant covers a different mode too.
        assert!(store.is_granted(&s, "read", Some("src/a.rs"), "research"));
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
        assert_eq!(store.grant_session_dir(&s, "./src/"), "src".to_string());
        assert!(store.is_granted(&s, "read", Some("src/a.rs"), "build"));
        assert!(store.is_granted(&s, "grep", Some("src/a.rs"), "build"));
        assert!(store.is_granted(&s, "glob", Some("src/*.rs"), "build"));
        assert!(!store.is_granted(&s, "edit", Some("src/a.rs"), "build"));
    }

    #[test]
    fn grant_session_dir_dot_covers_every_relative_arg() {
        let mut store = FileGrantStore::default();
        let s = SessionId::new("s");
        store.grant_session_dir(&s, ".");
        assert!(store.is_granted(&s, "read", Some("anything/at/all.rs"), "build"));
        assert!(store.is_granted(&s, "read", Some("top_level.rs"), "build"));
    }

    #[test]
    fn always_grant_persists_and_reloads_across_stores() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_path("persist");
        let _ = std::fs::remove_file(&path);
        // SAFETY: the var is only read by the *default* file store, which this test
        // does not construct; the guard above serializes against sibling modules.
        unsafe { std::env::set_var(GRANTS_FILE_ENV, &path) };

        let s = SessionId::new("s");
        let mut store = FileGrantStore::load();
        assert!(store.record(
            &s,
            "bash",
            Some("git status"),
            ApprovalScope::Always,
            "build"
        ));

        // A freshly loaded store (new process) sees the persisted grant, and it is
        // global -- any session under the same mode skips the prompt.
        let reloaded = FileGrantStore::load();
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

        unsafe { std::env::remove_var(GRANTS_FILE_ENV) };
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn concurrent_always_grants_from_two_stores_both_survive() {
        // Two "processes" (threads, each with its own `FileGrantStore::load()`)
        // race to record *different* `Always` grants against the same on-disk
        // file (#329). Without the lock's read-current-then-merge, the second
        // writer's `std::fs::write` of its own stale `self.always` would clobber
        // the first writer's grant -- a lost update. A freshly loaded third store
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
                "build",
            );
        });
        let b = std::thread::spawn(|| {
            let mut store = FileGrantStore::load();
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

        let reloaded = FileGrantStore::load();
        let any = SessionId::new("other");
        assert!(
            reloaded.is_granted(&any, "bash", Some("git status"), "build"),
            "grant recorded by the first store must survive a concurrent write"
        );
        assert!(
            reloaded.is_granted(&any, "bash", Some("git log"), "build"),
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
        store.record(
            &session,
            "bash",
            Some("echo hi"),
            ApprovalScope::Session,
            "build",
        );

        // Another instance persists an `Always` grant directly on disk.
        let mut other = FileGrantStore::load();
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
        assert!(!store.is_granted(&SessionId::new("s"), "bash", Some("x"), "build"));
        unsafe { std::env::remove_var(GRANTS_FILE_ENV) };
        let _ = std::fs::remove_file(&path);
    }

    /// ADR-0207 §8: a grants file written before this stage has no `mode` on
    /// its entries -- loaded as `mode: None`, which never matches a live
    /// call's `Some(mode)`, so a legacy `Always` grant is silently inert
    /// rather than re-honored under whatever mode a call happens to run in.
    #[test]
    fn legacy_mode_less_grant_never_matches_a_live_call() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_path("legacy");
        std::fs::write(&path, "grants:\n  - bash(git status)\n").unwrap();
        // SAFETY: single-threaded test guarded by ENV_LOCK.
        unsafe { std::env::set_var(GRANTS_FILE_ENV, &path) };
        let store = FileGrantStore::load();
        assert!(!store.is_granted(&SessionId::new("s"), "bash", Some("git status"), "build"));
        assert!(!store.is_granted(&SessionId::new("s"), "bash", Some("git status"), "research"));
        unsafe { std::env::remove_var(GRANTS_FILE_ENV) };
        let _ = std::fs::remove_file(&path);
    }

    /// A grant recorded by this stage round-trips through the on-disk
    /// `{mode, rule}` shape and is readable back by a fresh store.
    #[test]
    fn scoped_grant_round_trips_through_the_file() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_path("scoped-roundtrip");
        let _ = std::fs::remove_file(&path);
        unsafe { std::env::set_var(GRANTS_FILE_ENV, &path) };

        let mut store = FileGrantStore::load();
        store.record(
            &SessionId::new("s"),
            "bash",
            Some("git status"),
            ApprovalScope::Always,
            "research",
        );
        let reloaded = FileGrantStore::load();
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

        unsafe { std::env::remove_var(GRANTS_FILE_ENV) };
        let _ = std::fs::remove_file(&path);
    }
}
