//! Per-session skills for embedders (ADR-0249): a multi-tenant host serves
//! each session its own [`SkillRegistry`] — `list_skills`, `load_skill` and
//! a script's `skill::<name>` namespace all read the session's — through one
//! [`SkillResolver`]. Without a resolver every session reads the one live
//! registry, exactly as before.
//!
//! An embedder builds skills in code, with no `SKILL.md` on disk:
//! [`SkillMeta::new`] plus [`SkillMeta::with_lib`].

use std::sync::{Arc, RwLock};

use entanglement_core::SessionId;

use super::{SkillMeta, SkillRegistry};

/// The embedder seam: session → its skill registry. `None` falls back to the
/// live (global) registry. Called per `list_skills`/`load_skill`/script
/// launch, so keep it an in-memory lookup.
pub type SkillResolver = Arc<dyn Fn(&SessionId) -> Option<Arc<SkillRegistry>> + Send + Sync>;

/// Where a session's skills come from: the live registry, overridden per
/// session by an optional resolver.
#[derive(Clone)]
pub struct SessionSkills {
    live: Arc<RwLock<Arc<SkillRegistry>>>,
    resolver: Option<SkillResolver>,
}

impl SessionSkills {
    pub fn new(live: Arc<RwLock<Arc<SkillRegistry>>>) -> Self {
        Self {
            live,
            resolver: None,
        }
    }

    pub fn with_resolver(mut self, resolver: SkillResolver) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// The live registry only — for callers with no session.
    pub fn live(&self) -> Arc<SkillRegistry> {
        self.live
            .read()
            .expect("skill registry lock poisoned")
            .clone()
    }

    /// `session`'s registry: the resolver's answer, else the live one.
    pub fn for_session(&self, session: &SessionId) -> Arc<SkillRegistry> {
        self.resolver
            .as_ref()
            .and_then(|resolve| resolve(session))
            .unwrap_or_else(|| self.live())
    }

    pub(crate) fn set_live(&mut self, live: Arc<RwLock<Arc<SkillRegistry>>>) {
        self.live = live;
    }
}

impl Default for SessionSkills {
    fn default() -> Self {
        Self::new(Arc::new(RwLock::new(Arc::new(SkillRegistry::default()))))
    }
}

impl SkillMeta {
    /// A skill defined in code: no directory, no payload files, no lib.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            root_dir: None,
            body: body.into(),
            lib: None,
        }
    }

    /// Attach a `lib.rhai` compiled from `source` (registered in scripts as
    /// `skill::<name>`). `Err` names why it was refused — only `fn`
    /// definitions are allowed.
    pub fn with_lib(mut self, source: &str) -> Result<Self, String> {
        self.lib = Some(super::lib_rhai::compile(&self.name, source)?);
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(name: &str) -> Arc<SkillRegistry> {
        let mut reg = SkillRegistry::default();
        reg.insert(SkillMeta::new(name, "d", "body"));
        Arc::new(reg)
    }

    #[test]
    fn the_resolver_picks_per_session_and_falls_back_to_live() {
        let live = Arc::new(RwLock::new(registry("global")));
        let tenant_a = registry("a");
        let skills = SessionSkills::new(live).with_resolver(Arc::new(move |s: &SessionId| {
            (s.to_string() == "a:1").then(|| tenant_a.clone())
        }));
        assert!(skills
            .for_session(&SessionId::new("a:1"))
            .get("a")
            .is_some());
        let other = skills.for_session(&SessionId::new("b:1"));
        assert!(other.get("global").is_some() && other.get("a").is_none());
    }

    #[test]
    fn a_code_built_skill_carries_a_compiled_lib() {
        let skill = SkillMeta::new("inv", "d", "b")
            .with_lib("/// Twice.\nfn twice(x) { x * 2 }")
            .unwrap();
        let lib = skill.lib.expect("compiled");
        assert!(lib.functions[0].signature.contains("skill::inv::twice"));
        let err = SkillMeta::new("inv", "d", "b").with_lib("let x = 1;");
        assert!(err.is_err());
    }
}
