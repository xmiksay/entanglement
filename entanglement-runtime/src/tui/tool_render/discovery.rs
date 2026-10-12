//! Discovery tool bodies (ADR-0226). `list_agents`/`list_models`/
//! `list_skills`/`list_mcp` reply with a header line plus `  name — description`
//! rows; `mcp_tools` replies with one block per tool — `- tool: description`,
//! its JSON schema, and a `call as: mcp {...}` line. Both render readably:
//! rows in the tool-name color, a schema as a parameter summary (name, type,
//! required) instead of raw JSON.

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;

use crate::tui::theme::{Slot, Theme};
use crate::tui::wrap;

use super::{parse_input, readable};

/// `mcp_tools {"name": ...}`: the server it reads.
pub(super) fn render_mcp_tools_input(input: &str) -> Vec<Line<'static>> {
    let value = parse_input(input);
    let name = value.get("name").and_then(Value::as_str).unwrap_or("?");
    vec![Line::from(format!("  server: {name}"))]
}

fn header(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {text}"),
        Style::default().add_modifier(Modifier::BOLD),
    ))
}

/// A `list_*` reply: the header bold, each `  name — description` row via
/// [`name_row`], deeper-indented detail lines (`list_mcp`'s tool names)
/// muted beneath their row.
pub(super) fn render_list_output(
    output: &str,
    available_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for raw in output.lines().filter(|l| !l.trim().is_empty()) {
        if raw.starts_with("    ") {
            lines.extend(readable::indented_wrapped(
                vec![format!("    {}", raw.trim())],
                available_width,
                theme.fg(Slot::Muted),
            ));
        } else if let Some((name, description)) =
            raw.strip_prefix("  ").and_then(|row| row.split_once(" — "))
        {
            lines.extend(name_row(name, description, available_width, theme));
        } else if let Some(row) = raw.strip_prefix("  ") {
            lines.extend(name_row(row, "", available_width, theme));
        } else {
            lines.push(header(raw));
        }
    }
    lines
}

/// An `mcp_tools` reply: the server header, then per tool its row, its
/// parameters, and the call line.
pub(super) fn render_mcp_tools_output(
    output: &str,
    available_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for raw in output.lines().filter(|l| !l.trim().is_empty()) {
        if let Some(row) = raw.strip_prefix("- ") {
            let (name, description) = row.split_once(": ").unwrap_or((row, ""));
            lines.extend(name_row(name, description, available_width, theme));
        } else if raw.starts_with('{') {
            match serde_json::from_str::<Value>(raw) {
                Ok(schema) => lines.extend(parameter_lines(&schema)),
                Err(_) => lines.extend(readable::render_output(raw, available_width)),
            }
        } else if raw.starts_with("call as: ") {
            lines.extend(readable::indented_wrapped(
                vec![format!("    {raw}")],
                available_width,
                theme.fg(Slot::Muted),
            ));
        } else {
            lines.push(header(raw));
        }
    }
    lines
}

fn parameter_lines(schema: &Value) -> Vec<Line<'static>> {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(properties) = schema
        .get("properties")
        .and_then(Value::as_object)
        .filter(|p| !p.is_empty())
    else {
        return vec![Line::from("      (no parameters)")];
    };
    properties
        .iter()
        .map(|(name, property)| {
            let flag = if required.contains(&name.as_str()) {
                ", required"
            } else {
                ""
            };
            Line::from(format!("      {name}: {}{flag}", type_label(property)))
        })
        .collect()
}

fn type_label(property: &Value) -> String {
    let base = match property.get("type") {
        Some(Value::String(t)) if t == "array" => match property.get("items") {
            Some(items) => format!("array of {}", type_label(items)),
            None => "array".to_string(),
        },
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(types)) => types
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" | "),
        _ => "any".to_string(),
    };
    match property.get("enum").and_then(Value::as_array) {
        Some(values) => {
            let values: Vec<String> = values
                .iter()
                .map(|v| v.as_str().map_or_else(|| v.to_string(), String::from))
                .collect();
            format!("{base} (one of {})", values.join(", "))
        }
        None => base,
    }
}

/// `    name — description`, the name in the tool-name color, word-wrapped with
/// continuation lines hanging under the name.
fn name_row(
    name: &str,
    description: &str,
    available_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut spans = vec![Span::styled(name.to_string(), theme.fg(Slot::Accent))];
    if !description.is_empty() {
        spans.push(Span::raw(format!(" — {description}")));
    }
    wrap::wrap_line(Line::from(spans), available_width.saturating_sub(10).max(1))
        .into_iter()
        .enumerate()
        .map(|(i, wline)| {
            let mut row = vec![Span::raw(if i == 0 { "    " } else { "      " })];
            row.extend(
                wline
                    .spans
                    .into_iter()
                    .map(|s| Span::styled(s.content.into_owned(), s.style)),
            );
            Line::from(row)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::tui::markdown::MarkdownRenderer;
    use crate::tui::theme::Theme;

    fn expand(tool: &str, input: &str, output: &str) -> Vec<String> {
        super::super::render_expansion(
            Some(tool),
            input,
            Some(output),
            false,
            Theme::default(),
            100,
            &MarkdownRenderer::new(),
        )
        .lines
        .iter()
        .map(super::super::collect_line)
        .collect()
    }

    #[test]
    fn list_mcp_renders_header_rows_and_tool_names() {
        let output = "MCP servers — Call a tool with mcp {...}\n  gh — GitHub [connected]\n    tools: search, read\n  kb [available]";
        let rendered = expand("list_mcp", "{}", output);
        assert_eq!(
            rendered,
            vec![
                "  MCP servers — Call a tool with mcp {...}",
                "    gh — GitHub [connected]",
                "      tools: search, read",
                "    kb [available]",
            ]
        );
    }

    #[test]
    fn list_agents_renders_like_the_other_lists() {
        let rendered = expand(
            "list_agents",
            "{}",
            "Agents (start one with agent):\n  general — general-purpose work",
        );
        assert_eq!(
            rendered,
            vec![
                "  Agents (start one with agent):",
                "    general — general-purpose work",
            ]
        );
    }

    #[test]
    fn mcp_tools_renders_rows_parameters_and_the_call_line_not_json() {
        let output = "MCP server `gh` (GitHub) — tools:\n\n- search: search issues\n{\"type\":\"object\",\"properties\":{\"q\":{\"type\":\"string\"},\"labels\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"mode\":{\"type\":\"string\",\"enum\":[\"a\",\"b\"]}},\"required\":[\"q\"]}\ncall as: mcp {\"server\": \"gh\", \"tool\": \"search\", \"args\": {\"q\":\"example\"}}";
        let rendered = expand("mcp_tools", r#"{"name":"gh"}"#, output);
        for expected in [
            "  server: gh",
            "  MCP server `gh` (GitHub) — tools:",
            "    search — search issues",
            "      labels: array of string",
            "      mode: string (one of a, b)",
            "      q: string, required",
            r#"      call as: mcp {"server": "gh", "tool": "search", "args": {"q":"example"}}"#,
        ] {
            assert!(
                rendered.iter().any(|l| l == expected),
                "missing {expected:?} in {rendered:#?}"
            );
        }
        assert!(
            !rendered.join("\n").contains("\"properties\""),
            "{rendered:#?}"
        );
    }
}
