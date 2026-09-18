//! Non-tool `explore`/`describe` kinds (#560 P12, ADR-0207 §12): `agents`,
//! `skills`, `models`, `modes` — entirely different data domains from the
//! `tools`/`tool`/`mcp`/`skill`/`endpoint` tool-index kinds [`super::sections`]
//! already handles, so they fork *before* that filtering rather than adding
//! another `sections::Kind` variant. `tools` (and the omitted default) still
//! means exactly what it always has.
//!
//! `pending` is deliberately **not** one of these: it needs engine-wide
//! registries (`AgentRegistry`, job/script/retained/pending/question state)
//! that neither `explore`'s nor `describe`'s signature carries, and that the
//! interception ladder (`tool_runner/ladder/orchestration.rs`) already has
//! in hand — so it forks even earlier, at the ladder, via [`peek_kind`].

use std::sync::Arc;

use entanglement_core::{AgentCatalog, Catalog};
use serde_json::{json, Value};

use crate::mode::{describe as mode_describe, ModeTable};
use crate::skills::SkillRegistry;

use super::sections::Kind as ToolKind;

/// What `explore`'s top-level `kind` field selects between. `Tools` carries
/// the existing sub-filter (`None` = every section, matching today's
/// omitted-`kind` behaviour) so [`super::explore::build_rows`] needs no
/// change at all — only its caller forks on this first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TopKind {
    Tools(Option<ToolKind>),
    Agents,
    Skills,
    Models,
    Modes,
}

impl TopKind {
    /// Parse the raw `kind` string, case-insensitive. Anything unrecognized
    /// — including every existing `tool`/`mcp`/`skill`/`endpoint` sub-filter,
    /// and an explicit `"tools"` — falls through to `Tools`, whose own
    /// [`ToolKind::parse`] resolves the sub-filter (or `None` for "show
    /// everything", the lenient-degrade posture `explore` has always had).
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("agents") => TopKind::Agents,
            Some("skills") => TopKind::Skills,
            Some("models") => TopKind::Models,
            Some("modes") => TopKind::Modes,
            Some("tools") | None => TopKind::Tools(None),
            Some(other) => TopKind::Tools(ToolKind::parse(other)),
        }
    }
}

/// Peek the `{"kind": "..."}` field of a raw `explore`/`describe` call
/// without fully parsing it — used by the interception ladder to fork
/// `explore(kind: "pending")` before dispatching into this module or
/// `super::explore`, since `pending` needs state neither carries. Tolerates
/// a malformed body the same way every other `explore`/`describe` parse does
/// (degrades to "not pending", never an error).
pub fn peek_kind(input: &str) -> Option<String> {
    let v: Value = serde_json::from_str(input).ok()?;
    v.get("kind")?.as_str().map(|s| s.to_ascii_lowercase())
}

/// The static inputs a non-tool kind's index/lookup reads — bundled so the
/// ladder passes one struct instead of three, and so `explore`/`describe`'s
/// signatures grow by one parameter each rather than three.
#[derive(Clone)]
pub struct KindsCtx {
    pub agents: AgentCatalog,
    pub skills: Arc<SkillRegistry>,
    pub catalog: Option<Arc<Catalog>>,
    pub modes: Arc<ModeTable>,
}

/// `explore(kind: "agents")`: name + description of every registered agent —
/// what a spawning model reads to pick an `agent`/`agent_send` target.
/// Name-sorted, matching [`AgentCatalog::iter`]'s own order (the same
/// roster the `agent` tool's own description discloses).
pub fn agents_index(ctx: &KindsCtx) -> String {
    let mut out = String::from("AGENTS (spawn targets for agent/agent_send):");
    for p in ctx.agents.iter() {
        out.push_str(&format!("\n  {} — {}", p.name, p.description));
    }
    out
}

/// `explore(kind: "skills")`: name + description of every discoverable
/// skill — the same roster the tool-index `kind: "skill"` section already
/// shows, framed as its own top-level listing rather than one section among
/// several.
pub fn skills_index(ctx: &KindsCtx) -> String {
    let mut out = String::from("SKILLS (load with load_skill):");
    for d in ctx.skills.disclosures() {
        out.push_str(&format!("\n  {} — {}", d.name, d.description));
    }
    out
}

