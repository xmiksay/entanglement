use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Code(KeyCode),
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Key::Char(c) => write!(f, "{}", c),
            Key::Code(code) => match code {
                KeyCode::Esc => write!(f, "Esc"),
                KeyCode::Enter => write!(f, "Enter"),
                KeyCode::Tab => write!(f, "Tab"),
                KeyCode::Backspace => write!(f, "Backspace"),
                KeyCode::Left => write!(f, "←"),
                KeyCode::Right => write!(f, "→"),
                KeyCode::Up => write!(f, "↑"),
                KeyCode::Down => write!(f, "↓"),
                KeyCode::Home => write!(f, "Home"),
                KeyCode::End => write!(f, "End"),
                KeyCode::PageUp => write!(f, "PageUp"),
                KeyCode::PageDown => write!(f, "PageDown"),
                KeyCode::Delete => write!(f, "Delete"),
                KeyCode::Insert => write!(f, "Insert"),
                KeyCode::F(n) => write!(f, "F{}", n),
                KeyCode::Null => write!(f, "Null"),
                _ => write!(f, "{:?}", code),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeySequence {
    pub keys: Vec<(Key, KeyModifiers)>,
}

impl KeySequence {
    #[allow(dead_code)]
    pub fn single(key: Key) -> Self {
        Self {
            keys: vec![(key, KeyModifiers::empty())],
        }
    }

    pub fn ctrl(key: Key) -> Self {
        Self {
            keys: vec![(key, KeyModifiers::CONTROL)],
        }
    }

    pub fn from_event(event: &KeyEvent) -> Self {
        let key = match event.code {
            KeyCode::Char(c) => Key::Char(c),
            code => Key::Code(code),
        };
        Self {
            keys: vec![(key, event.modifiers)],
        }
    }

    pub fn starts_with(&self, prefix: &KeySequence) -> bool {
        if prefix.keys.len() > self.keys.len() {
            return false;
        }
        self.keys
            .iter()
            .zip(prefix.keys.iter())
            .all(|(a, b)| a == b)
    }

    pub fn matches(&self, other: &KeySequence) -> bool {
        self.keys == other.keys
    }
}

impl fmt::Display for KeySequence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, (key, modifiers)) in self.keys.iter().enumerate() {
            if i > 0 {
                write!(f, " ")?;
            }
            if modifiers.contains(KeyModifiers::CONTROL) {
                write!(f, "Ctrl+")?;
            }
            if modifiers.contains(KeyModifiers::ALT) {
                write!(f, "Alt+")?;
            }
            if modifiers.contains(KeyModifiers::SHIFT) {
                write!(f, "Shift+")?;
            }
            write!(f, "{}", key)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    Quit,
    NewSession,
    ListSessions,
    PickAgent,
    CycleAgent,
    PickModel,
    ToggleSidebar,
    OpenEditor,
    Export,
    Interrupt,
    #[allow(dead_code)]
    ScrollUp,
    #[allow(dead_code)]
    ScrollDown,
    ShowHelp,
    CommandPalette,
}

impl Action {
    pub fn description(&self) -> &str {
        match self {
            Action::Quit => "Quit the application",
            Action::NewSession => "Create a new session",
            Action::ListSessions => "List and switch sessions",
            Action::PickAgent => "Pick agent profile",
            Action::CycleAgent => "Cycle through agent profiles",
            Action::PickModel => "Pick model",
            Action::ToggleSidebar => "Toggle sidebar",
            Action::OpenEditor => "Open editor",
            Action::Export => "Export conversation",
            Action::Interrupt => "Interrupt current operation",
            Action::ScrollUp => "Scroll up",
            Action::ScrollDown => "Scroll down",
            Action::ShowHelp => "Show help",
            Action::CommandPalette => "Open command palette",
        }
    }

    pub fn category(&self) -> &str {
        match self {
            Action::Quit => "General",
            Action::NewSession => "Sessions",
            Action::ListSessions => "Sessions",
            Action::PickAgent => "Agent",
            Action::CycleAgent => "Agent",
            Action::PickModel => "Agent",
            Action::ToggleSidebar => "UI",
            Action::OpenEditor => "UI",
            Action::Export => "UI",
            Action::Interrupt => "General",
            Action::ScrollUp => "Navigation",
            Action::ScrollDown => "Navigation",
            Action::ShowHelp => "Help",
            Action::CommandPalette => "General",
        }
    }
}

pub struct KeyMap {
    bindings: HashMap<KeySequence, Action>,
    leader_sequence: KeySequence,
}

