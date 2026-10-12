//! The dispatch seam (issue #451, split by ADR-0227): one graded path for a
//! tool call, whoever issued it. [`Dispatcher::run`] grades and executes a
//! [`Call`] and hands back an [`Outcome`]; the caller decides what to do
//! with it — [`Dispatcher::dispatch`] folds it back to core as the
//! `ToolResult` a model call is owed, while a rhai binding returns it to the
//! script over the bridge (`crate::script`), so core never sees a result for
//! an id it never parked on.
//!
//! The ladder itself lives in [`gates`] (unknown/lazy-MCP resolution, alias
//! rewrite, the `Control` bypass, overlay deny, mode grade + grants, hooks,
//! escape-root, the credentialed-`http` gate), the approval park in
//! [`decision`], the run-against-the-registry step in [`execute`].

mod decision;
mod execute;
mod gates;
pub(crate) mod grade;
mod script;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use entanglement_core::{
    ContentPart, Holly, PermissionProfile, SessionId, ToolEnvelope, ToolOverlayEntry,
};

use crate::arg_validate::LoopBreaker;
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
    /// Read only for an explicit `http(...)` rule (ADR-0227).
    pub mode_table: Arc<ModeTable>,
    pub limits: Limits,
    pub denials: Arc<DenialTracker>,
    pub http_gate: Arc<HttpGate>,
}

/// One call to grade and run.
pub(crate) struct Call {
    pub request_id: String,
    pub tool: String,
    pub input: String,
    /// The call as the model emitted it when core unwrapped an `mcp`
    /// envelope (ADR-0226) — for the duplicate-key re-scan; `None` otherwise.
    pub envelope: Option<ToolEnvelope>,
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
