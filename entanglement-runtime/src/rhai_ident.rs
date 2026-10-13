//! The identifier mapping for generated rhai namespaces (ADR-0227): an MCP
//! server or tool name becomes a path segment of `mcp::<server>::<tool>`.
//! Names are free-form — a tool may start with a digit, contain `.`/`-`, or
//! be a Rhai keyword (`mcp::srv::call(...)` does not even parse) — so every
//! segment is mapped: non-identifier characters → `_`; a result Rhai would
//! not accept as an identifier (no letter before the first digit, or no
//! letter at all) gets a `t_` prefix; a keyword or reserved word gets a `_`
//! suffix. Validity is Rhai's own (`is_valid_identifier`,
//! `is_valid_function_name`), never a hand-kept list that could drift.

/// `raw` as a valid Rhai identifier segment.
pub fn rhai_ident(raw: &str) -> String {
    let mapped: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut out = if !mapped.chars().any(|c| c.is_ascii_alphanumeric()) {
        "t_".to_string()
    } else if rhai::is_valid_identifier(&mapped) {
        mapped
    } else {
        format!("t_{mapped}")
    };
    if !rhai::is_valid_function_name(&out) {
        out.push('_');
    }
    out
}

/// The rhai call path for MCP tool `tool` (its registered short name) on
/// `server`.
pub fn mcp_path(server: &str, tool: &str) -> String {
    format!("mcp::{}::{}", rhai_ident(server), rhai_ident(tool))
}

/// A tool's or skill's rhai name, or why it has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RhaiName {
    /// The call path (`mcp::gh::search`, `skill::git`).
    Path(String),
    /// Mapped onto a name another entry claimed first — it names that one.
    Collides(String),
}

impl RhaiName {
    /// The listing line for an entry with no rhai name — the model must not
    /// be pointed at a path that reaches a different tool.
    pub fn listing(&self, fallback: &str) -> String {
        match self {
            RhaiName::Path(path) => path.clone(),
            RhaiName::Collides(other) => {
                format!("no rhai name (collides with {other}) — use {fallback}")
            }
        }
    }
}

/// The one mapping + collision pass for `mcp::<server>::<tool>` names, shared
/// by namespace registration and every listing so they always agree.
/// `servers` is every server the session can see (a server segment is
/// claimed by the first, in name order, mapping onto it); `tools` are
/// `(server, registered short name)` pairs, claimed per server segment in
/// name order. Keyed by `(server, tool)`.
pub fn plan_mcp(
    servers: &[String],
    tools: &[(String, String)],
) -> std::collections::HashMap<(String, String), RhaiName> {
    use std::collections::{BTreeSet, HashMap};
    let mut owner: HashMap<String, &str> = HashMap::new();
    for server in servers.iter().collect::<BTreeSet<_>>() {
        owner.entry(rhai_ident(server)).or_insert(server);
    }
    let mut taken: HashMap<(String, String), String> = HashMap::new();
    let mut out = HashMap::new();
    for (server, tool) in tools.iter().collect::<BTreeSet<_>>() {
        let server_id = rhai_ident(server);
        let name = match owner.get(&server_id) {
            Some(first) if *first != server.as_str() => {
                RhaiName::Collides(format!("MCP server `{first}`"))
            }
            _ => {
                let key = (server_id.clone(), rhai_ident(tool));
                match taken.get(&key) {
                    Some(first) => RhaiName::Collides(format!("`{first}`")),
                    None => {
                        let registered = entanglement_core::mcp_tool_name(server, tool);
                        taken.insert(key.clone(), registered);
                        RhaiName::Path(format!("mcp::{}::{}", key.0, key.1))
                    }
                }
            }
        };
        out.insert((server.clone(), tool.clone()), name);
    }
    out
}

/// [`plan_mcp`] for skill libraries: `skill::<skill>` per skill name, the
/// first in name order winning a shared segment.
pub fn plan_skills(skills: &[String]) -> std::collections::HashMap<String, RhaiName> {
    use std::collections::{BTreeSet, HashMap};
    let mut taken: HashMap<String, &str> = HashMap::new();
    let mut out = HashMap::new();
    for skill in skills.iter().collect::<BTreeSet<_>>() {
        let ns = format!("skill::{}", rhai_ident(skill));
        let name = match taken.get(&ns) {
            Some(first) => RhaiName::Collides(format!("skill `{first}`")),
            None => {
                taken.insert(ns.clone(), skill);
                RhaiName::Path(ns)
            }
        };
        out.insert(skill.clone(), name);
    }
    out
}

/// `value` as a Rhai literal — objects as `#{key: value}` maps — for the
/// call examples `mcp_tools` and a script's arg-validate decline render.
pub fn rhai_literal(value: &serde_json::Value) -> String {
    use serde_json::Value;
    match value {
        Value::Null => "()".to_string(),
        Value::Bool(_) => value.to_string(),
        // Rhai's `INT` is i64: a larger integer only fits as a float.
        // `as_f64` is always `Some` for a u64; the fallback never fires.
        Value::Number(n) if n.is_u64() && n.as_i64().is_none() => {
            format!("{:?}", n.as_f64().unwrap_or(f64::MAX))
        }
        Value::Number(_) => value.to_string(),
        Value::String(s) => string_literal(s),
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(rhai_literal).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(map) => {
            let fields: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    // A keyword (`let`, `fn`, `this`, …) is a valid
                    // identifier yet not a valid bare property name.
                    let key = if rhai::is_valid_function_name(k) {
                        k.clone()
                    } else {
                        string_literal(k)
                    };
                    format!("{key}: {}", rhai_literal(v))
                })
                .collect();
            format!("#{{{}}}", fields.join(", "))
        }
    }
}

