//! Runtime-side file watcher for live-reloading definition dirs + managed
//! files (#329). Watches the same directories `layers.rs` resolves for
//! agents/skills, plus the provider catalog, user config, and the three
//! managed files (grants/agent-models/.env). On a debounced change it re-runs
//! the relevant loaders and swaps the result into the **runtime's own**
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
//! never had one) needs a restart to be picked up.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use entanglement_core::ProfileRegistry;
use notify_debouncer_mini::DebounceEventResult;
use sha2::{Digest, Sha256};

use crate::config::agent_models::AgentModelStore;
use crate::policy::DefaultGrantStore;
use crate::skills::SkillRegistry;
use crate::{agents, layers, mcp, skills, system_prompt};

/// Debounce window collapsing a burst of edits (e.g. an editor's save-as-two-
/// writes, or several files touched by one `git checkout`) into one reload.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// Env vars whose managed file, if overridden, needs its parent dir watched
/// too (the default `${config_dir}/entanglement/` catch-all below already
/// covers the un-overridden case for all five).
const MANAGED_FILE_ENVS: &[&str] = &[
    "ENTANGLEMENT_CONFIG_FILE",
    "ENTANGLEMENT_PROVIDERS_FILE",
    "ENTANGLEMENT_GRANTS_FILE",
    "ENTANGLEMENT_AGENT_MODELS_FILE",
    "ENTANGLEMENT_ENV_FILE",
];

/// The runtime-held mirrors a reload swaps. See the module doc for why this is
/// deliberately *not* core's `EngineConfig.profiles`.
#[derive(Clone)]
pub struct LiveDefinitions {
    pub profiles: Arc<RwLock<ProfileRegistry>>,
    pub skills: Arc<RwLock<Arc<SkillRegistry>>>,
    pub agent_models: Arc<Mutex<AgentModelStore>>,
    pub grants: Arc<DefaultGrantStore>,
    /// The MCP capability index (#426), captured once at startup like the
    /// config ceiling permissions — `config.yml`'s `mcp:` section is watched
    /// for the same *file content* fingerprint as everything else here, but
    /// (like the ceiling) is never itself re-parsed on a reload; only the
    /// agent/skill definition re-parse below is live. A config edit to
    /// `capabilities:` needs a restart to take effect, same as a ceiling edit.
    pub mcp_capabilities: mcp::McpCapabilityIndex,
}

/// Start the watcher for `cwd`'s resolved definition dirs + managed files.
/// `notice` receives a one-line human-readable message after each successful
/// reload (e.g. for a TUI status line); `None` sink is fine for headless heads
/// (the reload is still `tracing::info!`-logged either way). Returns `None`
/// when there is nothing to watch (no resolvable paths at all — should not
/// happen in practice since the project root always yields candidate dirs).
pub fn spawn_watcher(
    cwd: PathBuf,
    live: LiveDefinitions,
    notice: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) -> Option<tokio::task::JoinHandle<()>> {
    let paths = watch_paths(&cwd);
    // Baseline captured *before* the first debounced firing can land, so even
    // the very first wakeup is checked against real state rather than always
    // reloading unconditionally.
    let mut last_fingerprint = fingerprint(&cwd, &Fingerprint::default());
    spawn_debounced_watcher(paths, DEBOUNCE, move || {
        let fresh = fingerprint(&cwd, &last_fingerprint);
        let changed = definitions_changed(&last_fingerprint, &fresh);
        last_fingerprint = fresh;
        if !changed {
            // Two reasons a firing lands with nothing to reload: (1) a bare
            // `read()` of a watched file — which `reload()` itself does on every
            // pass — observably fires `notify` on this filesystem even though
            // nothing changed; (2) a write to a non-definition file under a
            // watched tree (e.g. a `call`/`bash` output artifact under
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
        match reload(&cwd, &live) {
            Ok(msg) => {
                tracing::info!("{msg}");
                if let Some(tx) = &notice {
                    let _ = tx.send(msg);
                }
            }
            Err(e) => tracing::warn!("definitions reload failed, keeping previous state: {e:#}"),
        }
    })
}

/// Per-file stamp: `(mtime, size, sha256-hex)`. The stat pair is a cheap cache
/// key to skip re-hashing an untouched file; the hash is the actual arbiter of
/// "did the content change" (see [`definitions_changed`]).
type FileStamp = (Option<SystemTime>, u64, String);

/// A content fingerprint of the **definition/config** files under the watched
/// trees: `path -> (mtime, size, sha256)`. Only files that actually feed a
/// loader are included (agent/skill `*.md`, managed `*.yml`/`*.yaml`/`.env`) —
/// a `call`/`bash` output artifact dropped under `.entanglement/tmp/` has none
/// of those shapes, so it never enters the map and never triggers a reload.
/// Sorted by path (a `BTreeMap`) so comparison is order-independent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Fingerprint(BTreeMap<PathBuf, FileStamp>);

/// Whether a path is a definition/config file worth hashing. Agent/skill
/// sources are `*.md`; the managed catalog/config/grants files are `*.yml`
/// (`.yaml` tolerated); the managed key file is `.env` (no extension).
fn is_definition_file(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some("md") | Some("yml") | Some("yaml") => true,
        _ => path.file_name().and_then(|n| n.to_str()) == Some(".env"),
    }
}

