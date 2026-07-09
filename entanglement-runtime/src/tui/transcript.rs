use crate::tui::wrap;
use entanglement_core::TaskStatus;
use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};

use crate::tui::app::App;
use crate::tui::markdown::MarkdownRenderer;
use crate::tui::session_view::{ApprovalMode, TranscriptEntry};
use crate::tui::theme::{RoleColors, Theme};
use crate::tui::tool_render;
/// Rendered transcript plus per-line provenance. `line_blocks[i]` holds the
/// reasoning-run id (transcript index of the run's first `ReasoningDelta`) that
/// produced rendered line `i`, or `None` for lines that aren't part of a
/// clickable block. Click hit-testing (`App::reasoning_block_at`) maps a
/// `row + scroll_offset` back to a block through this vector.
pub(crate) struct RenderedBody<'a> {
    pub lines: Vec<Line<'a>>,
    pub line_blocks: Vec<Option<usize>>,
}

pub(crate) fn render_body_lines<'a>(app: &'a App, available_width: u16) -> RenderedBody<'a> {
    let mut lines = Vec::new();
    // (block_id, start_line, end_line_exclusive) for each rendered reasoning run.
    let mut regions: Vec<(usize, usize, usize)> = Vec::new();
    let markdown_renderer = app.markdown_renderer();
    let theme = app.theme();
    let user = theme.user_colors(app.profile_color_for(app.agent()));

    if let Some(plan) = app.plan() {
        lines.push(Line::from(""));
        lines.push(Line::from("Plan:").bold());
        let rendered_plan = markdown_renderer.render(plan);
        for line in rendered_plan.lines {
            lines.push(line);
        }
        lines.push(Line::from(""));
    }

    if let Some(tasks) = app.task_list() {
        lines.push(Line::from("Tasks:").bold());
        for task in tasks {
            let symbol = match task.status {
                TaskStatus::Pending => "○",
                TaskStatus::InProgress => "▶",
                TaskStatus::Completed => "✓",
                TaskStatus::Cancelled => "✗",
            };
            lines.push(Line::from(format!("  {} {}", symbol, task.content)));
        }
        lines.push(Line::from(""));
    }

    append_transcript(
        &mut lines,
        &mut regions,
        markdown_renderer,
        app,
        theme,
        user,
        available_width,
    );

    if let ApprovalMode::WaitingForApproval { .. } = app.approval_mode() {
        if let Some((_, tool, input)) = app.pending_tool_request() {
            lines.push(Line::from(""));
            lines.push(Line::from("─".repeat(60)).fg(Color::Yellow));
            lines.push(Line::from(vec![
                Span::styled("?", Style::default().fg(Color::Yellow).bold()),
                Span::raw(" "),
                Span::styled(tool, Style::default().fg(Color::Cyan).bold()),
            ]));

            if let Ok(json) = serde_json::from_str::<serde_json::Value>(input) {
                if let Ok(pretty) = serde_json::to_string_pretty(&json) {
                    for line in pretty.lines() {
                        lines.push(Line::from(format!("  {line}")));
                    }
                } else {
                    for line in input.lines() {
                        lines.push(Line::from(format!("  {line}")));
                    }
                }
            } else {
                for line in input.lines() {
                    lines.push(Line::from(format!("  {line}")));
                }
            }

            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("[y]", Style::default().fg(Color::Green).bold()),
                Span::raw(" approve  "),
                Span::styled("[n]", Style::default().fg(Color::Red).bold()),
                Span::raw(" reject  "),
                Span::styled("[e]", Style::default().fg(Color::Yellow).bold()),
                Span::raw(" edit reason  "),
                Span::styled("[Esc]", Style::default().fg(Color::Gray).bold()),
                Span::raw(" interrupt"),
            ]));
            lines.push(Line::from("─".repeat(60)).fg(Color::Yellow));
        }
    }

    if let Some(q) = app.pending_question() {
        render_question(&mut lines, q, &app.input_text());
    }

    let mut line_blocks = vec![None; lines.len()];
    for (id, start, end) in regions {
        for slot in line_blocks.iter_mut().take(end).skip(start) {
            *slot = Some(id);
        }
    }

    RenderedBody { lines, line_blocks }
}

