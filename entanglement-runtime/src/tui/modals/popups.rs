use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

use super::centered_rect;
use crate::tui::app::App;
use crate::tui::keybindings::KeyMap;

pub fn draw_which_key_popup(f: &mut Frame, keymap: &KeyMap) {
    let bindings = keymap.all_bindings();
    let mut lines = Vec::new();

    for (sequence, action) in bindings {
        let key_str = format!("{}", sequence);
        let description = action.description();
        lines.push(ListItem::new(Line::from(vec![
            Span::styled(
                format!(" {:8} ", key_str),
                Style::default().fg(Color::Cyan).bold(),
            ),
            Span::styled(description, Style::default()),
        ])));
    }

    let list = List::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Keybindings (Esc to cancel)"),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));

    let area = centered_rect(80, 60, f.area());
    f.render_widget(Clear, area);
    f.render_widget(list, area);
}

pub fn draw_help_dialog(f: &mut Frame, keymap: &KeyMap) {
    let bindings = keymap.all_bindings();
    let mut current_category = String::new();
    let mut lines = Vec::new();

    for (sequence, action) in bindings {
        let category = action.category();
        if category != current_category {
            if !current_category.is_empty() {
                lines.push(ListItem::new(Line::from("")));
            }
            lines.push(ListItem::new(Line::from(vec![Span::styled(
                format!("{}:", category),
                Style::default().fg(Color::Yellow).bold(),
            )])));
            current_category = category.to_string();
        }

        let key_str = format!("{}", sequence);
        let description = action.description();
        lines.push(ListItem::new(Line::from(vec![
            Span::styled(
                format!("  {:12} ", key_str),
                Style::default().fg(Color::Cyan).bold(),
            ),
            Span::styled(description, Style::default()),
        ])));
    }

    let list = List::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Keybindings Help (Esc to close)"),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));

    let area = centered_rect(80, 70, f.area());
    f.render_widget(Clear, area);
    f.render_widget(list, area);
}

pub fn draw_command_palette(f: &mut Frame, app: &mut App) {
    let palette = app.command_palette();
    let query = palette.query().to_string();
    let commands = palette.filtered_commands().to_vec();

    let items: Vec<ListItem> = commands
        .iter()
        .map(|cmd| {
            let name = cmd.slash_name();
            let description = cmd.description();
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{} ", name),
                    Style::default().fg(Color::Cyan).bold(),
                ),
                Span::styled(description, Style::default().dim()),
            ]))
        })
        .collect();

    let input_paragraph = Paragraph::new(if query.is_empty() {
        "Type to filter commands..."
    } else {
        query.as_str()
    })
    .style(Style::default().fg(Color::Yellow));

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Command Palette (Esc to close, Enter to execute)"),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));

    let area = centered_rect(60, 50, f.area());

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)].as_ref())
        .split(area);

    f.render_widget(Clear, area);
    f.render_widget(input_paragraph, chunks[0]);
    f.render_stateful_widget(list, chunks[1], palette.state());
}

pub fn draw_slash_autocomplete(f: &mut Frame, app: &mut App, input_area: Rect) {
    let input_text = app.input().lines().join("\n");

    if !input_text.starts_with('/') || input_text.chars().count() > 1 {
        return;
    }

    let commands = crate::tui::commands::all_commands();

    let items: Vec<ListItem> = commands
        .iter()
        .map(|cmd| {
            let name = cmd.slash_name();
            let description = cmd.description();
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{} ", name),
                    Style::default().fg(Color::Cyan).bold(),
                ),
                Span::styled(description, Style::default().dim()),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Commands (Tab to select)"),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));

    let popup_area = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(15),
        width: input_area.width.min(60),
        height: 15.min(input_area.y),
    };

    f.render_widget(Clear, popup_area);
    f.render_widget(list, popup_area);
}

/// `@file` completion popup (ADR-0030). Anchored above the input like the slash
/// autocomplete, listing the fuzzy-matched relative paths with the current pick
/// highlighted.
pub fn draw_mention_popup(f: &mut Frame, app: &mut App, input_area: Rect) {
    if !app.mention().visible() {
        return;
    }
    let matches: Vec<String> = app.mention().matches().to_vec();
    if matches.is_empty() {
        return;
    }

    let items: Vec<ListItem> = matches
        .iter()
        .map(|p| {
            ListItem::new(Line::from(Span::styled(
                p.clone(),
                Style::default().fg(Color::Cyan),
            )))
        })
        .collect();

    let height = (matches.len() as u16 + 2).min(15).min(input_area.y);
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Files (Tab/Enter to insert, Esc to dismiss)"),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));

    let popup_area = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(height),
        width: input_area.width.min(70),
        height,
    };

    f.render_widget(Clear, popup_area);
    f.render_stateful_widget(list, popup_area, app.mention_mut().state());
}

