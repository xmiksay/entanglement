//! The resume modal: listing, navigating, restoring and deleting past sessions.

use entanglement_core::SessionId;
use ratatui::widgets::ListState;

use crate::session_store::{list_sessions, LogRecord, SessionMeta};

use super::App;

impl App {
    pub fn showing_resume_modal(&self) -> bool {
        self.showing_resume_modal
    }

    pub fn resume_state(&mut self) -> &mut ListState {
        &mut self.resume_state
    }

    pub fn toggle_resume_modal(&mut self) {
        self.showing_resume_modal = !self.showing_resume_modal;
        if self.showing_resume_modal {
            // `self.root()` (the canonicalized cwd wired in at startup) is the
            // same cwd `session_store::delete`/`prune` key off, so the modal
            // lists exactly the logs a `d` press can reach.
            if let Ok(mut sessions) = list_sessions(self.root()) {
                // Only root sessions are independently resumable; spawned
                // children live inside their root's file. Most-recent first.
                sessions.retain(|s| s.root);
                sessions.sort_by_key(|s| std::cmp::Reverse(s.last_active));
                self.available_sessions = sessions;
            }
            self.resume_state
                .select(if self.available_sessions.is_empty() {
                    None
                } else {
                    Some(0)
                });
        }
        self.mark_dirty();
    }

    pub fn close_resume_modal(&mut self) {
        self.showing_resume_modal = false;
        self.mark_dirty();
    }

    /// Rebuilds and switches to a session's view from persisted records,
    /// restoring its full visible transcript.
    pub fn restore_session(&mut self, id: SessionId, records: &[LogRecord]) {
        self.sessions.restore_from_records(id, records);
        self.mark_dirty();
    }

    pub fn resume_next(&mut self) {
        if self.available_sessions.is_empty() {
            return;
        }
        if let Some(selected) = self.resume_state.selected() {
            let next = (selected + 1) % self.available_sessions.len();
            self.resume_state.select(Some(next));
        }
    }

    pub fn resume_prev(&mut self) {
        if self.available_sessions.is_empty() {
            return;
        }
        if let Some(selected) = self.resume_state.selected() {
            let prev = if selected == 0 {
                self.available_sessions.len() - 1
            } else {
                selected - 1
            };
            self.resume_state.select(Some(prev));
        }
    }

    /// Page the resume-modal selection forward by `n`, clamping at the last
    /// resumable session (no wrap).
    pub fn resume_page_down(&mut self, n: usize) {
        if self.available_sessions.is_empty() {
            return;
        }
        if let Some(selected) = self.resume_state.selected() {
            let last = self.available_sessions.len() - 1;
            self.resume_state.select(Some((selected + n).min(last)));
        }
    }

    /// Page the resume-modal selection backward by `n`, clamping at the first
    /// resumable session.
    pub fn resume_page_up(&mut self, n: usize) {
        if self.available_sessions.is_empty() {
            return;
        }
        if let Some(selected) = self.resume_state.selected() {
            self.resume_state.select(Some(selected.saturating_sub(n)));
        }
    }

    pub fn available_sessions(&self) -> &[SessionMeta] {
        &self.available_sessions
    }

    #[cfg(test)]
    pub(crate) fn set_available_sessions_for_test(&mut self, sessions: Vec<SessionMeta>) {
        self.available_sessions = sessions;
    }

    pub fn selected_resume_session(&self) -> Option<SessionMeta> {
        self.resume_state
            .selected()
            .and_then(|i| self.available_sessions.get(i).cloned())
    }

    /// Delete the session highlighted in the **sessions modal** (Issue 4, Phase
    /// 4.1). The modal lists the *live* set (sessions with an in-memory view),
    /// so this refuses a live id with a status line rather than deleting its
    /// on-disk log underneath it. A past/non-live id is deleted from disk and
    /// its view (if any still hangs around) is left alone — the live set is the
    /// source of truth, not the on-disk log. The modal stays open.
    pub fn delete_session_from_modal(&mut self) {
        let Some(id) = self.modal_selected_session_id() else {
            return;
        };
        // Live-set guard: never delete a session the engine still holds. The
        // modal's `order` is the live set (`SessionRegistry::order`).
        let is_live = self.sessions().iter().any(|(sid, _)| **sid == id);
        if is_live {
            // Toast, not a transcript line: the modal covers the transcript, so
            // the info line is the only visible feedback surface.
            self.set_toast("cannot delete a live session".to_string());
            return;
        }
        let cwd = self.root().to_path_buf();
        match crate::session_store::delete(&cwd, &id) {
            Ok(()) => self.set_toast(format!("deleted session {}", id.0)),
            Err(e) => self.set_toast(format!("could not delete session {}: {e:#}", id.0)),
        }
    }

    /// Delete the session highlighted in the **resume modal** (Issue 4, Phase
    /// 4.1). The resume modal only lists past (persisted, non-live) sessions, so
    /// `d` here always deletes — no live-set guard needed. The deleted entry is
    /// dropped from the modal's list and the selection clamped; the modal stays
    /// open so several can be deleted in a row.
    pub fn delete_resume_session(&mut self) {
        let Some(meta) = self.selected_resume_session() else {
            return;
        };
        let id = meta.id.clone();
        let cwd = self.root().to_path_buf();
        match crate::session_store::delete(&cwd, &id) {
            Ok(()) => {
                // Drop the deleted entry from the modal list and clamp the
                // selection so the highlight stays in range.
                self.available_sessions.retain(|s| s.id != id);
                if let Some(selected) = self.resume_state.selected() {
                    if selected >= self.available_sessions.len()
                        && !self.available_sessions.is_empty()
                    {
                        self.resume_state
                            .select(Some(self.available_sessions.len() - 1));
                    } else if self.available_sessions.is_empty() {
                        self.resume_state.select(None);
                    }
                }
                self.set_toast(format!("deleted session {}", id.0));
            }
            Err(e) => self.set_toast(format!("could not delete session {}: {e:#}", id.0)),
        }
    }
}
