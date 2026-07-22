use ratatui::{
    style::{Color, Style},
    text::{Line, Span, Text},
};

use crate::tui::diff::DiffRenderer;
use crate::tui::markdown::MarkdownRenderer;
use crate::tui::theme::Theme;
use crate::tui::wrap;

pub fn render_tool_output(
    tool_name: Option<&str>,
    output: &str,
    theme: Theme,
    available_width: u16,
) -> Text<'static> {
    match tool_name {
        Some("edit") => render_edit_output(output, theme, available_width),
        Some("read") => render_read_output(output, theme, available_width),
        Some("glob") => render_glob_output(output, theme, available_width),
        Some("grep") => render_grep_output(output, theme, available_width),
        Some("bash") => render_plain_output(output, theme, available_width),
        _ => render_plain_output(output, theme, available_width),
    }
}

/// Build the expanded body of a tool block from **both** the call `input` and
/// its `output` (#341). Each operation gets a body that means something:
/// `read` → the file body, `edit` → a real diff, `write` → the new content,
/// `bash`/`call` → the command output (the command is already in the header),
/// the orchestration tools (`agent`/`ask_user`/`propose_plan`/…) → readable
/// prose instead of raw JSON, and every unknown tool → pretty-printed input
/// followed by the output body. The filename/command lives in the block header
/// (#340), never re-printed here.
///
/// `md` renders the plan/task markdown for `propose_plan`/`update_plan`/
/// `update_tasks`; it is ignored by the other arms. Wired into the live
/// transcript by `flush_tool_call`'s expanded branch (#340).
pub fn render_expansion(
    tool: Option<&str>,
    input: &str,
    output: &str,
    theme: Theme,
    available_width: u16,
    md: &MarkdownRenderer,
) -> Text<'static> {
    match tool {
        Some("read") => render_read_output(output, theme, available_width),
        Some("edit") => {
            let v: serde_json::Value =
                serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
            let old = v.get("oldString").and_then(|s| s.as_str()).unwrap_or("");
            let new = v.get("newString").and_then(|s| s.as_str()).unwrap_or("");
            DiffRenderer::render_change(old, new)
        }
        Some("write") => {
            let v: serde_json::Value =
                serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
            let content = v.get("content").and_then(|s| s.as_str()).unwrap_or("");
            Text::from(
                content
                    .lines()
                    .map(|line| Line::from(format!("  {line}")))
                    .collect::<Vec<_>>(),
            )
        }
        Some("bash") | Some("call") => render_plain_output(output, theme, available_width),
        Some("agent") | Some("agent_spawn") => {
            let v: serde_json::Value =
                serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
            render_prompt_body(
                v.get("prompt").and_then(|p| p.as_str()).unwrap_or(""),
                available_width,
            )
        }
        Some("agent_poll") => {
            let v: serde_json::Value =
                serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
            render_agent_poll_body(
                v.get("agent_id").and_then(|s| s.as_str()).unwrap_or(""),
                v.get("timeout_secs").and_then(|t| t.as_u64()),
            )
        }
        Some("ask_user") => {
            let v: serde_json::Value =
                serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
            render_ask_user_body(&v)
        }
        Some("propose_plan") => {
            let v: serde_json::Value =
                serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
            let plan = v.get("plan").and_then(|s| s.as_str()).unwrap_or("");
            render_markdown_body(md, plan, available_width)
        }
        Some("update_plan") | Some("update_tasks") => {
            let v: serde_json::Value =
                serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
            let content = v.get("content").and_then(|s| s.as_str()).unwrap_or("");
            render_markdown_body(md, content, available_width)
        }
        Some("load_skill") => {
            let v: serde_json::Value =
                serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
            render_skill_body(v.get("skill_name").and_then(|s| s.as_str()).unwrap_or(""))
        }
        _ => {
            let mut lines = Vec::new();
            match serde_json::from_str::<serde_json::Value>(input)
                .ok()
                .and_then(|v| serde_json::to_string_pretty(&v).ok())
            {
                Some(pretty) => {
                    for line in pretty.lines() {
                        lines.push(Line::from(format!("  {line}")));
                    }
                }
                None => {
                    for line in input.lines() {
                        lines.push(Line::from(format!("  {line}")));
                    }
                }
            }
            lines.extend(render_plain_output(output, theme, available_width).lines);
            Text::from(lines)
        }
    }
}

