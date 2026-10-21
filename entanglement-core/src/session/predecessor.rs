//! A session task started while an earlier task under its id is still
//! tearing down waits for that task's exit before it does anything (#892,
//! ADR-0244).

use tokio::sync::{mpsc, watch};

use super::SessionCmd;

/// The exit signal a hibernated task leaves behind: `true` once it has
/// exited. Its sender is dropped only after `true` was sent, so a closed
/// signal means exited too.
pub(crate) type ExitSignal = watch::Receiver<bool>;

/// Wait for the previous task under this id to exit, then say whether this
/// task should start. Until then it neither registers nor emits (a resume's
/// replay already ran in the supervisor): commands buffer in its inbox in
/// arrival order, as they do for a live session busy in an await that
/// doesn't watch its inbox. If the
/// inbox closed meanwhile (the id was closed or hibernated before this task
/// ever ran), it never starts and ends without emitting.
pub(super) async fn ready(prior: Option<ExitSignal>, rx: &mpsc::Receiver<SessionCmd>) -> bool {
    let Some(mut prior) = prior else {
        return true;
    };
    let _ = prior.wait_for(|exited| *exited).await;
    !rx.is_closed()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn waits_for_the_signal_and_refuses_a_closed_inbox() {
        let (_tx, rx) = mpsc::channel::<SessionCmd>(1);
        assert!(ready(None, &rx).await, "no predecessor: start at once");

        let (exited, signal) = watch::channel(false);
        let wait = tokio::spawn(async move { ready(Some(signal), &rx).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!wait.is_finished(), "must wait for the old task");
        exited.send_replace(true);
        assert!(wait.await.unwrap(), "inbox still open: start");

        let (tx, rx) = mpsc::channel::<SessionCmd>(1);
        let (exited, signal) = watch::channel(false);
        drop(tx);
        drop(exited); // a dropped signal counts as exited
        assert!(!ready(Some(signal), &rx).await, "closed before it ran");
    }
}