/// Render a pending `ask_user` question (ADR-0027) Claude-style: the prompt, a
/// numbered list of labelled choices with the highlighted one marked, an
/// optional "Other" free-text entry (showing the typed answer while active),
/// and a key hint footer.
fn render_question<'a>(
    lines: &mut Vec<Line<'a>>,
    q: &crate::tui::session_view::PendingQuestion,
    input_text: &str,
) {
    let accent = Color::Cyan;
    lines.push(Line::from(""));
    lines.push(Line::from("─".repeat(60)).fg(accent));
    lines.push(Line::from(vec![
        Span::styled("?", Style::default().fg(accent).bold()),
        Span::raw(" "),
        Span::styled(q.question.clone(), Style::default().bold()),
    ]));
    lines.push(Line::from(""));

    let selecting = !q.entering_free_form;
    for (i, opt) in q.options.iter().enumerate() {
        let picked = selecting && i == q.selected;
        push_choice(lines, i + 1, picked, &opt.label);
        if let Some(desc) = &opt.description {
            lines.push(Line::from(Span::styled(
                format!("      {desc}"),
                Style::default().dim(),
            )));
        }
    }

    if q.allow_free_form {
        let idx = q.options.len();
        let picked = q.selected == idx;
        let marker = if picked { "❯" } else { " " };
        let style = if picked {
            Style::default().fg(accent).bold()
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker} {}. ", idx + 1), style),
            Span::styled("Other", style),
            Span::styled(" (type a custom answer)", Style::default().dim()),
        ]));
        if q.entering_free_form {
            let shown = if input_text.is_empty() {
                "…".to_string()
            } else {
                input_text.to_string()
            };
            lines.push(Line::from(vec![
                Span::raw("      › "),
                Span::styled(shown, Style::default().fg(Color::White)),
            ]));
        }
    }

    lines.push(Line::from(""));
    let footer = if q.entering_free_form {
        vec![
            Span::styled("[Enter]", Style::default().fg(Color::Green).bold()),
            Span::raw(" submit  "),
            Span::styled("[Esc]", Style::default().fg(Color::Gray).bold()),
            Span::raw(" back"),
        ]
    } else {
        vec![
            Span::styled("[↑/↓]", Style::default().fg(accent).bold()),
            Span::raw(" select  "),
            Span::styled("[1-9]", Style::default().fg(accent).bold()),
            Span::raw(" pick  "),
            Span::styled("[Enter]", Style::default().fg(Color::Green).bold()),
            Span::raw(" choose  "),
            Span::styled("[Esc]", Style::default().fg(Color::Gray).bold()),
            Span::raw(" interrupt"),
        ]
    };
    lines.push(Line::from(footer));
    lines.push(Line::from("─".repeat(60)).fg(accent));
}

/// One numbered choice row, highlighted when `picked`.
fn push_choice<'a>(lines: &mut Vec<Line<'a>>, num: usize, picked: bool, label: &str) {
    let (marker, style) = if picked {
        ("❯", Style::default().fg(Color::Cyan).bold())
    } else {
        (" ", Style::default())
    };
    lines.push(Line::from(vec![
        Span::styled(format!(" {marker} {num}. "), style),
        Span::styled(label.to_string(), style),
    ]));
}

