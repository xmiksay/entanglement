//! [`AgentCatalog`]: the named agents an [`EngineConfig`][super::EngineConfig]
//! carries, plus the one built-in (`general`) core ships. Split out of
//! `config.rs` at the 400-line cap.

use std::collections::HashMap;

use super::config::ConfigError;
use super::DEFAULT_AGENT;
use crate::protocol::Agent;

/// Named set of [`Agent`]s. Comes with only the `build` built-in — the
/// one agent every session starts under and [`resolve`][Self::resolve] falls
/// back to. The full `build`/`plan`/`explore`/`debug`/`research` set is
/// defined once, as markdown, in `entanglement-runtime`'s embedded agent
/// registry (#201): core can't parse agent frontmatter, so it carries no
/// `plan`/`explore`/`debug`/`research` copy to drift from that source. Add
/// your own with [`insert`][Self::insert].
#[derive(Clone, Default)]
pub struct AgentCatalog {
    agents: HashMap<String, Agent>,
}

impl AgentCatalog {
    pub fn new() -> Self {
        let mut reg = Self::default();
        reg.insert(default_agent());
        reg
    }

    pub fn get(&self, name: &str) -> Option<&Agent> {
        self.agents.get(name)
    }

    /// Every registered agent, name-sorted for a stable roster (the runtime
    /// discloses this to a spawning model — see the `agent` tool
    /// descriptions). Sorting keeps the advertised order deterministic across
    /// runs regardless of `HashMap` iteration order.
    pub fn iter(&self) -> impl Iterator<Item = &Agent> {
        let mut agents: Vec<&Agent> = self.agents.values().collect();
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        agents.into_iter()
    }

    pub fn insert(&mut self, agent: Agent) {
        self.agents.insert(agent.name.clone(), agent);
    }

    /// Fail if the required [`DEFAULT_AGENT`] agent is absent. Embedders that
    /// assemble a custom registry should call this before handing it to
    /// [`Holly::spawn`]; the supervisor otherwise falls back to a synthesized
    /// default (see [`resolve`][Self::resolve]) rather than panicking.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.agents.contains_key(DEFAULT_AGENT) {
            Ok(())
        } else {
            Err(ConfigError::MissingDefaultAgent)
        }
    }

    /// Resolve an agent by name, falling back to the default
    /// [`DEFAULT_AGENT`] agent and finally to a synthesized built-in one.
    /// Never panics: a registry missing the default (an unvalidated custom
    /// one) yields a degraded-but-safe session instead of crashing the
    /// supervisor and taking down every session.
    pub(super) fn resolve(&self, name: &str) -> Agent {
        self.get(name)
            .or_else(|| self.get(DEFAULT_AGENT))
            .cloned()
            .unwrap_or_else(|| {
                tracing::warn!(
                    "agent catalog missing `{DEFAULT_AGENT}` and `{name}`; \
                     falling back to a synthesized default agent"
                );
                default_agent()
            })
    }
}

/// The built-in `general` agent — the only agent core carries. It is both
/// the default a fresh session starts under and the synthesized fallback the
/// supervisor uses when a custom registry omits it (see
/// [`AgentCatalog::resolve`]). Identity only — what it may do is the
/// session's permission mode (ADR-0207). The runtime re-defines this
/// same shape as `general.md` (formerly `build.md`, ADR-0207 stage 6a) and
/// owns the `plan`/`debug` siblings (#201).
fn default_agent() -> Agent {
    Agent {
        name: "general".into(),
        description: "Coding agent — implements changes using the available tools.".into(),
        system_prompt:
            "You are a coding agent. Implement the requested changes using the available tools."
                .into(),
        model: None,
        provider: None,
        default_mode: None,
    }
}
