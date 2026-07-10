//! `agent_poll` — the synchronous join half of non-blocking spawn (#89, ADR-0026).
//!
//! [`crate::subagent::launch_subagent`] returns a child handle (`agent_id`) to
//! the parent *immediately* and keeps watching the child in a detached task,
//! recording its answer + duration into the shared [`AgentRegistry`] keyed by
//! that handle. `agent_poll { agent_id, timeout_secs }` is how the parent later
//! collects the result: it blocks up to `timeout_secs` for that specific child
//! and returns the final answer (with elapsed time) once it completes, or a
//! still-running status on timeout so the model can poll again or do other work.
//!
//! Like `agent_spawn`/`ask_user`, `agent_poll` is a runtime-owned tool the
//! executor intercepts *before* permission resolution: it starts no session and
//! touches no host resource — it only reads accumulated spawn state — so it needs
//! no permission gating or spawn-budget charge (those apply per launch, ADR-0023/
//! 0024).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use entanglement_core::{Holly, InMsg, SessionId, ToolSpec};
use tokio::sync::watch;

/// Tool name the model calls to await a launched sub-agent's answer.
pub const AGENT_POLL_TOOL: &str = "agent_poll";

/// Default poll timeout when the model omits `timeout_secs`.
const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Upper bound on a single poll's wait so one call can't park the parent turn
/// indefinitely — the model is expected to poll again rather than block forever.
const MAX_TIMEOUT_SECS: u64 = 600;

/// Live status of a launched sub-agent, surfaced through [`AgentRegistry`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    /// The child is still running; no answer yet.
    Running,
    /// The child finished — carries its final answer and how long it ran
    /// (from the `Spawn` send to the child's `Done`).
    Complete { answer: String, elapsed: Duration },
}

/// One tracked sub-agent: when it launched and a watch handle to observe its
/// completion. The launch watcher owns the [`watch::Sender`]; every entry keeps
/// a receiver so the last value survives the sender being dropped, letting a
/// late poll still read a completed answer.
#[derive(Clone)]
struct Entry {
    started: Instant,
    status: watch::Receiver<AgentStatus>,
}

/// Shared table of launched sub-agents keyed by child `SessionId` (the handle
/// `agent_spawn` returns). Cloned into every launch/poll task — the `Arc<Mutex>`
/// is only ever held briefly to insert or clone a receiver, never across an
/// `.await`, so pollers block on the watch channel, not the lock.
#[derive(Clone, Default)]
pub struct AgentRegistry {
    inner: Arc<Mutex<HashMap<SessionId, Entry>>>,
}

impl AgentRegistry {
    /// Register a freshly-launched child as `Running`. Returns the sender the
    /// launch watcher flips to `Complete`, plus the launch instant so it can
    /// report the same elapsed a poller would compute.
    pub fn register(&self, child: SessionId) -> (watch::Sender<AgentStatus>, Instant) {
        let (tx, rx) = watch::channel(AgentStatus::Running);
        let started = Instant::now();
        self.lock().insert(
            child,
            Entry {
                started,
                status: rx,
            },
        );
        (tx, started)
    }

    /// Drop a child that never actually launched (the `Spawn` send failed), so a
    /// stray handle can't linger as perpetually `Running`.
    pub fn forget(&self, child: &SessionId) {
        self.lock().remove(child);
    }

    /// A poller's view of `child`: its launch instant and a fresh receiver.
    /// `None` when no such handle was ever launched here.
    fn view(&self, child: &SessionId) -> Option<(Instant, watch::Receiver<AgentStatus>)> {
        self.lock()
            .get(child)
            .map(|e| (e.started, e.status.clone()))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, Entry>> {
        // Poisoning only happens if a holder panicked while mutating the map;
        // we never panic under the lock, so this is provably unreachable.
        self.inner.lock().expect("agent registry mutex poisoned")
    }
}

/// The `agent_poll` tool schema advertised to the model. Appended to the
/// engine's `tool_specs` alongside `agent_spawn`.
pub fn agent_poll_spec() -> ToolSpec {
    ToolSpec::with_schema(
        AGENT_POLL_TOOL,
        "Await a sub-agent previously launched with agent_spawn. Pass the \
         agent_id it returned; blocks up to timeout_secs for that child and \
         returns its final answer once complete (with how long it ran), or a \
         still-running status on timeout so you can poll again or do other work \
         meanwhile. Launch several sub-agents first, then poll each handle to run \
         them concurrently.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "The handle returned by agent_spawn for the sub-agent to await."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Max seconds to wait this poll before returning a still-running status. Defaults to 60."
                }
            },
            "required": ["agent_id"]
        }),
    )
}

