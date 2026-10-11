//! First-run onboarding (#732, ADR-0222): when the TUI head starts with no
//! usable provider, a small standalone dialog runs **before boot** — provider
//! selection happens in `boot`, so choosing first means the engine simply
//! starts on the choice, with no live rebind. Non-TUI heads never come here;
//! they print [`crate::provider_setup::no_provider_hint`] and exit.

mod draw;
mod persist;
mod state;

use anyhow::{Context, Result};
use entanglement_provider::Catalog;
use ratatui::{
    backend::CrosstermBackend,
    crossterm::{
        event::{self, Event, KeyEventKind},
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    },
    Terminal,
};

use crate::config;
use state::{Onboarding, Outcome};

/// What `main` should do after the dialog.
pub(crate) enum Next {
    /// Boot normally (configured, or the explicit echo choice).
    Start,
    /// The user quit from the dialog.
    Quit,
}

/// Run the dialog, then persist and apply the choice to `user_config`. `Esc`
/// pins the in-memory provider to `echo` for this run only (never saved), so
/// the next start asks again.
pub(crate) async fn run(catalog: &Catalog, user_config: &mut config::Config) -> Result<Next> {
    let options = state::options(catalog);
    // The dialog blocks on terminal reads; nothing else runs yet, but keep the
    // runtime's worker threads free anyway.
    let outcome = tokio::task::spawn_blocking(move || dialog(Onboarding::new(options)))
        .await
        .context("onboarding dialog task")??;
    match outcome {
        Outcome::Configured(setup) => {
            let warnings = persist::persist(
                &setup,
                config::env_file::env_file_path(),
                config::user_config_path(),
            );
            for warning in warnings {
                eprintln!("skutter: warning: {warning}");
            }
            persist::apply(&setup, user_config);
            eprintln!(
                "skutter: configured provider={} model={}",
                setup.provider, setup.model
            );
            Ok(Next::Start)
        }
        Outcome::Echo => {
            user_config.provider = Some("echo".into());
            Ok(Next::Start)
        }
        Outcome::Quit => Ok(Next::Quit),
    }
}

/// The raw-mode loop. The terminal is restored on every exit path, including
/// an I/O error mid-dialog.
fn dialog(mut onboarding: Onboarding) -> Result<Outcome> {
    let mut stdout = std::io::stdout();
    enable_raw_mode().context("entering raw mode for the setup dialog")?;
    if let Err(e) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(e).context("entering the alternate screen");
    }
    let result = Terminal::new(CrosstermBackend::new(stdout))
        .context("opening the terminal")
        .and_then(|mut terminal| drive(&mut onboarding, &mut terminal));
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    let _ = disable_raw_mode();
    result
}

fn drive(
    onboarding: &mut Onboarding,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> Result<Outcome> {
    loop {
        terminal.draw(|f| draw::draw(f, onboarding))?;
        if let Event::Key(key) = event::read().context("reading a key")? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if let Some(outcome) = onboarding.on_key(key) {
                return Ok(outcome);
            }
        }
    }
}
