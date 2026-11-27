//! The gate ladder every call climbs, model-issued or script-issued
//! (ADR-0227): resolve the tool (lazy MCP connect), the `Control` bypass,
//! the resolver's grade + grants, the credentialed-`http` gate — then run,
//! refuse, or park for approval.

use entanglement_core::Permission;

use crate::capability;
use crate::policy::{deny_text, Grade};
use crate::tools::ToolRegistry;
use crate::{mode, run_limits};

use super::decision::Prompt;
use super::grade::{grade_call, graded_name, GradeInputs};
use super::{script, Call, Dispatcher, Outcome, Via};

/// What [`Dispatcher::clear`] decided: run, or refuse with the reason.
pub(super) enum Cleared {
    Run,
    Refused(String),
}

impl Dispatcher {
    /// Grade and run one call. `None` when a `Stop` unwound it.
    pub(crate) async fn run(&self, call: Call, via: Via<'_>) -> Option<Outcome> {
        if let Via::Script(_) = via {
            if let Some(why) = script::unreachable(&call.tool, &self.tools) {
                return Some(Outcome::Refused(why));
            }
        }
        // A hallucinated name can never execute, so it is refused before the
        // ladder (#437) — after a lazy MCP connect had its chance (ADR-0201):
        // nothing connects at startup, so an unregistered `mcp__*` name
        // usually just means "not connected yet".
        let tools = match self.resolve_tools(&call.tool, via).await {
            Ok(tools) => tools,
            Err(msg) => return Some(Outcome::Refused(msg)),
        };
        // ADR-0207 §3: a Control-only tool reads/orchestrates session state
        // and is never graded (a script never gets here — `unreachable`);
        // the spawning pair is the exception (ADR-0232), gated in [`Self::gate`].
        if capability::bypasses_grading(&call.tool, capability::capability_of(&call.tool, &tools)) {
            return Some(self.execute(&tools, call, via).await);
        }
        Some(match self.clear(&tools, &call, via).await? {
            Cleared::Run => self.execute(&tools, call, via).await,
            Cleared::Refused(why) => Outcome::Refused(why),
        })
    }

    /// The whole ladder for a model call whose route runs its own handler
    /// once cleared — `agent`/`agent_send` (ADR-0232): grade, mode deny,
    /// the unattended collapse and the approval park, the same rungs as
    /// any tool. `Err` is the refusal text the model sees; `None` when a
    /// `Stop` unwound the park.
    pub(crate) async fn gate(&self, call: &Call) -> Option<Result<(), String>> {
        Some(match self.clear(&self.tools, call, Via::Model).await? {
            Cleared::Run => Ok(()),
            Cleared::Refused(why) => Err(why),
        })
    }

    /// Every gate short of execution: grade + grants, the resolver's deny,
    /// the credentialed-`http` gate, the unattended collapse and the
    /// approval park. `None` when a `Stop` unwound the park.
    async fn clear(&self, tools: &ToolRegistry, call: &Call, via: Via<'_>) -> Option<Cleared> {
        let (grade, arg) = self.grade(tools, &call.tool, &call.input).await;
        // ADR-0207 §4: absolute, no prompt — names the way out, or gives
        // the resolver's own reason (#964).
        let (perm, reason) = match grade {
            Grade::Deny(reason) => {
                return Some(Cleared::Refused(deny_text(&call.tool, &self.mode, reason)))
            }
            Grade::Allow => (Permission::Allow, None),
            Grade::Ask(reason) => (Permission::Ask, reason),
        };
        // ADR-0227: a credentialed `http` call forces one approval per host
        // per session, whatever the grade.
        let host = self
            .http_gate
            .credentialed_host(&call.tool, &call.input)
            .filter(|host| !self.http_gate.is_approved(&self.session, host));
        let floored = self.floored(tools, &call.tool, &call.input);
        // ADR-0228: a trusted script, or one the user chose **Run all** for,
        // passes every remaining gate — a graded `Ask` (a resolver's
        // reasoned one included) and the credentialed-`http` approval. The
        // denies above (mode, ceiling) already won. A floored write is the
        // exception for **Run all**: approving a script once must not let
        // it author trusted code unseen, so that write gets its own card. A
        // trusted script's writes are its owner's, and pass.
        if let Via::Script(sc) = via {
            if sc.run_all() && (!floored || sc.trusted()) {
                return Some(Cleared::Run);
            }
        }
        let unattended = run_limits::unattended(&self.limits, &self.attendance, &self.chain);
        if let Some(host) = &host {
            if unattended {
                return Some(
                    if perm == Permission::Allow && self.explicit_http_allow(&arg) {
                        Cleared::Run
                    } else {
                        Cleared::Refused(format!(
                            "tool `{}` refused — an authenticated call to {host} needs approval \
                             and mode `{}` is unattended; an explicit `http(...)` allow rule \
                             permits it",
                            call.tool, self.mode
                        ))
                    },
                );
            }
        }
        if host.is_none() {
            if perm == Permission::Allow {
                return Some(Cleared::Run);
            }
            // A script asks once per exact call per run (tool + full input).
            if let Via::Script(sc) = via {
                if sc.was_approved(&call.tool, &call.input) {
                    return Some(Cleared::Run);
                }
            }
            // ADR-0207 §11: unattended, a first `Ask` collapses to a denial;
            // a repeat of the same call — graded by the same rule, ADR-0247
            // — parks (bounded by the timeout).
            if unattended
                && !self
                    .denials
                    .record_repeat(&self.session, self.repeat_of(call, arg.as_deref()))
            {
                return Some(Cleared::Refused(format!(
                    "tool `{}` denied — mode `{}` is unattended (question_timeout {}s, no one \
                     to prompt); call it again to request a one-time approval",
                    call.tool, self.mode, self.limits.question_timeout
                )));
            }
        }
        let prompt = match host {
            Some(host) => Prompt::Host(host),
            None => Prompt::Grade(reason),
        };
        self.park(call, via, arg, prompt, floored).await
    }

