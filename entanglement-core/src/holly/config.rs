//! Engine configuration + agent-profile registry: the immutable inputs an
//! embedder hands [`Holly::spawn`][super::Holly::spawn]. Kept separate from the
//! supervisor loop so the config/profile surface reads on its own.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::protocol::{AgentMode, AgentProfile, Permission, PermissionProfile, SessionId};
use entanglement_provider::{
    EchoLlm, GenerationParams, Llm, LlmFactory, ModelPricing, ModelResolver, ToolSpec,
};

use super::DEFAULT_PROFILE;

/// Resolves the base tool schemas advertised to the model for a specific
/// session (#308). Its output **replaces** the engine-global
/// [`EngineConfig::tool_specs`][EngineConfig::tool_specs] for that session (the
/// per-profile [`profile_tool_specs`][EngineConfig::profile_tool_specs] are
/// still appended, and the active profile's mask still filters both). Consulted
/// fresh at every turn build, so an embedder that mutates its backing store —
/// e.g. a per-user MCP-server set — sees the change on the *next* turn without
/// respawning the engine. The `Fn` is intentionally sync: an embedder keeps a
/// snapshot cache (`Arc<RwLock<HashMap<SessionId, Vec<ToolSpec>>>>`) hydrated
/// from its store rather than doing I/O on the turn path.
pub type ToolSpecResolver = Arc<dyn Fn(&SessionId) -> Vec<ToolSpec> + Send + Sync>;

/// Resolves the system prompt for a specific session's turn (#310). Its output
/// **overrides** the active profile's
/// [`system_prompt`][AgentProfile::system_prompt] for that turn; returning
/// `None` falls back to the profile's own prompt. Consulted fresh at every turn
/// build, so an embedder whose prompt is user-editable content — a site serving
/// its prompt from a CMS page — picks up an edit on the *next* turn without
/// respawning the engine (which would also tear down live sessions). The
/// resolver receives the running session's own [`SessionId`] + resolved profile,
/// so per-profile prompts (researcher / page-writer sub-agents) keep working and
/// an embedder can key off the root session for tenant context. The `Fn` is
/// intentionally sync: an embedder keeps a snapshot cache
/// (`Arc<RwLock<HashMap<SessionId, String>>>`) hydrated from its store rather
/// than doing I/O on the turn path — same guidance as [`ToolSpecResolver`].
pub type SystemPromptResolver =
    Arc<dyn Fn(&SessionId, &AgentProfile) -> Option<String> + Send + Sync>;

