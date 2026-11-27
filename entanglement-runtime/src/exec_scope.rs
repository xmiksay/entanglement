//! The `ToolExec` a running call belongs to (ADR-0212): the executor puts
//! each dispatched call's seq in a task-local scope, and an embedder's file
//! tools read it to name the change's exact log record (`skutter`'s
//! per-write backups, #932). A scope rather than a parameter, so the `Tool`
//! trait carries no bookkeeping argument and the executor never names the
//! backups.

use std::future::Future;

/// What the running call task tells a change about the `ToolExec` it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecScope {
    /// The `ToolExec`'s seq: the change names its exact log record.
    Seq(u64),
    /// A switched background script's call (ADR-0241): the launch's
    /// `ToolExec` is a record of the predecessor's log, not the successor's
    /// the change is recorded under.
    Adopted,
}

tokio::task_local! {
    /// Set by the executor around each dispatched call ([`with_exec_seq`]),
    /// read when a file tool records a change (the per-write backups).
    static EXEC_SCOPE: ExecScope;
}

/// Run `fut` (one dispatched tool call) with its `ToolExec` seq in scope, so
/// any change it records is placed in the right turn even when request ids
/// repeat across turns (Gemini's `name#ordinal`).
pub fn with_exec_seq<F: Future>(seq: u64, fut: F) -> impl Future<Output = F::Output> {
    EXEC_SCOPE.scope(ExecScope::Seq(seq), fut)
}

/// Run `fut` with no `ToolExec` seq in scope, inside a call task that has
/// one: a background script's nested call after its session compacted
/// (ADR-0241) records under the successor, whose log never had the launch's
/// `ToolExec`. Its seq, or its request id (repeating ids, ADR-0243), could
/// name an unrelated record there, so the change is marked adopted and
/// placed by its wall clock.
pub fn without_exec_seq<F: Future>(fut: F) -> impl Future<Output = F::Output> {
    EXEC_SCOPE.scope(ExecScope::Adopted, fut)
}

/// The running call's scope, `None` outside a dispatched call.
pub fn current_exec_scope() -> Option<ExecScope> {
    EXEC_SCOPE.try_with(|s| *s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A background script's call after its session compacted (ADR-0241) runs
    /// with the launch's `ToolExec` seq masked and marked adopted: the seq would
    /// name an unrelated record in the successor's log (ADR-0243).
    #[tokio::test]
    async fn without_exec_seq_masks_the_enclosing_seq() {
        let seen = with_exec_seq(7, async {
            let outer = current_exec_scope();
            let inner = without_exec_seq(async { current_exec_scope() }).await;
            (outer, inner, current_exec_scope())
        })
        .await;
        let seq = Some(ExecScope::Seq(7));
        assert_eq!(seen, (seq, Some(ExecScope::Adopted), seq));
        assert_eq!(current_exec_scope(), None, "no scope at all");
    }
}