/// Orchestrate one `agent_poll` call: look up the handle, wait up to the
/// timeout for the child to complete, and reply to the parent with the answer or
/// a still-running note.
pub async fn run_agent_poll(
    holly: Holly,
    registry: AgentRegistry,
    session: SessionId,
    request_id: String,
    input: String,
) {
    let (agent_id, timeout_secs) = parse_input(&input);
    let Some(agent_id) = agent_id else {
        reply(
            &holly,
            session,
            request_id,
            "agent_poll: missing agent_id — pass the handle returned by agent_spawn.".to_string(),
        )
        .await;
        return;
    };

    let child = SessionId::new(agent_id.clone());
    let Some((started, mut rx)) = registry.view(&child) else {
        reply(
            &holly,
            session,
            request_id,
            format!(
                "agent_poll: no sub-agent found for agent_id `{agent_id}` — it was never launched \
                 from this session (use the id returned by agent_spawn)."
            ),
        )
        .await;
        return;
    };

    let output =
        match tokio::time::timeout(Duration::from_secs(timeout_secs), wait_complete(&mut rx)).await
        {
            Ok(AgentStatus::Complete { answer, elapsed }) => {
                format!(
                    "sub-agent `{agent_id}` completed in {:.1}s:\n\n{answer}",
                    elapsed.as_secs_f64()
                )
            }
            // `wait_complete` only returns on completion; a running value never escapes.
            Ok(AgentStatus::Running) => unreachable!("wait_complete returns only on completion"),
            Err(_) => format!(
            "sub-agent `{agent_id}` still running ({:.1}s elapsed); polled with a {timeout_secs}s \
             timeout. Poll again later or do other work meanwhile.",
            started.elapsed().as_secs_f64()
        ),
        };
    reply(&holly, session, request_id, output).await;
}

/// Resolve once the watched child reaches [`AgentStatus::Complete`]. Checks the
/// current value first (so an already-finished child returns instantly), then
/// awaits changes. If the sender drops without ever completing, treats it as a
/// completion with an explanatory answer so the poll never hangs.
async fn wait_complete(rx: &mut watch::Receiver<AgentStatus>) -> AgentStatus {
    loop {
        {
            let cur = rx.borrow_and_update();
            if let AgentStatus::Complete { .. } = &*cur {
                return cur.clone();
            }
        }
        if rx.changed().await.is_err() {
            return AgentStatus::Complete {
                answer: "sub-agent ended without producing an answer".to_string(),
                elapsed: Duration::ZERO,
            };
        }
    }
}

/// Parse the `agent_poll` tool input. Providers send a JSON object; a bare or
/// malformed input yields no `agent_id` (the caller replies with guidance).
fn parse_input(input: &str) -> (Option<String>, u64) {
    match serde_json::from_str::<serde_json::Value>(input) {
        Ok(v) => {
            let agent_id = v
                .get("agent_id")
                .and_then(|a| a.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let timeout_secs = v
                .get("timeout_secs")
                .and_then(|t| t.as_u64())
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .min(MAX_TIMEOUT_SECS);
            (agent_id, timeout_secs)
        }
        Err(_) => (None, DEFAULT_TIMEOUT_SECS),
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
    fn parse_input_reads_id_and_timeout() {
        let (id, timeout) = parse_input(r#"{"agent_id":"abc","timeout_secs":5}"#);
        assert_eq!(id.as_deref(), Some("abc"));
        assert_eq!(timeout, 5);
    }

    #[test]
    fn parse_input_defaults_timeout_and_clamps_max() {
        let (_, default) = parse_input(r#"{"agent_id":"abc"}"#);
        assert_eq!(default, DEFAULT_TIMEOUT_SECS);
        let (_, clamped) = parse_input(r#"{"agent_id":"abc","timeout_secs":100000}"#);
        assert_eq!(clamped, MAX_TIMEOUT_SECS);
    }

    #[test]
    fn parse_input_missing_id_yields_none() {
        let (id, _) = parse_input(r#"{"timeout_secs":5}"#);
        assert!(id.is_none());
        let (bare, _) = parse_input("not json");
        assert!(bare.is_none());
    }

    #[tokio::test]
    async fn view_none_for_unknown_handle() {
        let reg = AgentRegistry::default();
        assert!(reg.view(&SessionId::new("nope")).is_none());
    }

    #[tokio::test]
    async fn complete_is_readable_after_sender_dropped() {
        // A poll that arrives *after* the child finished (and the launch task
        // dropped its sender) must still read the completed answer.
        let reg = AgentRegistry::default();
        let child = SessionId::new("c1");
        let (tx, _started) = reg.register(child.clone());
        tx.send(AgentStatus::Complete {
            answer: "done".to_string(),
            elapsed: Duration::from_millis(3),
        })
        .unwrap();
        drop(tx);

        let (_started, mut rx) = reg.view(&child).expect("entry present");
        let status = wait_complete(&mut rx).await;
        assert_eq!(
            status,
            AgentStatus::Complete {
                answer: "done".to_string(),
                elapsed: Duration::from_millis(3),
            }
        );
    }

    #[tokio::test]
    async fn wait_complete_blocks_until_completion() {
        let reg = AgentRegistry::default();
        let child = SessionId::new("c2");
        let (tx, _started) = reg.register(child.clone());
        let (_started, mut rx) = reg.view(&child).expect("entry present");

        // Still running: a short timeout elapses without a completion.
        let early = tokio::time::timeout(Duration::from_millis(20), wait_complete(&mut rx)).await;
        assert!(early.is_err(), "poll must not complete while child runs");

        tx.send(AgentStatus::Complete {
            answer: "late".to_string(),
            elapsed: Duration::from_millis(1),
        })
        .unwrap();
        let status = tokio::time::timeout(Duration::from_secs(1), wait_complete(&mut rx))
            .await
            .expect("completion observed after send");
        assert!(matches!(status, AgentStatus::Complete { answer, .. } if answer == "late"));
    }

    #[tokio::test]
    async fn forget_removes_a_failed_launch() {
        let reg = AgentRegistry::default();
        let child = SessionId::new("c3");
        reg.register(child.clone());
        reg.forget(&child);
        assert!(reg.view(&child).is_none());
    }
}