/// A double-quoted Rhai string. Not JSON's escaping: Rhai has no `\b`/`\f`,
/// so every other control character goes out as `\uXXXX`.
fn string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plan_claims_in_name_order_and_names_the_winner() {
        let servers = ["s".to_string(), "a-b".to_string(), "a.b".to_string()];
        let tools: Vec<(String, String)> = [
            ("s", "delete.item"),
            ("s", "delete-item"),
            ("s", "t_2fa"),
            ("s", "2fa"),
            ("a.b", "x"),
            ("a-b", "y"),
        ]
        .iter()
        .map(|(s, t)| (s.to_string(), t.to_string()))
        .collect();
        let plan = plan_mcp(&servers, &tools);
        let get = |s: &str, t: &str| plan[&(s.to_string(), t.to_string())].clone();
        assert_eq!(
            get("s", "delete-item"),
            RhaiName::Path("mcp::s::delete_item".into())
        );
        assert_eq!(
            get("s", "delete.item"),
            RhaiName::Collides("`mcp__s__delete-item`".into())
        );
        assert_eq!(get("s", "2fa"), RhaiName::Path("mcp::s::t_2fa".into()));
        assert_eq!(
            get("s", "t_2fa"),
            RhaiName::Collides("`mcp__s__2fa`".into())
        );
        assert_eq!(get("a-b", "y"), RhaiName::Path("mcp::a_b::y".into()));
        assert_eq!(
            get("a.b", "x"),
            RhaiName::Collides("MCP server `a-b`".into())
        );
        let skills = plan_skills(&["x.y".to_string(), "x-y".to_string()]);
        assert_eq!(skills["x-y"], RhaiName::Path("skill::x_y".into()));
        assert!(skills["x.y"]
            .listing("load_skill")
            .contains("collides with skill `x-y`"));
    }

    #[test]
    fn json_renders_as_a_rhai_literal() {
        let v =
            serde_json::json!({"q": "x", "max-n": 3, "tags": ["a", null], "deep": {"ok": true}});
        assert_eq!(
            rhai_literal(&v),
            r#"#{deep: #{ok: true}, "max-n": 3, q: "x", tags: ["a", ()]}"#
        );
    }

    /// Every rendering must parse, and evaluate back to the same value.
    #[test]
    fn every_literal_compiles_and_round_trips() {
        let corpus = serde_json::json!({
            "let": 1, "fn": 2, "import": 3, "if": 4, "for": 5, "in": 6, "this": 7,
            "print": 8, "plain": 9, "max-n": 10, "": 11, "2x": 12,
            "s": "quote \" back \\ nl \n cr \r tab \t bs \u{8} ff \u{c} nul \u{0} del \u{7f} é ${x}",
            "nested": {"arr": [1, -2, 1.5, 1e10, 1e300, null, true, "x", [], {}]},
            "neg": -9223372036854775808i64,
        });
        let engine = rhai::Engine::new_raw();
        for value in [
            corpus.clone(),
            serde_json::json!(null),
            serde_json::json!("\u{8}"),
        ] {
            let lit = rhai_literal(&value);
            let ast = engine
                .compile_expression(&lit)
                .unwrap_or_else(|e| panic!("{lit} does not parse: {e}"));
            let back: rhai::Dynamic = engine.eval_ast(&ast).unwrap();
            let back: serde_json::Value = rhai::serde::from_dynamic(&back).unwrap();
            assert_eq!(back, value, "{lit}");
        }
        let big = rhai_literal(&serde_json::json!(u64::MAX));
        assert!(engine.compile_expression(&big).is_ok(), "{big}");
    }

    #[test]
    fn maps_non_identifier_chars_leading_digits_and_reserved_words() {
        for (raw, want) in [
            ("search-issues", "search_issues"),
            ("my.docs", "my_docs"),
            ("2fa", "t_2fa"),
            ("_2fa", "t__2fa"),
            ("call", "call_"),
            ("print", "print_"),
            ("type_of", "type_of_"),
            ("let", "let_"),
            ("plain_name", "plain_name"),
            ("", "t_"),
            ("--", "t_"),
            ("__", "t_"),
        ] {
            assert_eq!(rhai_ident(raw), want, "{raw:?}");
        }
        assert_eq!(
            mcp_path("gh-enterprise", "call"),
            "mcp::gh_enterprise::call_"
        );
    }

    /// The mapping is only right if Rhai parses and dispatches the result:
    /// register each mapped name as `mcp::<server>::<tool>` and call it.
    #[test]
    fn every_mapped_name_actually_evaluates() {
        let names = [
            "2fa", "_2fa", "call", "print", "type_of", "let", "fn", "", "--", "__", "a.b-c", "9",
            "Fn", "is", "this", "x",
        ];
        let mut engine = rhai::Engine::new_raw();
        for server in ["srv", "1srv", "let", "--"] {
            let mut module = rhai::Module::new();
            for tool in names {
                let id = rhai_ident(tool);
                let ret = id.clone();
                module.set_native_fn(&id, move || Ok(ret.clone()));
            }
            engine.register_static_module(format!("mcp::{}", rhai_ident(server)), module.into());
        }
        for server in ["srv", "1srv", "let", "--"] {
            for tool in names {
                let path = mcp_path(server, tool);
                let got = engine
                    .eval::<String>(&format!("{path}()"))
                    .unwrap_or_else(|e| panic!("{path}() failed: {e}"));
                assert_eq!(got, rhai_ident(tool), "{path}");
            }
        }
    }
}
