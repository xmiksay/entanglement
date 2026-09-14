//! `tool_search` (P7, ADR-0196 §3, `responses_native` encoding): answers a
//! streamed, client-executed `tool_search_call` from the OpenAI Responses
//! client. Dispatched exactly like `explore`/`describe` — the wire's own
//! `tool_search` primitive drives it, not a schema the model saw in its
//! tools array, so the reserved call name
//! [`entanglement_core::TOOL_SEARCH_CALL_TOOL`] is non-maskable and always
//! `Allow` (`tool_names::RESPONSES_TOOL_SEARCH_TOOL`, folded into
//! `tool_runner`'s `Intercept::Discover` route).
//!
//! Reuses the *same* live index [`super::explore`] serves and the *same*
//! per-name resolution [`super::describe`] uses — this is deliberately not a
//! third, independent search implementation. The reply carries a
//! [`ContentPart::ToolSearchOutput`] block (rather than `describe`'s plain
//! schema text) so the client can echo a native `tool_search_output` input
//! item on the next request.

use entanglement_core::{ContentPart, Holly, SessionId};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::mcp::{ActiveServers, AvailableMcp, McpScopes};
use crate::seam;
use crate::skills::SkillRegistry;
use crate::tool_advertising::AdvertisingState;
use crate::tools::ToolRegistry;

use super::describe::resolve_spec;
use super::explore::build_index;
use super::spec_to_json;

/// Result cap (mirrors Anthropic's native tool-search default `limit` of 5,
/// kept a little roomier since this search draws from a terser index than a
/// hosted 200+-tool MCP aggregation).
const MAX_RESULTS: usize = 8;

/// The provider tag [`ContentPart::ToolSearchOutput::provider`] is stamped
/// with — must match `entanglement_provider::openai_responses::request`'s
/// `WIRE_NAME` so the client's replay-side recognizes its own reply and
/// echoes a native `tool_search_output` item (a mismatch would silently
/// degrade every round-trip to plain text, never a hard failure — see that
/// module's `WIRE_NAME` doc for the round-trip contract this mirrors).
const WIRE_NAME: &str = "openai_responses";

#[derive(Deserialize, Default)]
struct Input {
    #[serde(default)]
    query: Option<String>,
}

/// Parse the `{"query": "..."}` input this client's own `tool_search`
/// schema declares (required, but tolerated missing/malformed — same
/// low-stakes posture as `explore`'s filter: a bad query degrades to "show
/// everything" rather than a hard failure the model can't recover from).
fn parse_query(input: &str) -> Option<String> {
    if input.trim().is_empty() {
        return None;
    }
    serde_json::from_str::<Input>(input)
        .ok()
        .and_then(|i| i.query)
        .map(|q| q.to_ascii_lowercase())
}

/// Render a resolved [`entanglement_core::ToolSpec`] to the flat Responses-wire
/// tool shape (`type`/`name`/`description`/`parameters`, `defer_loading:
/// true`) — mirrors `entanglement_provider::openai_responses::request`'s own
/// `convert_tools`, duplicated rather than shared because the runtime has no
/// client handle to call into; `describe::spec_to_json`'s generic
/// `name`/`description`/`schema` shape is the starting point, reshaped here.
fn to_responses_tool_entry(spec: &entanglement_core::ToolSpec) -> Value {
    let generic = spec_to_json(spec);
    json!({
        "type": "function",
        "name": generic["name"],
        "description": generic["description"],
        "parameters": generic["schema"],
        "defer_loading": true,
    })
}

/// Live index rows resolvable by [`resolve_spec`] — excludes an "allowed but
/// unconnected" MCP hint row (not a real tool; `resolve_spec` would fail on
/// its server-name-shaped "name") and a skill row (loaded via `load_skill`,
/// never describable), the same two source kinds `describe`'s own
/// `unknown_entry` distinguishes for a human-authored `describe` call.
fn describable_candidates(rows: Vec<Value>) -> Vec<String> {
    rows.into_iter()
        .filter(|r| {
            let source = r.get("source").and_then(Value::as_str).unwrap_or("");
            source == "built-in" || source.starts_with("mcp:")
        })
        .filter_map(|r| r.get("name").and_then(Value::as_str).map(str::to_string))
        .take(MAX_RESULTS)
        .collect()
}

