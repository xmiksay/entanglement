//! Engine configuration + agent-profile registry: the immutable inputs an
//! embedder hands [`Holly::spawn`][super::Holly::spawn]. Kept separate from the
//! supervisor loop so the config/profile surface reads on its own.

use std::collections::HashMap;
use std::sync::Arc;

use crate::llm::{EchoLlm, LlmFactory, LlmSession, ToolSpec};
use crate::protocol::{AgentMode, AgentProfile, Permission, PermissionProfile};

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
    /// the active profile only (#119, ADR-0040). `run_turn` looks the running
    /// session's profile name up here and appends its entry (also filtered
    /// through [`AgentProfile::advertises_tool`]) after the #116 mask. Populated
    /// by the runtime with each profile's spawnable roster (the
    /// `agent_spawn`/`agent`/`agent_poll` triple, target-name enum + description
    /// scoped to that profile). A generic table — later per-profile features
    /// reuse it. Empty for a profile that may not spawn or has no valid targets.
    pub profile_tool_specs: HashMap<String, Vec<ToolSpec>>,
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

/// Named set of [`AgentProfile`]s. Comes with `build`, `plan`, `explore`
/// built-ins (mirroring opencode); add your own with [`insert`][Self::insert].
#[derive(Clone, Default)]
pub struct ProfileRegistry {
    profiles: HashMap<String, AgentProfile>,
}

impl ProfileRegistry {
    pub fn new() -> Self {
        let mut reg = Self::default();
        for profile in built_in_profiles() {
            reg.insert(profile);
        }
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

/// The built-in `build` profile — the synthesized fallback the supervisor uses
/// when a custom registry omits it (see [`ProfileRegistry::resolve`]).
fn default_profile() -> AgentProfile {
    let [build, ..] = built_in_profiles();
    build
}

fn built_in_profiles() -> [AgentProfile; 3] {
    [
        AgentProfile {
            name: "build".into(),
            description: "Coding agent — implements changes using the available tools.".into(),
            mode: AgentMode::Primary,
            system_prompt: "You are a coding agent. Implement the requested changes using the available tools.".into(),
            model: None,
            permission: PermissionProfile::new(Permission::Allow),
            tools: None,
            disallowed_tools: Vec::new(),
            // `build` spawns everything except primaries (the target-side mode
            // gate, #119) — no `spawnable_agents` list, so user-defined
            // exploration agents stay spawnable without editing this built-in.
            can_spawn: None,
            spawnable_agents: None,
        },
        AgentProfile {
            name: "plan".into(),
            description: "Planning agent — produces a plan without making changes.".into(),
            mode: AgentMode::Primary,
            system_prompt: "You are a planning agent. Analyze the request and produce a plan without making changes. Record the working plan with the update_plan tool, and delegate research to exploration agents.".into(),
            model: None,
            permission: PermissionProfile::new(Permission::Ask).with("read", Permission::Allow),
            // Physically read-only (#140, ADR-0041): the plan agent authors the
            // plan and delegates research — no `edit`/`write`/`bash`. Via
            // `tool_masked`'s ancestor intersection, every child spawned under
            // plan is clamped to this read-only set too.
            // The plan agent authors the plan: its allowlist explicitly opts into
            // `update_plan`/`propose_plan`, which is now what grants plan authority
            // (#231, ADR-0049) — an inherit-all profile never gets them by default.
            tools: Some(vec![
                "read".into(),
                "glob".into(),
                "grep".into(),
                "agent".into(),
                "agent_spawn".into(),
                "agent_poll".into(),
                "ask_user".into(),
                "load_skill".into(),
                "update_plan".into(),
                "propose_plan".into(),
            ]),
            disallowed_tools: Vec::new(),
            // `plan` may spawn (a primary), but omits `spawnable_agents` so any
            // user-defined exploration agent stays reachable (#119).
            can_spawn: None,
            spawnable_agents: None,
        },
        AgentProfile {
            name: "explore".into(),
            description: "Read-only exploration agent — answers questions about the codebase.".into(),
            mode: AgentMode::Subagent,
            system_prompt: "You are a read-only exploration agent. Answer questions about the codebase using only read tools.".into(),
            model: None,
            permission: PermissionProfile::new(Permission::Deny)
                .with("read", Permission::Allow)
                .with("glob", Permission::Allow)
                .with("grep", Permission::Allow),
            // Reference read-only agent (#116): the read trio is *all* it can
            // reach — no `edit`/`write`, no `bash`, no `agent_spawn`. A physical
            // boundary, matching the `permission` denies above.
            tools: Some(vec!["read".into(), "glob".into(), "grep".into()]),
            disallowed_tools: Vec::new(),
            // Reference leaf: a `Subagent` mode defaults `can_spawn` closed (#119),
            // so the whole `agent_*` family is withheld — matching the tool mask.
            can_spawn: None,
            spawnable_agents: None,
        },
    ]
}
