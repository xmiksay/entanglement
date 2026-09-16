//! `explore`'s section taxonomy (#560 P9, ADR-0199 part 1/3): the `Kind`
//! enum every row is classified into, `explore`'s own sectioned-text
//! rendering, and [`IndexRow`]/[`index_rows`] — the shape the TUI `/tools`
//! view reuses instead of re-walking the registry/MCP/skill state itself.
//! Split out of `explore.rs` once it crossed the 400-line cap; `explore.rs`
//! keeps the index-*building* half (`build_rows` and its row-source
//! functions), this module keeps the *classifying and presenting* half.

use entanglement_core::SessionId;

use crate::mcp::{ActiveServers, AvailableMcp};
use crate::skills::SkillRegistry;
use crate::tools::ToolRegistry;

use super::explore::{build_rows, Row};

/// The four `explore` sections: derived from a row's `source` string rather
/// than carried as its own field, so [`super::explore::build_index`]'s JSON
/// row shape — `name`/`description`/`source` — stays byte-identical to what
/// [`super::tool_search`] already depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Tool,
    Mcp,
    Skill,
    Endpoint,
}

impl Kind {
    pub(super) fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "tool" => Some(Kind::Tool),
            "mcp" => Some(Kind::Mcp),
            "skill" => Some(Kind::Skill),
            "endpoint" => Some(Kind::Endpoint),
            _ => None,
        }
    }

    /// The section header a `kind`'s rows render under — the MCP section
    /// carries the clarifying line the task calls for, the rest stay terse.
    fn header(self) -> &'static str {
        match self {
            Kind::Tool => "TOOLS",
            Kind::Mcp => {
                "MCP servers — a server bundles tools; enable the \
                server (mcp_enable {\"server\": \"<name>\"}), then call its \
                tools by their mcp__<server>__<tool> names:"
            }
            Kind::Skill => "SKILLS",
            Kind::Endpoint => "ENDPOINTS",
        }
    }

    /// Every section in fixed display order.
    const ALL: [Kind; 4] = [Kind::Tool, Kind::Mcp, Kind::Skill, Kind::Endpoint];

    /// The wire-facing label (matches [`explore_spec`][super::explore_spec]'s
    /// `kind` enum values byte-for-byte) — also what the TUI `/tools` view
    /// (#560 P9, ADR-0199 part 3) groups its own rows by, via [`IndexRow`].
    fn label(self) -> &'static str {
        match self {
            Kind::Tool => "tool",
            Kind::Mcp => "mcp",
            Kind::Skill => "skill",
            Kind::Endpoint => "endpoint",
        }
    }
}

/// A row's kind, derived from its `source` label — the single place that
/// decodes the `source` convention every row-building function in
/// `explore.rs` already follows (`"built-in"`, `"mcp:..."`/
/// `"mcp (allowed)"`, `"skill"`/`"skill:..."`, `"endpoint"`).
pub(super) fn kind_of(source: &str) -> Kind {
    if source.starts_with("mcp") {
        Kind::Mcp
    } else if source == "endpoint" {
        Kind::Endpoint
    } else if source == "skill" || source.starts_with("skill:") {
        Kind::Skill
    } else {
        Kind::Tool
    }
}

/// `explore`'s own terse, sectioned rendering: one header per non-empty
/// [`Kind`] (in fixed order), rows underneath as `name — description`. Plain
/// text rather than the JSON array `build_index` still returns for
/// `tool_search`'s internal reuse — a human/model-readable index is the
/// point of `explore`, and headers plus the MCP clarifying line only make
/// sense as prose.
pub(super) fn render_sections(rows: Vec<Row>) -> String {
    let mut out = String::new();
    for kind in Kind::ALL {
        let section: Vec<&Row> = rows.iter().filter(|r| kind_of(&r.source) == kind).collect();
        if section.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(kind.header());
        out.push('\n');
        for r in section {
            out.push_str(&format!("  {} — {}\n", r.name, r.description));
        }
    }
    if out.is_empty() {
        out.push_str("(no matches)");
    }
    out
}

/// A live-index row plus its derived [`Kind`] label — the shape the TUI
/// `/tools` view (#560 P9, ADR-0199 part 3) consumes via [`index_rows`],
/// reusing `explore.rs`'s own index-building rather than duplicating it.
/// `pub`, not `pub(crate)`: the TUI lives in the **binary** crate
/// (`entanglement-runtime/src/main.rs`'s `mod tui`), a separate compilation
/// unit from this library crate, so a `pub(crate)` item here is invisible to
/// it even though both share one source tree — the same reason
/// `SharedRegistry`/`ToolRegistry` are plain `pub` at the crate root.
pub struct IndexRow {
    pub name: String,
    pub description: String,
    pub source: String,
    pub kind: &'static str,
}

/// The full, unfiltered live index with each row's [`Kind`] resolved —
/// `build_rows` plus the same `kind_of` derivation [`render_sections`]
/// groups by, just handed back as a plain struct instead of rendered text or
/// re-parsed JSON.
pub fn index_rows(
    registry: &ToolRegistry,
    avail: &AvailableMcp,
    active: &ActiveServers,
    skills: &SkillRegistry,
    session: &SessionId,
) -> Vec<IndexRow> {
    build_rows(registry, avail, active, skills, session, None, None)
        .into_iter()
        .map(|r| IndexRow {
            kind: kind_of(&r.source).label(),
            name: r.name,
            description: r.description,
            source: r.source,
        })
        .collect()
}
