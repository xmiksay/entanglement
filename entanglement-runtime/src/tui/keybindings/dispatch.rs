use super::{Action, Key, KeyMap, KeySequence};
use crossterm::event::{KeyCode, KeyEvent};

#[derive(Debug, Clone, PartialEq)]
pub enum LeaderState {
    Idle,
    Pending {
        sequence: KeySequence,
        started_at: std::time::Instant,
    },
}

pub struct LeaderKeyHandler {
    state: LeaderState,
    keymap: KeyMap,
    timeout: std::time::Duration,
}

impl LeaderKeyHandler {
    pub fn new() -> Self {
        Self {
            state: LeaderState::Idle,
            keymap: KeyMap::new(),
            timeout: std::time::Duration::from_millis(2000),
        }
    }

    pub fn keymap(&self) -> &KeyMap {
        &self.keymap
    }

    pub fn state(&self) -> &LeaderState {
        &self.state
    }

    #[allow(dead_code)]
    pub fn timeout(&self) -> std::time::Duration {
        self.timeout
    }

    pub fn handle_key(&mut self, event: &KeyEvent) -> Option<Action> {
        let sequence = KeySequence::from_event(event);

        match &self.state {
            LeaderState::Idle => {
                if sequence.matches(self.keymap.leader_sequence()) {
                    self.state = LeaderState::Pending {
                        sequence: sequence.clone(),
                        started_at: std::time::Instant::now(),
                    };
                    None
                } else {
                    None
                }
            }
            LeaderState::Pending {
                sequence: pending_sequence,
                started_at,
            } => {
                if event.code == KeyCode::Esc {
                    self.state = LeaderState::Idle;
                    return None;
                }

                let extended = pending_sequence.extend_with(match event.code {
                    KeyCode::Char(c) => Key::Char(c),
                    code => Key::Code(code),
                });

                if let Some(action) = self.keymap.get(&extended) {
                    self.state = LeaderState::Idle;
                    Some(action.clone())
                } else {
                    let candidates = self.keymap.get_candidates(&extended);
                    if candidates.is_empty() {
                        self.state = LeaderState::Idle;
                    } else {
                        self.state = LeaderState::Pending {
                            sequence: extended,
                            started_at: *started_at,
                        };
                    }
                    None
                }
            }
        }
    }

    pub fn check_timeout(&mut self) -> bool {
        if let LeaderState::Pending { started_at, .. } = &self.state {
            if started_at.elapsed() > self.timeout {
                self.state = LeaderState::Idle;
                return true;
            }
        }
        false
    }

    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.state = LeaderState::Idle;
    }
}

impl Default for LeaderKeyHandler {
    fn default() -> Self {
        Self::new()
    }
}
