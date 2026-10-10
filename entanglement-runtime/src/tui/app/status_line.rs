//! The status line's App-side state (#762): the probe's latest git facts,
//! the live-reloaded templates, and the probe wake-up.

use std::sync::Arc;

use tokio::sync::Notify;

use crate::config::statusline::StatuslineConfig;
use crate::tui::statusline::{GitInfo, StatusLineState};

use super::App;

impl App {
    pub(crate) fn status_line(&self) -> &StatusLineState {
        &self.status_line
    }

    /// A probe result landed ([`crate::tui::event::Event::StatusLine`]).
    pub(crate) fn set_status_git(&mut self, git: GitInfo) {
        self.status_line.git = git;
        self.mark_dirty();
    }

    /// Startup and every config reload.
    pub(crate) fn set_statusline_config(&mut self, config: StatuslineConfig) {
        if self.status_line.config != config {
            self.status_line.config = config;
            self.mark_dirty();
        }
    }

    pub(crate) fn set_status_refresh(&mut self, wake: Arc<Notify>) {
        self.status_line.refresh = Some(wake);
    }

    /// Ask the probe to re-read the repo now (an exec tool just finished).
    pub(crate) fn refresh_status_line(&self) {
        if let Some(wake) = &self.status_line.refresh {
            wake.notify_one();
        }
    }

    /// The focused session's narrated action (ADR-0151/0183) — `{action}`.
    pub(crate) fn status_action(&self) -> Option<&str> {
        self.sessions.active_view().action()
    }
}
