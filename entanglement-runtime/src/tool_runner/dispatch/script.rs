//! What the dispatch seam knows about a script-issued call (ADR-0227): the
//! per-run approval cache, the detached flag, the run's deadline, which
//! tools a script may never reach, and how a background script follows its
//! session's compaction (ADR-0241).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use entanglement_core::{SessionId, ToolOverlayEntry};

use crate::capability::{self, Capability};
use crate::tool_names::{MCP_TOOL, RHAI_TOOL};
use crate::tools::ToolRegistry;

use super::{Dispatcher, Via};

/// One script run's state at the seam.
pub(crate) struct ScriptCall {
    /// Calls the user approved this run, keyed on tool + full input — an
    /// approved `edit` of one file never clears an edit of another.
    approved: Mutex<HashSet<String>>,
    /// A `background: true` run (#637): no session state transitions.
    pub detached: bool,
    posture: Posture,
    /// When the script's own wall-clock budget runs out.
    deadline: Instant,
}

/// How a script run's nested calls are cleared (ADR-0228).
// The lean build has no script engine, so only tests construct a posture.
#[cfg_attr(not(feature = "rhai"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Posture {
    /// Graded exactly like direct calls; an `Ask` prompts per call.
    AskPerCommand,
    /// The user chose **Run all** on the launch card: every nested `Ask`
    /// passes, except a floored write (its own card).
    RunAll,
    /// A `rhai.trusted` script: like Run all, floored writes included.
    Trusted,
}

impl ScriptCall {
    #[cfg_attr(not(feature = "rhai"), allow(dead_code))]
    pub(crate) fn new(detached: bool, posture: Posture, deadline: Instant) -> Self {
        Self {
            approved: Mutex::default(),
            detached,
            posture,
            deadline,
        }
    }

    /// Force-allow this run's nested calls. Explicit denies still win.
    pub(crate) fn run_all(&self) -> bool {
        self.posture != Posture::AskPerCommand
    }

    pub(crate) fn trusted(&self) -> bool {
        self.posture == Posture::Trusted
    }

    pub(super) fn was_approved(&self, tool: &str, input: &str) -> bool {
        self.approved
            .lock()
            .is_ok_and(|set| set.contains(&cache_key(tool, input)))
    }

    pub(super) fn approve(&self, tool: &str, input: &str) {
        if let Ok(mut set) = self.approved.lock() {
            set.insert(cache_key(tool, input));
        }
    }
}

impl Dispatcher {
    /// Before each nested call of a background run (ADR-0241): switch to the
    /// live successor of the run's session once the executor has folded its
    /// mode, then re-read the session's current mode, returning the new
    /// session on a switch. Between calls, never mid-call, so a call already
    /// graded or parked as the predecessor finishes as it began (ADR-0238
    /// moves its card). A blocking script's turn is parked on its own
    /// session, which can't compact under it, so only a detached run follows.
    #[cfg_attr(not(feature = "rhai"), allow(dead_code))]
    pub(crate) fn follow_session(&mut self, script: &ScriptCall) -> Option<SessionId> {
        if !script.detached {
            return None;
        }
        let switched = self.followable();
        if let Some(to) = &switched {
            rekey(&mut self.session, &mut self.chain, &mut self.overlays, to);
        }
        if let Some(mode) = self.mode_of(&self.session) {
            drop_stale_overlay(&mut self.overlays, &self.session, &self.overlay_mode, &mode);
            if mode != self.mode {
                self.limits = self
                    .mode_table
                    .get(&mode)
                    .map(|m| m.limits)
                    .unwrap_or_default();
                self.mode = mode;
            }
        }
        switched
    }

