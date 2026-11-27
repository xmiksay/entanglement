//! Key specs for `tui.keybindings` (#735): a terminal-library-free key +
//! modifiers ([`KeyChord`]), where it fires ([`KeyBinding`]), and the spec
//! grammar — `[ctrl+][alt+][shift+]<key>` or `leader <key>`.

use std::fmt;

/// A key, independent of any terminal library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KeyName {
    Char(char),
    Enter,
    Tab,
    /// Shift+Tab — terminals report it as its own key, not a modifier.
    BackTab,
    Esc,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    F(u8),
}

/// A key plus modifiers, normalized so equal keypresses compare equal: a
/// character never carries `shift` (it is in the character's case), and
/// `shift+tab` is [`KeyName::BackTab`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyChord {
    pub key: KeyName,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl KeyChord {
    pub fn new(key: KeyName, ctrl: bool, alt: bool, shift: bool) -> Self {
        let (key, shift) = match key {
            KeyName::Char(c) if shift => (KeyName::Char(c.to_ascii_uppercase()), false),
            KeyName::Char(_) => (key, false),
            KeyName::Tab if shift => (KeyName::BackTab, false),
            KeyName::BackTab => (key, false),
            _ => (key, shift),
        };
        Self {
            key,
            ctrl,
            alt,
            shift,
        }
    }

    pub(crate) const fn plain(key: KeyName) -> Self {
        Self {
            key,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    /// Would typing this key insert text (so binding it would stop typing)?
    /// A key every list/text dialog handles itself, whatever the modifiers.
    pub(crate) fn is_dialog_navigation(&self) -> bool {
        use KeyName::*;
        matches!(
            self.key,
            Enter
                | Tab
                | BackTab
                | Up
                | Down
                | Left
                | Right
                | PageUp
                | PageDown
                | Home
                | End
                | Backspace
                | Delete
        )
    }

    pub(crate) fn is_printable(&self) -> bool {
        matches!(self.key, KeyName::Char(_)) && !self.ctrl && !self.alt
    }
}

impl fmt::Display for KeyChord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (on, label) in [
            (self.ctrl, "Ctrl+"),
            (self.alt, "Alt+"),
            (self.shift, "Shift+"),
        ] {
            if on {
                f.write_str(label)?;
            }
        }
        match self.key {
            KeyName::Char(' ') => f.write_str("Space"),
            KeyName::Char(c) => write!(f, "{c}"),
            KeyName::BackTab => f.write_str("Shift+Tab"),
            KeyName::Left => f.write_str("←"),
            KeyName::Right => f.write_str("→"),
            KeyName::Up => f.write_str("↑"),
            KeyName::Down => f.write_str("↓"),
            KeyName::F(n) => write!(f, "F{n}"),
            other => write!(f, "{other:?}"),
        }
    }
}

/// Where a binding fires: straight from the keyboard, or as the key after the
/// leader chord. The two contexts never conflict with each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KeyBinding {
    Direct(KeyChord),
    Leader(KeyChord),
}

fn parse_key_name(token: &str) -> Option<KeyName> {
    let mut chars = token.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Some(KeyName::Char(c));
    }
    let lower = token.to_ascii_lowercase();
    if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        return (1..=24).contains(&n).then_some(KeyName::F(n));
    }
    Some(match lower.as_str() {
        "enter" | "return" => KeyName::Enter,
        "tab" => KeyName::Tab,
        "backtab" => KeyName::BackTab,
        "esc" | "escape" => KeyName::Esc,
        "backspace" => KeyName::Backspace,
        "delete" | "del" => KeyName::Delete,
        "insert" | "ins" => KeyName::Insert,
        "home" => KeyName::Home,
        "end" => KeyName::End,
        "pageup" | "pgup" => KeyName::PageUp,
        "pagedown" | "pgdn" => KeyName::PageDown,
        "up" => KeyName::Up,
        "down" => KeyName::Down,
        "left" => KeyName::Left,
        "right" => KeyName::Right,
        "space" => KeyName::Char(' '),
        _ => return None,
    })
}

/// Parse one chord: `[ctrl+][alt+][shift+]<key>` (modifiers in any order,
/// case-insensitive; `ctrl++` binds the `+` key).
pub fn parse_chord(spec: &str) -> Result<KeyChord, String> {
    let s = spec.trim();
    let (mods, key) = if let Some(m) = s.strip_suffix("++") {
        (m, "+")
    } else {
        s.rsplit_once('+').unwrap_or(("", s))
    };
    let key = parse_key_name(key).ok_or_else(|| format!("`{spec}`: unknown key `{key}`"))?;
    let (mut ctrl, mut alt, mut shift) = (false, false, false);
    for m in mods.split('+').filter(|m| !m.is_empty()) {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "alt" | "meta" => alt = true,
            "shift" => shift = true,
            other => return Err(format!("`{spec}`: unknown modifier `{other}`")),
        }
    }
    Ok(KeyChord::new(key, ctrl, alt, shift))
}

/// Parse a binding: a chord, or `leader <key>` with one unmodified key.
pub fn parse_binding(spec: &str) -> Result<KeyBinding, String> {
    let parts: Vec<&str> = spec.split_whitespace().collect();
    match parts.as_slice() {
        [chord] => parse_chord(chord).map(KeyBinding::Direct),
        ["leader", key] => {
            let chord = parse_chord(key)?;
            if chord.ctrl || chord.alt || chord.shift {
                return Err(format!(
                    "`{spec}`: the key after `leader` takes no modifiers"
                ));
            }
            Ok(KeyBinding::Leader(chord))
        }
        _ => Err(format!(
            "`{spec}`: expected a key like `ctrl+o` or a leader sequence like `leader o`"
        )),
    }
}
