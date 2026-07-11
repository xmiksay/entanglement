mod app;
mod attention;
mod commands;
mod diff;
mod editor;
mod event;
mod export;
mod input;
mod input_panel;
mod keybindings;
mod markdown;
mod mention;
mod modals;
mod progress;
mod session_view;
mod sessions;
mod theme;
mod tool_render;
mod transcript;
mod ui;
mod wrap;

use anyhow::Result;
use entanglement_core::{AgentMode, Holly, InMsg, ProfileRegistry, SessionId};
use ratatui::{
    backend::CrosstermBackend,
    crossterm::{
        event::{
            DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture,
            KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
            MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
        },
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    },
    Terminal,
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::debug;

use crate::ModelInfo;
use app::App;
use attention::Attention;
use entanglement_provider::Catalog;
use event::Event;
use session_view::ApprovalMode;

pub async fn tui(
    holly: &Holly,
    initial_session: SessionId,
    model_info: ModelInfo,
    catalog: Catalog,
    profiles: ProfileRegistry,
    root: std::path::PathBuf,
    bash_enabled: bool,
) -> Result<()> {
    setup_panic_handler();

    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    enable_raw_mode()?;
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        )
    );
    // Mouse capture lets the wheel scroll the chat and blocks become clickable.
    // The trade-off is losing native text selection (use Shift+drag), so allow
    // opting out via `ENTANGLEMENT_TUI_NO_MOUSE`.
    if editor::mouse_capture_enabled() {
        let _ = execute!(stdout, EnableMouseCapture);
    }
    // Focus reporting lets attention signals mute while the terminal is focused
    // (issue #14). Best-effort: many terminals never report it, and we default to
    // signalling in that case.
    let _ = execute!(stdout, EnableFocusChange);
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (event_tx, mut event_rx) = mpsc::channel(128);
    spawn_crossterm_task(event_tx.clone());

    // Registry-driven entry-agent roster (#119): the `/agent` picker and
    // Tab-cycle offer only entry agents (`mode ∈ {primary, all}`) — a `subagent`
    // leaf like `explore` is a spawn target, never a manual entry agent. Ordered
    // by the registry's stable `iter` (name-sorted).
    let entry_profiles: Vec<app::ProfileInfo> = profiles
        .iter()
        .filter(|p| matches!(p.mode, AgentMode::Primary | AgentMode::All))
        .map(|p| app::ProfileInfo {
            name: p.name.clone(),
            description: p.description.clone(),
        })
        .collect();
    let mut app = App::new(initial_session, catalog, entry_profiles);
    app.set_model_info(model_info);
    app.init_head_context(root, bash_enabled);

    let mut attention = Attention::from_env();
    let mut holly_sub = holly.subscribe();

    const FRAME_INTERVAL: Duration = Duration::from_millis(33);
    let mut last_draw = Instant::now();

    loop {
        app.tick_thinking();
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
                if handle_event(&mut app, holly, &mut attention, ev).await? {
                    break;
                }
            }
            recv = tokio::time::timeout(Duration::from_millis(50), holly_sub.recv()) => match recv {
                Ok(Ok(event)) => dispatch_engine_event(&mut app, &mut attention, event),
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                    tracing::warn!("TUI lagged, skipped {n} engine events");
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                Err(_) => {}
            }
        }

        if drain_terminal_events(&mut event_rx, &mut app, holly, &mut attention).await? {
            break;
        }
        drain_engine_events(&mut holly_sub, &mut app, &mut attention);

        // A command/action may have requested a terminal-owning effect (open
        // `$EDITOR`, export). Run it here — the loop owns the `Terminal` — and
        // keep the session alive on failure rather than propagating.
        if let Some(effect) = app.take_pending_effect() {
            if let Err(e) = editor::run_effect(&mut terminal, &mut app, effect) {
                tracing::error!("external editor / export failed: {e:#}");
            }
            app.mark_dirty();
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

async fn handle_event(
    app: &mut App,
    holly: &Holly,
    attention: &mut Attention,
    ev: Event,
) -> Result<bool> {
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
                if app.showing_resume_modal() {
                    return handle_resume_modal_event(app, holly, key).await;
                }
                // A model-driven `ask_user` question takes over input until
                // answered (ADR-0027), just like an approval prompt.
                if app.is_asking() {
                    return handle_question_event(app, holly, key).await;
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
                        }
                        KeyCode::Char('e') => {
                            app.set_approval_mode(ApprovalMode::EnteringRejectReason {
                                request_id: request_id.clone(),
                            });
                        }
                        KeyCode::Esc => {
                            let _ = holly
                                .send(InMsg::Stop {
                                    session: app.active_session_id().clone(),
                                })
                                .await;
                            app.clear_approval();
                        }
                        _ => {}
                    },
                    ApprovalMode::EnteringRejectReason { request_id } => match key.code {
                        KeyCode::Esc => {
                            app.set_approval_mode(ApprovalMode::WaitingForApproval {
                                request_id: request_id.clone(),
                            });
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
                        KeyCode::Char(c) => {
                            app.input().insert_char(c);
                        }
                        KeyCode::Backspace => {
                            app.input().delete_char();
                        }
                        KeyCode::Left => {
                            app.input().move_cursor_left();
                        }
                        KeyCode::Right => {
                            app.input().move_cursor_right();
                        }
                        _ => {}
                    },
                    ApprovalMode::Normal => match key.code {
                        KeyCode::Tab if app.mention_visible() => {
                            app.accept_mention();
                        }
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
                        KeyCode::Left if key.modifiers.contains(KeyModifiers::SHIFT) => {
                            app.scroll_left(10);
                        }
                        KeyCode::Right if key.modifiers.contains(KeyModifiers::SHIFT) => {
                            app.scroll_right(10);
                        }
                        KeyCode::Esc => {
                            if app.mention_visible() {
                                app.hide_mention();
                            } else if app.is_input_multiline() {
                                app.set_input_multiline(false);
                            } else {
                                return Ok(true);
                            }
                        }
                        KeyCode::Enter => {
                            if key.modifiers.contains(KeyModifiers::SHIFT) {
                                app.input().insert_newline();
                                app.update_mention();
                            } else if app.mention_visible() {
                                app.accept_mention();
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
                                    // `!bash` passthrough (ADR-0030): run head-side,
                                    // inject output locally — never sent to the engine.
                                    if let Some(cmd) = text.strip_prefix('!') {
                                        let cmd = cmd.trim().to_string();
                                        if !cmd.is_empty() {
                                            run_bash_passthrough(app, &cmd).await;
                                        }
                                        return Ok(false);
                                    }
                                    app.record_user_message(text.clone());
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
                            app.update_mention();
                        }
                        KeyCode::Up => {
                            if app.mention_visible() {
                                app.mention_select_prev();
                            } else if app.input().cursor() == (0, 0) {
                                app.history_up();
                            } else {
                                app.input().move_cursor_up();
                            }
                        }
                        KeyCode::Down => {
                            if app.mention_visible() {
                                app.mention_select_next();
                            } else if app.input().cursor() == (0, 0) {
                                app.history_down();
                            } else {
                                app.input().move_cursor_down();
                            }
                        }
                        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if !app.handle_readline_key(c) {
                                match c {
                                    'a' => app.input().move_cursor_to_head(),
                                    'e' => app.input().move_cursor_to_end(),
                                    'k' => app.input().delete_line_by_end(),
                                    'u' => app.input().delete_line_by_head(),
                                    'w' => app.input().delete_word(),
                                    _ => app.input().insert_char(c),
                                }
                            }
                            app.update_mention();
                        }
                        KeyCode::Char(c) => {
                            app.input().insert_char(c);
                            app.update_mention();
                        }
                        KeyCode::Backspace => {
                            app.input().delete_char();
                            app.update_mention();
                        }
                        KeyCode::Left => {
                            app.input().move_cursor_left();
                            app.update_mention();
                        }
                        KeyCode::Right => {
                            app.input().move_cursor_right();
                            app.update_mention();
                        }
                        _ => {}
                    },
                }
            }
        }
        Event::Mouse(mouse_event) => handle_mouse(app, mouse_event),
        Event::Resize => {}
        Event::FocusGained => attention.set_focused(true),
        Event::FocusLost => attention.set_focused(false),
        Event::Paste(s) => {
            if matches!(app.approval_mode(), ApprovalMode::Normal) {
                app.input().insert_str(&s);
                app.update_mention();
            }
        }
    }
    Ok(false)
}

