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

use std::time::Duration;

use entanglement_core::{
    Agent, AgentCatalog, AgentState, Holly, IdKind, InMsg, OutEvent, SessionId, ToolSpec,
};
use tokio::sync::broadcast::{error::RecvError, Receiver};

use crate::agent_registry::{AgentRegistry, AgentStatus};
use crate::host::MAX_OUTPUT_BYTES;
use crate::retained_output::RetainedOutputRegistry;
use crate::seam::reply;
use crate::tool_names::AGENT_TOOL;

mod spawn_guard;
pub use spawn_guard::{SpawnGuard, SpawnSlot};

/// Sub-agent profile used when the model omits `agent` (ADR-0207 stage 6a:
/// the roster collapsed to `general`/`plan`/`debug` — read-only posture is a
/// permission mode now, not a persona, so the default target is simply the
/// default worker persona, same as [`entanglement_core::holly::DEFAULT_AGENT`]).
const DEFAULT_SUBAGENT: &str = "general";

/// The `agent`/`agent_send` tool specs, advertised unconditionally to every
/// session (ADR-0207 §4/§6): spawning is never *graded* — `agent`/
/// `agent_send` are `Capability::Control` — and any registered agent may be a
/// session root or a spawn target, so the roster is a **constant**: every
/// profile in `registry`, not a per-spawner subset (the old `can_spawn`/
/// `spawnable_agents` gates are retired, ADR-0040 superseded). Spawning is
/// bounded instead by the session's mode `max_depth`/`max_agents`
/// ([`SpawnGuard::try_spawn`]). Because this no longer varies by profile, it
/// joins the shared `cfg.tool_specs` like `ask_user`/`poll` rather than a
/// per-profile table — ADR-0207 §9: "the advertised tools array no longer
/// varies by agent or by mode".
pub fn agent_specs(registry: &AgentCatalog) -> Vec<ToolSpec> {
    let targets: Vec<&Agent> = registry.iter().collect();
    if targets.is_empty() {
        return Vec::new();
    }
    vec![agent_spec(&targets), crate::agent_send::agent_send_spec()]
}

/// The `agent` tool schema advertised to the model (#606, ADR-0161 §1 —
/// replaces the `agent`/`agent_spawn` split of ADR-0033). Blocks by default and
/// returns the sub-agent's final answer directly; `background: true` returns a
/// handle immediately instead, joined later with `poll`. The `targets` roster
/// is disclosed inline (#112): each spawnable agent's `name: description` is
/// listed in the tool description and the `agent` argument is constrained to
/// that set.
pub fn agent_spec(targets: &[&Agent]) -> ToolSpec {
    ToolSpec::with_schema(
        AGENT_TOOL,
        format!(
            "Delegate a focused subtask to a sub-agent. Blocks until it \
             finishes and returns its final answer directly — the one-call \
             path for a single delegation. Pass background: true to return an \
             agent_id handle immediately instead (it does not wait for the \
             sub-agent to finish), so you can launch several in a row and let \
             them run concurrently; collect each answer later by calling poll \
             with its agent_id.\n\n{}",
            roster(targets)
        ),
        agent_input_schema(targets),
    )
}

/// The `name: description` roster line block disclosed to the spawning model —
/// `description` is the only field of a definition a parent ever sees (#112).
/// Scoped to the profiles this spawner may target (#119).
fn roster(targets: &[&Agent]) -> String {
    let mut out = String::from("Available agents:");
    for p in targets {
        out.push_str(&format!("\n- {}: {}", p.name, p.description));
    }
    out
}

/// The `agent` tool's `{ agent, prompt, background? }` input schema. The
/// `agent` name is constrained to `targets` (an enum) so the model can only
/// pick a profile it is actually allowed to spawn (#119); `background` (#606)
/// flips the return shape from the blocking default to an immediate handle.
fn agent_input_schema(targets: &[&Agent]) -> serde_json::Value {
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
            },
            "background": {
                "type": "boolean",
                "description": "Return an agent_id handle immediately instead of \
                    waiting for the sub-agent's answer. Poll the handle with \
                    `poll` to collect it once it's done. Default false (blocks \
                    until the sub-agent finishes)."
            },
            "model": {
                "type": "string",
                "description": "Catalog model id to run the sub-agent on \
                    (see explore kind: models for the active roster). Omit \
                    to inherit the parent's model. An unknown id refuses the \
                    spawn and names the valid ids instead of silently \
                    falling back."
            }
        },
        "required": ["agent", "prompt"]
    })
}

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
    let child = SessionId::new(holly.next_id(IdKind::Session));
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

