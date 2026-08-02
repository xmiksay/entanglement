//! Sub-agent spawn orchestration (#60, ADR-0021/0010; non-blocking #89, ADR-0026;
//! `agent_*` tool family + blocking `agent`, #120, ADR-0033).
//!
//! The `agent_*` family are not filesystem tools in the [`ToolRegistry`] — they
//! are engine-coordination primitives owned by the runtime:
//!
//! - `agent_spawn` (renamed from `spawn_agent`) — [`launch_subagent`] creates a
//!   child session via [`InMsg::Spawn`] and replies to the parent *immediately*
//!   with the child's handle (`agent_id`); it does **not** wait for the child's
//!   `Done`, so it never blocks the parent turn (ADR-0026 supersedes ADR-0022's
//!   synchronous answer-relay). It then keeps watching the child in the same
//!   detached task, recording the final answer + duration into the shared
//!   [`AgentRegistry`] keyed by the handle; the parent collects it later with
//!   `agent_poll` (see [`crate::agent_poll`]).
//! - `agent` (blocking, #120) — [`run_agent`] runs the exact same launch path
//!   (guard, clamp, `Spawn`), but instead of handing back the handle it parks on
//!   the child's *genuine* completion and folds the child's answer + elapsed
//!   straight into the `ToolOutput` — the one-call path for a single delegation.
//!   It still records into the registry, so a parent `Stop` while parked leaves
//!   the child collectable via `agent_poll`.
//!
//! Both routes share [`collect_child_answer`], which waits past an errored turn
//! with no usable answer instead of unblocking the parent on it (#562): the
//! child session stays alive and steerable, and the parent parks until the
//! child's next turn genuinely finishes, or the child ends. See the function's
//! own doc for the detail.
//!
//! Because they only orchestrate sessions (they touch no host resource), the
//! executor runs them *before* permission resolution — they bypass the permission
//! profile exactly like the runtime's `propose_plan` / `update_tasks` state tools.

use std::collections::{HashMap, HashSet};

use entanglement_core::{
    AgentProfile, AgentState, Holly, InMsg, OutEvent, ProfileRegistry, SessionId, ToolSpec,
};
use tokio::sync::broadcast::{error::RecvError, Receiver};

use crate::agent_poll::{AgentRegistry, AgentStatus};
use crate::seam::reply;
use crate::tool_names::{AGENT_SPAWN_TOOL, AGENT_TOOL};

/// Maximum spawn nesting: the root (user-initiated) session is depth 0, so this
/// lets the root spawn a child (depth 1), that child spawn (depth 2), and so on
/// up to and including depth `MAX_SPAWN_DEPTH`. A spawn that would exceed it is
/// refused. Bounds unbounded recursion — a sub-agent that keeps calling
/// `agent_spawn` (#76, follow-up to ADR-0022).
const MAX_SPAWN_DEPTH: usize = 3;

/// Maximum sub-agents spawned beneath a single root, summed across the whole
/// tree. Cumulative and never decremented — sequential spawns count too, so a
/// session cannot dodge the cap by letting each child finish before the next.
const MAX_SPAWNS_PER_ROOT: usize = 16;

/// Tracks the live session tree so the runtime can bound sub-agent spawning
/// (#76). Fed each `SessionStarted` (for the parent link) and consulted on every
/// `agent_spawn`/`agent` call before a child is started. Lives in the tool executor's
/// single-threaded event loop, so it needs no synchronization.
///
/// A **sponsored child** (ADR-0138) has a parent-child link but its permission
/// resolution stops at the child — it runs with its own profile's permissions,
/// no ancestor walk. Authorization is user approval of a plan (`propose_plan`
/// accept), which spawns a `build` child of the read-only `plan` session.
/// Sponsored spawns are exempt from `MAX_SPAWNS_PER_ROOT` (sequential,
/// user-authorized) but still bounded by `MAX_SPAWN_DEPTH`.
#[derive(Default)]
pub struct SpawnGuard {
    /// child → parent, from `SessionStarted`. Absent or `None` ⇒ a root.
    parents: HashMap<SessionId, Option<SessionId>>,
    /// root → cumulative sub-agents spawned beneath it (never decremented).
    spawns_per_root: HashMap<SessionId, usize>,
    /// Sessions spawned as **sponsored** (ADR-0138) — permission roots despite
    /// having a parent link. Authorization is user plan approval, not the
    /// ancestor chain, so [`crate::permission::effective_permission`] skips the
    /// ancestor walk for these. Exempt from `MAX_SPAWNS_PER_ROOT` but not from
    /// `MAX_SPAWN_DEPTH`.
    sponsored: HashSet<SessionId>,
}