    /// The session an approval's session grant and `http` host unlock are
    /// recorded under. A background script's card can be parked as the
    /// predecessor and answered after the compaction; recorded there, the
    /// grant would be lost with it, so it goes where the script's next call
    /// will be graded: [`followable`](Self::followable), else its session.
    pub(super) fn earner(&self, via: Via<'_>) -> SessionId {
        match via {
            Via::Script(sc) if sc.detached => {
                self.followable().unwrap_or_else(|| self.session.clone())
            }
            _ => self.session.clone(),
        }
    }

    /// The session a background script switches to (ADR-0241): the end of
    /// its session's compaction chain (`successor_of` already walks to the
    /// last link), once that is live and the executor has folded its mode.
    /// `None` when it never compacted; when the successor is not live (not
    /// started yet — a card there would carry seq 0 — or hibernated); or
    /// while its mode is unfolded — the resolver fails closed on an unseen
    /// session, and the serial fold handled its `SessionStarted` (which
    /// moves the predecessor's cards) first. One condition for the switch
    /// and for where a grant is recorded, so a grant always lands where the
    /// next call looks it up.
    fn followable(&self) -> Option<SessionId> {
        // The live end a detached card is homed on too (ADR-0242).
        Some(crate::pending::live_end(&self.holly, &self.session))
            .filter(|s| s != &self.session && self.mode_of(s).is_some())
    }

    /// `session`'s mode as the executor last folded it.
    fn mode_of(&self, session: &SessionId) -> Option<String> {
        self.perm_modes.lock().ok()?.get(session).cloned()
    }
}

/// Re-key a snapshot from `session` to `to`: the session itself, its slot in
/// the ancestor chain, and its own overlay entry.
#[cfg_attr(not(feature = "rhai"), allow(dead_code))]
fn rekey(
    session: &mut SessionId,
    chain: &mut [SessionId],
    overlays: &mut HashMap<SessionId, Vec<ToolOverlayEntry>>,
    to: &SessionId,
) {
    let from = std::mem::replace(session, to.clone());
    for link in chain.iter_mut().filter(|s| **s == from) {
        *link = to.clone();
    }
    if let Some(own) = overlays.remove(&from) {
        overlays.insert(to.clone(), own);
    }
}

/// The overlay is mode-scoped (ADR-0207 §8): the executor drops a session's
/// entry on a mode change, so the snapshot's own entry, taken under
/// `snapshot_mode`, goes once the session's mode is another.
#[cfg_attr(not(feature = "rhai"), allow(dead_code))]
fn drop_stale_overlay(
    overlays: &mut HashMap<SessionId, Vec<ToolOverlayEntry>>,
    session: &SessionId,
    snapshot_mode: &str,
    mode: &str,
) {
    if mode != snapshot_mode {
        overlays.remove(session);
    }
}

/// Tools never cut by the bridge clock (see [`Via::budget`]).
const UNBOUNDED: &[&str] = &["bash", "call", "write", "edit", "apply_patch"];

/// How much of the script's budget is left for it after a nested call is cut.
const GRACE: Duration = Duration::from_millis(300);

fn cache_key(tool: &str, input: &str) -> String {
    format!("{tool}\u{0}{input}")
}

impl Via<'_> {
    /// The bridge-level timeout a nested call runs under: the script's
    /// remaining budget less a grace margin, so the script still gets to
    /// handle the error before its own deadline ends it. Exempt: `bash`/
    /// `call`, whose own `timeout` parameter is already derived from the
    /// budget (#419), and the file-mutating tools, which a dropped future
    /// could cut between backup and write (ADR-0212).
    pub(super) fn budget(&self, tool: &str) -> Option<Duration> {
        match self {
            Via::Script(sc) if !UNBOUNDED.contains(&tool) => Some(
                sc.deadline
                    .saturating_duration_since(Instant::now())
                    .saturating_sub(GRACE),
            ),
            _ => None,
        }
    }
}

