//! `SKILL.md` `tools:` frontmatter (#560 P8): per-skill tools registered as
//! `skill__<skill>__<tool>`, **strict/native layers only** — the lenient
//! foreign-vendor parse drops the key silently (`skills::parse_foreign_skill`
//! never reads it), since a Claude-Code-style skill directory has no concept
//! of it and entanglement doesn't own the file.
//!
//! Three kinds, one frontmatter list (tagged by `kind`):
//! - [`SkillToolDef::Endpoint`] — the same [`EndpointConfig`]/[`EndpointTool`]
//!   machinery as `config.yml`'s `endpoints:` (P8's other half), just
//!   namespaced under the skill instead of the top level.
//! - [`SkillToolDef::Rhai`] — a script path relative to the skill dir, run
//!   through the *existing* sandboxed `rhai` machinery, graded exactly like
//!   `rhai` (ADR-0046). Implemented as sugar over [`AliasTool`]: registering
//!   `{"kind": "rhai", "script": "…"}` is exactly registering an alias whose
//!   target is `rhai` and whose one preset arg is `script` (the file's
//!   content, read once at registration — see the module doc on why once).
//!   `AliasTool::alias_rewrite` then rewrites the call to `rhai` *before*
//!   grading, so it goes through precisely the same permission chain,
//!   approval flow, and background/timeout handling as the model calling
//!   `rhai` directly — no new execution path to build or keep in sync.
//! - [`SkillToolDef::Alias`] — a renamed/preset-args wrapper over any
//!   existing tool (a host tool, another registered skill tool, an MCP tool
//!   connected before this pass, or a runtime-owned pseudo-tool name). Also
//!   an [`AliasTool`]; grades as the tool it targets (§3 of the plan — an
//!   alias must not launder permission through its own name).
//!
//! # Registration timing: discovery, not `load_skill`
//!
//! The old plan sketch imagined tools appearing only for a "one-turn
//! activation window" after `load_skill`. This implementation registers at
//! **skill-discovery time** instead (this module's [`register_skill_tools`],
//! called once at startup right alongside every other definitions-driven
//! registration) — chosen over gating registration behind `load_skill` for
//! three reasons:
//!
//! 1. **ADR-0194 (skills are additive-only).** A skill no longer masks or
//!    gates anything at runtime — `load_skill` is purely a *disclosure*
//!    mechanism (tier-2 body text), not an activation switch. Tying tool
//!    *registration* to it would resurrect exactly the kind of state
//!    `load_skill` no longer owns.
//! 2. **The advertised tool surface is stable within a session** (this
//!    project's own contract, `.claude/CLAUDE.md`): any mid-session addition
//!    to the registry busts the provider prompt cache from the tools block
//!    onward under `full` mode, and under `tool_search` mode it's simply
//!    unnecessary — these tools are never in the lean kernel, so they're
//!    already reachable *only* via `explore`+`describe` regardless of when
//!    they were registered, exactly like an MCP tool.
//! 3. **`load_skill`'s own handler is a pure, session-free function** (no
//!    `ToolRegistry` handle, no `holly`/session context) — wiring registry
//!    mutation through it would need the same live-mutation machinery MCP's
//!    `mcp_enable` needed (`SharedRegistry`, a `WeakRegistry` handle) for a
//!    feature with no corresponding "skills load asynchronously" requirement.
//!
//! `load_skill`'s own result still lists only tool **names** (never
//! schemas) — a model discovers a skill tool's full schema through
//! `describe`, same as any other non-kernel tool.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use entanglement_core::{EndpointMethod, HttpClient};

use crate::endpoint::{EndpointConfig, EndpointParam, EndpointTool};
use crate::tool_names::{self, RHAI_TOOL};
use crate::tools::ToolRegistry;

use super::alias_tool::AliasTool;
use super::{SkillMeta, SkillRegistry};

/// One `tools:` entry. `deny_unknown_fields` per variant — a typo'd key in a
/// native-layer `SKILL.md` is a loud parse error, matching every other
/// strict section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SkillToolDef {
    Endpoint {
        name: String,
        #[serde(default = "default_method")]
        method: EndpointMethod,
        url: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        params: HashMap<String, EndpointParam>,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        body: Option<String>,
    },
    Rhai {
        name: String,
        /// Path to a `.rhai` script, relative to the skill's own directory
        /// (the directory holding this `SKILL.md`).
        script: String,
        #[serde(default)]
        description: String,
    },
    Alias {
        name: String,
        /// The tool this alias wraps — a literal registered tool name
        /// (`bash`, `mcp__docs__search`, another `skill__…__…` tool) or a
        /// runtime-owned pseudo-tool name (`rhai`, `poll`, …).
        target: String,
        /// Fixed argument overrides merged onto (and winning over) the
        /// caller's own args at call time. Absent ⇒ a pure rename.
        #[serde(default)]
        args: Map<String, Value>,
        #[serde(default)]
        description: String,
    },
}