/// Append the transcript entries. Consecutive `TextDelta`s are streamed
/// token-by-token by the engine, so they're coalesced into one string before
/// markdown rendering — rendering each delta on its own would give every chunk
/// its own hard line break, wrecking word wrap.
fn append_transcript<'a>(
    lines: &mut Vec<Line<'a>>,
    regions: &mut Vec<(usize, usize, usize)>,
    markdown_renderer: &'a MarkdownRenderer,
    app: &'a App,
    theme: Theme,
    user: RoleColors,
    available_width: u16,
) {
    fn render_text_run<'a>(
        lines: &mut Vec<Line<'a>>,
        markdown_renderer: &'a MarkdownRenderer,
        run: &str,
        theme: Theme,
        colors: RoleColors,
        available_width: u16,
    ) {
        if run.trim().is_empty() {
            return;
        }
        let rendered = markdown_renderer.render(run);
        for line in rendered.lines {
            let is_table = line
                .spans
                .first()
                .map(|s| s.content.as_ref().starts_with('|'))
                .unwrap_or(false);

            let is_code =
                line.spans.len() > 1 && line.spans.iter().skip(1).any(|s| s.style.fg.is_some());

            if is_table || is_code {
                let decorated = theme.decorate(line, colors, available_width);
                lines.push(decorated);
            } else {
                let wrapped = wrap::wrap_line(line, available_width.saturating_sub(4));
                for wline in wrapped {
                    let decorated = theme.decorate(wline, colors, available_width);
                    lines.push(decorated);
                }
            }
        }
    }

    fn render_reasoning_run<'a>(
        lines: &mut Vec<Line<'a>>,
        markdown_renderer: &'a MarkdownRenderer,
        run: &str,
        theme: Theme,
        colors: RoleColors,
        available_width: u16,
    ) {
        if run.trim().is_empty() {
            return;
        }
        let rendered = markdown_renderer.render(run);
        for line in rendered.lines {
            let is_table = line
                .spans
                .first()
                .map(|s| s.content.as_ref().starts_with('|'))
                .unwrap_or(false);

            let is_code =
                line.spans.len() > 1 && line.spans.iter().skip(1).any(|s| s.style.fg.is_some());

            let styled_line = if is_table || is_code {
                line
            } else {
                let styled_spans: Vec<Span> = line
                    .spans
                    .iter()
                    .map(|s| {
                        Span::styled(s.content.clone(), Style::default().fg(colors.fg).italic())
                    })
                    .collect();
                Line::from(styled_spans)
            };

            if is_table || is_code {
                let decorated = theme.decorate(styled_line, colors, available_width);
                lines.push(decorated);
            } else {
                let wrapped = wrap::wrap_line(styled_line, available_width.saturating_sub(4));
                for wline in wrapped {
                    let decorated = theme.decorate(wline, colors, available_width);
                    lines.push(decorated);
                }
            }
        }
    }

    /// Renders a coalesced assistant text run, optionally wrapped in the
    /// colored left-bar padding. Streaming (uncommitted) trailing text renders
    /// without padding; a run committed by a following entry gets the bar.
    fn flush_text<'a>(
        lines: &mut Vec<Line<'a>>,
        markdown_renderer: &'a MarkdownRenderer,
        run: &mut String,
        theme: Theme,
        colors: RoleColors,
        available_width: u16,
        with_padding: bool,
    ) {
        if with_padding {
            let padding = Line::from(vec![
                Span::styled("▌", Style::default().fg(colors.fg).bg(colors.bg)),
                Span::raw(" ".repeat(available_width.saturating_sub(1) as usize)),
            ]);
            lines.push(padding.clone());
            render_text_run(
                lines,
                markdown_renderer,
                run,
                theme,
                colors,
                available_width,
            );
            lines.push(padding);
        } else {
            render_text_run(
                lines,
                markdown_renderer,
                run,
                theme,
                colors,
                available_width,
            );
        }
        run.clear();
    }

    /// Renders a coalesced reasoning run as a collapsible block: a one-line
    /// `▸ Thinking (N lines)` header (collapsed, the default) or `▾ …` plus the
    /// italic body (expanded). Records the rendered line range under `block_id`
    /// so a click anywhere in the block toggles it.
    #[allow(clippy::too_many_arguments)]
    fn flush_reasoning<'a>(
        lines: &mut Vec<Line<'a>>,
        regions: &mut Vec<(usize, usize, usize)>,
        markdown_renderer: &'a MarkdownRenderer,
        run: &str,
        theme: Theme,
        colors: RoleColors,
        available_width: u16,
        block_id: usize,
        expanded: bool,
    ) {
        if run.trim().is_empty() {
            return;
        }
        let start = lines.len();
        let source_lines = run.lines().filter(|l| !l.trim().is_empty()).count().max(1);
        let padding = Line::from(vec![
            Span::styled("▌", Style::default().fg(colors.fg).bg(colors.bg)),
            Span::raw(" ".repeat(available_width.saturating_sub(1) as usize)),
        ]);
        lines.push(padding.clone());

        let arrow = if expanded { "▾" } else { "▸" };
        let header = Line::from(vec![Span::styled(
            format!("{arrow} Thinking ({source_lines} lines)"),
            Style::default().fg(colors.fg).italic(),
        )]);
        lines.push(theme.decorate(header, colors, available_width));

        if expanded {
            render_reasoning_run(
                lines,
                markdown_renderer,
                run,
                theme,
                colors,
                available_width,
            );
        }

        lines.push(padding);
        regions.push((block_id, start, lines.len()));
    }

    let assistant = theme.assistant_colors();
    let reasoning = theme.reasoning_colors();
    let tool_req = theme.tool_req_colors();
    let tool_out = theme.tool_out_colors();
    let error = theme.error_colors();

    let mut pending_text = String::new();
    let mut pending_reasoning = String::new();
    // Transcript index of the first `ReasoningDelta` in the current run — its
    // stable click id, resolved once per coalesced run.
    let mut reasoning_start: Option<usize> = None;
    for (idx, entry) in app.transcript().iter().enumerate() {
        if let TranscriptEntry::TextDelta { text } = entry {
            // Switching text→reasoning would break arrival order, so commit any
            // reasoning run in progress before this text run starts.
            if !pending_reasoning.is_empty() {
                let block_id = reasoning_start.take().unwrap_or(idx);
                flush_reasoning(
                    lines,
                    regions,
                    markdown_renderer,
                    &pending_reasoning,
                    theme,
                    reasoning,
                    available_width,
                    block_id,
                    app.reasoning_expanded(block_id),
                );
                pending_reasoning.clear();
            }
            pending_text.push_str(text);
            continue;
        }
        if let TranscriptEntry::ReasoningDelta { text } = entry {
            // Symmetric flush: commit the text run before the reasoning run so a
            // thinking block that arrives after text renders after it.
            if !pending_text.is_empty() {
                flush_text(
                    lines,
                    markdown_renderer,
                    &mut pending_text,
                    theme,
                    assistant,
                    available_width,
                    true,
                );
            }
            if pending_reasoning.is_empty() {
                reasoning_start = Some(idx);
            }
            pending_reasoning.push_str(text);
            continue;
        }
        // Flush-on-switch keeps the two accumulators mutually exclusive, so at a
        // non-delta boundary at most one is non-empty — flush order is immaterial.
        if !pending_text.is_empty() {
            flush_text(
                lines,
                markdown_renderer,
                &mut pending_text,
                theme,
                assistant,
                available_width,
                true,
            );
        }
        if !pending_reasoning.is_empty() {
            let block_id = reasoning_start.take().unwrap_or(idx);
            flush_reasoning(
                lines,
                regions,
                markdown_renderer,
                &pending_reasoning,
                theme,
                reasoning,
                available_width,
                block_id,
                app.reasoning_expanded(block_id),
            );
            pending_reasoning.clear();
        }

        match entry {
            TranscriptEntry::TextDelta { .. } | TranscriptEntry::ReasoningDelta { .. } => {
                unreachable!()
            }
            TranscriptEntry::User { text, pending } => {
                let padding = Line::from(vec![
                    Span::styled("▌", Style::default().fg(user.fg).bg(user.bg)),
                    Span::raw(" ".repeat(available_width.saturating_sub(1) as usize)),
                ]);
                lines.push(padding.clone());
                for line in text.lines() {
                    let user_line = Line::from(vec![Span::styled(
                        line.to_string(),
                        if *pending {
                            Style::default().fg(user.fg).dim()
                        } else {
                            Style::default().fg(user.fg)
                        },
                    )]);
                    let wrapped = wrap::wrap_line(user_line, available_width.saturating_sub(4));
                    for wline in wrapped {
                        lines.push(theme.decorate(wline, user, available_width));
                    }
                }
                lines.push(padding);
            }
            TranscriptEntry::ToolCall { tool, input, .. } => {
                let padding = Line::from(vec![
                    Span::styled("▌", Style::default().fg(tool_req.fg).bg(tool_req.bg)),
                    Span::raw(" ".repeat(available_width.saturating_sub(1) as usize)),
                ]);
                lines.push(padding.clone());
                let request_line = Line::from(vec![
                    Span::styled("Tool Call: ", Style::default().fg(Color::Cyan)),
                    Span::styled(tool, Style::default().bold()),
                ]);
                let wrapped = wrap::wrap_line(request_line, available_width.saturating_sub(4));
                for wline in wrapped {
                    lines.push(theme.decorate(wline, tool_req, available_width));
                }
                for line in input.lines() {
                    let content_line = Line::from(format!("  {line}"));
                    let wrapped = wrap::wrap_line(content_line, available_width.saturating_sub(4));
                    for wline in wrapped {
                        lines.push(theme.decorate(wline, tool_req, available_width));
                    }
                }
                lines.push(padding);
            }
            TranscriptEntry::ToolOutput { tool, output } => {
                let padding = Line::from(vec![
                    Span::styled("▌", Style::default().fg(tool_out.fg).bg(tool_out.bg)),
                    Span::raw(" ".repeat(available_width.saturating_sub(1) as usize)),
                ]);
                lines.push(padding.clone());
                let header_text = if let Some(tool_name) = tool {
                    format!("Tool Output ({tool_name}):")
                } else {
                    "Tool Output:".to_string()
                };
                let output_header = Line::from(header_text);
                let wrapped = wrap::wrap_line(output_header, available_width.saturating_sub(4));
                for wline in wrapped {
                    lines.push(theme.decorate(wline, tool_out, available_width));
                }

                let rendered = tool_render::render_tool_output(
                    tool.as_deref(),
                    output,
                    theme,
                    available_width,
                );
                for line in rendered.lines {
                    lines.push(theme.decorate(line, tool_out, available_width));
                }
                lines.push(padding);
            }
            TranscriptEntry::Error { message } => {
                let padding = Line::from(vec![
                    Span::styled("▌", Style::default().fg(error.fg).bg(error.bg)),
                    Span::raw(" ".repeat(available_width.saturating_sub(1) as usize)),
                ]);
                lines.push(padding.clone());
                let error_line = Line::from(vec![
                    Span::styled("Error: ", Style::default().fg(Color::Red).bold()),
                    Span::styled(message, Style::default().fg(Color::Red)),
                ]);
                let wrapped = wrap::wrap_line(error_line, available_width.saturating_sub(4));
                for wline in wrapped {
                    lines.push(theme.decorate(wline, error, available_width));
                }
                lines.push(padding);
            }
            TranscriptEntry::Done => {
                lines.push(Line::from(""));
            }
        }
    }
    // End of stream: at most one accumulator survives (flush-on-switch). The
    // trailing text run is still being streamed, so it renders bar-less.
    if !pending_text.is_empty() {
        flush_text(
            lines,
            markdown_renderer,
            &mut pending_text,
            theme,
            assistant,
            available_width,
            false,
        );
    }
    if !pending_reasoning.is_empty() {
        let block_id = reasoning_start.unwrap_or(app.transcript().len());
        flush_reasoning(
            lines,
            regions,
            markdown_renderer,
            &pending_reasoning,
            theme,
            reasoning,
            available_width,
            block_id,
            app.reasoning_expanded(block_id),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::App;
    use crate::tui::theme::hash_profile_color;
    use entanglement_core::{OutEvent, SessionId};

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn feed_reasoning(app: &mut App, sid: &SessionId, body: &str) {
        for (i, chunk) in body.split_inclusive('\n').enumerate() {
            app.handle_out_event(OutEvent::ReasoningDelta {
                session: sid.clone(),
                seq: i as u64 + 1,
                text: chunk.to_string(),
            });
        }
    }

    /// Index of the first rendered line whose text contains `needle`.
    fn line_index_of(body: &RenderedBody, needle: &str) -> usize {
        body.lines
            .iter()
            .position(|l| line_text(l).contains(needle))
            .unwrap_or_else(|| panic!("no rendered line contains {needle:?}"))
    }

    #[test]
    fn text_then_reasoning_renders_in_arrival_order() {
        // A turn that streams assistant text and *then* a thinking block must
        // render the thinking header after the text, not before it (#88).
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        app.handle_out_event(OutEvent::TextDelta {
            session: sid.clone(),
            seq: 1,
            text: "ANSWERTEXT\n".to_string(),
        });
        app.handle_out_event(OutEvent::ReasoningDelta {
            session: sid.clone(),
            seq: 2,
            text: "afterthought\n".to_string(),
        });

        let body = render_body_lines(&app, 80);
        let text_at = line_index_of(&body, "ANSWERTEXT");
        let thinking_at = line_index_of(&body, "▸ Thinking");
        assert!(
            text_at < thinking_at,
            "text (line {text_at}) must render before the trailing thinking block (line {thinking_at})"
        );
    }

    #[test]
    fn reasoning_then_text_renders_thinking_first() {
        // The common case — model thinks, then answers. The thinking block must
        // render before the assistant text, not after it (the #88 regression).
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        app.handle_out_event(OutEvent::ReasoningDelta {
            session: sid.clone(),
            seq: 1,
            text: "forethought\n".to_string(),
        });
        app.handle_out_event(OutEvent::TextDelta {
            session: sid.clone(),
            seq: 2,
            text: "ANSWERTEXT\n".to_string(),
        });

        let body = render_body_lines(&app, 80);
        let thinking_at = line_index_of(&body, "▸ Thinking");
        let text_at = line_index_of(&body, "ANSWERTEXT");
        assert!(
            thinking_at < text_at,
            "leading thinking block (line {thinking_at}) must render before the text (line {text_at})"
        );
    }

    #[test]
    fn interleaved_runs_stay_ordered_and_distinct() {
        // text → reasoning → text produces two text runs bracketing one thinking
        // block, each a separately-toggleable run keyed by its own block id.
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        app.handle_out_event(OutEvent::TextDelta {
            session: sid.clone(),
            seq: 1,
            text: "FIRSTTEXT\n".to_string(),
        });
        app.handle_out_event(OutEvent::ReasoningDelta {
            session: sid.clone(),
            seq: 2,
            text: "mid think\n".to_string(),
        });
        app.handle_out_event(OutEvent::TextDelta {
            session: sid.clone(),
            seq: 3,
            text: "SECONDTEXT\n".to_string(),
        });

        let body = render_body_lines(&app, 80);
        let first = line_index_of(&body, "FIRSTTEXT");
        let thinking = line_index_of(&body, "▸ Thinking");
        let second = line_index_of(&body, "SECONDTEXT");
        assert!(
            first < thinking && thinking < second,
            "expected FIRSTTEXT ({first}) < Thinking ({thinking}) < SECONDTEXT ({second})"
        );
        // The reasoning run's block id is the transcript index of its first
        // `ReasoningDelta` (entry 1), tagged on its rendered header.
        assert_eq!(body.line_blocks[thinking], Some(1));
    }

    #[test]
    fn reasoning_collapsed_by_default_shows_only_header() {
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        feed_reasoning(&mut app, &sid, "alpha\nbeta\nSECRETBODY\n");

        let body = render_body_lines(&app, 80);
        assert!(
            body.lines
                .iter()
                .any(|l| line_text(l).contains("▸ Thinking")),
            "collapsed run should show a ▸ header"
        );
        assert!(
            !body
                .lines
                .iter()
                .any(|l| line_text(l).contains("SECRETBODY")),
            "collapsed run must hide the reasoning body"
        );
        // The header line is tagged with the run's block id (transcript index 0).
        let header_idx = body
            .lines
            .iter()
            .position(|l| line_text(l).contains("▸ Thinking"))
            .unwrap();
        assert_eq!(body.line_blocks[header_idx], Some(0));
    }

    #[test]
    fn reasoning_expands_on_toggle() {
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        feed_reasoning(&mut app, &sid, "alpha\nbeta\nSECRETBODY\n");

        app.toggle_reasoning_block(0);
        let body = render_body_lines(&app, 80);
        assert!(
            body.lines
                .iter()
                .any(|l| line_text(l).contains("▾ Thinking")),
            "expanded run should show a ▾ header"
        );
        assert!(
            body.lines
                .iter()
                .any(|l| line_text(l).contains("SECRETBODY")),
            "expanded run must show the reasoning body"
        );

        // Toggle round-trip collapses it again.
        app.toggle_reasoning_block(0);
        let body = render_body_lines(&app, 80);
        assert!(!body
            .lines
            .iter()
            .any(|l| line_text(l).contains("SECRETBODY")));
    }

    #[test]
    fn streamed_table_renders_as_grid_after_all_deltas() {
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        // A table streamed token-by-token, exactly as the engine emits it.
        let deltas = [
            "| name | role |\n",
            "| --- | --- |\n",
            "| holly | engine |\n",
            "| tui | head |\n",
        ];
        for (i, d) in deltas.iter().enumerate() {
            app.handle_out_event(OutEvent::TextDelta {
                session: sid.clone(),
                seq: i as u64 + 1,
                text: (*d).to_string(),
            });
        }

        let lines = render_body_lines(&app, 80).lines;
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<String>()
            .replace('\n', "\\n");
        println!("STREAMED TABLE LINES:");
        for l in &lines {
            let s: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
            println!("  {s:?}");
        }
        let has_grid = lines.iter().any(|l| {
            let s: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
            s.contains("---")
        });
        assert!(
            has_grid,
            "streamed table did not render as a grid: {joined}"
        );
    }

    #[test]
    fn narrow_widths_do_not_panic() {
        // The padding rows compute `" ".repeat(width - 1)`; at width 0 or 1 a
        // raw `u16` subtraction underflows (panic in debug, 65535 in release).
        // Feed one of every padded entry kind and render at the degenerate
        // widths — this must not panic and must produce lines.
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        app.record_user_message("hello".to_string());
        app.handle_out_event(OutEvent::TextDelta {
            session: sid.clone(),
            seq: 1,
            text: "assistant reply\n".to_string(),
        });
        app.handle_out_event(OutEvent::ToolCall {
            session: sid.clone(),
            seq: 2,
            request_id: "c1".to_string(),
            tool: "read".to_string(),
            input: "{\"path\":\"x\"}".to_string(),
        });
        app.handle_out_event(OutEvent::ToolOutput {
            session: sid.clone(),
            seq: 3,
            request_id: "c1".to_string(),
            tool: "read".to_string(),
            output: "file body".to_string(),
        });
        app.handle_out_event(OutEvent::Error {
            session: sid.clone(),
            seq: 4,
            message: "boom".to_string(),
        });
        feed_reasoning(&mut app, &sid, "thinking hard\n");

        for width in [0u16, 1, 2] {
            let body = render_body_lines(&app, width);
            assert!(
                !body.lines.is_empty(),
                "width {width} should still render lines"
            );
        }
    }

    #[test]
    fn user_messages_use_profile_colors() {
        let sid = SessionId::new("test");
        let mut app = App::new_for_test(sid.clone());
        app.record_user_message("Hello world".to_string());

        let lines = render_body_lines(&app, 80).lines;
        let user_color = hash_profile_color("build");
        let theme = app.theme();
        let expected_user_bg = theme.user_colors(user_color).bg;

        let user_lines: Vec<_> = lines
            .iter()
            .filter(|l| {
                l.spans
                    .iter()
                    .any(|s| s.content.contains("Hello") || s.content.contains("world"))
            })
            .collect();

        assert!(!user_lines.is_empty(), "Should have user message lines");
        for line in user_lines {
            if let Some(bg) = line.style.bg {
                assert_eq!(
                    bg, expected_user_bg,
                    "User message should have message background"
                );
            }
        }
    }

    #[test]
    fn assistant_lines_use_theme_colors() {
        let sid = SessionId::new("test");
        let mut app = App::new_for_test(sid.clone());
        app.handle_out_event(OutEvent::TextDelta {
            session: sid.clone(),
            seq: 1,
            text: "Response".to_string(),
        });

        let lines = render_body_lines(&app, 80).lines;
        let theme = app.theme();
        let expected_bg = theme.assistant_colors().bg;

        let assistant_lines: Vec<_> = lines
            .iter()
            .filter(|l| l.spans.iter().any(|s| s.content.contains("Response")))
            .collect();

        assert!(!assistant_lines.is_empty(), "Should have assistant lines");
        for line in assistant_lines {
            if let Some(bg) = line.style.bg {
                assert_eq!(
                    bg, expected_bg,
                    "Assistant lines should use theme message background"
                );
            }
        }
    }
}
