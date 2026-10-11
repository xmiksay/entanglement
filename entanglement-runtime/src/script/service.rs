//! The async side of the binding bridge: servicing each script binding call
//! through permission grading, the escape-root gate, and the approval
//! round-trip, then executing the delegated host tool.

use std::collections::HashSet;
use std::path::Path;

use entanglement_core::{
    AgentState, ApprovalKind, ApprovalScope, Holly, Permission, SessionId, ToolCall,
};

use crate::tools::ToolRegistry;
use tokio::sync::mpsc::{self};

use crate::pending::{self, PendingDecisions};
use crate::permission::permission_workdir;
use crate::permission_path::grading_arg;
use crate::seam;
use crate::tool_runner::EscapeRoot;

use super::{set_state, BindingCall, BindingPolicy};

/// Service binding calls until every sender is dropped — which happens only
/// when the engine finishes and the blocking closure returns (dropping the
/// registered functions that hold the senders). `spawn_blocking` cannot be
/// aborted, so the wall-clock timeout is enforced *inside* the engine by the
/// progress callback, not by dropping this task. Returns whether a `Stop`
/// unwound the run. Shared by the blocking path above and the detached
/// background path (#637).
///
/// The approval cache is keyed by `approval_cache_key`, not bare tool name:
/// for `call`/`bash` that key includes the resolved command line, so approving
/// `call(git status)` does not silently pre-clear `call(rm -rf /)` in the same
/// run (#419 fix A). Every other binding keeps the coarser per-function cache
/// (approve one `edit`, cover the rest) — its argument is always a file path
/// already implied by the tool, not an open-ended command line.
#[allow(clippy::too_many_arguments)]
pub(super) async fn service_bindings(
    mut rx: mpsc::UnboundedReceiver<BindingCall>,
    tools: &ToolRegistry,
    policy: &BindingPolicy,
    escape_root: Option<&EscapeRoot>,
    holly: &Holly,
    session: &SessionId,
    request_id: &str,
    pending: &PendingDecisions,
    detached: bool,
) -> bool {
    let mut approved: HashSet<String> = HashSet::new();
    let mut stopped = false;
    while let Some(mut call) = rx.recv().await {
        if let Some(input) = policy.rewrite(call.tool, &call.input) {
            call.input = input;
        }
        let (result, was_stopped) = service_binding(
            tools,
            policy,
            escape_root,
            holly,
            session,
            request_id,
            pending,
            &mut approved,
            &call,
            detached,
        )
        .await;
        let _ = call.reply.send(result);
        if was_stopped {
            stopped = true;
        }
    }
    stopped
}

