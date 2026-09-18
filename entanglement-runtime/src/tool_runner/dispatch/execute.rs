//! The actual run-against-the-registry step (issue #451, split out of
//! `tool_runner.rs`): [`run_and_reply`] is reached both by `dispatch`'s
//! straight-through `Allow` and by [`super::decision::await_decision`]'s
//! `Approve` — one place that executes a host tool call and replies, so an
//! approved call and an outright-allowed one run identically.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, RwLock};

use entanglement_core::{Holly, OutEvent, SessionId, ToolCall, ToolEnvelope};

use crate::arg_validate;
use crate::hooks::Hooks;
use crate::seam;
use crate::skills::load_skill::parse_skill_id;
use crate::skills::SkillRegistry;
use crate::tool_advertising;
use crate::tool_names::LOAD_SKILL_TOOL;
use crate::tools::{ToolExecution, ToolRegistry};

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_and_reply(
    holly: &Holly,
    tools: &ToolRegistry,
    skills: &Arc<RwLock<Arc<SkillRegistry>>>,
    active_skill: &Arc<Mutex<HashSet<SessionId>>>,
    hooks: &Hooks,
    advertising: &tool_advertising::AdvertisingState,
    validation: &arg_validate::LoopBreaker,
    // The call exactly as the model emitted it, when core unwrapped an
    // `invoke` envelope (ADR-0204) — `None` for an ordinary native call.
    // Needed only for duplicate-key re-scanning below; every other use of
    // `tool`/`input` in this function already means the *inner* call either
    // way.
    envelope: Option<ToolEnvelope>,
    session: SessionId,
    request_id: String,
    tool: String,
    input: String,
) {
    // `update_tasks` carries no host resource (#231, ADR-0049): it is not in
    // the registry. The runtime emits its `TaskList` snapshot — minting a
    // **fresh** per-session seq (#157) so it takes an ordered place in the
    // content stream instead of colliding with the parked `ToolExec` seq —
    // and acks (text), instead of dispatching.
    if crate::plan_tasks::is_state_tool(&tool) {
        holly.emit_for_session(&session, |seq| {
            crate::plan_tasks::state_event(&session, seq, &tool, &input)
                .expect("is_state_tool ⇒ state_event is Some")
        });
        let ack = crate::plan_tasks::ack(&tool);
        hooks
            .run_post_tool_use(&session, &tool, &input, &ack, false, None)
            .await;
        seam::reply(holly, session, request_id, ack, false).await;
        return;
    }
    // Pre-dispatch argument validation (#560, ADR-0196 §6): a call whose
    // input violates the tool's advertised schema (missing/unexpected
    // properties, a type mismatch) never reaches `Tool::run` at all — it gets
    // a specific complaint instead of `run()`'s opaque parse-failure text.
    // A tool absent from the registry (a runtime-owned pseudo-tool like
    // `update_tasks`/`ask_user`/`poll`, already handled above or dispatched
    // elsewhere) has no advertised schema here, so it's exempt by
    // construction — nothing to validate against.
    if let Some(spec) = tools.spec_for(&tool) {
        if let Some(mut violation) = arg_validate::validate(&spec.schema, &input) {
            // Core's `invoke` unwrap has to parse the outer envelope to pull
            // out `args`, which collapses a duplicate key inside it exactly
            // like `validate`'s own parse of `input` just did above — so a
            // duplicate that lived under `args` is already gone from `input`
            // by the time it reaches here. Re-scan the envelope's raw text
            // (the call exactly as the model emitted it, still carrying the
            // duplicate) instead, whenever this call arrived that way.
            if let Some(env) = &envelope {
                violation.duplicate_keys = arg_validate::find_duplicate_keys(&env.input);
            }
            tracing::warn!(
                tool = %tool,
                violation = ?violation.lines(),
                "tool call failed pre-dispatch schema validation"
            );
            let already_delivered = advertising
                .discovered
                .lock()
                .expect("discovered-tool mutex poisoned")
                .contains(&session, &tool);
            if !already_delivered {
                advertising
                    .discovered
                    .lock()
                    .expect("discovered-tool mutex poisoned")
                    .mark(&session, &tool);
            }
            let via_invoke = tool_advertising::example_via_invoke(advertising, &session, &tool);
            // The delivered-schema dedup guard (ADR-0196 §6) only ever applies
            // to a *native* call: that tool's schema sits in the `tools` array
            // every round, so repeating it in a decline is genuinely
            // redundant. A tool reached through `invoke` is never in `tools`
            // — the model sees its schema nowhere else but inside a tool
            // result — so "already provided above" would point it at
            // something it cannot see. Never suppress here, no matter how
            // many times this tool has already violated its schema this
            // session.
            let suppress_schema = already_delivered && !via_invoke;
            let mut output =
                arg_validate::decline_text_for(&spec, &violation, suppress_schema, via_invoke);
            if validation.note(&session, &tool, &input, true) {
                output.push_str("\n\n");
                output.push_str(arg_validate::LOOP_BREAKER_NOTE);
            }
            hooks
                .run_post_tool_use(&session, &tool, &input, &output, true, None)
                .await;
            seam::reply(holly, session, request_id, output, true).await;
            return;
        }
    }
    // Every other tool executes against the host registry, returning multimodal
    // content (a text result, or an image block for `read` on an image, #221)
    // plus `is_error` (#636, ADR-0176). `edit`/`write` record their change into
    // the capture scope (#202); the executor mints a fresh `FileChange` seq
    // (#157) and broadcasts the audit event before replying with the
    // `ToolResult`. `duration_ms` is measured around the whole execution —
    // generic across every host tool, unlike `is_error` which the registry
    // itself classifies — so a slow `bash`/`call` is visible without either
    // parsing its `[exit N]` header or threading a bespoke timer through every
    // `Tool` impl.
    let started = std::time::Instant::now();
    let execution = crate::file_change::capture_and_emit(
        holly,
        &session,
        tools.execute(
            &ToolCall {
                id: request_id.clone(),
                name: tool.clone(),
                input: input.clone(),
                provider_meta: None,
            },
            &session,
        ),
    )
    .await;
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let ToolExecution {
        mut content,
        is_error,
        exit_code,
    } = execution;
    let mut output_text = entanglement_core::content_text(&content);
    // Loop-breaker guard (#560, ADR-0196 §6): two identical failing calls in a
    // row — same tool, same input, both `is_error` — mean the schema was
    // never the problem. Deliberately generic across every failure kind
    // (unknown-tool and schema-violation return earlier, above/in `dispatch`;
    // this covers a runtime tool error and an MCP required-param rejection
    // alike). A non-error result never triggers the note, matching decision
    // 3: a command failure (non-zero exit) is untouched.
    if validation.note(&session, &tool, &input, is_error) && is_error {
        output_text.push_str("\n\n");
        output_text.push_str(arg_validate::LOOP_BREAKER_NOTE);
        content.push(entanglement_core::ContentPart::text(format!(
            "\n\n{}",
            arg_validate::LOOP_BREAKER_NOTE
        )));
    }
    // #400, ADR-0106 (posture-only since ADR-0194): a successful `load_skill`
    // records the session's skill-active posture and tells any listening head
    // via `OutEvent::SkillActive` — parsed from the result's `skill_id:`
    // header (absent on a failed load: unknown/`user_only` skill, which
    // leaves any prior posture untouched). It no longer narrows the
    // session's tool set.
    if tool == LOAD_SKILL_TOOL {
        activate_skill(holly, skills, active_skill, &session, &output_text);
    }
    // `post_tool_use` (#199) observes the result before it is folded back — a
    // pure side-effect (formatter/telemetry); it cannot rewrite `content`, but
    // it now also observes `is_error` (#636) so a hook can branch on outcome
    // without re-parsing `output`.
    hooks
        .run_post_tool_use(&session, &tool, &input, &output_text, is_error, exit_code)
        .await;
    seam::reply_content(
        holly,
        session,
        request_id,
        content,
        is_error,
        Some(duration_ms),
        exit_code,
    )
    .await;
}

/// Record `session`'s skill-active posture (#400, ADR-0106; posture-only
/// since ADR-0194 — skills no longer mask tools) from a `load_skill` result:
/// parse its `skill_id:` header, look the skill up in the live registry for
/// its (now-vestigial, wire-compat-only) `allowed_tools`, and tell any
/// listening head via [`OutEvent::SkillActive`]. A `result` with no
/// `skill_id:` header (a failed load) is a no-op — the session keeps
/// whatever posture was active before.
fn activate_skill(
    holly: &Holly,
    skills: &Arc<RwLock<Arc<SkillRegistry>>>,
    active_skill: &Arc<Mutex<HashSet<SessionId>>>,
    session: &SessionId,
    result: &str,
) {
    let Some(skill_id) = parse_skill_id(result) else {
        return;
    };
    let allowed_tools = skills
        .read()
        .expect("skill registry lock poisoned")
        .get(skill_id)
        .and_then(|s| s.allowed_tools.clone());
    active_skill
        .lock()
        .expect("active-skill mutex poisoned")
        .insert(session.clone());
    holly.emit_for_session(session, |seq| OutEvent::SkillActive {
        session: session.clone(),
        seq,
        skill_id: Some(skill_id.to_string()),
        allowed_tools,
    });
}
