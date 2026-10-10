//! The `config.yml` `tui:` block — terminal-UI preferences (ADR-0213).
//!
//! One struct for every TUI knob, so the block grows by adding fields rather
//! than new top-level keys: #739 moved the two former env vars here
//! (`notify`, `mouse`); `theme`/`keybindings` (#735) and `statusline` (#762)
//! join as further fields of [`TuiConfig`]. `#[serde(default)]` keeps every
//! field optional — a layer may set one knob and inherit the rest — while
//! `deny_unknown_fields` keeps a typo loud, like the rest of `config.yml`.
//!
//! Live-reloaded: the TUI re-resolves the layered config when the definitions
//! watcher reports a change and re-applies this block in place
//! (`tui::editor::reload_tui_config`).

use serde::Deserialize;

/// Resolved `tui:` settings. The defaults are the pre-#739 behavior with
/// neither env var set.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TuiConfig {
    /// Also emit an OSC 9 desktop notification (not just the bell) when a
    /// turn needs attention. Off by default: not every terminal renders OSC 9
    /// gracefully. Replaces `ENTANGLEMENT_TUI_NOTIFY=1`.
    pub notify: bool,
    /// Capture the mouse (wheel scroll, clickable blocks). `false` gives the
    /// terminal back its native text selection. Replaces
    /// `ENTANGLEMENT_TUI_NO_MOUSE`.
    pub mouse: bool,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            notify: false,
            mouse: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Result<TuiConfig, serde_yaml::Error> {
        serde_yaml::from_str(yaml)
    }

    #[test]
    fn defaults_are_bell_only_with_mouse_capture() {
        assert_eq!(
            TuiConfig::default(),
            TuiConfig {
                notify: false,
                mouse: true
            }
        );
        assert_eq!(parse("{}").unwrap(), TuiConfig::default());
    }

    #[test]
    fn each_field_is_optional_and_independent() {
        let c = parse("notify: true\n").unwrap();
        assert!(c.notify && c.mouse);
        let c = parse("mouse: false\n").unwrap();
        assert!(!c.notify && !c.mouse);
    }

    #[test]
    fn an_unknown_key_is_a_loud_error() {
        let err = parse("notfy: true\n").unwrap_err();
        assert!(err.to_string().contains("notfy"), "{err}");
    }
}