/// Runs a `!bash` passthrough command head-side and injects the output into the
/// transcript (ADR-0030). Gated on `ENTANGLEMENT_ENABLE_BASH` — the same opt-in
/// as the model-facing `bash` tool (ADR-0010), since it runs unsandboxed. When
/// disabled, a hint is recorded instead of running anything.
async fn run_bash_passthrough(app: &mut App, command: &str) {
    if !app.bash_enabled() {
        app.record_bash_passthrough(
            command.to_string(),
            "[bash passthrough disabled] set ENTANGLEMENT_ENABLE_BASH=1 to run `!` commands"
                .to_string(),
        );
        return;
    }
    use entanglement_core::tools::Tool;
    let tool = crate::host::bash::BashTool::new(app.root().to_path_buf());
    let input = serde_json::json!({ "command": command }).to_string();
    let output = match tool.run(&input).await {
        Ok(out) => out,
        Err(e) => format!("[bash error] {e:#}"),
    };
    app.record_bash_passthrough(command.to_string(), output);
}

/// Routes a mouse event. The wheel prefers an open modal's selection (mirroring
/// `j`/`k`), else scrolls the chat transcript; a left click hit-tests the chat
/// area and toggles the reasoning block it lands on.
fn handle_mouse(app: &mut App, ev: MouseEvent) {
    match ev.kind {
        MouseEventKind::ScrollUp => {
            if !wheel_modal_prev(app) {
                app.scroll_up(3);
            }
        }
        MouseEventKind::ScrollDown => {
            if !wheel_modal_next(app) {
                app.scroll_down(3);
            }
        }
        MouseEventKind::Down(MouseButton::Left) if !any_modal_open(app) => {
            if let Some(id) = app.reasoning_block_at(ev.column, ev.row) {
                app.toggle_reasoning_block(id);
            }
        }
        _ => {}
    }
}