/// Dispatch `tool_search`: parse the query, search the live index, resolve
/// each candidate to a full spec (marking it discovered, same as `describe`
/// under `ToolSearch` mode), reply with a
/// [`ContentPart::ToolSearchOutput`] block. Always-`Allow`/non-maskable
/// (enforced by the executor's dispatch ladder, not here) — a search that
/// matches nothing still replies cleanly (an empty `tools` array), never a
/// hard tool failure.
#[allow(clippy::too_many_arguments)]
pub async fn run_tool_search(
    holly: &Holly,
    registry: ToolRegistry,
    avail: &AvailableMcp,
    active: &ActiveServers,
    skills: &SkillRegistry,
    mcp_scopes: Option<&McpScopes>,
    advertising: &AdvertisingState,
    session: SessionId,
    request_id: String,
    input: String,
) {
    let query = parse_query(&input);
    let rows = build_index(&registry, avail, active, skills, &session, query.as_deref());
    let candidates = describable_candidates(rows);

    let mut tools = Vec::with_capacity(candidates.len());
    let mut names = Vec::with_capacity(candidates.len());
    for name in &candidates {
        if let Ok(spec) = resolve_spec(&registry, mcp_scopes, &session, name).await {
            advertising
                .discovered
                .lock()
                .expect("discovered-tool mutex poisoned")
                .mark(&session, name);
            names.push(name.clone());
            tools.push(to_responses_tool_entry(&spec));
        }
    }

    let summary = if names.is_empty() {
        match &query {
            Some(q) => format!("no tools found matching \"{q}\""),
            None => "no additional tools available".to_string(),
        }
    } else {
        format!("discovered: {}", names.join(", "))
    };
    let content = vec![ContentPart::tool_search_output(
        WIRE_NAME,
        summary,
        Value::Array(tools),
    )];
    seam::reply_content(holly, session, request_id, content, false, None, None).await;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;
    use crate::skills::SkillMeta;
    use crate::tools::Tool;

    struct Fake;
    #[async_trait]
    impl Tool for Fake {
        fn name(&self) -> std::borrow::Cow<'static, str> {
            std::borrow::Cow::Borrowed("glob")
        }
        fn description(&self) -> &str {
            "find files by pattern"
        }
        fn schema(&self) -> Value {
            json!({ "type": "object", "properties": { "pattern": { "type": "string" } } })
        }
        async fn run(&self, _input: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    fn registry() -> ToolRegistry {
        let mut r = ToolRegistry::new();
        r.register(Fake);
        r
    }

    fn empty_mcp() -> (AvailableMcp, ActiveServers) {
        let (startup, avail) = AvailableMcp::partition(
            &entanglement_core::Catalog {
                providers: Vec::new(),
            },
            &HashMap::new(),
            Vec::new(),
        );
        assert!(startup.is_empty());
        (avail, Arc::new(Mutex::new(HashMap::new())))
    }

    #[test]
    fn parse_query_is_lenient_and_case_folds() {
        assert_eq!(parse_query(""), None);
        assert_eq!(parse_query("{}"), None);
        assert_eq!(
            parse_query(r#"{"query":"Weather"}"#),
            Some("weather".to_string())
        );
        assert_eq!(parse_query("not json"), None);
    }

    #[test]
    fn describable_candidates_excludes_hints_and_skills() {
        let rows = vec![
            json!({ "name": "glob", "description": "d", "source": "built-in" }),
            json!({ "name": "docs", "description": "d", "source": "mcp (allowed)" }),
            json!({ "name": "git", "description": "d", "source": "skill" }),
            json!({ "name": "mcp__docs__search", "description": "d", "source": "mcp:docs (enabled)" }),
        ];
        let candidates = describable_candidates(rows);
        assert_eq!(
            candidates,
            vec!["glob".to_string(), "mcp__docs__search".to_string()]
        );
    }

    #[test]
    fn describable_candidates_caps_at_max_results() {
        let rows: Vec<Value> = (0..20)
            .map(|i| json!({ "name": format!("t{i}"), "description": "d", "source": "built-in" }))
            .collect();
        assert_eq!(describable_candidates(rows).len(), MAX_RESULTS);
    }

    #[tokio::test]
    async fn a_matching_builtin_resolves_to_a_flat_responses_tool_entry_and_marks_discovered() {
        let reg = registry();
        let (avail, active) = empty_mcp();
        let skills = SkillRegistry::default();
        let advertising = AdvertisingState::new();
        let session = SessionId::new("s");

        let rows = build_index(&reg, &avail, &active, &skills, &session, Some("glob"));
        let candidates = describable_candidates(rows);
        assert_eq!(candidates, vec!["glob".to_string()]);

        let spec = resolve_spec(&reg, None, &session, "glob").await.unwrap();
        let entry = to_responses_tool_entry(&spec);
        assert_eq!(entry["type"], "function");
        assert_eq!(entry["name"], "glob");
        assert_eq!(entry["defer_loading"], true);
        assert!(entry.get("schema").is_none());
        assert!(entry["parameters"]["properties"]["pattern"].is_object());

        // `run_tool_search`'s own marking step (exercised directly here,
        // matching `describe`'s equivalent test — `run_tool_search` itself
        // needs a live `Holly` to reply through, so the dispatch call isn't
        // exercised end to end, same testing posture as `run_describe`/
        // `run_explore`).
        advertising
            .discovered
            .lock()
            .unwrap()
            .mark(&session, "glob");
        assert!(advertising
            .discovered
            .lock()
            .unwrap()
            .contains(&session, "glob"));
    }

    #[test]
    fn a_skill_named_row_is_excluded_from_candidates() {
        let mut skills = SkillRegistry::default();
        skills.insert(SkillMeta {
            name: "git".to_string(),
            description: "commit helpers".to_string(),
            user_only: false,
            allowed_tools: None,
            root_dir: None,
            body: String::new(),
        });
        let reg = ToolRegistry::new();
        let (avail, active) = empty_mcp();
        let session = SessionId::new("s");
        let rows = build_index(&reg, &avail, &active, &skills, &session, None);
        assert!(rows.iter().any(|r| r["name"] == "git"));
        // The skill row is excluded, but the runtime-owned pseudo-tools
        // (`poll`/`ask_user`/`update_tasks`/…) still show up as `built-in`
        // rows even with an empty registry — `build_index` always includes
        // them.
        let candidates = describable_candidates(rows);
        assert!(!candidates.contains(&"git".to_string()));
    }
}