/// Engine configuration: how to build per-session LLMs, which host tools to
/// advertise to the model, and the named agent profiles sessions can switch
/// between.
///
/// Core advertises tool *schemas* ([`tool_specs`][Self::tool_specs]) but no
/// longer holds executable tools — the runtime owns execution and answers
/// [`OutEvent::ToolExec`][crate::protocol::OutEvent::ToolExec] with
/// [`InMsg::ToolResult`][crate::protocol::InMsg::ToolResult] (ADR-0006/0010).
#[derive(Clone)]
pub struct EngineConfig {
    pub llm_factory: LlmFactory,
    pub tool_specs: Vec<ToolSpec>,
    pub profiles: ProfileRegistry,
    /// Per-profile tool specs appended to [`tool_specs`][Self::tool_specs] for
    /// the active profile only (#119, ADR-0040). At turn time `run_turn` looks
    /// the running session's profile name up here and appends its entry (also
    /// filtered through [`AgentProfile::advertises_tool`]) after the #116 mask.
    /// A generic table keyed by profile name; the embedder fills it (an entry is
    /// absent/empty when a profile advertises no profile-scoped tools).
    pub profile_tool_specs: HashMap<String, Vec<ToolSpec>>,
    /// Per-session override for the advertised base tool schemas (#308,
    /// ADR-0076). When set, it is consulted at every turn build and its output
    /// **replaces** the engine-global [`tool_specs`][Self::tool_specs] for that
    /// session; [`profile_tool_specs`][Self::profile_tool_specs] are still
    /// appended and the active profile's mask still filters the result — the
    /// resolver widens/varies *discovery* per session, it never bypasses
    /// masking. This is the seam a multi-tenant embedder needs: one `Holly`
    /// advertising a different tool surface per user (their per-user MCP-server
    /// tools, a site's `enabled_mcp_server_ids` restriction) without one engine
    /// per user. `None` (the default) keeps the engine-global `tool_specs` for
    /// every session. See [`ToolSpecResolver`] for the snapshot-cache pattern.
    pub tool_spec_resolver: Option<ToolSpecResolver>,
    /// Per-turn override for the active profile's system prompt (#310,
    /// ADR-0078). When set, it is consulted at every turn build; a `Some(prompt)`
    /// return **replaces** the profile's
    /// [`system_prompt`][AgentProfile::system_prompt] for that turn, `None` falls
    /// back to it. This is the seam an embedder needs when the prompt is
    /// user-editable content — a site serving its prompt from a CMS page — so an
    /// edit lands on the *next* turn without respawning the engine (which would
    /// tear down every live session). The resolver sees the running session's own
    /// [`SessionId`] + resolved profile, so per-profile sub-agent prompts keep
    /// working and an embedder can key off the root session for tenant context.
    /// `None` (the default) keeps the profile's static prompt for every turn. See
    /// [`SystemPromptResolver`] for the snapshot-cache pattern.
    pub system_prompt_resolver: Option<SystemPromptResolver>,
    /// The backend's resolved default model id — what a profile with
    /// `model: None` actually runs under (#192). Lets the engine price a turn
    /// (via [`pricing`][Self::pricing]) even when the profile doesn't pin a
    /// model. `None` for the `EchoLlm` stub, which has no billable model.
    pub default_model: Option<String>,
    /// The active model's context window in tokens (#178), from the provider
    /// catalog. Each session derives its history token budget from this (see
    /// [`Context::with_window`][crate::context::Context::with_window]) so the
    /// engine compacts/refuses against the *real* window instead of a fixed
    /// Anthropic-shaped ceiling. `None` (unknown model / `EchoLlm`) falls back to
    /// [`CONTEXT_LIMIT_TOKENS`][crate::context::CONTEXT_LIMIT_TOKENS].
    pub context_window: Option<usize>,
    /// Resolved generation knobs for the active model (#191), supplied by the
    /// runtime from the catalog [`ModelEntry`][entanglement_provider::ModelEntry]'s
    /// capability metadata (temperature/max-output/thinking). Core threads it onto
    /// every [`LlmRequest`][entanglement_provider::LlmRequest] so the previously
    /// write-only catalog flags actually reach the provider. `None` (echo / a model
    /// absent from the catalog) sends no knobs — the backend's own defaults win.
    pub generation: Option<GenerationParams>,
    /// Per-model USD pricing keyed by catalog model id (#192), supplied by the
    /// runtime from the provider catalog. The engine multiplies a turn's reported
    /// [`Usage`][entanglement_provider::Usage] by the entry for the effective
    /// model to fill [`OutEvent::Usage`][crate::protocol::OutEvent::Usage]'s
    /// `cost_usd`; a model absent from the map yields `None` (unknown cost).
    pub pricing: HashMap<String, ModelPricing>,
    /// Re-resolves a `(provider, model)` pair against the catalog for a live
    /// model/provider switch (#218) — the seam that lets a running session swap
    /// its [`Session::llm`][crate::session::Session] without restarting the
    /// engine. Supplied by the runtime capturing the provider catalog + the
    /// per-endpoint HTTP client (already warm, #217). `None` (the `EchoLlm`
    /// default, or an embedder that doesn't wire it) makes an
    /// [`InMsg::SetModel`][crate::protocol::InMsg::SetModel] a no-op that surfaces
    /// an [`OutEvent::Error`][crate::protocol::OutEvent::Error].
    pub model_resolver: Option<ModelResolver>,
    /// How long a turn may sit parked on unresolved tool calls before the engine
    /// **re-offers** the pending batch — re-emitting each pending `ToolExec` with
    /// the same `request_id` and a fresh `seq` (#274). `OutEvent::ToolExec` rides
    /// the lossy outbound broadcast, so an in-process executor that falls behind
    /// (`RecvError::Lagged`) can drop an offer and strand the parked turn until a
    /// restart/resume; after this much *silence* (no `ToolResult` arriving) the
    /// executor gets another chance to run it. Sound only because the runtime
    /// executor dedupes by `request_id` — a re-offer to a still-in-flight call is
    /// a no-op there, not a double-run
    /// ([ADR-0071](../../docs/adr/0071-parked-turn-reoffer-timer.md)). `None`
    /// disables the timer (a turn parks indefinitely). Default: 60s.
    pub reoffer_interval: Option<Duration>,
}

