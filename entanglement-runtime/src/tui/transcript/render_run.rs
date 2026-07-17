use crate::tui::wrap;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use std::borrow::Cow;
use unicode_width::UnicodeWidthStr;

use crate::tui::markdown::MarkdownRenderer;
use crate::tui::theme::{RoleColors, Theme};
use crate::tui::tool_render;

/// Reforge a `Line` whose spans the markdown renderer produced under `&self`'s
/// lifetime into an owned `Line<'static>`. Every span's content is already an
/// owned `String` behind the `Cow` (see [`MarkdownRenderer::render`]), so
/// `into_owned()` is a move, not a copy — this only relabels the lifetime so the
/// assembled line can live in the render cache ([`super::cache`]).
pub(super) fn to_static(line: Line<'_>) -> Line<'static> {
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|s| Span {
            style: s.style,
            content: Cow::Owned(s.content.into_owned()),
        })
        .collect();
    let mut out = Line::from(spans);
    out.style = line.style;
    out.alignment = line.alignment;
    out
}

/// Whether a markdown-rendered line is output from a fenced code block (and so
/// must not be word-wrapped). Heuristic: syntect styles *every* span it emits
/// with a foreground color, while prose — even prose containing inline `code`,
/// which adds one styled span — leaves most spans unstyled (`Span::raw`). A line
/// counts as code only when all of its non-empty spans carry an `fg`. Empty
/// spans (the raw spaces between words) are ignored so padding doesn't mislabel
/// a prose line. Tables are detected separately by their leading `|`.
fn is_code_block_line(line: &Line<'_>) -> bool {
    if line.spans.len() < 2 {
        return false;
    }
    line.spans
        .iter()
        .filter(|s| !s.content.is_empty())
        .all(|s| s.style.fg.is_some())
}

fn render_text_run(
    md: &MarkdownRenderer,
    run: &str,
    theme: Theme,
    colors: RoleColors,
    available_width: u16,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if run.trim().is_empty() {
        return out;
    }
    let rendered = md.render(run);
    for line in rendered.lines {
        let line = to_static(line);
        // Everything wraps to the panel width — tables, fenced code blocks,
        // and prose alike (#wrap). No horizontal scroll survives.
        for wline in wrap::wrap_line(line, available_width.saturating_sub(4)) {
            out.push(theme.decorate(wline, colors, available_width));
        }
    }
    out
}

fn render_reasoning_run(
    md: &MarkdownRenderer,
    run: &str,
    theme: Theme,
    colors: RoleColors,
    available_width: u16,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if run.trim().is_empty() {
        return out;
    }
    let rendered = md.render(run);
    for line in rendered.lines {
        let line = to_static(line);
        let is_table = line
            .spans
            .first()
            .map(|s| s.content.as_ref().starts_with('|'))
            .unwrap_or(false);

        let is_code = is_code_block_line(&line);

        // Italicize prose spans; code/table lines keep the syntect styling the
        // renderer already applied.
        let styled_line = if is_table || is_code {
            line
        } else {
            let styled_spans: Vec<Span<'static>> = line
                .spans
                .iter()
                .map(|s| {
                    Span::styled(
                        s.content.clone().into_owned(),
                        Style::default().fg(colors.fg).italic(),
                    )
                })
                .collect();
            Line::from(styled_spans)
        };

        // Everything wraps to the panel width — tables, fenced code blocks,
        // and prose alike (#wrap). No horizontal scroll survives.
        for wline in wrap::wrap_line(styled_line, available_width.saturating_sub(4)) {
            out.push(theme.decorate(wline, colors, available_width));
        }
    }
    out
}

/// The owned colored left-bar padding row that brackets a message block.
pub(super) fn padding_line(colors: RoleColors, available_width: u16) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "▌".to_string(),
            Style::default().fg(colors.fg).bg(colors.bg),
        ),
        Span::raw(" ".repeat(available_width.saturating_sub(1) as usize)),
    ])
}

/// Renders a coalesced assistant text run, optionally wrapped in the
/// colored left-bar padding. Streaming (uncommitted) trailing text renders
/// without padding; a run committed by a following entry gets the bar.
pub(super) fn flush_text(
    md: &MarkdownRenderer,
    run: &str,
    theme: Theme,
    colors: RoleColors,
    available_width: u16,
    with_padding: bool,
) -> Vec<Line<'static>> {
    let body = render_text_run(md, run, theme, colors, available_width);
    if with_padding {
        let padding = padding_line(colors, available_width);
        let mut out = Vec::with_capacity(body.len() + 2);
        out.push(padding.clone());
        out.extend(body);
        out.push(padding);
        out
    } else {
        body
    }
}

