//! The argument an argument-scoped `tool(pattern)` rule (#173) matches —
//! split from `permission` so the per-tool extraction and the "does this
//! tool ever have one" question ([`takes_permission_arg`], #878) share one
//! match and live beside their consistency test.

/// The argument string an argument-scoped permission rule (#173) matches
/// against: the shell command for `bash`, the `command`+`args` line for `call`,
/// the target path for `edit`/`write`/`read`/`apply_patch` (#455) and for
/// `rhai`'s script `path` (ADR-0228 — an inline script has none, so only a
/// bare `rhai` rule matches it), the search
/// pattern (itself a path glob) for `glob`, and the optional file filter for
/// `grep` (#417 — a path, distinct from `grep`'s `pattern` which is a regex,
/// not a path). `None` for any other tool, a `grep` call with no `path`
/// filter, or on malformed input — an argument-scoped rule then never
/// matches, so resolution falls through to the tool's name-only rules.
///
/// `agent` takes the profile the spawn will actually run (ADR-0235): an
/// omitted, empty or non-JSON input resolves to the default profile exactly
/// as the spawn does, so `agent(general)` also covers a call that names no
/// agent. `agent_send` takes its `agent_id` handle, trimmed — the id the
/// model addresses, which survives the child's compactions (ADR-0234).
pub fn permission_arg(tool: &str, input: &str) -> Option<String> {
    let value = || serde_json::from_str::<serde_json::Value>(input).ok();
    let str_field = |v: &serde_json::Value, key: &str| v.get(key)?.as_str().map(String::from);
    match arg_source(tool)? {
        ArgSource::Agent => Some(crate::subagent::target_agent(input)),
        ArgSource::AgentSend => Some(value()?.get("agent_id")?.as_str()?.trim())
            .filter(|id| !id.is_empty())
            .map(String::from),
        ArgSource::Command => str_field(&value()?, "command"),
        ArgSource::Call => {
            let value = value()?;
            let mut line = value.get("command")?.as_str()?.to_string();
            // Space-joining loses the argv split: `{command:"rm",args:["x"]}`
            // and `{command:"rm x"}` grade (and grant-match) identically.
            // Accepted: `call` execs `command` as the program with no shell,
            // so a colliding decomposition can only fail to exec — it can
            // never run a different program than the one graded.
            if let Some(args) = value.get("args").and_then(|a| a.as_array()) {
                for a in args.iter().filter_map(|a| a.as_str()) {
                    line.push(' ');
                    line.push_str(a);
                }
            }
            Some(line)
        }
        ArgSource::Path => str_field(&value()?, "path"),
        ArgSource::Glob => {
            let value = value()?;
            let pattern = value.get("pattern")?.as_str()?;
            // ADR-0150: the optional `path` base dir joins onto the pattern
            // exactly as the tool resolves it, so an argument-scoped rule or
            // grant like `glob(src/**)` sees the same string the walk uses.
            let base = value.get("path").and_then(|p| p.as_str());
            Some(crate::host::glob::joined_pattern(base, pattern))
        }
        // #736: the URL *as it will be fetched* (WHATWG-normalized), so a
        // `web_fetch(https://pastebin.com/*)` rule can't be dodged by
        // `HTTPS://PASTEBIN.COM./x`. Unparseable → no argument (it can't fetch).
        ArgSource::Url => {
            entanglement_core::web_fetch::normalize_url(value()?.get("url")?.as_str()?)
        }
        // ADR-0227: `{METHOD} {url}`, so a rule can allow reads only —
        // `http(GET https://api.github.com/*)`. Both halves normalized as
        // sent; either unparseable → no argument (the call would fail).
        ArgSource::Http => {
            let value = value()?;
            let method =
                entanglement_core::web_fetch::parse_method(value.get("method")?.as_str()?).ok()?;
            let url = entanglement_core::web_fetch::normalize_url(value.get("url")?.as_str()?)?;
            Some(format!("{method} {url}"))
        }
    }
}

/// Where [`permission_arg`] reads a tool's argument from. Split out so
/// [`takes_permission_arg`] answers from the very match the grader uses —
/// a hand-kept list beside it would drift the first time a tool gains one.
enum ArgSource {
    Agent,
    AgentSend,
    Command,
    Call,
    Path,
    Glob,
    Url,
    Http,
}

fn arg_source(tool: &str) -> Option<ArgSource> {
    Some(match tool {
        crate::tool_names::AGENT_TOOL => ArgSource::Agent,
        crate::tool_names::AGENT_SEND_TOOL => ArgSource::AgentSend,
        "bash" => ArgSource::Command,
        "call" => ArgSource::Call,
        // `grep`'s `path` is its optional file filter (#417), not its regex.
        "edit" | "write" | "read" | "apply_patch" | "rhai" | "grep" => ArgSource::Path,
        "glob" => ArgSource::Glob,
        "web_fetch" => ArgSource::Url,
        crate::tool_names::HTTP_TOOL => ArgSource::Http,
        _ => return None,
    })
}

/// Whether `tool` ever has a [`permission_arg`] — when not, an
/// argument-scoped `tool(pattern)` rule can never match a call
/// (`config::mode_warn` warns on one, #878). Decided by tool identity
/// alone: an MCP tool (`mcp__<server>__<tool>`) never takes one.
pub(crate) fn takes_permission_arg(tool: &str) -> bool {
    arg_source(tool).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_names::{is_non_maskable, known_tool_names};

    /// One input carrying every field any [`ArgSource`] reads, so each tool
    /// that takes an argument yields one from it.
    const EVERY_FIELD: &str = r#"{"agent":"plan","agent_id":"s-1","command":"x",
        "path":"src/a.rs","pattern":"*.rs","url":"https://a.example/","method":"GET"}"#;

    /// #878: `takes_permission_arg` agrees with what `permission_arg`
    /// actually extracts, for every known tool and for an MCP tool.
    #[test]
    fn takes_permission_arg_matches_what_permission_arg_extracts() {
        for tool in known_tool_names()
            .iter()
            .copied()
            .chain(["mcp__gh__create"])
        {
            assert_eq!(
                takes_permission_arg(tool),
                permission_arg(tool, EVERY_FIELD).is_some(),
                "{tool}"
            );
        }
        assert!(!takes_permission_arg("mcp__gh__create"));
    }

    /// Every gradable known tool is scopable today; only the non-maskable
    /// ones (never graded under their own name) take no argument. A new
    /// maskable tool that forgets its [`ArgSource`] would grade every scoped
    /// rule as a silent no-op, so this trips until the omission is deliberate.
    #[test]
    fn every_maskable_known_tool_takes_an_argument() {
        for tool in known_tool_names() {
            assert_eq!(
                takes_permission_arg(tool),
                !is_non_maskable(tool),
                "{tool}: maskable but no ArgSource — add one to arg_source, \
                 or exempt it here if it truly takes no argument"
            );
        }
    }
}
