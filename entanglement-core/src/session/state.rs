//! The per-session mutable state: the [`Session`] struct, its running usage
//! tally, and the constructors/rebind helper. Split out of the module root
//! (#323) so the loop in [`super`] and the state definition read on their own.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use tokio::sync::broadcast;

use super::TurnState;
use crate::context::Context;
use crate::protocol::{AgentProfile, OutEvent, SessionId};
use crate::EngineConfig;
use entanglement_provider::{GenerationParams, Llm, ResolvedModel};

/// Tracks files an agent has seen in a session (the file-touch gate,
/// ADR-0142). Maps canonical path -> modification timestamp (`None` means the
/// file didn't exist when it was touched). A file is considered "touched" if:
///
/// 1. The `read` tool was called on it successfully (capturing its mtime), or
/// 2. The agent modified it in this session via `edit`/`write`/`apply_patch`.
///
/// Write-eligible tools (`edit`/`write`/`apply_patch`) check this gate before
/// executing: a file that hasn't been touched, or has been modified externally
/// (mtime differs), is rejected with a clear error asking the agent to read
/// first.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TouchedFiles {
    /// Maps canonical path -> modification timestamp (None = file didn't exist when touched)
    touched: HashMap<String, Option<u64>>,
}

impl TouchedFiles {
    /// Mark a file as touched with its modification timestamp (milliseconds since
    /// Unix epoch). `None` means the file didn't exist at the time of the touch.
    pub fn mark_touched(&mut self, path: String, mtime: Option<u64>) {
        self.touched.insert(path, mtime);
    }

    /// Whether the file has been touched in this session.
    pub fn is_touched(&self, path: &str) -> bool {
        self.touched.contains_key(path)
    }

    /// Get the known modification timestamp for a file, if any.
    pub fn get_known_mtime(&self, path: &str) -> Option<u64> {
        self.touched.get(path).copied().flatten()
    }

    /// Whether the file's current modification timestamp matches what we
    /// recorded when it was touched. Returns true only if the timestamps match
    /// exactly or both are `None` (file didn't exist then and still doesn't).
    pub fn matches_current(&self, path: &str, current_mtime: Option<u64>) -> bool {
        match (self.get_known_mtime(path), current_mtime) {
            (Some(known), Some(current)) => known == current,
            (None, None) => true, // Both didn't exist
            _ => false,           // Existence changed
        }
    }
}

