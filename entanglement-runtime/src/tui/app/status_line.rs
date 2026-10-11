//! The status line's App-side state (#762): the probe's latest git facts,
//! the live-reloaded templates, the probe wake-up, and the `command:`
//! runner's handle and last result (part B).

use std::sync::Arc;
use std::time::Duration;

use entanglement_core::OutEvent;
use tokio::sync::Notify;

use crate::config::statusline::StatuslineConfig;
use crate::tui::statusline::command::{Job, JobTx, Outcome};
use crate::tui::statusline::{payload::payload, GitInfo, StatusLineState};

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

    /// Startup and every config reload. A changed `command` starts over:
    /// the old command's line and failure streak say nothing about the new.
    pub(crate) fn set_statusline_config(&mut self, config: StatuslineConfig) {
        if self.status_line.config == config {
            return;
        }
        if self.status_line.config.command() != config.command() {
            let state = &mut self.status_line.command;
            state.line = None;
            state.failing = false;
        }
        self.status_line.config = config;
        self.mark_dirty();
    }

    pub(crate) fn set_status_refresh(&mut self, wake: Arc<Notify>) {
        self.status_line.refresh = Some(wake);
    }

    pub(crate) fn set_status_command_runner(&mut self, jobs: JobTx) {
        self.status_line.command.jobs = Some(jobs);
    }

    /// Ask the probe to re-read the repo now (an exec tool just finished),
    /// and re-run the status command even if its payload is unchanged — the
    /// script may read state the payload doesn't carry.
    pub(crate) fn refresh_status_line(&self) {
        if let Some(wake) = &self.status_line.refresh {
            wake.notify_one();
        }
        if let Some(jobs) = &self.status_line.command.jobs {
            jobs.send_modify(|_| {});
        }
    }

    /// Publish the current job; the runner wakes only when it differs from
    /// the last one, so calling this every dirty frame is cheap.
    pub(crate) fn sync_status_command(&self) {
        let Some(jobs) = &self.status_line.command.jobs else {
            return;
        };
        let config = &self.status_line.config;
        let job = config.command().map(|command| Job {
            command: command.to_string(),
            timeout: Duration::from_millis(config.timeout_ms),
            payload: payload(self),
        });
        jobs.send_if_modified(|current| {
            let changed = *current != job;
            if changed {
                *current = job;
            }
            changed
        });
    }

    /// A run finished. Success replaces the line; a failure shows the
    /// template again, toasting once per failure streak rather than once a
    /// second.
    pub(crate) fn on_status_command(&mut self, outcome: Outcome) {
        if self.status_line.config.command() != Some(outcome.command.as_str()) {
            return; // a reload replaced the command mid-run
        }
        let state = &mut self.status_line.command;
        match outcome.result {
            Ok(line) => {
                state.line = Some(line);
                state.failing = false;
            }
            Err(reason) => {
                state.line = None;
                if !state.failing {
                    state.failing = true;
                    tracing::warn!(command = %outcome.command, "status line command: {reason}");
                    self.set_toast(format!(
                        "status line command failed ({reason}); showing the template"
                    ));
                }
            }
        }
        self.mark_dirty();
    }

    /// Fold a session-log outage transition (#767): the status-line badge
    /// stays up while any root's log is down, and each transition leaves one
    /// transcript line with the detail. Other events are ignored.
    pub(crate) fn handle_persistence(&mut self, event: &OutEvent) {
        let outages = &mut self.status_line.log_outages;
        let (root, message) = match event {
            OutEvent::PersistenceDegraded { root, error } if outages.insert(root.clone()) => (
                root,
                format!(
                    "session log {root} is not being written ({error}); the session continues, \
                     but its log now has a gap"
                ),
            ),
            OutEvent::PersistenceRecovered { root, lost } if outages.remove(root) => (
                root,
                format!("session log {root} recovered; {lost} record(s) lost"),
            ),
            // WHY: a root that is gone writes no more records, so its gap
            // tombstone only lands on another root's append — or never. The
            // live badge is about sessions still running; the note keeps it.
            OutEvent::SessionEnded { session, .. }
            | OutEvent::SessionHibernated { session, .. }
                if outages.remove(session) =>
            {
                (
                    session,
                    format!(
                        "session {session} stopped while its log was not being written; \
                         the log may end without a gap marker"
                    ),
                )
            }
            _ => return,
        };
        // The root's own transcript if the TUI holds one, else the focused
        // one (the message names the root either way).
        let root = root.clone();
        match self.sessions.view_for_mut(&root) {
            Some(view) => view.record_status("log", message),
            None => self
                .sessions
                .active_view_mut()
                .record_status("log", message),
        }
        self.mark_dirty();
    }

    /// The engine stream lagged: a `PersistenceRecovered` may be among the
    /// missed events, and there is no query to re-derive the state from, so
    /// a raised badge is cleared rather than left possibly stuck.
    pub(crate) fn engine_events_lagged(&mut self) {
        if self.status_line.log_outages.is_empty() {
            return;
        }
        self.status_line.log_outages.clear();
        self.sessions.active_view_mut().record_status(
            "log",
            "missed engine events: the ⚠ log badge may have been stale and was cleared; \
             see the tracing log for persistence errors"
                .to_string(),
        );
        self.mark_dirty();
    }

    /// The focused session's narrated action (ADR-0151/0183) — `{action}`.
    pub(crate) fn status_action(&self) -> Option<&str> {
        self.sessions.active_view().action()
    }
}
