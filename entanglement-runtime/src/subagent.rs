//! Sub-agent spawn orchestration (#60, ADR-0021/0010; non-blocking #89, ADR-0026).
//!
//! `spawn_agent` is not a filesystem tool in the [`ToolRegistry`] — it is an
//! engine-coordination primitive owned by the runtime. When the model calls it,
//! [`launch_subagent`] creates a child session via [`InMsg::Spawn`] and replies
//! to the parent *immediately* with the child's handle (`agent_id`) — it does
//! **not** wait for the child's `Done`, so it never blocks the parent turn
//! (ADR-0026 supersedes ADR-0022's synchronous answer-relay). It then keeps
//! watching the child in the same detached task, recording the final answer +
//! duration into the shared [`AgentRegistry`] keyed by the handle; the parent
//! collects it later with `agent_poll` (see [`crate::agent_poll`]).
//!
//! Because it only orchestrates sessions (it touches no host resource), the
//! executor runs it *before* permission resolution — it bypasses the permission
//! profile exactly like core's `update_plan` / `update_tasks` built-ins.

use std::collections::{HashMap, HashSet};

use entanglement_core::{Holly, InMsg, OutEvent, SessionId, ToolSpec};
use tokio::sync::broadcast::{error::RecvError, Receiver};

use crate::agent_poll::{AgentRegistry, AgentStatus};

/// Tool name the model calls to spawn a sub-agent.
pub const SPAWN_TOOL: &str = "spawn_agent";

/// Maximum spawn nesting: the root (user-initiated) session is depth 0, so this
/// lets the root spawn a child (depth 1), that child spawn (depth 2), and so on
/// up to and including depth `MAX_SPAWN_DEPTH`. A spawn that would exceed it is
/// refused. Bounds unbounded recursion — a sub-agent that keeps calling
/// `spawn_agent` (#76, follow-up to ADR-0022).
const MAX_SPAWN_DEPTH: usize = 3;

/// Maximum sub-agents spawned beneath a single root, summed across the whole
/// tree. Cumulative and never decremented — sequential spawns count too, so a
/// session cannot dodge the cap by letting each child finish before the next.
const MAX_SPAWNS_PER_ROOT: usize = 16;

/// Tracks the live session tree so the runtime can bound sub-agent spawning
/// (#76). Fed each `SessionStarted` (for the parent link) and consulted on every
/// `spawn_agent` call before a child is started. Lives in the tool executor's
/// single-threaded event loop, so it needs no synchronization.
#[derive(Default)]
pub struct SpawnGuard {
    /// child → parent, from `SessionStarted`. Absent or `None` ⇒ a root.
    parents: HashMap<SessionId, Option<SessionId>>,
    /// root → cumulative sub-agents spawned beneath it (never decremented).
    spawns_per_root: HashMap<SessionId, usize>,
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

    /// Decide whether `parent` may spawn another sub-agent. On approval, charges
    /// the spawn against the root's budget and returns `Ok`. On refusal, returns
    /// the message to relay to the parent as the `spawn_agent` tool output.
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

/// The `spawn_agent` tool schema advertised to the model. Appended to the
/// engine's `tool_specs` alongside the host quartet.
pub fn spawn_agent_spec() -> ToolSpec {
    ToolSpec::with_schema(
        SPAWN_TOOL,
        "Launch a sub-agent session to handle a focused subtask. Returns \
         immediately with an agent_id handle (it does not wait for the \
         sub-agent to finish), so you can launch several in a row and let them \
         run concurrently. Collect a sub-agent's answer by calling agent_poll \
         with its agent_id.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "Agent profile for the sub-agent (build | plan | explore | custom). Defaults to explore (read-only)."
                },
                "prompt": {
                    "type": "string",
                    "description": "The task or question for the sub-agent to work on."
                }
            },
            "required": ["agent", "prompt"]
        }),
    )
}

/// Orchestrate one `spawn_agent` call (ADR-0026): start a child session, reply
/// to `parent` *immediately* with the child handle, then keep watching the child
/// and record its answer + duration into `registry` for a later `agent_poll`.
///
/// `events` must be a receiver subscribed *before* the [`InMsg::Spawn`] is sent
/// (the caller subscribes synchronously), so the child's events — including its
/// terminal `Done` — cannot race ahead of the watcher.
pub async fn launch_subagent(
    holly: Holly,
    mut events: Receiver<OutEvent>,
    registry: AgentRegistry,
    parent: SessionId,
    request_id: String,
    input: String,
) {
    let (agent, prompt) = parse_input(&input);
    let child = SessionId::new_uuid();
    // Register *before* sending Spawn so a poll can never precede the handle
    // (the parent only learns the id from the reply below, which comes after).
    let (status_tx, started) = registry.register(child.clone());

    if holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: parent.clone(),
            agent: agent.clone(),
            prompt,
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

    // Hand the handle back now — the parent turn continues instead of blocking
    // on the child's `Done` (ADR-0026 supersedes ADR-0022's synchronous relay).
    reply(
        &holly,
        parent,
        request_id,
        format!(
            "Sub-agent launched under the `{agent}` profile. agent_id: {child}. \
             Call agent_poll with this agent_id to await its answer."
        ),
    )
    .await;

    // Keep accumulating the child's answer; publish it (with timing) for poll.
    let answer = collect_child_answer(&mut events, &child).await;
    // The registry keeps a receiver, so the completed value survives this drop.
    let _ = status_tx.send(AgentStatus::Complete {
        answer,
        elapsed: started.elapsed(),
    });
}

/// Watch the child's event stream, accumulating its assistant text until the
/// child's turn finishes (`Done`). Returns the final answer, or an explanatory
/// note when the child errored or produced nothing.
async fn collect_child_answer(events: &mut Receiver<OutEvent>, child: &SessionId) -> String {
    let mut text = String::new();
    let mut error: Option<String> = None;
    loop {
        match events.recv().await {
            Ok(ev) if ev.session() != child => {}
            Ok(OutEvent::TextDelta { text: delta, .. }) => text.push_str(&delta),
            Ok(OutEvent::Error { message, .. }) => error = Some(message),
            Ok(OutEvent::Done { .. }) => break,
            Ok(_) => {}
            // A lagging watcher could miss the child's `Done` and park the parent
            // forever; surface what we have instead of blocking indefinitely.
            Err(RecvError::Lagged(_)) => break,
            Err(RecvError::Closed) => break,
        }
    }
    let text = text.trim();
    match (text.is_empty(), error) {
        (false, _) => text.to_string(),
        (true, Some(e)) => format!("sub-agent ended with error: {e}"),
        (true, None) => "sub-agent produced no output".to_string(),
    }
}

/// Parse the `spawn_agent` tool input. Providers send a JSON object
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

async fn reply(holly: &Holly, session: SessionId, request_id: String, output: String) {
    let _ = holly
        .send(InMsg::ToolResult {
            session,
            request_id,
            output,
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
