use ratatui::{
    style::{Color, Style},
    text::{Line, Span, Text},
};

use crate::tui::diff::DiffRenderer;
use crate::tui::markdown::MarkdownRenderer;
use crate::tui::theme::Theme;
use crate::tui::wrap;

mod expansion;
mod search_output;

pub use expansion::render_write_approval_body;
use search_output::{render_glob_output, render_grep_output};

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
/// `read` → the full path + the file body, `edit` → the full path + a real
/// diff, `write` → the full path + the new content, `apply_patch` → the full
/// path + the patch rendered as a real diff, `bash`/`call` → the full command
/// (+ `workdir`) + its output, `glob`/`grep` → the full pattern/filter, the
/// orchestration tools (`agent`/`ask_user`/`propose_plan`/…) → readable prose
/// instead of raw JSON, and every unknown tool → pretty-printed input followed
/// by the output body. Every arm above must render *something* — an approval
/// preview (called with an empty `output`) must never be left blank (#519).
///
/// `md` renders the plan/task markdown for `propose_plan`/`update_tasks`; it
/// is ignored by the other arms. Wired into the live
/// transcript by `flush_tool_call`'s expanded branch (#340) and into the
/// approval tail by `transcript.rs` (#487/#519).
pub fn render_expansion(
    tool: Option<&str>,
    input: &str,
    output: &str,
    theme: Theme,
    available_width: u16,
    md: &MarkdownRenderer,
) -> Text<'static> {
    match tool {
        Some("read") => expansion::render_read_expansion(input, output, theme, available_width),
        Some("edit") => {
            let v: serde_json::Value =
                serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
            let old = v.get("oldString").and_then(|s| s.as_str()).unwrap_or("");
            let new = v.get("newString").and_then(|s| s.as_str()).unwrap_or("");
            let mut lines = Vec::new();
            lines.extend(expansion::location_line("edit", input, "path"));
            lines.extend(DiffRenderer::render_change(old, new).lines);
            Text::from(lines)
        }
        Some("write") => expansion::render_write_expansion(input),
        Some("apply_patch") => expansion::render_apply_patch_expansion(input),
        Some(t @ ("bash" | "call")) => {
            expansion::render_command_expansion(t, input, output, theme, available_width)
        }
        Some("glob") => expansion::render_glob_expansion(input, output, theme, available_width),
        Some("grep") => expansion::render_grep_expansion(input, output, theme, available_width),
        Some("agent") => {
            let v: serde_json::Value =
                serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
            render_prompt_body(
                v.get("prompt").and_then(|p| p.as_str()).unwrap_or(""),
                available_width,
            )
        }
        Some("poll") => {
            let v: serde_json::Value =
                serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
            render_poll_body(
                v.get("handle").and_then(|s| s.as_str()).unwrap_or(""),
                v.get("timeout_secs").and_then(|t| t.as_u64()),
            )
        }
        Some("agent_send") => {
            let v: serde_json::Value =
                serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
            render_agent_send_body(
                v.get("agent_id").and_then(|s| s.as_str()).unwrap_or(""),
                v.get("prompt").and_then(|p| p.as_str()).unwrap_or(""),
                available_width,
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
            // The approval prompt's `ToolRequest.input` always carries the
            // *resolved* `content` (#513) regardless of whether the model
            // called `content` or `path`; a raw `ToolCall`'s input (rendered
            // in the transcript before resolution) may carry only `path` — no
            // file content to show without a disk read, so name the file
            // instead of leaving the block blank (#519).
            match v.get("content").and_then(|s| s.as_str()) {
                Some(content) => render_markdown_body(md, content, available_width),
                None => {
                    let path = v
                        .get("path")
                        .and_then(|s| s.as_str())
                        .unwrap_or("(unknown)");
                    render_markdown_body(md, &format!("_plan file: `{path}`_"), available_width)
                }
            }
        }
        Some("update_tasks") => {
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

/// Wrap and indent a multi-line plain-text body (e.g. an `agent` `prompt`).
/// Word-wraps at `available_width - 4` so long prompts don't overflow
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

/// A compact `handle` + `timeout_secs` summary for a `poll` body.
fn render_poll_body(handle: &str, timeout_secs: Option<u64>) -> Text<'static> {
    let mut lines = Vec::new();
    lines.push(Line::from(format!("  handle: {handle}")));
    if let Some(t) = timeout_secs {
        lines.push(Line::from(format!("  timeout_secs: {t}")));
    }
    Text::from(lines)
}

/// An `agent_id` line followed by the prompt body — the `agent_send` (#609)
/// counterpart of `render_prompt_body`, naming which sub-agent the follow-up
/// prompt is going to.
fn render_agent_send_body(agent_id: &str, prompt: &str, available_width: u16) -> Text<'static> {
    let mut lines = vec![Line::from(format!("  agent_id: {agent_id}"))];
    lines.extend(render_prompt_body(prompt, available_width).lines);
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
/// applied uniformly, which is all these orchestration bodies need). Also
/// used by `expansion`'s `render_wrapped_labeled` for the same reason.
pub(super) fn collect_line(line: &Line<'_>) -> String {
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
/// Also used by `expansion::render_read_expansion` for the same reason.
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
            r##"{"content":"# Goal\nDo X","path":".entanglement/plans/s1.md"}"##,
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
            "",
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
            "",
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
    fn test_expansion_agent_send_renders_agent_id_and_prompt() {
        let result = render_expansion(
            Some("agent_send"),
            r#"{"agent_id":"s-abc123","prompt":"focus on Y instead"}"#,
            "",
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
            "",
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
}
