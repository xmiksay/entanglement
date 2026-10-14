//! Sub-agent spawn orchestration (#60, ADR-0021/0010; non-blocking #89,
//! ADR-0026; unified `agent { background }` flag, #606, ADR-0161 — supersedes
//! the `agent`/`agent_spawn` tool split of #120/ADR-0033).
//!
//! `agent` is not a filesystem tool in the [`ToolRegistry`] — it is an
//! engine-coordination primitive owned by the runtime, blocking by default with
//! an opt-in `background: bool` that flips the return shape:
//!
//! - **`background: true`** — [`launch_subagent`] creates a child session via
//!   [`InMsg::Spawn`] and replies to the parent *immediately* with the child's
//!   handle (`agent_id`); it does **not** wait for the child's `Done`, so it
//!   never blocks the parent turn (ADR-0026 supersedes ADR-0022's synchronous
//!   answer-relay). It then keeps watching the child in the same detached task,
//!   recording the final answer + duration into the shared
//!   [`AgentRegistry`][crate::agent_registry::AgentRegistry] keyed by the
//!   handle; the parent collects it later with `poll` (#605, formerly
//!   `agent_poll`, see [`crate::poll`]).
//! - **default (blocking)** — [`run_agent`] runs the exact same launch path
//!   (guard, clamp, `Spawn`), but instead of handing back the handle it parks on
//!   the child's *genuine* completion and folds the child's answer + elapsed
//!   straight into the `ToolOutput` — the one-call path for a single delegation.
//!   It still records into the registry, so a parent `Stop` while parked leaves
//!   the child collectable via `poll`.
//!
//! Both routes share [`collect_child_answer`], which waits past an errored turn
//! with no usable answer instead of unblocking the parent on it (#562): the
//! child session stays alive and steerable, and the parent parks until the
//! child's next turn genuinely finishes, or the child ends. See the function's
//! own doc for the detail.
//!
//! Because it only orchestrates sessions (it touches no host resource), the
//! executor runs it *before* permission resolution — it bypasses the permission
//! profile exactly like the runtime's `propose_plan` / `update_tasks` state tools.

use entanglement_core::{AgentState, Holly, IdContext, IdKind, InMsg, OutEvent, SessionId};
use tokio::sync::broadcast::Receiver;

use crate::agent_registry::{AgentRegistry, AgentStatus};
use crate::retained_output::RetainedOutputRegistry;
use crate::seam::reply;

mod answer;
mod spawn_guard;
mod spec;
pub use answer::{collect_child_answer, format_agent_answer};
pub use spawn_guard::{SpawnGuard, SpawnSlot};
pub use spec::{agent_spec, agent_specs};

/// Sub-agent profile used when the model omits `agent` (ADR-0207 stage 6a:
/// the roster collapsed to `general`/`plan`/`debug` — read-only posture is a
/// permission mode now, not a persona, so the default target is simply the
/// default worker persona, same as [`entanglement_core::holly::DEFAULT_AGENT`]).
const DEFAULT_SUBAGENT: &str = "general";

/// Whether a launch hands the handle back immediately (`background: true`) or
/// parks for the child's answer and returns it directly (the default, #120,
/// #606).
#[derive(Clone, Copy, PartialEq, Eq)]
enum LaunchMode {
    /// Non-blocking: reply the handle at once, then record the answer for poll.
    Detached,
    /// Blocking: record the answer, then reply it (with timing) to the parent.
    AwaitAnswer,
}

/// Orchestrate one `background: true` `agent` call (ADR-0026): start a child
/// session, reply to `parent` *immediately* with the child handle, then keep
/// watching the child and record its answer + duration into `registry` for a
/// later `poll` (#605).
///
/// `events` must be a receiver subscribed *before* the [`InMsg::Spawn`] is sent
/// (the caller subscribes synchronously), so the child's events — including its
/// terminal `Done` — cannot race ahead of the watcher.
#[allow(clippy::too_many_arguments)]
pub async fn launch_subagent(
    holly: Holly,
    events: Receiver<OutEvent>,
    registry: AgentRegistry,
    retained: RetainedOutputRegistry,
    parent: SessionId,
    request_id: String,
    input: String,
    // The child's model pin (#560 P12, ADR-0207 §12): resolved + validated
    // against the catalog by the caller (`orchestration::spawn`) before this
    // task was even spawned, so a refusal never mints a child — see
    // `permission::resolve_model`. `None` inherits, exactly as before.
    model_pin: Option<(String, String)>,
) {
    launch(
        holly,
        events,
        registry,
        retained,
        parent,
        request_id,
        input,
        LaunchMode::Detached,
        model_pin,
    )
    .await;
}