/// Wrap and indent a multi-line plain-text body (e.g. an `agent`/`agent_spawn`
/// `prompt`). Word-wraps at `available_width - 4` so long prompts don't overflow
/// horizontally, matching how assistant text runs are wrapped.
fn render_prompt_body(prompt: &str, available_width: u16) -> Text<'static> {
    let mut lines = Vec::new();
    let wrap_width = available_width.saturating_sub(4);
    for raw in prompt.lines() {
        if raw.trim().is_empty() {
            lines.push(Line::from(""));
            continue;
        }
        for wline in wrap::wrap_line(Line::from(raw.to_string()), wrap_width) {
            lines.push(Line::from(format!("  {}", collect_line(&wline))));
        }
    }
    Text::from(lines)
}

/// A compact `agent_id` + `timeout_secs` summary for an `agent_poll` body.
fn render_agent_poll_body(agent_id: &str, timeout_secs: Option<u64>) -> Text<'static> {
    let mut lines = Vec::new();
    lines.push(Line::from(format!("  agent_id: {agent_id}")));
    if let Some(t) = timeout_secs {
        lines.push(Line::from(format!("  timeout_secs: {t}")));
    }
    Text::from(lines)
}

/// An `ask_user` body (#488): each question followed by its numbered option
/// labels. Accepts the current `{"questions": [...]}` array shape as well as
/// the legacy single-question `{"question", "options"}` shape, so a replayed
/// pre-#488 log still renders.
fn render_ask_user_body(value: &serde_json::Value) -> Text<'static> {
    let mut lines = Vec::new();
    let questions = value
        .get("questions")
        .and_then(|q| q.as_array())
        .cloned()
        .unwrap_or_else(|| vec![value.clone()]);
    for question in &questions {
        if let Some(q) = question.get("question").and_then(|s| s.as_str()) {
            lines.push(Line::from(format!("  {q}")));
        }
        if let Some(options) = question.get("options").and_then(|o| o.as_array()) {
            for (i, opt) in options.iter().enumerate() {
                if let Some(label) = opt.get("label").and_then(|s| s.as_str()) {
                    lines.push(Line::from(format!("  {}. {label}", i + 1)));
                }
            }
        }
        if question
            .get("multi_select")
            .and_then(|f| f.as_bool())
            .unwrap_or(false)
        {
            lines.push(Line::from("  (multiple selections allowed)"));
        }
    }
    Text::from(lines)
}

/// A `load_skill` body: the skill name on its own indented line.
fn render_skill_body(skill_name: &str) -> Text<'static> {
    Text::from(vec![Line::from(format!("  {skill_name}"))])
}

/// Render a markdown body (a plan or task snapshot) via the shared
/// [`MarkdownRenderer`], word-wrapping each rendered line at
/// `available_width - 4` so long paragraphs don't overflow — mirroring how
/// assistant text runs are wrapped (`render_text_run`).
fn render_markdown_body(
    md: &MarkdownRenderer,
    markdown: &str,
    available_width: u16,
) -> Text<'static> {
    if markdown.trim().is_empty() {
        return Text::default();
    }
    let wrap_width = available_width.saturating_sub(4);
    let mut lines = Vec::new();
    for line in md.render(markdown).lines {
        for wline in wrap::wrap_line(line, wrap_width) {
            lines.push(Line::from(format!("  {}", collect_line(&wline))));
        }
    }
    Text::from(lines)
}

/// Flatten a `Line`'s spans into a single owned `String` for the indentation
/// helpers above (they re-wrap into a fresh `Line` with the 2-space indent
/// applied uniformly, which is all these orchestration bodies need).
fn collect_line(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn render_edit_output(output: &str, _theme: Theme, _available_width: u16) -> Text<'static> {
    if output.contains("created file:") {
        let line = Line::from(vec![
            Span::styled("✓ ", Style::default().fg(Color::Green)),
            Span::raw(output.to_string()),
        ]);
        return Text::from(vec![line]);
    }

    if output.contains("matches replaced") {
        let line = Line::from(vec![
            Span::styled("✓ ", Style::default().fg(Color::Green)),
            Span::raw(output.to_string()),
        ]);
        return Text::from(vec![line]);
    }

    Text::raw(output.to_string())
}

/// The file body of a `read`. The filename lives in the block header (#340), so
/// the expanded body is just the contents — indented like other tool output.
fn render_read_output(output: &str, _theme: Theme, _available_width: u16) -> Text<'static> {
    Text::from(
        output
            .lines()
            .map(|line| Line::from(format!("  {line}")))
            .collect::<Vec<_>>(),
    )
}

