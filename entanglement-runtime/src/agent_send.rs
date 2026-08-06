//! `agent_send` — send a follow-up prompt to a sub-agent already launched
//! with `agent`, instead of talking to it exactly once (#609, ADR-0162).
//!
//! The child→parent channel already existed: [`crate::subagent::collect_child_answer`]
//! ends its wait on **any** `Done` carrying text, so a child that concludes
//! "I'm blocked, advise" already unparks its parent today. The only missing
//! piece was the reply — `InMsg::Prompt` is session-addressed with no parent/
//! child restriction, and a child session task stays alive after its turn
//! ends, so prompting it starts a fresh turn on its existing context. This
//! module is structurally "a launcher against an existing session": it shares
//! the `background` flag ([ADR-0161](../../../docs/adr/0161-unified-async-work-background-flag-and-one-poll.md))
//! and the [`collect_child_answer`][crate::subagent::collect_child_answer]
//! wait with `agent`, and reuses [`AgentRegistry`] for both the descendant
//! check (a handle is only ever sendable by the session that launched it) and
//! the completion bookkeeping `poll` reads.
//!
//! **Refusals are load-bearing** (ADR-0162 §4): a closed (tombstoned) or
//! hibernated child must never be silently prompted. A hibernated child's
//! session task is gone from memory — the engine-wide supervisor's
//! lazy-`Prompt` path would otherwise respawn a *blank* session wearing the
//! right id, discarding its context. [`AgentRegistry::begin_send`] resolves
//! ownership and lifecycle in one lock acquisition and this module never
//! calls `Holly::send` unless it returns `Ok`.

use entanglement_core::{AgentState, ContentPart, Holly, InMsg, OutEvent, SessionId, ToolSpec};
use tokio::sync::broadcast::Receiver;

use crate::agent_registry::{AgentRegistry, SendOutcome};
use crate::retained_output::RetainedOutputRegistry;
use crate::seam::reply;
use crate::subagent::{collect_child_answer, format_agent_answer};
use crate::tool_names::AGENT_SEND_TOOL;

/// The `agent_send` tool schema advertised to the model, alongside `agent`
/// (#609, ADR-0162) — appended by [`crate::subagent::spawn_specs_for`], so
/// only a profile that may spawn ever sees it: `agent_send` is only useful
/// against a handle `agent` (or a `propose_plan` sponsored build, ADR-0162
/// §5) already produced.
pub fn agent_send_spec() -> ToolSpec {
    ToolSpec::with_schema(
        AGENT_SEND_TOOL,
        "Send a follow-up prompt to a sub-agent you already launched with \
         agent (or the build child a propose_plan approval named). Use this \
         to steer a child that's still working, follow up with one that \
         already finished, or send another round of feedback to a build \
         child instead of spawning a fresh one. Blocks until the child's next \
         answer by default, exactly like agent; pass background: true to \
         return immediately and collect the answer later with poll. Refused \
         for an agent_id you didn't launch, or one whose session has closed \
         or gone hibernated — those can't be safely reached this way.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "The handle of a sub-agent you launched — from \
                        agent's reply/handle, or the agent_id a propose_plan \
                        approval named."
                },
                "prompt": {
                    "type": "string",
                    "description": "The message to send the sub-agent."
                },
                "background": {
                    "type": "boolean",
                    "description": "Return immediately instead of waiting for the \
                        sub-agent's next answer. Poll the same agent_id with \
                        `poll` to collect it later. Default false (blocks until \
                        the sub-agent's next answer)."
                }
            },
            "required": ["agent_id", "prompt"]
        }),
    )
}

struct SendInput {
    agent_id: String,
    prompt: String,
    background: bool,
}

/// Parse the `agent_send` tool input. `None` when `agent_id`/`prompt` is
/// missing or empty, or the input isn't a JSON object — the caller replies
/// with guidance rather than silently picking a default (unlike `agent`,
/// there is no sane default target here).
fn parse_input(input: &str) -> Option<SendInput> {
    let v: serde_json::Value = serde_json::from_str(input).ok()?;
    let agent_id = v
        .get("agent_id")
        .and_then(|a| a.as_str())
        .filter(|s| !s.is_empty())?
        .to_string();
    let prompt = v
        .get("prompt")
        .and_then(|p| p.as_str())
        .filter(|s| !s.is_empty())?
        .to_string();
    let background = v
        .get("background")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    Some(SendInput {
        agent_id,
        prompt,
        background,
    })
}

