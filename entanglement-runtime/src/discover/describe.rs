//! `describe(names)` (#560, ADR-0196 §4): the full schema for one or more
//! discovered names, byte-identical in shape to a native `<tools>` entry —
//! the serialized [`ToolSpec`] itself, not a friendlier rendering. Under the
//! `client_side` encoding (§3, the only one this phase implements) a
//! successfully resolved name also joins the session's discovered set, so
//! the next round's `tool_spec_resolver` advertises it directly.

use entanglement_core::{ContentPart, Holly, SessionId, ToolAdvertising, ToolSpec};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::mcp::McpScopes;
use crate::seam;
use crate::skills::SkillRegistry;
use crate::tool_advertising::{AdvertisingState, Encoding};
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
/// `overlay_registry_for_call`); `None` is a plain "no such name". `pub(crate)`:
/// also the return-error shape [`super::tool_search`] (P7) discards through
/// (a search candidate that fails to resolve is simply skipped, never
/// surfaced as a schema-shaped error entry the way `describe` does).
pub(crate) type ResolveErr = Option<String>;

/// Look up one name's spec: first the runtime-owned pseudo-tools that
/// dispatch by name rather than living in the registry, then — for an
/// `mcp__*` name under a scoped session — the session-scoped registry view
/// (ADR-0188), else the plain registry snapshot. `pub(crate)`: also reused by
/// [`super::tool_search`] (P7, ADR-0196 §3) to resolve each of its own
/// search-index candidates to a full spec.
pub(crate) async fn resolve_spec(
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

/// The short-line reply for a name whose schema this session's
/// [`DiscoveredSet`][crate::tool_advertising::DiscoveredSet] already shows as
/// delivered (#560 describe-dedup follow-up: a model stuck re-`describe`-ing
/// an unchanging tool — seen looping up to 131 times on one name — must not
/// keep re-paying, and re-reading, the same schema JSON every round). Worded
/// per whether the discovered tail is actually growing the advertised array
/// (ADR-0200's `advertise_discovered`): an append-mode session can truthfully
/// say the tool is now directly callable; a frozen-array session must not —
/// the schema stands from where it was first shown, but the tool never
/// joined the advertised list, so claiming otherwise would be false.
fn already_delivered_entry(
    advertising: &AdvertisingState,
    session: &SessionId,
    name: &str,
) -> Value {
    let note = if advertising.advertise_discovered(session) {
        format!("{name}: schema already provided above — the tool is ready to call")
    } else {
        format!(
            "{name}: schema already provided above — this session does not \
             auto-advertise discovered tools (the tool list won't show it), \
             but the schema already shown is still valid and the call will \
             still dispatch"
        )
    };
    json!({ "name": name, "note": note })
}

/// Resolve every requested name against the given inputs, marking each new
/// success into `discovered` when `mode` is `ToolSearch` — the pure core of
/// `describe`, independent of the tool round-trip. A name already present in
/// `discovered` (a prior `describe()` this session, or an `arg_validate`
/// schema-violation decline, ADR-0196 §6 — same set) short-circuits to
/// [`already_delivered_entry`] instead of re-resolving and re-emitting the
/// full schema (#560 describe-dedup follow-up). Returns the schema entries
/// alongside the names that were *newly* resolved this round (in call
/// order) — the `anthropic_native` encoding (ADR-0196 §3) needs that second
/// list to build `tool_reference` parts for exactly the tools now safely
/// referenceable (each one's full definition is present, `defer_loading:
/// true`, in the same request's `tools` array); a name that failed to
/// resolve, or was already delivered in an earlier round, is deliberately
/// excluded.
async fn build_entries(
    registry: &ToolRegistry,
    skills: &SkillRegistry,
    mcp_scopes: Option<&McpScopes>,
    advertising: &AdvertisingState,
    session: &SessionId,
    mode: ToolAdvertising,
    names: &[String],
) -> (Vec<Value>, Vec<String>) {
    let mut entries = Vec::with_capacity(names.len());
    let mut resolved = Vec::new();
    for name in names {
        let already_delivered = advertising
            .discovered
            .lock()
            .expect("discovered-tool mutex poisoned")
            .contains(session, name);
        if already_delivered {
            entries.push(already_delivered_entry(advertising, session, name));
            continue;
        }
        match resolve_spec(registry, mcp_scopes, session, name).await {
            Ok(spec) => {
                if mode == ToolAdvertising::ToolSearch {
                    advertising
                        .discovered
                        .lock()
                        .expect("discovered-tool mutex poisoned")
                        .mark(session, name);
                }
                resolved.push(name.clone());
                entries.push(spec_to_json(&spec));
            }
            Err(mcp_error) => entries.push(unknown_entry(registry, skills, name, mcp_error)),
        }
    }
    (entries, resolved)
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
    let (entries, resolved) = build_entries(
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

    // ADR-0196 §3, `anthropic_native` encoding: append a `tool_reference`
    // part per newly-resolved name alongside the schema text this reply
    // already carries — the API auto-expands each reference into the
    // matching (already-sent, `defer_loading: true`) tool definition. Every
    // other encoding, and a `Full`-mode session (where nothing is deferred
    // to begin with), keeps the plain text reply unchanged.
    if mode == ToolAdvertising::ToolSearch
        && advertising.encoding(&session) == Encoding::AnthropicNative
        && !resolved.is_empty()
    {
        let mut content = vec![ContentPart::text(output)];
        content.extend(resolved.into_iter().map(ContentPart::tool_reference));
        seam::reply_content(holly, session, request_id, content, false, None, None).await;
    } else {
        seam::reply(holly, session, request_id, output, false).await;
    }
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

    struct FakeGrep;
    #[async_trait]
    impl Tool for FakeGrep {
        fn name(&self) -> Cow<'static, str> {
            Cow::Borrowed("grep")
        }
        fn description(&self) -> &str {
            "search file contents"
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

    fn registry_with_grep() -> ToolRegistry {
        let mut r = registry();
        r.register(FakeGrep);
        r
    }

    #[tokio::test]
    async fn a_registered_tool_describes_byte_identical_to_its_registry_spec() {
        let reg = registry();
        let advertising = AdvertisingState::new();
        let session = SessionId::new("s");
        let (entries, _resolved) = build_entries(
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
    async fn an_endpoint_tool_describes_byte_identical_to_its_registry_spec() {
        // #560 P8: `describe` needs zero endpoint-specific code — a
        // registered `endpoint__*`/`skill__*__*` tool resolves through the
        // exact same `registry.spec_for` path as any other tool.
        let mut reg = registry();
        let cfg: crate::endpoint::EndpointConfig =
            serde_yaml::from_str("url: https://example.com/x\ndescription: weather lookup")
                .unwrap();
        reg.register(crate::endpoint::EndpointTool::new(
            "endpoint__weather".to_string(),
            &cfg,
            entanglement_core::HttpClient::new().unwrap(),
        ));
        let advertising = AdvertisingState::new();
        let session = SessionId::new("s");
        let (entries, resolved) = build_entries(
            &reg,
            &SkillRegistry::default(),
            None,
            &advertising,
            &session,
            ToolAdvertising::ToolSearch,
            &["endpoint__weather".to_string()],
        )
        .await;
        let expected = spec_to_json(&reg.spec_for("endpoint__weather").unwrap());
        assert_eq!(entries[0], expected);
        assert_eq!(resolved, vec!["endpoint__weather".to_string()]);
    }

    #[tokio::test]
    async fn resolved_names_list_matches_only_successful_lookups() {
        // ADR-0196 §3: `run_describe` needs exactly this list to know which
        // names are safe to reference on the `anthropic_native` encoding —
        // an unresolved name must never appear in it.
        let reg = registry();
        let advertising = AdvertisingState::new();
        let session = SessionId::new("s");
        let (entries, resolved) = build_entries(
            &reg,
            &SkillRegistry::default(),
            None,
            &advertising,
            &session,
            ToolAdvertising::ToolSearch,
            &["glob".to_string(), "nope".to_string()],
        )
        .await;
        assert_eq!(entries.len(), 2);
        assert_eq!(resolved, vec!["glob".to_string()]);
    }

    #[tokio::test]
    async fn full_mode_resolves_but_does_not_mark_discovered() {
        let reg = registry();
        let advertising = AdvertisingState::new();
        let session = SessionId::new("s");
        let (entries, _resolved) = build_entries(
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
        let (entries, _resolved) = build_entries(
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
        let (entries, _resolved) = build_entries(
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
            tools: Vec::new(),
        });
        let advertising = AdvertisingState::new();
        let session = SessionId::new("s");
        let (entries, _resolved) = build_entries(
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
        let (entries, _resolved) = build_entries(
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

    /// Pin a session's mode/encoding directly through the public `modes`
    /// field — mirrors `tool_advertising::overlay`'s own test helper; the
    /// executor loop normally does this via `AdvertisingInputs` off a
    /// `SessionStarted`, more machinery than these tests need.
    fn pin(advertising: &AdvertisingState, session: &SessionId, encoding: Encoding) {
        advertising.modes.lock().unwrap().pin(
            session.clone(),
            ToolAdvertising::ToolSearch,
            encoding,
        );
    }

    /// #560 describe-dedup follow-up: a second `describe()` of a name already
    /// delivered this session gets a short pointer line, not the full schema
    /// again — the fix for a model looping `describe(["glob"])` up to 131
    /// times, each reply re-paying the full schema JSON.
    #[tokio::test]
    async fn second_describe_of_the_same_name_sends_a_short_line_not_the_full_schema() {
        let reg = registry();
        let advertising = AdvertisingState::new();
        let session = SessionId::new("s");

        let (first, _resolved) = build_entries(
            &reg,
            &SkillRegistry::default(),
            None,
            &advertising,
            &session,
            ToolAdvertising::ToolSearch,
            &["glob".to_string()],
        )
        .await;
        assert!(
            first[0].get("schema").is_some(),
            "first delivery is full: {first:?}"
        );

        let (second, resolved) = build_entries(
            &reg,
            &SkillRegistry::default(),
            None,
            &advertising,
            &session,
            ToolAdvertising::ToolSearch,
            &["glob".to_string()],
        )
        .await;
        assert!(
            second[0].get("schema").is_none(),
            "repeat delivery must not resend the schema: {second:?}"
        );
        let note = second[0]["note"].as_str().expect("a short note entry");
        assert!(note.contains("glob"), "{note}");
        assert!(note.contains("schema already provided above"), "{note}");
        assert!(
            note.contains("ready to call"),
            "default (append) session must say the tool is callable: {note}"
        );
        assert!(
            resolved.is_empty(),
            "a repeat delivery is not newly resolved, so it must not appear in `resolved`: {resolved:?}"
        );
    }

    /// #560 follow-up: a request mixing an already-delivered name with a new
    /// one sends the full schema for the new tool and the short line for the
    /// repeat, in the same reply.
    #[tokio::test]
    async fn mixed_request_sends_full_schema_for_new_and_a_short_line_for_repeats() {
        let reg = registry_with_grep();
        let advertising = AdvertisingState::new();
        let session = SessionId::new("s");
        advertising
            .discovered
            .lock()
            .unwrap()
            .mark(&session, "glob");

        let (entries, resolved) = build_entries(
            &reg,
            &SkillRegistry::default(),
            None,
            &advertising,
            &session,
            ToolAdvertising::ToolSearch,
            &["glob".to_string(), "grep".to_string()],
        )
        .await;

        assert!(
            entries[0].get("note").is_some() && entries[0].get("schema").is_none(),
            "glob was already delivered: {:?}",
            entries[0]
        );
        assert!(
            entries[1].get("schema").is_some() && entries[1].get("note").is_none(),
            "grep is new, so it gets the full schema: {:?}",
            entries[1]
        );
        assert_eq!(
            resolved,
            vec!["grep".to_string()],
            "only the newly-resolved name joins `resolved`"
        );
    }

    /// #560 follow-up: under a `client_side` session that opted out of
    /// growing its advertised array (ADR-0200's `advertise_discovered:
    /// false`), the short-line wording must not claim the tool is now
    /// "ready to call" — it never joined the advertised list, so that would
    /// be false. It states the truth instead: the schema stands, the tool
    /// list just won't show it.
    #[tokio::test]
    async fn already_delivered_wording_is_truthful_under_a_frozen_advertised_array() {
        let reg = registry();
        let advertising = AdvertisingState::new();
        let session = SessionId::new("s");
        pin(&advertising, &session, Encoding::ClientSide);
        advertising
            .modes
            .lock()
            .unwrap()
            .set_advertise_discovered(&session, false);
        advertising
            .discovered
            .lock()
            .unwrap()
            .mark(&session, "glob");

        let (entries, _resolved) = build_entries(
            &reg,
            &SkillRegistry::default(),
            None,
            &advertising,
            &session,
            ToolAdvertising::ToolSearch,
            &["glob".to_string()],
        )
        .await;
        let note = entries[0]["note"].as_str().expect("a short note entry");
        assert!(note.contains("schema already provided above"), "{note}");
        assert!(
            !note.contains("ready to call"),
            "a frozen-array session must not falsely claim the tool is now callable: {note}"
        );
        assert!(
            note.contains("does not auto-advertise"),
            "the wording must truthfully explain why: {note}"
        );
    }
}
