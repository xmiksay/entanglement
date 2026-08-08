use crossterm::event::{self, Event as CEvent, KeyEvent, MouseEvent};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize,
    FocusGained,
    FocusLost,
    Paste(String),
    /// An external `SIGINT` (`kill -INT`, or a Ctrl+C from a terminal that
    /// ignores crossterm's keyboard-enhancement flags). In raw mode Ctrl+C
    /// arrives as a key event (ISIG suppressed), so this only fires for a
    /// true out-of-band signal — routed through the same two-stage quit path
    /// (`App::handle_quit_key`) so the terminal is always restored
    /// (ADR-0087).
    Interrupt,
    /// The background-built `@file` completion index (#678) — the walk runs
    /// off the startup critical path so the first draw never waits on it.
    FileIndexReady(crate::tui::mention::FileIndex),
}

pub fn spawn_crossterm_task(tx: tokio::sync::mpsc::Sender<Event>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match read().await {
                Ok(ev) => {
                    if tx.send(ev).await.is_err() {
                        break;
                    }
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    })
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
/// The returned handle must be aborted — and `reset_sigint_to_default` called
/// — after `restore_terminal`: `tokio::signal::ctrl_c()` installs a
/// process-global handler that outlives the future, so without the reset a
/// Ctrl+C during `main`'s post-TUI shutdown is swallowed (forcing `kill -9`).
pub fn spawn_sigint_task(tx: tokio::sync::mpsc::Sender<Event>) -> tokio::task::JoinHandle<()> {
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

pub async fn read() -> Result<Event, std::io::Error> {
    tokio::task::spawn_blocking(move || match event::poll(Duration::from_millis(50)) {
        Ok(true) => match event::read() {
            Ok(ev) => match ev {
                CEvent::Key(k) => Ok(Event::Key(k)),
                CEvent::Mouse(m) => Ok(Event::Mouse(m)),
                CEvent::Resize(_, _) => Ok(Event::Resize),
                CEvent::FocusGained => Ok(Event::FocusGained),
                CEvent::FocusLost => Ok(Event::FocusLost),
                CEvent::Paste(s) => Ok(Event::Paste(s)),
            },
            Err(e) => Err(e),
        },
        Ok(false) => Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "timeout",
        )),
        Err(e) => Err(e),
    })
    .await?
}
