//! `explore`/`describe` bodies (ADR-0204 §6). The discovery pair renders like
//! any other call: its query, then a readable result — `explore`'s sectioned
//! index as `name — description` rows under a heading per source kind, and
//! `describe`'s `Loaded: …` line plus a parameter summary (name, type,
//! required) per schema — instead of the reply's raw text or JSON.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;

use crate::tui::wrap;

use super::{parse_input, readable};

pub(super) fn render_explore_input(input: &str) -> Vec<Line<'static>> {
    let value = parse_input(input);
    let mut lines = Vec::new();
    for key in ["filter", "kind"] {
        if let Some(v) = value.get(key).and_then(Value::as_str) {
            lines.push(Line::from(format!("  {key}: {v}")));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from("  (whole catalog)"));
    }
    lines
}

pub(super) fn render_describe_input(input: &str) -> Vec<Line<'static>> {
    let value = parse_input(input);
    let names: Vec<&str> = value
        .get("names")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    vec![Line::from(format!("  names: {}", names.join(", ")))]
}

/// `explore`'s reply (`discover::sections::render_sections`): one header line
/// per non-empty section, `  name — description` rows beneath it, or
/// `(no matches)`.
pub(super) fn render_explore_output(output: &str, available_width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut rows = 0;
    for raw in output.lines().filter(|l| !l.trim().is_empty()) {
        match raw.strip_prefix("  ").and_then(|row| row.split_once(" — ")) {
            Some((name, description)) => {
                rows += 1;
                lines.extend(name_row(name, description, available_width));
            }
            None if raw.starts_with(' ') => {
                lines.extend(readable::render_output(raw.trim(), available_width))
            }
            None => lines.push(Line::from(Span::styled(
                format!("  {}", section_label(raw)),
                Style::default().add_modifier(Modifier::BOLD),
            ))),
        }
    }
    if rows > 0 {
        let plural = if rows == 1 { "" } else { "s" };
        lines.insert(0, Line::from(format!("  {rows} result{plural}")));
    }
    lines
}

/// The short source heading for an `explore` section header — the MCP header
/// carries a long how-to-enable sentence written for the model, not the user.
fn section_label(header: &str) -> &str {
    match header {
        "TOOLS" => "built-in tools",
        "SKILLS" => "skills",
        "ENDPOINTS" => "endpoints",
        h if h.starts_with("MCP") => "MCP servers",
        h => h,
    }
}

/// `describe`'s reply (`discover::describe`): an optional `Loaded: a, b.
/// <call hint>` paragraph, then a JSON array of `{name, description, schema}`
/// / `{name, error}` / `{name, note}` entries (or, on the `anthropic_native`
/// wire, only the non-schema entries).
pub(super) fn render_describe_output(output: &str, available_width: u16) -> Vec<Line<'static>> {
    let (loaded, rest) = match output.strip_prefix("Loaded: ") {
        Some(after) => after
            .split_once("\n\n")
            .map_or((Some(after), ""), |(l, r)| (Some(l), r)),
        None => (None, output),
    };
    let mut lines = Vec::new();
    if let Some(loaded) = loaded {
        let (names, hint) = loaded.split_once(". ").unwrap_or((loaded, ""));
        lines.push(Line::from(vec![
            Span::styled("  Loaded: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(names.trim_end_matches('.').to_string()),
        ]));
        if !hint.is_empty() {
            lines.extend(readable::indented_wrapped(
                vec![hint.to_string()],
                available_width,
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    if rest.trim().is_empty() {
        return lines;
    }
    match serde_json::from_str::<Value>(rest) {
        Ok(Value::Array(entries)) => {
            for entry in &entries {
                lines.extend(schema_entry(entry, available_width));
            }
        }
        _ => lines.extend(readable::render_output(rest, available_width)),
    }
    lines
}

fn schema_entry(entry: &Value, available_width: u16) -> Vec<Line<'static>> {
    let name = entry.get("name").and_then(Value::as_str).unwrap_or("?");
    let description = entry
        .get("description")
        .and_then(Value::as_str)
        .and_then(|d| d.lines().next())
        .unwrap_or("");
    let mut lines = name_row(name, description, available_width);
    if let Some(error) = entry.get("error").and_then(Value::as_str) {
        lines.extend(readable::indented_wrapped(
            vec![format!("    ✗ {error}")],
            available_width,
            Style::default().fg(Color::Red),
        ));
    }
    if let Some(note) = entry.get("note").and_then(Value::as_str) {
        lines.extend(readable::indented_wrapped(
            vec![format!("    {note}")],
            available_width,
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(schema) = entry.get("schema") {
        lines.extend(parameter_lines(schema));
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
fn name_row(name: &str, description: &str, available_width: u16) -> Vec<Line<'static>> {
    let mut spans = vec![Span::styled(
        name.to_string(),
        Style::default().fg(Color::Cyan),
    )];
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
    fn explore_renders_filter_and_rows_under_source_headings() {
        let output = "TOOLS\n  grep — Search file contents\n\nMCP servers — a server bundles tools; enable the server (mcp_enable {\"server\": \"<name>\"}), then call its tools by their mcp__<server>__<tool> names:\n  mcp__gh__issue — Create an issue\n";
        let rendered = expand("explore", r#"{"filter":"issue","kind":"mcp"}"#, output);
        assert_eq!(
            rendered,
            vec![
                "  filter: issue",
                "  kind: mcp",
                "  2 results",
                "  built-in tools",
                "    grep — Search file contents",
                "  MCP servers",
                "    mcp__gh__issue — Create an issue",
            ]
        );
    }

    #[test]
    fn explore_without_query_or_matches_stays_readable() {
        let rendered = expand("explore", "{}", "(no matches)");
        assert_eq!(rendered, vec!["  (whole catalog)", "  (no matches)"]);
    }

    #[test]
    fn describe_renders_loaded_line_and_parameter_summary_not_json() {
        let output = "Loaded: grep. Call each through invoke {\"name\": \"<tool>\", \"args\": {...}}; they cannot be called directly.\n\n[\n  {\"name\": \"grep\", \"description\": \"Search file contents\\nMore.\", \"schema\": {\"type\": \"object\", \"properties\": {\"pattern\": {\"type\": \"string\"}, \"exclude\": {\"type\": \"array\", \"items\": {\"type\": \"string\"}}, \"mode\": {\"type\": \"string\", \"enum\": [\"a\", \"b\"]}}, \"required\": [\"pattern\"]}},\n  {\"name\": \"nope\", \"error\": \"unknown tool: `nope`\"}\n]";
        let rendered = expand("describe", r#"{"names":["grep","nope"]}"#, output);
        for expected in [
            "  names: grep, nope",
            "  Loaded: grep",
            "    grep — Search file contents",
            "      exclude: array of string",
            "      mode: string (one of a, b)",
            "      pattern: string, required",
            "    nope",
            "      ✗ unknown tool: `nope`",
        ] {
            assert!(
                rendered.iter().any(|l| l == expected),
                "missing {expected:?} in {rendered:#?}"
            );
        }
        assert!(!rendered.join("\n").contains("\"schema\""), "{rendered:#?}");
    }
}
