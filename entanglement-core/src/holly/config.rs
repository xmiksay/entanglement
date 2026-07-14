//! Engine configuration + agent-profile registry: the immutable inputs an
//! embedder hands [`Holly::spawn`][super::Holly::spawn]. Kept separate from the
//! supervisor loop so the config/profile surface reads on its own.

use std::collections::HashMap;
use std::sync::Arc;

use crate::protocol::{AgentMode, AgentProfile, Permission, PermissionProfile};
use entanglement_provider::{EchoLlm, LlmFactory, LlmSession, ModelPricing, ToolSpec};

use super::DEFAULT_PROFILE;

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
    /// Per-model USD pricing keyed by catalog model id (#192), supplied by the
    /// runtime from the provider catalog. The engine multiplies a turn's reported
    /// [`Usage`][entanglement_provider::Usage] by the entry for the effective
    /// model to fill [`OutEvent::Usage`][crate::protocol::OutEvent::Usage]'s
    /// `cost_usd`; a model absent from the map yields `None` (unknown cost).
    pub pricing: HashMap<String, ModelPricing>,
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
            llm_factory: Arc::new(|| LlmSession::new(Box::new(EchoLlm))),
            tool_specs: Vec::new(),
            profiles: ProfileRegistry::new(),
            profile_tool_specs: HashMap::new(),
            default_model: None,
            context_window: None,
            pricing: HashMap::new(),
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
