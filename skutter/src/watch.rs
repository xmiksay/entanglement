//! `skutter`'s file watcher for live-reloading definition dirs + managed
//! files (#329; moved out of the library by #967). Watches the same
//! directories the library's `layers::candidate_dirs` resolves for
//! agents/skills/commands, plus the provider catalog, user config, and the
//! managed files (agent-models/.env). On a debounced change it re-runs
//! the relevant loaders and swaps the result into **skutter's own**
//! mirrors of the profile/skill registries — never core's `EngineConfig`,
//! which core holds immutably for the process lifetime (ADR-0081 precedent:
//! mutating a shared registry live is a rejected design, a concurrency hazard
//! for no gain). A live session's core-resolved profile is therefore
//! unaffected by a reload — new sessions, the next `SetAgent`, and the TUI
//! agent picker see the fresh state because they consult these mirrors, not a
//! frozen snapshot.
//!
//! Known v1 limitation: only directories that exist at watch-start are
//! registered (`notify` can't watch a path that isn't there yet). A dir
//! created after startup (e.g. the first `~/.claude/skills` on a machine that
//! never had one) needs a restart to be picked up. Likewise the env
//! overrides that choose those paths (`ENTANGLEMENT_*_DIR`, the state dir,
//! the `ENTANGLEMENT_*_FILE` parents) are read **once, at watcher spawn**: a
//! later change to the process env — e.g. a future live `.env` reload — moves
//! nothing here until a restart.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use entanglement_runtime::watch::spawn_debounced_watcher;
use sha2::{Digest, Sha256};

use crate::layers::{self, UserLayer};
use crate::user_dirs::{AGENTS_DIR_ENV, COMMANDS_DIR_ENV, SKILLS_DIR_ENV};

mod live;
use live::reload;
pub use live::{LiveDefinitions, ReloadNotice};

/// Debounce window collapsing a burst of edits (e.g. an editor's save-as-two-
/// writes, or several files touched by one `git checkout`) into one reload.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// Env vars whose file, if overridden, needs its parent dir watched too (the
/// default `${config_dir}/entanglement/` catch-all below already covers the
/// un-overridden case — this list only matters when one is relocated outside
/// that directory, #558). The seven managed-state files move together with
/// `ENTANGLEMENT_STATE_DIR` (ADR-0213), watched as a directory in
/// [`watch_paths`].
const MANAGED_FILE_ENVS: &[&str] = &[
    "ENTANGLEMENT_CONFIG_FILE",
    "ENTANGLEMENT_PROVIDERS_FILE",
    "ENTANGLEMENT_ENV_FILE",
];

/// Start the watcher for `cwd`'s resolved definition dirs + managed files.
/// `notice` receives a [`ReloadNotice`] after each reload attempt a content
/// change triggers — the summary on success, the error on failure (e.g. for a
/// TUI status line); `None` sink is fine for headless heads (the outcome is
/// still `tracing`-logged either way). Returns `None`
/// when there is nothing to watch (no resolvable paths at all — should not
/// happen in practice since the project root always yields candidate dirs).
pub fn spawn_watcher(
    cwd: PathBuf,
    live: LiveDefinitions,
    notice: Option<tokio::sync::mpsc::UnboundedSender<ReloadNotice>>,
) -> Option<tokio::task::JoinHandle<()>> {
    let paths = watch_paths(&cwd);
    // Baseline captured *before* the first debounced firing can land, so even
    // the very first wakeup is checked against real state rather than always
    // reloading unconditionally.
    let mut last_fingerprint = fingerprint(&paths, &Fingerprint::default());
    let fingerprinted = paths.clone();
    spawn_debounced_watcher(paths, DEBOUNCE, move || {
        let fresh = fingerprint(&fingerprinted, &last_fingerprint);
        let changed = definitions_changed(&last_fingerprint, &fresh);
        last_fingerprint = fresh;
        if !changed {
            // Two reasons a firing lands with nothing to reload: (1) a bare
            // `read()` of a watched file — which `reload()` itself does on every
            // pass — observably fires `notify` on this filesystem even though
            // nothing changed; (2) a write to a non-definition file under a
            // watched tree (e.g. a `bash` output artifact under
            // `.entanglement/tmp/`). The content fingerprint is blind to both:
            // it only tracks definition/config files and compares by SHA-256, so
            // a stray write or a same-content re-save is a cheap no-op instead
            // of a full re-parse + user-facing notice.
            tracing::debug!(
                "definitions watcher fired but no agent/skill/config content \
                 changed — skipping reload"
            );
            return;
        }
        // Independent of the definitions below: a bad skill must not pin
        // stale backup caps, nor a bad config block a definitions reload.
        if let Some(backups) = &live.file_backups {
            crate::config::undo::reapply(backups, crate::config::Config::load(&cwd));
        }
        let outcome = reload_notice(reload(&cwd, &live));
        if let Some(tx) = &notice {
            let _ = tx.send(outcome);
        }
    })
}

