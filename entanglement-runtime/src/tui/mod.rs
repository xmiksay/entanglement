mod app;
mod attention;
mod commands;
mod diff;
mod editor;
mod event;
mod event_loop;
mod export;
mod input;
mod input_panel;
mod key_dialog;
mod keybindings;
mod markdown;
mod mention;
mod modal_events;
mod modals;
mod progress;
mod session_view;
mod sessions;
mod theme;
mod tool_render;
mod tools_dialog;
mod transcript;
mod ui;
mod wrap;

use anyhow::Result;
use entanglement_core::{AgentMode, Holly, ProfileRegistry, SessionId};
use ratatui::{
    backend::CrosstermBackend,
    crossterm::{
        event::{
            DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture,
            KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
        },
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    },
    Terminal,
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::ModelInfo;
use app::App;
use attention::Attention;
use entanglement_provider::Catalog;
use event::Event;
use event_loop::handle_event;

#[allow(clippy::too_many_arguments)]
pub async fn tui(
    holly: &Holly,
    initial_session: SessionId,
    model_info: ModelInfo,
    provider_name: String,
    catalog: Catalog,
    profiles: std::sync::Arc<std::sync::RwLock<ProfileRegistry>>,
    agent_models: std::sync::Arc<std::sync::Mutex<crate::config::agent_models::AgentModelStore>>,
    mut reload_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    root: std::path::PathBuf,
    bash_enabled: bool,
    tool_roster: Vec<String>,
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
    // External SIGINT safety net (ADR-0087): in raw mode Ctrl+C is delivered
    // as a key event (ISIG suppressed), so this only fires for an out-of-band
    // `kill -INT` — routing it through the same two-stage quit path so the
    // terminal is always restored instead of left "half killed". The handle is
    // aborted (and SIGINT reset to default) after `restore_terminal` so the
    // post-TUI shutdown in `main` stays interruptible by a second Ctrl+C.
    let sigint_handle = spawn_sigint_task(event_tx.clone());

    // Registry-driven entry-agent roster (#119): the `/agent` picker lists every
    // entry agent (`mode ∈ {primary, all}`) — a `subagent` leaf like `explore` is
    // a spawn target, never a manual entry agent. The mode is carried through so
    // the Tab cycle can narrow to `primary` only (#322). Ordered by the
    // registry's stable `iter` (name-sorted).
    let entry_profiles = entry_profiles_from(&profiles.read().unwrap());
    let mut app = App::new(initial_session, catalog, entry_profiles, tool_roster);
    app.set_model_info(model_info);
    app.set_active_provider(provider_name);
    app.set_agent_models(agent_models);
    app.init_head_context(root, bash_enabled);

    let mut attention = Attention::from_env();
    let mut holly_sub = holly.subscribe();

    const FRAME_INTERVAL: Duration = Duration::from_millis(33);
    let mut last_draw = Instant::now();

    loop {
        app.tick_thinking();
        // Disarm a pending two-stage quit once its window elapses (ADR-0087) so
        // the "press again" hint disappears promptly, not only on the next key.
        if app.quit_pending_expired() {
            app.clear_quit_pending();
            app.mark_dirty();
        }
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
            },
            // The definitions watcher (#329) reloaded: refresh the `/agent`
            // picker + Tab-cycle roster from the fresh registry and surface a
            // status line, matching the `/key`/`/model` status pattern.
            msg = reload_rx.recv() => {
                if let Some(notice) = msg {
                    let fresh = entry_profiles_from(&profiles.read().unwrap());
                    app.refresh_profiles(fresh);
                    app.record_reload_status(notice);
                    app.mark_dirty();
                }
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

    // Abort the external-SIGINT task and reset SIGINT to its default disposition
    // (terminate). `tokio::signal::ctrl_c()` installs a *process-global* handler
    // that outlives the future it's dropped from, so without this reset a Ctrl+C
    // during `main`'s post-TUI shutdown (engine/persistence teardown) would be
    // swallowed by tokio's lingering handler — leaving the user no escape but
    // `kill -9`. Resetting here makes that shutdown interruptible again, as it
    // was before any SIGINT handler existed.
    sigint_handle.abort();
    reset_sigint_to_default();

    Ok(())
}

/// The `/agent` picker + Tab-cycle roster derived from a [`ProfileRegistry`]
/// snapshot: every entry agent (`mode ∈ {primary, all}`) — a `subagent` leaf
/// like `explore` is a spawn target, never a manual entry agent. Shared by the
/// startup build and the definitions-watcher reload arm (#329) so both derive
/// the roster identically.
fn entry_profiles_from(registry: &ProfileRegistry) -> Vec<app::ProfileInfo> {
    registry
        .iter()
        .filter(|p| matches!(p.mode, AgentMode::Primary | AgentMode::All))
        .map(|p| app::ProfileInfo {
            name: p.name.clone(),
            description: p.description.clone(),
            mode: p.mode,
            tools: p.tools.clone(),
            disallowed_tools: p.disallowed_tools.clone(),
        })
        .collect()
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

/// External-SIGINT safety net (ADR-0087). In raw mode crossterm suppresses
/// ISIG, so an in-terminal Ctrl+C arrives as a `KeyEvent` handled by the
/// centralized intercept in `handle_event`; this task therefore only wakes for
/// a true out-of-band signal (`kill -INT`, or a terminal that ignores
/// keyboard-enhancement flags). It forwards a synthetic [`Event::Interrupt`]
/// through the same event channel, which routes through
/// `App::handle_quit_key` → `restore_terminal`, so an external signal can't
/// leave the terminal in raw mode.
///
/// The returned handle must be aborted — and [`reset_sigint_to_default`] called
/// — after `restore_terminal`: `tokio::signal::ctrl_c()` installs a
/// process-global handler that outlives the future, so without the reset a
/// Ctrl+C during `main`'s post-TUI shutdown is swallowed (forcing `kill -9`).
fn spawn_sigint_task(tx: mpsc::Sender<Event>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            // `ctrl_c()` resolves anew each call; an error means signal
            // handling is unavailable (exit quietly rather than spin).
            if tokio::signal::ctrl_c().await.is_err() {
                break;
            }
            if tx.send(Event::Interrupt).await.is_err() {
                break;
            }
        }
    })
}

