use entanglement_core::{AgentMode, AgentState, OutEvent, SessionId};
use entanglement_provider::ModelInfo;
use ratatui::layout::Rect;
use ratatui::widgets::ListState;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Instant;

use crate::session_store::SessionMeta;
use crate::tui::commands::CommandPalette;
use crate::tui::input::SimpleInput;
use crate::tui::keybindings::LeaderKeyHandler;
use crate::tui::markdown::MarkdownRenderer;
use crate::tui::mention::MentionPopup;
use crate::tui::session_view::TranscriptEntry;
use crate::tui::sessions::SessionRegistry;
use crate::tui::theme::Theme;

// `App` is split across sibling submodules (issue #109); each contributes a
// cohesive slice of the `impl App` surface. Fields stay private here — child
// modules reach them through their descendant visibility.
mod construct;
mod dispatch;
mod input;
mod inspect;
mod key;
mod mention;
mod pickers;
mod state;
mod tools;
mod view;

pub use inspect::InspectTab;

#[cfg(test)]
mod tests;

const HISTORY_CAPACITY: usize = 100;

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

pub struct App {
    sessions: SessionRegistry,
    dirty: bool,
    markdown_renderer: MarkdownRenderer,

    input: SimpleInput,
    history: VecDeque<String>,
    history_index: Option<usize>,
    history_search_term: Option<String>,

    // Profile picker state — catalog is global, selection acts on the active session.
    showing_profile_picker: bool,
    profile_picker_state: ListState,
    available_profiles: Vec<ProfileInfo>,
    primary_profile_order: Vec<String>,

    // Model picker state — catalog is global, selection is display-only (requires restart)
    showing_model_picker: bool,
    model_picker_state: ListState,
    available_models: Vec<(String, Vec<String>)>,
    model_info: ModelInfo,

    // Per-agent model pins (#323, ADR-0081): the managed `agent-models.yml` store,
    // and the pending persist recorded when the `/model` picker confirms. The
    // matching `ModelChanged` for the active session commits the write; an `Error`
    // (or a `ModelChanged` with no pending, i.e. a `SetAgent` pin application)
    // clears it without writing. `None` store in tests / when no config dir.
    agent_models: Option<crate::config::agent_models::AgentModelStore>,
    /// `(agent, provider, model)` awaiting its `ModelChanged` confirmation.
    pending_model_persist: Option<(String, String, String)>,

    // `/key` dialog (#304): two-stage modal to persist a provider API key.
    key_dialog: crate::tui::key_dialog::KeyDialog,

    // `/agent` picker's `e` tools-checklist dialog (#330): the full advertised
    // tool roster (host + MCP + runtime-owned specs, from
    // `EngineConfig::tool_specs` at startup) plus the checklist's own state.
    tool_roster: Vec<String>,
    tools_dialog: crate::tui::tools_dialog::ToolsDialog,

    // Leader key state
    leader_handler: LeaderKeyHandler,
    showing_help: bool,

    // Command palette state
    command_palette: CommandPalette,

    // Sidebar state
    sidebar_visible: bool,
    sidebar_width: u16,

    // Theme and rendering state
    theme: Theme,
    profile_colors: HashMap<String, ratatui::style::Color>,
    thinking_since: Option<Instant>,

    // Token usage tracking (#192): accumulated from `OutEvent::Usage` deltas.
    input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,

    // Input state
    input_multiline: bool,
    help_text: String,

    // Resume session modal state
    showing_resume_modal: bool,
    resume_state: ListState,
    available_sessions: Vec<SessionMeta>,

    // Chat click hit-testing: geometry + per-rendered-line block provenance
    // captured at draw time so a later mouse click maps back to a transcript
    // block. `chat_scroll_offset` is the resolved vertical offset the paragraph
    // was drawn at.
    chat_area: Rect,
    chat_scroll_offset: usize,
    chat_line_blocks: Vec<Option<usize>>,

