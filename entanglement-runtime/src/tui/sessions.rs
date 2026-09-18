use entanglement_core::{InMsg, OutEvent, SessionId};
use ratatui::widgets::ListState;
use std::collections::HashMap;

use crate::session_store::{LogPayload, LogRecord};
use crate::tui::session_view::SessionView;

/// Owns every `SessionView` the head has seen and which one is active.
/// Kept separate from `App` so the routing/lifecycle logic (switch, create,
/// auto-discover, sessions modal) can be unit-tested without the input/
/// profile-picker state that lives on `App`.
pub struct SessionRegistry {
    active: SessionId,
    order: Vec<SessionId>,
    views: HashMap<SessionId, SessionView>,
    showing_modal: bool,
    modal_state: ListState,
}

impl SessionRegistry {
    pub fn new(initial: SessionId) -> Self {
        let mut views = HashMap::new();
        views.insert(initial.clone(), SessionView::new());

        let mut modal_state = ListState::default();
        modal_state.select(Some(0));

        Self {
            active: initial.clone(),
            order: vec![initial],
            views,
            showing_modal: false,
            modal_state,
        }
    }

    pub fn active_id(&self) -> &SessionId {
        &self.active
    }

    pub fn active_view(&self) -> &SessionView {
        self.views
            .get(&self.active)
            .expect("active session always has a view")
    }

    pub fn active_view_mut(&mut self) -> &mut SessionView {
        self.views
            .get_mut(&self.active)
            .expect("active session always has a view")
    }

    /// Read-only access to a session's view by id, if it exists. Used by the
    /// compaction fork (ADR-0101) to read the source's agent name.
    pub fn view_for(&self, id: &SessionId) -> Option<&SessionView> {
        self.views.get(id)
    }

    /// Mutable access to a session's view by id, if it exists. Used by the
    /// compaction fork (ADR-0101) to record a notice on the source view.
    pub fn view_for_mut(&mut self, id: &SessionId) -> Option<&mut SessionView> {
        self.views.get_mut(id)
    }

    fn view_or_insert(&mut self, id: &SessionId) -> &mut SessionView {
        if !self.views.contains_key(id) {
            self.views.insert(id.clone(), SessionView::new());
            self.order.push(id.clone());
        }
        self.views.get_mut(id).expect("just inserted")
    }

    pub fn switch_to(&mut self, id: SessionId) {
        if self.views.contains_key(&id) {
            self.active = id;
        }
    }

    /// Creates a new session view head-side; the engine spawns the matching
    /// task lazily on the session's first `InMsg` (holly.rs), so nothing is
    /// sent here.
    pub fn create(&mut self) -> SessionId {
        loop {
            // Each new session is an independent, opaque v4 UUID — no
            // human-readable suffix index. A collision is astronomically
            // unlikely; the loop is a cheap belt-and-suspenders guard.
            let candidate = SessionId::new_uuid();
            if !self.views.contains_key(&candidate) {
                self.views.insert(candidate.clone(), SessionView::new());
                self.order.push(candidate.clone());
                self.switch_to(candidate.clone());
                return candidate;
            }
        }
    }

    /// Create `id`'s view if absent, leaving the active session alone. Used by
    /// the compaction hand-off (ADR-0205), which must seed the successor's
    /// transcript before deciding whether to switch to it — a *background*
    /// session compacting must not yank the user's view.
    pub fn ensure(&mut self, id: &SessionId) {
        self.view_or_insert(id);
    }

    /// Every session in **spawn-tree order** (roots in insertion order, each
    /// followed by its descendants depth-first) — the one ordering source for
    /// the sidebar, the sessions modal, and the attention panel, so a click/
    /// jump target always matches what is drawn.
    pub fn all(&self) -> Vec<(&SessionId, &SessionView)> {
        self.all_with_depth()
            .into_iter()
            .map(|(id, view, _)| (id, view))
            .collect()
    }

