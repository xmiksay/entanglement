//! The async side of the binding bridge: every script binding call goes
//! through the dispatch seam (ADR-0227) — the same ladder a model-issued
//! call climbs — and its outcome comes back to the blocking thread; plus the
//! approval park for `rhai`'s own launch gate.

use std::path::PathBuf;

use entanglement_core::{AgentState, ApprovalKind, Holly, SessionId};
use tokio::sync::mpsc;

use crate::pending::{self, PendingDecisions};
use crate::seam;
use crate::tool_names::HTTP_TOOL;
use crate::tool_runner::{Call, Dispatcher, Outcome, ScriptCall, Via};

use super::BindingCall;

/// Service binding calls until every sender is dropped — when the engine
/// finishes and its closures go. `spawn_blocking` cannot be aborted, so the
/// wall-clock bound lives inside the engine (progress callback) and on each
/// nested call (`Via::budget`), not here. Returns whether a `Stop` unwound a
/// nested approval.
pub(super) async fn service_bindings(
    mut rx: mpsc::UnboundedReceiver<BindingCall>,
    dispatcher: &Dispatcher,
    workdir: Option<&(PathBuf, PathBuf)>,
    script: &ScriptCall,
    request_id: &str,
) -> bool {
    let mut stopped = false;
    while let Some(call) = rx.recv().await {
        let input = workdir
            .and_then(|(root, wd)| crate::workdir::rewrite(root, Some(wd), &call.tool, &call.input))
            .unwrap_or(call.input);
        // The nested call's own id: the head's Approve/Reject matches it, and
        // a `Once` escape-root grant is bound to it (#449).
        let nested = Call {
            request_id: format!("{request_id}:rhai:{}", call.tool),
            tool: call.tool.clone(),
            input,
            envelope: None,
        };
        let result = match dispatcher.run(nested, Via::Script(script)).await {
            None => {
                stopped = true;
                Err("rhai run stopped".to_string())
            }
            Some(Outcome::Refused(why) | Outcome::TimedOut(why)) => Err(why),
            Some(Outcome::Ran {
                content, is_error, ..
            }) => {
                let text = entanglement_core::content_text(&content);
                if is_error && throws_on_error(&call.tool) {
                    Err(text)
                } else {
                    Ok(text)
                }
            }
        };
        let _ = call.reply.send(result);
    }
    stopped
}

/// Whether a failed run of `tool` throws in the script rather than returning
/// its error text. The file/exec bindings keep returning text (a script
/// reads a failed `read` like the model does); `http()` and an MCP tool
/// throw, so `try`/`catch` is the one way to handle a remote failure.
fn throws_on_error(tool: &str) -> bool {
    tool == HTTP_TOOL || tool.starts_with("mcp__")
}

/// Outcome of `rhai`'s own launch approval.
pub(super) enum Approval {
    Approved,
    Rejected(String),
    Stopped,
}

/// Emit `rhai`'s launch `ToolRequest` and park for the head's decision via
/// the lag-proof [`PendingDecisions`] registry (#156) — registered before
/// emitting so a fast decision can't race the waiter.
pub(super) async fn await_approval(
    holly: &Holly,
    pending: &PendingDecisions,
    session: &SessionId,
    request_id: &str,
    tool: &str,
    input: &str,
) -> Approval {
    let req = pending::approval(
        session,
        request_id,
        ApprovalKind::Rhai,
        tool,
        input,
        format!("`{tool}` graded Ask"),
    );
    let rx = pending::park_approval(holly, pending, req, tool);
    holly.emit_status(session, AgentState::WaitingApproval);
    match pending::await_decision(rx).await {
        seam::Decision::Approve { .. } => Approval::Approved,
        seam::Decision::Reject { reason } => {
            Approval::Rejected(reason.unwrap_or_else(|| "user".to_string()))
        }
        seam::Decision::Stop
        | seam::Decision::Answer { .. }
        | seam::Decision::Retract
        | seam::Decision::Replace { .. } => Approval::Stopped,
    }
}

#[cfg(test)]
mod tests {
    use super::throws_on_error;

    #[test]
    fn only_remote_bindings_throw_on_a_failed_run() {
        assert!(throws_on_error("http"));
        assert!(throws_on_error("mcp__gh__search"));
        assert!(!throws_on_error("read"));
        assert!(!throws_on_error("call"));
    }
}