/// `explore(kind: "models")`: the active catalog — id, context window, and
/// pricing where known — so a model choosing a child's `agent` `model`
/// parameter (or explaining a `request_mode`-adjacent limit) can see what's
/// actually available. Only *usable* providers are listed (#560 P12 follow-up
/// — [`entanglement_provider::ProviderEntry::is_usable`], the same filter
/// [`crate::permission::resolve_model`] applies to its refusal): a model this
/// session cannot possibly reach should not appear in discovery. `None`
/// catalog (a lean/test wrapper with no catalog wired) and "a catalog but
/// nothing in it is usable" are reported with distinct messages — different
/// problems, different fixes — rather than folding into one empty list.
pub fn models_index(ctx: &KindsCtx) -> String {
    let Some(catalog) = ctx.catalog.as_deref() else {
        return "MODELS: no catalog configured.".to_string();
    };
    let usable: Vec<_> = catalog.providers.iter().filter(|p| p.is_usable()).collect();
    if usable.is_empty() {
        return "MODELS: catalog configured, but no provider is currently usable — \
                set an API key (or connect one via OAuth) for at least one provider."
            .to_string();
    }
    let mut out = String::from("MODELS:");
    for provider in usable {
        for model in &provider.models {
            out.push_str(&format!(
                "\n  {}/{} — {}{}",
                provider.name,
                model.id,
                model
                    .context_window
                    .map(|c| format!("context_window={c}"))
                    .unwrap_or_else(|| "context_window=unknown".to_string()),
                model
                    .pricing
                    .map(|p| format!(", pricing: {}", pricing_text(&p)))
                    .unwrap_or_default(),
            ));
        }
    }
    out
}