impl SpawnGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a session's parent from its `SessionStarted` event.
    pub fn record_start(&mut self, session: SessionId, parent: Option<SessionId>) {
        self.parents.insert(session, parent);
    }

    /// The recorded parent of `session`, if any. Lets the tool executor walk a
    /// child's ancestry to clamp its permissions to the parent chain (#77).
    pub fn parent_of(&self, session: &SessionId) -> Option<SessionId> {
        self.parents.get(session).cloned().flatten()
    }

    /// Record a sponsored spawn (ADR-0138): establishes the parent link and
    /// marks `child` as a permission root, so [`effective_permission`][crate::permission::effective_permission]
    /// resolves its own profile without walking ancestors. Called *before* the
    /// `InMsg::Spawn` is sent so the link is in place by the time the child's
    /// first `ToolExec` arrives.
    pub fn record_sponsored_start(&mut self, child: SessionId, parent: SessionId) {
        self.parents.insert(child.clone(), Some(parent));
        self.sponsored.insert(child);
    }

    /// Whether `session` was spawned as a sponsored child (ADR-0138) — a
    /// permission root despite having a parent link.
    pub fn is_sponsored(&self, session: &SessionId) -> bool {
        self.sponsored.contains(session)
    }

    /// Decide whether `parent` may spawn another sub-agent. On approval, charges
    /// the spawn against the root's budget and returns `Ok`. On refusal, returns
    /// the message to relay to the parent as the `agent_spawn`/`agent` tool output.
    pub fn try_spawn(&mut self, parent: &SessionId) -> Result<(), String> {
        let child_depth = self.depth(parent) + 1;
        if child_depth > MAX_SPAWN_DEPTH {
            return Err(format!(
                "sub-agent spawn refused: max spawn depth ({MAX_SPAWN_DEPTH}) reached — \
                 this sub-agent is too deeply nested to spawn another. Do the work directly."
            ));
        }
        let root = self.root_of(parent);
        let count = self.spawns_per_root.entry(root).or_insert(0);
        if *count >= MAX_SPAWNS_PER_ROOT {
            return Err(format!(
                "sub-agent spawn refused: per-root spawn budget ({MAX_SPAWNS_PER_ROOT}) \
                 exhausted — too many sub-agents already spawned in this session tree. \
                 Do the work directly."
            ));
        }
        *count += 1;
        Ok(())
    }

    /// Decide whether `parent` may sponsor another sub-agent (ADR-0138).
    /// Sponsored spawns are exempt from `MAX_SPAWNS_PER_ROOT` — they are
    /// sequential and individually user-authorized (each `propose_plan`
    /// approval spawns one build child), so a long plan/build cycle can't
    /// exhaust the fan-out budget meant for unattended `agent_spawn` fan-out.
    /// `MAX_SPAWN_DEPTH` still applies: a sponsored build nested three levels
    /// deep still can't sponsor further. On approval records nothing — the
    /// caller follows up with [`record_sponsored_start`] once the child id is
    /// minted.
    pub fn try_sponsor_spawn(&self, parent: &SessionId) -> Result<(), String> {
        let child_depth = self.depth(parent) + 1;
        if child_depth > MAX_SPAWN_DEPTH {
            return Err(format!(
                "sponsored spawn refused: max spawn depth ({MAX_SPAWN_DEPTH}) reached — \
                 this sub-agent is too deeply nested to sponsor another."
            ));
        }
        Ok(())
    }

    /// Number of ancestors of `session` (a root is depth 0). The `visited` set
    /// guards against a malformed cycle in the parent links.
    fn depth(&self, session: &SessionId) -> usize {
        let mut depth = 0;
        let mut current = session.clone();
        let mut visited = HashSet::new();
        while visited.insert(current.clone()) {
            match self.parents.get(&current).cloned().flatten() {
                Some(parent) => {
                    depth += 1;
                    current = parent;
                }
                None => break,
            }
        }
        depth
    }

    /// Walk to the root of `session`'s tree (itself if it has no parent).
    fn root_of(&self, session: &SessionId) -> SessionId {
        let mut current = session.clone();
        let mut visited = HashSet::new();
        while visited.insert(current.clone()) {
            match self.parents.get(&current).cloned().flatten() {
                Some(parent) => current = parent,
                None => break,
            }
        }
        current
    }
}

