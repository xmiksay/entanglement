//! External `$EDITOR` integration + transcript export (ADR-0029).
//!
//! Both features suspend the TUI (leave the alternate screen + raw mode), run a
//! blocking editor process with inherited stdio, then re-enter and force a full
//! redraw. They are driven from the event loop — which owns the [`Terminal`] —
//! via a deferred [`UiEffect`] set on [`App`] by the command/action handlers.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::Terminal;

use crate::config::TuiConfig;
use crate::tui::app::{App, UiEffect};
use crate::tui::attention::Attention;
use crate::tui::export;

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Executes a deferred terminal-owning effect: launch `$EDITOR` on the input
/// draft (reading the result back into the input box), or write the transcript
/// to `<session>-<unix_secs>.md` and open it. Errors are returned, not fatal —
/// the caller logs them and keeps the session alive.
pub fn run_effect(terminal: &mut Term, app: &mut App, effect: UiEffect) -> Result<()> {
    // Resolve once up front (config.yml editor > $VISUAL > $EDITOR > vi) so both
    // effect paths launch the same editor.
    let editor = resolve_editor(app.configured_editor());
    match effect {
        UiEffect::OpenEditor => {
            let edited = edit_text(terminal, &app.input_text(), &editor)?;
            app.set_input_text(edited);
        }
        UiEffect::Export => {
            let secs = unix_secs();
            let markdown = export::transcript_to_markdown(
                app.active_session_id(),
                app.plan().map(String::as_str),
                app.task_list().map(String::as_str),
                app.transcript(),
                secs,
            );
            let path = std::env::current_dir()
                .unwrap_or_default()
                .join(export::export_filename(app.active_session_id(), secs));
            std::fs::write(&path, markdown)
                .with_context(|| format!("writing transcript to {}", path.display()))?;
            tracing::info!("exported transcript to {}", path.display());
            suspended(terminal, || launch_editor(&path, &editor))?;
        }
        UiEffect::OpenPlanFile => {
            let Some(plan_path) = app.plan_path().map(str::to_string) else {
                app.set_toast("no plan proposed yet in this session".to_string());
                return Ok(());
            };
            let path = app.root().join(&plan_path);
            suspended(terminal, || launch_editor(&path, &editor))?;
        }
        // Clipboard write needs no terminal suspend (OSC 52 is a control string,
        // not a visible cursor move) — just write it to the backend in place.
        UiEffect::CopyToClipboard(text) => {
            crate::tui::clipboard::copy_osc52(terminal, &text)?;
        }
    }
    Ok(())
}

/// Seeds a temp file with `initial`, opens it in `$EDITOR`, and returns the
/// edited content (trailing newlines trimmed so the input box gains no blank
/// lines). The temp file is removed afterwards.
fn edit_text(terminal: &mut Term, initial: &str, editor: &str) -> Result<String> {
    let path = std::env::temp_dir().join(format!("skutter-input-{}.md", std::process::id()));
    std::fs::write(&path, initial)
        .with_context(|| format!("seeding editor buffer {}", path.display()))?;
    suspended(terminal, || launch_editor(&path, editor))?;
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("reading editor buffer {}", path.display()))?;
    let _ = std::fs::remove_file(&path);
    Ok(content.trim_end_matches('\n').to_string())
}

/// Runs `f` with the TUI suspended, restoring the alternate screen + raw mode
/// afterwards regardless of the outcome and forcing a full redraw.
fn suspended<T>(terminal: &mut Term, f: impl FnOnce() -> Result<T>) -> Result<T> {
    leave(terminal)?;
    let result = f();
    enter(terminal)?;
    terminal.clear()?;
    result
}

/// Leaves the alternate screen + raw mode (mirrors `tui::restore_terminal`).
fn leave(terminal: &mut Term) -> Result<()> {
    disable_raw_mode().context("disabling raw mode")?;
    let _ = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        PopKeyboardEnhancementFlags
    );
    Ok(())
}

/// Re-enters the alternate screen + raw mode (mirrors `tui::tui` setup).
fn enter(terminal: &mut Term) -> Result<()> {
    let _ = execute!(terminal.backend_mut(), EnterAlternateScreen);
    enable_raw_mode().context("enabling raw mode")?;
    let _ = execute!(
        terminal.backend_mut(),
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        )
    );
    if mouse_capture_enabled() {
        let _ = execute!(terminal.backend_mut(), EnableMouseCapture);
    }
    Ok(())
}

/// Launches the resolved editor on `path`, blocking until it exits (the
/// `--wait` convention). Word-splits the command so `"code --wait"` works.
fn launch_editor(path: &Path, editor: &str) -> Result<()> {
    let (program, args) = split_editor_command(editor);
    let status = Command::new(program)
        .args(args)
        .arg(path)
        .status()
        .with_context(|| format!("launching editor `{editor}`"))?;
    if !status.success() {
        tracing::warn!("editor `{editor}` exited with {status}");
    }
    Ok(())
}

/// Word-split an editor command into `(program, args)` so `EDITOR="code --wait"`
/// runs `code` with `--wait`. An empty/whitespace value falls back to `vi` (the
/// same last-resort as [`pick_editor`], defensive should it ever reach here).
fn split_editor_command(editor: &str) -> (String, Vec<String>) {
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi").to_string();
    (program, parts.map(str::to_string).collect())
}

/// The persisted `config.yml` `editor:` (if any) wins, then `$VISUAL`, then
/// `$EDITOR`, then `vi` — so an in-app default overrides the shell env
/// (#persist-editor).
fn resolve_editor(configured: Option<&str>) -> String {
    pick_editor(
        configured.map(str::to_string),
        std::env::var("VISUAL").ok(),
        std::env::var("EDITOR").ok(),
    )
}