/// Log a [`reload`] outcome and turn it into the notice a head receives.
fn reload_notice(outcome: anyhow::Result<String>) -> ReloadNotice {
    match outcome {
        Ok(msg) => {
            tracing::info!("{msg}");
            ReloadNotice::Reloaded(msg)
        }
        Err(e) => {
            let msg = format!("definitions reload failed, keeping previous state: {e:#}");
            tracing::warn!("{msg}");
            ReloadNotice::Failed(msg)
        }
    }
}

/// Per-file stamp: `(mtime, size, sha256-hex)`. The stat pair is a cheap cache
/// key to skip re-hashing an untouched file; the hash is the actual arbiter of
/// "did the content change" (see [`definitions_changed`]).
type FileStamp = (Option<SystemTime>, u64, String);

/// A content fingerprint of the **definition/config** files under the watched
/// trees: `path -> (mtime, size, sha256)`. Only files that actually feed a
/// loader are included (see [`is_definition_file`]) — a `bash` output
/// artifact dropped under `.entanglement/tmp/` has none of those shapes, so it
/// never enters the map and never triggers a reload.
/// Sorted by path (a `BTreeMap`) so comparison is order-independent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Fingerprint(BTreeMap<PathBuf, FileStamp>);

/// Whether a path is a definition/config file worth hashing. Agent/skill
/// sources are `*.md`; of the `*.rhai` files only a skill's `lib.rhai` — a
/// scratch or trusted script feeds no loader; the managed catalog/config files are
/// `*.yml` (`.yaml` tolerated); the managed key file is `.env` (no extension).
fn is_definition_file(path: &Path) -> bool {
    // The last-seen MCP tool cache and the grants file feed no loader (a head
    // reads grants itself, per check) and are rewritten on every MCP connect /
    // `Always` approval; reloading every definition for them is pure churn.
    use crate::config::state_dir::{GRANTS_FILE, MCP_TOOLS_FILE};
    if matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some(MCP_TOOLS_FILE | GRANTS_FILE)
    ) {
        return false;
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some("md") | Some("yml") | Some("yaml") => true,
        Some("rhai") => path.file_name().is_some_and(|n| n == "lib.rhai"),
        _ => path.file_name().and_then(|n| n.to_str()) == Some(".env"),
    }
}

/// Build the fingerprint, reusing `prev`'s cached hash for any file whose
/// `(mtime, size)` is unchanged (stage 1) and only reading + SHA-256-hashing a
/// file whose stat pair moved (stage 2). A no-op `read()` — which `reload()`
/// itself does every pass — leaves `(mtime, size)` untouched, so it costs a
/// `stat()`, never a re-hash.
fn fingerprint(paths: &[PathBuf], prev: &Fingerprint) -> Fingerprint {
    let mut map = BTreeMap::new();
    for root in paths.iter().filter(|p| p.exists()) {
        fingerprint_into(root, prev, &mut map);
    }
    Fingerprint(map)
}

fn fingerprint_into(path: &Path, prev: &Fingerprint, out: &mut BTreeMap<PathBuf, FileStamp>) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            fingerprint_into(&entry.path(), prev, out);
        }
        return;
    }
    if !is_definition_file(path) {
        return;
    }
    let mtime = meta.modified().ok();
    let size = meta.len();
    // Stage 1: unchanged stat pair -> reuse the cached hash, no read.
    if let Some((pm, ps, hash)) = prev.0.get(path) {
        if *pm == mtime && *ps == size {
            out.insert(path.to_path_buf(), (mtime, size, hash.clone()));
            return;
        }
    }
    // Stage 2: new file or moved stat pair -> read + hash.
    let hash = std::fs::read(path)
        .map(|bytes| format!("{:x}", Sha256::digest(&bytes)))
        .unwrap_or_default();
    out.insert(path.to_path_buf(), (mtime, size, hash));
}