    // Deferred terminal-owning effect (editor / export) for the event loop to run.
    pending_effect: Option<UiEffect>,

    // In-session inspection overlay (#214): resolved prompt / agents / skills.
    inspect: inspect::InspectState,

    // `@file` mention completion + `!bash` passthrough (ADR-0030). `root` is the
    // working directory both the file index and `!bash` execution are rooted at.
    root: PathBuf,
    bash_enabled: bool,
    mention: MentionPopup,
}

impl App {
    pub fn active_session_id(&self) -> &SessionId {
        self.sessions.active_id()
    }

    pub fn create_session(&mut self) -> SessionId {
        let id = self.sessions.create();
        self.mark_dirty();
        id
    }

    pub fn sessions(&self) -> Vec<(&SessionId, &crate::tui::session_view::SessionView)> {
        self.sessions.all()
    }

    /// Adopt an externally-minted session id (create its view if absent + switch)
    /// — the `propose_plan` handoff mints a fresh root `build` session (#141).
    pub fn adopt_session(&mut self, id: SessionId) {
        self.sessions.adopt(id);
        self.mark_dirty();
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn markdown_renderer(&self) -> &MarkdownRenderer {
        &self.markdown_renderer
    }

    pub fn agent(&self) -> &str {
        self.sessions.active_view().agent()
    }

    pub fn state(&self) -> AgentState {
        self.sessions.active_view().state()
    }

    pub fn transcript(&self) -> &[TranscriptEntry] {
        self.sessions.active_view().transcript()
    }

    pub fn plan(&self) -> Option<&String> {
        self.sessions.active_view().plan()
    }

    pub fn task_list(&self) -> Option<&String> {
        self.sessions.active_view().task_list()
    }

    /// Records a deferred terminal-owning effect for the event loop to run.
    pub fn request_effect(&mut self, effect: UiEffect) {
        self.pending_effect = Some(effect);
    }

    /// Takes the pending terminal-owning effect, if any (event loop drains it).
    pub fn take_pending_effect(&mut self) -> Option<UiEffect> {
        self.pending_effect.take()
    }

    /// Records the user's prompt into the active session's transcript so the
    /// chat scrollback mirrors a real conversation (the engine never echoes
    /// `InMsg::Prompt` back as an `OutEvent`).
    pub fn record_user_message(&mut self, text: String) {
        self.sessions.active_view_mut().record_user_message(text);
        self.mark_dirty();
    }

    pub fn handle_out_event(&mut self, event: OutEvent) {
        tracing::debug!("App handling OutEvent: {:?}", event);
        // Token totals + cost are head-level (#192): accumulate the per-round-trip
        // delta before routing the event into its session view.
        if let OutEvent::Usage {
            input_tokens,
            output_tokens,
            cost_usd,
            ..
        } = &event
        {
            self.add_input_tokens(*input_tokens);
            self.add_output_tokens(*output_tokens);
            if let Some(cost) = cost_usd {
                self.add_cost(*cost);
            }
            self.mark_dirty();
        }
        // A live model switch (#218) updates the head's model display (context
        // bar) directly — model_info is app-global, not per-session-view state.
        if let OutEvent::ModelChanged {
            session,
            provider,
            model,
            context_window,
        } = &event
        {
            self.set_model_info(ModelInfo {
                id: model.clone(),
                display_name: model.clone(),
                context_window: context_window.map(|w| w as u32),
            });
            // Persist-on-confirmation (#323): a `/model` pick recorded a pending
            // persist; its matching `ModelChanged` commits the write. A
            // `ModelChanged` from a `SetAgent` pin application has no pending, so
            // it never writes.
            self.persist_model_if_pending(session, provider, model);
        }
        // A failed switch clears any pending persist without writing (#323).
        if let OutEvent::Error { session, .. } = &event {
            self.clear_pending_model_persist_on_error(session);
        }
        if self.sessions.handle_out_event(event) {
            self.mark_dirty();
        }
    }
}