/// Resolve one binding call per its policy and either run it or refuse. Returns
/// the script-visible result plus whether a `Stop` was seen (which unwinds the
/// whole run). An `Ask` is prompted **once per cache key per run** — see
/// [`approval_cache_key`]: for most bindings that's once per function (the
/// first `edit` asks, approval covers the rest — per-call prompts in a loop
/// would be noise); for `call`/`bash` it's once per resolved command line
/// (#419 fix A), since a single approved command must not silently clear a
/// different, more dangerous one in the same run.
///
/// Escape-root gate (ADR-0109, #446): mirrors `tool_runner::dispatch`. A path/
/// `workdir` that resolves outside the project root forces an `Ask` even when
/// `policy.decide` graded `Allow`, unless the user already durably granted this
/// exact `(tool, path)` — e.g. via an earlier direct call. Never applies once
/// the grade is `Deny` (refused above, same as `dispatch`). An escaping call
/// bypasses the coarse per-run `approved` cache — that cache exists to avoid
/// re-prompting an ordinary `Ask`, not to authorize a fresh out-of-root target —
/// so it always re-checks the `ExtraRootStore` instead.
#[allow(clippy::too_many_arguments)]
pub(super) async fn service_binding(
    tools: &ToolRegistry,
    policy: &BindingPolicy,
    escape_root: Option<&EscapeRoot>,
    holly: &Holly,
    session: &SessionId,
    request_id: &str,
    pending: &PendingDecisions,
    approved: &mut HashSet<String>,
    call: &BindingCall,
    detached: bool,
) -> (Result<String, String>, bool) {
    // This binding's own request id (distinct from the outer `rhai` call's):
    // the head's Approve/Reject for a nested `Ask` matches this, not the outer
    // call, and it doubles as the identity a `Once` escape-root grant is bound
    // to (#449) — threaded into `exec` below so the tool that redeems the grant
    // is the exact one that was approved, not a differently-keyed stand-in.
    let bind_rid = format!("{request_id}:rhai:{}", call.tool);

    let perm = match policy.decide(call.tool, &call.input).await {
        // Names the mode, matching `tool_runner::dispatch`'s own decline
        // text (ADR-0207 stage 4b) — covers both an ordinary mode `deny` and
        // an overlay withdrawal (#634), which also resolves to `Deny` here.
        Permission::Deny => {
            return (
                Err(format!(
                    "tool `{}` denied by mode `{}` — use /mode to switch",
                    call.tool, policy.mode
                )),
                false,
            )
        }
        perm => perm,
    };

    let escape = escape_root
        .and_then(|er| er.escaping(call.tool, &call.input).map(|abs| (er, abs)))
        .filter(|(er, abs)| !er.store.is_durably_allowed(call.tool, abs));

    if perm == Permission::Allow && escape.is_none() {
        return (Ok(exec(tools, session, &bind_rid, call).await), false);
    }

    let key = approval_cache_key(call.tool, &call.input, policy.root.as_deref());
    if escape.is_none() && approved.contains(&key) {
        return (Ok(exec(tools, session, &bind_rid, call).await), false);
    }

    // The card shows the binding's tool + args; the script source rode the
    // outer approval. An escaping call also carries the same "outside the
    // project root" warning a direct call's approval card would (ADR-0109) —
    // otherwise the user approves a generic-looking "{tool} (rhai)" prompt with
    // no signal the script is about to reach outside the project.
    let card_tool = format!("{} (rhai)", call.tool);
    let card_input = match &escape {
        Some((_, abs)) => format!(
            "{}\n\n⚠ accesses a path OUTSIDE the project root: {}",
            call.input,
            abs.display()
        ),
        None => call.input.clone(),
    };
    match await_approval(
        holly,
        pending,
        session,
        &bind_rid,
        &card_tool,
        &card_input,
        detached,
    )
    .await
    {
        Approval::Approved(scope) => {
            if let Some((er, abs)) = &escape {
                // Record into the same store a direct call's approval would
                // (`tool_runner::await_decision`), so the delegated host tool's
                // own containment check lets this call through. Bound to
                // `bind_rid` (#449) — the same id `exec` below hands the tool as
                // its request id, so a `Once` grant is redeemed by this exact
                // binding call, not a concurrently-running one.
                er.store.record(call.tool, abs, scope, &bind_rid);
            } else {
                approved.insert(key);
            }
            if !detached {
                set_state(holly, session, AgentState::Thinking);
            }
            (Ok(exec(tools, session, &bind_rid, call).await), false)
        }
        Approval::Rejected(reason) => {
            if !detached {
                set_state(holly, session, AgentState::Thinking);
            }
            (
                Err(format!("tool `{}` rejected: {reason}", call.tool)),
                false,
            )
        }
        Approval::Stopped => (Err("rhai run stopped".to_string()), true),
    }
}

/// The `approved` cache key for one binding call (#419 fix A). For `call`/
/// `bash` the key includes the resolved command line (`grading_arg`, same
/// extraction the permission grade itself uses, #485 — a no-op for these two
/// since neither is a path-arg tool, kept for one canonical extraction path)
/// **and** the `workdir` (#480) — a workdir-scoped rule (`tool{pattern}`) can
/// grade the same command differently in two directories, so an approval in
/// one workdir must not silently clear the same command in another. Every
/// other binding keeps the coarser bare-tool-name key (approve one `edit`,
/// cover the rest of the run) — that surface is already the fixed,
/// pre-existing "once per function" behavior this issue leaves unchanged.
pub(super) fn approval_cache_key(tool: &'static str, input: &str, root: Option<&Path>) -> String {
    match tool {
        "call" | "bash" => {
            let arg = grading_arg(tool, input, root).unwrap_or_default();
            let workdir = permission_workdir(tool, input).unwrap_or_default();
            format!("{tool}:{arg}:{workdir}")
        }
        _ => tool.to_string(),
    }
}

/// Execute a delegated host tool and return its text output verbatim (the
/// registry already formats failures as a string, so a binding never hard-errors
/// the run — it surfaces the message to the script). A `rhai` script is a text
/// context, so an image result (#221) collapses to its text parts (empty for an
/// image-only `read`) rather than smuggling base64 into the script.
/// `request_id` (#449) is this binding's own id (`bind_rid` in
/// [`service_binding`]) — carried as the `ToolCall`'s id so a delegated host
/// tool's `Once` escape-root grant, bound to that same id, is redeemed by this
/// exact call.
pub(super) async fn exec(
    tools: &ToolRegistry,
    session: &SessionId,
    request_id: &str,
    call: &BindingCall,
) -> String {
    let execution = tools
        .execute(
            &ToolCall {
                id: request_id.to_string(),
                name: call.tool.to_string(),
                input: call.input.clone(),
                provider_meta: None,
            },
            session,
        )
        .await;
    entanglement_core::content_text(&execution.content)
}