    /// [`all`][Self::all] plus each session's spawn-tree depth (for indent).
    pub fn all_with_depth(&self) -> Vec<(&SessionId, &SessionView, usize)> {
        let entries: Vec<(&SessionId, &SessionView)> = self
            .order
            .iter()
            .filter_map(|id| self.views.get(id).map(|v| (id, v)))
            .collect();
        let links: crate::tui::session_tree::ParentLinks = entries
            .iter()
            .map(|(id, view)| ((*id).clone(), view.parent().cloned()))
            .collect();
        let ids: Vec<&SessionId> = entries.iter().map(|(id, _)| *id).collect();
        crate::tui::session_tree::tree_order(&ids, &links)
            .into_iter()
            .map(|i| {
                let (id, view) = entries[i];
                (id, view, crate::tui::session_tree::get_depth(id, &links))
            })
            .collect()
    }

    /// The tree-ordered id list the modal's `ListState` index points into.
    fn ordered_ids(&self) -> Vec<&SessionId> {
        self.all().into_iter().map(|(id, _)| id).collect()
    }

    /// Routes an event into its session's view, auto-discovering sessions
    /// seen for the first time on the broadcast. Background sessions keep
    /// accumulating state even while another session is active, so nothing
    /// is dropped when the user switches away. Returns whether anything changed.
    pub fn handle_out_event(&mut self, event: OutEvent) -> bool {
        // A supervisor-global query reply (SessionList/History, #160) names no
        // single session — it is not a per-session view update, so it never
        // conjures a phantom view keyed by a correlation id.
        let Some(id) = event.session().cloned() else {
            return false;
        };
        self.view_or_insert(&id).apply_event(event)
    }

    pub fn showing_modal(&self) -> bool {
        self.showing_modal
    }

    pub fn toggle_modal(&mut self) {
        self.showing_modal = !self.showing_modal;
        if self.showing_modal {
            let current_index = self
                .ordered_ids()
                .iter()
                .position(|id| *id == &self.active)
                .unwrap_or(0);
            self.modal_state.select(Some(current_index));
        }
    }

    pub fn close_modal(&mut self) {
        self.showing_modal = false;
    }

    pub fn modal_state(&mut self) -> &mut ListState {
        &mut self.modal_state
    }

    pub fn modal_next(&mut self) {
        if self.order.is_empty() {
            return;
        }
        if let Some(selected) = self.modal_state.selected() {
            self.modal_state
                .select(Some((selected + 1) % self.order.len()));
        }
    }

    pub fn modal_prev(&mut self) {
        if self.order.is_empty() {
            return;
        }
        if let Some(selected) = self.modal_state.selected() {
            let prev = if selected == 0 {
                self.order.len() - 1
            } else {
                selected - 1
            };
            self.modal_state.select(Some(prev));
        }
    }

    /// Page the modal selection forward by `n`, clamping at the last session.
    /// Unlike [`modal_next`][Self::modal_next] (which wraps), a page past the
    /// end stays on the last session.
    pub fn modal_page_down(&mut self, n: usize) {
        if self.order.is_empty() {
            return;
        }
        if let Some(selected) = self.modal_state.selected() {
            let last = self.order.len() - 1;
            self.modal_state.select(Some((selected + n).min(last)));
        }
    }

    /// Page the modal selection backward by `n`, clamping at the first session.
    pub fn modal_page_up(&mut self, n: usize) {
        if self.order.is_empty() {
            return;
        }
        if let Some(selected) = self.modal_state.selected() {
            self.modal_state.select(Some(selected.saturating_sub(n)));
        }
    }

