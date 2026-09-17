use ratatui::layout::Rect;

/// A deferred, terminal-owning side effect a command/action requests but cannot
/// perform itself: the `App` has no `Terminal`, so it records the intent here
/// and the event loop (which does) runs it via `tui::editor::run_effect`
/// (ADR-0029).
#[derive(Debug, Clone, PartialEq)]
pub enum UiEffect {
    /// Suspend the TUI, edit the input draft in `$EDITOR`, read it back.
    OpenEditor,
    /// Export the transcript to Markdown and open it in `$EDITOR`.
    Export,
    /// Open the active session's bound plan file in `$EDITOR` (#513) — the
    /// `/plan` command, now that the plan side panel is gone. A no-op (with a
    /// transcript notice) when no plan has been proposed yet.
    OpenPlanFile,
    /// Copy the given text to the system clipboard (OSC 52). Deferred to the
    /// event loop because it writes to the terminal the loop owns.
    CopyToClipboard(String),
}

#[derive(Clone)]
pub struct ProfileInfo {
    pub name: String,
    pub description: String,
}

/// The list `Rect` each open modal captured at draw time, so a left-click can
/// map to a row index and dispatch the row's `Enter` action (Issue 1 — mouse
/// support). A field stays `Rect::default()` while its modal is closed, so a
/// stale click can never hit a list that isn't on screen. The drawers write
/// these via `App::set_modal_*` setters; `modal_events::handle_mouse` reads
/// them back. `command_palette_list` is the inner list chunk (the palette
/// splits its area into a query row + the list); every other field is the
/// full bordered `Rect` the `List`/`Paragraph` rendered into.
#[derive(Debug, Default, Clone)]
pub struct ModalClickAreas {
    /// Sessions modal (`Ctrl+L`).
    pub sessions: Rect,
    /// Resume modal (`/resume`).
    pub resume: Rect,
    /// `/agent` profile picker (`Ctrl+A`).
    pub profile_picker: Rect,
    /// `/model` picker.
    pub model_picker: Rect,
    /// `/mode` picker (#560 P12, ADR-0207 §12).
    pub mode_picker: Rect,
    /// `/key` dialog — the provider list on the `PickProvider` stage only.
    pub key_dialog: Rect,
    /// Bare `/enable` session-tools checklist (#539).
    pub session_tools_dialog: Rect,
    /// `Ctrl+P` command palette — the list chunk below the query row.
    pub command_palette_list: Rect,
    /// `/mcp list` panel — a `Paragraph` rendering two lines per server.
    pub mcp_panel: Rect,
    /// `/cmd` slash-autocomplete popup, anchored above the input box.
    pub slash_popup: Rect,
    /// `@file` mention popup, anchored above the input box.
    pub mention_popup: Rect,
}
