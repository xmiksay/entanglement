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

/// Outcome of feeding a key to the leader handler.
///
/// The caller must distinguish "I ate this key" (arming the leader or resolving
/// a chord) from "this key isn't mine" — otherwise arming `Ctrl+X` falls through
/// to the generic Ctrl-char arm and leaks a literal `x` into the input (#326).
#[derive(Debug, Clone, PartialEq)]
pub enum LeaderResult {
    /// A complete chord resolved to an action.
    Action(Action),
    /// The key was consumed (armed the leader, extended a chord, or cancelled) —
    /// the caller must not process it further.
    Consumed,
    /// Not a leader key; the caller handles it normally.
    NotMine,
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

    pub fn handle_key(&mut self, event: &KeyEvent) -> LeaderResult {
        let sequence = KeySequence::from_event(event);

        match &self.state {
            LeaderState::Idle => {
                if sequence.matches(self.keymap.leader_sequence()) {
                    self.state = LeaderState::Pending {
                        sequence: sequence.clone(),
                        started_at: std::time::Instant::now(),
                    };
                    LeaderResult::Consumed
                } else {
                    LeaderResult::NotMine
                }
            }
            LeaderState::Pending {
                sequence: pending_sequence,
                started_at,
            } => {
                if event.code == KeyCode::Esc {
                    self.state = LeaderState::Idle;
                    return LeaderResult::Consumed;
                }

                let extended = pending_sequence.extend_with(match event.code {
                    KeyCode::Char(c) => Key::Char(c),
                    code => Key::Code(code),
                });

                if let Some(action) = self.keymap.get(&extended) {
                    self.state = LeaderState::Idle;
                    LeaderResult::Action(action.clone())
                } else {
                    let candidates = self.keymap.get_candidates(&extended);
                    if candidates.is_empty() {
                        // Unknown chord: swallow the second key too — an invalid
                        // `Ctrl+X z` must not leak a literal `z` — and reset (#326).
                        self.state = LeaderState::Idle;
                    } else {
                        self.state = LeaderState::Pending {
                            sequence: extended,
                            started_at: *started_at,
                        };
                    }
                    LeaderResult::Consumed
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
