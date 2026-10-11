//! The TUI's key map (#735): the `tui.keybindings` config
//! ([`crate::config::keybindings`]) turned into crossterm lookups. Two
//! contexts — direct chords and `leader <key>` sequences — each one action per
//! key; the help dialog and the which-key popup list whatever is in effect.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;
use std::fmt;

pub use crate::config::keybindings::KeyAction as Action;
use crate::config::keybindings::{KeyBinding, KeyChord, KeyName, ResolvedKeys};

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

    #[cfg(test)]
    pub fn ctrl(key: Key) -> Self {
        Self {
            keys: vec![(key, KeyModifiers::CONTROL)],
        }
    }

    /// A keypress, normalized like a config chord: a character never carries
    /// `SHIFT` (it is in the case) and neither does `BackTab`, whose `SHIFT`
    /// flag crossterm reports inconsistently across terminals (#322).
    pub fn from_event(event: &KeyEvent) -> Self {
        let mut mods = event.modifiers;
        let key = match event.code {
            KeyCode::Char(c) => {
                mods.remove(KeyModifiers::SHIFT);
                Key::Char(c)
            }
            KeyCode::BackTab => {
                mods.remove(KeyModifiers::SHIFT);
                Key::Code(KeyCode::BackTab)
            }
            code => Key::Code(code),
        };
        Self {
            keys: vec![(key, mods)],
        }
    }

    fn from_chord(c: &KeyChord) -> Self {
        let mut mods = KeyModifiers::empty();
        mods.set(KeyModifiers::CONTROL, c.ctrl);
        mods.set(KeyModifiers::ALT, c.alt);
        mods.set(KeyModifiers::SHIFT, c.shift);
        Self {
            keys: vec![(key_of(c.key), mods)],
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

    pub fn extend_with(&self, key: Key) -> KeySequence {
        let mut keys = self.keys.clone();
        keys.push((key, KeyModifiers::empty()));
        KeySequence { keys }
    }
}

fn key_of(name: KeyName) -> Key {
    Key::Code(match name {
        KeyName::Char(c) => return Key::Char(c),
        KeyName::Enter => KeyCode::Enter,
        KeyName::Tab => KeyCode::Tab,
        KeyName::BackTab => KeyCode::BackTab,
        KeyName::Esc => KeyCode::Esc,
        KeyName::Backspace => KeyCode::Backspace,
        KeyName::Delete => KeyCode::Delete,
        KeyName::Insert => KeyCode::Insert,
        KeyName::Home => KeyCode::Home,
        KeyName::End => KeyCode::End,
        KeyName::PageUp => KeyCode::PageUp,
        KeyName::PageDown => KeyCode::PageDown,
        KeyName::Up => KeyCode::Up,
        KeyName::Down => KeyCode::Down,
        KeyName::Left => KeyCode::Left,
        KeyName::Right => KeyCode::Right,
        KeyName::F(n) => KeyCode::F(n),
    })
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

pub struct KeyMap {
    /// Leader sequences (`leader` + one key).
    bindings: HashMap<KeySequence, Action>,
    /// Direct single-chord bindings.
    direct: HashMap<KeySequence, Action>,
    leader_sequence: KeySequence,
    /// Every binding with its display label, in help-dialog order.
    listing: Vec<(String, Action)>,
}

impl KeyMap {
    /// The built-in map.
    pub fn new() -> Self {
        Self::from_resolved(&ResolvedKeys::defaults())
    }

    pub fn from_resolved(keys: &ResolvedKeys) -> Self {
        let leader_sequence = KeySequence::from_chord(&keys.leader);
        let mut bindings = HashMap::new();
        let mut direct = HashMap::new();
        let mut listing = Vec::new();
        for (action, binding) in &keys.bindings {
            let label = match binding {
                KeyBinding::Direct(c) => {
                    direct.insert(KeySequence::from_chord(c), *action);
                    c.to_string()
                }
                KeyBinding::Leader(c) => {
                    bindings.insert(leader_sequence.extend_with(key_of(c.key)), *action);
                    format!("{} {c}", keys.leader)
                }
            };
            listing.push((label, *action));
        }
        listing.push((keys.leader.to_string(), Action::Leader));
        listing.sort_by_key(|(_, a)| (a.category(), *a));
        Self {
            bindings,
            direct,
            leader_sequence,
            listing,
        }
    }

    pub fn leader_sequence(&self) -> &KeySequence {
        &self.leader_sequence
    }

    /// The action a full leader sequence resolves to.
    pub fn get(&self, sequence: &KeySequence) -> Option<&Action> {
        self.bindings.get(sequence)
    }

    /// The action a direct keypress is bound to, if any.
    pub fn direct_action(&self, event: &KeyEvent) -> Option<Action> {
        self.direct.get(&KeySequence::from_event(event)).copied()
    }

    pub fn get_candidates(&self, prefix: &KeySequence) -> Vec<(&KeySequence, &Action)> {
        self.bindings
            .iter()
            .filter(|(seq, _)| seq.starts_with(prefix))
            .collect()
    }

    /// The leader sequences, for the which-key popup.
    pub fn leader_bindings(&self) -> Vec<(&KeySequence, &Action)> {
        let mut bindings: Vec<_> = self.bindings.iter().collect();
        bindings.sort_by_key(|(seq, action)| (action.category(), seq.to_string()));
        bindings
    }

    /// Every effective binding (label, action), grouped by category — the
    /// help dialog's listing.
    pub fn all_bindings(&self) -> &[(String, Action)] {
        &self.listing
    }

    /// The first key bound to `action`, for an inline hint.
    pub fn first_key(&self, action: Action) -> Option<&str> {
        self.listing
            .iter()
            .find(|(_, a)| *a == action)
            .map(|(label, _)| label.as_str())
    }

    /// Every key bound to `action`, joined for an inline hint.
    pub fn keys_for(&self, action: Action) -> String {
        self.listing
            .iter()
            .filter(|(_, a)| *a == action)
            .map(|(label, _)| label.as_str())
            .collect::<Vec<_>>()
            .join("/")
    }
}

impl Default for KeyMap {
    fn default() -> Self {
        Self::new()
    }
}

mod dispatch;
pub use dispatch::{LeaderKeyHandler, LeaderResult, LeaderState};

#[cfg(test)]
mod tests;