/// Sub-agent profile used when the model omits `agent` — read-only explore is
/// the safe default.
const DEFAULT_SUBAGENT: &str = "explore";

/// The per-profile spawn tool specs (#119, ADR-0040): the `agent_spawn`/`agent`/
/// `agent_poll` triple advertised to a session running under `profile`, with the
/// roster + `agent` enum scoped to exactly the profiles `profile` may spawn (its
/// `spawnable_agents` allowlist ∩ the target-side mode gate). Empty when the
/// profile may not spawn or has no valid targets — so the whole family is
/// **withheld** from that session's model (the structural half of the gate; the
/// runtime executor refuses a stale call regardless). Stored in
/// [`EngineConfig::profile_tool_specs`][entanglement_core::EngineConfig] and
/// appended by core's `run_turn` for the active profile.
pub fn spawn_specs_for(profile: &AgentProfile, registry: &ProfileRegistry) -> Vec<ToolSpec> {
    if !profile.may_spawn() {
        return Vec::new();
    }
    // A valid target is spawnable-mode (subagent/all) *and* on this profile's
    // allowlist — checked against `profile`'s own list, so the roster is not
    // transitive down the tree (each hop re-checks the spawner).
    let targets: Vec<&AgentProfile> = registry
        .iter()
        .filter(|t| t.spawnable_as_subagent() && profile.spawn_target_allowed(&t.name))
        .collect();
    if targets.is_empty() {
        return Vec::new();
    }
    vec![
        agent_spawn_spec(&targets),
        agent_spec(&targets),
        crate::agent_poll::agent_poll_spec(),
    ]
}

/// The `agent_spawn` tool schema advertised to the model. The `targets` roster is
/// disclosed inline (#112): each spawnable agent's `name: description` is listed
/// in the tool description and the `agent` argument is constrained to that set.
pub fn agent_spawn_spec(targets: &[&AgentProfile]) -> ToolSpec {
    ToolSpec::with_schema(
        AGENT_SPAWN_TOOL,
        format!(
            "Launch a sub-agent session to handle a focused subtask. Returns \
             immediately with an agent_id handle (it does not wait for the \
             sub-agent to finish), so you can launch several in a row and let \
             them run concurrently. Collect a sub-agent's answer by calling \
             agent_poll with its agent_id. To delegate a single subtask and get \
             the answer in one call, use `agent` instead.\n\n{}",
            roster(targets)
        ),
        agent_input_schema(targets),
    )
}

/// The blocking `agent` tool schema (#120). Same input shape as `agent_spawn`,
/// but it waits for the sub-agent and returns its final answer directly. The
/// roster is disclosed once, on `agent_spawn` — repeating it here would bill
/// every request twice for the same lines (`targets` still drives the enum).
pub fn agent_spec(targets: &[&AgentProfile]) -> ToolSpec {
    ToolSpec::with_schema(
        AGENT_TOOL,
        "Delegate a focused subtask to a sub-agent and wait for its answer. \
         Spawns the sub-agent, blocks until it finishes, and returns its \
         final answer directly — the one-call path for a single delegation. \
         To launch several sub-agents and let them run concurrently, use \
         agent_spawn + agent_poll instead. Takes the same agents and arguments \
         as `agent_spawn` — see its description for the roster.",
        agent_input_schema(targets),
    )
}

/// The `name: description` roster line block disclosed to the spawning model —
/// `description` is the only field of a definition a parent ever sees (#112).
/// Scoped to the profiles this spawner may target (#119).
fn roster(targets: &[&AgentProfile]) -> String {
    let mut out = String::from("Available agents:");
    for p in targets {
        out.push_str(&format!("\n- {}: {}", p.name, p.description));
    }
    out
}

/// Shared `{ agent, prompt }` input schema for the `agent_spawn` and `agent`
/// tools — both take the same arguments; only their return shape differs. The
/// `agent` name is constrained to `targets` (an enum) so the model can only pick
/// a profile it is actually allowed to spawn (#119).
fn agent_input_schema(targets: &[&AgentProfile]) -> serde_json::Value {
    let names: Vec<&str> = targets.iter().map(|p| p.name.as_str()).collect();
    serde_json::json!({
        "type": "object",
        "properties": {
            "agent": {
                "type": "string",
                "enum": names,
                "description": "Which agent profile to run the sub-agent under. Defaults to explore (read-only)."
            },
            "prompt": {
                "type": "string",
                "description": "The task or question for the sub-agent to work on."
            }
        },
        "required": ["agent", "prompt"]
    })
}