/// Orchestrate one `agent_send` call (#609, ADR-0162): resolve `agent_id`
/// against `registry` (ownership + lifecycle, [`AgentRegistry::begin_send`]),
/// refuse clearly on anything but a live child owned by `parent`, then send
/// `InMsg::Prompt` and either reply immediately (`background: true`) or park
/// on [`collect_child_answer`] for the child's next answer — the same guard
/// path and wait `agent`'s blocking route takes, starting from `Prompt`
/// instead of `Spawn`.
///
/// `events` must be a receiver subscribed *before* this call runs (the caller
/// subscribes synchronously in the tool executor's single-threaded loop), so
/// the child's events can't race ahead of the watcher — mirrors
/// [`crate::subagent::launch_subagent`]/[`crate::subagent::run_agent`].
pub async fn run_agent_send(
    holly: Holly,
    mut events: Receiver<OutEvent>,
    registry: AgentRegistry,
    retained: RetainedOutputRegistry,
    parent: SessionId,
    request_id: String,
    input: String,
) {
    let Some(parsed) = parse_input(&input) else {
        reply(
            &holly,
            parent,
            request_id,
            "agent_send: requires a non-empty `agent_id` and `prompt`".to_string(),
            true,
        )
        .await;
        return;
    };
    let child = SessionId::new(parsed.agent_id.clone());

    let (status_tx, started) = match begin(&registry, &parent, &child, &parsed.agent_id) {
        Ok(v) => v,
        Err(refusal) => {
            reply(&holly, parent, request_id, refusal, true).await;
            return;
        }
    };

    let content = vec![ContentPart::text(parsed.prompt)];
    if holly
        .send(InMsg::Prompt {
            session: child.clone(),
            content,
        })
        .await
        .is_err()
    {
        reply(
            &holly,
            parent,
            request_id,
            "agent_send: engine inbox closed".to_string(),
            true,
        )
        .await;
        return;
    }

    if parsed.background {
        reply(
            &holly,
            parent.clone(),
            request_id.clone(),
            format!(
                "Prompt sent to sub-agent `{child}`. It does not wait for an \
                 answer — call poll with this agent_id to await its reply."
            ),
            false,
        )
        .await;
    } else {
        holly.emit_status(&parent, AgentState::WaitingAgent);
    }

    let answer = collect_child_answer(&holly, &parent, &mut events, &child).await;
    let elapsed = started.elapsed();
    let _ = status_tx.send(crate::agent_registry::AgentStatus::Complete {
        answer: answer.clone(),
        elapsed,
    });

    if !parsed.background {
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

/// Resolve [`AgentRegistry::begin_send`] into either the fresh watch sender
/// to drive, or the exact refusal text to reply with — pulled out of
/// [`run_agent_send`] so the message mapping is testable without a `Holly` or
/// a live turn to park a `ToolResult` against (the registry's own ownership +
/// lifecycle contract is unit-tested directly in `agent_registry.rs`; this is
/// only the message mapping on top of it).
fn begin(
    registry: &AgentRegistry,
    parent: &SessionId,
    child: &SessionId,
    agent_id: &str,
) -> Result<
    (
        tokio::sync::watch::Sender<crate::agent_registry::AgentStatus>,
        std::time::Instant,
    ),
    String,
> {
    match registry.begin_send(parent, child) {
        SendOutcome::Ok(tx, started) => Ok((tx, started)),
        SendOutcome::Unknown => Err(unknown_message(agent_id)),
        SendOutcome::Closed => Err(closed_message(agent_id)),
        SendOutcome::Hibernated => Err(hibernated_message(agent_id)),
    }
}

/// Unknown-handle refusal (mirrors `poll`'s convention): the same message for
/// "never launched" and "belongs to someone else", so a stranger's guess
/// can't confirm existence.
fn unknown_message(agent_id: &str) -> String {
    format!(
        "agent_send: unknown agent_id `{agent_id}` — it was never launched by \
         this session (use the agent_id from agent's reply, or a propose_plan \
         approval's build agent_id)."
    )
}

/// ADR-0162 §4: a closed (tombstoned) child refuses clearly — the id is spent
/// and can never run again.
fn closed_message(agent_id: &str) -> String {
    format!(
        "agent_send: sub-agent `{agent_id}` has closed — its session ended and \
         cannot be sent another prompt."
    )
}

/// ADR-0162 §4: a hibernated child refuses loudly instead of silently
/// respawning blank. Its context isn't lost — a head can resume it — but this
/// tool can't do that from inside a running turn.
fn hibernated_message(agent_id: &str) -> String {
    format!(
        "agent_send: sub-agent `{agent_id}` has hibernated (evicted from \
         memory to free resources) — sending it a prompt now would silently \
         start it over with no memory of its prior work, so this is refused. \
         Its context is not lost: a resume brings it back, but that isn't \
         something this tool can trigger. Treat it as unreachable for now — \
         if the work still needs doing, spawn a fresh sub-agent instead."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_input_reads_agent_id_prompt_and_background() {
        let p =
            parse_input(r#"{"agent_id":"s-abc","prompt":"keep going","background":true}"#).unwrap();
        assert_eq!(p.agent_id, "s-abc");
        assert_eq!(p.prompt, "keep going");
        assert!(p.background);
    }

    #[test]
    fn parse_input_defaults_background_to_false() {
        let p = parse_input(r#"{"agent_id":"s-abc","prompt":"hi"}"#).unwrap();
        assert!(!p.background);
    }

    #[test]
    fn parse_input_rejects_missing_or_empty_fields() {
        assert!(parse_input(r#"{"prompt":"hi"}"#).is_none());
        assert!(parse_input(r#"{"agent_id":"s-abc"}"#).is_none());
        assert!(parse_input(r#"{"agent_id":"","prompt":"hi"}"#).is_none());
        assert!(parse_input(r#"{"agent_id":"s-abc","prompt":""}"#).is_none());
        assert!(parse_input("not json").is_none());
    }

    /// #609: a call for an `agent_id` this session never launched refuses
    /// with the same message an outright-nonexistent handle gets — a
    /// stranger's guess (or a typo) can't confirm existence, and neither
    /// case ever reaches `Holly::send`.
    #[test]
    fn begin_refuses_an_unknown_handle() {
        let registry = AgentRegistry::default();
        let parent = SessionId::new("parent");
        let child = SessionId::new("s-nope");

        let err = begin(&registry, &parent, &child, "s-nope").unwrap_err();
        assert!(err.contains("unknown agent_id"), "{err}");
    }

    #[test]
    fn begin_refuses_a_stranger() {
        let registry = AgentRegistry::default();
        let owner = SessionId::new("owner");
        let stranger = SessionId::new("stranger");
        let child = SessionId::new("child");
        registry.register(child.clone(), owner, "build".to_string());

        let err = begin(&registry, &stranger, &child, "child").unwrap_err();
        assert!(err.contains("unknown agent_id"), "{err}");
    }

    #[test]
    fn begin_refuses_a_closed_child() {
        let registry = AgentRegistry::default();
        let parent = SessionId::new("parent2");
        let child = SessionId::new("child2");
        registry.register(child.clone(), parent.clone(), "build".to_string());
        registry.mark_closed(&child);

        let err = begin(&registry, &parent, &child, "child2").unwrap_err();
        assert!(err.contains("closed"), "{err}");
    }

    #[test]
    fn begin_refuses_a_hibernated_child() {
        let registry = AgentRegistry::default();
        let parent = SessionId::new("parent3");
        let child = SessionId::new("child3");
        registry.register(child.clone(), parent.clone(), "build".to_string());
        registry.mark_hibernated(&child);

        let err = begin(&registry, &parent, &child, "child3").unwrap_err();
        assert!(err.contains("hibernated"), "{err}");
    }

    #[test]
    fn begin_accepts_a_live_owned_child() {
        let registry = AgentRegistry::default();
        let parent = SessionId::new("parent4");
        let child = SessionId::new("child4");
        registry.register(child.clone(), parent.clone(), "build".to_string());

        assert!(begin(&registry, &parent, &child, "child4").is_ok());
    }
}
