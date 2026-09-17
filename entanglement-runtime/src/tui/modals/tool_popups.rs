//! Tool/MCP availability popups: the `/mcp list` panel (#373, selectable
//! since #539), bare `/enable`'s session-tools checklist (#539), and `/tools`
//! (#560 P9, ADR-0199 part 3). Split from `popups.rs` once it crossed the
//! 400-line cap. The `/agent` picker's tools-allowlist checklist (#330) this
//! module also used to draw is retired (ADR-0207: no more per-agent tool
//! mask to allowlist).

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

use super::centered_rect;
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

/// Draw `/tools` (#560 P9, ADR-0199 part 3): every row from
/// `App::tools_view_rows`, grouped visually by kind label, filtered live by
/// the typed free text and the `Tab`-cycled category. The title bar doubles
/// as the filter/category status line — there's no separate input widget to
/// render, `tools_view`'s own state already carries both.
pub fn draw_tools_view(f: &mut Frame, app: &mut App) {
    let category = app.tools_view().category().unwrap_or("all");
    let filter = app.tools_view().filter_text().to_string();
    let rows = app.tools_view().filtered();

    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            let masked_style = if row.masked {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::Green)
            };
            let masked_tag = if row.masked { "masked" } else { "usable" };
            let grade_tag = row.grade.unwrap_or("-");
            let spans = vec![
                Span::styled(format!("[{:<8}] ", row.kind), Style::default().dim()),
                Span::styled(row.name.clone(), Style::default().bold()),
                Span::raw("  "),
                Span::styled(row.status, Style::default().fg(Color::Cyan)),
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
        .highlight_style(Style::default().bg(Color::DarkGray));

    // No mouse hit-test rect (unlike the other tool dialogs): `/tools` is a
    // keyboard-driven typeahead browser, not a click target — out of scope
    // for this pass, left for a follow-up if mouse selection is wanted here
    // too.
    let area = centered_rect(80, 70, f.area());
    f.render_widget(Clear, area);
    f.render_stateful_widget(list, area, app.tools_view_state());
}