/// Why `tool` is out of a script's reach, or `None` when it may be called:
/// nested `rhai`, the `mcp` envelope (a script calls `mcp::<server>::<tool>`
/// instead), and every runtime-owned/`Control` tool — those orchestrate the
/// session, and a script is not the session.
pub(super) fn unreachable(tool: &str, tools: &ToolRegistry) -> Option<String> {
    let owned = tool == RHAI_TOOL
        || tool == MCP_TOOL
        || capability::runtime_owned(tool).is_some()
        || crate::plan_tasks::is_state_tool(tool)
        || crate::tool_names::DISCOVERY_TOOLS.contains(&tool)
        || capability::capability_of(tool, tools) == Some([Capability::Control].as_slice());
    owned.then(|| format!("`{tool}` cannot be called from a script"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_keys_on_the_full_input() {
        let sc = ScriptCall::new(false, Posture::AskPerCommand, Instant::now());
        sc.approve("edit", r#"{"path":"a"}"#);
        assert!(sc.was_approved("edit", r#"{"path":"a"}"#));
        assert!(!sc.was_approved("edit", r#"{"path":"b"}"#));
        assert!(!sc.was_approved("write", r#"{"path":"a"}"#));
    }

    #[test]
    fn rekey_moves_the_session_its_chain_slot_and_its_overlay() {
        let (p, s, parent) = (
            SessionId::new("p"),
            SessionId::new("s"),
            SessionId::new("parent"),
        );
        let entry = ToolOverlayEntry::ask;
        let mut session = p.clone();
        let mut chain = vec![p.clone(), parent.clone()];
        let mut overlays = HashMap::from([
            (p.clone(), vec![entry("bash")]),
            (parent.clone(), vec![entry("read")]),
        ]);
        rekey(&mut session, &mut chain, &mut overlays, &s);
        assert_eq!(session, s);
        assert_eq!(chain, vec![s.clone(), parent.clone()]);
        assert!(!overlays.contains_key(&p));
        assert_eq!(overlays[&s][0].pattern, "bash");
        assert_eq!(overlays[&parent][0].pattern, "read", "an ancestor's stays");
    }

    #[test]
    fn a_mode_change_drops_only_the_sessions_own_snapshot_overlay() {
        let (s, parent) = (SessionId::new("s"), SessionId::new("parent"));
        let mut overlays = HashMap::from([
            (s.clone(), vec![ToolOverlayEntry::ask("write")]),
            (parent.clone(), vec![ToolOverlayEntry::ask("read")]),
        ]);
        drop_stale_overlay(&mut overlays, &s, "build", "build");
        assert!(overlays.contains_key(&s), "same mode keeps it");
        drop_stale_overlay(&mut overlays, &s, "build", "plan");
        assert!(!overlays.contains_key(&s));
        assert!(overlays.contains_key(&parent), "an ancestor's stays");
    }

    #[test]
    fn runtime_owned_tools_and_envelopes_are_unreachable() {
        let tools = ToolRegistry::new();
        for tool in [
            "rhai",
            "mcp",
            "agent",
            "agent_send",
            "poll",
            "ask_user",
            "propose_plan",
            "request_mode",
            "update_tasks",
            "list_mcp",
            "mcp_tools",
            "load_skill",
        ] {
            assert!(unreachable(tool, &tools).is_some(), "{tool}");
        }
        for tool in ["read", "bash", "http", "mcp__gh__search"] {
            assert!(unreachable(tool, &tools).is_none(), "{tool}");
        }
    }

    #[test]
    fn bash_and_call_keep_their_own_timeout() {
        let sc = ScriptCall::new(
            false,
            Posture::AskPerCommand,
            Instant::now() + Duration::from_secs(5),
        );
        let via = Via::Script(&sc);
        for exempt in ["bash", "call", "write", "edit", "apply_patch"] {
            assert!(via.budget(exempt).is_none(), "{exempt}");
        }
        assert!(via
            .budget("read")
            .is_some_and(|d| d <= Duration::from_secs(5)));
        assert!(Via::Model.budget("read").is_none());
    }
}
