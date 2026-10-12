//! The internal tool index the TUI `/tools` view browses: every host tool,
//! runtime-owned pseudo-tool, definition-driven endpoint/skill tool, skill,
//! and MCP server (its tools when connected), each with a kind label. Built
//! fresh per call from live state — never cached.

use crate::mcp::{ActiveServers, AvailableMcp};
use crate::skills::SkillRegistry;
use crate::tools::ToolRegistry;

use super::runtime_owned_specs;

/// One index row. `pub`, not `pub(crate)`: the TUI lives in the **binary**
/// crate (`main.rs`'s `mod tui`), a separate compilation unit from this
/// library crate.
pub struct IndexRow {
    pub name: String,
    pub description: String,
    pub source: String,
    /// `"tool"` | `"mcp"` | `"skill"` | `"endpoint"`.
    pub kind: &'static str,
}

/// First line of a description — some host-tool descriptions (`bash`,
/// `rhai`) run to several paragraphs.
fn one_line(description: &str) -> String {
    description.lines().next().unwrap_or_default().to_string()
}

fn row(name: String, description: &str, source: String, kind: &'static str) -> IndexRow {
    IndexRow {
        name,
        description: one_line(description),
        source,
        kind,
    }
}

/// The full index, name-sorted within each source.
pub fn index_rows(
    registry: &ToolRegistry,
    avail: &AvailableMcp,
    active: &ActiveServers,
    skills: &SkillRegistry,
) -> Vec<IndexRow> {
    let mut rows = Vec::new();
    for spec in registry.specs().into_iter().chain(runtime_owned_specs()) {
        let (source, kind) = if spec.name.starts_with("mcp__") {
            continue; // listed per server below
        } else if spec.name.starts_with("endpoint__") {
            ("endpoint".to_string(), "endpoint")
        } else if let Some(rest) = spec.name.strip_prefix("skill__") {
            let skill = rest.split("__").next().unwrap_or_default();
            (format!("skill:{skill}"), "skill")
        } else {
            ("built-in".to_string(), "tool")
        };
        rows.push(row(spec.name, &spec.description, source, kind));
    }
    for server in crate::mcp::roster(avail, active) {
        let source = format!("mcp:{}", server.name);
        if server.connected {
            for tool in &server.tools {
                let description = registry
                    .spec_for(tool)
                    .map(|s| s.description)
                    .unwrap_or_default();
                rows.push(row(tool.clone(), &description, source.clone(), "mcp"));
            }
        } else {
            let description = server
                .description
                .unwrap_or_else(|| "MCP server (not connected yet)".to_string());
            rows.push(row(server.name, &description, source, "mcp"));
        }
    }
    for d in skills.disclosures() {
        rows.push(row(d.name, &d.description, "skill".to_string(), "skill"));
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows.dedup_by(|a, b| a.name == b.name);
    rows
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;
    use crate::tools::Tool;

    struct Fake(&'static str);
    #[async_trait]
    impl Tool for Fake {
        fn name(&self) -> Cow<'static, str> {
            Cow::Borrowed(self.0)
        }
        fn description(&self) -> &str {
            "first line\nsecond line"
        }
        async fn run(&self, _input: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    #[test]
    fn rows_are_classified_by_source_and_unconnected_servers_get_one_row() {
        let mut registry = ToolRegistry::new();
        for name in ["glob", "endpoint__weather", "skill__git__log"] {
            registry.register(Fake(name));
        }
        let avail = AvailableMcp::default();
        avail.insert_user("kb", serde_yaml::from_str("url: https://kb/mcp").unwrap());
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let rows = index_rows(&registry, &avail, &active, &SkillRegistry::default());
        let kind = |name: &str| rows.iter().find(|r| r.name == name).map(|r| r.kind);
        assert_eq!(kind("glob"), Some("tool"));
        assert_eq!(kind("list_mcp"), Some("tool"));
        assert_eq!(kind("endpoint__weather"), Some("endpoint"));
        assert_eq!(kind("skill__git__log"), Some("skill"));
        assert_eq!(kind("kb"), Some("mcp"));
        let glob = rows.iter().find(|r| r.name == "glob").unwrap();
        assert_eq!(glob.description, "first line");
    }
}
