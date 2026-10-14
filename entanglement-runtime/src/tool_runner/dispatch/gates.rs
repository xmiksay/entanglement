//! The gate ladder every call climbs, model-issued or script-issued
//! (ADR-0227): resolve the tool (lazy MCP connect), the `Control` bypass,
//! overlay withdrawal, mode grade + grants, the `pre_tool_use` veto, the
//! escape-root gate (ADR-0109), the credentialed-`http` gate — then run,
//! refuse, or park for approval.

use std::path::PathBuf;

use entanglement_core::{Permission, ToolOverlayEntry};

use crate::capability;
use crate::permission::{overlay_denies, overlay_grade_entry};
use crate::tools::ToolRegistry;
use crate::{mode, run_limits};

use super::decision::Prompt;
use super::grade::{grade_call, graded_name, GradeInputs};
use super::{script, Call, Dispatcher, Outcome, Via};

/// What [`Dispatcher::clear`] decided: run (with the overlay entry that
/// graded it, and the out-of-root path a trusted / **Run all** script's call
/// reaches — its single-use escape token is recorded just before the host
/// tool runs), or refuse with the reason.
pub(super) enum Cleared {
    Run(Option<ToolOverlayEntry>, Option<PathBuf>),
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
            return Some(self.execute(&tools, call, via, None, None).await);
        }
        Some(match self.clear(&tools, &call, via).await? {
            Cleared::Run(overlay_entry, escape_token) => {
                self.execute(&tools, call, via, overlay_entry, escape_token)
                    .await
            }
            Cleared::Refused(why) => Outcome::Refused(why),
        })
    }

    /// The whole ladder for a model call whose route runs its own handler
    /// once cleared — `agent`/`agent_send` (ADR-0232): overlay deny, grade,
    /// hooks, mode deny, the unattended collapse and the approval park, the
    /// same rungs as any tool. `Err` is the refusal text the model sees;
    /// `None` when a `Stop` unwound the park.
    pub(crate) async fn gate(&self, call: &Call) -> Option<Result<(), String>> {
        Some(match self.clear(&self.tools, call, Via::Model).await? {
            Cleared::Run(..) => Ok(()),
            Cleared::Refused(why) => Err(why),
        })
    }

    /// Every gate short of execution: overlay deny, grade + grants,
    /// `pre_tool_use`, the mode deny, escape-root, the credentialed-`http`
    /// gate, the unattended collapse and the approval park. `None` when a
    /// `Stop` unwound the park.
    async fn clear(&self, tools: &ToolRegistry, call: &Call, via: Via<'_>) -> Option<Cleared> {
        let (perm, arg, overlay_entry) = match self.grade(tools, &call.tool, &call.input).await {
            Ok(graded) => graded,
            Err(why) => return Some(Cleared::Refused(why)),
        };
        if let Some(reason) = self
            .hooks
            .run_pre_tool_use(&self.session, &call.tool, &call.input)
            .await
        {
            return Some(Cleared::Refused(reason));
        }
        if perm == Permission::Deny {
            // ADR-0207 §4: absolute, no prompt, names the way out.
            return Some(Cleared::Refused(format!(
                "tool `{}` denied by mode `{}` — use /mode to switch",
                call.tool, self.mode
            )));
        }
        // ADR-0109: an out-of-root target forces approval unless durably
        // granted for this exact `(tool, path)`.
        let escape = self
            .escape_root
            .as_ref()
            .and_then(|er| er.escaping(&call.tool, &call.input))
            .filter(|abs| {
                self.escape_root
                    .as_ref()
                    .is_some_and(|er| !er.store.is_durably_allowed(&call.tool, abs))
            });
        // ADR-0227: a credentialed `http` call forces one approval per host
        // per session, whatever the grade.
        let host = self
            .http_gate
            .credentialed_host(&call.tool, &call.input)
            .filter(|host| !self.http_gate.is_approved(&self.session, host));
        let run = Cleared::Run(overlay_entry.clone(), None);
        let floored = self.floored(graded_name(&call.tool), &call.input);
        // ADR-0228: a trusted script, or one the user chose **Run all** for,
        // passes every remaining gate — a graded `Ask`, escape-root (a
        // single-use token the host tool spends), the credentialed-`http`
        // approval. The denies above (overlay, hook, mode, ceiling) already
        // won. A floored write is the exception for **Run all**: approving a
        // script once must not let it author trusted code unseen, so that
        // write gets its own card. A trusted script's writes are its
        // owner's, and pass.
        if let Via::Script(sc) = via {
            if sc.run_all() && (!floored || sc.trusted()) {
                return Some(Cleared::Run(overlay_entry, escape));
            }
        }
        let unattended = run_limits::collapses_ask(&self.limits);
        if let Some(host) = &host {
            if unattended {
                return Some(
                    if perm == Permission::Allow && self.explicit_http_allow(&arg) {
                        run
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
        if escape.is_none() && host.is_none() {
            if perm == Permission::Allow {
                return Some(run);
            }
            // A script asks once per exact call per run (tool + full input).
            if let Via::Script(sc) = via {
                if sc.was_approved(&call.tool, &call.input) {
                    return Some(run);
                }
            }
            // ADR-0207 §11: unattended, a first `Ask` collapses to a denial;
            // a repeat of the same call parks (bounded by the timeout).
            if unattended
                && !self
                    .denials
                    .record_repeat(&self.session, &call.tool, arg.as_deref())
            {
                return Some(Cleared::Refused(format!(
                    "tool `{}` denied — mode `{}` is unattended (question_timeout {}s, no one \
                     to prompt); call it again to request a one-time approval",
                    call.tool, self.mode, self.limits.question_timeout
                )));
            }
        }
        let prompt = match (escape, host) {
            (Some(abs), _) => Prompt::Escape {
                abs,
                graded_ask: perm == Permission::Ask,
            },
            (None, Some(host)) => Prompt::Host(host),
            (None, None) => Prompt::Grade,
        };
        self.park(call, via, arg, prompt, overlay_entry, floored)
            .await
    }

    /// The overlay deny, then the grade (overlay enable entry or mode grade,
    /// grants, the write floor) — the first rungs of every call, shared with
    /// `rhai`'s own launch (ADR-0228). `Err` is the refusal text.
    pub(crate) async fn grade(
        &self,
        tools: &ToolRegistry,
        tool: &str,
        input: &str,
    ) -> Result<(Permission, Option<String>, Option<ToolOverlayEntry>), String> {
        // Read-variants grade as the tool they ride on (ADR-0098/0206).
        let graded = graded_name(tool);
        // An overlay deny withdraws the tool outright (#634), ahead of hooks.
        if overlay_denies(&self.overlays, &self.chain, graded) {
            return Err(format!(
                "tool `{tool}` withdrawn for this session by /disable — /enable to restore"
            ));
        }
        let overlay_entry = overlay_grade_entry(&self.overlays, &self.chain, graded);
        let inputs = GradeInputs {
            tools,
            resolver: &*self.resolver,
            chain: &self.chain,
            grants: &*self.grants,
            session: &self.session,
            mode: &self.mode,
            overlay_entry: overlay_entry.as_ref(),
            ceiling: &self.ceiling,
            root: self.escape_root.as_ref().map(|er| er.root.as_path()),
            floor: Some((&*self.trust, self.files_root.as_path())),
        };
        let (perm, arg) = grade_call(&inputs, graded, input).await;
        Ok((perm, arg, overlay_entry))
    }

    /// Whether this call is a write the floor carries at `Ask` (ADR-0228) —
    /// in every mode, `auto` included.
    pub(super) fn floored(&self, tool: &str, input: &str) -> bool {
        crate::rhai_trust::floored_write(tool, input, &self.files_root, &self.trust)
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