impl SkillToolDef {
    /// The tool's own short name (unnamespaced) — every variant carries one.
    pub fn name(&self) -> &str {
        match self {
            Self::Endpoint { name, .. } | Self::Rhai { name, .. } | Self::Alias { name, .. } => {
                name
            }
        }
    }
}

fn default_method() -> EndpointMethod {
    EndpointMethod::Get
}

/// Register every discovered skill's `tools:` entries, namespaced
/// `skill__<skill>__<name>`. Two passes: endpoint/rhai-backed tools first,
/// then aliases — so an alias may target a sibling skill tool registered in
/// the first pass (as well as any host/MCP-so-far/pseudo-tool). Per-entry
/// failures (a rhai script that doesn't exist, an alias with an unknown
/// target) are logged and skipped, never fatal to startup — matching how an
/// MCP server that fails to connect is logged and skipped rather than
/// aborting.
pub fn register_skill_tools(tools: &mut ToolRegistry, skills: &SkillRegistry, http: &HttpClient) {
    for skill in skills.iter() {
        for def in &skill.tools {
            match def {
                SkillToolDef::Endpoint { .. } => register_endpoint(tools, skill, def, http),
                SkillToolDef::Rhai { .. } => register_rhai(tools, skill, def),
                SkillToolDef::Alias { .. } => {} // second pass, below
            }
        }
    }
    for skill in skills.iter() {
        for def in &skill.tools {
            if let SkillToolDef::Alias { .. } = def {
                register_alias(tools, skill, def);
            }
        }
    }
}

fn namespaced(skill: &str, name: &str) -> String {
    format!("skill__{skill}__{name}")
}

fn register_endpoint(
    tools: &mut ToolRegistry,
    skill: &SkillMeta,
    def: &SkillToolDef,
    http: &HttpClient,
) {
    let SkillToolDef::Endpoint {
        name,
        method,
        url,
        description,
        params,
        headers,
        body,
    } = def
    else {
        return;
    };
    let cfg = EndpointConfig {
        method: *method,
        url: url.clone(),
        description: description.clone(),
        params: params.clone(),
        headers: headers.clone(),
        body: body.clone(),
    };
    tools.register(EndpointTool::new(
        namespaced(&skill.name, name),
        &cfg,
        http.clone(),
    ));
}

/// Sugar over [`AliasTool`]: target `rhai`, one preset arg (`script`, the
/// file's content read once here). See the module doc for why this is a
/// rewrite rather than a bespoke execution path.
fn register_rhai(tools: &mut ToolRegistry, skill: &SkillMeta, def: &SkillToolDef) {
    let SkillToolDef::Rhai {
        name,
        script,
        description,
    } = def
    else {
        return;
    };
    let Some(dir) = skill.root_dir.as_deref() else {
        tracing::warn!(
            skill = %skill.name,
            tool = %name,
            "rhai-backed skill tool needs an on-disk skill directory to resolve its \
             script path (built-in skills have none) — skipping",
        );
        return;
    };
    let path = dir.join(script);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                skill = %skill.name,
                tool = %name,
                path = %path.display(),
                error = %e,
                "could not read rhai-backed skill tool's script — skipping",
            );
            return;
        }
    };
    let mut preset = Map::new();
    preset.insert("script".to_string(), Value::String(content));
    let desc = if description.is_empty() {
        format!(
            "Run the `{name}` script bundled with the `{}` skill.",
            skill.name
        )
    } else {
        description.clone()
    };
    tools.register(AliasTool::new(
        namespaced(&skill.name, name),
        desc,
        // No params exposed: the script is fixed, sandboxed exactly like a
        // direct `rhai` call (ADR-0046) but with no caller-controlled code.
        serde_json::json!({ "type": "object", "properties": {} }),
        RHAI_TOOL.to_string(),
        preset,
        // `rhai` dispatches by name (`tool_runner`'s interception ladder),
        // never a `ToolRegistry` entry — nothing to delegate to directly.
        None,
    ));
}

