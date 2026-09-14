use ratatui::{
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use super::centered_rect;
use crate::tui::app::App;
use crate::tui::session_view::ApprovalMode;
use crate::tui::transcript::render_approval_tool_body;

/// The full-body approval pager (#B2, `v` in `ApprovalMode::WaitingForApproval`):
/// mirrors `/inspect`'s scroll-only text-pane shape (`centered_rect(88, 88)`,
/// `Wrap { trim: false }` + `.scroll((n, 0))`, `modals/inspect.rs`'s
/// `draw_text_pane`) but renders styled `Line`s from `render_approval_tool_body`
/// instead of a plain string, since the approval body carries the same
/// theme-colored diff/plan rendering the transcript tail uses — plain text
/// would lose that.
pub fn draw_approval_pager(f: &mut Frame, app: &App) {
    let area = centered_rect(88, 88, f.area());
    f.render_widget(Clear, area);

    let title = Line::from(vec![
        Span::raw(" Approval "),
        Span::styled(
            "· j/k/↑↓ scroll · PgUp/PgDn ×10 · y/s/a/d/n/e decide · Esc close",
            Style::default().dim(),
        ),
    ]);
    let block = Block::default().borders(Borders::ALL).title(title);

    let lines: Vec<Line<'static>> = match (app.approval_mode(), app.pending_tool_request()) {
        (ApprovalMode::WaitingForApproval { .. }, Some((_, tool, input))) => {
            render_approval_tool_body(app, tool, input, area.width.saturating_sub(2))
        }
        // Defensive only: `advance_approval`/`clear_approval` close the pager
        // the moment the approval they're showing resolves, so this arm
        // shouldn't actually be reachable in a real draw.
        _ => vec![Line::from("(no pending approval)")],
    };

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((app.approval_pager_scroll(), 0));
    f.render_widget(paragraph, area);
}