    /// The grade (mode grade, grants, the write floor) — the first rung of
    /// every call, shared with `rhai`'s own launch (ADR-0228).
    ///
    /// A tool returning [`CallGrading`][crate::tools::CallGrading] from its
    /// `call_grading(input)` is graded under the target name and capabilities
    /// it returns (ADR-0249): the resolver, the grants and the
    /// approval card all see the target, not the envelope tool.
    pub(crate) async fn grade(
        &self,
        tools: &ToolRegistry,
        tool: &str,
        input: &str,
    ) -> (Grade, Option<String>) {
        // A per-call grading override: the tool tells us what name and
        // capabilities to grade as (e.g. an embedder's `invoke` -> its target).
        let call_grade = tools.get(tool).and_then(|t| t.call_grading(input));
        let graded = call_grade.as_ref().map_or(tool, |cg| cg.name.as_str());
        let inputs = GradeInputs {
            tools,
            resolver: &*self.resolver,
            chain: &self.chain,
            grants: &*self.grants,
            session: &self.session,
            mode: &self.mode,
            root: self.grading_root.as_deref(),
            floor: Some((&*self.trust, self.files_root.as_path())),
            capability_override: call_grade.as_ref().map(|cg| cg.capabilities),
        };
        grade_call(&inputs, graded, input).await
    }

    /// Whether this call is a write the floor carries at `Ask` (ADR-0228) —
    /// in every mode, `auto` included.
    pub(super) fn floored(&self, tools: &ToolRegistry, tool: &str, input: &str) -> bool {
        crate::rhai_trust::floored_write(tools, tool, input, &self.files_root, &self.trust)
    }

    /// What makes a retry of `call` the same call (ADR-0247): the session
    /// mode's named rule that graded it `Ask`, else its exact argument —
    /// a default grade, or an `Ask` from elsewhere (an ancestor's clamp),
    /// keys on the argument as before.
    fn repeat_of(&self, call: &Call, arg: Option<&str>) -> run_limits::Repeat {
        let tool = &graded_name(&self.tools, &call.tool, &call.input);
        let rule = self.mode_table.get(&self.mode).and_then(|mode| {
            let capabilities = capability::capability_of(tool, &self.tools).unwrap_or(&[]);
            let workdir = crate::permission::permission_workdir(&self.tools, tool, &call.input);
            let kind = crate::permission::permission_scope(&self.tools, tool).arg;
            mode.deciding_rule(tool, capabilities, arg, kind, workdir.as_deref())
        });
        match rule {
            Some((key, Permission::Ask)) => run_limits::Repeat::Rule(key),
            _ => run_limits::Repeat::Call {
                tool: call.tool.clone(),
                arg: arg.map(str::to_string),
            },
        }
    }

    /// Whether the mode carries an explicit, argument-scoped `http(...)`
    /// rule allowing this call — the only thing that lets an unattended mode
    /// make a credentialed call (a class-wide `exec` allow does not).
    fn explicit_http_allow(&self, arg: &Option<String>) -> bool {
        let Some(mode) = self.mode_table.get(&self.mode) else {
            return false;
        };
        let untrusted = |key: &str| self.http_gate.is_untrusted_rule(&self.mode, key);
        mode::rules::scoped_grade(
            &mode.rules,
            crate::tool_names::HTTP_TOOL,
            arg.as_deref(),
            untrusted,
        ) == Some(Permission::Allow)
    }
}
