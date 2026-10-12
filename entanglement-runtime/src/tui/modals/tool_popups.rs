//! Tool/MCP availability popups: the `/mcp list` panel (#373, selectable
//! since #539), bare `/enable`'s session-tools checklist (#539), and `/tools`
//! (#560 P9, ADR-0199 part 3). Split from `popups.rs` once it crossed the
//! 400-line cap. The `/agent` picker's tools-allowlist checklist (#330) this
//! module also used to draw is retired (ADR-0207: no more per-agent tool
//! mask to allowlist).

use ratatui::{
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

use super::centered_rect;
use crate::tui::app::App;
use crate::tui::theme::Slot;

/// Draw the `/mcp list` result panel (#373, selectable since #539): every
/// registered server, transport, status (connected / available / auth
/// required), and tools; a tag shows the session's `mcp__<name>__*` overlay
/// override, if any. `Up`/`Down`/`j`/`k` move the server selection (`▶`),
/// `d` denies the selected server for the active session (a
/// `mcp__<name>__*` overlay entry), `c`/`t` run the OAuth connect/check for it
/// (ADR-0153),
/// `PageUp`/`PageDown` scroll, `Esc` closes.
pub fn draw_mcp_panel(f: &mut Frame, app: &mut App) {
    let theme = app.theme();
    let servers = app.mcp_servers();
    let overlay = app.overlay_entries(app.active_session_id());
    let selected = app.mcp_selected();
    let mut lines: Vec<Line> = Vec::new();

    if servers.is_empty() {
        // WHY: a bare "no servers" panel leaves the user stuck — show the
        // `/mcp add` syntax inline so the path to attach a server is visible
        // right where the gap is.
        lines.push(Line::from(Span::styled(
            "No MCP servers configured.",
            Style::default().dim(),
        )));
        let usage = [
            "Add a server:",
            "  /mcp add <name> --url <url> [--header KEY:VALUE]...",
            "  /mcp add <name> -- <command> [args...]",
            "List/remove:  /mcp list   /mcp remove <name>",
            "Sign in:      /mcp connect <name>   (check/disconnect too)",
        ];
        lines.push(Line::from(""));
        for u in usage {
            lines.push(Line::from(Span::styled(u, theme.fg(Slot::Muted))));
        }
    } else {
        for (i, s) in servers.iter().enumerate() {
            // Registered = available (ADR-0226): an unconnected server just
            // hasn't been used yet — it connects on its first call, so it is
            // not painted as broken.
            let status = if s.connected {
                Span::styled("connected", theme.fg(Slot::Success))
            } else if s.auth.as_deref() == Some("needs auth") {
                Span::styled(
                    format!("auth required (/mcp connect {}, or c)", s.name),
                    theme.fg(Slot::Warning),
                )
            } else {
                Span::styled("available", theme.fg(Slot::Muted))
            };
            let marker = if i == selected { "▶ " } else { "  " };
            let name_style = if i == selected {
                theme.selection().bold()
            } else {
                Style::default().bold()
            };
            let mut header = vec![
                Span::raw(marker),
                Span::styled(format!("{} ", s.name), name_style),
                Span::styled(format!("[{}] ", s.transport), Style::default().dim()),
                status,
            ];
            // The active session's overlay override for this server (#539):
            // an exact `mcp__<name>__*` entry, as `d` or `/disable tool` writes it.
            let server_pattern = crate::mcp::tool::server_tools_pattern(&s.name);
            if let Some(e) = overlay.iter().find(|e| e.pattern == server_pattern) {
                let tag = if e.deny {
                    Span::styled("  session: off", theme.fg(Slot::Error))
                } else if e.allow {
                    Span::styled("  session: on (allow)", theme.fg(Slot::Success))
                } else {
                    Span::styled("  session: on", theme.fg(Slot::Success))
                };
                header.push(tag);
            }
            lines.push(Line::from(header));
            if let Some(err) = &s.error {
                lines.push(Line::from(Span::styled(
                    format!("    error: {err}"),
                    theme.fg(Slot::Error),
                )));
            } else if s.tools.is_empty() {
                lines.push(Line::from(Span::styled(
                    if s.connected {
                        "    (no tools)"
                    } else {
                        "    (connects on first use)"
                    },
                    Style::default().dim(),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    format!("    tools: {}", s.tools.join(", ")),
                    Style::default().dim(),
                )));
            }
        }
    }

    let para = Paragraph::new(lines).scroll((app.mcp_scroll(), 0)).block(
        Block::default()
            .borders(Borders::ALL)
            .title("MCP Servers (d: deny for session, c/t: sign in/check, ↑↓: select, Esc: close)"),
    );

    let area = centered_rect(70, 60, f.area());
    app.set_mcp_panel_rect(area);
    f.render_widget(Clear, area);
    f.render_widget(para, area);
}

/// Draw bare `/enable`'s session-tools checklist (#539): every advertised tool
/// with a checkbox reflecting the active session's effective availability.
/// `Space` toggles, `a` toggles auto-allow on an enabled override, `Enter`
/// applies the overlay diff, `Esc` discards.
pub fn draw_session_tools_dialog(f: &mut Frame, app: &mut App) {
    let theme = app.theme();
    let rows = app.session_tools_dialog().rows().to_vec();

    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            let (mark, style) = if row.checked {
                ("[x] ", Style::default())
            } else {
                ("[ ] ", Style::default().dim())
            };
            let mut spans = vec![
                Span::styled(mark, style),
                Span::styled(row.name.clone(), style),
            ];
            match (row.checked, row.profile_default) {
                // An enable override the diff will materialize.
                (true, false) => {
                    let tag = if row.allow {
                        "  +session (allow)"
                    } else {
                        "  +session"
                    };
                    spans.push(Span::styled(tag, theme.fg(Slot::Success)));
                }
                // A deny override the diff will materialize.
                (false, true) => {
                    spans.push(Span::styled("  -session", theme.fg(Slot::Error)));
                }
                _ => {}
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Session tools (Space: toggle, a: auto-allow, Enter: apply, Esc: cancel)"),
        )
        .highlight_style(theme.selection());

    let area = centered_rect(60, 60, f.area());
    app.set_session_tools_dialog_rect(area);
    f.render_widget(Clear, area);
    f.render_stateful_widget(list, area, app.session_tools_dialog_state());
}

/// Draw `/tools` (#560 P9, ADR-0199 part 3): every row from
/// `App::tools_view_rows`, grouped visually by kind label, filtered live by
/// the typed free text and the `Tab`-cycled category. The title bar doubles
/// as the filter/category status line — there's no separate input widget to
/// render, `tools_view`'s own state already carries both.
pub fn draw_tools_view(f: &mut Frame, app: &mut App) {
    let theme = app.theme();
    let category = app.tools_view().category().unwrap_or("all");
    let filter = app.tools_view().filter_text().to_string();
    let rows = app.tools_view().filtered();

    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            let masked_style = if row.masked {
                theme.fg(Slot::Error)
            } else {
                theme.fg(Slot::Success)
            };
            let masked_tag = if row.masked { "masked" } else { "usable" };
            let grade_tag = row.grade.unwrap_or("-");
            let spans = vec![
                Span::styled(format!("[{:<8}] ", row.kind), Style::default().dim()),
                Span::styled(row.name.clone(), Style::default().bold()),
                Span::raw("  "),
                Span::styled(row.status, theme.fg(Slot::Accent)),
                Span::raw("  "),
                Span::styled(masked_tag, masked_style),
                Span::raw("  "),
                Span::styled(grade_tag, Style::default().dim()),
                Span::raw("  "),
                Span::styled(row.description.clone(), Style::default().dim()),
            ];
            ListItem::new(Line::from(spans))
        })
        .collect();

    let title = if filter.is_empty() {
        format!("Tools [{category}] (Tab: category, type to filter, Enter: enable, Esc: close)")
    } else {
        format!(
            "Tools [{category}] filter: '{filter}' (Tab: category, Backspace: edit, Enter: enable, Esc: close)"
        )
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(theme.selection());

    // No mouse hit-test rect (unlike the other tool dialogs): `/tools` is a
    // keyboard-driven typeahead browser, not a click target — out of scope
    // for this pass, left for a follow-up if mouse selection is wanted here
    // too.
    let area = centered_rect(80, 70, f.area());
    f.render_widget(Clear, area);
    f.render_stateful_widget(list, area, app.tools_view_state());
}

#[cfg(test)]
mod tests {
    use entanglement_core::{McpServerStatus, OutEvent, SessionId};
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;

    /// ADR-0226: every server is on by default, so the panel offers the
    /// per-session deny (`d`) and the OAuth keys — never an enable key — and
    /// an unconnected server reads "available", not broken.
    #[test]
    fn the_panel_offers_deny_but_no_enable_and_shows_availability() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.record_pending_mcp_list("c1".into());
        app.handle_out_event(OutEvent::McpList {
            correlation_id: "c1".into(),
            servers: vec![McpServerStatus {
                name: "kb".into(),
                transport: "http".into(),
                connected: false,
                tools: vec![],
                error: None,
                state: None,
                auth: None,
            }],
        });
        assert!(app.showing_mcp_panel());
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| draw_mcp_panel(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text: String = (0..30)
            .flat_map(|y| (0..140).map(move |x| (x, y)))
            .map(|(x, y)| buffer[(x, y)].symbol().to_string())
            .collect();
        assert!(text.contains("d: deny for session"), "{text}");
        assert!(!text.contains("e/d"), "{text}");
        assert!(text.contains("available"), "{text}");
    }
}