/// Reset the `SIGINT` disposition to its OS default (terminate the process).
///
/// `tokio::signal::ctrl_c()` registers a process-global signal handler (via
/// `signal-hook-registry`) that persists for the lifetime of the program — it is
/// **not** removed when the `ctrl_c()` future is dropped or its task aborted.
/// After the TUI loop exits and the terminal is restored, `main` tears down the
/// engine + persistence; if that stalls, the user's Ctrl+C must terminate the
/// process. Without this reset tokio's lingering handler swallows the signal,
/// leaving `kill -9` as the only escape.
///
/// Direct `libc::signal` is the lean, dependency-clean way to undo the tokio
/// registration (`signal_hook::cleanup` would re-add a dependency). Best-effort:
/// a failure is logged, not fatal — the worst case is the pre-ADR-0087 state
/// where a stall needs `kill -9`.
fn reset_sigint_to_default() {
    // SAFETY: `SIG_DFL`/`signal` are async-signal-safe; restoring the default
    // SIGINT disposition has no preconditions. This runs single-threaded at the
    // tail of the TUI shutdown, outside any signal handler.
    #[cfg(unix)]
    unsafe {
        const SIGINT: libc::c_int = 2;
        if libc::signal(SIGINT, libc::SIG_DFL) == libc::SIG_ERR {
            tracing::warn!("failed to reset SIGINT to default disposition");
        }
    }
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
    use super::modal_events::handle_mouse;
    use super::*;
    use entanglement_core::SessionId;
    use ratatui::crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};

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