/// Render a sub-agent's completion reply: the "completed in Xs" status line
/// (kept verbatim) followed by its `answer`, bounded to
/// [`crate::host::MAX_OUTPUT_BYTES`] with a head+tail split so a long answer's
/// conclusion survives truncation — the same shape `bash`/`call`/`rhai` now
/// share instead of returning an unbounded answer (#622). Shared by the
/// blocking `agent` path ([`launch`]) and `poll`.
///
/// Capping alone would trade a context blow-up for silently discarded work
/// (#614), so a truncated answer also mints a retained-output handle
/// (`owner`-scoped, the same "work you might still have questions about" rule
/// `call` follows) that `poll` pages the rest of.
pub fn format_agent_answer(
    agent_id: impl std::fmt::Display,
    elapsed: Duration,
    answer: String,
    retained: &RetainedOutputRegistry,
    owner: Option<&SessionId>,
) -> String {
    let status = format!(
        "sub-agent `{agent_id}` completed in {:.1}s:\n\n",
        elapsed.as_secs_f64()
    );
    bound_answer(status, answer, retained, owner)
}

/// Shared truncation-with-a-handle shape for a sub-agent's answer: `status` is
/// kept verbatim, `answer` gets [`crate::host::bounded_result`]'s head+tail
/// byte cap, and if the combined result would overflow, the *full* `answer`
/// is registered with `retained` (scoped to `owner`) so `poll` can page past
/// the cap instead of the excess vanishing (#614). Shared by
/// [`format_agent_answer`] and the sponsored-build reply in `propose_plan`,
/// whose status line names the plan file too.
pub(crate) fn bound_answer(
    mut status: String,
    answer: String,
    retained: &RetainedOutputRegistry,
    owner: Option<&SessionId>,
) -> String {
    if status.len() + answer.len() > MAX_OUTPUT_BYTES {
        let handle = retained.register_text(owner.cloned(), answer.clone());
        status.push_str(&format!(
            "[answer truncated — poll(handle=\"{handle}\") for the rest]\n"
        ));
    }
    crate::host::bounded_result(&status, answer)
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
    // Rebound when the child compacts (ADR-0205): its context overflowed, so
    // the engine forked it into a successor and retired it. The answer this
    // parent is parked on now comes from the successor, and the retired id's
    // `SessionEnded` must not be read as "the child finished".
    let mut child = child.clone();
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
            // Checked before the session filter below — the announcement is
            // the successor's, not the retired child's.
            Ok(OutEvent::SessionStarted {
                session: successor,
                predecessor: Some(source),
                ..
            }) if source == child => {
                tracing::debug!(%source, %successor, "sub-agent watch follows the compaction successor");
                child = successor;
            }
            Ok(ev) if ev.session() != Some(&child) => {}
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
    use entanglement_core::EngineConfig;
    use std::time::Duration;

    fn empty_engine() -> Holly {
        Holly::spawn(EngineConfig::default())
    }

    /// #622: a huge answer keeps a head + tail slice instead of growing the
    /// reply unbounded — the status line always survives intact.
    #[test]
    fn format_agent_answer_bounds_a_huge_answer() {
        use crate::host::MAX_OUTPUT_BYTES;
        let mut answer = String::from("HEAD_MARKER");
        answer.push_str(&"x".repeat(MAX_OUTPUT_BYTES * 2));
        answer.push_str("TAIL_MARKER");
        let retained = RetainedOutputRegistry::new();
        let out = format_agent_answer(
            SessionId::new("child-1"),
            Duration::from_secs(3),
            answer,
            &retained,
            None,
        );
        assert!(
            out.starts_with("sub-agent `child-1` completed in 3.0s:\n\n"),
            "status line dropped: {out}"
        );
        assert!(out.contains("HEAD_MARKER"), "head lost: {out}");
        assert!(out.ends_with("TAIL_MARKER"), "tail lost: {out}");
        assert!(out.contains("omitted from the middle"), "got: {out}");
    }

    #[test]
    fn format_agent_answer_passes_through_small_answer() {
        let retained = RetainedOutputRegistry::new();
        let out = format_agent_answer(
            SessionId::new("child-1"),
            Duration::from_secs(1),
            "hi".to_string(),
            &retained,
            None,
        );
        assert_eq!(out, "sub-agent `child-1` completed in 1.0s:\n\nhi");
    }

    /// #614: a truncated answer isn't just capped — it mints a retained-output
    /// handle so `poll` can page the rest, scoped to the given `owner`.
    #[test]
    fn format_agent_answer_truncation_mints_a_retained_handle() {
        use crate::host::MAX_OUTPUT_BYTES;
        let answer = "x".repeat(MAX_OUTPUT_BYTES * 2);
        let retained = RetainedOutputRegistry::new();
        let owner = SessionId::new("parent-1");
        let out = format_agent_answer(
            SessionId::new("child-1"),
            Duration::from_secs(2),
            answer.clone(),
            &retained,
            Some(&owner),
        );
        let handle = out
            .lines()
            .find_map(|l| l.split("poll(handle=\"").nth(1))
            .and_then(|s| s.split('"').next())
            .expect("handle in output");
        let page = retained
            .page(handle, &owner, 0, 0)
            .expect("retained entry exists for the owner");
        assert!(
            page.text.contains(&answer[answer.len() - 100..]),
            "full answer recoverable via the handle"
        );
        assert!(
            retained
                .page(handle, &SessionId::new("stranger"), 0, 0)
                .is_none(),
            "handle is scoped to the owner"
        );
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

    #[test]
    fn agent_specs_list_every_registered_agent() {
        // ADR-0207 §6/§9: spawning is unconditional and the roster is
        // constant — every registered agent is a valid target (stage 6a's
        // collapsed three-persona roster: `general`/`plan`/`debug`).
        let reg = crate::agents::built_in_registry().expect("built-in agents must parse");
        let specs = agent_specs(&reg);
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec![AGENT_TOOL, crate::tool_names::AGENT_SEND_TOOL]);
        let enum_names = specs[0].schema["properties"]["agent"]["enum"]
            .as_array()
            .unwrap();
        assert!(enum_names.iter().any(|n| n == "general"));
        assert!(enum_names.iter().any(|n| n == "debug"));
        assert!(enum_names.iter().any(|n| n == "plan"));
        // #560 P12, ADR-0207 §12: `model` rides the same spec every session
        // advertises unconditionally (`agent` is a `TOOL_SEARCH_KERNEL`
        // member) — verified here against the exact schema a session
        // receives, not a separate description.
        assert!(
            specs[0].schema["properties"]["model"].is_object(),
            "{:?}",
            specs[0].schema
        );
    }

    #[test]
    fn agent_specs_are_identical_regardless_of_which_profile_asks() {
        // The whole point of the constant roster (ADR-0207 §9): the array
        // must not depend on the caller's own profile, since `SetAgent` must
        // stay free of prompt-cache invalidation. `agent_specs` takes no
        // spawner argument at all now, so calling it twice against the same
        // registry is the only meaningful "regardless of who asks" check —
        // compare by (name, description, schema) since `ToolSpec` has no
        // `PartialEq`.
        let reg = crate::agents::built_in_registry().expect("built-in agents must parse");
        let a = agent_specs(&reg);
        let b = agent_specs(&reg);
        let project = |specs: &[ToolSpec]| -> Vec<(String, String, serde_json::Value)> {
            specs
                .iter()
                .map(|s| (s.name.clone(), s.description.clone(), s.schema.clone()))
                .collect()
        };
        assert_eq!(project(&a), project(&b));
    }
}
