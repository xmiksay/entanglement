//! `skutter inspect config`'s "advanced catalog" section (ADR-0213).
//!
//! The wire-fact knobs — `thinking_*`, `effort_tiers`, `supports_temperature`,
//! `web_search_tool_version`, `prompt_cache_key`, per-model
//! `tool_advertising`/`discovery`, rate limits — are catalog data describing
//! what an endpoint/model accepts, not user preferences, so they live only in
//! `providers.yml`. They stay visible here so "why did the request carry that
//! shape?" still has an answer without reading YAML. Only facts that differ
//! from the built-in default are listed, keeping the section scannable.

use std::fmt::Debug;
use std::fmt::Write as _;

use entanglement_core::{Catalog, ModelEntry};

/// Append the section for every catalog provider to `out`.
pub(super) fn render(out: &mut String, catalog: &Catalog) {
    let _ = writeln!(
        out,
        "\nadvanced catalog (providers.yml wire facts; non-default values only):"
    );
    for p in &catalog.providers {
        let mut facts = vec![format!("wire={}", snake(&p.wire))];
        push_opt(&mut facts, "rpm", p.rpm);
        push_opt(&mut facts, "concurrency", p.concurrency);
        if p.prompt_cache_key {
            facts.push("prompt_cache_key=true".to_string());
        }
        push_opt(&mut facts, "discovery", p.discovery.map(|d| d.label()));
        push_opt(
            &mut facts,
            "thinking_control",
            p.thinking_control.map(|t| snake(&t)),
        );
        let _ = writeln!(out, "  {}: {}", p.name, facts.join(" "));
        for m in &p.models {
            let facts = model_facts(m);
            if !facts.is_empty() {
                let _ = writeln!(out, "    {}: {}", m.id, facts.join(" "));
            }
        }
    }
}

fn model_facts(m: &ModelEntry) -> Vec<String> {
    let mut facts = Vec::new();
    push_opt(
        &mut facts,
        "thinking_style",
        m.thinking_style.map(|v| snake(&v)),
    );
    push_opt(
        &mut facts,
        "thinking_format",
        m.thinking_format.map(|v| snake(&v)),
    );
    if m.thinking_required {
        facts.push("thinking_required=true".to_string());
    }
    push_opt(&mut facts, "replay_thinking", m.replay_thinking);
    if let Some(tiers) = m.effort_tiers {
        let list: Vec<&str> = tiers.iter().map(|e| e.as_str()).collect();
        facts.push(format!("effort_tiers=[{}]", list.join(",")));
    }
    if !m.supports_temperature {
        facts.push("supports_temperature=false".to_string());
    }
    push_opt(
        &mut facts,
        "web_search_tool_version",
        m.web_search_tool_version.as_deref(),
    );
    push_opt(
        &mut facts,
        "tool_advertising",
        m.tool_advertising.map(|v| v.label()),
    );
    push_opt(&mut facts, "discovery", m.discovery.map(|d| d.label()));
    push_opt(&mut facts, "concurrency", m.concurrency);
    facts
}

fn push_opt(facts: &mut Vec<String>, key: &str, value: Option<impl std::fmt::Display>) {
    if let Some(v) = value {
        facts.push(format!("{key}={v}"));
    }
}

/// A unit enum's `Debug` name in the catalog's snake_case spelling
/// (`InlineTags` → `inline_tags`), so the output reads like the YAML.
fn snake(value: &impl Debug) -> String {
    let mut out = String::new();
    for (i, c) in format!("{value:?}").chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_matches_the_yaml_spelling() {
        #[derive(Debug)]
        enum E {
            InlineTags,
            Zai,
        }
        assert_eq!(snake(&E::InlineTags), "inline_tags");
        assert_eq!(snake(&E::Zai), "zai");
    }

    #[test]
    fn only_non_default_facts_are_listed() {
        let catalog: Catalog = serde_yaml::from_str(
            "providers:\n\
             \x20 - name: p\n\
             \x20   default_model: a\n\
             \x20   rpm: 40\n\
             \x20   models:\n\
             \x20     - id: a\n\
             \x20     - id: b\n\
             \x20       thinking_format: inline_tags\n\
             \x20       supports_temperature: false\n\
             \x20       effort_tiers: [low, high]\n\
             \x20       discovery: invoke\n",
        )
        .unwrap();
        let mut out = String::new();
        render(&mut out, &catalog);
        assert!(out.contains("  p: wire=openai rpm=40"), "{out}");
        assert!(
            !out.contains("    a:"),
            "a default-only model is omitted: {out}"
        );
        let b = out.lines().find(|l| l.starts_with("    b:")).unwrap();
        for fact in [
            "thinking_format=inline_tags",
            "supports_temperature=false",
            "effort_tiers=[low,high]",
            "discovery=invoke",
        ] {
            assert!(b.contains(fact), "{fact} in {b}");
        }
    }

    #[test]
    fn the_shipped_catalog_shows_its_wire_facts() {
        let mut out = String::new();
        render(&mut out, &Catalog::builtin());
        assert!(out.contains("thinking_control=zai"), "{out}");
        assert!(out.contains("effort_tiers=["), "{out}");
    }
}