impl EngineConfig {
    /// Fail if the config can't back a running engine — currently, a profile
    /// registry without the required `build` profile. Lets an embedder reject a
    /// bad config up front instead of relying on the supervisor's fallback.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.profiles.validate()
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            llm_factory: Arc::new(|| Box::new(EchoLlm) as Box<dyn Llm>),
            tool_specs: Vec::new(),
            profiles: ProfileRegistry::new(),
            profile_tool_specs: HashMap::new(),
            tool_spec_resolver: None,
            system_prompt_resolver: None,
            default_model: None,
            context_window: None,
            generation: None,
            pricing: HashMap::new(),
            model_resolver: None,
            reoffer_interval: Some(Duration::from_secs(60)),
        }
    }
}

/// A malformed [`EngineConfig`]/[`ProfileRegistry`] the engine can't run with.
/// Surfaced by [`EngineConfig::validate`]/[`ProfileRegistry::validate`] so an
/// embedder gets a clean error instead of a panicking supervisor task.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    /// The registry lacks the `build` profile every new session starts under.
    #[error("profile registry is missing the required `{DEFAULT_PROFILE}` profile")]
    MissingDefaultProfile,
}

/// Named set of [`AgentProfile`]s. Comes with only the `build` built-in — the
/// one profile every session starts under and [`resolve`][Self::resolve] falls
/// back to. The full `build`/`plan`/`explore` trio is defined once, as markdown,
/// in `entanglement-runtime`'s embedded agent registry (#201): core can't parse
/// agent frontmatter, so it carries no `plan`/`explore` copy to drift from that
/// source. Add your own with [`insert`][Self::insert].
#[derive(Clone, Default)]
pub struct ProfileRegistry {
    profiles: HashMap<String, AgentProfile>,
}

impl ProfileRegistry {
    pub fn new() -> Self {
        let mut reg = Self::default();
        reg.insert(default_profile());
        reg
    }

    pub fn get(&self, name: &str) -> Option<&AgentProfile> {
        self.profiles.get(name)
    }

    /// Every registered profile, name-sorted for a stable roster (the runtime
    /// discloses this to a spawning model — see the `agent`/`agent_spawn` tool
    /// descriptions). Sorting keeps the advertised order deterministic across
    /// runs regardless of `HashMap` iteration order.
    pub fn iter(&self) -> impl Iterator<Item = &AgentProfile> {
        let mut profiles: Vec<&AgentProfile> = self.profiles.values().collect();
        profiles.sort_by(|a, b| a.name.cmp(&b.name));
        profiles.into_iter()
    }

    pub fn insert(&mut self, profile: AgentProfile) {
        self.profiles.insert(profile.name.clone(), profile);
    }

    /// Fail if the required `build` profile is absent. Embedders that assemble a
    /// custom registry should call this before handing it to [`Holly::spawn`];
    /// the supervisor otherwise falls back to a synthesized default (see
    /// [`resolve`][Self::resolve]) rather than panicking.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.profiles.contains_key(DEFAULT_PROFILE) {
            Ok(())
        } else {
            Err(ConfigError::MissingDefaultProfile)
        }
    }

    /// Resolve a profile by name, falling back to the default `build` profile
    /// and finally to a synthesized built-in `build`. Never panics: a registry
    /// missing `build` (an unvalidated custom one) yields a degraded-but-safe
    /// session instead of crashing the supervisor and taking down every session.
    pub(super) fn resolve(&self, name: &str) -> AgentProfile {
        self.get(name)
            .or_else(|| self.get(DEFAULT_PROFILE))
            .cloned()
            .unwrap_or_else(|| {
                tracing::warn!(
                    "profile registry missing `{DEFAULT_PROFILE}` and `{name}`; \
                     falling back to a synthesized default profile"
                );
                default_profile()
            })
    }
}

/// The built-in `build` profile — the only profile core carries. It is both the
/// default a fresh session starts under and the synthesized fallback the
/// supervisor uses when a custom registry omits it (see
/// [`ProfileRegistry::resolve`]). An inherit-all coding agent: no tool mask, no
/// plan authority (default-closed, #231/ADR-0049). The runtime re-defines this
/// same shape as `build.md` and owns the `plan`/`explore` siblings (#201).
fn default_profile() -> AgentProfile {
    AgentProfile {
        name: "build".into(),
        description: "Coding agent — implements changes using the available tools.".into(),
        mode: AgentMode::Primary,
        system_prompt:
            "You are a coding agent. Implement the requested changes using the available tools."
                .into(),
        model: None,
        provider: None,
        permission: PermissionProfile::new(Permission::Allow),
        tools: None,
        disallowed_tools: Vec::new(),
        // `build` spawns everything except primaries (the target-side mode gate,
        // #119) — no `spawnable_agents` list, so user-defined exploration agents
        // stay spawnable without editing this built-in.
        can_spawn: None,
        spawnable_agents: None,
    }
}