/// Whether the definition/config **content** differs between two fingerprints —
/// compared by the file set and each file's SHA-256, deliberately **ignoring**
/// mtime/size. So a same-content re-save (a `touch`, an editor rewrite with no
/// change) that bumps only mtime is *not* a change, while an add/remove of a
/// tracked file, or any content edit, is. Split out so the decision is
/// unit-testable without a real timer or a live `notify` watcher.
fn definitions_changed(old: &Fingerprint, new: &Fingerprint) -> bool {
    if old.0.len() != new.0.len() {
        return true;
    }
    // Both maps are path-sorted and equal length: a mismatched key or hash at
    // any position is a real change.
    old.0
        .iter()
        .zip(new.0.iter())
        .any(|((op, os), (np, ns))| op != np || os.2 != ns.2)
}

/// The resolved set of paths to watch: every candidate agent/skill/command dir
/// `layers.rs` would read, plus `${config_dir}/entanglement/` (covers
/// providers.yml, config.yml, agent-models.yml, agent-generation.yml,
/// aux-models.yml, extra-roots.yml, .env in one recursive watch), `<root>/.entanglement/` (covers the project
/// config.yml), the managed-state dir (`ENTANGLEMENT_STATE_DIR`, ADR-0213),
/// and the parent dir of any `ENTANGLEMENT_*_FILE` override so a non-default
/// location is still watched (#558). Deduplicated (a `BTreeSet` — several of these overlap
/// by construction).
fn watch_paths(cwd: &Path) -> Vec<PathBuf> {
    watch_paths_from(
        cwd,
        |name| std::env::var_os(name),
        crate::user_dirs::home_dir(),
        crate::user_dirs::config_dir(),
    )
}