    /// Rebuilds a `SessionView` from persisted log records and switches to it,
    /// restoring the full visible transcript of a resumed session. The view is
    /// built fresh (seq-dedupe starts at 0) by folding `In(Prompt)` records as
    /// user messages and `Out` events through the normal `apply_event` path — the
    /// same reducers a live session uses.
    pub fn restore_from_records(&mut self, id: SessionId, records: &[LogRecord]) {
        // A root's log interleaves its whole spawn sub-tree, so each record
        // folds into *its own* session's view — the same routing live events
        // get. Folding them all into the root mixed sub-agents' streams into
        // its transcript and ran their independent `seq` counters through
        // one view's dedupe guard, dropping events.
        let mut views: Vec<(SessionId, SessionView)> = vec![(id.clone(), SessionView::new())];
        for record in records {
            let slot = match views.iter().position(|(s, _)| s == &record.session) {
                Some(i) => i,
                None => {
                    views.push((record.session.clone(), SessionView::new()));
                    views.len() - 1
                }
            };
            let view = &mut views[slot].1;
            match &record.payload {
                LogPayload::In(InMsg::Prompt { content, .. }) => {
                    view.record_user_message(entanglement_core::content_text(content));
                }
                LogPayload::In(_) => {}
                LogPayload::Out(event) => {
                    view.apply_event(event.clone());
                }
                // A gap tombstone carries no transcript content. Resume refuses a
                // gapped log upstream, so this only guards a stray restore.
                LogPayload::Gap { .. } => {}
            }
        }

        for (session, view) in views {
            if !self.order.contains(&session) {
                self.order.push(session.clone());
            }
            self.views.insert(session, view);
        }
        self.switch_to(id);
    }

    /// Switches to the highlighted session and closes the modal.
    pub fn select_from_modal(&mut self) {
        if let Some(id) = self.modal_selected_id() {
            self.switch_to(id);
        }
        self.showing_modal = false;
    }

    /// The session id highlighted in the open modal, if any — used by the
    /// sessions-modal quick keys (#6) to act on the highlighted session.
    /// Indexes the same tree order [`all`][Self::all] renders.
    pub fn modal_selected_id(&self) -> Option<SessionId> {
        self.modal_state
            .selected()
            .and_then(|i| self.ordered_ids().get(i).cloned().cloned())
    }

    /// Token/cost totals for `id` plus every descendant, recursively (#560):
    /// a child spawned via `agent`/`agent_send` is a separate session, so a
    /// parent's own [`SessionView`] totals never include its fan-out spend.
    /// Head-side only — no protocol change, just folding the already-
    /// broadcast `OutEvent::Usage` totals this process already accumulated
    /// per session, walking the same parent links the sessions modal orders
    /// by.
    pub fn usage_rollup(&self, id: &SessionId) -> UsageRollup {
        let mut total = self
            .views
            .get(id)
            .map(UsageRollup::from_view)
            .unwrap_or_default();
        for (child_id, view) in &self.views {
            if view.parent() == Some(id) {
                total = total.merge(self.usage_rollup(child_id));
            }
        }
        total
    }
}

/// Rolled-up usage for a session plus every descendant — see
/// [`SessionRegistry::usage_rollup`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageRollup {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    /// `Some` only when every contributing session (that has any tokens)
    /// carried catalog pricing — a partial sum that silently dropped an
    /// unpriced session's cost would understate the real total, so the whole
    /// rollup falls back to token-only display instead (#560).
    pub cost_usd: Option<f64>,
    /// Sessions folded in (own + descendants) — `/cost` names the child count.
    pub sessions: usize,
}

impl UsageRollup {
    fn from_view(view: &SessionView) -> Self {
        let priced = view.cost_known() || (view.input_tokens() == 0 && view.output_tokens() == 0);
        Self {
            input_tokens: view.input_tokens(),
            output_tokens: view.output_tokens(),
            cached_input_tokens: view.cached_input_tokens(),
            cost_usd: priced.then(|| view.cost_usd()),
            sessions: 1,
        }
    }

    fn merge(mut self, other: Self) -> Self {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.sessions += other.sessions;
        self.cost_usd = match (self.cost_usd, other.cost_usd) {
            (Some(a), Some(b)) => Some(a + b),
            _ => None,
        };
        self
    }
}

#[cfg(test)]
mod tests;
