use std::borrow::Cow;

use ratatui::{
    style::{Color, Style},
    text::{Line, Span, Text},
};

use crate::run::summary;
use crate::tui::markdown::MarkdownRenderer;
use crate::tui::theme::Theme;

mod discovery;
mod expansion;
mod orchestration;
mod readable;
mod search_output;

pub use expansion::render_write_approval_body;
use search_output::{render_glob_output, render_grep_output};

/// The standalone `ToolOutput` block (an output with no paired call): the
/// per-tool output renderer, readable `key: value` lines for any other named
/// tool, plain text for a head-local status notice (`None`).
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
        Some("explore") => Text::from(discovery::render_explore_output(output, available_width)),
        Some("describe") => Text::from(discovery::render_describe_output(output, available_width)),
        Some(_) => Text::from(readable::render_output(output, available_width)),
        None => render_plain_output(output, theme, available_width),
    }
}

/// Build the expanded body of a tool block: a per-tool body for the call
/// `input` (#341), then its `output` once the call has run — `None` while it
/// is in flight or awaiting approval (#519: an approval preview is never left
/// blank). Every tool reads as what it did: `read` → the full path + the file
/// body, `edit` → the path + a real diff, `write`/`apply_patch` → the path +
/// the content/patch as a diff, `bash`/`call`/`rhai` → the full command or
/// script + its output, `glob`/`grep` → the full pattern/filter, the
/// orchestration tools → readable prose, `explore`/`describe` → the query and
/// a readable index/schema summary, and everything else (MCP, endpoint, skill
/// and unknown tools) → `key: value` arguments and output (ADR-0204 §6). A
/// failed call's (`is_error`) output is marked in the error color.
///
/// `md` renders the markdown bodies (plans, task snapshots, sub-agent
/// replies). Wired into the live transcript by `flush_tool_call`'s expanded
/// branch (#340) and into the approval tail by `transcript.rs` (#487/#519).
pub fn render_expansion(
    tool: Option<&str>,
    input: &str,
    output: Option<&str>,
    is_error: bool,
    theme: Theme,
    available_width: u16,
    md: &MarkdownRenderer,
) -> Text<'static> {
    let (tool, input) = match tool {
        Some(t) => {
            let (t, i) = summary::unwrap_invoke(t, input);
            (Some(t), i)
        }
        None => (None, Cow::Borrowed(input)),
    };
    let tool = tool.as_deref();
    let mut lines = input_body(tool, &input, available_width, md);
    match output {
        Some(out) if is_error => lines.extend(readable::render_error_output(
            out,
            available_width,
            theme.error_colors().fg,
        )),
        Some(out) => lines.extend(output_body(tool, out, theme, available_width, md)),
        None => {}
    }
    Text::from(lines)
}

fn input_body(
    tool: Option<&str>,
    input: &str,
    available_width: u16,
    md: &MarkdownRenderer,
) -> Vec<Line<'static>> {
    match tool {
        Some("read") => expansion::render_read_input(input),
        Some("edit") => expansion::render_edit_input(input),
        Some("write") => expansion::render_write_expansion(input).lines,
        Some("apply_patch") => expansion::render_apply_patch_expansion(input).lines,
        Some(t @ ("bash" | "call")) => expansion::render_command_input(t, input, available_width),
        Some("glob") => expansion::render_glob_input(input),
        Some("grep") => expansion::render_grep_input(input),
        Some("rhai") => expansion::render_rhai_input(input, available_width),
        Some("explore") => discovery::render_explore_input(input),
        Some("describe") => discovery::render_describe_input(input),
        Some(
            t @ ("agent" | "agent_send" | "poll" | "ask_user" | "propose_plan" | "update_tasks"
            | "load_skill"),
        ) => orchestration::render_input(t, input, available_width, md),
        _ => readable::render_args(input, available_width),
    }
}

fn output_body(
    tool: Option<&str>,
    output: &str,
    theme: Theme,
    available_width: u16,
    md: &MarkdownRenderer,
) -> Vec<Line<'static>> {
    match tool {
        // An empty glob/grep result *is* the answer ("no matches").
        Some("glob") => render_glob_output(output, theme, available_width).lines,
        Some("grep") => render_grep_output(output, theme, available_width).lines,
        _ if output.trim().is_empty() => Vec::new(),
        Some("read") => render_read_output(output, theme, available_width).lines,
        Some("edit") => render_edit_output(output, theme, available_width).lines,
        Some("bash" | "call") => render_plain_output(output, theme, available_width).lines,
        Some("explore") => discovery::render_explore_output(output, available_width),
        Some("describe") => discovery::render_describe_output(output, available_width),
        Some("agent" | "agent_send") => {
            orchestration::render_markdown_body(md, output, available_width).lines
        }
        _ => readable::render_output(output, available_width),
    }
}

/// A tool input parsed as JSON, `Null` when malformed (e.g. a still-streaming
/// fragment) so every body degrades to its empty fields instead of failing.
pub(super) fn parse_input(input: &str) -> serde_json::Value {
    serde_json::from_str(input).unwrap_or(serde_json::Value::Null)
}

