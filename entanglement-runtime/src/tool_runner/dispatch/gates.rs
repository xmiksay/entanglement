//! The gate ladder every call climbs, model-issued or script-issued
//! (ADR-0227): resolve the tool (lazy MCP connect), alias rewrite, the
//! `Control` bypass, overlay withdrawal, mode grade + grants, the
//! `pre_tool_use` veto, the escape-root gate (ADR-0109), the
//! credentialed-`http` gate — then run, refuse, or park for approval.

use entanglement_core::Permission;

use crate::capability::{self, Capability};
use crate::permission::{overlay_denies, overlay_grade_entry};
use crate::tool_advertising;
use crate::tools::ToolRegistry;
use crate::{mode, run_limits};

use super::decision::Prompt;
use super::grade::{grade_call, graded_name, GradeInputs};
use super::{script, Call, Dispatcher, Outcome, Via};

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
        let tools = match self.resolve_tools(&call.tool).await {
            Ok(tools) => tools,
            Err(msg) => return Some(Outcome::Refused(msg)),
        };
        // A skill alias (#560 P8) grades as the tool it wraps: rewritten
        // before anything below looks at the name.
        let call = match tools
            .get(&call.tool)
            .and_then(|t| t.alias_rewrite(&call.input))
        {
            Some((tool, input)) => Call {
                tool,
                input,
                ..call
            },
            None => call,
        };
        // ADR-0207 §3: a Control-only tool reads/orchestrates session state
        // and is never graded (a script never gets here — `unreachable`).
        if capability::capability_of(&call.tool, &tools) == Some([Capability::Control].as_slice()) {
            return Some(self.execute(&tools, call, via, None).await);
        }
        // Read-variants grade as the tool they ride on (ADR-0098/0206).
        let graded = graded_name(&call.tool);
        // An overlay deny withdraws the tool outright (#634), ahead of hooks.
        if overlay_denies(&self.overlays, &self.chain, graded) {
            return Some(Outcome::Refused(format!(
                "tool `{}` withdrawn for this session by /disable — /enable to restore",
                call.tool
            )));
        }
        let overlay_entry = overlay_grade_entry(&self.overlays, &self.chain, graded);
        let inputs = GradeInputs {
            tools: &tools,
            resolver: &*self.resolver,
            chain: &self.chain,
            grants: &*self.grants,
            session: &self.session,
            mode: &self.mode,
            overlay_entry: overlay_entry.as_ref(),
            ceiling: &self.ceiling,
            root: self.escape_root.as_ref().map(|er| er.root.as_path()),
        };
        let (perm, arg) = grade_call(&inputs, graded, &call.input).await;
        if let Some(reason) = self
            .hooks
            .run_pre_tool_use(&self.session, &call.tool, &call.input)
            .await
        {
            return Some(Outcome::Refused(reason));
        }
        if perm == Permission::Deny {
            // ADR-0207 §4: absolute, no prompt, names the way out.
            return Some(Outcome::Refused(format!(
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
        let unattended = run_limits::collapses_ask(&self.limits);
        if let Some(host) = &host {
            if unattended {
                return Some(
                    if perm == Permission::Allow && self.explicit_http_allow(&arg) {
                        self.execute(&tools, call, via, overlay_entry).await
                    } else {
                        Outcome::Refused(format!(
                        "tool `{}` refused — an authenticated call to {host} needs approval and \
                         mode `{}` is unattended; an explicit `http(...)` allow rule permits it",
                        call.tool, self.mode
                    ))
                    },
                );
            }
        }
        if escape.is_none() && host.is_none() {
            if perm == Permission::Allow {
                return Some(self.execute(&tools, call, via, overlay_entry).await);
            }
            // A script asks once per exact call per run (tool + full input).
            if let Via::Script(sc) = via {
                if sc.was_approved(&call.tool, &call.input) {
                    return Some(self.execute(&tools, call, via, overlay_entry).await);
                }
            }
            // ADR-0207 §11: unattended, a first `Ask` collapses to a denial;
            // a repeat of the same call parks (bounded by the timeout).
            if unattended
                && !self
                    .denials
                    .record_repeat(&self.session, &call.tool, arg.as_deref())
            {
                return Some(Outcome::Refused(format!(
                    "tool `{}` denied — mode `{}` is unattended (question_timeout {}s, no one \
                     to prompt); call it again to request a one-time approval",
                    call.tool, self.mode, self.limits.question_timeout
                )));
            }
        }
        let prompt = match (escape, host) {
            (Some(abs), _) => Prompt::Escape(abs),
            (None, Some(host)) => Prompt::Host(host),
            (None, None) => Prompt::Grade,
        };
        self.park(&tools, call, via, arg, prompt, overlay_entry)
            .await
    }

    /// The registry this call runs against: the scoped session's own MCP
    /// tools when scoped (#684), then a lazy connect for an unregistered
    /// `mcp__*` name (ADR-0201/0226), else the snapshot.
    async fn resolve_tools(&self, tool: &str) -> Result<ToolRegistry, String> {
        let tools = match &self.mcp_scopes {
            Some(scopes) => {
                let tools = scopes
                    .overlay_registry_for_call(&self.session, self.tools.clone(), tool)
                    .await?;
                if let Some(decline) = scopes.scope_decline(&self.session, &tools, tool) {
                    return Err(decline);
                }
                tools
            }
            None => self.tools.clone(),
        };
        if tools.contains(tool)
            || crate::plan_tasks::is_state_tool(tool)
            || tool == crate::tool_names::REQUEST_MODE_TOOL
        {
            return Ok(tools);
        }
        if tool.starts_with("mcp__") {
            return crate::mcp::available::resolve_unregistered(
                &self.mcp_avail,
                tool,
                &self.registry,
                &self.mcp_active,
                self.mcp_http.as_ref(),
            )
            .await;
        }
        Err(tool_advertising::unknown_tool_reply(&tools, tool))
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