fn register_alias(tools: &mut ToolRegistry, skill: &SkillMeta, def: &SkillToolDef) {
    let SkillToolDef::Alias {
        name,
        target,
        args,
        description,
    } = def
    else {
        return;
    };
    let known = tools.contains(target) || tool_names::known_tool_names().contains(&target.as_str());
    if !known {
        tracing::warn!(
            skill = %skill.name,
            tool = %name,
            target,
            "alias targets a tool that isn't registered (or a known runtime-owned \
             name) at skill-registration time — skipping",
        );
        return;
    }
    let delegate = tools.get(target);
    let base_schema = tools
        .spec_for(target)
        .map(|s| s.schema)
        .unwrap_or_else(|| serde_json::json!({ "type": "object", "properties": {} }));
    let schema = schema_minus_preset(&base_schema, args);
    let desc = if description.is_empty() {
        format!("Alias for `{target}`.")
    } else {
        description.clone()
    };
    tools.register(AliasTool::new(
        namespaced(&skill.name, name),
        desc,
        schema,
        target.clone(),
        args.clone(),
        delegate,
    ));
}

/// The target's advertised schema with every preset-covered key removed from
/// `properties`/`required` — the model isn't asked to (and can't) supply an
/// argument the alias already fixes.
fn schema_minus_preset(schema: &Value, preset: &Map<String, Value>) -> Value {
    let mut schema = schema.clone();
    if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        for k in preset.keys() {
            props.remove(k);
        }
    }
    if let Some(req) = schema.get_mut("required").and_then(Value::as_array_mut) {
        req.retain(|v| v.as_str().is_none_or(|s| !preset.contains_key(s)));
    }
    schema
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use async_trait::async_trait;

    use super::*;
    use crate::tools::Tool;

    /// A minimal stand-in for the real `read` host tool (unavailable to a
    /// unit test with no filesystem root) — just enough surface (a `path`
    /// param) for the alias/schema tests below to exercise something real.
    struct DummyRead;
    #[async_trait]
    impl Tool for DummyRead {
        fn name(&self) -> Cow<'static, str> {
            Cow::Borrowed("read")
        }
        fn schema(&self) -> Value {
            serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            })
        }
        async fn run(&self, input: &str) -> anyhow::Result<String> {
            Ok(input.to_string())
        }
    }

    fn skill_with_tools(
        name: &str,
        root_dir: Option<std::path::PathBuf>,
        tools: Vec<SkillToolDef>,
    ) -> SkillMeta {
        SkillMeta {
            name: name.to_string(),
            description: "d".to_string(),
            user_only: false,
            allowed_tools: None,
            root_dir,
            body: String::new(),
            tools,
        }
    }

    fn registry_with(skills: Vec<SkillMeta>) -> SkillRegistry {
        let mut reg = SkillRegistry::default();
        for s in skills {
            reg.insert(s);
        }
        reg
    }

    #[test]
    fn parses_all_three_kinds_from_yaml() {
        let yaml = "
- kind: endpoint
  name: gh_search
  url: \"https://api.github.com/search?q={{query}}\"
  params:
    query: { type: string }
- kind: rhai
  name: run_lint
  script: scripts/lint.rhai
- kind: alias
  name: quick_read
  target: read
  args:
    path: README.md
";
        let defs: Vec<SkillToolDef> = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(defs.len(), 3);
        assert_eq!(defs[0].name(), "gh_search");
        assert_eq!(defs[1].name(), "run_lint");
        assert_eq!(defs[2].name(), "quick_read");
    }

    #[test]
    fn endpoint_kind_rejects_unknown_fields() {
        let err = serde_yaml::from_str::<SkillToolDef>(
            "kind: endpoint\nname: x\nurl: https://x\ntypo: 1",
        )
        .unwrap_err();
        assert!(format!("{err}").contains("typo"), "{err}");
    }

    fn http() -> HttpClient {
        HttpClient::new().unwrap()
    }

    #[test]
    fn register_skill_tools_namespaces_every_kind() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lint.rhai"), "// noop").unwrap();
        let skill = skill_with_tools(
            "research",
            Some(dir.path().to_path_buf()),
            vec![
                SkillToolDef::Endpoint {
                    name: "gh_search".to_string(),
                    method: EndpointMethod::Get,
                    url: "https://api.github.com/search".to_string(),
                    description: String::new(),
                    params: HashMap::new(),
                    headers: HashMap::new(),
                    body: None,
                },
                SkillToolDef::Rhai {
                    name: "run_lint".to_string(),
                    script: "lint.rhai".to_string(),
                    description: String::new(),
                },
                SkillToolDef::Alias {
                    name: "quick_read".to_string(),
                    target: "read".to_string(),
                    args: Map::new(),
                    description: String::new(),
                },
            ],
        );
        let skills = registry_with(vec![skill]);
        let mut tools = ToolRegistry::new();
        tools.register(DummyRead);
        register_skill_tools(&mut tools, &skills, &http());

        assert!(tools.contains("skill__research__gh_search"));
        assert!(tools.contains("skill__research__run_lint"));
        assert!(tools.contains("skill__research__quick_read"));
    }

    #[test]
    fn rhai_tool_rewrites_to_rhai_with_the_script_content_preset() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lint.rhai"), "let x = 1;").unwrap();
        let skill = skill_with_tools(
            "research",
            Some(dir.path().to_path_buf()),
            vec![SkillToolDef::Rhai {
                name: "run_lint".to_string(),
                script: "lint.rhai".to_string(),
                description: String::new(),
            }],
        );
        let skills = registry_with(vec![skill]);
        let mut tools = ToolRegistry::new();
        register_skill_tools(&mut tools, &skills, &http());

        let tool = tools.get("skill__research__run_lint").unwrap();
        let (target, merged) = tool.alias_rewrite("{}").unwrap();
        assert_eq!(target, RHAI_TOOL);
        let v: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(v["script"], "let x = 1;");
    }

    #[test]
    fn rhai_tool_without_a_root_dir_is_skipped_not_fatal() {
        let skill = skill_with_tools(
            "built_in",
            None,
            vec![SkillToolDef::Rhai {
                name: "run_lint".to_string(),
                script: "lint.rhai".to_string(),
                description: String::new(),
            }],
        );
        let skills = registry_with(vec![skill]);
        let mut tools = ToolRegistry::new();
        register_skill_tools(&mut tools, &skills, &http());
        assert!(!tools.contains("skill__built_in__run_lint"));
    }

    #[test]
    fn alias_with_unknown_target_is_skipped_not_fatal() {
        let skill = skill_with_tools(
            "x",
            None,
            vec![SkillToolDef::Alias {
                name: "bogus".to_string(),
                target: "no_such_tool".to_string(),
                args: Map::new(),
                description: String::new(),
            }],
        );
        let skills = registry_with(vec![skill]);
        let mut tools = ToolRegistry::new();
        register_skill_tools(&mut tools, &skills, &http());
        assert!(!tools.contains("skill__x__bogus"));
    }

    #[test]
    fn alias_can_target_a_sibling_skill_tool_registered_in_pass_one() {
        let dir = tempfile::tempdir().unwrap();
        let skill = skill_with_tools(
            "x",
            Some(dir.path().to_path_buf()),
            vec![
                SkillToolDef::Endpoint {
                    name: "search".to_string(),
                    method: EndpointMethod::Get,
                    url: "https://x/search".to_string(),
                    description: String::new(),
                    params: HashMap::new(),
                    headers: HashMap::new(),
                    body: None,
                },
                SkillToolDef::Alias {
                    name: "search_alias".to_string(),
                    target: "skill__x__search".to_string(),
                    args: Map::new(),
                    description: String::new(),
                },
            ],
        );
        let skills = registry_with(vec![skill]);
        let mut tools = ToolRegistry::new();
        register_skill_tools(&mut tools, &skills, &http());
        assert!(tools.contains("skill__x__search_alias"));
    }

    #[test]
    fn alias_schema_omits_preset_keys() {
        let mut tools = ToolRegistry::new();
        tools.register(DummyRead);
        let skill = skill_with_tools(
            "x",
            None,
            vec![SkillToolDef::Alias {
                name: "quick_read".to_string(),
                target: "read".to_string(),
                args: Map::from_iter([(
                    "path".to_string(),
                    Value::String("README.md".to_string()),
                )]),
                description: String::new(),
            }],
        );
        let skills = registry_with(vec![skill]);
        register_skill_tools(&mut tools, &skills, &http());
        let spec = tools.spec_for("skill__x__quick_read").unwrap();
        assert!(spec.schema["properties"].get("path").is_none());
    }
}
