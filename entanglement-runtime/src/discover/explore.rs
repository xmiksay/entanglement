//! `explore(filter?)` (#560, ADR-0196 §4): a terse, always-live index — name,
//! one-line description, source — over every dynamic tool source. Computed
//! fresh from the registry/MCP/skill state on every call, never cached, so
//! there's no staleness window to announce across (pull-only discovery).

use std::collections::HashSet;

use entanglement_core::{Holly, SessionId};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::mcp::{ActiveServers, AvailableMcp};
use crate::seam;
use crate::skills::SkillRegistry;
use crate::tools::ToolRegistry;

use super::kinds::{self, KindsCtx, TopKind};
use super::runtime_owned_specs;
use super::sections::{kind_of, render_sections, Kind};

#[derive(Deserialize, Default)]
struct Input {
    #[serde(default)]
    filter: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

/// One index row — `pub(super)`, not module-private: [`super::sections`]
/// builds [`super::sections::IndexRow`]/renders `explore`'s sectioned text
/// straight off this shape, and needs to see it (and [`build_rows`] below)
/// from a sibling module.
pub(super) struct Row {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) source: String,
}

/// First line of a spec description, capped — `explore`'s index is a terse
/// roster (`describe` carries the full text); some host-tool descriptions
/// (`bash`, `rhai`) run to several paragraphs.
const MAX_ONE_LINE: usize = 160;

fn one_line(description: &str) -> String {
    let first = description.lines().next().unwrap_or_default();
    if first.chars().count() > MAX_ONE_LINE {
        let truncated: String = first.chars().take(MAX_ONE_LINE).collect();
        format!("{truncated}…")
    } else {
        first.to_string()
    }
}

/// Parse the optional `{"filter": "...", "kind": "..."}` input, tolerating an
/// empty body (no filter, no kind) — `explore` is read-only and low-stakes,
/// so a malformed filter degrades to "show everything" rather than a hard
/// error. `kind` always resolves to *some* [`TopKind`] — an unrecognized
/// string falls through to [`TopKind::Tools`]`(None)`, the same "show
/// everything" degrade the tool-index sub-filter has always had (ADR-0207
/// §12: `agents`/`skills`/`models`/`modes` are new top-level values; the
/// existing `tool`/`mcp`/`skill`/`endpoint` sub-filters are unchanged).
fn parse_input(input: &str) -> (Option<String>, TopKind) {
    if input.trim().is_empty() {
        return (None, TopKind::Tools(None));
    }
    let Ok(parsed) = serde_json::from_str::<Input>(input) else {
        return (None, TopKind::Tools(None));
    };
    let filter = parsed.filter.map(|f| f.to_ascii_lowercase());
    let top = TopKind::parse(parsed.kind.as_deref());
    (filter, top)
}

/// Build every row, apply the filter, sort by name — the pure core of
/// `explore`, independent of the tool round-trip so it's unit-testable with
/// no engine in the loop. `pub(crate)`: also reused by
/// [`super::tool_search`] (P7, ADR-0196 §3) to answer a `responses_native`
/// wire's native `tool_search_call` with the same live index `explore`
/// itself serves.
pub(crate) fn build_index(
    registry: &ToolRegistry,
    avail: &AvailableMcp,
    active: &ActiveServers,
    skills: &SkillRegistry,
    session: &SessionId,
    filter: Option<&str>,
) -> Vec<Value> {
    build_rows(registry, avail, active, skills, session, filter, None)
        .into_iter()
        .map(row_to_json)
        .collect()
}

fn row_to_json(r: Row) -> Value {
    json!({ "name": r.name, "description": r.description, "source": r.source })
}