/// Build the fingerprint, reusing `prev`'s cached hash for any file whose
/// `(mtime, size)` is unchanged (stage 1) and only reading + SHA-256-hashing a
/// file whose stat pair moved (stage 2). A no-op `read()` — which `reload()`
/// itself does every pass — leaves `(mtime, size)` untouched, so it costs a
/// `stat()`, never a re-hash.
fn fingerprint(cwd: &Path, prev: &Fingerprint) -> Fingerprint {
    let mut map = BTreeMap::new();
    for root in watch_paths(cwd).into_iter().filter(|p| p.exists()) {
        fingerprint_into(&root, prev, &mut map);
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

/// Pure watch/debounce primitive: watches every existing path in `paths`
/// (non-existent ones are skipped — see the module doc) and calls `on_change`
/// once per debounced batch of filesystem events, however many raw events or
/// files were touched within the `debounce` window. Independently
/// unit-testable without touching any real definition loader. Returns `None`
/// (spawning nothing) when no candidate path exists.
fn spawn_debounced_watcher(
    paths: Vec<PathBuf>,
    debounce: Duration,
    mut on_change: impl FnMut() + Send + 'static,
) -> Option<tokio::task::JoinHandle<()>> {
    let existing: Vec<PathBuf> = paths.into_iter().filter(|p| p.exists()).collect();
    if existing.is_empty() {
        return None;
    }

    // `notify_debouncer_mini::new_debouncer` already runs its own background
    // `std::thread` (notify's watch API is blocking) and batches every raw
    // event that lands inside one `debounce` window into a single
    // `handle_event` call — the debounce-collapses-a-burst behavior the
    // watcher unit tests exercise. The handler forwards one signal per batch
    // over an unbounded channel; the tokio task below just relays it into
    // `on_change`, keeping the `Debouncer` guard alive (it stops on drop) for
    // as long as the task lives.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut debouncer =
        match notify_debouncer_mini::new_debouncer(debounce, move |res: DebounceEventResult| {
            match res {
                Ok(events) if !events.is_empty() => {
                    tracing::debug!(count = events.len(), "definitions watcher: debounced batch");
                    let _ = tx.send(());
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("definitions watcher error: {e:#}"),
            }
        }) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("could not start definitions watcher: {e:#}");
                return None;
            }
        };
    for p in &existing {
        if let Err(e) = debouncer
            .watcher()
            .watch(p, notify::RecursiveMode::Recursive)
        {
            tracing::warn!("could not watch {}: {e:#}", p.display());
        }
    }

    Some(tokio::spawn(async move {
        let _debouncer = debouncer; // held for the task's lifetime; stops on drop
        while rx.recv().await.is_some() {
            on_change();
        }
    }))
}

/// The resolved set of paths to watch: every candidate agent/skill dir
/// `layers.rs` would read, plus `${config_dir}/entanglement/` (covers
/// providers.yml, config.yml, grants.yml, agent-models.yml, .env in one
/// recursive watch), `<root>/.entanglement/` (covers the project config.yml),
/// and the parent dir of any `ENTANGLEMENT_*_FILE`/`ENTANGLEMENT_*_DIR`
/// override so a non-default managed-file location is still watched.
/// Deduplicated (a `BTreeSet` — several of these overlap by construction).
fn watch_paths(cwd: &Path) -> Vec<PathBuf> {
    let mut set = BTreeSet::new();
    let home = dirs::home_dir();
    let config = dirs::config_dir();

    for (kind, env) in [
        ("agents", "ENTANGLEMENT_AGENTS_DIR"),
        ("skills", "ENTANGLEMENT_SKILLS_DIR"),
    ] {
        let env_override = std::env::var_os(env).map(PathBuf::from);
        for (_, dir, _) in
            layers::candidate_dirs(cwd, kind, env_override, home.as_deref(), config.as_deref())
        {
            set.insert(dir);
        }
    }

    if let Some(cfg) = &config {
        set.insert(cfg.join("entanglement"));
    }
    set.insert(cwd.join(".entanglement"));

    for env in MANAGED_FILE_ENVS {
        if let Some(parent) = std::env::var_os(env)
            .map(PathBuf::from)
            .as_deref()
            .and_then(Path::parent)
        {
            set.insert(parent.to_path_buf());
        }
    }

    set.into_iter().collect()
}

/// Re-run the skill + agent-profile loaders, reload the grants/agent-models
/// stores from disk, and swap the result into `live` — but only if every step
/// succeeds. A malformed edit mid-save must not crash a long-running watcher
/// or wipe out a previously-good in-memory registry (unlike startup, which
/// fails fast) — log and keep serving the last-known-good state instead.
/// Returns the one-line notice on success. Only called once `spawn_watcher`'s
/// fingerprint check has already confirmed something on disk actually
/// changed, so every call here is expected to produce a real notice.
fn reload(cwd: &Path, live: &LiveDefinitions) -> anyhow::Result<String> {
    let new_skills = skills::load_registry(cwd)?;
    let mut prompt_ctx = system_prompt::PromptContext::load(cwd);
    prompt_ctx.skills = new_skills.disclosures();
    let mut new_profiles =
        agents::load_registry(cwd, &prompt_ctx, &new_skills, &live.mcp_capabilities)?;

    // Managed-file mirrors (#329): re-read whatever another skutter instance
    // may have written since this process's last load, then re-apply pins onto
    // the freshly-loaded profiles (persisted file > frontmatter, ADR-0081).
    {
        let mut agent_models = live.agent_models.lock().unwrap();
        agent_models.reload();
        agent_models.apply(&mut new_profiles);
    }
    live.grants.reload();

    let agent_count = new_profiles.iter().count();
    let skill_count = new_skills.disclosures().len();
    *live.skills.write().unwrap() = Arc::new(new_skills);
    *live.profiles.write().unwrap() = new_profiles;

    Ok(format!(
        "definitions reloaded: {agent_count} agent(s), {skill_count} skill(s) — \
         new sessions and the next agent switch see the update"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

        let fp1 = fingerprint(cwd.path(), &Fingerprint::default());
        for _ in 0..5 {
            let _ = std::fs::read_to_string(&file).unwrap();
        }
        let fp2 = fingerprint(cwd.path(), &fp1);
        assert!(
            !definitions_changed(&fp1, &fp2),
            "repeated reads of unchanged files must not count as a change"
        );

        // Some filesystems have coarse mtime resolution; make sure the write
        // below lands in a distinguishably later tick.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&file, "v2, different length").unwrap();
        let fp3 = fingerprint(cwd.path(), &fp2);
        assert!(
            definitions_changed(&fp2, &fp3),
            "an actual content change must be reflected in the fingerprint"
        );
    }

    /// The core of #1: a write to a non-definition file under a watched tree
    /// (a `call`/`bash` output artifact under `.entanglement/tmp/`) must not
    /// register as a change, and neither must a same-content re-save that only
    /// bumps a real definition file's mtime.
    #[test]
    fn non_definition_writes_and_mtime_only_touches_are_not_changes() {
        let cwd = tempfile::tempdir().unwrap();
        let agents = cwd.path().join(".entanglement").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let skill = agents.join("test.md");
        std::fs::write(&skill, "v1").unwrap();

        let fp1 = fingerprint(cwd.path(), &Fingerprint::default());

        // A command dumps output under .entanglement/tmp — not a definition file.
        let tmp = cwd
            .path()
            .join(".entanglement")
            .join("tmp")
            .join("call-output");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("call-1.stdout"), "lots of noisy output").unwrap();
        let fp2 = fingerprint(cwd.path(), &fp1);
        assert!(
            !definitions_changed(&fp1, &fp2),
            "a write to a non-definition (.stdout) file must not trigger a reload"
        );

        // Re-save the definition file with identical content, later mtime.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&skill, "v1").unwrap();
        let fp3 = fingerprint(cwd.path(), &fp2);
        assert!(
            !definitions_changed(&fp2, &fp3),
            "a same-content re-save (mtime bump only) must not trigger a reload"
        );

        // A genuine edit still does.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&skill, "v2").unwrap();
        let fp4 = fingerprint(cwd.path(), &fp3);
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

        let fp1 = fingerprint(cwd.path(), &Fingerprint::default());
        std::fs::write(agents.join("b.md"), "b").unwrap();
        let fp2 = fingerprint(cwd.path(), &fp1);
        assert!(
            definitions_changed(&fp1, &fp2),
            "adding a definition is a change"
        );

        std::fs::remove_file(&a).unwrap();
        let fp3 = fingerprint(cwd.path(), &fp2);
        assert!(
            definitions_changed(&fp2, &fp3),
            "removing a definition is a change"
        );
    }

    /// A debounce window generous enough that five back-to-back synchronous
    /// `fs::write` calls (each well under a millisecond in isolation) stay
    /// inside one window even under a heavily loaded/parallel `cargo test`
    /// run, where scheduling jitter can stretch "instant" syscalls across tens
    /// of milliseconds. `notify_debouncer_mini` emits a provisional
    /// `AnyContinuous` (in addition to the final `Any`) for a burst whose
    /// *total* span approaches the debounce window — a real flake source at a
    /// too-tight window, not a bug in the watcher.
    const TEST_DEBOUNCE: Duration = Duration::from_millis(1000);

    #[tokio::test]
    async fn one_change_fires_the_callback_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent.md");
        std::fs::write(&file, "v1").unwrap();

        let count = Arc::new(AtomicUsize::new(0));
        let counted = count.clone();
        let handle =
            spawn_debounced_watcher(vec![dir.path().to_path_buf()], TEST_DEBOUNCE, move || {
                counted.fetch_add(1, Ordering::SeqCst);
            })
            .expect("existing dir must spawn a watcher");

        std::fs::write(&file, "v2").unwrap();
        tokio::time::sleep(TEST_DEBOUNCE * 3).await;

        assert_eq!(count.load(Ordering::SeqCst), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn a_burst_of_writes_collapses_into_one_callback() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent.md");
        std::fs::write(&file, "v0").unwrap();

        let count = Arc::new(AtomicUsize::new(0));
        let counted = count.clone();
        let handle =
            spawn_debounced_watcher(vec![dir.path().to_path_buf()], TEST_DEBOUNCE, move || {
                counted.fetch_add(1, Ordering::SeqCst);
            })
            .expect("existing dir must spawn a watcher");

        // Five rapid writes, all well within the debounce window — no sleeps
        // between them — must collapse into (at most a couple of) reload
        // callbacks, not five.
        for i in 0..5 {
            std::fs::write(&file, format!("v{i}")).unwrap();
        }
        tokio::time::sleep(TEST_DEBOUNCE * 3).await;

        // Usually collapses to exactly 1. `notify_debouncer_mini` can emit a
        // provisional `AnyContinuous` plus a final `Any` (2 callbacks) when the
        // last raw event's kernel delivery lands right at the debounce
        // boundary — inherent scheduler jitter under a loaded host, not a
        // watcher bug — so the assertion allows that one extra tick while still
        // proving the burst collapsed (5 writes, nowhere near 5 callbacks).
        let fired = count.load(Ordering::SeqCst);
        assert!(
            (1..=2).contains(&fired),
            "a burst within the debounce window must collapse to 1 (occasionally 2 under \
             scheduler jitter) callbacks, not {fired}"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn missing_path_spawns_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(spawn_debounced_watcher(vec![missing], TEST_DEBOUNCE, || {}).is_none());
    }

    #[tokio::test]
    async fn empty_paths_spawns_nothing() {
        assert!(spawn_debounced_watcher(Vec::new(), TEST_DEBOUNCE, || {}).is_none());
    }

    #[test]
    fn watch_paths_is_deduped_and_nonempty() {
        let dir = tempfile::tempdir().unwrap();
        let paths = watch_paths(dir.path());
        // The project `.entanglement` dir is always a candidate, so the set is
        // never empty even on a bare tempdir with no home/config layers.
        assert!(paths.contains(&dir.path().join(".entanglement")));
        let mut sorted = paths.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(paths, sorted, "watch_paths must already be deduplicated");
    }
}
