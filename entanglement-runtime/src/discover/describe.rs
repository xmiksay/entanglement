//! `describe(names)` (#560, ADR-0196 §4): the full schema for one or more
//! discovered names, byte-identical in shape to a native `<tools>` entry —
//! the serialized [`ToolSpec`] itself, not a friendlier rendering. Under the
//! `client_side` encoding (§3, the only one this phase implements) a
//! successfully resolved name also joins the session's discovered set, so
//! the next round's `tool_spec_resolver` advertises it directly.

use entanglement_core::{Holly, SessionId, ToolAdvertising, ToolSpec};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::mcp::McpScopes;
use crate::seam;
use crate::skills::SkillRegistry;
use crate::tool_advertising::AdvertisingState;
use crate::tools::{closest_name, ToolRegistry};

use super::runtime_owned_specs;

/// `ToolSpec` carries no `Serialize` impl (it's assembled straight from a
/// registered [`Tool`][crate::tools::Tool]'s name/description/schema, never
/// round-tripped through JSON on the advertising path) — this is the
/// byte-identical rendering `describe` promises: the same three fields, same
/// names, no reformatting. `pub(crate)`: also reused by
/// [`crate::arg_validate`] (ADR-0196 §6) so a schema-violation decline's
/// "correct usage" block is byte-identical to what `describe`/the native
/// `<tools>` entry would show — never a second, drifting rendering.
pub(crate) fn spec_to_json(spec: &ToolSpec) -> Value {
    json!({
        "name": spec.name,
        "description": spec.description,
        "schema": spec.schema,
    })
}

#[derive(Deserialize)]
struct Input {
    #[serde(default)]
    names: Vec<String>,
}

/// One `resolve_spec` outcome that isn't a spec: `Some(msg)` is an MCP
/// connect/auth failure with an already-good user-facing message (ADR-0188's
/// `overlay_registry_for_call`); `None` is a plain "no such name".
type ResolveErr = Option<String>;

/// Look up one name's spec: first the runtime-owned pseudo-tools that
/// dispatch by name rather than living in the registry, then — for an
/// `mcp__*` name under a scoped session — the session-scoped registry view
/// (ADR-0188), else the plain registry snapshot.
async fn resolve_spec(
    registry: &ToolRegistry,
    mcp_scopes: Option<&McpScopes>,
    session: &SessionId,
    name: &str,
) -> Result<ToolSpec, ResolveErr> {
    if let Some(spec) = runtime_owned_specs().into_iter().find(|s| s.name == name) {
        return Ok(spec);
    }
    if name.starts_with("mcp__") {
        if let Some(scopes) = mcp_scopes {
            let scoped = scopes
                .overlay_registry_for_call(session, registry.clone(), name)
                .await
                .map_err(Some)?;
            return scoped.spec_for(name).ok_or(None);
        }
    }
    registry.spec_for(name).ok_or(None)
}

/// Build the "unknown name" entry: an already-good MCP error verbatim, a
/// distinct note when `name` is actually a skill (loaded via `load_skill`,
/// never `describe`), else a closest-match hint over the describable
/// vocabulary (registry names plus the runtime-owned pseudo-tools).
fn unknown_entry(
    registry: &ToolRegistry,
    skills: &SkillRegistry,
    name: &str,
    mcp_error: ResolveErr,
) -> Value {
    if let Some(msg) = mcp_error {
        return json!({ "name": name, "error": msg });
    }
    if skills.get(name).is_some() {
        return json!({
            "name": name,
            "error": format!(
                "`{name}` is a skill, not a tool — load it with load_skill, not describe"
            )
        });
    }
    let mut candidates: Vec<String> = registry.names();
    candidates.extend(runtime_owned_specs().into_iter().map(|s| s.name));
    candidates.sort();
    candidates.dedup();
    let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
    let mut msg = format!("unknown tool: `{name}`");
    match closest_name(name, &refs) {
        Some(hint) => msg.push_str(&format!(
            " — did you mean `{hint}`? (see explore for the full catalog)"
        )),
        None => msg.push_str(" — see explore for the full catalog"),
    }
    json!({ "name": name, "error": msg })
}

