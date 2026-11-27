//! `skutter`'s compile-time tool vocabulary: its own host tools joined to
//! the names the library routes ([`LIBRARY_TOOL_NAMES`]). Read where no
//! live registry exists — the engine-free `inspect modes` view and the
//! startup check for a config rule naming a stale or typo'd tool.

use crate::capability::{runtime_owned, Capability};
use crate::tool_names::{non_maskable_reason as library_reason, LIBRARY_TOOL_NAMES};

/// The host tools `skutter` registers, the script-only read variants
/// included.
pub const HOST_TOOL_NAMES: &[&str] = &[
    "read",
    "glob",
    "grep",
    "edit",
    "write",
    "apply_patch",
    "bash",
    "web_fetch",
    "read_raw",
    "glob_json",
    "grep_json",
];

/// Every literal tool name a rule key can spell (#623): the host tools
/// plus [`LIBRARY_TOOL_NAMES`]. Independent of what is registered this run,
/// so a rule naming a real but inactive tool never warns.
pub fn known_tool_names() -> impl Iterator<Item = &'static str> {
    HOST_TOOL_NAMES.iter().chain(LIBRARY_TOOL_NAMES).copied()
}

/// Whether `entry` — the tool part of a rule key — names something the
/// stale-rule check can vouch for: a known literal tool, a `*`/`?` pattern,
/// or an MCP tool (`mcp__<server>__<tool>`, unknowable until the server
/// connects, #426).
pub fn is_recognized_mask_entry(entry: &str) -> bool {
    entry.contains('*')
        || entry.contains('?')
        || entry.starts_with("mcp__")
        || known_tool_names().any(|name| name == entry)
}

/// Why a rule naming `tool` never takes effect: the library's reason for
/// one of its own non-maskable tools, or — for a script-only read variant,
/// graded as the tool it rides on — the name to use instead.
pub fn non_maskable_reason(tool: &str) -> Option<String> {
    library_reason(tool).or_else(|| {
        Some(match tool {
            "read_raw" => "`read_raw` is graded as the `read` tool — name `read(<pattern>)` \
                           (the tool); a bare `read` rule is the whole read class \
                           (read/grep/glob)"
                .to_string(),
            "glob_json" => "`glob_json` is graded as `glob` — name `glob` instead".to_string(),
            "grep_json" => "`grep_json` is graded as `grep` — name `grep` instead".to_string(),
            _ => return None,
        })
    })
}

/// A known tool's capability with no registry in hand: the library's own
/// routes, then the host tools and the library registry tools — each a
/// compile-time fact of its `Tool` impl.
pub fn static_capability_of(name: &str) -> Option<&'static [Capability]> {
    if let Some(c) = runtime_owned(name) {
        return Some(c);
    }
    match name {
        "read" | "glob" | "grep" | "read_raw" | "glob_json" | "grep_json" => {
            Some(&[Capability::Read])
        }
        "load_skill" => Some(&[Capability::Control]),
        "edit" | "write" | "apply_patch" => Some(&[Capability::Write]),
        // `web_fetch`/`http` are outbound network, graded like `curl` (#736,
        // ADR-0220; ADR-0227).
        "bash" | "web_fetch" | "http" => Some(&[Capability::Exec]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::bypasses_grading;

    #[test]
    fn recognizes_known_literal_tool_names() {
        for name in known_tool_names() {
            assert!(is_recognized_mask_entry(name), "{name} should be known");
        }
    }

    #[test]
    fn recognizes_tools_globs_and_mcp_tools() {
        assert!(is_recognized_mask_entry("read"));
        assert!(is_recognized_mask_entry("write"));
        assert!(is_recognized_mask_entry("*"));
        assert!(is_recognized_mask_entry("mcp__*"));
        assert!(is_recognized_mask_entry("mcp__docs__search"));
        assert!(is_recognized_mask_entry("bash(git *)"));
    }

    #[test]
    fn flags_removed_and_typo_d_tool_names() {
        for gone in [
            "bash_output",
            "agent_poll",
            "agent_spawn",
            "raed",
            // #916: the argv `call` tool.
            "call",
            // ADR-0226 retirements.
            "explore",
            "describe",
            "invoke",
            "mcp_enable",
            "responses_tool_search",
        ] {
            assert!(!is_recognized_mask_entry(gone), "{gone} is retired");
        }
    }

    #[test]
    fn the_read_variants_name_their_base_tool() {
        let glob = non_maskable_reason("glob_json").unwrap();
        assert!(glob.contains("name `glob`"), "{glob}");
        let raw = non_maskable_reason("read_raw").unwrap();
        assert!(
            raw.contains("read(<pattern>)") && raw.contains("read class"),
            "{raw}"
        );
        assert!(non_maskable_reason("mcp").is_some(), "library reasons stay");
        for maskable in ["bash", "read", "glob"] {
            assert!(non_maskable_reason(maskable).is_none(), "{maskable}");
        }
    }

    #[test]
    fn every_known_tool_has_a_static_capability() {
        for tool in known_tool_names().filter(|t| *t != "mcp") {
            assert!(static_capability_of(tool).is_some(), "{tool}");
        }
        assert_eq!(
            static_capability_of("write"),
            Some([Capability::Write].as_slice())
        );
        assert_eq!(static_capability_of("not_a_real_tool"), None);
        assert!(!bypasses_grading("read", static_capability_of("read")));
        assert!(bypasses_grading(
            "load_skill",
            static_capability_of("load_skill")
        ));
    }
}
