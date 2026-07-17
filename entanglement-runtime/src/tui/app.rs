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
mod compact;
mod construct;
mod dispatch;
mod generation;
mod input;
mod inspect;
mod key;
mod mcp;
mod mention;
mod pickers;
mod quit;
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
    /// Active provider name, set from the initial selection and updated on
    /// `ModelChanged`. Shown in the bottom bar beside the model.
    active_provider: String,

    // Per-agent model pins (#323, ADR-0081): the managed `agent-models.yml` store,
    // and the pending persist recorded when the `/model` picker confirms. The
    // matching `ModelChanged` for the active session commits the write; an `Error`
    // (or a `ModelChanged` with no pending, i.e. a `SetAgent` pin application)
    // clears it without writing. `None` store in tests / when no config dir.
    agent_models:
        Option<std::sync::Arc<std::sync::Mutex<crate::config::agent_models::AgentModelStore>>>,
    /// `(agent, provider, model)` awaiting its `ModelChanged` confirmation.
    pending_model_persist: Option<(String, String, String)>,

    // Per-agent generation overrides (#376, mirroring the model-pin shape above):
    // the managed `agent-generation.yml` store, and the pending persist recorded
    // when `/set`'s Enter sends `InMsg::SetGeneration`. The matching
    // `GenerationChanged` for the active session commits the write; an `Error`
    // (or a `GenerationChanged` with no pending, i.e. a `/show` query or a
    // `SetAgent` reapplication) clears it without writing. `None` store in tests
    // / when no config dir.
    agent_generation: Option<
        std::sync::Arc<std::sync::Mutex<crate::config::agent_generation::AgentGenerationStore>>,
    >,
    /// `(agent, overrides)` awaiting its matching `GenerationChanged` confirmation.
    pending_generation_persist: Option<(String, entanglement_provider::GenerationParams)>,

    // `/key` dialog (#304): two-stage modal to persist a provider API key.
    key_dialog: crate::tui::key_dialog::KeyDialog,

    // `/mcp list` result panel (#373): the last snapshot + the correlation id
    // of any outstanding query.
    mcp_panel: crate::tui::mcp_panel::McpPanel,

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

    // Deferred session fork on compaction (ADR-0101): a `Compacted` event forks
    // the summary into a new session via `InMsg::Spawn`. The head-side view
    // switch + the summary-as-first-user-message record happen synchronously in
    // `handle_out_event`; the engine `Spawn` is recorded here for the async main
    // loop (which owns `Holly`) to send.
    pending_compact_fork: Option<CompactFork>,

    // In-session inspection overlay (#214): resolved prompt / agents / skills.
    inspect: inspect::InspectState,

    // `@file` mention completion + `!bash` passthrough (ADR-0030). `root` is the
    // working directory both the file index and `!bash` execution are rooted at.
    root: PathBuf,
    bash_enabled: bool,
    mention: MentionPopup,

    // Two-stage Ctrl+C (ADR-0087): first press clears transient input + arms a
    // pending quit; a second press within `quit::QUIT_TIMEOUT` quits. Any other
    // key — or expiry — disarms.
    quit_pending: bool,
    quit_pending_at: Option<Instant>,
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

    /// Records a head-side status line into the active session's transcript
    /// (#329) — the definitions watcher's one-line notice after a debounced
    /// reload, mirroring the `/key`/`/model` status pattern.
    pub fn record_reload_status(&mut self, message: String) {
        self.sessions
            .active_view_mut()
            .record_status("reload", message);
        self.mark_dirty();
    }

    pub fn handle_out_event(&mut self, event: OutEvent) {
        tracing::debug!("App handling OutEvent: {:?}", event);
        // Token totals + cost are tracked per-session view (#192), so a resumed
        // session restores its accumulated counts — the `Usage` event is folded
        // into the active view by `sessions.handle_out_event` below.
        // A live model switch (#218) updates the head's model display (context
        // bar) directly — model_info is app-global, not per-session-view state.
        if let OutEvent::ModelChanged {
            session,
            provider,
            model,
            context_window,
        } = &event
        {
            self.active_provider = provider.clone();
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
        // A generation-knob change (#374/#376): always render a status line with
        // the current effective params, and — if this is the confirmation of a
        // pending `/set` — persist it to `agent-generation.yml` too.
        if let OutEvent::GenerationChanged {
            session,
            generation,
        } = &event
        {
            self.handle_generation_changed(session, *generation);
        }
        // A failed switch clears any pending persist without writing (#323/#376).
        if let OutEvent::Error { session, .. } = &event {
            self.clear_pending_model_persist_on_error(session);
            self.clear_pending_generation_persist_on_error(session);
        }
        // MCP ops (#375) are engine-global, not per-session — folded here
        // rather than routed through `sessions.handle_out_event` below (which
        // never sees them, since `event.session()` is `None` for both).
        if let OutEvent::McpList {
            correlation_id,
            servers,
        } = &event
        {
            self.handle_mcp_list(correlation_id, servers.clone());
        }
        if let OutEvent::McpChanged { name, action } = &event {
            self.handle_mcp_changed(name, *action);
        }
        // Compaction forks (ADR-0101): intercept before routing, so the source
        // view renders a fork notice, a new view is minted + switched to, and a
        // pending `Spawn` is recorded for the async main loop to send. Deduped
        // by seq against the source view's watermark so a replayed/lagged
        // duplicate doesn't fork a second time (mirrors the reducer's own
        // seq-dedupe guard). Auto-compaction (`auto: true`, #398, ADR-0103) is
        // an in-place mutation the live engine already applied — no fork: the
        // session already continued under the reduced context, so the
        // reducer's own `Compacted` arm renders an in-place notice on the same
        // view via the ordinary `sessions.handle_out_event` routing below.
        if let OutEvent::Compacted {
            session: source,
            seq,
            summary,
            auto: false,
            ..
        } = &event
        {
            let is_new = self
                .sessions
                .view_for(source)
                .map(|v| *seq > v.last_seen_seq())
                .unwrap_or(true);
            if is_new {
                self.handle_compacted(source.clone(), summary.clone());
            }
        }
        if self.sessions.handle_out_event(event) {
            self.mark_dirty();
        }
    }
}
