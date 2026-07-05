use entanglement_core::{Holly, OutEvent, SessionId};
use tracing::debug;

pub struct App {
    _holly: Holly,
    session_id: SessionId,
    dirty: bool,
}

impl App {
    pub fn new(holly: Holly, session_id: SessionId) -> Self {
        Self {
            _holly: holly,
            session_id,
            dirty: true,
        }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn handle_out_event(&mut self, event: OutEvent) {
        debug!("App handling OutEvent: {:?}", event);
        self.mark_dirty();
    }
}