/// Whether a launch hands the handle back immediately (`agent_spawn`) or parks
/// for the child's answer and returns it directly (`agent`, #120).
#[derive(Clone, Copy, PartialEq, Eq)]
enum LaunchMode {
    /// Non-blocking: reply the handle at once, then record the answer for poll.
    Detached,
    /// Blocking: record the answer, then reply it (with timing) to the parent.
    AwaitAnswer,
}

/// Orchestrate one `agent_spawn` call (ADR-0026): start a child session, reply
/// to `parent` *immediately* with the child handle, then keep watching the child
/// and record its answer + duration into `registry` for a later `agent_poll`.
///
/// `events` must be a receiver subscribed *before* the [`InMsg::Spawn`] is sent
/// (the caller subscribes synchronously), so the child's events — including its
/// terminal `Done` — cannot race ahead of the watcher.
pub async fn launch_subagent(
    holly: Holly,
    events: Receiver<OutEvent>,
    registry: AgentRegistry,
    parent: SessionId,
    request_id: String,
    input: String,
) {
    launch(
        holly,
        events,
        registry,
        parent,
        request_id,
        input,
        LaunchMode::Detached,
    )
    .await;
}

/// Orchestrate one blocking `agent` call (#120): run the exact `agent_spawn`
/// launch path, then park on the child's genuine completion ([`collect_child_answer`])
/// and fold its answer + elapsed straight into the `ToolOutput`. Still records
/// into `registry`, so a parent `Stop` while parked leaves the child collectable
/// via `agent_poll`.
pub async fn run_agent(
    holly: Holly,
    events: Receiver<OutEvent>,
    registry: AgentRegistry,
    parent: SessionId,
    request_id: String,
    input: String,
) {
    launch(
        holly,
        events,
        registry,
        parent,
        request_id,
        input,
        LaunchMode::AwaitAnswer,
    )
    .await;
}

/// Shared launch path for `agent_spawn` (`Detached`) and `agent` (`AwaitAnswer`).
/// The two differ only in *when* and *what* they reply: a detached launch hands
/// the handle back before watching the child; a blocking launch watches first,
/// then replies the answer. Both record the answer into `registry`.
async fn launch(
    holly: Holly,
    mut events: Receiver<OutEvent>,
    registry: AgentRegistry,
    parent: SessionId,
    request_id: String,
    input: String,
    mode: LaunchMode,
) {
    let (agent, prompt) = parse_input(&input);
    let child = SessionId::new_uuid();
    // Register *before* sending Spawn so a poll can never precede the handle
    // (the parent only learns the id from the reply below, which comes after).
    let (status_tx, started) = registry.register(child.clone());

    if holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(parent.clone()),
            predecessor: None,
            agent: agent.clone(),
            prompt,
            user: None,
        })
        .await
        .is_err()
    {
        registry.forget(&child);
        reply(
            &holly,
            parent,
            request_id,
            "sub-agent spawn failed: engine inbox closed".to_string(),
        )
        .await;
        return;
    }

    // Non-blocking: hand the handle back now — the parent turn continues instead
    // of blocking on the child's `Done` (ADR-0026 supersedes ADR-0022's relay).
    if mode == LaunchMode::Detached {
        reply(
            &holly,
            parent.clone(),
            request_id.clone(),
            format!(
                "Sub-agent launched under the `{agent}` profile. agent_id: {child}. \
                 Call agent_poll with this agent_id to await its answer."
            ),
        )
        .await;
    } else {
        // Blocking: the parent parks on the child's result — surface that as a
        // distinct state so a head can show "waiting for sub-agent" instead of
        // the ambiguous `Thinking` (ADR-0139).
        holly.emit_status(&parent, AgentState::WaitingAgent);
    }

    // Keep accumulating the child's answer; publish it (with timing) for poll.
    let answer = collect_child_answer(&holly, &parent, &mut events, &child).await;
    let elapsed = started.elapsed();
    // The registry keeps a receiver, so the completed value survives this drop —
    // a blocking `agent` whose parent `Stop`ped is still poll-able by handle.
    let _ = status_tx.send(AgentStatus::Complete {
        answer: answer.clone(),
        elapsed,
    });

    // Blocking: the parent parked on this call — fold the answer back directly.
    // If the parent already `Stop`ped, core cancels its turn and ignores this
    // reply; the answer above stays collectable via `agent_poll`.
    if mode == LaunchMode::AwaitAnswer {
        reply(
            &holly,
            parent,
            request_id,
            format!(
                "sub-agent `{child}` completed in {:.1}s:\n\n{answer}",
                elapsed.as_secs_f64()
            ),
        )
        .await;
    }
}

