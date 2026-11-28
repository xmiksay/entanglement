//! Live variant of the #934 guard (`make test-live`): the same 300-tool,
//! six-skill scenario as `it::tool_surface_guard`, against real z.ai GLM and
//! Anthropic endpoints. Reports the first request's token count and whether
//! the model reached the deep tool; asserts only that the turn finished, since
//! a live model's path is not deterministic.
//!
//! Every test is `#[ignore]` (network + a real key) and skips with a note
//! when the provider's key (`ZAI_API_KEY` / `ANTHROPIC_API_KEY`) is unset.
//! `<NAME>_MODEL` / `<NAME>_API_BASE` override the catalog's model and base.

// Shared with the offline guard, which uses every item of it.
#[allow(dead_code)]
#[path = "it/tool_surface_guard/fixture.rs"]
mod fixture;

use std::time::Duration;

use entanglement_core::{LlmFactory, OutEvent, SessionId};
use entanglement_provider::{
    anthropic_factory, openai_factory, Catalog, HttpClient, Wire, ANTHROPIC_BASE, OPENAI_BASE,
};

const SYSTEM: &str = "You are an operations assistant. Your capabilities are grouped \
                      into skills: find them with list_skills and load one with \
                      load_skill before using it.";

const PROMPT: &str = "How many units of SKU A-17 are on hand in the brno warehouse? \
                      Look it up with your tools and answer with the number.";

/// The deep tools that answer the prompt (`get` or `search` a stock level).
const TARGETS: [&str; 2] = ["inventory_get_stock_level", "inventory_search_stock_level"];

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The provider's client from the embedded catalog, or `None` (with a note)
/// when its key is unset.
fn factory(provider: &str) -> Option<(LlmFactory, String)> {
    let catalog = Catalog::builtin();
    let entry = catalog.provider(provider).expect("provider in the catalog");
    let key_env = entry.key_env.as_deref().expect("a keyed provider");
    let Some(key) = env(key_env) else {
        eprintln!("skipping live {provider}: {key_env} is unset");
        return None;
    };
    let upper = provider.to_uppercase();
    let model = env(&format!("{upper}_MODEL")).unwrap_or_else(|| entry.default_model.clone());
    let base = env(&format!("{upper}_API_BASE")).or_else(|| entry.base_url.clone());
    let http = HttpClient::new().expect("HTTP client builds");
    let limits = catalog.limits_resolver(provider);
    let llm = match entry.wire {
        Wire::Openai => openai_factory(
            base.unwrap_or_else(|| OPENAI_BASE.into()),
            Some(key),
            model.clone(),
            limits,
            entry.prompt_cache_key,
            catalog.thinking_spec_resolver(provider),
            http,
        ),
        Wire::Anthropic => anthropic_factory(
            base.unwrap_or_else(|| ANTHROPIC_BASE.into()),
            key,
            model.clone(),
            limits,
            catalog.anthropic_model_spec(provider, &model),
            http,
        ),
        other => panic!("{provider}: no live client for {other:?}"),
    };
    Some((llm, model))
}

async fn live(provider: &str) {
    let Some((llm, model)) = factory(provider) else {
        return;
    };
    let s = fixture::spawn(llm, fixture::mode_table(&[], &[]), true, SYSTEM);
    let sid = SessionId::new("live");
    let result = s
        .turn_within(&sid, PROMPT, true, Duration::from_secs(180))
        .await;
    let finished = result.is_ok();
    let events = result.unwrap_or_else(|events| events);

    let seen = s.seen.lock().unwrap().clone();
    let first_usage = events.iter().find_map(|e| match e {
        OutEvent::Usage {
            input_tokens,
            cached_input_tokens,
            cache_write_tokens,
            ..
        } => Some(input_tokens + cached_input_tokens + cache_write_tokens),
        _ => None,
    });
    let errors: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            OutEvent::Error { message, .. } => Some(message.as_str()),
            _ => None,
        })
        .collect();
    let answer: String = events
        .iter()
        .filter_map(|e| match e {
            OutEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let calls = s.calls.lock().unwrap().clone();
    println!(
        "live {provider}/{model}: {} registered deep tools; first request: {} advertised \
         tools, {} tool-schema bytes, {} input tokens; {} requests; reached a deep \
         tool answering the prompt: {}; deep calls: {calls:?}; finished: {finished}; \
         errors: {errors:?}; answer: {}",
        fixture::deep_tool_names().len(),
        seen.first_names.len(),
        seen.first_tool_bytes,
        first_usage.map_or("unreported".into(), |t| t.to_string()),
        seen.requests,
        calls.iter().any(|c| TARGETS.contains(&c.as_str())),
        answer.trim(),
    );
    assert!(finished, "the live turn did not finish: {events:#?}");
}

#[tokio::test]
#[ignore = "live: needs ZAI_API_KEY and network"]
async fn zai_reaches_a_deep_tool_through_skills() {
    live("zai").await;
}

#[tokio::test]
#[ignore = "live: needs ANTHROPIC_API_KEY and network"]
async fn anthropic_reaches_a_deep_tool_through_skills() {
    live("anthropic").await;
}
