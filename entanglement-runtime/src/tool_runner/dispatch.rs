//! The dispatch seam (issue #451, split by ADR-0227): one graded path for a
//! tool call, whoever issued it. [`Dispatcher::run`] grades and executes a
//! [`Call`] and hands back an [`Outcome`]; the caller decides what to do
//! with it — [`Dispatcher::dispatch`] folds it back to core as the
//! `ToolResult` a model call is owed, while a rhai binding returns it to the
//! script over the bridge (`crate::script`), so core never sees a result for
//! an id it never parked on.
//!
//! The ladder itself lives in [`gates`] (the `Control` bypass, overlay
//! deny, mode grade + grants, hooks, escape-root, the credentialed-`http`
//! gate), unknown/lazy-MCP resolution in [`resolve`], the approval park in
//! [`decision`], the run-against-the-registry step in [`execute`].

mod decision;
mod execute;
mod gates;
pub(crate) mod grade;
mod resolve;
mod script;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use entanglement_core::{
    ContentPart, Holly, PermissionProfile, SessionId, ToolEnvelope, ToolOverlayEntry,
};

use crate::arg_validate::LoopBreaker;
use crate::attendance::Attendance;
use crate::hooks::Hooks;
use crate::http_gate::HttpGate;
use crate::mcp::{ActiveServers, AvailableMcp, McpScopes};
use crate::mode::{Limits, ModeTable};
use crate::pending::PendingDecisions;
use crate::policy::{GrantStore, PermissionResolver};
use crate::run_limits::DenialTracker;
use crate::seam;
use crate::skills::SkillRegistry;
use crate::tool_advertising::SharedAdvertisingState;
use crate::tool_names::REQUEST_MODE_TOOL;
use crate::tools::{SharedRegistry, ToolRegistry};

use super::EscapeRoot;

#[cfg(feature = "rhai")]
pub(crate) use script::Posture;
pub(crate) use script::ScriptCall;

/// Everything one session's calls are graded and executed against, owned so
/// it moves into the detached task (and, for a script, lives for the run).
/// Built by `ladder::graded` from the executor's shared state.
pub(crate) struct Dispatcher {
    pub holly: Holly,
    /// The registry snapshot taken before spawning (#372).
    pub tools: ToolRegistry,
    /// The live registry — a lazy MCP connect registers into it and
    /// re-snapshots (ADR-0201).
    pub registry: SharedRegistry,
    pub skills: Arc<RwLock<Arc<SkillRegistry>>>,
    pub active_skill: Arc<Mutex<HashSet<SessionId>>>,
    pub resolver: Arc<dyn PermissionResolver>,
    /// The ancestor chain, nearest first (ADR-0024).
    pub chain: Vec<SessionId>,
    pub grants: Arc<dyn GrantStore>,
    pub hooks: Arc<Hooks>,
    pub pending: PendingDecisions,
    pub escape_root: Option<EscapeRoot>,
    /// The tool overlays of the chain's sessions (#539/#628/#634), looked up
    /// per call since one script calls many tools.
    pub overlays: HashMap<SessionId, Vec<ToolOverlayEntry>>,
    pub ceiling: PermissionProfile,
    pub advertising: SharedAdvertisingState,
    pub validation: Arc<LoopBreaker>,
    pub mcp_avail: Arc<AvailableMcp>,
    pub mcp_active: ActiveServers,
    pub mcp_http: Option<entanglement_core::HttpClient>,
    pub mcp_scopes: Option<Arc<McpScopes>>,
    pub session: SessionId,
    /// The session's permission mode — grants are scoped to it (ADR-0207 §8).
    pub mode: String,
    /// Every session's mode as the executor folded it — a script re-reads
    /// its session's before each nested call (ADR-0241, ADR-0243).
    pub perm_modes: Arc<Mutex<HashMap<SessionId, String>>>,
    /// Every compaction lineage's overlay epoch, as the executor folded its
    /// modes (ADR-0243).
    pub mode_epochs: super::mode_epochs::ModeEpochs,
    /// Each `overlays` entry's lineage head and that lineage's epoch when the
    /// entry was taken: the overlay is mode-scoped, so a script drops an
    /// entry once its lineage epoch moved (ADR-0243).
    pub overlay_epochs: super::mode_epochs::Snapshot,
    /// Read only for an explicit `http(...)` rule (ADR-0227).
    pub mode_table: Arc<ModeTable>,
    pub limits: Limits,
    /// Who watches which session: an attended chain never collapses an
    /// `Ask` and parks with no deadline (ADR-0247).
    pub attendance: Attendance,
    pub denials: Arc<DenialTracker>,
    pub http_gate: Arc<HttpGate>,
    /// `rhai.trusted` folders and skill dirs, canonical (ADR-0228): what a
    /// launch trusts and what a file-tool write is floored at `Ask` for.
    pub trust: Arc<crate::rhai_trust::Trust>,
    /// The project root a relative file-tool `path` resolves against.
    pub files_root: std::path::PathBuf,
    /// A background script's pins on the seq counters of every session it
    /// has run as — its launch session, then each compaction successor it
    /// followed (ADR-0241) — so its late cards keep real seqs however many
    /// other sessions retire meanwhile (#891, ADR-0244). Empty for every
    /// other run; dropped with the dispatcher when the script ends.
    pub seq_leases: Vec<entanglement_core::SeqLease>,
}