/// Watch the child's event stream, accumulating its assistant text until the
/// child *genuinely* finishes. Returns the final answer, or an explanatory note
/// when the child produced nothing. Public so the sponsored build launch in
/// `propose_plan.rs` can reuse the exact same accumulation (ADR-0138).
///
/// A turn's `Done` is **not** by itself the end of the wait (#562, amends
/// ADR-0111/ADR-0123/ADR-0138 — see ADR-0155): the engine emits `Done` even for
/// a turn that ended in `Error` (`emit_turn_error` fires both), so a child that
/// ran out of budget mid-answer would otherwise resolve the parent with a bare
/// "sub-agent ended with error" and let it conclude on top of a failed child. If
/// a turn's `Done` lands with no accumulated text *and* that turn surfaced an
/// `Error`, the child session is still alive and steerable (prompting it
/// "continue" starts a new turn) — this clears the per-turn text/error and keeps
/// watching instead of breaking, and tells `parent` why it's still parked so the
/// user knows to steer the child. The wait only ends definitively on a turn
/// whose `Done` carries a usable answer, or on the child's `SessionEnded`/
/// `SessionHibernated` — a lagging or closed watcher still breaks defensively so
/// a missed event can't park the parent forever.
pub async fn collect_child_answer(
    holly: &Holly,
    parent: &SessionId,
    events: &mut Receiver<OutEvent>,
    child: &SessionId,
) -> String {
    let mut text = String::new();
    // This turn's error, if any — consumed (`.take()`) whenever it gates a
    // "keep waiting" decision, so a later empty-and-error-free turn isn't
    // mistaken for a still-erroring one.
    let mut error: Option<String> = None;
    // The most recent turn's error, kept across a "keep waiting" reset purely
    // as the fallback message if the wait ends (`SessionEnded`/lagged/closed)
    // without ever landing an answer — losing it to "produced no output" would
    // erase the one piece of context the user has for why the child stalled.
    let mut last_error: Option<String> = None;
    loop {
        match events.recv().await {
            Ok(ev) if ev.session() != Some(child) => {}
            Ok(OutEvent::TextDelta { text: delta, .. }) => text.push_str(&delta),
            // An ambiguous-stop retry (ADR-0118) supersedes the truncated round:
            // drop the partial text so the final answer is the recovered round's
            // alone, not the discarded round concatenated onto it.
            Ok(OutEvent::AmbiguousRetry { .. }) => text.clear(),
            Ok(OutEvent::Error { message, .. }) => {
                last_error = Some(message.clone());
                error = Some(message);
            }
            Ok(OutEvent::Done { .. }) => {
                if text.trim().is_empty() {
                    if let Some(message) = error.take() {
                        text.clear();
                        holly.emit_for_session(parent, |seq| OutEvent::Error {
                            session: parent.clone(),
                            seq,
                            message: format!(
                                "sub-agent `{child}` ended its turn in error with no answer \
                                 ({message}); it is still alive — steer it (e.g. prompt it to \
                                 continue) to resolve this wait."
                            ),
                        });
                        continue;
                    }
                }
                break;
            }
            Ok(OutEvent::SessionEnded { .. }) | Ok(OutEvent::SessionHibernated { .. }) => break,
            Ok(_) => {}
            // A lagging watcher could miss the child's `Done` and park the parent
            // forever; surface what we have instead of blocking indefinitely.
            Err(RecvError::Lagged(_)) => break,
            Err(RecvError::Closed) => break,
        }
    }
    let text = text.trim();
    match (text.is_empty(), last_error) {
        (false, _) => text.to_string(),
        (true, Some(e)) => format!("sub-agent ended with error: {e}"),
        (true, None) => "sub-agent produced no output".to_string(),
    }
}

/// The spawn *target* named in an `agent_spawn`/`agent` tool input — the runtime
/// executor reads it to apply the per-profile allowlist + target-mode gate before
/// a child is minted (#119). Mirrors [`parse_input`]'s agent resolution (a bare
/// string / omitted `agent` ⇒ the read-only default).
pub fn target_agent(input: &str) -> String {
    parse_input(input).0
}

