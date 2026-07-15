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

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use entanglement_core::ProfileRegistry;
use notify_debouncer_mini::DebounceEventResult;

use crate::config::agent_models::AgentModelStore;
use crate::policy::DefaultGrantStore;
use crate::skills::SkillRegistry;
use crate::{agents, layers, skills, system_prompt};

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
    spawn_debounced_watcher(paths, DEBOUNCE, move || match reload(&cwd, &live) {
        Ok(msg) => {
            tracing::info!("{msg}");
            if let Some(tx) = &notice {
                let _ = tx.send(msg);
            }
        }
        Err(e) => tracing::warn!("definitions reload failed, keeping previous state: {e:#}"),
    })
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
/// Returns the one-line notice on success.
fn reload(cwd: &Path, live: &LiveDefinitions) -> anyhow::Result<String> {
    let new_skills = skills::load_registry(cwd)?;
    let mut prompt_ctx = system_prompt::PromptContext::load(cwd);
    prompt_ctx.skills = new_skills.disclosures();
    let mut new_profiles = agents::load_registry(cwd, &prompt_ctx, &new_skills)?;

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