fn pricing_text(p: &entanglement_core::ModelPricing) -> String {
    let field = |label: &str, v: Option<f64>| v.map(|v| format!("{label}=${v}/M"));
    [
        field("input", p.input),
        field("output", p.output),
        field("cached_input", p.cached_input),
        field("cache_write", p.cache_write),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
}

/// `explore(kind: "modes")`: the four permission modes and what each
/// permits (see [`mode_describe`]) — so a session denied a call can explain
/// itself, or a model can check what widening `request_mode` would grant,
/// without guessing from denial messages alone.
pub fn modes_index(_ctx: &KindsCtx) -> String {
    mode_describe::modes_index()
}

/// `describe(["agent:<name>", "skill:<name>", "model:<provider>/<id>",
/// "mode:<name>"])`: qualified lookups into the four non-tool kinds,
/// resolved the same way `describe` resolves a plain tool name — one JSON
/// entry, `None` when the prefix isn't one of the four (the caller then
/// falls through to the ordinary tool-name resolution).
pub fn resolve_qualified(name: &str, ctx: &KindsCtx) -> Option<Value> {
    let (prefix, rest) = name.split_once(':')?;
    match prefix {
        "agent" => {
            let p = ctx.agents.get(rest)?;
            Some(json!({ "name": name, "description": p.description }))
        }
        "skill" => {
            let d = ctx
                .skills
                .disclosures()
                .into_iter()
                .find(|d| d.name == rest)?;
            Some(json!({ "name": name, "description": d.description }))
        }
        "model" => {
            let catalog = ctx.catalog.as_deref()?;
            let (provider_name, model_id) = rest.split_once('/').unwrap_or(("", rest));
            let provider = catalog
                .providers
                .iter()
                .find(|p| provider_name.is_empty() || p.name == provider_name)?;
            let model = provider
                .models
                .iter()
                .find(|m| m.id == model_id || m.id == rest)?;
            Some(json!({
                "name": name,
                "provider": provider.name,
                "id": model.id,
                "context_window": model.context_window,
                "pricing": model.pricing.map(|p| json!({
                    "input": p.input,
                    "output": p.output,
                    "cached_input": p.cached_input,
                    "cache_write": p.cache_write,
                })),
            }))
        }
        "mode" => {
            let summary = mode_describe::mode_summary(rest)?;
            Some(json!({ "name": name, "description": summary }))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::Agent;

    fn ctx() -> KindsCtx {
        let mut agents = AgentCatalog::new();
        agents.insert(Agent {
            name: "general".to_string(),
            description: "general-purpose work".to_string(),
            system_prompt: String::new(),
            model: None,
            provider: None,
        });
        let mut skills = SkillRegistry::default();
        skills.insert(crate::skills::SkillMeta {
            name: "git".to_string(),
            description: "commit helpers".to_string(),
            user_only: false,
            allowed_tools: None,
            root_dir: None,
            body: String::new(),
            tools: Vec::new(),
        });
        KindsCtx {
            agents,
            skills: Arc::new(skills),
            catalog: None,
            modes: Arc::new(ModeTable::builtin().expect("built-ins parse")),
        }
    }

    #[test]
    fn parse_recognizes_every_new_kind_and_falls_through_to_tools() {
        assert_eq!(TopKind::parse(Some("agents")), TopKind::Agents);
        assert_eq!(TopKind::parse(Some("Skills")), TopKind::Skills);
        assert_eq!(TopKind::parse(Some("MODELS")), TopKind::Models);
        assert_eq!(TopKind::parse(Some("modes")), TopKind::Modes);
        assert_eq!(TopKind::parse(None), TopKind::Tools(None));
        assert_eq!(TopKind::parse(Some("tools")), TopKind::Tools(None));
        assert_eq!(
            TopKind::parse(Some("skill")),
            TopKind::Tools(Some(ToolKind::parse("skill").unwrap()))
        );
    }

    #[test]
    fn peek_kind_reads_the_field_and_tolerates_garbage() {
        assert_eq!(
            peek_kind(r#"{"kind":"Pending"}"#),
            Some("pending".to_string())
        );
        assert_eq!(peek_kind("not json"), None);
        assert_eq!(peek_kind("{}"), None);
    }

    #[test]
    fn agents_index_lists_the_roster() {
        let out = agents_index(&ctx());
        assert!(out.contains("general — general-purpose work"), "{out}");
    }

    #[test]
    fn skills_index_lists_discoverable_skills() {
        let out = skills_index(&ctx());
        assert!(out.contains("git — commit helpers"), "{out}");
    }

    #[test]
    fn models_index_reports_no_catalog_plainly() {
        assert_eq!(models_index(&ctx()), "MODELS: no catalog configured.");
    }

    fn catalog_with(usable_key_env: &str, unusable_key_env: &str) -> Catalog {
        serde_yaml::from_str(&format!(
            "providers:\n\
             \x20\x20- name: zai\n\
             \x20\x20\x20\x20key_env: {usable_key_env}\n\
             \x20\x20\x20\x20default_model: glm-5.1\n\
             \x20\x20\x20\x20models:\n\
             \x20\x20\x20\x20\x20\x20- id: glm-5.1\n\
             \x20\x20- name: openai\n\
             \x20\x20\x20\x20key_env: {unusable_key_env}\n\
             \x20\x20\x20\x20default_model: gpt-4o\n\
             \x20\x20\x20\x20models:\n\
             \x20\x20\x20\x20\x20\x20- id: gpt-4o\n"
        ))
        .expect("test catalog must parse")
    }

    /// Bug 2: an unusable provider's models never appear in discovery.
    #[test]
    fn models_index_omits_an_unusable_providers_models() {
        std::env::set_var("KINDS_TEST_ZAI_560", "k");
        std::env::remove_var("KINDS_TEST_OPENAI_560_UNSET");
        let mut ctx = ctx();
        ctx.catalog = Some(Arc::new(catalog_with(
            "KINDS_TEST_ZAI_560",
            "KINDS_TEST_OPENAI_560_UNSET",
        )));
        let out = models_index(&ctx);
        assert!(out.contains("zai/glm-5.1"), "{out}");
        assert!(!out.contains("gpt-4o"), "{out}");
        std::env::remove_var("KINDS_TEST_ZAI_560");
    }

    /// Bug 2, plain-message half: a catalog is configured but nothing in it
    /// is usable — a distinct message from "no catalog configured".
    #[test]
    fn models_index_reports_catalog_present_but_nothing_usable() {
        std::env::remove_var("KINDS_TEST_ZAI_560_UNSET");
        std::env::remove_var("KINDS_TEST_OPENAI_560_UNSET2");
        let mut ctx = ctx();
        ctx.catalog = Some(Arc::new(catalog_with(
            "KINDS_TEST_ZAI_560_UNSET",
            "KINDS_TEST_OPENAI_560_UNSET2",
        )));
        let out = models_index(&ctx);
        assert!(out.contains("no provider is currently usable"), "{out}");
        assert_ne!(out, "MODELS: no catalog configured.", "{out}");
    }

    #[test]
    fn modes_index_lists_all_four() {
        let out = modes_index(&ctx());
        for name in ["research", "plan", "build", "auto"] {
            assert!(out.contains(name), "{out}");
        }
    }

    #[test]
    fn resolve_qualified_covers_all_four_prefixes_and_rejects_unknown() {
        let ctx = ctx();
        assert!(resolve_qualified("agent:general", &ctx).is_some());
        assert!(resolve_qualified("agent:nope", &ctx).is_none());
        assert!(resolve_qualified("skill:git", &ctx).is_some());
        assert!(resolve_qualified("mode:build", &ctx).is_some());
        assert!(resolve_qualified("mode:nope", &ctx).is_none());
        // No catalog wired in this test ctx, so a model: lookup is `None`
        // too — distinct from "unrecognized prefix", but both fall through
        // identically to the caller.
        assert!(resolve_qualified("model:zai/glm-5.3", &ctx).is_none());
        assert!(resolve_qualified("bogus:x", &ctx).is_none());
        assert!(resolve_qualified("no-colon", &ctx).is_none());
    }
}