fn render_glob_output(output: &str, _theme: Theme, _available_width: u16) -> Text<'static> {
    let mut lines = Vec::new();

    let mut file_count = 0;
    let mut dir_count = 0;

    for line in output.lines() {
        if line.contains("matched") && line.contains("director") {
            dir_count = line
                .chars()
                .filter(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0);
        } else if !line.trim().is_empty() && !line.starts_with("matched") {
            file_count += 1;
            lines.push(Line::from(format!("  {}", line)));
        }
    }

    let header = if file_count > 0 {
        let msg = format!("{} files matched", file_count);
        Line::from(vec![
            Span::styled("✓ ", Style::default().fg(Color::Green)),
            Span::styled(msg, Style::default().fg(Color::Cyan)),
        ])
    } else if dir_count > 0 {
        let msg = format!(
            "{} directories matched (use pattern/* to list files)",
            dir_count
        );
        Line::from(vec![
            Span::styled("⚠ ", Style::default().fg(Color::Yellow)),
            Span::styled(msg, Style::default().fg(Color::Yellow)),
        ])
    } else {
        Line::from(vec![
            Span::styled("✗ ", Style::default().fg(Color::Red)),
            Span::raw("No matches found"),
        ])
    };

    let mut result = vec![header];
    result.extend(lines);
    Text::from(result)
}

fn render_grep_output(output: &str, _theme: Theme, _available_width: u16) -> Text<'static> {
    let mut lines = Vec::new();
    let mut match_count = 0;

    for line in output.lines() {
        if line.contains(':') {
            match_count += 1;
            let parts: Vec<&str> = line.splitn(2, ':').collect();
            if parts.len() == 2 {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {}:", parts[0]), Style::default().fg(Color::Cyan)),
                    Span::raw(parts[1].to_string()),
                ]));
            }
        }
    }

    let msg = format!("{} matches found", match_count);
    let header = Line::from(vec![
        Span::styled("✓ ", Style::default().fg(Color::Green)),
        Span::styled(msg, Style::default().fg(Color::Cyan)),
    ]);

    let mut result = vec![header];
    result.extend(lines);
    Text::from(result)
}

