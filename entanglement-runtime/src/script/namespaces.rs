//! The per-run generated namespaces (ADR-0227): `mcp::<server>::<tool>(#{…})`
//! for every MCP tool the session can see, and `skill::<skill>::<fn>(…)` for
//! every skill's compiled `lib.rhai`. Generated from the registry at launch;
//! each MCP call is graded under its real registered `mcp__<server>__<tool>`
//! name at the seam, and an unconnected server's last-seen tools connect
//! lazily on first call. Names come from the shared [`plan_mcp`]/
//! [`plan_skills`] pass the listings use too, so a colliding (skipped) entry
//! is never listed under a name that reaches another tool.

use std::collections::BTreeMap;

use rhai::{Dynamic, Engine, EvalAltResult, Module};
use tokio::sync::mpsc::UnboundedSender;

use crate::rhai_ident::{plan_mcp, plan_skills, RhaiName};
use crate::skills::SkillRegistry;
use crate::tool_runner::Dispatcher;

use super::bindings::runtime_err;
use super::BindingCall;

/// One MCP tool a script may call.
#[derive(Clone)]
pub(super) struct McpEntry {
    /// The server's configured name.
    server: String,
    /// The tool's registered short name (what `mcp_tools` lists).
    tool: String,
    /// `mcp__<server>__<tool>` — what the seam grades and runs.
    registered: String,
}

/// Every server the session can see plus the MCP tools it can call: a
/// connected server's registered tools, else (unscoped only) its last-seen
/// list — a scoped session never reads the shared cache (ADR-0188), and lists
/// only its scope's servers.
pub(super) struct McpView {
    servers: Vec<String>,
    entries: Vec<McpEntry>,
}

pub(super) fn mcp_entries(d: &Dispatcher) -> McpView {
    match d.mcp_scopes.as_ref().and_then(|s| s.rows(&d.session)) {
        Some(rows) => entries_from(rows, |_, _| None),
        None => entries_from(
            crate::mcp::roster(&d.mcp_avail, &d.mcp_active),
            |server, target| d.mcp_avail.tool_cache().lookup(server, target),
        ),
    }
}

/// [`mcp_entries`] over explicit rows and a last-seen lookup (`|_, _| None`
/// for a scoped session).
fn entries_from(
    rows: Vec<crate::mcp::ServerRow>,
    last_seen: impl Fn(&str, &str) -> Option<Vec<String>>,
) -> McpView {
    let servers = rows.iter().map(|r| r.name.clone()).collect();
    let mut out = Vec::new();
    for row in rows {
        let prefix = entanglement_core::mcp_tool_name(&row.name, "");
        let shorts: Vec<String> = if row.connected {
            row.tools
                .iter()
                .map(|t| t.strip_prefix(&prefix).unwrap_or(t).to_string())
                .collect()
        } else {
            last_seen(&row.name, &row.target).unwrap_or_default()
        };
        for tool in shorts {
            out.push(McpEntry {
                registered: entanglement_core::mcp_tool_name(&row.name, &tool),
                server: row.name.clone(),
                tool,
            });
        }
    }
    McpView {
        servers,
        entries: out,
    }
}

/// Register the `mcp::…` and `skill::…` modules on `engine`.
pub(super) fn register(
    engine: &mut Engine,
    tx: UnboundedSender<BindingCall>,
    mcp: &McpView,
    skills: &SkillRegistry,
) {
    let pairs: Vec<(String, String)> = mcp
        .entries
        .iter()
        .map(|e| (e.server.clone(), e.tool.clone()))
        .collect();
    let plan = plan_mcp(&mcp.servers, &pairs);
    // server segment → its module
    let mut modules: BTreeMap<String, Module> = BTreeMap::new();
    for entry in &mcp.entries {
        let path = match &plan[&(entry.server.clone(), entry.tool.clone())] {
            RhaiName::Path(path) => path.clone(),
            RhaiName::Collides(other) => {
                tracing::warn!(tool = %entry.registered, "no rhai name: collides with {other}");
                continue;
            }
        };
        let Some((module_path, tool_id)) = path.rsplit_once("::") else {
            continue;
        };
        let module = modules.entry(module_path.to_string()).or_default();
        let (t, name, at) = (tx.clone(), entry.registered.clone(), path.clone());
        module.set_native_fn(tool_id, move |args: rhai::Map| {
            call_mcp(&t, &name, &at, args)
        });
        let (t, name, at) = (tx.clone(), entry.registered.clone(), path.clone());
        module.set_native_fn(tool_id, move || call_mcp(&t, &name, &at, rhai::Map::new()));
    }
    for (module_path, module) in modules {
        engine.register_static_module(module_path, module.into());
    }
    let with_lib: Vec<String> = skills
        .iter()
        .filter(|s| s.lib.is_some())
        .map(|s| s.name.clone())
        .collect();
    let plan = plan_skills(&with_lib);
    for skill in skills.iter() {
        let Some(lib) = &skill.lib else { continue };
        match &plan[&skill.name] {
            RhaiName::Path(ns) => {
                engine.register_static_module(ns.clone(), lib.module.clone());
            }
            RhaiName::Collides(other) => {
                tracing::warn!(skill = %skill.name, "no rhai namespace: collides with {other}")
            }
        };
    }
}