fn any_modal_open(app: &App) -> bool {
    app.showing_sessions_modal()
        || app.showing_profile_picker()
        || app.showing_model_picker()
        || app.showing_command_palette()
        || app.showing_resume_modal()
        || app.showing_help()
}

/// Moves the open modal's selection forward for a wheel-down; returns whether a
/// modal consumed the event (so the chat isn't scrolled underneath it).
fn wheel_modal_next(app: &mut App) -> bool {
    if app.showing_sessions_modal() {
        app.sessions_modal_next();
    } else if app.showing_profile_picker() {
        app.profile_picker_next();
    } else if app.showing_model_picker() {
        app.model_picker_next();
    } else if app.showing_command_palette() {
        app.command_palette().select_next();
    } else if app.showing_resume_modal() {
        app.resume_next();
    } else if app.showing_help() {
        // Consume without acting — the help dialog has no selection.
    } else {
        return false;
    }
    true
}

fn wheel_modal_prev(app: &mut App) -> bool {
    if app.showing_sessions_modal() {
        app.sessions_modal_prev();
    } else if app.showing_profile_picker() {
        app.profile_picker_prev();
    } else if app.showing_model_picker() {
        app.model_picker_prev();
    } else if app.showing_command_palette() {
        app.command_palette().select_prev();
    } else if app.showing_resume_modal() {
        app.resume_prev();
    } else if app.showing_help() {
    } else {
        return false;
    }
    true
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

/// Drives the resume modal: navigate the past-session list and, on Enter,
/// restore the picked session's full transcript into a fresh view and reseed the
/// engine's context from the same log (`Holly::resume`). Read/resume failures are
/// logged, not fatal — the modal simply closes.
async fn handle_resume_modal_event(app: &mut App, holly: &Holly, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_resume_modal();
        }
        KeyCode::Enter => {
            if let Some(meta) = app.selected_resume_session() {
                let id = meta.id.clone();
                let cwd = std::env::current_dir().unwrap_or_default();
                match crate::session_store::read(&cwd, &id) {
                    Ok(records) => {
                        // A gap tombstone means the log lost a contiguous run of
                        // events (#104); replaying it would silently rebuild a
                        // wrong context, so refuse rather than resume.
                        if let Some(dropped) = crate::session_store::integrity_gap(&records) {
                            tracing::error!(
                                "Refusing to resume session {}: log is missing {} dropped record(s)",
                                id,
                                dropped
                            );
                        } else {
                            // Visible transcript first, then engine context.
                            app.restore_session(id.clone(), &records);
                            let paired = crate::session_store::pair_records(&records);
                            if let Err(e) = holly.resume(id.clone(), paired).await {
                                tracing::error!("Failed to resume session {}: {}", id, e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to read session {}: {}", id, e);
                    }
                }
            }
            app.close_resume_modal();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.resume_next();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.resume_prev();
        }
        KeyCode::Char('q') | KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        _ => {}
    }
    Ok(false)
}

/// Drive a pending `ask_user` question (ADR-0027): arrow/number selection over
/// the labelled options plus an "Other" entry that opens the shared input box
/// for a free-text answer. The picked label or typed text returns as
/// [`InMsg::AnswerQuestion`]; `Esc` interrupts the turn like an approval.
async fn handle_question_event(app: &mut App, holly: &Holly, key: KeyEvent) -> Result<bool> {
    let Some(q) = app.pending_question() else {
        return Ok(false);
    };
    let request_id = q.request_id.clone();
    let entering = q.entering_free_form;
    let free_form_selected = q.free_form_selected();
    let selected_label = q.options.get(q.selected).map(|o| o.label.clone());

    let session = app.active_session_id().clone();
    let answer = |text: String| InMsg::AnswerQuestion {
        session: session.clone(),
        request_id: request_id.clone(),
        answer: text,
    };

    if entering {
        match key.code {
            KeyCode::Esc => {
                let _ = app.take_input_text();
                app.question_cancel_free_form();
            }
            KeyCode::Enter => {
                let text = app.take_input_text();
                if !text.is_empty() {
                    let _ = holly.send(answer(text)).await;
                    app.clear_question();
                }
            }
            KeyCode::Char(c) => app.input().insert_char(c),
            KeyCode::Backspace => app.input().delete_char(),
            KeyCode::Left => app.input().move_cursor_left(),
            KeyCode::Right => app.input().move_cursor_right(),
            _ => {}
        }
        return Ok(false);
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        KeyCode::Up | KeyCode::Char('k') => app.question_move(-1),
        KeyCode::Down | KeyCode::Char('j') => app.question_move(1),
        // Quick-pick by number: options are 1-based; the "Other" entry follows.
        KeyCode::Char(c @ '1'..='9') => {
            let idx = (c as u8 - b'1') as usize;
            let (opt_count, allow_free_form) = app
                .pending_question()
                .map(|q| (q.options.len(), q.allow_free_form))
                .unwrap_or((0, false));
            if idx < opt_count {
                if let Some(label) = app
                    .pending_question()
                    .and_then(|q| q.options.get(idx).map(|o| o.label.clone()))
                {
                    let _ = holly.send(answer(label)).await;
                    app.clear_question();
                }
            } else if allow_free_form && idx == opt_count {
                app.question_begin_free_form();
            }
        }
        KeyCode::Enter => {
            if free_form_selected {
                app.question_begin_free_form();
            } else if let Some(label) = selected_label {
                let _ = holly.send(answer(label)).await;
                app.clear_question();
            }
        }
        KeyCode::Esc => {
            let _ = holly.send(InMsg::Stop { session }).await;
            app.clear_question();
        }
        _ => {}
    }
    Ok(false)
}

fn setup_panic_handler() {
    std::panic::set_hook(Box::new(|_| {
        let _ = disable_raw_mode();
        // Disable mouse capture unconditionally — harmless if it was never
        // enabled — so a crash never leaves the terminal eating mouse input.
        let _ = execute!(
            std::io::stdout(),
            DisableMouseCapture,
            DisableFocusChange,
            LeaveAlternateScreen,
            PopKeyboardEnhancementFlags
        );
    }));
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    let _ = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        DisableFocusChange,
        LeaveAlternateScreen,
        PopKeyboardEnhancementFlags
    );
    terminal.show_cursor()?;
    Ok(())
}

async fn drain_terminal_events(
    event_rx: &mut mpsc::Receiver<Event>,
    app: &mut App,
    holly: &Holly,
    attention: &mut Attention,
) -> Result<bool> {
    while let Ok(ev) = event_rx.try_recv() {
        if handle_event(app, holly, attention, ev).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn drain_engine_events(
    holly_sub: &mut tokio::sync::broadcast::Receiver<entanglement_core::OutEvent>,
    app: &mut App,
    attention: &mut Attention,
) {
    while let Ok(event) = holly_sub.try_recv() {
        dispatch_engine_event(app, attention, event);
    }
}

/// Routes one engine event to the UI, first letting the attention layer ring the
/// bell / raise a desktop notification on a signal-worthy `Status` transition
/// (issue #14).
fn dispatch_engine_event(
    app: &mut App,
    attention: &mut Attention,
    event: entanglement_core::OutEvent,
) {
    attention.observe(&event, &mut std::io::stdout());
    app.handle_out_event(event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::SessionId;

    fn wheel(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        }
    }

    #[test]
    fn wheel_moves_modal_selection_not_chat() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.create_session();
        app.create_session();
        // Give the active transcript headroom so a chat scroll *would* freeze
        // auto-follow — proving the wheel didn't touch it.
        app.set_viewport_metrics(20, 5);
        app.toggle_sessions_modal();

        let before = app.sessions_modal_state().selected();
        handle_mouse(&mut app, wheel(MouseEventKind::ScrollUp));
        let after = app.sessions_modal_state().selected();

        assert_ne!(before, after, "wheel should move the modal selection");
        assert!(
            app.auto_follow(),
            "chat must not scroll while a modal is open"
        );
    }

    #[test]
    fn wheel_scrolls_chat_when_no_modal_open() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.set_viewport_metrics(20, 5);
        handle_mouse(&mut app, wheel(MouseEventKind::ScrollUp));
        assert!(
            !app.auto_follow(),
            "wheel up should scroll (freeze) the chat when no modal is open"
        );
    }
}
