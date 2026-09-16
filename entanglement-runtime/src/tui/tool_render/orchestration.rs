//! Input bodies for the orchestration tools — `agent`, `agent_send`, `poll`,
//! `ask_user`, `propose_plan`, `update_tasks`, `load_skill` — as readable
//! prose instead of raw JSON. Split out of `tool_render.rs` (400-line cap).

use ratatui::text::{Line, Text};
use serde_json::Value;

use crate::tui::markdown::MarkdownRenderer;
use crate::tui::wrap;

use super::{collect_line, parse_input};

pub(super) fn render_input(
    tool: &str,
    input: &str,
    available_width: u16,
    md: &MarkdownRenderer,
) -> Vec<Line<'static>> {
    let v = parse_input(input);
    let text = match tool {
        "agent" => render_prompt_body(str_field(&v, "prompt"), available_width),
        "agent_send" => render_agent_send_body(
            str_field(&v, "agent_id"),
            str_field(&v, "prompt"),
            available_width,
        ),
        "poll" => render_poll_body(
            str_field(&v, "handle"),
            v.get("timeout_secs").and_then(Value::as_u64),
        ),
        "ask_user" => render_ask_user_body(&v),
        // The approval prompt's `ToolRequest.input` always carries the
        // *resolved* `content` (#513) regardless of whether the model called
        // `content` or `path`; a raw `ToolCall`'s input (rendered in the
        // transcript before resolution) may carry only `path` — no file
        // content to show without a disk read, so name the file instead of
        // leaving the block blank (#519).
        "propose_plan" => match v.get("content").and_then(Value::as_str) {
            Some(content) => render_markdown_body(md, content, available_width),
            None => {
                let path = v.get("path").and_then(Value::as_str).unwrap_or("(unknown)");
                render_markdown_body(md, &format!("_plan file: `{path}`_"), available_width)
            }
        },
        "update_tasks" => render_markdown_body(md, str_field(&v, "content"), available_width),
        "load_skill" => Text::from(vec![Line::from(format!(
            "  {}",
            str_field(&v, "skill_name")
        ))]),
        _ => Text::default(),
    };
    text.lines
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
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
    let mut lines = vec![Line::from(format!("  handle: {handle}"))];
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
fn render_ask_user_body(value: &Value) -> Text<'static> {
    let mut lines = Vec::new();
    let questions = value
        .get("questions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| vec![value.clone()]);
    for question in &questions {
        if let Some(q) = question.get("question").and_then(Value::as_str) {
            lines.push(Line::from(format!("  {q}")));
        }
        if let Some(options) = question.get("options").and_then(Value::as_array) {
            for (i, opt) in options.iter().enumerate() {
                if let Some(label) = opt.get("label").and_then(Value::as_str) {
                    lines.push(Line::from(format!("  {}. {label}", i + 1)));
                }
            }
        }
        if question
            .get("multi_select")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            lines.push(Line::from("  (multiple selections allowed)"));
        }
    }
    Text::from(lines)
}

/// Render a markdown body (a plan, a task snapshot, a sub-agent's reply) via
/// the shared [`MarkdownRenderer`], word-wrapping each rendered line at
/// `available_width - 4` so long paragraphs don't overflow — mirroring how
/// assistant text runs are wrapped (`render_text_run`).
pub(super) fn render_markdown_body(
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