/// Parse the `agent_spawn`/`agent` tool input. Providers send a JSON object
/// `{"agent": …, "prompt": …}`; scripted/raw backends may send a bare string,
/// which is treated as the prompt under the default sub-agent profile.
fn parse_input(input: &str) -> (String, String) {
    match serde_json::from_str::<serde_json::Value>(input) {
        Ok(v) => {
            let agent = v
                .get("agent")
                .and_then(|a| a.as_str())
                .filter(|a| !a.is_empty())
                .unwrap_or(DEFAULT_SUBAGENT)
                .to_string();
            let prompt = v
                .get("prompt")
                .and_then(|p| p.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| input.to_string());
            (agent, prompt)
        }
        Err(_) => (DEFAULT_SUBAGENT.to_string(), input.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::EngineConfig;
    use std::time::Duration;

    fn empty_engine() -> Holly {
        Holly::spawn(EngineConfig::default())
    }

    /// Drain `sub` for up to a short deadline, returning the first event
    /// matching `pred`. Panics if none arrives — these tests only await events
    /// the code under test is expected to emit.
    async fn expect_event(
        sub: &mut Receiver<OutEvent>,
        pred: impl Fn(&OutEvent) -> bool,
    ) -> OutEvent {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match sub.recv().await.expect("event bus closed before match") {
                    ev if pred(&ev) => return ev,
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for expected event")
    }

    #[tokio::test]
    async fn collect_child_answer_waits_past_an_errored_turn_for_the_next_one() {
        // Repro shape for #562: the child's first turn errors with no usable
        // text (e.g. an exhausted 429 budget). The old code broke on that
        // turn's `Done` and folded the bare error in as the answer, unblocking
        // the parent on a failed child. The fix keeps watching — a second turn
        // (the child steered to "continue") with real text is what should
        // actually resolve the wait.
        let holly = empty_engine();
        let parent = SessionId::new("parent");
        let child = SessionId::new("child");
        let mut parent_events = holly.subscribe();
        let (tx, mut rx) = tokio::sync::broadcast::channel::<OutEvent>(16);

        let handle = {
            let holly = holly.clone();
            let parent = parent.clone();
            let child = child.clone();
            tokio::spawn(
                async move { collect_child_answer(&holly, &parent, &mut rx, &child).await },
            )
        };

        tx.send(OutEvent::Error {
            session: child.clone(),
            seq: 1,
            message: "out of tokens".to_string(),
        })
        .unwrap();
        tx.send(OutEvent::Done {
            session: child.clone(),
            seq: 2,
        })
        .unwrap();

        // The parent is told why it's still parked, not unblocked.
        let note = expect_event(
            &mut parent_events,
            |ev| matches!(ev, OutEvent::Error { session, .. } if session == &parent),
        )
        .await;
        match note {
            OutEvent::Error { message, .. } => {
                assert!(
                    message.contains("child"),
                    "should name the child: {message}"
                );
                assert!(
                    message.to_lowercase().contains("steer")
                        || message.to_lowercase().contains("continue"),
                    "should hint at steering: {message}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(
            !handle.is_finished(),
            "the wait must not have resolved on the errored turn's Done"
        );

        // The child is steered ("continue") and its next turn produces a real
        // answer.
        tx.send(OutEvent::TextDelta {
            session: child.clone(),
            seq: 3,
            text: "done for real".to_string(),
        })
        .unwrap();
        tx.send(OutEvent::Done {
            session: child.clone(),
            seq: 4,
        })
        .unwrap();

        let answer = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("collect_child_answer must resolve once the child truly finishes")
            .unwrap();
        assert_eq!(answer, "done for real");
    }

    #[tokio::test]
    async fn collect_child_answer_ends_on_session_ended_after_an_error() {
        // If the child never gets steered and its session simply ends, the
        // wait must still resolve — not hang forever.
        let holly = empty_engine();
        let parent = SessionId::new("parent2");
        let child = SessionId::new("child2");
        let (tx, mut rx) = tokio::sync::broadcast::channel::<OutEvent>(16);

        tx.send(OutEvent::Error {
            session: child.clone(),
            seq: 1,
            message: "out of tokens".to_string(),
        })
        .unwrap();
        tx.send(OutEvent::Done {
            session: child.clone(),
            seq: 2,
        })
        .unwrap();
        tx.send(OutEvent::SessionEnded {
            session: child.clone(),
            ts: 0,
        })
        .unwrap();

        let answer = tokio::time::timeout(
            Duration::from_secs(2),
            collect_child_answer(&holly, &parent, &mut rx, &child),
        )
        .await
        .expect("SessionEnded must end the wait");
        assert!(
            answer.contains("out of tokens"),
            "the best-effort error answer should surface: {answer}"
        );
    }

    #[tokio::test]
    async fn collect_child_answer_breaks_on_the_first_done_when_it_has_an_answer() {
        // The common case — no error at all — is unaffected: any `Done`
        // carrying usable text still ends the wait immediately.
        let holly = empty_engine();
        let parent = SessionId::new("parent3");
        let child = SessionId::new("child3");
        let (tx, mut rx) = tokio::sync::broadcast::channel::<OutEvent>(16);

        tx.send(OutEvent::TextDelta {
            session: child.clone(),
            seq: 1,
            text: "all good".to_string(),
        })
        .unwrap();
        tx.send(OutEvent::Done {
            session: child.clone(),
            seq: 2,
        })
        .unwrap();

        let answer = tokio::time::timeout(
            Duration::from_secs(1),
            collect_child_answer(&holly, &parent, &mut rx, &child),
        )
        .await
        .expect("a clean Done must resolve immediately");
        assert_eq!(answer, "all good");
    }

    #[test]
    fn parse_input_reads_json_object() {
        let (agent, prompt) = parse_input(r#"{"agent":"build","prompt":"do it"}"#);
        assert_eq!(agent, "build");
        assert_eq!(prompt, "do it");
    }

    #[test]
    fn parse_input_defaults_agent_to_explore() {
        let (agent, prompt) = parse_input(r#"{"prompt":"look around"}"#);
        assert_eq!(agent, DEFAULT_SUBAGENT);
        assert_eq!(prompt, "look around");
    }

    #[test]
    fn parse_input_falls_back_to_raw_string() {
        let (agent, prompt) = parse_input("just a prompt");
        assert_eq!(agent, DEFAULT_SUBAGENT);
        assert_eq!(prompt, "just a prompt");
    }

    #[test]
    fn spawn_specs_scope_the_enum_to_valid_targets() {
        // The default registry: build/plan (Primary, not targets), explore +
        // debug (Subagent, targets). `build` may spawn, so it gets the triple —
        // and both spawnable leaves are valid targets, so the enum lists them.
        let reg = crate::agents::built_in_registry().expect("built-in agents must parse");
        let build = reg.get("build").unwrap();
        let specs = spawn_specs_for(build, &reg);
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec![AGENT_SPAWN_TOOL, AGENT_TOOL, "agent_poll"]);
        let enum_names = specs[0].schema["properties"]["agent"]["enum"]
            .as_array()
            .unwrap();
        assert!(enum_names.iter().any(|n| n == "explore"));
        assert!(enum_names.iter().any(|n| n == "debug"));
        assert!(!enum_names.iter().any(|n| n == "build"));
        assert!(!enum_names.iter().any(|n| n == "plan"));
    }

    #[test]
    fn spawn_specs_empty_for_a_non_spawning_profile() {
        // `explore` is a Subagent leaf — it may not spawn, so it gets no family.
        let reg = crate::agents::built_in_registry().expect("built-in agents must parse");
        let explore = reg.get("explore").unwrap();
        assert!(spawn_specs_for(explore, &reg).is_empty());
    }

    /// Build a guard with a linear ancestry chain `root → a → b → …` recorded.
    fn guard_with_chain(chain: &[&str]) -> (SpawnGuard, Vec<SessionId>) {
        let mut guard = SpawnGuard::new();
        let ids: Vec<SessionId> = chain.iter().map(|c| SessionId::new(*c)).collect();
        for (i, id) in ids.iter().enumerate() {
            let parent = i.checked_sub(1).map(|p| ids[p].clone());
            guard.record_start(id.clone(), parent);
        }
        (guard, ids)
    }

    #[test]
    fn root_may_spawn_and_charges_the_budget() {
        let (mut guard, ids) = guard_with_chain(&["root"]);
        assert!(guard.try_spawn(&ids[0]).is_ok());
        assert_eq!(guard.spawns_per_root.get(&ids[0]).copied(), Some(1));
    }

    #[test]
    fn spawn_refused_past_max_depth() {
        // root(0) → a(1) → b(2) → c(3): c is at MAX_SPAWN_DEPTH, so its spawn
        // (which would be depth 4) is refused.
        let (mut guard, ids) = guard_with_chain(&["root", "a", "b", "c"]);
        let deepest = ids.last().unwrap();
        let err = guard.try_spawn(deepest).unwrap_err();
        assert!(err.contains("max spawn depth"), "got: {err}");
        // A shallower ancestor (depth 2 → child depth 3) is still allowed.
        assert!(guard.try_spawn(&ids[2]).is_ok());
    }

    #[test]
    fn spawn_refused_past_per_root_budget() {
        let (mut guard, ids) = guard_with_chain(&["root"]);
        for _ in 0..MAX_SPAWNS_PER_ROOT {
            assert!(guard.try_spawn(&ids[0]).is_ok());
        }
        let err = guard.try_spawn(&ids[0]).unwrap_err();
        assert!(err.contains("per-root spawn budget"), "got: {err}");
    }

    #[test]
    fn budget_is_shared_across_the_whole_tree() {
        // A grandchild's spawns count against the same root budget as the root's.
        let (mut guard, ids) = guard_with_chain(&["root", "child"]);
        guard.try_spawn(&ids[0]).unwrap();
        guard.try_spawn(&ids[1]).unwrap();
        assert_eq!(guard.spawns_per_root.get(&ids[0]).copied(), Some(2));
        assert!(!guard.spawns_per_root.contains_key(&ids[1]));
    }

    #[test]
    fn unknown_session_treated_as_root() {
        let mut guard = SpawnGuard::new();
        let orphan = SessionId::new("orphan");
        // No `record_start`: depth 0, its own root — the spawn is allowed.
        assert!(guard.try_spawn(&orphan).is_ok());
    }

    #[test]
    fn sponsored_spawn_records_parent_link_and_set() {
        let mut guard = SpawnGuard::new();
        let parent = SessionId::new("plan");
        let child = SessionId::new("build");
        guard.record_start(parent.clone(), None);
        guard.record_sponsored_start(child.clone(), parent.clone());
        assert_eq!(guard.parent_of(&child), Some(parent.clone()));
        assert!(guard.is_sponsored(&child));
        // The plan parent is not itself sponsored.
        assert!(!guard.is_sponsored(&parent));
    }

    #[test]
    fn try_sponsor_spawn_exceeds_depth_at_max() {
        // root(0) → a(1) → b(2) → c(3): c is at MAX_SPAWN_DEPTH, so its
        // sponsored spawn (which would be depth 4) is refused.
        let (guard, ids) = guard_with_chain(&["root", "a", "b", "c"]);
        let deepest = ids.last().unwrap();
        let err = guard.try_sponsor_spawn(deepest).unwrap_err();
        assert!(err.contains("max spawn depth"), "got: {err}");
        // A shallower ancestor (depth 2 → child depth 3) is still allowed.
        assert!(guard.try_sponsor_spawn(&ids[2]).is_ok());
    }

    #[test]
    fn sponsored_spawns_exhaust_never_hit_fanout_cap() {
        // N sequential sponsored spawns never trip MAX_SPAWNS_PER_ROOT —
        // sponsorship is exempt from the fan-out budget (ADR-0138). Sponsoring
        // far past the cap leaves the budget untouched, so plain spawns still
        // succeed afterward.
        let (mut guard, ids) = guard_with_chain(&["root"]);
        for _ in 0..(MAX_SPAWNS_PER_ROOT + 5) {
            assert!(
                guard.try_sponsor_spawn(&ids[0]).is_ok(),
                "sponsored spawns must not exhaust the fan-out budget"
            );
        }
        // Sponsored spawns didn't charge the budget: a plain spawn still works.
        assert!(
            guard.try_spawn(&ids[0]).is_ok(),
            "fan-out budget must be untouched by sponsored spawns"
        );
        // And exhausting it with plain spawns is still the ceiling for plain.
        for _ in 1..MAX_SPAWNS_PER_ROOT {
            guard.try_spawn(&ids[0]).unwrap();
        }
        let err = guard.try_spawn(&ids[0]).unwrap_err();
        assert!(err.contains("per-root spawn budget"), "got: {err}");
        // A sponsored spawn under an exhausted plain budget still succeeds.
        assert!(guard.try_sponsor_spawn(&ids[0]).is_ok());
    }
}