/// One MCP call: the argument map as the tool's JSON input; the text result.
fn call_mcp(
    tx: &UnboundedSender<BindingCall>,
    tool: &str,
    path: &str,
    args: rhai::Map,
) -> Result<String, Box<EvalAltResult>> {
    let input: serde_json::Value =
        rhai::serde::from_dynamic(&Dynamic::from_map(args)).map_err(|e| {
            runtime_err(&format!(
                "{tool}: arguments are not JSON-representable: {e}"
            ))
        })?;
    super::send_binding(tx, tool, Some(path), input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{lib_rhai, SkillMeta};

    fn entry(server: &str, tool: &str) -> McpEntry {
        McpEntry {
            server: server.into(),
            tool: tool.into(),
            registered: entanglement_core::mcp_tool_name(server, tool),
        }
    }

    fn skill_with_lib(name: &str, source: &str) -> SkillMeta {
        SkillMeta {
            name: name.into(),
            description: "d".into(),
            allowed_tools: None,
            root_dir: None,
            body: String::new(),
            lib: Some(lib_rhai::compile(name, source).unwrap()),
        }
    }

    /// Run `script` against the generated namespaces, answering each bridge
    /// call with its tool name + input (+ ` @<rhai path>` for a namespace
    /// call) so the script can return them.
    fn eval(mcp: &[McpEntry], skills: &SkillRegistry, script: &str) -> Result<String, String> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<BindingCall>();
        let mut engine = Engine::new_raw();
        let mut servers: Vec<String> = mcp.iter().map(|e| e.server.clone()).collect();
        servers.dedup();
        let view = McpView {
            servers,
            entries: mcp.to_vec(),
        };
        register(&mut engine, tx, &view, skills);
        let responder = std::thread::spawn(move || {
            while let Some(call) = rx.blocking_recv() {
                let at = call.rhai_path.map(|p| format!(" @{p}")).unwrap_or_default();
                let _ = call
                    .reply
                    .send(Ok(format!("{} {}{at}", call.tool, call.input)));
            }
        });
        let out = engine
            .eval::<Dynamic>(script)
            .map(|d| d.to_string())
            .map_err(|e| e.to_string());
        drop(engine);
        responder.join().unwrap();
        out
    }

    fn row(name: &str, connected: bool, tools: &[&str]) -> crate::mcp::ServerRow {
        crate::mcp::ServerRow {
            name: name.into(),
            description: None,
            transport: "http".into(),
            target: format!("https://{name}/mcp"),
            connected,
            tools: tools.iter().map(|t| t.to_string()).collect(),
            oauth: false,
            auth_required: false,
        }
    }

    /// A connected server lists its registered tools; an unconnected one
    /// only what the last-seen lookup gives — which a scoped session passes
    /// as "nothing", never the shared cache (ADR-0188).
    #[test]
    fn entries_come_from_connected_tools_and_the_given_last_seen_only() {
        let rows = || vec![row("gh", true, &["mcp__gh__search"]), row("kb", false, &[])];
        let names = |v: McpView| {
            v.entries
                .into_iter()
                .map(|e| e.registered)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(entries_from(rows(), |s, _| (s == "kb").then(|| vec!["find".into()]))),
            vec!["mcp__gh__search", "mcp__kb__find"]
        );
        assert_eq!(
            names(entries_from(rows(), |_, _| None)),
            vec!["mcp__gh__search"]
        );
    }

    #[test]
    fn mcp_tools_are_called_under_their_registered_name() {
        let mcp = [entry("my-gh", "search.issues"), entry("my-gh", "call")];
        let out = eval(
            &mcp,
            &SkillRegistry::default(),
            r#"mcp::my_gh::search_issues(#{q: "x"}) + " | " + mcp::my_gh::call_()"#,
        )
        .unwrap();
        assert_eq!(
            out,
            r#"mcp__my-gh__search_issues {"q":"x"} @mcp::my_gh::search_issues | mcp__my-gh__call {} @mcp::my_gh::call_"#
        );
    }

    /// The call carries the path it was made through, which a decline shows
    /// (#844): a server named `a__b` can't be split back out of
    /// `mcp__a__b__x`.
    #[test]
    fn a_namespace_call_carries_its_own_rhai_path() {
        let out = eval(
            &[entry("a__b", "x")],
            &SkillRegistry::default(),
            "mcp::a__b::x()",
        )
        .unwrap();
        assert_eq!(out, "mcp__a__b__x {} @mcp::a__b::x");
    }

    #[test]
    fn a_collision_after_mapping_keeps_the_first_and_skips_the_later() {
        // `a-b` and `a.b` both map to `a_b`; the second server is skipped.
        let mcp = [
            entry("a-b", "t"),
            entry("a.b", "u"),
            entry("s", "x-y"),
            entry("s", "x.y"),
        ];
        let skills = SkillRegistry::default();
        assert!(eval(&mcp, &skills, "mcp::a_b::t()").is_ok());
        assert!(eval(&mcp, &skills, "mcp::a_b::u()").is_err());
        let out = eval(&mcp, &skills, "mcp::s::x_y()").unwrap();
        assert!(out.starts_with("mcp__s__x-y"), "the first tool won: {out}");
    }

    #[test]
    fn skill_libs_register_as_static_modules_calling_the_bindings() {
        let mut skills = SkillRegistry::default();
        skills.insert(skill_with_lib(
            "git-tools",
            "fn search(q) { helper(q) }\nprivate fn helper(q) { mcp::gh::find(#{q: q}) }",
        ));
        let out = eval(
            &[entry("gh", "find")],
            &skills,
            r#"skill::git_tools::search("bug")"#,
        )
        .unwrap();
        assert_eq!(out, r#"mcp__gh__find {"q":"bug"} @mcp::gh::find"#);
        assert!(
            eval(&[], &skills, "skill::git_tools::helper(1)").is_err(),
            "a private fn stays private"
        );
    }

    /// The listing and the registration come from one plan: every listed
    /// path reaches exactly the tool it is listed for, and a tool listed as
    /// colliding is reachable under no name of its own.
    #[test]
    fn listing_and_registration_agree_under_collision() {
        let mcp = [
            entry("s", "delete.item"),
            entry("s", "delete-item"),
            entry("s", "t_2fa"),
            entry("s", "2fa"),
        ];
        let pairs: Vec<(String, String)> = mcp
            .iter()
            .map(|e| (e.server.clone(), e.tool.clone()))
            .collect();
        let plan = plan_mcp(&["s".to_string()], &pairs);
        let skills = SkillRegistry::default();
        let mut listed = 0;
        for e in &mcp {
            match &plan[&(e.server.clone(), e.tool.clone())] {
                RhaiName::Path(path) => {
                    listed += 1;
                    let out = eval(&mcp, &skills, &format!("{path}()")).unwrap();
                    assert!(
                        out.starts_with(&format!("{} ", e.registered)),
                        "{path}: {out}"
                    );
                }
                RhaiName::Collides(other) => {
                    assert_ne!(other, &format!("`{}`", e.registered));
                }
            }
        }
        assert_eq!(listed, 2, "one winner per colliding pair");
    }

    #[test]
    fn colliding_skill_namespaces_keep_the_first() {
        let mut skills = SkillRegistry::default();
        skills.insert(skill_with_lib("a-b", "fn who() { \"dash\" }"));
        skills.insert(skill_with_lib("a.b", "fn who() { \"dot\" }"));
        // Name-sorted iteration: `a-b` registers first.
        assert_eq!(eval(&[], &skills, "skill::a_b::who()").unwrap(), "dash");
    }
}