/// Draw the two-stage `/key` dialog (#304): the provider list, then a masked
/// input for the chosen provider's key. The key is only ever shown as bullets —
/// [`crate::tui::key_dialog::KeyDialog::masked`] renders the buffer, never its
/// characters.
pub fn draw_key_dialog(f: &mut Frame, app: &mut App) {
    use crate::tui::key_dialog::KeyStage;

    let area = centered_rect(50, 40, f.area());
    f.render_widget(Clear, area);

    match app.key_dialog_stage() {
        KeyStage::PickProvider => {
            let items: Vec<ListItem> = app
                .key_dialog()
                .providers()
                .iter()
                .map(|p| {
                    ListItem::new(Line::from(vec![
                        Span::styled(p.name.clone(), Style::default().bold()),
                        Span::raw("  "),
                        Span::styled(p.key_env.clone(), Style::default().dim()),
                    ]))
                })
                .collect();
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Set API Key — pick provider (Enter: next, Esc: close)"),
                )
                .highlight_style(Style::default().bg(Color::DarkGray));
            f.render_stateful_widget(list, area, app.key_dialog_state());
        }
        KeyStage::EnterKey => {
            let provider = app
                .key_dialog()
                .selected_provider()
                .map(|p| (p.name.clone(), p.key_env.clone()))
                .unwrap_or_default();
            let bullets = app.key_dialog().masked();
            let text = vec![
                Line::from(vec![
                    Span::raw("Provider: "),
                    Span::styled(provider.0, Style::default().bold()),
                ]),
                Line::from(vec![
                    Span::raw("Env var:  "),
                    Span::styled(provider.1, Style::default().dim()),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::raw("Key: "),
                    Span::styled(bullets, Style::default().fg(Color::Cyan)),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "Enter: save   Esc: back",
                    Style::default().dim(),
                )),
            ];
            let para = Paragraph::new(text).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Set API Key — enter value (never shown)"),
            );
            f.render_widget(para, area);
        }
    }
}

/// Draw the `/mcp list` result panel (#373): connected servers, transport,
/// status, and tools, reusing the read-only-list shape of [`draw_help_dialog`]
/// — `Esc` is the only key it consumes, so no `ListState`/highlight is needed.
pub fn draw_mcp_panel(f: &mut Frame, app: &App) {
    let servers = app.mcp_servers();
    let mut lines: Vec<ListItem> = Vec::new();

    if servers.is_empty() {
        lines.push(ListItem::new(Line::from(Span::styled(
            "No MCP servers connected.",
            Style::default().dim(),
        ))));
    } else {
        for s in servers {
            let status = if s.connected {
                Span::styled("connected", Style::default().fg(Color::Green))
            } else {
                Span::styled("disconnected", Style::default().fg(Color::Red))
            };
            lines.push(ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", s.name), Style::default().bold()),
                Span::styled(format!("[{}] ", s.transport), Style::default().dim()),
                status,
            ])));
            if let Some(err) = &s.error {
                lines.push(ListItem::new(Line::from(Span::styled(
                    format!("  error: {err}"),
                    Style::default().fg(Color::Red),
                ))));
            } else if s.tools.is_empty() {
                lines.push(ListItem::new(Line::from(Span::styled(
                    "  (no tools)",
                    Style::default().dim(),
                ))));
            } else {
                lines.push(ListItem::new(Line::from(Span::styled(
                    format!("  tools: {}", s.tools.join(", ")),
                    Style::default().dim(),
                ))));
            }
        }
    }

    let list = List::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title("MCP Servers (Esc to close)"),
    );

    let area = centered_rect(70, 60, f.area());
    f.render_widget(Clear, area);
    f.render_widget(list, area);
}

/// Draw the `/agent` picker's `e` tools-checklist dialog (#330): every
/// advertised tool with a checkbox reflecting the profile's current effective
/// mask. `Space` toggles, `Enter` saves a user-layer override, `Esc` discards.
pub fn draw_tools_dialog(f: &mut Frame, app: &mut App) {
    let agent = app.tools_dialog().agent().to_string();
    let tools = app.tools_dialog().tools().to_vec();

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
            ListItem::new(Line::from(vec![
                Span::styled(mark, style),
                Span::styled(name.clone(), style),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(format!(
            "Tool allowlist — {agent} (Space: toggle, Enter: save, Esc: cancel)"
        )))
        .highlight_style(Style::default().bg(Color::DarkGray));

    let area = centered_rect(60, 60, f.area());
    f.render_widget(Clear, area);
    f.render_stateful_widget(list, area, app.tools_dialog_state());
}
