//! The run-against-the-registry step (issue #451): reached by a straight
//! `Allow`, an approval, and the `Control` bypass alike, so every path runs
//! a call identically — state tools, pre-dispatch argument validation,
//! `FileChange` capture, the loop-breaker, `load_skill` posture, the
//! `post_tool_use` hook. A script's call additionally runs under the
//! script's remaining budget (ADR-0227).

use std::collections::HashSet;
use std::sync::{Arc, Mutex, RwLock};

use entanglement_core::{Holly, OutEvent, SessionId, ToolCall, ToolOverlayEntry};

use crate::arg_validate;
use crate::skills::load_skill::parse_skill_id;
use crate::skills::SkillRegistry;
use crate::tool_names::LOAD_SKILL_TOOL;
use crate::tool_runner::call_grader::{self, CallGrader};
use crate::tools::{ToolExecution, ToolRegistry};

use super::{Call, Dispatcher, Outcome, Via};

impl Dispatcher {
    pub(super) async fn execute(
        &self,
        tools: &ToolRegistry,
        call: Call,
        via: Via<'_>,
        overlay_entry: Option<ToolOverlayEntry>,
    ) -> Outcome {
        let Call {
            request_id,
            tool,
            input,
            envelope,
        } = call;
        let session = &self.session;
        // `update_tasks` carries no host resource (#231, ADR-0049): emit its
        // snapshot on a fresh seq (#157) and ack.
        if crate::plan_tasks::is_state_tool(&tool) {
            self.holly.emit_for_session(session, |seq| {
                crate::plan_tasks::state_event(session, seq, &tool, &input)
                    .expect("is_state_tool ⇒ state_event is Some")
            });
            let ack = crate::plan_tasks::ack(&tool);
            self.hooks
                .run_post_tool_use(session, &tool, &input, &ack, false, None)
                .await;
            return Outcome::text(ack.to_string(), false);
        }
        // Pre-dispatch argument validation (#560, ADR-0196 §6): a schema
        // violation never reaches `Tool::run`.
        if let Some(spec) = tools.spec_for(&tool) {
            if let Some(mut violation) = arg_validate::validate(&spec.schema, &input) {
                // Core's `mcp` unwrap collapsed any duplicate key under
                // `args`; the envelope still carries it.
                if let Some(env) = &envelope {
                    violation.duplicate_keys = arg_validate::find_duplicate_keys(&env.input);
                }
                tracing::warn!(
                    tool = %tool,
                    violation = ?violation.lines(),
                    "tool call failed pre-dispatch schema validation"
                );
                // An MCP schema reaches the model only in a tool result; a
                // repeat violation points back instead of resending it.
                let already_delivered = {
                    let mut delivered = self
                        .advertising
                        .delivered
                        .lock()
                        .expect("delivered-schema mutex poisoned");
                    let seen = delivered.contains(session, &tool);
                    delivered.mark(session, &tool);
                    seen
                };
                let mut output = arg_validate::decline_text(&spec, &violation, already_delivered);
                if self.validation.note(session, &tool, &input, true) {
                    output.push_str("\n\n");
                    output.push_str(arg_validate::LOOP_BREAKER_NOTE);
                }
                self.hooks
                    .run_post_tool_use(session, &tool, &input, &output, true, None)
                    .await;
                return Outcome::text(output, true);
            }
        }
        // The call's own grading context, reachable mid-run for targets it
        // reaches beyond its argument (redirect hops, #808/ADR-0225).
        let grader = CallGrader {
            tools: tools.clone(),
            resolver: self.resolver.clone(),
            chain: self.chain.clone(),
            grants: self.grants.clone(),
            session: session.clone(),
            mode: self.mode.clone(),
            overlay_entry,
            overlay_denied: false,
            ceiling: self.ceiling.clone(),
            root: self.escape_root.as_ref().map(|er| er.root.clone()),
        };
        let tool_call = ToolCall {
            id: request_id,
            name: tool.clone(),
            input: input.clone(),
            provider_meta: None,
        };
        let started = std::time::Instant::now();
        let run = crate::file_change::capture_and_emit(
            &self.holly,
            session,
            call_grader::scope(grader, tools.execute(&tool_call, session)),
        );
        let execution = match via.budget(&tool) {
            Some(budget) => match tokio::time::timeout(budget, run).await {
                Ok(execution) => execution,
                Err(_) => {
                    return Outcome::TimedOut(format!(
                        "binding `{tool}` was cut off at the script's time budget — it may or \
                         may not have taken effect"
                    ))
                }
            },
            None => run.await,
        };
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let ToolExecution {
            mut content,
            is_error,
            exit_code,
        } = execution;
        let mut output_text = entanglement_core::content_text(&content);
        // Two identical failing calls in a row mean the schema was never the
        // problem (#560, ADR-0196 §6).
        if self.validation.note(session, &tool, &input, is_error) && is_error {
            output_text.push_str("\n\n");
            output_text.push_str(arg_validate::LOOP_BREAKER_NOTE);
            content.push(entanglement_core::ContentPart::text(format!(
                "\n\n{}",
                arg_validate::LOOP_BREAKER_NOTE
            )));
        }
        if tool == LOAD_SKILL_TOOL {
            activate_skill(
                &self.holly,
                &self.skills,
                &self.active_skill,
                session,
                &output_text,
            );
        }
        self.hooks
            .run_post_tool_use(session, &tool, &input, &output_text, is_error, exit_code)
            .await;
        Outcome::Ran {
            content,
            is_error,
            duration_ms: Some(duration_ms),
            exit_code,
        }
    }
}

/// Record `session`'s skill-active posture (#400, ADR-0106; posture-only
/// since ADR-0194) from a `load_skill` result's `skill_id:` header and tell
/// any listening head; a failed load (no header) is a no-op.
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