impl KeyMap {
    pub fn new() -> Self {
        let mut bindings = HashMap::new();

        // Leader key: Ctrl+X
        let leader = KeySequence::ctrl(Key::Char('x'));

        // Session management
        bindings.insert(
            leader.clone().extend_with(Key::Char('n')),
            Action::NewSession,
        );
        bindings.insert(
            leader.clone().extend_with(Key::Char('l')),
            Action::ListSessions,
        );

        // Agent management
        bindings.insert(
            leader.clone().extend_with(Key::Char('a')),
            Action::PickAgent,
        );
        bindings.insert(
            leader.clone().extend_with(Key::Char('A')),
            Action::CycleAgent,
        );
        bindings.insert(
            leader.clone().extend_with(Key::Char('m')),
            Action::PickModel,
        );

        // UI controls
        bindings.insert(
            leader.clone().extend_with(Key::Char('s')),
            Action::ToggleSidebar,
        );
        bindings.insert(
            leader.clone().extend_with(Key::Char('e')),
            Action::OpenEditor,
        );
        bindings.insert(leader.clone().extend_with(Key::Char('E')), Action::Export);

        // General
        bindings.insert(leader.clone().extend_with(Key::Char('q')), Action::Quit);
        bindings.insert(
            leader.clone().extend_with(Key::Char('c')),
            Action::Interrupt,
        );
        bindings.insert(
            leader.clone().extend_with(Key::Char('p')),
            Action::CommandPalette,
        );

        // Help
        bindings.insert(leader.clone().extend_with(Key::Char('?')), Action::ShowHelp);
        bindings.insert(leader.clone().extend_with(Key::Char('h')), Action::ShowHelp);

        Self {
            bindings,
            leader_sequence: leader,
        }
    }

    pub fn leader_sequence(&self) -> &KeySequence {
        &self.leader_sequence
    }

    pub fn get(&self, sequence: &KeySequence) -> Option<&Action> {
        self.bindings.get(sequence)
    }

    pub fn get_candidates(&self, prefix: &KeySequence) -> Vec<(&KeySequence, &Action)> {
        self.bindings
            .iter()
            .filter(|(seq, _)| seq.starts_with(prefix))
            .collect()
    }

    pub fn all_bindings(&self) -> Vec<(&KeySequence, &Action)> {
        let mut bindings: Vec<_> = self.bindings.iter().collect();
        bindings.sort_by_key(|(seq, action)| (action.category().to_string(), format!("{}", seq)));
        bindings
    }
}

impl KeySequence {
    pub fn extend_with(&self, key: Key) -> KeySequence {
        let mut keys = self.keys.clone();
        keys.push((key, KeyModifiers::empty()));
        KeySequence { keys }
    }
}

impl Default for KeyMap {
    fn default() -> Self {
        Self::new()
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_sequence_display() {
        let seq = KeySequence::ctrl(Key::Char('x'));
        assert_eq!(format!("{}", seq), "Ctrl+x");
    }

    #[test]
    fn test_key_sequence_starts_with() {
        let leader = KeySequence::ctrl(Key::Char('x'));
        let extended = leader.extend_with(Key::Char('q'));
        assert!(extended.starts_with(&leader));
    }

    #[test]
    fn test_keymap_quit_binding() {
        let keymap = KeyMap::new();
        let leader = KeySequence::ctrl(Key::Char('x'));
        let quit_seq = leader.extend_with(Key::Char('q'));
        assert_eq!(keymap.get(&quit_seq), Some(&Action::Quit));
    }

    #[test]
    fn test_leader_handler_pending_state() {
        let mut handler = LeaderKeyHandler::new();
        let leader_event = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);

        assert!(matches!(handler.state(), LeaderState::Idle));
        assert!(handler.handle_key(&leader_event).is_none());
        assert!(matches!(handler.state(), LeaderState::Pending { .. }));
    }

    #[test]
    fn test_leader_handler_dispatches_action() {
        let mut handler = LeaderKeyHandler::new();
        let leader_event = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        let quit_event = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty());

        handler.handle_key(&leader_event);
        let action = handler.handle_key(&quit_event);
        assert_eq!(action, Some(Action::Quit));
        assert!(matches!(handler.state(), LeaderState::Idle));
    }

    #[test]
    fn test_leader_handler_esc_cancels() {
        let mut handler = LeaderKeyHandler::new();
        let leader_event = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        let esc_event = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());

        handler.handle_key(&leader_event);
        handler.handle_key(&esc_event);
        assert!(matches!(handler.state(), LeaderState::Idle));
    }

    #[test]
    fn test_action_category() {
        assert_eq!(Action::Quit.category(), "General");
        assert_eq!(Action::NewSession.category(), "Sessions");
        assert_eq!(Action::PickAgent.category(), "Agent");
        assert_eq!(Action::ScrollUp.category(), "Navigation");
    }

    #[test]
    fn test_leader_handler_timeout() {
        let mut handler = LeaderKeyHandler::new();
        let leader_event = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);

        handler.handle_key(&leader_event);
        assert!(matches!(handler.state(), LeaderState::Pending { .. }));

        std::thread::sleep(handler.timeout() + std::time::Duration::from_millis(100));
        handler.check_timeout();
        assert!(matches!(handler.state(), LeaderState::Idle));
    }

    #[test]
    fn test_keymap_multiple_candidates() {
        let keymap = KeyMap::new();
        let leader = KeySequence::ctrl(Key::Char('x'));
        let candidates = keymap.get_candidates(&leader);
        assert!(!candidates.is_empty());
        assert!(candidates
            .iter()
            .any(|(_, action)| matches!(action, Action::Quit)));
        assert!(candidates
            .iter()
            .any(|(_, action)| matches!(action, Action::NewSession)));
    }
}
