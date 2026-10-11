//! Applying the `tui.theme` / `tui.keybindings` config (#735), at startup and
//! on every live reload.

use crate::config::TuiConfig;
use crate::tui::keybindings::KeyMap;
use crate::tui::markdown::MarkdownRenderer;
use crate::tui::theme::Theme;

use super::App;

/// The `NO_COLOR` convention (no-color.org): set and non-empty disables colour.
pub(crate) fn no_color_env() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
}

impl App {
    /// Apply the block's theme and key map. A key map that fails validation
    /// keeps the current one (the defaults at startup). Each *changed* set of
    /// theme/keybinding warnings is recorded once as a transcript notice — the
    /// watcher fires on any config-dir change, so an unchanged problem must not
    /// re-report on every unrelated save. Startup warnings already went to
    /// stderr, so the first call only primes the set.
    pub(crate) fn apply_appearance(&mut self, config: &TuiConfig, no_color: bool, startup: bool) {
        let resolved = config.theme.resolve();
        let mut warnings = resolved.warnings.clone();
        self.set_theme(Theme::resolve(&resolved, no_color));
        match config.keybindings.resolve() {
            Ok(keys) => self.leader_handler.set_keymap(KeyMap::from_resolved(&keys)),
            Err(e) => warnings.push(e),
        }
        self.note_tui_warnings(warnings, !startup);
    }

    /// A reload whose config no longer parses at all (a shape error such as
    /// `theme: {bsae: …}`): the previous settings stay, and the error is
    /// reported once, deduplicated like the per-slot/conflict warnings.
    pub(crate) fn report_config_error(&mut self, error: String) {
        self.note_tui_warnings(vec![error], true);
    }

    fn note_tui_warnings(&mut self, warnings: Vec<String>, show: bool) {
        if warnings != self.tui_warnings {
            if show {
                for w in &warnings {
                    self.record_status("config", format!("⚠ {w}"));
                }
            }
            self.tui_warnings = warnings;
        }
    }

    pub(crate) fn set_theme(&mut self, theme: Theme) {
        if self.theme != theme {
            self.theme = theme;
            self.markdown_renderer = MarkdownRenderer::with_theme(theme);
            self.mark_dirty();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::theme::ThemeBase;
    use crate::tui::keybindings::Action;
    use crate::tui::session_view::TranscriptEntry;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use entanglement_core::SessionId;

    fn config(yaml: &str) -> TuiConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn notices(app: &App) -> usize {
        app.transcript()
            .iter()
            .filter(
                |e| matches!(e, TranscriptEntry::ToolOutput { tool: Some(t), .. } if t == "config"),
            )
            .count()
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn theme_and_keys_apply_and_a_bad_key_map_keeps_the_previous_one() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.apply_appearance(
            &config("{theme: light, keybindings: {pick_model: ctrl+o}}"),
            false,
            true,
        );
        assert_eq!(app.theme().base(), ThemeBase::Light);
        assert_eq!(
            app.keymap().direct_action(&ctrl('o')),
            Some(Action::PickModel)
        );
        assert_eq!(
            notices(&app),
            0,
            "startup warnings go to stderr, not the transcript"
        );

        // A conflicting remap is refused whole; the ctrl+o map stays.
        app.apply_appearance(&config("{keybindings: {export: ctrl+g}}"), false, false);
        assert_eq!(
            app.keymap().direct_action(&ctrl('o')),
            Some(Action::PickModel)
        );
        assert_eq!(
            app.keymap().direct_action(&ctrl('g')),
            Some(Action::JumpAttention)
        );
        assert_eq!(
            app.theme().base(),
            ThemeBase::Dark,
            "the theme still reloads"
        );
        assert_eq!(notices(&app), 1);

        // The same problem on an unrelated reload is not re-reported.
        app.apply_appearance(&config("{keybindings: {export: ctrl+g}}"), false, false);
        assert_eq!(notices(&app), 1);
    }

    #[test]
    fn a_reload_load_error_is_reported_once() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.apply_appearance(&TuiConfig::default(), false, true);
        app.report_config_error("bad shape".into());
        app.report_config_error("bad shape".into());
        assert_eq!(notices(&app), 1, "deduplicated across watcher fires");
        // Fixing the file clears the remembered error, so a recurrence shows.
        app.apply_appearance(&TuiConfig::default(), false, false);
        app.report_config_error("bad shape".into());
        assert_eq!(notices(&app), 2);
    }

    #[test]
    fn no_color_reaches_the_markdown_renderer_and_profile_colours() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.apply_appearance(&TuiConfig::default(), true, true);
        assert!(app.theme().no_color());
        assert!(app.markdown_renderer().theme().no_color());
        assert_eq!(app.profile_color_for("build"), ratatui::style::Color::Reset);
    }
}