/// Mutable per-session loop + turn state (#61). Holds the conversation
/// [`Context`], the provider LLM backend (`llm`, a plain `Box<dyn Llm>` — the
/// resilience state it references is keyed per endpoint in the provider, not per
/// session, so there is no session-scoped handle to wrap it, #195/ADR-0062), the
/// active profile,
/// and the emit sequence — nothing pointing at the filesystem or a fixed tool
/// set. Plan/task snapshots are the runtime's display state, not engine state
/// (#231, ADR-0049), so the session carries neither. The tool schemas advertised
/// to the model are config, not session state: they come from
/// [`EngineConfig::tool_specs`] at turn time (see [`super::turn`]).
pub struct Session {
    pub ctx: Context,
    pub llm: Box<dyn Llm>,
    pub profile: AgentProfile,
    /// Effective model id when the user switched model/provider mid-session
    /// (#218), overriding the profile's pinned [`AgentProfile::model`] on every
    /// request and in pricing. `None` keeps the profile's model (the startup
    /// default). Set by [`SessionCmd::SetModel`][super::SessionCmd]; reset only by
    /// another switch.
    pub model: Option<String>,
    /// Catalog provider name the session's [`llm`][Self::llm] is currently bound
    /// to (#323, ADR-0081). Tracked so a per-profile pin re-bind on `SetAgent`
    /// can no-op when the target `(provider, model)` already matches the live
    /// binding — a child spawned straight onto its pinned endpoint never
    /// rebuilds. `None` until the first pin/switch (the startup default, whose
    /// provider name core is not told).
    pub provider: Option<String>,
    /// Per-profile model choices made via
    /// [`SetModel`][super::SessionCmd::SetModel] this session (#323, ADR-0081):
    /// profile name → the resolved `(provider, model)`. This is the
    /// session-memory layer that wins over a profile's static
    /// [`model_pin`][crate::protocol::AgentProfile::model_pin] when `SetAgent`
    /// switches back to that profile, so a live `/model` choice sticks per profile
    /// for the life of the session. Reconstructed on replay from the
    /// [`ModelChanged`][crate::protocol::OutEvent::ModelChanged] records.
    pub profile_models: HashMap<String, (String, String)>,
    /// Effective generation knobs for the active model (#218). Seeded from
    /// [`EngineConfig::generation`][crate::EngineConfig] at creation and replaced
    /// on a model switch so temperature / max-output / thinking follow the model.
    pub generation: Option<GenerationParams>,
    /// Per-profile generation choices made via
    /// [`SetGeneration`][super::SessionCmd::SetGeneration] this session (#374,
    /// ADR-0094) — the generation-parameter analogue of
    /// [`profile_models`][Self::profile_models] (#323, ADR-0081). Keyed by
    /// profile name, holding the **full** merged effective params (not a partial
    /// override), so a `SetAgent` switch back to that profile re-applies it
    /// verbatim, winning over the profile's persisted/catalog default.
    /// Reconstructed on replay from
    /// [`GenerationChanged`][crate::protocol::OutEvent::GenerationChanged]
    /// records.
    pub profile_generation: HashMap<String, GenerationParams>,
    /// Monotonic per-session emit counter, shared (`Arc<AtomicU64>`, #157) with
    /// the supervisor's seq registry so runtime-authored events minted while this
    /// session is parked (an approval `ToolRequest`, a `Plan`/`TaskList` snapshot,
    /// a `FileChange`) draw a fresh seq from the *same* sequence via
    /// [`Holly::emit_for_session`][crate::Holly] — keeping `(session, seq)` unique
    /// instead of reusing the parked `ToolExec` seq (the pre-#157 defect).
    pub seq: Arc<AtomicU64>,
    pub parent: Option<SessionId>,
    /// The sessions this one spawned as sub-agents (#child-lineage): the live
    /// children of this session, appended on
    /// [`ChildSpawned`][super::SessionCmd::ChildSpawned] and pruned on
    /// [`ChildClosed`][super::SessionCmd::ChildClosed]. The inverse of
    /// [`parent`][Self::parent]; the supervisor's `parent_links` map stays the
    /// authoritative tree, this is the per-session mirror. Reconstructed on
    /// replay by inverting the `parent` edges in the shared root log.
    pub children: Vec<SessionId>,
    /// The session this one **succeeds** (#compact-successor, ADR-0110): set when
    /// this session is the copy-on-write fork of a `/compact` on `predecessor`.
    /// Unlike [`parent`][Self::parent] it is *not* a live spawn edge — the
    /// predecessor's interactive session is closed once the successor starts — so
    /// it never joins the spawn sub-tree or the permission ancestor clamp.
    /// Reconstructed on replay from
    /// [`SessionStarted`][crate::protocol::OutEvent::SessionStarted].
    pub predecessor: Option<SessionId>,
    /// Cumulative token usage + cost across every model round-trip this session
    /// has run (#192). Each `LlmEvent::Finish` folds its normalized `Usage` in
    /// here and emits the per-round-trip delta as [`OutEvent::Usage`].
    pub usage: SessionUsage,
    /// The in-flight turn (#270, ADR-0061): `Some` while a turn is live —
    /// streaming or parked on unresolved tool calls — `None` when idle.
    /// Serde-capable so an embedder can persist a suspended-mid-turn session
    /// (via the event log + replay) and resolve the pending calls against its
    /// own state.
    pub turn: Option<TurnState>,
    /// Files this agent has seen or modified (the file-touch gate, ADR-0142).
    /// Tracks what files the agent has read or modified so we can reject blind
    /// writes to unchanged or externally-modified files.
    pub touched_files: TouchedFiles,
}

/// Running per-session usage tally (#192): the sum of every round-trip's
/// normalized token counts plus the accrued dollar cost. Kept in the engine so a
/// session total survives across turns; heads reconstruct the same total by
/// accumulating the per-round-trip [`OutEvent::Usage`] deltas.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
}

impl Session {
    /// Creates a new empty session with the given configuration and profile.
    pub fn new_empty(cfg: &EngineConfig, profile: AgentProfile) -> Self {
        Self {
            // Budget the history against the active model's real context window
            // (#178), not a fixed Anthropic-shaped ceiling.
            ctx: Context::with_window(cfg.context_window),
            llm: (cfg.llm_factory)(),
            profile,
            model: None,
            provider: None,
            profile_models: HashMap::new(),
            generation: cfg.generation,
            profile_generation: HashMap::new(),
            seq: Arc::new(AtomicU64::new(0)),
            parent: None,
            children: Vec::new(),
            predecessor: None,
            usage: SessionUsage::default(),
            turn: None,
            touched_files: TouchedFiles::default(),
        }
    }