/// The pure core shared by [`build_index`] (no `kind` filter — every caller
/// but `explore` itself, incl. [`super::tool_search`]) and `explore`'s own
/// dispatch, which additionally narrows to one section (#560 P9, ADR-0199).
/// `pub(super)`: also the row source [`super::sections::index_rows`] builds
/// on for the TUI `/tools` view.
pub(super) fn build_rows(
    registry: &ToolRegistry,
    avail: &AvailableMcp,
    active: &ActiveServers,
    skills: &SkillRegistry,
    session: &SessionId,
    filter: Option<&str>,
    kind: Option<Kind>,
) -> Vec<Row> {
    let mut rows = builtin_rows(registry);
    rows.extend(mcp_rows(registry, avail, active, session));
    rows.extend(endpoint_rows(registry));
    rows.extend(skill_tool_rows(registry, skills));
    rows.extend(skill_rows(skills));

    if let Some(f) = filter {
        rows.retain(|r| {
            r.name.to_ascii_lowercase().contains(f)
                || r.description.to_ascii_lowercase().contains(f)
        });
    }
    if let Some(k) = kind {
        rows.retain(|r| kind_of(&r.source) == k);
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

/// Dispatch `explore`: parse, build, reply. Always-`Allow`/non-maskable per
/// ADR-0196 §4 (enforced by the executor's dispatch ladder, not here).
/// `kinds_ctx` answers the four non-tool `kind`s (ADR-0207 §12); `pending` is
/// handled earlier, at the ladder (see [`super::kinds::peek_kind`]), since it
/// needs state this function's signature doesn't carry.
#[allow(clippy::too_many_arguments)]
pub async fn run_explore(
    holly: &Holly,
    registry: &ToolRegistry,
    avail: &AvailableMcp,
    active: &ActiveServers,
    skills: &SkillRegistry,
    kinds_ctx: &KindsCtx,
    session: SessionId,
    request_id: String,
    input: String,
) {
    let (filter, top) = parse_input(&input);
    let output = match top {
        TopKind::Agents => kinds::agents_index(kinds_ctx),
        TopKind::Skills => kinds::skills_index(kinds_ctx),
        TopKind::Models => kinds::models_index(kinds_ctx),
        TopKind::Modes => kinds::modes_index(kinds_ctx),
        TopKind::Tools(kind) => {
            let rows = build_rows(
                registry,
                avail,
                active,
                skills,
                &session,
                filter.as_deref(),
                kind,
            );
            render_sections(rows)
        }
    };
    seam::reply(holly, session, request_id, output, false).await;
}

/// Registered host tools — minus `read_raw` (an internal rhai-binding alias,
/// never advertised, ADR-0098) and `mcp__*` (covered by [`mcp_rows`] with a
/// richer per-server source label) — plus the runtime-owned pseudo-tools that
/// dispatch by name rather than living in the registry (`poll`/`ask_user`/
/// `update_tasks`/`rhai`). Deliberately includes tools already in the lean
/// kernel too — a redundant-but-harmless row, matching how `describe` stays
/// answerable for a kernel name.
fn builtin_rows(registry: &ToolRegistry) -> Vec<Row> {
    let mut rows: Vec<Row> = registry
        .specs()
        .into_iter()
        .filter(|s| {
            s.name != "read_raw"
                && !s.name.starts_with("mcp__")
                // Definition-driven sources (#560 P8) get their own rows
                // below, with a distinct `source` label — an endpoint tool
                // isn't a "built-in" any more than an MCP tool is.
                && !s.name.starts_with("endpoint__")
                && !s.name.starts_with("skill__")
        })
        .map(|s| Row {
            name: s.name,
            description: one_line(&s.description),
            source: "built-in".to_string(),
        })
        .collect();
    rows.extend(runtime_owned_specs().into_iter().map(|s| Row {
        name: s.name,
        description: one_line(&s.description),
        source: "built-in".to_string(),
    }));
    rows
}

/// `config.yml`-declared endpoint tools (#560 P8) — every registered
/// `endpoint__<name>` tool, one row each.
fn endpoint_rows(registry: &ToolRegistry) -> Vec<Row> {
    registry
        .specs()
        .into_iter()
        .filter(|s| s.name.starts_with("endpoint__"))
        .map(|s| Row {
            name: s.name,
            description: one_line(&s.description),
            source: "endpoint".to_string(),
        })
        .collect()
}

/// Skill-declared tools (#560 P8) — every registered `skill__<skill>__<name>`
/// tool, sourced `skill:<skill>` so the index shows which skill it came from
/// (distinct from [`skill_rows`]'s own skill-index entries, which point at
/// `load_skill`, not `describe`).
fn skill_tool_rows(registry: &ToolRegistry, skills: &SkillRegistry) -> Vec<Row> {
    let mut rows = Vec::new();
    for skill in skills.iter() {
        let prefix = format!("skill__{}__", skill.name);
        for def in &skill.tools {
            let name = format!("{prefix}{}", def.name());
            let description = registry
                .spec_for(&name)
                .map(|s| one_line(&s.description))
                .unwrap_or_default();
            rows.push(Row {
                name,
                description,
                source: format!("skill:{}", skill.name),
            });
        }
    }
    rows
}

/// MCP servers, three-state (#542): a server currently connected (startup-
/// `enabled`, or lazily `allowed`-then-enabled by this session or an
/// ancestor) lists each of its tools by name; every other `allowed` server —
/// including one connected only for a *different* session — gets one hint
/// row pointing at `mcp_enable`, never auto-connecting; a `disabled` server
/// is absent from both `active` and `avail` by construction, so it never
/// reaches this function at all (existing three-state semantics).
fn mcp_rows(
    registry: &ToolRegistry,
    avail: &AvailableMcp,
    active: &ActiveServers,
    session: &SessionId,
) -> Vec<Row> {
    let snapshot: Vec<(String, Vec<String>)> = active
        .lock()
        .expect("MCP active-server mutex poisoned")
        .iter()
        .map(|(name, entry)| (name.clone(), entry.tools.clone()))
        .collect();
    mcp_rows_from_snapshot(registry, avail, &snapshot, session)
}

/// The pure core of [`mcp_rows`], taking a plain `(server, tool names)`
/// snapshot instead of the live [`ActiveServers`] lock — what a unit test
/// exercises, so it never needs a real connected [`crate::mcp::McpClient`]
/// just to assert on row shape.
fn mcp_rows_from_snapshot(
    registry: &ToolRegistry,
    avail: &AvailableMcp,
    connected: &[(String, Vec<String>)],
    session: &SessionId,
) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut visible_servers: HashSet<String> = HashSet::new();
    for (server, tools) in connected {
        if !avail.server_visible(server, session) {
            continue;
        }
        visible_servers.insert(server.clone());
        for tool in tools {
            let description = registry
                .spec_for(tool)
                .map(|s| one_line(&s.description))
                .unwrap_or_default();
            rows.push(Row {
                name: tool.clone(),
                description,
                source: format!("mcp:{server} (enabled)"),
            });
        }
    }
    for name in avail.available_names() {
        if visible_servers.contains(&name) {
            continue;
        }
        rows.push(Row {
            description: format!(
                "available MCP server — enable with the mcp_enable tool: call \
                 mcp_enable with {{\"server\": \"{name}\"}}"
            ),
            name,
            source: "mcp (allowed)".to_string(),
        });
    }
    rows
}

/// The skill index: name + one-liner, explicitly marked not describable
/// (skills load through `load_skill`, not `describe`).
fn skill_rows(skills: &SkillRegistry) -> Vec<Row> {
    skills
        .disclosures()
        .into_iter()
        .map(|d| Row {
            name: d.name,
            description: format!("{} (load with load_skill — not describable)", d.description),
            source: "skill".to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;
    use crate::mcp::McpServerConfig;
    use crate::skills::SkillMeta;
    use crate::tools::Tool;

    struct Fake {
        name: &'static str,
        desc: &'static str,
    }
    #[async_trait]
    impl Tool for Fake {
        fn name(&self) -> Cow<'static, str> {
            Cow::Borrowed(self.name)
        }
        fn description(&self) -> &str {
            self.desc
        }
        async fn run(&self, _input: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    fn skill_registry() -> SkillRegistry {
        let mut skills = SkillRegistry::default();
        skills.insert(SkillMeta {
            name: "git".to_string(),
            description: "commit and branch helpers".to_string(),
            user_only: false,
            allowed_tools: None,
            root_dir: None,
            body: String::new(),
            tools: Vec::new(),
        });
        skills
    }

    fn allowed_avail(server: &str) -> AvailableMcp {
        let (startup, avail) = AvailableMcp::partition(
            &entanglement_core::Catalog {
                providers: Vec::new(),
            },
            &HashMap::from([(
                server.to_string(),
                McpServerConfig {
                    command: Some("true".into()),
                    args: Vec::new(),
                    env: HashMap::new(),
                    url: None,
                    headers: HashMap::new(),
                    disabled: false,
                    capabilities: HashMap::new(),
                    oauth: None,
                    state: Some(entanglement_core::McpServerState::Allowed),
                },
            )]),
            Vec::new(),
        );
        assert!(startup.is_empty());
        avail
    }

    #[test]
    fn one_line_truncates_long_first_lines_and_keeps_short_ones() {
        assert_eq!(one_line("short"), "short");
        assert_eq!(one_line("first\nsecond"), "first");
        let long = "x".repeat(200);
        let out = one_line(&long);
        assert_eq!(out.chars().count(), MAX_ONE_LINE + 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn parse_input_is_lenient_and_case_folds() {
        assert_eq!(parse_input(""), (None, TopKind::Tools(None)));
        assert_eq!(parse_input("{}"), (None, TopKind::Tools(None)));
        assert_eq!(
            parse_input(r#"{"filter":"Git"}"#),
            (Some("git".to_string()), TopKind::Tools(None))
        );
        assert_eq!(parse_input("not json"), (None, TopKind::Tools(None)));
    }

    #[test]
    fn parse_input_reads_kind_case_insensitively_and_ignores_garbage() {
        assert_eq!(
            parse_input(r#"{"kind":"MCP"}"#),
            (None, TopKind::Tools(Some(Kind::Mcp)))
        );
        assert_eq!(
            parse_input(r#"{"filter":"search","kind":"tool"}"#),
            (Some("search".to_string()), TopKind::Tools(Some(Kind::Tool)))
        );
        // An unrecognized kind degrades to "show everything", same lenient
        // posture as a malformed filter.
        assert_eq!(
            parse_input(r#"{"kind":"bogus"}"#),
            (None, TopKind::Tools(None))
        );
    }

    #[test]
    fn parse_input_recognizes_the_new_top_level_kinds() {
        assert_eq!(parse_input(r#"{"kind":"agents"}"#), (None, TopKind::Agents));
        assert_eq!(parse_input(r#"{"kind":"Skills"}"#), (None, TopKind::Skills));
        assert_eq!(parse_input(r#"{"kind":"models"}"#), (None, TopKind::Models));
        assert_eq!(parse_input(r#"{"kind":"modes"}"#), (None, TopKind::Modes));
    }

    #[test]
    fn index_covers_a_builtin_an_allowed_mcp_hint_and_a_skill_row() {
        let mut registry = ToolRegistry::new();
        registry.register(Fake {
            name: "sample_tool",
            desc: "does a sample thing",
        });
        let avail = allowed_avail("docs");
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let skills = skill_registry();
        let session = SessionId::new("s");

        let rows = build_index(&registry, &avail, &active, &skills, &session, None);

        assert!(rows
            .iter()
            .any(|r| r["name"] == "sample_tool" && r["source"] == "built-in"));
        assert!(rows.iter().any(|r| r["name"] == "docs"
            && r["source"] == "mcp (allowed)"
            && r["description"].as_str().unwrap().contains("mcp_enable")));
        assert!(rows.iter().any(|r| r["name"] == "git"
            && r["source"] == "skill"
            && r["description"].as_str().unwrap().contains("load_skill")));
    }

    #[test]
    fn index_labels_endpoint_and_skill_tool_sources_distinctly() {
        // #560 P8: an `endpoint__*`/`skill__*__*` registered tool must not
        // fall into the generic "built-in" bucket — it gets its own source
        // label, distinguishable from a plain host tool.
        let mut registry = ToolRegistry::new();
        registry.register(Fake {
            name: "endpoint__weather",
            desc: "current weather",
        });
        registry.register(Fake {
            name: "skill__research__gh_search",
            desc: "search github",
        });
        let mut skills = skill_registry();
        skills.insert(crate::skills::SkillMeta {
            name: "research".to_string(),
            description: "d".to_string(),
            user_only: false,
            allowed_tools: None,
            root_dir: None,
            body: String::new(),
            tools: vec![crate::skills::SkillToolDef::Alias {
                name: "gh_search".to_string(),
                target: "read".to_string(),
                args: serde_json::Map::new(),
                description: String::new(),
            }],
        });
        let avail = allowed_avail("docs");
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let session = SessionId::new("s");

        let rows = build_index(&registry, &avail, &active, &skills, &session, None);

        let endpoint_row = rows
            .iter()
            .find(|r| r["name"] == "endpoint__weather")
            .expect("endpoint row present");
        assert_eq!(endpoint_row["source"], "endpoint");
        // Never lumped into the generic built-in bucket.
        assert!(!rows
            .iter()
            .any(|r| r["name"] == "endpoint__weather" && r["source"] == "built-in"));

        let skill_tool_row = rows
            .iter()
            .find(|r| r["name"] == "skill__research__gh_search")
            .expect("skill tool row present");
        assert_eq!(skill_tool_row["source"], "skill:research");
        assert_eq!(skill_tool_row["description"], "search github");
    }

    #[test]
    fn filter_narrows_to_matching_rows_only() {
        let registry = ToolRegistry::new();
        let avail = allowed_avail("docs");
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let skills = skill_registry();
        let session = SessionId::new("s");

        let rows = build_index(&registry, &avail, &active, &skills, &session, Some("git"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], "git");
    }

    #[test]
    fn kind_narrows_to_matching_section_only() {
        let mut registry = ToolRegistry::new();
        registry.register(Fake {
            name: "sample_tool",
            desc: "does a sample thing",
        });
        let avail = allowed_avail("docs");
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let skills = skill_registry();
        let session = SessionId::new("s");

        let mcp_only = build_rows(
            &registry,
            &avail,
            &active,
            &skills,
            &session,
            None,
            Some(Kind::Mcp),
        );
        assert!(mcp_only.iter().all(|r| r.source.starts_with("mcp")));
        assert!(mcp_only.iter().any(|r| r.name == "docs"));

        let tool_only = build_rows(
            &registry,
            &avail,
            &active,
            &skills,
            &session,
            None,
            Some(Kind::Tool),
        );
        assert!(tool_only.iter().all(|r| r.source == "built-in"));
        assert!(tool_only.iter().any(|r| r.name == "sample_tool"));
        assert!(!tool_only.iter().any(|r| r.name == "docs"));
    }

    #[test]
    fn render_sections_groups_under_headers_with_the_mcp_clarifying_line() {
        let rows = vec![
            Row {
                name: "sample_tool".to_string(),
                description: "does a thing".to_string(),
                source: "built-in".to_string(),
            },
            Row {
                name: "docs".to_string(),
                description: "available MCP server".to_string(),
                source: "mcp (allowed)".to_string(),
            },
        ];
        let out = render_sections(rows);
        assert!(out.contains("TOOLS\n"));
        assert!(out.contains("  sample_tool — does a thing\n"));
        assert!(out.contains("mcp_enable"));
        assert!(out.contains("mcp__<server>__<tool>"));
        assert!(out.contains("  docs — available MCP server\n"));
        // No skills/endpoints in this input — those headers are omitted.
        assert!(!out.contains("SKILLS"));
        assert!(!out.contains("ENDPOINTS"));
    }

    #[test]
    fn render_sections_reports_no_matches_on_an_empty_index() {
        assert_eq!(render_sections(Vec::new()), "(no matches)");
    }

    #[test]
    fn a_connected_server_visible_to_this_session_lists_its_tools() {
        let registry = ToolRegistry::new();
        let avail = allowed_avail("docs");
        let session = SessionId::new("s");
        avail.mark_enabled("docs", &session);
        let connected = vec![("docs".to_string(), vec!["mcp__docs__search".to_string()])];

        let rows = mcp_rows_from_snapshot(&registry, &avail, &connected, &session);
        assert!(rows
            .iter()
            .any(|r: &Row| r.name == "mcp__docs__search" && r.source == "mcp:docs (enabled)"));
        // Connected doesn't also duplicate the "allowed" hint row.
        assert!(!rows.iter().any(|r| r.name == "docs"));
    }

    #[test]
    fn a_connected_server_not_visible_to_this_session_stays_a_hint() {
        let registry = ToolRegistry::new();
        let avail = allowed_avail("docs");
        let owner = SessionId::new("owner");
        avail.mark_enabled("docs", &owner);
        let connected = vec![("docs".to_string(), vec!["mcp__docs__search".to_string()])];
        let other = SessionId::new("other");

        let rows = mcp_rows_from_snapshot(&registry, &avail, &connected, &other);
        assert!(!rows.iter().any(|r| r.name == "mcp__docs__search"));
        assert!(rows
            .iter()
            .any(|r| r.name == "docs" && r.source == "mcp (allowed)"));
    }
}