/// One call to grade and run.
pub(crate) struct Call {
    pub request_id: String,
    pub tool: String,
    pub input: String,
    /// The call as the model emitted it when core unwrapped an `mcp`
    /// envelope (ADR-0226) — for the duplicate-key re-scan; `None` otherwise.
    pub envelope: Option<ToolEnvelope>,
    /// The `mcp::<server>::<tool>` path a script called this by — what its
    /// arg-validate decline shows; `None` for every other call.
    pub rhai_path: Option<String>,
}

/// Who issued a call: the model (owed a `ToolResult`) or a running script.
// The lean build has no script engine, so nothing constructs `Script` there.
#[cfg_attr(not(feature = "rhai"), allow(dead_code))]
#[derive(Clone, Copy)]
pub(crate) enum Via<'a> {
    Model,
    Script(&'a ScriptCall),
}

/// What a call came to. `None` from [`Dispatcher::run`] means a `Stop`
/// unwound it: core cancels the turn on the same Stop, so nothing is owed.
pub(crate) enum Outcome {
    /// A gate declined it (mode deny, overlay withdrawal, hook veto,
    /// rejection, unknown tool, unreachable from a script…) — it never ran.
    Refused(String),
    /// A script's nested call was cut off by the script's budget mid-run: it
    /// started, so whether it took effect is unknown — never "it never ran".
    TimedOut(String),
    /// It ran (or failed validation at the door); `is_error` per ADR-0176.
    Ran {
        content: Vec<ContentPart>,
        is_error: bool,
        duration_ms: Option<u64>,
        exit_code: Option<i32>,
    },
}

impl Outcome {
    fn text(text: String, is_error: bool) -> Self {
        Outcome::Ran {
            content: if text.is_empty() {
                Vec::new()
            } else {
                vec![ContentPart::text(text)]
            },
            is_error,
            duration_ms: None,
            exit_code: None,
        }
    }
}

impl Dispatcher {
    /// A model call: run it through the seam and reply to core.
    pub(crate) async fn dispatch(self, call: Call) {
        // `request_mode` (ADR-0207 §10) is a Control pseudo-tool that always
        // parks its own approval and replies itself.
        if call.tool == REQUEST_MODE_TOOL {
            crate::request_mode::run_request_mode(
                &self.holly,
                &self.pending,
                self.session.clone(),
                call.request_id,
                call.input,
                self.mode.clone(),
            )
            .await;
            return;
        }
        let request_id = call.request_id.clone();
        let session = self.session.clone();
        match self.run(call, Via::Model).await {
            None => {}
            Some(Outcome::Refused(text) | Outcome::TimedOut(text)) => {
                seam::reply(&self.holly, session, request_id, text, true).await;
            }
            Some(Outcome::Ran {
                content,
                is_error,
                duration_ms,
                exit_code,
            }) => {
                seam::reply_content(
                    &self.holly,
                    session,
                    request_id,
                    content,
                    is_error,
                    duration_ms,
                    exit_code,
                )
                .await;
            }
        }
    }
}
