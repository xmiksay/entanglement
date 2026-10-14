//! The async side of the binding bridge: every script binding call goes
//! through the dispatch seam (ADR-0227) — the same ladder a model-issued
//! call climbs — and its outcome comes back to the blocking thread.

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::tool_names::HTTP_TOOL;
use crate::tool_runner::{Call, Dispatcher, Outcome, ScriptCall, Via};

use super::activity::NestedCalls;
use super::BindingCall;

/// Service binding calls until every sender is dropped — when the engine
/// finishes and its closures go. `spawn_blocking` cannot be aborted, so the
/// wall-clock bound lives inside the engine (progress callback) and on each
/// nested call (`Via::budget`), not here. Every call is reported live as a
/// `NestedCall` (capped, with one closing summary — ADR-0233) — here, since
/// this loop is the one place every binding of every run passes through.
/// Returns whether a `Stop` unwound a nested approval.
pub(super) async fn service_bindings(
    mut rx: mpsc::UnboundedReceiver<BindingCall>,
    dispatcher: &Dispatcher,
    workdir: Option<&(PathBuf, PathBuf)>,
    script: &ScriptCall,
    request_id: &str,
) -> bool {
    let mut stopped = false;
    let mut report = NestedCalls::new(
        dispatcher.holly.clone(),
        dispatcher.session.clone(),
        request_id,
    );
    while let Some(call) = rx.recv().await {
        let input = workdir
            .and_then(|(root, wd)| crate::workdir::rewrite(root, Some(wd), &call.tool, &call.input))
            .unwrap_or(call.input);
        // The nested call's own id: the head's Approve/Reject matches it, and
        // a `Once` escape-root grant is bound to it (#449).
        report.begin(&call.tool, &input);
        let nested = Call {
            request_id: format!("{request_id}:rhai:{}", call.tool),
            tool: call.tool.clone(),
            input,
            envelope: None,
            rhai_path: call.rhai_path,
        };
        let (result, failed) = match dispatcher.run(nested, Via::Script(script)).await {
            None => {
                stopped = true;
                (Err("rhai run stopped".to_string()), true)
            }
            Some(Outcome::Refused(why) | Outcome::TimedOut(why)) => (Err(why), true),
            Some(Outcome::Ran {
                content, is_error, ..
            }) => {
                let text = entanglement_core::content_text(&content);
                if is_error && throws_on_error(&call.tool) {
                    (Err(text), true)
                } else {
                    (Ok(text), is_error)
                }
            }
        };
        report.end(failed);
        let _ = call.reply.send(result);
    }
    // The summary goes out here, before the caller replies; a `Stop` that
    // aborts this task instead drops `report` mid-await, which still emits it.
    drop(report);
    stopped
}

/// Whether a failed run of `tool` throws in the script rather than returning
/// its error text. The file/exec bindings keep returning text (a script
/// reads a failed `read` like the model does); `http()` and an MCP tool
/// throw, so `try`/`catch` is the one way to handle a remote failure.
fn throws_on_error(tool: &str) -> bool {
    tool == HTTP_TOOL || tool.starts_with("mcp__")
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