fn render_plain_output(output: &str, _theme: Theme, _available_width: u16) -> Text<'static> {
    let mut lines = Vec::new();
    for line in output.lines() {
        lines.push(Line::from(format!("  {}", line)));
    }
    Text::from(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_with_hyphenated_paths() {
        let output = "docs/adr/0001-actor-model-abi.md\ndocs/adr/0002-protocol.md\nsrc/main.rs\n";
        let theme = Theme::default();
        let result = render_glob_output(output, theme, 80);
        let text: String = result
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            text.contains("0001-actor-model-abi.md"),
            "Should render hyphenated paths, got: {text}"
        );
        assert!(
            text.contains("0002-protocol.md"),
            "Should render hyphenated paths, got: {text}"
        );
        assert!(
            text.contains("3 files matched"),
            "Should show file count, got: {text}"
        );
    }

    #[test]
    fn test_glob_with_directories_only() {
        let output = "pattern `**` matched 3 directories but no files (files are filtered out). Try `**/*` to list files inside those directories.\n";
        let theme = Theme::default();
        let result = render_glob_output(output, theme, 80);
        let text: String = result
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            text.contains("3 directories matched"),
            "Should show directory hint, got: {text}"
        );
    }

    #[test]
    fn test_grep_with_matches() {
        let output = "src/main.rs:10: fn main() {\nsrc/main.rs:20: println!(\"Hello\");\n";
        let theme = Theme::default();
        let result = render_grep_output(output, theme, 80);
        let text: String = result
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("2 matches found"), "Should show match count");
        assert!(text.contains("src/main.rs:10:"), "Should show file:line");
        assert!(text.contains("src/main.rs:20:"), "Should show file:line");
    }

    #[test]
    fn test_edit_creates_file() {
        let output = "created file: test.txt";
        let theme = Theme::default();
        let result = render_edit_output(output, theme, 80);
        let text: String = result
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("✓"), "Should show checkmark");
        assert!(
            text.contains("created file"),
            "Should show creation message"
        );
    }

    fn flatten(text: &Text<'_>) -> String {
        text.lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn test_read_renders_body() {
        let output = "1: line 1\n2: line 2\n3: line 3\n";
        let result = render_read_output(output, Theme::default(), 80);
        let text = flatten(&result);
        assert!(text.contains("line 1"), "read should render the file body");
        assert!(text.contains("line 3"), "read should render the file body");
    }

    #[test]
    fn test_expansion_read_shows_body() {
        let result = render_expansion(
            Some("read"),
            r#"{"path":"src/main.rs"}"#,
            "fn main() {}\n",
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        assert!(
            flatten(&result).contains("fn main() {}"),
            "read expansion should show the file body"
        );
    }

    #[test]
    fn test_expansion_edit_shows_diff() {
        let result = render_expansion(
            Some("edit"),
            r#"{"path":"a.rs","oldString":"a","newString":"b"}"#,
            "",
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let has_delete = result
            .lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content == "- "));
        let has_insert = result
            .lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content == "+ "));
        assert!(
            has_delete && has_insert,
            "edit expansion should render a `-`/`+` pair"
        );
    }

    #[test]
    fn test_expansion_write_shows_content() {
        let result = render_expansion(
            Some("write"),
            r#"{"path":"a.rs","content":"hello\nworld"}"#,
            "",
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(
            text.contains("hello"),
            "write expansion should show the content"
        );
        assert!(
            text.contains("world"),
            "write expansion should show the content"
        );
    }

    #[test]
    fn test_expansion_propose_plan_renders_markdown_not_json() {
        let result = render_expansion(
            Some("propose_plan"),
            r##"{"plan":"# Goal\nDo X"}"##,
            "",
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(
            text.contains("Goal"),
            "propose_plan expansion should render the plan heading"
        );
        assert!(
            !text.contains('{'),
            "propose_plan expansion must not dump raw JSON braces: {text:?}"
        );
        assert!(
            !text.contains("\"plan\""),
            "propose_plan expansion must not dump the JSON field name: {text:?}"
        );
    }

    #[test]
    fn test_expansion_update_plan_renders_markdown() {
        let result = render_expansion(
            Some("update_plan"),
            r##"{"content":"# Step 1"}"##,
            "",
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(
            text.contains("Step 1"),
            "update_plan expansion should render the content heading"
        );
        assert!(
            !text.contains('{'),
            "update_plan expansion must not dump raw JSON braces: {text:?}"
        );
    }

    #[test]
    fn test_expansion_agent_spawn_renders_prompt() {
        let result = render_expansion(
            Some("agent_spawn"),
            r#"{"agent":"explore","prompt":"find X"}"#,
            "",
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(
            text.contains("find X"),
            "agent_spawn expansion should render the prompt text"
        );
        assert!(
            !text.contains('{'),
            "agent_spawn expansion must not dump raw JSON braces: {text:?}"
        );
    }

    #[test]
    fn test_expansion_agent_renders_prompt() {
        let result = render_expansion(
            Some("agent"),
            r#"{"agent":"backend","prompt":"wire it up"}"#,
            "",
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(
            text.contains("wire it up"),
            "agent expansion should render the prompt text"
        );
    }

    #[test]
    fn test_expansion_ask_user_renders_legacy_single_question_shape() {
        let result = render_expansion(
            Some("ask_user"),
            r#"{"question":"Which?","options":[{"label":"A","description":"x"}]}"#,
            "",
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(
            text.contains("Which?"),
            "ask_user expansion should render the question"
        );
        assert!(
            text.contains("A"),
            "ask_user expansion should render the option label"
        );
        assert!(
            !text.contains('{'),
            "ask_user expansion must not dump raw JSON braces: {text:?}"
        );
    }

    #[test]
    fn test_expansion_ask_user_renders_multiple_questions() {
        let result = render_expansion(
            Some("ask_user"),
            r#"{"questions":[
                {"question":"Which DB?","options":[{"label":"Postgres"}]},
                {"question":"Which regions?","options":[{"label":"us-east"}],"multi_select":true}
            ]}"#,
            "",
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(text.contains("Which DB?"), "{text:?}");
        assert!(text.contains("Which regions?"), "{text:?}");
        assert!(text.contains("Postgres"), "{text:?}");
        assert!(
            text.contains("multiple selections allowed"),
            "multi_select question should note it: {text:?}"
        );
    }

    #[test]
    fn test_expansion_load_skill_renders_name() {
        let result = render_expansion(
            Some("load_skill"),
            r#"{"skill_name":"arch"}"#,
            "",
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(
            text.contains("arch"),
            "load_skill expansion should render the skill name"
        );
        assert!(
            !text.contains('{'),
            "load_skill expansion must not dump raw JSON braces: {text:?}"
        );
    }

    #[test]
    fn test_expansion_agent_poll_renders_id_and_timeout() {
        let result = render_expansion(
            Some("agent_poll"),
            r#"{"agent_id":"abc","timeout_secs":60}"#,
            "",
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(
            text.contains("abc") && text.contains("60"),
            "agent_poll expansion should render the agent_id and timeout_secs: {text:?}"
        );
    }
}
