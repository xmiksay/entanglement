use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};

use crate::tui::app::App;
use crate::tui::session_view::ApprovalMode;
use crate::tui::theme::RoleColors;
use crate::tui::tool_render;
use crate::tui::wrap;

mod block;
pub(crate) mod cache;
mod question;
mod render_run;
mod segment;
#[cfg(test)]
mod tests;

use question::render_question;

/// Rendered transcript plus per-line provenance. `line_blocks[i]` holds the
/// clickable-block id (a reasoning run's first `ReasoningDelta` index or a tool
/// op's `ToolCall` index) that produced rendered line `i`, or `None` for lines
/// that aren't part of a clickable block. Click hit-testing (`App::block_at`)
/// maps a `row + scroll_offset` back to a block through this vector.
///
/// The body is owned (`Line<'static>`) so it can be memoized in the per-session
/// [`cache::RenderCache`] and cloned into the `Paragraph` each frame (#342).
pub(crate) struct RenderedBody {
    pub lines: Vec<Line<'static>>,
    pub line_blocks: Vec<Option<usize>>,
}

pub(crate) fn render_body_lines(app: &mut App, available_width: u16) -> RenderedBody {
    let theme = app.theme();
    let user = theme.user_colors(app.profile_color_for(app.agent()));

    // The transcript body is cache-aware: only the block whose content hash
    // changed re-parses markdown; an idle redraw clones owned lines (#342).
    let (mut lines, mut line_blocks) = app.render_cached_body(available_width, theme, user);

    // Plan and task-list snapshots now live in the sidebar's "Plan Outline" /
    // "Tasks" sections (Ctrl+X s, or /plan · /tasks), not inline at the top of
    // the chat transcript (#325).

    // The approval/question tail is small and changes every frame while parked,
    // so it renders fresh after the cached body rather than through the cache.
    if let ApprovalMode::WaitingForApproval { .. } = app.approval_mode() {
        if let Some((_, tool, input)) = app.pending_tool_request() {
            let (tool, input) = (tool.clone(), input.clone());
            let rule = "─".repeat(available_width.max(1) as usize);
            // Yellow accent ties this tail to the surrounding rules; the tail
            // now goes through `Theme::decorate` like any other block so it
            // reads as part of the transcript, not a bolted-on overlay (#487).
            let approval_colors = RoleColors {
                fg: Color::Yellow,
                bg: theme.message_bg,
            };
            lines.push(Line::from(""));
            lines.push(Line::from(rule.clone()).fg(Color::Yellow));

            // Same `▸/▾ tool  primary_arg` idiom the collapsed/expanded block
            // header uses (`flush_tool_call`) — the tail is always fully shown,
            // so the arrow is `▾`.
            let mut header = render_run::tool_header_spans(
                &tool,
                &input,
                '▾',
                Color::Yellow,
                available_width,
                None,
            );
            // Core batch-emits tool calls (#270), so more approvals may be
            // parked behind this one (#273) — show how many are waiting.
            let queued = app.queued_approvals();
            if queued > 0 {
                header.push(Span::styled(
                    format!("  (+{queued} more queued)"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            lines.push(theme.decorate(Line::from(header), approval_colors, available_width));

            // The body: the same per-tool renderer the expanded block uses (a
            // real diff for `edit`, the new content for `write`, plan markdown
            // for `propose_plan`, …) instead of raw JSON — the primary arg
            // already lives in the header above, so it's never re-dumped
            // (mirrors `flush_tool_call`'s expanded branch, #487). No output
            // yet — the call hasn't run.
            let rendered = tool_render::render_expansion(
                Some(&tool),
                &input,
                "",
                theme,
                available_width,
                app.markdown_renderer(),
            );
            for line in rendered.lines {
                lines.push(theme.decorate(line, approval_colors, available_width));
            }

            lines.push(Line::from(""));
            let mut footer = vec![
                Span::styled("[y]", Style::default().fg(Color::Green).bold()),
                Span::raw(" approve  "),
            ];
            // `[d]` (#486, ADR-0126) only makes sense for the read-only triad
            // (`read`/`grep`/`glob`) — a `SessionDir` grant on any other tool
            // would just degrade to an exact `Session` grant, so the hint is
            // withheld rather than shown misleadingly.
            if crate::tool_names::is_read_capability_member(&tool) {
                footer.push(Span::styled(
                    "[d]",
                    Style::default().fg(Color::Green).bold(),
                ));
                footer.push(Span::raw(" allow dir (session)  "));
            }
            footer.extend([
                Span::styled("[n]", Style::default().fg(Color::Red).bold()),
                Span::raw(" reject  "),
                Span::styled("[e]", Style::default().fg(Color::Yellow).bold()),
                Span::raw(" edit reason  "),
                Span::styled("[Esc]", Style::default().fg(Color::Gray).bold()),
                Span::raw(" interrupt"),
            ]);
            // The footer is short, but wrap it too so a very narrow panel can't
            // overflow horizontally (#wrap).
            push_wrapped_spans(&mut lines, footer, available_width);
            lines.push(Line::from(rule).fg(Color::Yellow));
        }
    }

    if let Some(q) = app.pending_question() {
        render_question(&mut lines, q, &app.input_text(), available_width);
    }

    // The freshly-appended tail lines carry no clickable-block provenance.
    line_blocks.resize(lines.len(), None);

    RenderedBody { lines, line_blocks }
}

/// Wrap a footer of styled `spans` to `available_width`, pushing each wrapped
/// line. The approval/question footers are short, but on a very narrow panel
/// they could still overflow — wrapping keeps every box horizontal-scroll-free
/// (#wrap).
fn push_wrapped_spans<'a>(lines: &mut Vec<Line<'a>>, spans: Vec<Span<'a>>, available_width: u16) {
    for wline in wrap::wrap_line(Line::from(spans), available_width) {
        lines.push(wline);
    }
}
