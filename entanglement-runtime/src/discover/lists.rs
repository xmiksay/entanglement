//! The static discovery rosters (ADR-0226): `list_agents`, `list_models`,
//! `list_skills`. Pure renderers over data the ladder already holds — no I/O,
//! no state, so each is unit-testable directly.

use entanglement_core::{AgentCatalog, Catalog};

use crate::system_prompt::SkillDisclosure;

/// `list_agents`: every registered agent — what a model reads to pick an
/// `agent`/`agent_send` target, now that the `agent` spec carries no roster
/// (so it stays byte-identical for the session). Name-sorted, matching
/// [`AgentCatalog::iter`].
pub fn agents_list(agents: &AgentCatalog) -> String {
    if agents.iter().next().is_none() {
        return "No agents are registered.".to_string();
    }
    let mut out = String::from("Agents (start one with agent, follow up with agent_send):");
    for a in agents.iter() {
        let pin = match (&a.provider, &a.model) {
            (Some(p), Some(m)) => format!(" [model: {p}/{m}]"),
            (None, Some(m)) => format!(" [model: {m}]"),
            _ => String::new(),
        };
        out.push_str(&format!("\n  {} — {}{pin}", a.name, a.description));
    }
    out
}

/// `list_skills`: name + description of every model-loadable skill (the
/// registry's disclosures, `user_only` withheld) — the roster the system
/// prompt no longer carries. `skutter inspect skills --disclosures` prints
/// the same text.
pub fn skills_list(disclosures: &[SkillDisclosure]) -> String {
    if disclosures.is_empty() {
        return "No skills are available.".to_string();
    }
    let mut out = String::from("Skills (load one with load_skill):");
    for d in disclosures {
        out.push_str(&format!("\n  {} — {}", d.name, d.description));
    }
    out
}

/// `list_models`: the active catalog's models, only from *usable* providers
/// (a key or OAuth credential resolves — the same filter
/// [`crate::permission::resolve_model`] applies to its refusal): a model this
/// session cannot reach should not be offered. "No catalog" and "nothing
/// usable" read differently — different problems, different fixes.
pub fn models_list(catalog: Option<&Catalog>) -> String {
    let Some(catalog) = catalog else {
        return "No model catalog is configured.".to_string();
    };
    let usable: Vec<_> = catalog.providers.iter().filter(|p| p.is_usable()).collect();
    if usable.is_empty() {
        return "A catalog is configured, but no provider is currently usable — set an API \
                key (or connect one via OAuth) for at least one provider."
            .to_string();
    }
    let mut out = String::from("Models (pass provider/id as agent's model):");
    for provider in usable {
        for model in &provider.models {
            let name = model.display_name.as_deref().unwrap_or(&model.id);
            let window = model
                .context_window
                .map(|c| format!(", context window {c}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "\n  {}/{} — {name}{window}",
                provider.name, model.id
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::Agent;

    fn agents() -> AgentCatalog {
        let mut agents = AgentCatalog::new();
        agents.insert(Agent {
            name: "general".to_string(),
            description: "general-purpose work".to_string(),
            system_prompt: String::new(),
            model: Some("glm-5.3".to_string()),
            provider: Some("zai".to_string()),
            default_mode: None,
        });
        agents
    }

    #[test]
    fn agents_list_carries_name_description_and_model_pin() {
        let out = agents_list(&agents());
        assert!(
            out.contains("general — general-purpose work [model: zai/glm-5.3]"),
            "{out}"
        );
    }

    #[test]
    fn skills_list_lists_loadable_skills() {
        let mut skills = crate::skills::SkillRegistry::default();
        skills.insert(crate::skills::SkillMeta {
            name: "git".to_string(),
            description: "commit helpers".to_string(),
            user_only: false,
            allowed_tools: None,
            root_dir: None,
            body: String::new(),
            tools: Vec::new(),
        });
        assert!(skills_list(&skills.disclosures()).contains("git — commit helpers"));
        assert_eq!(skills_list(&[]), "No skills are available.");
    }

    fn catalog_with(usable_key_env: &str, unusable_key_env: &str) -> Catalog {
        serde_yaml::from_str(&format!(
            "providers:\n\
             \x20\x20- name: zai\n\
             \x20\x20\x20\x20key_env: {usable_key_env}\n\
             \x20\x20\x20\x20default_model: glm-5.1\n\
             \x20\x20\x20\x20models:\n\
             \x20\x20\x20\x20\x20\x20- id: glm-5.1\n\
             \x20\x20\x20\x20\x20\x20\x20\x20display_name: GLM 5.1\n\
             \x20\x20- name: openai\n\
             \x20\x20\x20\x20key_env: {unusable_key_env}\n\
             \x20\x20\x20\x20default_model: gpt-4o\n\
             \x20\x20\x20\x20models:\n\
             \x20\x20\x20\x20\x20\x20- id: gpt-4o\n"
        ))
        .expect("test catalog must parse")
    }

    #[test]
    fn models_list_offers_only_usable_providers_with_display_names() {
        std::env::set_var("LISTS_TEST_ZAI_0226", "k");
        std::env::remove_var("LISTS_TEST_OPENAI_0226_UNSET");
        let catalog = catalog_with("LISTS_TEST_ZAI_0226", "LISTS_TEST_OPENAI_0226_UNSET");
        let out = models_list(Some(&catalog));
        assert!(out.contains("zai/glm-5.1 — GLM 5.1"), "{out}");
        assert!(!out.contains("gpt-4o"), "{out}");
        std::env::remove_var("LISTS_TEST_ZAI_0226");
    }

    #[test]
    fn models_list_tells_no_catalog_from_nothing_usable() {
        assert_eq!(models_list(None), "No model catalog is configured.");
        std::env::remove_var("LISTS_TEST_ZAI_0226_UNSET");
        std::env::remove_var("LISTS_TEST_OPENAI_0226_UNSET2");
        let catalog = catalog_with("LISTS_TEST_ZAI_0226_UNSET", "LISTS_TEST_OPENAI_0226_UNSET2");
        assert!(models_list(Some(&catalog)).contains("no provider is currently usable"));
    }
}
