mod app;
mod commands;
mod diff;
mod event;
mod keybindings;
mod markdown;
mod modals;
mod session_view;
mod sessions;
mod ui;

use anyhow::Result;
use entanglement_core::{Holly, InMsg, SessionId};
use ratatui::{
    backend::CrosstermBackend,
    crossterm::{
        event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    },
    widgets::Borders,
    Terminal,
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::debug;

use crate::ModelInfo;
use app::App;
use event::Event;
use session_view::ApprovalMode;

pub async fn tui(holly: Holly, initial_session: SessionId, model_info: ModelInfo) -> Result<()> {
    setup_panic_handler();

    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    enable_raw_mode()?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (event_tx, mut event_rx) = mpsc::channel(128);
    spawn_crossterm_task(event_tx.clone());

    let mut app = App::new(initial_session);
    app.set_model_info(model_info.provider, model_info.model);

    let mut holly_sub = holly.subscribe();

    const FRAME_INTERVAL: Duration = Duration::from_millis(33);
    let mut last_draw = Instant::now();

    loop {
        if app.is_dirty() {
            let wait = FRAME_INTERVAL.saturating_sub(last_draw.elapsed());
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            terminal.draw(|f| ui::draw(f, &mut app))?;
            last_draw = Instant::now();
        }

        if app.leader_handler().check_timeout() {
            app.mark_dirty();
        }

        tokio::select! {
            biased;
            Some(ev) = event_rx.recv() => {
                if handle_event(&mut app, &holly, ev).await? {
                    break;
                }
            }
            recv = tokio::time::timeout(Duration::from_millis(50), holly_sub.recv()) => match recv {
                Ok(Ok(event)) => app.handle_out_event(event),
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                    tracing::warn!("TUI lagged, skipped {n} engine events");
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                Err(_) => {}
            }
        }

        if drain_terminal_events(&mut event_rx, &mut app, &holly).await? {
            break;
        }
        drain_engine_events(&mut holly_sub, &mut app);
    }

    restore_terminal(&mut terminal)?;
    Ok(())
}

fn spawn_crossterm_task(tx: mpsc::Sender<Event>) {
    tokio::spawn(async move {
        loop {
            match event::read().await {
                Ok(ev) => {
                    if tx.send(ev).await.is_err() {
                        break;
                    }
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    });
}

async fn handle_event(app: &mut App, holly: &Holly, ev: Event) -> Result<bool> {
    app.mark_dirty();
    match ev {
        Event::Key(key) => {
            if key.kind == KeyEventKind::Press {
                if app.showing_sessions_modal() {
                    return handle_sessions_modal_event(app, key).await;
                }
                if app.showing_profile_picker() {
                    return handle_profile_picker_event(app, holly, key).await;
                }
                if app.showing_model_picker() {
                    return handle_model_picker_event(app, key).await;
                }
                if app.showing_help() {
                    if key.code == KeyCode::Esc {
                        app.close_help();
                    }
                    return Ok(false);
                }
                if app.showing_command_palette() {
                    return handle_command_palette_event(app, key).await;
                }

                let current_mode = app.approval_mode().clone();

                if key.code == KeyCode::Char('l')
                    && key.modifiers == KeyModifiers::CONTROL
                    && !matches!(current_mode, ApprovalMode::EnteringRejectReason { .. })
                {
                    app.toggle_sessions_modal();
                    return Ok(false);
                }

                if matches!(current_mode, ApprovalMode::Normal) {
                    if let Some(action) = app.leader_handler().handle_key(&key) {
                        if app.dispatch_action(action) {
                            return Ok(true);
                        }
                        return Ok(false);
                    }
                }

                match current_mode {
                    ApprovalMode::WaitingForApproval { request_id } => match key.code {
                        KeyCode::Char('y') => {
                            let _ = holly
                                .send(InMsg::Approve {
                                    session: app.active_session_id().clone(),
                                    request_id: request_id.clone(),
                                })
                                .await;
                            app.set_approval_mode(ApprovalMode::Normal);
                        }
                        KeyCode::Char('n') => {
                            app.set_approval_mode(ApprovalMode::EnteringRejectReason {
                                request_id: request_id.clone(),
                            });
                            app.input().set_block(
                                ratatui::widgets::Block::new()
                                    .borders(Borders::TOP)
                                    .border_style(
                                        ratatui::style::Style::default()
                                            .fg(ratatui::style::Color::Yellow),
                                    ),
                            );
                        }
                        KeyCode::Char('e') => {
                            app.set_approval_mode(ApprovalMode::EnteringRejectReason {
                                request_id: request_id.clone(),
                            });
                            app.input().set_block(
                                ratatui::widgets::Block::new()
                                    .borders(Borders::TOP)
                                    .border_style(
                                        ratatui::style::Style::default()
                                            .fg(ratatui::style::Color::Yellow),
                                    ),
                            );
                        }
                        KeyCode::Esc => {
                            let _ = holly
                                .send(InMsg::Stop {
                                    session: app.active_session_id().clone(),
                                })
                                .await;
                            app.note_stop_sent();
                            app.clear_approval();
                        }
                        _ => {}
                    },
                    ApprovalMode::EnteringRejectReason { request_id } => match key.code {
                        KeyCode::Esc => {
                            app.set_approval_mode(ApprovalMode::WaitingForApproval {
                                request_id: request_id.clone(),
                            });
                            app.input()
                                .set_block(ratatui::widgets::Block::new().borders(Borders::TOP));
                            let text = app.take_input_text();
                            if !text.is_empty() {
                                app.input().insert_str(&text);
                            }
                        }
                        KeyCode::Enter => {
                            let text = app.take_input_text();
                            let _ = holly
                                .send(InMsg::Reject {
                                    session: app.active_session_id().clone(),
                                    request_id: request_id.clone(),
                                    reason: if text.is_empty() { None } else { Some(text) },
                                })
                                .await;
                            app.clear_approval();
                        }
                        _ => {
                            app.input().input(tui_textarea::Input::from(key));
                        }
                    },
                    ApprovalMode::Normal => match key.code {
                        KeyCode::Tab => {
                            let input_text = app.input().lines().join("\n");
                            if input_text.starts_with('/') && input_text.chars().count() == 1 {
                                app.toggle_command_palette();
                            } else if let Some(agent_name) = app.cycle_primary_profile() {
                                let _ = holly
                                    .send(entanglement_core::InMsg::SetAgent {
                                        session: app.active_session_id().clone(),
                                        agent: agent_name,
                                    })
                                    .await;
                            }
                        }
                        KeyCode::Char('a') if key.modifiers == KeyModifiers::CONTROL => {
                            app.toggle_profile_picker();
                        }
                        KeyCode::Char('p') if key.modifiers == KeyModifiers::CONTROL => {
                            app.toggle_command_palette();
                        }
                        KeyCode::Char('q') | KeyCode::Char('c')
                            if key.modifiers == KeyModifiers::CONTROL =>
                        {
                            return Ok(true);
                        }
                        KeyCode::PageUp => {
                            app.scroll_up(5);
                        }
                        KeyCode::PageDown => {
                            app.scroll_down(5);
                        }
                        KeyCode::End => {
                            app.scroll_to_bottom();
                        }
                        KeyCode::Enter => {
                            if key.modifiers.contains(KeyModifiers::SHIFT) {
                                app.input().insert_newline();
                            } else {
                                let text = app.take_input_text();
                                if !text.is_empty() {
                                    if text.starts_with('/') {
                                        if let Some(cmd) =
                                            crate::tui::commands::parse_command(&text)
                                        {
                                            if app.execute_command(cmd) {
                                                return Ok(true);
                                            }
                                            return Ok(false);
                                        }
                                    }
                                    app.note_prompt_sent();
                                    if let Err(e) = holly
                                        .send(InMsg::Prompt {
                                            session: app.active_session_id().clone(),
                                            text,
                                        })
                                        .await
                                    {
                                        debug!("Failed to send prompt: {}", e);
                                    }
                                }
                            }
                        }
                        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            app.input().insert_newline();
                        }
                        KeyCode::Up => {
                            if app.input().cursor().0 == 0 && app.input().cursor().1 == 0 {
                                app.history_up();
                            } else {
                                app.input().move_cursor(tui_textarea::CursorMove::Up);
                            }
                        }
                        KeyCode::Down => {
                            let input = app.input();
                            let input_lines = input.lines();
                            let cursor = input.cursor();
                            let is_at_end = cursor.0 == input_lines.len().saturating_sub(1)
                                && cursor.1
                                    == input_lines
                                        .last()
                                        .map(|l: &String| l.chars().count())
                                        .unwrap_or(0);

                            if is_at_end && app.history_index().is_some() {
                                app.history_down();
                            } else {
                                app.input().move_cursor(tui_textarea::CursorMove::Down);
                            }
                        }
                        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if !app.handle_readline_key(c) {
                                app.input().input(tui_textarea::Input::from(key));
                            }
                        }
                        _ => {
                            app.input().input(tui_textarea::Input::from(key));
                        }
                    },
                }
            }
        }
        Event::Mouse(mouse_event) => match mouse_event.kind {
            crossterm::event::MouseEventKind::ScrollUp => {
                app.scroll_up(3);
            }
            crossterm::event::MouseEventKind::ScrollDown => {
                app.scroll_down(3);
            }
            _ => {}
        },
        Event::Resize => {}
        Event::FocusGained | Event::FocusLost => {}
        Event::Paste(s) => {
            if matches!(app.approval_mode(), ApprovalMode::Normal) {
                app.input().insert_str(&s);
            }
        }
    }
    Ok(false)
}

async fn handle_profile_picker_event(app: &mut App, holly: &Holly, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_profile_picker();
        }
        KeyCode::Enter => {
            if let Some(agent_name) = app.select_profile_picker() {
                let _ = holly
                    .send(entanglement_core::InMsg::SetAgent {
                        session: app.active_session_id().clone(),
                        agent: agent_name,
                    })
                    .await;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.profile_picker_next();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.profile_picker_prev();
        }
        KeyCode::Char('q') | KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        _ => {}
    }
    Ok(false)
}

async fn handle_model_picker_event(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_model_picker();
        }
        KeyCode::Enter => {
            app.close_model_picker();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.model_picker_next();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.model_picker_prev();
        }
        KeyCode::Char('q') | KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        _ => {}
    }
    Ok(false)
}