    /// Accessor for this session's touched-files state (file-touch gate, ADR-0142).
    pub fn touched_files(&self) -> &TouchedFiles {
        &self.touched_files
    }

    /// Mutable accessor for this session's touched-files state (file-touch gate, ADR-0142).
    pub fn touched_files_mut(&mut self) -> &mut TouchedFiles {
        &mut self.touched_files
    }

    /// Apply a re-resolved model to this session and announce it (#323, ADR-0081
    /// — the factored-out `SetModel` success arm, #218). Rebuilds the backend,
    /// retargets the effective model + generation + context-window budget, tracks
    /// the bound [`provider`][Self::provider] (for the pin no-op guard), and emits
    /// [`OutEvent::ModelChanged`]. The single locus the live `SetModel` switch,
    /// the per-profile pin re-bind on `SetAgent`, and the session-start pin all
    /// funnel through.
    pub(super) fn rebind(
        &mut self,
        session: &SessionId,
        resolved: ResolvedModel,
        events: &broadcast::Sender<OutEvent>,
    ) {
        self.provider = Some(resolved.provider.clone());
        self.model = Some(resolved.model.clone());
        self.generation = resolved.generation;
        self.ctx.set_window(resolved.context_window);
        self.llm = (resolved.llm_factory)();
        let _ = events.send(OutEvent::ModelChanged {
            session: session.clone(),
            provider: resolved.provider,
            model: resolved.model,
            context_window: resolved.context_window,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_touched_files_default_is_empty() {
        let touched = TouchedFiles::default();
        assert!(!touched.is_touched("any/path"));
        assert!(touched.get_known_mtime("any/path").is_none());
    }

    #[test]
    fn test_touched_files_mark_and_check() {
        let mut touched = TouchedFiles::default();

        assert!(!touched.is_touched("test/path"));

        touched.mark_touched("test/path".to_string(), Some(12345));

        assert!(touched.is_touched("test/path"));
        assert_eq!(touched.get_known_mtime("test/path"), Some(12345));
    }

    #[test]
    fn test_touched_files_matches_current_same_mtime() {
        let mut touched = TouchedFiles::default();
        touched.mark_touched("test/path".to_string(), Some(12345));

        assert!(touched.matches_current("test/path", Some(12345)));
    }

    #[test]
    fn test_touched_files_matches_current_different_mtime() {
        let mut touched = TouchedFiles::default();
        touched.mark_touched("test/path".to_string(), Some(12345));

        assert!(!touched.matches_current("test/path", Some(99999)));
    }

    #[test]
    fn test_touched_files_matches_current_both_none() {
        let mut touched = TouchedFiles::default();
        touched.mark_touched("nonexistent/path".to_string(), None);

        assert!(touched.matches_current("nonexistent/path", None));
    }

    #[test]
    fn test_touched_files_matches_current_existence_changed() {
        let mut touched = TouchedFiles::default();

        // File didn't exist when touched, now exists
        touched.mark_touched("test/path".to_string(), None);
        assert!(!touched.matches_current("test/path", Some(12345)));

        // File existed when touched, now doesn't
        touched.mark_touched("test/path2".to_string(), Some(12345));
        assert!(!touched.matches_current("test/path2", None));
    }

    #[test]
    fn test_touched_files_multiple_paths() {
        let mut touched = TouchedFiles::default();

        touched.mark_touched("path1".to_string(), Some(111));
        touched.mark_touched("path2".to_string(), Some(222));
        touched.mark_touched("path3".to_string(), None);

        assert!(touched.is_touched("path1"));
        assert!(touched.is_touched("path2"));
        assert!(touched.is_touched("path3"));
        assert!(!touched.is_touched("path4"));

        assert_eq!(touched.get_known_mtime("path1"), Some(111));
        assert_eq!(touched.get_known_mtime("path2"), Some(222));
        assert_eq!(touched.get_known_mtime("path3"), None);
    }

    #[test]
    fn test_touched_files_update_overwrites() {
        let mut touched = TouchedFiles::default();

        touched.mark_touched("test/path".to_string(), Some(12345));
        assert_eq!(touched.get_known_mtime("test/path"), Some(12345));

        // Overwrite with new mtime
        touched.mark_touched("test/path".to_string(), Some(67890));
        assert_eq!(touched.get_known_mtime("test/path"), Some(67890));
    }
}
