use entanglement_core::{AgentMode, SessionId};
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

/// A deferred compaction fork (ADR-0101): on `OutEvent::Compacted`, the TUI
/// mints a fresh session id, switches the active view to it, and records the
/// summary as its first user message — all synchronously. The engine side
/// (`InMsg::Spawn`) needs `Holly`, which the synchronous `handle_out_event`
/// doesn't hold, so it's recorded here for the async main loop to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactFork {
    pub new_session: SessionId,
    pub source: SessionId,
    /// The source session's agent profile name — `Spawn` inherits it so the
    /// fork runs under the same profile/model pin.
    pub agent: String,
    pub summary: String,
}

#[derive(Clone)]
pub struct ProfileInfo {
    pub name: String,
    pub description: String,
    /// Governs the *implicit* Tab cycle ring (`Primary` only, #322); the
    /// `/agent` picker still lists every entry agent (`primary | all`).
    pub mode: AgentMode,
    /// Current effective tool allowlist (#330): `None` inherits every advertised
    /// tool. Seeds the `/agent` picker's `e` tools-checklist dialog.
    pub tools: Option<Vec<String>>,
    /// Current effective tool denylist, applied after `tools` (#330).
    pub disallowed_tools: Vec<String>,
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
    /// `/key` dialog — the provider list on the `PickProvider` stage only.
    pub key_dialog: Rect,
    /// `/agent` picker's `e` tools-checklist dialog (#330).
    pub tools_dialog: Rect,
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