/// Orchestrate one default (blocking) `agent` call (#120): run the exact
/// `background: true` launch path, then park on the child's genuine completion
/// ([`collect_child_answer`]) and fold its answer + elapsed straight into the
/// `ToolOutput`. Still records into `registry`, so a parent `Stop` while parked
/// leaves the child collectable via `poll`.
#[allow(clippy::too_many_arguments)]
pub async fn run_agent(
    holly: Holly,
    events: Receiver<OutEvent>,
    registry: AgentRegistry,
    retained: RetainedOutputRegistry,
    parent: SessionId,
    request_id: String,
    input: String,
    model_pin: Option<(String, String)>,
) {
    launch(
        holly,
        events,
        registry,
        retained,
        parent,
        request_id,
        input,
        LaunchMode::AwaitAnswer,
        model_pin,
    )
    .await;
}

/// Shared launch path for `background: true` (`Detached`) and the default
/// blocking call (`AwaitAnswer`). The two differ only in *when* and *what*
/// they reply: a detached launch hands the handle back before watching the
/// child; a blocking launch watches first, then replies the answer. Both
/// record the answer into `registry`.
#[allow(clippy::too_many_arguments)]
async fn launch(
    holly: Holly,
    mut events: Receiver<OutEvent>,
    registry: AgentRegistry,
    retained: RetainedOutputRegistry,
    parent: SessionId,
    request_id: String,
    input: String,
    mode: LaunchMode,
    model_pin: Option<(String, String)>,
) {
    let (agent, prompt, _background, _model) = parse_input(&input);
    // Minted in the parent's lineage so it keeps the tenant prefix (ADR-0211).
    let child = SessionId::new(holly.next_id_in(IdKind::Session, &IdContext::child_of(&parent)));
    // Register *before* sending Spawn so a poll can never precede the handle
    // (the parent only learns the id from the reply below, which comes after).
    let (status_tx, started) = registry.register(child.clone(), parent.clone(), agent.clone());

    if holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(parent.clone()),
            predecessor: None,
            agent: agent.clone(),
            prompt,
            user: None,
            model: None,
            mode: None,
            // Inherited from the parent by the supervisor (#764).
            workdir: None,
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
            true,
        )
        .await;
        return;
    }
    // The child's model pin (#560 P12, ADR-0207 §12): sent right after
    // `Spawn` so it lands before the child's first turn. Already validated
    // against the catalog by the caller — a `SetModel` failure here (e.g. a
    // key that vanished between validation and this send) surfaces as the
    // child's own `OutEvent::Error`, same as any live `SetModel`; it does
    // not unwind the spawn, since the child session now genuinely exists.
    if let Some((provider, model)) = model_pin {
        let _ = holly
            .send(InMsg::SetModel {
                session: child.clone(),
                provider,
                model,
            })
            .await;
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
                 Call poll with this agent_id to await its answer."
            ),
            false,
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
    // reply; the answer above stays collectable via `poll`.
    if mode == LaunchMode::AwaitAnswer {
        reply(
            &holly,
            parent.clone(),
            request_id,
            format_agent_answer(&child, elapsed, answer, &retained, Some(&parent)),
            false,
        )
        .await;
    }
}

/// Track a sub-agent a cold resume rebuilt, new to this process's registry,
/// so its parent's `poll`/`agent_send`/`ListOperations` reach it again
/// (ADR-0234). The handle is what the parent knew it by: the head of its
/// compaction lineage, which the resume re-taught the engine's directory.
pub(crate) fn track_resumed(
    holly: &Holly,
    registry: &AgentRegistry,
    session: &SessionId,
    parent: &SessionId,
    agent: &str,
) {
    let handle = holly.lineage_head(session);
    if let Some(tx) =
        registry.track_resumed(handle, session.clone(), parent.clone(), agent.to_string())
    {
        watch_resumed(holly, parent.clone(), session.clone(), tx);
    }
}