async fn handle_sessions_modal_event(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_sessions_modal();
        }
        KeyCode::Enter => {
            app.select_session_from_modal();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.sessions_modal_next();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.sessions_modal_prev();
        }
        KeyCode::Char('n') => {
            app.create_session();
            app.close_sessions_modal();
        }
        KeyCode::Char('q') | KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        _ => {}
    }
    Ok(false)
}

async fn handle_command_palette_event(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_command_palette();
        }
        KeyCode::Enter => {
            if let Some(cmd) = app.command_palette().execute_selected() {
                if app.execute_command(cmd) {
                    return Ok(true);
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.command_palette().select_next();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.command_palette().select_prev();
        }
        KeyCode::Char('q') | KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        KeyCode::Char(c) => {
            let mut query = app.command_palette().query().to_string();
            query.push(c);
            app.command_palette().set_query(query);
        }
        KeyCode::Backspace => {
            let mut query = app.command_palette().query().to_string();
            query.pop();
            app.command_palette().set_query(query);
        }
        _ => {}
    }
    Ok(false)
}

fn setup_panic_handler() {
    std::panic::set_hook(Box::new(|_| {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    }));
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

async fn drain_terminal_events(
    event_rx: &mut mpsc::Receiver<Event>,
    app: &mut App,
    holly: &Holly,
) -> Result<bool> {
    while let Ok(ev) = event_rx.try_recv() {
        if handle_event(app, holly, ev).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn drain_engine_events(
    holly_sub: &mut tokio::sync::broadcast::Receiver<entanglement_core::OutEvent>,
    app: &mut App,
) {
    while let Ok(event) = holly_sub.try_recv() {
        app.handle_out_event(event);
    }
}