/// Flatten a `Line`'s spans into a single owned `String` for the indentation
/// helpers (they re-wrap into a fresh `Line` with the indent applied
/// uniformly, which is all these bodies need).
pub(super) fn collect_line(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn render_edit_output(output: &str, theme: Theme, available_width: u16) -> Text<'static> {
    if output.contains("created file:") || output.contains("matches replaced") {
        let line = Line::from(vec![
            Span::styled("✓ ", Style::default().fg(Color::Green)),
            Span::raw(output.to_string()),
        ]);
        return Text::from(vec![line]);
    }
    render_plain_output(output, theme, available_width)
}

/// The file body of a `read`. The filename lives in the block header (#340), so
/// the body is just the contents — indented like other tool output.
pub(super) fn render_read_output(
    output: &str,
    _theme: Theme,
    _available_width: u16,
) -> Text<'static> {
    Text::from(
        output
            .lines()
            .map(|line| Line::from(format!("  {line}")))
            .collect::<Vec<_>>(),
    )
}

pub(super) fn render_plain_output(
    output: &str,
    _theme: Theme,
    _available_width: u16,
) -> Text<'static> {
    Text::from(
        output
            .lines()
            .map(|line| Line::from(format!("  {line}")))
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
            Some("fn main() {}\n"),
            false,
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
            None,
            false,
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
            None,
            false,
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
            r##"{"content":"# Goal\nDo X","path":".entanglement/plans/s1.md"}"##,
            None,
            false,
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
            !text.contains("\"content\""),
            "propose_plan expansion must not dump the JSON field name: {text:?}"
        );
    }

    #[test]
    fn test_expansion_propose_plan_path_only_names_the_file() {
        // A raw `ToolCall`'s `path`-mode input carries no inline content —
        // nothing to render as markdown without a disk read, so the file is
        // named instead of leaving the block blank (#519).
        let result = render_expansion(
            Some("propose_plan"),
            r#"{"path":".entanglement/plans/s1.md"}"#,
            None,
            false,
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(
            text.contains(".entanglement/plans/s1.md"),
            "propose_plan path-only expansion should name the file: {text:?}"
        );
    }

    #[test]
    fn test_expansion_update_tasks_renders_markdown() {
        let result = render_expansion(
            Some("update_tasks"),
            r##"{"content":"# Step 1"}"##,
            None,
            false,
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(
            text.contains("Step 1"),
            "update_tasks expansion should render the content heading"
        );
        assert!(
            !text.contains('{'),
            "update_tasks expansion must not dump raw JSON braces: {text:?}"
        );
    }

    #[test]
    fn test_expansion_agent_renders_prompt() {
        let result = render_expansion(
            Some("agent"),
            r#"{"agent":"backend","prompt":"wire it up"}"#,
            None,
            false,
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(
            text.contains("wire it up"),
            "agent expansion should render the prompt text"
        );
        assert!(
            !text.contains('{'),
            "agent expansion must not dump raw JSON braces: {text:?}"
        );
    }

    #[test]
    fn test_expansion_ask_user_renders_legacy_single_question_shape() {
        let result = render_expansion(
            Some("ask_user"),
            r#"{"question":"Which?","options":[{"label":"A","description":"x"}]}"#,
            None,
            false,
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
            None,
            false,
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
            None,
            false,
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
    fn test_expansion_agent_send_renders_agent_id_and_prompt() {
        let result = render_expansion(
            Some("agent_send"),
            r#"{"agent_id":"s-abc123","prompt":"focus on Y instead"}"#,
            None,
            false,
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(text.contains("s-abc123"), "{text:?}");
        assert!(text.contains("focus on Y instead"), "{text:?}");
        assert!(
            !text.contains('{'),
            "agent_send expansion must not dump raw JSON braces: {text:?}"
        );
    }

    #[test]
    fn test_expansion_poll_renders_handle_and_timeout() {
        let result = render_expansion(
            Some("poll"),
            r#"{"handle":"abc","timeout_secs":60}"#,
            None,
            false,
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(
            text.contains("abc") && text.contains("60"),
            "poll expansion should render the handle and timeout_secs: {text:?}"
        );
    }

    #[test]
    fn test_expansion_poll_and_agent_show_their_output() {
        let md = MarkdownRenderer::new();
        let poll = render_expansion(
            Some("poll"),
            r#"{"handle":"j1"}"#,
            Some("exited 0\nbuild ok"),
            false,
            Theme::default(),
            80,
            &md,
        );
        assert!(flatten(&poll).contains("build ok"), "{:?}", flatten(&poll));
        let agent = render_expansion(
            Some("agent"),
            r#"{"agent":"explore","prompt":"look"}"#,
            Some("**Found** it"),
            false,
            Theme::default(),
            80,
            &md,
        );
        let text = flatten(&agent);
        assert!(
            text.contains("look") && text.contains("Found it"),
            "{text:?}"
        );
    }

    #[test]
    fn test_expansion_invoke_envelope_renders_the_inner_edit() {
        let direct = render_expansion(
            Some("edit"),
            r#"{"path":"a.rs","oldString":"a","newString":"b"}"#,
            None,
            false,
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let enveloped = render_expansion(
            Some("invoke"),
            r#"{"name":"edit","args":{"path":"a.rs","oldString":"a","newString":"b"}}"#,
            None,
            false,
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        assert_eq!(flatten(&direct), flatten(&enveloped));
    }
}