/// Renders a coalesced reasoning run as a collapsible block: a one-line
/// `▸ Thinking (N lines)` header (collapsed, the default) or `▾ …` plus the
/// italic body (expanded). The whole block maps to one clickable region; its
/// stable id is resolved by the caller ([`super::segment`]).
pub(super) fn flush_reasoning(
    md: &MarkdownRenderer,
    run: &str,
    theme: Theme,
    colors: RoleColors,
    available_width: u16,
    expanded: bool,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if run.trim().is_empty() {
        return out;
    }
    let source_lines = run.lines().filter(|l| !l.trim().is_empty()).count().max(1);
    let padding = padding_line(colors, available_width);
    out.push(padding.clone());

    let arrow = if expanded { "▾" } else { "▸" };
    let header = Line::from(vec![Span::styled(
        format!("{arrow} Thinking ({source_lines} lines)"),
        Style::default().fg(colors.fg).italic(),
    )]);
    out.push(theme.decorate(header, colors, available_width));

    if expanded {
        out.extend(render_reasoning_run(
            md,
            run,
            theme,
            colors,
            available_width,
        ));
    }

    out.push(padding);
    out
}

/// The primary argument shown on a tool op's collapsed header: the path for
/// `read`/`edit`/`write`, the command line for `bash`/`call` (via the runtime's
/// [`permission_arg`][entanglement_runtime::permission::permission_arg]), or the
/// `pattern` for `glob`/`grep` (a local fallback, since `permission_arg` returns
/// `None` for those). `None` when nothing informative is available.
///
/// Orchestration tools (`agent`, `ask_user`, `propose_plan`, …) fall through to
/// [`orchestration_primary_arg`], which pulls a readable hint from their JSON
/// input so the header isn't a bare tool name while a call is in flight.
fn tool_primary_arg(tool: &str, input: &str) -> Option<String> {
    if let Some(arg) = entanglement_runtime::permission::permission_arg(tool, input) {
        return Some(arg);
    }
    let value: serde_json::Value = serde_json::from_str(input).ok()?;
    if let Some(arg) = orchestration_primary_arg(tool, &value) {
        return Some(arg);
    }
    value.get("pattern")?.as_str().map(String::from)
}

/// A readable collapsed-header hint for the orchestration tools, whose inputs
/// carry no file path or shell command (so [`permission_arg`] ignores them). The
/// shapes are confirmed in source (#89/#90/#120/#124/#140/#141):
/// `agent`/`agent_spawn` → the agent target (+ a truncated prompt);
/// `agent_poll` → the `agent_id`; `ask_user` → a truncated `question`;
/// `propose_plan` → `"plan"`; `update_plan`/`update_tasks` → `"snapshot"`;
/// `load_skill` → the `skill_name`. Returns `None` for every other tool or on
/// malformed input, so the header falls back to the bare tool name.
fn orchestration_primary_arg(tool: &str, value: &serde_json::Value) -> Option<String> {
    match tool {
        "agent" | "agent_spawn" => {
            let agent = value.get("agent")?.as_str()?;
            if let Some(prompt) = value.get("prompt").and_then(|p| p.as_str()) {
                Some(format!("{agent}  {}", truncate_to_width(prompt, 40)))
            } else {
                Some(agent.to_string())
            }
        }
        "agent_poll" => value.get("agent_id")?.as_str().map(String::from),
        "ask_user" => value
            .get("question")?
            .as_str()
            .map(|q| truncate_to_width(q, 40)),
        "propose_plan" => Some("plan".to_string()),
        "update_plan" | "update_tasks" => Some("snapshot".to_string()),
        "load_skill" => value.get("skill_name")?.as_str().map(String::from),
        _ => None,
    }
}