/// Outcome of a parked approval round-trip. `Approved` carries the scope
/// (#446) — needed to record an escape-root grant at the scope the user chose,
/// mirroring `tool_runner::await_decision`; `rhai`'s own gate ignores it.
pub(super) enum Approval {
    Approved(ApprovalScope),
    Rejected(String),
    Stopped,
}

/// Emit a `ToolRequest` and park for the head's decision via the lag-proof
/// [`PendingDecisions`] registry (#156). Shared by `rhai`'s own gate and each
/// binding's `Ask`; registers per `request_id` before emitting so a fast decision
/// routes to this waiter rather than racing a subscription that could lag.
/// `detached` (#637) suppresses the session-state transition: a background
/// script's Ask arrives outside any live turn, so flipping the session to
/// `WaitingApproval` (and back to `Thinking` on resolution) would strand a
/// stale status on an otherwise idle session — the `ToolRequest` event alone
/// carries the prompt to the head.
#[allow(clippy::too_many_arguments)]
pub(super) async fn await_approval(
    holly: &Holly,
    pending: &PendingDecisions,
    session: &SessionId,
    request_id: &str,
    tool: &str,
    input: &str,
    detached: bool,
) -> Approval {
    // Register before emitting so the inbound router can never resolve the
    // decision ahead of this waiter (#156).
    let req = pending::approval(
        session,
        request_id,
        ApprovalKind::Rhai,
        tool,
        input,
        format!("rhai binding `{tool}` graded Ask"),
    );
    let rx = pending::park_approval(holly, pending, req, tool);
    if !detached {
        set_state(holly, session, AgentState::WaitingApproval);
    }
    match pending::await_decision(rx).await {
        // `mode` (#560) is `propose_plan`-only; a rhai binding's own `Ask`
        // approval never sets it.
        seam::Decision::Approve { scope, .. } => Approval::Approved(scope),
        seam::Decision::Reject { reason } => {
            Approval::Rejected(reason.unwrap_or_else(|| "user".to_string()))
        }
        // `Stop`, a closed inbox, or an unexpected `Answer`/`Retract`/`Replace`
        // (the latter two `ask_user`-only, #515) all unwind the run.
        seam::Decision::Stop
        | seam::Decision::Answer { .. }
        | seam::Decision::Retract
        | seam::Decision::Replace { .. } => Approval::Stopped,
    }
}

#[cfg(test)]
mod tests {
    use super::approval_cache_key;

    /// #419 fix A: the `approved` cache key scopes `call`/`bash` to the
    /// resolved command line, not the bare tool name — approving one command
    /// must not silently clear a different one.
    #[test]
    fn approval_cache_key_scopes_exec_tools_by_command_not_just_tool_name() {
        let a = approval_cache_key("call", r#"{"command":"git","args":["status"]}"#, None);
        let b = approval_cache_key("call", r#"{"command":"rm","args":["-rf","/"]}"#, None);
        assert_ne!(
            a, b,
            "different call commands must get different cache keys"
        );

        let a_again = approval_cache_key("call", r#"{"command":"git","args":["status"]}"#, None);
        assert_eq!(a, a_again, "the same call command reuses its cache key");

        // Every other binding keeps the coarser bare-tool-name key (approve
        // one `edit`, cover the rest of the run) — unchanged by this fix.
        assert_eq!(
            approval_cache_key("edit", r#"{"path":"a.rs"}"#, None),
            approval_cache_key("edit", r#"{"path":"b.rs"}"#, None),
        );
    }

    /// #480: same command, different `workdir` — a workdir-scoped rule can
    /// grade these two calls differently, so an approval in one workdir must
    /// not silently clear the same command in another.
    #[test]
    fn approval_cache_key_scopes_exec_tools_by_workdir_too() {
        let a = approval_cache_key("bash", r#"{"command":"ls","workdir":"/tmp/a"}"#, None);
        let b = approval_cache_key("bash", r#"{"command":"ls","workdir":"/tmp/b"}"#, None);
        assert_ne!(
            a, b,
            "same command, different workdir must get different cache keys"
        );

        let a_again = approval_cache_key("bash", r#"{"command":"ls","workdir":"/tmp/a"}"#, None);
        assert_eq!(a, a_again, "the same command+workdir reuses its cache key");

        // No `workdir` at all is still its own (empty-suffix) key, distinct
        // from either workdir-scoped variant.
        let no_workdir = approval_cache_key("bash", r#"{"command":"ls"}"#, None);
        assert_ne!(no_workdir, a);
        assert_ne!(no_workdir, b);
    }
}
