//! `tui.keybindings` — remappable TUI actions (#735).
//!
//! A map of action name → one key spec or a list of them, each replacing that
//! action's default binding list (`[]` unbinds it). A spec is a chord —
//! `ctrl+`/`alt+`/`shift+` modifiers on a named key (`enter`, `tab`,
//! `pageup`, `space`, `f5`, …) or a single character — or a leader sequence
//! `leader <key>` (one unmodified key after the `leader` chord).
//!
//! Unlike `tui.theme`, a key map is all-or-nothing: a conflict (two actions on
//! one key in the same context), an unknown action, an unparseable or reserved
//! key rejects the *whole* override with one warning naming the culprits, and
//! the TUI keeps the map it had (the defaults at startup). Half-applying a
//! remap could leave e.g. `submit` and `newline` swapped onto one key.
//!
//! The action set is exactly the TUI's pre-#735 configurable bindings. Fixed
//! on purpose: `ctrl+c` (two-stage interrupt/quit, ADR-0087 — also where an
//! external SIGINT lands), `ctrl+q` (immediate quit, the escape hatch in every
//! dialog) and `esc` (cancel/close everywhere, and the leader-chord cancel) —
//! a remap must never be able to trap the user. Dialog-local keys (approval
//! `y/s/a/d/n/e/v`, list navigation) and readline editing keys stay fixed too.
//! `jump_attention`/`list_sessions` are checked ahead of every dialog, so
//! they may not take a dialog navigation key (enter, tab, arrows, page,
//! home/end, backspace/delete) as a direct chord.

use std::collections::BTreeMap;

use serde::Deserialize;

pub use super::keychord::{parse_binding, parse_chord, KeyBinding, KeyChord, KeyName};

macro_rules! actions {
    ($($variant:ident => $name:literal [$($spec:literal),*] $cat:literal $desc:literal),* $(,)?) => {
        /// One remappable TUI action.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum KeyAction { $($variant),* }

        impl KeyAction {
            pub const ALL: &'static [KeyAction] = &[$(KeyAction::$variant),*];

            /// The action's config name.
            pub fn name(self) -> &'static str {
                match self { $(KeyAction::$variant => $name),* }
            }

            /// The pre-#735 bindings, as key specs.
            pub fn default_specs(self) -> &'static [&'static str] {
                match self { $(KeyAction::$variant => &[$($spec),*]),* }
            }

            /// Help-dialog group.
            pub fn category(self) -> &'static str {
                match self { $(KeyAction::$variant => $cat),* }
            }

            pub fn description(self) -> &'static str {
                match self { $(KeyAction::$variant => $desc),* }
            }
        }
    };
}

actions! {
    Leader => "leader" ["ctrl+x"] "General" "Start a leader chord",
    Submit => "submit" ["enter"] "Input" "Send the prompt (or accept the open popup)",
    Newline => "newline" ["shift+enter", "alt+enter", "ctrl+j"] "Input" "Insert a newline",
    CycleMode => "cycle_mode" ["tab"] "Input" "Next permission mode (or accept the open popup)",
    CycleModeBack => "cycle_mode_back" ["shift+tab"] "Input" "Previous permission mode",
    ScrollUp => "scroll_up" ["pageup"] "Navigation" "Scroll the transcript up",
    ScrollDown => "scroll_down" ["pagedown"] "Navigation" "Scroll the transcript down",
    ToggleReasoning => "toggle_reasoning" ["leader t"] "Navigation" "Toggle the most recent thinking or tool block",
    JumpAttention => "jump_attention" ["ctrl+g"] "Sessions" "Jump to the session waiting on you",
    NewSession => "new_session" ["leader n"] "Sessions" "Create a new session",
    ListSessions => "list_sessions" ["leader l", "ctrl+l"] "Sessions" "List and switch sessions",
    PauseToggle => "pause_toggle" ["ctrl+space"] "Sessions" "Pause / resume the active session",
    PickAgent => "pick_agent" ["leader a", "ctrl+a"] "Agent" "Show available agents",
    PickModel => "pick_model" ["leader m"] "Agent" "Pick model",
    Inspect => "inspect" ["leader i"] "Agent" "Inspect prompt, agents & skills",
    ToggleSidebar => "toggle_sidebar" ["leader s"] "UI" "Toggle sidebar",
    OpenEditor => "open_editor" ["leader e"] "UI" "Open editor",
    Export => "export" ["leader E"] "UI" "Export conversation",
    CommandPalette => "command_palette" ["leader p", "ctrl+p"] "General" "Open command palette",
    Interrupt => "interrupt" ["leader c"] "General" "Interrupt current operation",
    Quit => "quit" ["leader q"] "General" "Quit (Ctrl+Q immediate; Ctrl+C clears then quits on second press)",
    Help => "help" ["leader ?", "leader h"] "Help" "Show help",
}

impl KeyAction {
    pub fn from_name(name: &str) -> Option<KeyAction> {
        KeyAction::ALL.iter().copied().find(|a| a.name() == name)
    }
}
/// Actions the event loop checks ahead of every open dialog.
const GLOBAL_ACTIONS: [KeyAction; 2] = [KeyAction::JumpAttention, KeyAction::ListSessions];