/// Record a resumed sub-agent's next answer for its parent's `poll`, as the
/// launch watcher does for a fresh one. Subscribed here, synchronously, so
/// nothing the child emits after its `SessionStarted` is missed.
fn watch_resumed(
    holly: &Holly,
    parent: SessionId,
    child: SessionId,
    tx: tokio::sync::watch::Sender<crate::agent_registry::AgentStatus>,
) {
    let mut events = holly.subscribe();
    let holly = holly.clone();
    let started = std::time::Instant::now();
    tokio::spawn(async move {
        let answer = collect_child_answer(&holly, &parent, &mut events, &child).await;
        let _ = tx.send(crate::agent_registry::AgentStatus::Complete {
            answer,
            elapsed: started.elapsed(),
        });
    });
}

/// The spawn *target* named in an `agent_spawn`/`agent` tool input — the runtime
/// executor reads it to apply the per-profile allowlist + target-mode gate before
/// a child is minted (#119). Mirrors [`parse_input`]'s agent resolution (a bare
/// string / omitted `agent` ⇒ the read-only default).
pub fn target_agent(input: &str) -> String {
    parse_input(input).0
}

/// Whether an `agent` call's input requests the non-blocking path (#606,
/// ADR-0161 §1) — read by the tool executor to pick [`launch_subagent`] over
/// [`run_agent`] before either runs.
pub fn is_background(input: &str) -> bool {
    parse_input(input).2
}

/// The requested `model` (#560 P12, ADR-0207 §12), if any — read by the tool
/// executor to validate/resolve it against the catalog before a child is
/// minted, mirroring [`target_agent`]. `None` means inherit, exactly as
/// before this parameter existed.
pub fn target_model(input: &str) -> Option<String> {
    parse_input(input).3
}

/// Parse the `agent` tool input. Providers send a JSON object `{"agent": …,
/// "prompt": …, "background": …, "model": …}`; scripted/raw backends may send
/// a bare string, which is treated as the prompt under the default sub-agent
/// profile with `background` defaulting to `false` and no `model` override.
fn parse_input(input: &str) -> (String, String, bool, Option<String>) {
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
            let background = v
                .get("background")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            let model = v
                .get("model")
                .and_then(|m| m.as_str())
                .filter(|m| !m.is_empty())
                .map(str::to_string);
            (agent, prompt, background, model)
        }
        Err(_) => (DEFAULT_SUBAGENT.to_string(), input.to_string(), false, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_input_reads_json_object() {
        let (agent, prompt, background, model) =
            parse_input(r#"{"agent":"build","prompt":"do it"}"#);
        assert_eq!(agent, "build");
        assert_eq!(prompt, "do it");
        assert!(!background);
        assert_eq!(model, None);
    }

    #[test]
    fn parse_input_reads_background_flag() {
        let (_, _, background, _) =
            parse_input(r#"{"agent":"build","prompt":"do it","background":true}"#);
        assert!(background);
    }

    #[test]
    fn parse_input_reads_model_override() {
        let (_, _, _, model) =
            parse_input(r#"{"agent":"build","prompt":"do it","model":"glm-5.3"}"#);
        assert_eq!(model, Some("glm-5.3".to_string()));
        assert_eq!(target_model(r#"{"prompt":"x"}"#), None);
    }

    #[test]
    fn parse_input_defaults_agent_to_general() {
        let (agent, prompt, _, _) = parse_input(r#"{"prompt":"look around"}"#);
        assert_eq!(agent, DEFAULT_SUBAGENT);
        assert_eq!(prompt, "look around");
    }

    #[test]
    fn parse_input_falls_back_to_raw_string() {
        let (agent, prompt, background, model) = parse_input("just a prompt");
        assert_eq!(agent, DEFAULT_SUBAGENT);
        assert_eq!(prompt, "just a prompt");
        assert!(!background);
        assert_eq!(model, None);
    }
}