/// Pure editor selection: the first non-blank of `config` → `visual` → `editor`,
/// else `vi`. Each source is skipped independently when blank, so a set-but-empty
/// `$VISUAL` no longer shadows a real `$EDITOR`. Split out from env/config access
/// so the precedence is unit-testable.
fn pick_editor(config: Option<String>, visual: Option<String>, editor: Option<String>) -> String {
    [config, visual, editor]
        .into_iter()
        .flatten()
        .find(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "vi".to_string())
}

/// The live `tui.mouse` setting. Process-global like the terminal it
/// configures: one TUI per process, and the `$EDITOR` round-trip must re-enter
/// with whatever the setting is *now*, not what it was at launch.
static MOUSE_CAPTURE: AtomicBool = AtomicBool::new(true);

/// Whether mouse capture should be (re-)enabled.
pub(crate) fn mouse_capture_enabled() -> bool {
    MOUSE_CAPTURE.load(Ordering::Relaxed)
}

/// Record `enabled` as the live setting; `Some(enabled)` when that is a
/// change the terminal must be told about.
fn set_mouse_capture(enabled: bool) -> Option<bool> {
    (MOUSE_CAPTURE.swap(enabled, Ordering::Relaxed) != enabled).then_some(enabled)
}

/// Apply a `tui:` block to the running TUI (startup and every live reload,
/// ADR-0213): toggle mouse capture only on an actual change, and hand
/// `notify` to the attention signaller.
pub(crate) fn apply_tui_config(
    out: &mut impl std::io::Write,
    attention: &mut Attention,
    config: &TuiConfig,
) {
    match set_mouse_capture(config.mouse) {
        Some(true) => {
            let _ = execute!(out, EnableMouseCapture);
        }
        Some(false) => {
            let _ = execute!(out, DisableMouseCapture);
        }
        None => {}
    }
    attention.set_notify(config.notify);
}

/// The watcher fired (any config file may have changed): re-resolve the
/// layered config for `root` and re-apply its `tui:` block. A config that no
/// longer parses keeps the previous settings — a mid-save edit must not
/// break a running TUI. The status-line templates go to the `App`, which
/// owns the row they render.
pub(crate) fn reload_tui_config(
    term: &mut Term,
    attention: &mut Attention,
    app: &mut App,
    root: &Path,
) {
    match crate::config::Config::load(root) {
        Ok(c) => {
            apply_tui_config(term.backend_mut(), attention, &c.tui);
            app.apply_appearance(&c.tui, crate::tui::app::appearance::no_color_env(), false);
            app.set_statusline_config(c.tui.statusline);
        }
        Err(e) => tracing::warn!("tui config not reloaded, keeping previous settings: {e:#}"),
    }
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{apply_tui_config, pick_editor, split_editor_command, Attention, TuiConfig};

    /// One test owns the process-global flag, so its transitions are
    /// deterministic.
    #[test]
    fn mouse_capture_toggles_only_on_a_change() {
        let mut out = Vec::new();
        let mut attention = Attention::new(false);
        let cfg = |mouse| TuiConfig {
            notify: false,
            mouse,
            ..TuiConfig::default()
        };
        apply_tui_config(&mut out, &mut attention, &cfg(true));
        apply_tui_config(&mut out, &mut attention, &cfg(true));
        assert!(out.is_empty(), "unchanged (default on) writes nothing");
        apply_tui_config(&mut out, &mut attention, &cfg(false));
        assert!(!out.is_empty(), "a change is sent to the terminal");
        assert!(!super::mouse_capture_enabled());
        out.clear();
        apply_tui_config(&mut out, &mut attention, &cfg(false));
        assert!(out.is_empty());
        apply_tui_config(&mut out, &mut attention, &cfg(true));
        assert!(!out.is_empty() && super::mouse_capture_enabled());
    }

    #[test]
    fn pick_editor_config_wins_over_env() {
        // The persisted config editor beats both $VISUAL and $EDITOR.
        assert_eq!(
            pick_editor(
                Some("nvim".into()),
                Some("emacs".into()),
                Some("nano".into())
            ),
            "nvim"
        );
    }

    #[test]
    fn pick_editor_prefers_visual_over_editor() {
        assert_eq!(
            pick_editor(None, Some("emacs".into()), Some("nano".into())),
            "emacs"
        );
    }

    #[test]
    fn pick_editor_falls_back_to_editor_then_vi() {
        assert_eq!(pick_editor(None, None, Some("nano".into())), "nano");
        assert_eq!(pick_editor(None, None, None), "vi");
    }

    #[test]
    fn pick_editor_skips_blank_values_per_source() {
        // A blank source is skipped independently, so a set-but-empty $VISUAL
        // falls through to a real $EDITOR rather than shadowing it.
        assert_eq!(pick_editor(None, Some("   ".into()), None), "vi");
        assert_eq!(
            pick_editor(None, Some("".into()), Some("nano".into())),
            "nano"
        );
        // A blank config falls through to $VISUAL.
        assert_eq!(
            pick_editor(Some("  ".into()), Some("emacs".into()), None),
            "emacs"
        );
    }

    #[test]
    fn split_editor_command_separates_program_and_args() {
        let (prog, args) = split_editor_command("code --wait --new-window");
        assert_eq!(prog, "code");
        assert_eq!(args, vec!["--wait", "--new-window"]);
    }

    #[test]
    fn split_editor_command_bare_program_has_no_args() {
        let (prog, args) = split_editor_command("vim");
        assert_eq!(prog, "vim");
        assert!(args.is_empty());
    }

    #[test]
    fn split_editor_command_empty_falls_back_to_vi() {
        let (prog, args) = split_editor_command("   ");
        assert_eq!(prog, "vi");
        assert!(args.is_empty());
    }
}