/// Keys no action may take (see the module docs for why).
const RESERVED: [(KeyChord, &str); 3] = [
    (
        KeyChord {
            key: KeyName::Char('c'),
            ctrl: true,
            alt: false,
            shift: false,
        },
        "interrupt / two-stage quit",
    ),
    (
        KeyChord {
            key: KeyName::Char('q'),
            ctrl: true,
            alt: false,
            shift: false,
        },
        "immediate quit",
    ),
    (KeyChord::plain(KeyName::Esc), "cancel / close"),
];
/// One spec or a list of them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum KeySpecs {
    One(String),
    Many(Vec<String>),
}

impl KeySpecs {
    pub fn specs(&self) -> Vec<&str> {
        match self {
            KeySpecs::One(s) => vec![s.as_str()],
            KeySpecs::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

/// Raw `tui.keybindings`: action name → spec(s), validated by
/// [`Self::resolve`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct KeybindingsConfig(pub BTreeMap<String, KeySpecs>);

/// A validated key map: the leader chord plus every (action, binding) pair,
/// sorted by action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedKeys {
    pub leader: KeyChord,
    pub bindings: Vec<(KeyAction, KeyBinding)>,
}

impl ResolvedKeys {
    /// The built-in map.
    pub fn defaults() -> ResolvedKeys {
        KeybindingsConfig::default()
            .resolve()
            .expect("invariant: the built-in key map is conflict-free (tested)")
    }
}

impl KeybindingsConfig {
    /// Apply the overrides to the defaults and validate the result. `Err` is
    /// one message naming every problem; the caller keeps its previous map.
    pub fn resolve(&self) -> Result<ResolvedKeys, String> {
        let mut problems = Vec::new();
        let mut specs: BTreeMap<KeyAction, Vec<String>> = KeyAction::ALL
            .iter()
            .map(|a| {
                (
                    *a,
                    a.default_specs().iter().map(|s| s.to_string()).collect(),
                )
            })
            .collect();
        for (name, value) in &self.0 {
            match KeyAction::from_name(name) {
                Some(a) => {
                    specs.insert(a, value.specs().iter().map(|s| s.to_string()).collect());
                }
                None => problems.push(format!("unknown action `{name}`")),
            }
        }

        let mut leader = None;
        let mut owner: BTreeMap<KeyBinding, KeyAction> = BTreeMap::new();
        let mut bindings = Vec::new();
        for (action, list) in &specs {
            for spec in list {
                let binding = match parse_binding(spec) {
                    Ok(b) => b,
                    Err(e) => {
                        problems.push(format!("{}: {e}", action.name()));
                        continue;
                    }
                };
                if let Some(why) = reserved(binding) {
                    problems.push(format!(
                        "{}: `{spec}` is reserved ({why}) and can't be rebound",
                        action.name()
                    ));
                    continue;
                }
                if let KeyBinding::Direct(c) = binding {
                    if GLOBAL_ACTIONS.contains(action) && c.is_dialog_navigation() {
                        problems.push(format!(
                            "{}: `{spec}` is a dialog navigation key, and this action is \
                             checked before every dialog — it would break them",
                            action.name()
                        ));
                        continue;
                    }
                    if c.is_printable() {
                        problems.push(format!(
                            "{}: `{spec}` would stop that key typing — add a modifier or \
                             use `leader {spec}`",
                            action.name()
                        ));
                        continue;
                    }
                }
                if *action == KeyAction::Leader {
                    match binding {
                        KeyBinding::Direct(c) => leader = Some(c),
                        KeyBinding::Leader(_) => {
                            problems.push(format!("leader: `{spec}` can't start with `leader`"))
                        }
                    }
                    continue;
                }
                match owner.get(&binding) {
                    Some(other) if other != action => problems.push(format!(
                        "`{spec}` is bound to both `{}` and `{}`",
                        other.name(),
                        action.name()
                    )),
                    Some(_) => {}
                    None => {
                        owner.insert(binding, *action);
                        bindings.push((*action, binding));
                    }
                }
            }
        }
        if specs.get(&KeyAction::Leader).map_or(0, Vec::len) != 1 {
            problems.push("leader: exactly one key is required".to_string());
        }
        if specs.get(&KeyAction::Submit).is_some_and(Vec::is_empty) {
            problems.push("submit: at least one key is required".to_string());
        }
        if let Some(l) = leader {
            if let Some(a) = owner.get(&KeyBinding::Direct(l)) {
                problems.push(format!(
                    "`{l}` is bound to both `leader` and `{}`",
                    a.name()
                ));
            }
        }
        match (leader, problems.is_empty()) {
            (Some(leader), true) => Ok(ResolvedKeys { leader, bindings }),
            _ => Err(format!(
                "tui.keybindings: {} — keeping the previous key map",
                problems.join("; ")
            )),
        }
    }

    pub fn warnings(&self) -> Vec<String> {
        self.resolve().err().into_iter().collect()
    }
}

fn reserved(binding: KeyBinding) -> Option<&'static str> {
    match binding {
        KeyBinding::Direct(c) => RESERVED.iter().find(|(r, _)| *r == c).map(|(_, why)| *why),
        KeyBinding::Leader(c) if c.key == KeyName::Esc => Some("cancels the leader chord"),
        KeyBinding::Leader(_) => None,
    }
}

#[cfg(test)]
#[path = "tests_keybindings.rs"]
mod tests;
