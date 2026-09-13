//! Tool/MCP availability popups: the `/mcp list` panel (#373, selectable
//! since #539), bare `/enable`'s session-tools checklist (#539), and the
//! `/agent` picker's tools-allowlist checklist (#330). Split from
//! `popups.rs` once it crossed the 400-line cap.

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

use super::centered_rect;
use crate::tool_state::DispatchState;
use crate::tui::app::App;

/// Draw the `/mcp list` result panel (#373, selectable since #539): connected
/// servers, transport, status, and tools. `Up`/`Down`/`j`/`k` move the server
/// selection (`▶`), `e`/`d` enable/disable the selected server for the active
/// session (a `mcp__<name>__*` overlay entry — the tag after the status shows
/// the session's current override), `c`/`t` run the OAuth connect/check for it
/// (ADR-0153), `PageUp`/`PageDown` scroll, `Esc` closes.
pub fn draw_mcp_panel(f: &mut Frame, app: &mut App) {
    let servers = app.mcp_servers();
    let overlay = app.overlay_entries(app.active_session_id());
    let selected = app.mcp_selected();
    let mut lines: Vec<Line> = Vec::new();

    if servers.is_empty() {
        // WHY: a bare "no servers" panel leaves the user stuck — show the
        // `/mcp add` syntax inline so the path to attach a server is visible
        // right where the gap is.
        lines.push(Line::from(Span::styled(
            "No MCP servers connected.",
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
            lines.push(Line::from(Span::styled(
                u,
                Style::default().fg(Color::DarkGray),
            )));
        }
    } else {
        for (i, s) in servers.iter().enumerate() {
            // Three-state display (#542): an `allowed` server that isn't
            // connected is *available* (enable it with `e` / `/enable mcp
            // <name>`), not broken — don't paint it red.
            let status = if s.auth.as_deref() == Some("needs auth") {
                // An OAuth server awaiting sign-in (ADR-0153) is not broken
                // either — it just needs `c`. Say so instead of "disconnected".
                Span::styled(
                    "needs auth (sign in with c)",
                    Style::default().fg(Color::Yellow),
                )
            } else if s.connected {
                Span::styled("connected", Style::default().fg(Color::Green))
            } else if s.state.as_deref() == Some("allowed") {
                Span::styled(
                    "available (enable with e)",
                    Style::default().fg(Color::Yellow),
                )
            } else {
                Span::styled("disconnected", Style::default().fg(Color::Red))
            };
            let marker = if i == selected { "▶ " } else { "  " };
            let name_style = if i == selected {
                Style::default().bold().bg(Color::DarkGray)
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
            // an exact `mcp__<name>__*` entry, as the `e`/`d` keys write it.
            let server_pattern = format!("mcp__{}__*", s.name);
            if let Some(e) = overlay.iter().find(|e| e.pattern == server_pattern) {
                let tag = if e.deny {
                    Span::styled("  session: off", Style::default().fg(Color::Red))
                } else if e.allow {
                    Span::styled("  session: on (allow)", Style::default().fg(Color::Green))
                } else {
                    Span::styled("  session: on", Style::default().fg(Color::Green))
                };
                header.push(tag);
            }
            lines.push(Line::from(header));
            if let Some(err) = &s.error {
                lines.push(Line::from(Span::styled(
                    format!("    error: {err}"),
                    Style::default().fg(Color::Red),
                )));
            } else if s.tools.is_empty() {
                lines.push(Line::from(Span::styled(
                    "    (no tools)",
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
            .title("MCP Servers (e/d: enable/disable, c/t: sign in/check, ↑↓: select, Esc: close)"),
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
                    spans.push(Span::styled(tag, Style::default().fg(Color::Green)));
                }
                // A deny override the diff will materialize.
                (false, true) => {
                    spans.push(Span::styled("  -session", Style::default().fg(Color::Red)));
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
        .highlight_style(Style::default().bg(Color::DarkGray));

    let area = centered_rect(60, 60, f.area());
    app.set_session_tools_dialog_rect(area);
    f.render_widget(Clear, area);
    f.render_stateful_widget(list, area, app.session_tools_dialog_state());
}

/// Draw the `/agent` picker's `e` tools-checklist dialog (#330): every tool
/// with a checkbox reflecting the profile's current effective mask **and** the
/// dispatch state that mask + the profile's permission grades produce
/// (`allowed`/`asks`/`declines`, `crate::tool_state`) — the checkbox alone
/// would read as "the model can't see this tool", which stopped being true
/// once advertisement decoupled from enforcement. Unchecking a row flips its
/// state to `declines` live, before the override is saved. `Space` toggles,
/// `Enter` saves a user-layer override, `Esc` discards.
pub fn draw_tools_dialog(f: &mut Frame, app: &mut App) {
    let agent = app.tools_dialog().agent().to_string();
    let tools = app.tools_dialog().tools().to_vec();
    let width = tools.iter().map(|t| t.chars().count()).max().unwrap_or(0);

    let items: Vec<ListItem> = tools
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let checked = app.tools_dialog().is_checked(i);
            let (mark, style) = if checked {
                ("[x] ", Style::default())
            } else {
                ("[ ] ", Style::default().dim())
            };
            let dispatch = app.tools_dialog().tool_state(i);
            let color = match dispatch.state {
                DispatchState::Allowed => Color::Green,
                DispatchState::Asks => Color::Yellow,
                DispatchState::Declines => Color::Red,
            };
            ListItem::new(Line::from(vec![
                Span::styled(mark, style),
                Span::styled(format!("{name:<width$}  "), style),
                Span::styled(dispatch.label(), Style::default().fg(color)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(format!(
            "Tool mask — {agent} (Space: toggle, Enter: save, Esc: cancel)"
        )))
        .highlight_style(Style::default().bg(Color::DarkGray));

    let area = centered_rect(60, 60, f.area());
    app.set_tools_dialog_rect(area);
    f.render_widget(Clear, area);
    f.render_stateful_widget(list, area, app.tools_dialog_state());
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::SessionId;
    use ratatui::{backend::TestBackend, Terminal};

    /// Every row of the `/agent` checklist shows its dispatch state beside the
    /// checkbox — a masked-out tool reads `declines`, not "absent".
    #[test]
    fn tools_dialog_rows_render_their_dispatch_state() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.open_tools_dialog(); // `build`: default-allow, inherit-all mask.
        app.tools_dialog_toggle(); // uncheck the first row.

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| draw_tools_dialog(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rendered = (0..30)
            .map(|y| {
                (0..100)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("Tool mask — build"), "{rendered}");
        assert!(rendered.contains("[ ] read"), "{rendered}");
        assert!(rendered.contains("declines"), "{rendered}");
        assert!(rendered.contains("[x] edit"), "{rendered}");
        assert!(rendered.contains("allowed"), "{rendered}");
    }
}