/// Renders one tool op as a collapsible block, mirroring [`flush_reasoning`]:
/// a one-line `{arrow} {tool}  {primary_arg}  {status}` header (collapsed, the
/// default) plus — when expanded — the call args and its output. The whole block
/// maps to one clickable region so a click toggles it (#340).
#[allow(clippy::too_many_arguments)]
pub(super) fn flush_tool_call(
    md: &MarkdownRenderer,
    tool: &str,
    input: &str,
    output: Option<&str>,
    theme: Theme,
    colors: RoleColors,
    available_width: u16,
    expanded: bool,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let padding = padding_line(colors, available_width);
    out.push(padding.clone());

    let arrow = if expanded { "▾" } else { "▸" };
    let mut header = vec![
        Span::styled(format!("{arrow} "), Style::default().fg(colors.fg)),
        Span::styled(tool.to_string(), Style::default().fg(Color::Cyan).bold()),
    ];
    if let Some(arg) = tool_primary_arg(tool, input) {
        // Budget the arg so the header stays on one line: strip the bar (2),
        // arrow (2), tool name, the two-space gaps, and the trailing status.
        let status_w = if output.is_some() { 2 } else { 0 };
        let fixed = 2 + 2 + UnicodeWidthStr::width(tool) + 2 + status_w;
        let budget = (available_width as usize).saturating_sub(fixed);
        header.push(Span::raw("  "));
        header.push(Span::styled(
            truncate_to_width(&arg, budget),
            Style::default().fg(colors.fg),
        ));
    }
    if output.is_some() {
        header.push(Span::styled(" ✓", Style::default().fg(Color::Green)));
    }
    out.push(theme.decorate(Line::from(header), colors, available_width));

    if expanded {
        // The expanded body means something per tool (#341): `read` → the file
        // body, `edit` → a `+`/`-` diff, `write` → the new content, `bash`/`call`
        // → the command output, everything else → pretty-printed input + output.
        // The primary arg already lives in the header, so it is never re-dumped.
        let rendered = tool_render::render_expansion(
            Some(tool),
            input,
            output.unwrap_or(""),
            theme,
            available_width,
            md,
        );
        for line in rendered.lines {
            out.push(theme.decorate(line, colors, available_width));
        }
    }

    out.push(padding);
    out
}

/// Truncates `s` to `max` display columns, appending `…` when it had to cut.
fn truncate_to_width(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut width = 0;
    for ch in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w > max.saturating_sub(1) {
            break;
        }
        out.push(ch);
        width += w;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_tools_keep_their_existing_primary_arg() {
        // `permission_arg` still drives these; the orchestration fallback must
        // not intercept them.
        assert_eq!(
            tool_primary_arg("read", r#"{"path":"src/main.rs"}"#).as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            tool_primary_arg("bash", r#"{"command":"git status"}"#).as_deref(),
            Some("git status")
        );
        assert_eq!(
            tool_primary_arg("glob", r#"{"pattern":"**/*.rs"}"#).as_deref(),
            Some("**/*.rs")
        );
    }

    #[test]
    fn agent_spawn_returns_target_and_truncated_prompt() {
        let arg = tool_primary_arg(
            "agent_spawn",
            r#"{"agent":"explore","prompt":"find the thing"}"#,
        )
        .expect("agent_spawn should yield a primary arg");
        assert!(
            arg.contains("explore"),
            "agent_spawn header must name the target: {arg:?}"
        );
        assert!(
            arg.contains("find the thing"),
            "agent_spawn header should surface the prompt: {arg:?}"
        );
    }

    #[test]
    fn agent_spawn_long_prompt_is_truncated() {
        let long_prompt = "x".repeat(80);
        let input = format!(r#"{{"agent":"explore","prompt":"{long_prompt}"}}"#);
        let arg = tool_primary_arg("agent_spawn", &input).expect("primary arg");
        // The prompt portion is budgeted to 40 display columns + ellipsis.
        assert!(arg.contains("explore"));
        assert!(arg.contains('…'), "long prompt must be truncated: {arg:?}");
    }

    #[test]
    fn agent_poll_returns_the_agent_id() {
        assert_eq!(
            tool_primary_arg("agent_poll", r#"{"agent_id":"abc123"}"#).as_deref(),
            Some("abc123")
        );
    }

    #[test]
    fn ask_user_returns_truncated_question() {
        let arg = tool_primary_arg("ask_user", r#"{"question":"Which option do you prefer?"}"#)
            .expect("ask_user should yield a primary arg");
        assert!(
            arg.contains("Which option"),
            "ask_user header should surface the question: {arg:?}"
        );
    }

    #[test]
    fn propose_plan_returns_label() {
        assert_eq!(
            tool_primary_arg("propose_plan", r##"{"plan":"# Goal"}"##).as_deref(),
            Some("plan")
        );
    }

    #[test]
    fn update_plan_and_tasks_return_snapshot_label() {
        assert_eq!(
            tool_primary_arg("update_plan", r##"{"content":"# Step 1"}"##).as_deref(),
            Some("snapshot")
        );
        assert_eq!(
            tool_primary_arg("update_tasks", r#"{"content":"- [ ] a"}"#).as_deref(),
            Some("snapshot")
        );
    }

    #[test]
    fn load_skill_returns_the_skill_name() {
        assert_eq!(
            tool_primary_arg("load_skill", r#"{"skill_name":"arch"}"#).as_deref(),
            Some("arch")
        );
    }

    #[test]
    fn unknown_tool_with_no_arg_yields_none() {
        assert_eq!(tool_primary_arg("mystery", r#"{"x":1}"#), None);
    }

    #[test]
    fn malformed_input_yields_none() {
        assert_eq!(tool_primary_arg("ask_user", "not json"), None);
    }
}