/// Resolve every requested name against the given inputs, marking each
/// success into `discovered` when `mode` is `ToolSearch` — the pure core of
/// `describe`, independent of the tool round-trip.
async fn build_entries(
    registry: &ToolRegistry,
    skills: &SkillRegistry,
    mcp_scopes: Option<&McpScopes>,
    advertising: &AdvertisingState,
    session: &SessionId,
    mode: ToolAdvertising,
    names: &[String],
) -> Vec<Value> {
    let mut entries = Vec::with_capacity(names.len());
    for name in names {
        match resolve_spec(registry, mcp_scopes, session, name).await {
            Ok(spec) => {
                if mode == ToolAdvertising::ToolSearch {
                    advertising
                        .discovered
                        .lock()
                        .expect("discovered-tool mutex poisoned")
                        .mark(session, name);
                }
                entries.push(spec_to_json(&spec));
            }
            Err(mcp_error) => entries.push(unknown_entry(registry, skills, name, mcp_error)),
        }
    }
    entries
}

/// Dispatch `describe`: parse, resolve every name, reply. Always-`Allow`/
/// non-maskable per ADR-0196 §4 (enforced by the executor's dispatch ladder,
/// not here) — a malformed call or an unresolved name each become one entry
/// in the reply, never a hard tool failure, so a batch of mixed hits/misses
/// still reaches the model in one round-trip.
#[allow(clippy::too_many_arguments)]
pub async fn run_describe(
    holly: &Holly,
    registry: ToolRegistry,
    skills: &SkillRegistry,
    mcp_scopes: Option<&McpScopes>,
    advertising: &AdvertisingState,
    session: SessionId,
    request_id: String,
    input: String,
) {
    let names = match serde_json::from_str::<Input>(&input) {
        Ok(Input { names }) if !names.is_empty() => names,
        Ok(_) => {
            seam::reply(
                holly,
                session,
                request_id,
                "describe requires a non-empty `names` array".to_string(),
                true,
            )
            .await;
            return;
        }
        Err(e) => {
            seam::reply(
                holly,
                session,
                request_id,
                format!("describe expects {{\"names\": [string, ...]}} — {e}"),
                true,
            )
            .await;
            return;
        }
    };

    let mode = advertising.mode(&session);
    let entries = build_entries(
        &registry,
        skills,
        mcp_scopes,
        advertising,
        &session,
        mode,
        &names,
    )
    .await;
    let output = serde_json::to_string_pretty(&entries).unwrap_or_default();
    seam::reply(holly, session, request_id, output, false).await;
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use entanglement_core::{StoredAuth, TokenStore};

    use super::*;
    use crate::mcp::{McpScope, McpServerConfig};
    use crate::skills::SkillMeta;
    use crate::tools::Tool;

    struct Fake;
    #[async_trait]
    impl Tool for Fake {
        fn name(&self) -> Cow<'static, str> {
            Cow::Borrowed("glob")
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

    #[tokio::test]
    async fn a_registered_tool_describes_byte_identical_to_its_registry_spec() {
        let reg = registry();
        let advertising = AdvertisingState::new();
        let session = SessionId::new("s");
        let entries = build_entries(
            &reg,
            &SkillRegistry::default(),
            None,
            &advertising,
            &session,
            ToolAdvertising::ToolSearch,
            &["glob".to_string()],
        )
        .await;
        let expected = spec_to_json(&reg.spec_for("glob").unwrap());
        assert_eq!(entries[0], expected);
        // ToolSearch mode marks it discovered.
        assert_eq!(
            advertising.discovered.lock().unwrap().names(&session),
            vec!["glob".to_string()]
        );
    }

    #[tokio::test]
    async fn full_mode_resolves_but_does_not_mark_discovered() {
        let reg = registry();
        let advertising = AdvertisingState::new();
        let session = SessionId::new("s");
        let entries = build_entries(
            &reg,
            &SkillRegistry::default(),
            None,
            &advertising,
            &session,
            ToolAdvertising::Full,
            &["glob".to_string()],
        )
        .await;
        // A real spec entry carries `schema`, never `error` — both a real
        // spec and an unknown-name entry carry `name`, so that alone can't
        // distinguish them.
        assert!(entries[0].get("schema").is_some());
        assert!(entries[0].get("error").is_none());
        assert!(advertising
            .discovered
            .lock()
            .unwrap()
            .names(&session)
            .is_empty());
    }

    #[tokio::test]
    async fn a_runtime_owned_pseudo_tool_describes_too() {
        let reg = ToolRegistry::new();
        let advertising = AdvertisingState::new();
        let session = SessionId::new("s");
        let entries = build_entries(
            &reg,
            &SkillRegistry::default(),
            None,
            &advertising,
            &session,
            ToolAdvertising::ToolSearch,
            &["poll".to_string()],
        )
        .await;
        assert_eq!(entries[0]["name"], "poll");
    }

    #[tokio::test]
    async fn unknown_name_gets_a_closest_match_hint() {
        let reg = registry();
        let advertising = AdvertisingState::new();
        let session = SessionId::new("s");
        let entries = build_entries(
            &reg,
            &SkillRegistry::default(),
            None,
            &advertising,
            &session,
            ToolAdvertising::ToolSearch,
            &["glbo".to_string()],
        )
        .await;
        let err = entries[0]["error"].as_str().unwrap();
        assert!(err.contains("did you mean `glob`"), "{err}");
    }

    #[tokio::test]
    async fn a_skill_name_gets_a_distinct_not_describable_note() {
        let reg = registry();
        let mut skills = SkillRegistry::default();
        skills.insert(SkillMeta {
            name: "git".to_string(),
            description: "commit helpers".to_string(),
            user_only: false,
            allowed_tools: None,
            root_dir: None,
            body: String::new(),
        });
        let advertising = AdvertisingState::new();
        let session = SessionId::new("s");
        let entries = build_entries(
            &reg,
            &skills,
            None,
            &advertising,
            &session,
            ToolAdvertising::ToolSearch,
            &["git".to_string()],
        )
        .await;
        let err = entries[0]["error"].as_str().unwrap();
        assert!(err.contains("load_skill"), "{err}");
    }

    struct EmptyStore;
    impl TokenStore for EmptyStore {
        fn load(&self, _server: &str) -> anyhow::Result<Option<StoredAuth>> {
            Ok(None)
        }
        fn save(&self, _server: &str, _auth: &StoredAuth) -> anyhow::Result<()> {
            Ok(())
        }
        fn delete(&self, _server: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn an_mcp_name_under_an_unauthenticated_scope_surfaces_the_scope_error() {
        let reg = registry();
        let server_cfg: McpServerConfig =
            serde_yaml::from_str("url: https://192.0.2.1/mcp\noauth: {}").unwrap();
        let servers = HashMap::from([("kb".to_string(), server_cfg)]);
        let key = "user-a".to_string();
        let scopes = McpScopes::new(
            Arc::new(move |_session| {
                Some(McpScope {
                    key: key.clone(),
                    servers: servers.clone(),
                    token_store: Some(Arc::new(EmptyStore)),
                })
            }),
            entanglement_core::HttpClient::new().unwrap(),
            Vec::new(),
        );
        let advertising = AdvertisingState::new();
        let session = SessionId::new("s");
        let entries = build_entries(
            &reg,
            &SkillRegistry::default(),
            Some(scopes.as_ref()),
            &advertising,
            &session,
            ToolAdvertising::ToolSearch,
            &["mcp__kb__search".to_string()],
        )
        .await;
        let err = entries[0]["error"].as_str().unwrap();
        assert!(err.contains("requires authorization"), "{err}");
    }
}