/// [`watch_paths`] over an injected env lookup and home/config dirs, so tests
/// never touch process env or the real user dirs (#773).
fn watch_paths_from(
    cwd: &Path,
    env: impl Fn(&str) -> Option<OsString>,
    home: Option<PathBuf>,
    config: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut set = BTreeSet::new();

    for (kind, env_var) in [
        ("agents", AGENTS_DIR_ENV),
        ("skills", SKILLS_DIR_ENV),
        // User slash commands reload on this watcher's notice, so their
        // layered dirs are fingerprinted here too.
        ("commands", COMMANDS_DIR_ENV),
    ] {
        let user = match env(env_var) {
            Some(over) => UserLayer::Dir(over.into()),
            None => UserLayer::Home {
                home: home.clone(),
                config: config.clone(),
            },
        };
        for (_, dir, _) in layers::candidate_dirs(cwd, kind, &user) {
            set.insert(dir);
        }
    }

    if let Some(cfg) = &config {
        set.insert(cfg.join("entanglement"));
    }
    set.insert(cwd.join(".entanglement"));
    let default_state = || config.as_ref().map(|c| c.join("entanglement"));
    if let Some(state) = crate::config::state_dir::resolve(&env, default_state) {
        set.insert(state);
    }

    for var in MANAGED_FILE_ENVS {
        if let Some(parent) = env(var)
            .map(PathBuf::from)
            .as_deref()
            .and_then(Path::parent)
        {
            set.insert(parent.to_path_buf());
        }
    }

    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #785: a failed definitions reload still yields a notice, so the TUI
    /// re-applies its `tui:` block instead of being blocked by a bad agent.
    #[test]
    fn a_failed_reload_still_notifies() {
        let notice = reload_notice(Err(anyhow::anyhow!("bad agent file")));
        assert_eq!(
            notice,
            ReloadNotice::Failed(
                "definitions reload failed, keeping previous state: bad agent file".into()
            )
        );
        assert_eq!(
            reload_notice(Ok("reloaded".into())),
            ReloadNotice::Reloaded("reloaded".into())
        );
    }

    /// The watch set with no env overrides and no home/config dirs: the
    /// project layer only, so tests never see the real user dirs.
    fn project_paths(cwd: &Path) -> Vec<PathBuf> {
        watch_paths_from(cwd, |_| None, None, None)
    }

    #[test]
    fn fingerprint_is_stable_across_reads_but_changes_on_a_real_write() {
        // The bug this guards: on this filesystem a bare `read()` of a
        // watched file observably fires `notify` even though nothing
        // changed — and `reload()` itself reads every watched file on every
        // pass, so without a pre-check the watcher perpetually re-triggers
        // itself forever (reload -> reads -> fires notify -> reload -> ...),
        // reported as an infinite chain of reload notices. `fingerprint()`
        // must be blind to that: unchanged after repeated reads, but must
        // still catch a genuine content change.
        let cwd = tempfile::tempdir().unwrap();
        let dir = cwd.path().join(".entanglement").join("agents");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("test.md");
        std::fs::write(&file, "v1").unwrap();

        let fp1 = fingerprint(&project_paths(cwd.path()), &Fingerprint::default());
        for _ in 0..5 {
            let _ = std::fs::read_to_string(&file).unwrap();
        }
        let fp2 = fingerprint(&project_paths(cwd.path()), &fp1);
        assert!(
            !definitions_changed(&fp1, &fp2),
            "repeated reads of unchanged files must not count as a change"
        );

        // Some filesystems have coarse mtime resolution; make sure the write
        // below lands in a distinguishably later tick.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&file, "v2, different length").unwrap();
        let fp3 = fingerprint(&project_paths(cwd.path()), &fp2);
        assert!(
            definitions_changed(&fp2, &fp3),
            "an actual content change must be reflected in the fingerprint"
        );
    }

    /// Only a `lib.rhai` is a definition; a scratch script or a
    /// trusted-folder script under a watched tree is not.
    #[test]
    fn only_lib_rhai_counts_among_rhai_files() {
        let cwd = tempfile::tempdir().unwrap();
        let ent = cwd.path().join(".entanglement");
        for dir in ["tmp", "skills/git/scripts"] {
            std::fs::create_dir_all(ent.join(dir)).unwrap();
        }
        let fp1 = fingerprint(&project_paths(cwd.path()), &Fingerprint::default());
        std::fs::write(ent.join("tmp/scratch.rhai"), "1").unwrap();
        std::fs::write(ent.join("skills/git/scripts/run.rhai"), "1").unwrap();
        let fp2 = fingerprint(&project_paths(cwd.path()), &fp1);
        assert!(
            !definitions_changed(&fp1, &fp2),
            "scripts are not definitions"
        );
        std::fs::write(ent.join("skills/git/lib.rhai"), "fn f() { 1 }").unwrap();
        let fp3 = fingerprint(&project_paths(cwd.path()), &fp2);
        assert!(definitions_changed(&fp2, &fp3), "a skill lib is");
        assert!(!is_definition_file(Path::new(
            "/c/entanglement/rhai/modules/a/b.rhai"
        )));
        assert!(!is_definition_file(Path::new("/p/rhai/x.rhai")));
    }

    /// Neither the grants file nor the last-seen MCP tool cache feeds a
    /// loader (#966): an `Always` approval or an MCP connect must not reload.
    #[test]
    fn managed_files_no_loader_reads_are_not_definitions() {
        assert!(!is_definition_file(Path::new("/c/entanglement/grants.yml")));
        assert!(!is_definition_file(Path::new(
            "/c/entanglement/mcp-tools.yml"
        )));
        assert!(is_definition_file(Path::new(
            "/c/entanglement/aux-models.yml"
        )));
    }

    /// The core of #1: a write to a non-definition file under a watched tree
    /// (a `bash` output artifact under `.entanglement/tmp/`) must not
    /// register as a change, and neither must a same-content re-save that only
    /// bumps a real definition file's mtime.
    #[test]
    fn non_definition_writes_and_mtime_only_touches_are_not_changes() {
        let cwd = tempfile::tempdir().unwrap();
        let agents = cwd.path().join(".entanglement").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let skill = agents.join("test.md");
        std::fs::write(&skill, "v1").unwrap();

        let fp1 = fingerprint(&project_paths(cwd.path()), &Fingerprint::default());

        // A command dumps output under .entanglement/tmp — not a definition file.
        let tmp = cwd
            .path()
            .join(".entanglement")
            .join("tmp")
            .join("call-output");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("call-1.stdout"), "lots of noisy output").unwrap();
        let fp2 = fingerprint(&project_paths(cwd.path()), &fp1);
        assert!(
            !definitions_changed(&fp1, &fp2),
            "a write to a non-definition (.stdout) file must not trigger a reload"
        );

        // Re-save the definition file with identical content, later mtime.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&skill, "v1").unwrap();
        let fp3 = fingerprint(&project_paths(cwd.path()), &fp2);
        assert!(
            !definitions_changed(&fp2, &fp3),
            "a same-content re-save (mtime bump only) must not trigger a reload"
        );

        // A genuine edit still does.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&skill, "v2").unwrap();
        let fp4 = fingerprint(&project_paths(cwd.path()), &fp3);
        assert!(
            definitions_changed(&fp3, &fp4),
            "a real content edit must trigger a reload"
        );
    }

    #[test]
    fn adding_or_removing_a_definition_file_is_a_change() {
        let cwd = tempfile::tempdir().unwrap();
        let agents = cwd.path().join(".entanglement").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let a = agents.join("a.md");
        std::fs::write(&a, "a").unwrap();

        let fp1 = fingerprint(&project_paths(cwd.path()), &Fingerprint::default());
        std::fs::write(agents.join("b.md"), "b").unwrap();
        let fp2 = fingerprint(&project_paths(cwd.path()), &fp1);
        assert!(
            definitions_changed(&fp1, &fp2),
            "adding a definition is a change"
        );

        std::fs::remove_file(&a).unwrap();
        let fp3 = fingerprint(&project_paths(cwd.path()), &fp2);
        assert!(
            definitions_changed(&fp2, &fp3),
            "removing a definition is a change"
        );

        // A slash-command file under a foreign dir is a definition too (#733).
        let commands = cwd.path().join(".claude").join("commands");
        std::fs::create_dir_all(&commands).unwrap();
        std::fs::write(commands.join("review.md"), "Review $1").unwrap();
        let fp4 = fingerprint(&project_paths(cwd.path()), &fp3);
        assert!(
            definitions_changed(&fp3, &fp4),
            "adding a command file is a change"
        );
    }

    #[test]
    fn watch_paths_is_deduped_and_nonempty() {
        let dir = tempfile::tempdir().unwrap();
        let paths = project_paths(dir.path());
        // The project `.entanglement` dir is always a candidate, so the set is
        // never empty even on a bare tempdir with no home/config layers.
        assert!(paths.contains(&dir.path().join(".entanglement")));
        // User slash commands live-reload too (#733), opencode's dir included.
        assert!(paths.contains(&dir.path().join(".entanglement").join("commands")));
        assert!(paths.contains(&dir.path().join(".opencode").join("command")));
        let mut sorted = paths.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(paths, sorted, "watch_paths must already be deduplicated");
    }

    /// #558/ADR-0213: a relocated managed-state dir is watched itself, so
    /// a managed file written outside `${config_dir}/entanglement/` still
    /// live-reloads.
    #[test]
    fn watch_paths_includes_a_relocated_state_dir() {
        let dir = tempfile::tempdir().unwrap();
        let custom = dir.path().join("elsewhere");
        let env = |name: &str| {
            (name == crate::config::state_dir::STATE_DIR_ENV).then(|| custom.clone().into())
        };
        let paths = watch_paths_from(dir.path(), env, None, Some("/cfg".into()));
        assert!(
            paths.contains(&custom),
            "a relocated ENTANGLEMENT_STATE_DIR must be watched"
        );
        assert!(paths.contains(&PathBuf::from("/cfg/entanglement")));
    }

    /// The user layers and a relocated managed file's parent are watched too.
    #[test]
    fn watch_paths_covers_user_layers_and_relocated_file_parents() {
        let dir = tempfile::tempdir().unwrap();
        let env = |name: &str| (name == "ENTANGLEMENT_ENV_FILE").then(|| "/keys/x.env".into());
        let paths = watch_paths_from(dir.path(), env, Some("/home/u".into()), None);
        assert!(paths.contains(&PathBuf::from("/home/u/.claude/skills")));
        assert!(paths.contains(&PathBuf::from("/keys")));
    }
}
