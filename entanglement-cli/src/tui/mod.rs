mod app;
mod event;
mod ui;

use anyhow::Result;
use entanglement_core::{Holly, InMsg, SessionId};
use ratatui::{
    backend::CrosstermBackend,
    crossterm::{
        event::{KeyCode, KeyEventKind, KeyModifiers},
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    },
    Terminal,
};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::debug;

use app::App;
use event::Event;

pub async fn tui(holly: Holly) -> Result<()> {
    setup_panic_handler();

    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    enable_raw_mode()?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (event_tx, mut event_rx) = mpsc::channel(128);
    spawn_crossterm_task(event_tx.clone());

    let session_id = SessionId::new("tui");
    let mut app = App::new(holly.clone(), session_id);

    let mut holly_sub = holly.subscribe();

    loop {
        terminal.draw(|f| ui::draw(f, &mut app))?;

        tokio::select! {
            Some(ev) = event_rx.recv() => {
                debug!("Received event: {:?}", ev);
                if handle_event(&mut app, &holly, ev).await? {
                    break;
                }
            }
            result = tokio::time::timeout(Duration::from_millis(50), holly_sub.recv()) => {
                match result {
                    Ok(Ok(event)) => {
                        debug!("Received Holly event: {:?}", event);
                        if event.session() == app.session_id() {
                            app.handle_out_event(event);
                        }
                    }
                    Ok(Err(_)) => {
                        debug!("Holly subscription closed");
                        break;
                    }
                    Err(_) => {
                    }
                }
            }
        }
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
    match ev {
        Event::Key(key) => {
            if key.kind == KeyEventKind::Press {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Char('c')
                        if key.modifiers == KeyModifiers::CONTROL =>
                    {
                        return Ok(true);
                    }
                    KeyCode::Char('q') => return Ok(true),
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
                                if let Err(e) = holly
                                    .send(InMsg::Prompt {
                                        session: app.session_id().clone(),
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
            app.input().insert_str(&s);
        }
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
