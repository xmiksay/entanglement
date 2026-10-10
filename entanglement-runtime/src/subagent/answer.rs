//! Collecting and bounding a child's answer — shared by the blocking
//! `agent` path, the background watcher, and `poll`.

use std::time::Duration;

use entanglement_core::{Holly, OutEvent, SessionId};
use tokio::sync::broadcast::{error::RecvError, Receiver};

use crate::host::MAX_OUTPUT_BYTES;
use crate::retained_output::RetainedOutputRegistry;

/// Render a sub-agent's completion reply: the "completed in Xs" status line
/// (kept verbatim) followed by its `answer`, bounded to
/// [`crate::host::MAX_OUTPUT_BYTES`] with a head+tail split so a long answer's
/// conclusion survives truncation — the same shape `bash`/`call`/`rhai` now
/// share instead of returning an unbounded answer (#622). Shared by the
/// blocking `agent` path ([`launch`]) and `poll`.
///
/// Capping alone would trade a context blow-up for silently discarded work
/// (#614), so a truncated answer also mints a retained-output handle
/// (`owner`-scoped, the same "work you might still have questions about" rule
/// `call` follows) that `poll` pages the rest of.
pub fn format_agent_answer(
    agent_id: impl std::fmt::Display,
    elapsed: Duration,
    answer: String,
    retained: &RetainedOutputRegistry,
    owner: Option<&SessionId>,
) -> String {
    let status = format!(
        "sub-agent `{agent_id}` completed in {:.1}s:\n\n",
        elapsed.as_secs_f64()
    );
    bound_answer(status, answer, retained, owner)
}

/// Shared truncation-with-a-handle shape for a sub-agent's answer: `status` is
/// kept verbatim, `answer` gets [`crate::host::bounded_result`]'s head+tail
/// byte cap, and if the combined result would overflow, the *full* `answer`
/// is registered with `retained` (scoped to `owner`) so `poll` can page past
/// the cap instead of the excess vanishing (#614). Shared by
/// [`format_agent_answer`] and the sponsored-build reply in `propose_plan`,
/// whose status line names the plan file too.
pub(crate) fn bound_answer(
    mut status: String,
    answer: String,
    retained: &RetainedOutputRegistry,
    owner: Option<&SessionId>,
) -> String {
    if status.len() + answer.len() > MAX_OUTPUT_BYTES {
        let handle = retained.register_text(owner.cloned(), answer.clone());
        status.push_str(&format!(
            "[answer truncated — poll(handle=\"{handle}\") for the rest]\n"
        ));
    }
    crate::host::bounded_result(&status, answer)
}

/// Watch the child's event stream, accumulating its assistant text until the
/// child *genuinely* finishes. Returns the final answer, or an explanatory note
/// when the child produced nothing. Public so the sponsored build launch in
/// `propose_plan.rs` can reuse the exact same accumulation (ADR-0138).
///
/// A turn's `Done` is **not** by itself the end of the wait (#562, amends
/// ADR-0111/ADR-0123/ADR-0138 — see ADR-0155): the engine emits `Done` even for
/// a turn that ended in `Error` (`emit_turn_error` fires both), so a child that
/// ran out of budget mid-answer would otherwise resolve the parent with a bare
/// "sub-agent ended with error" and let it conclude on top of a failed child. If
/// a turn's `Done` lands with no accumulated text *and* that turn surfaced an
/// `Error`, the child session is still alive and steerable (prompting it
/// "continue" starts a new turn) — this clears the per-turn text/error and keeps
/// watching instead of breaking, and tells `parent` why it's still parked so the
/// user knows to steer the child. The wait only ends definitively on a turn
/// whose `Done` carries a usable answer, or on the child's `SessionEnded`/
/// `SessionHibernated` — a lagging or closed watcher still breaks defensively so
/// a missed event can't park the parent forever.
pub async fn collect_child_answer(
    holly: &Holly,
    parent: &SessionId,
    events: &mut Receiver<OutEvent>,
    child: &SessionId,
) -> String {
    // Rebound when the child compacts (ADR-0205): its context overflowed, so
    // the engine forked it into a successor and retired it. The answer this
    // parent is parked on now comes from the successor, and the retired id's
    // `SessionEnded` must not be read as "the child finished".
    let mut child = child.clone();
    let mut text = String::new();
    // This turn's error, if any — consumed (`.take()`) whenever it gates a
    // "keep waiting" decision, so a later empty-and-error-free turn isn't
    // mistaken for a still-erroring one.
    let mut error: Option<String> = None;
    // The most recent turn's error, kept across a "keep waiting" reset purely
    // as the fallback message if the wait ends (`SessionEnded`/lagged/closed)
    // without ever landing an answer — losing it to "produced no output" would
    // erase the one piece of context the user has for why the child stalled.
    let mut last_error: Option<String> = None;
    loop {
        match events.recv().await {
            // Checked before the session filter below — the announcement is
            // the successor's, not the retired child's.
            Ok(OutEvent::SessionStarted {
                session: successor,
                predecessor: Some(source),
                ..
            }) if source == child => {
                tracing::debug!(%source, %successor, "sub-agent watch follows the compaction successor");
                child = successor;
            }
            Ok(ev) if ev.session() != Some(&child) => {}
            Ok(OutEvent::TextDelta { text: delta, .. }) => text.push_str(&delta),
            // An ambiguous-stop retry (ADR-0118) supersedes the truncated round:
            // drop the partial text so the final answer is the recovered round's
            // alone, not the discarded round concatenated onto it.
            Ok(OutEvent::AmbiguousRetry { .. }) => text.clear(),
            Ok(OutEvent::Error { message, .. }) => {
                last_error = Some(message.clone());
                error = Some(message);
            }
            Ok(OutEvent::Done { .. }) => {
                if text.trim().is_empty() {
                    if let Some(message) = error.take() {
                        text.clear();
                        holly.emit_for_session(parent, |seq| OutEvent::Error {
                            session: parent.clone(),
                            seq,
                            message: format!(
                                "sub-agent `{child}` ended its turn in error with no answer \
                                 ({message}); it is still alive — steer it (e.g. prompt it to \
                                 continue) to resolve this wait."
                            ),
                        });
                        continue;
                    }
                }
                break;
            }
            Ok(OutEvent::SessionEnded { .. }) | Ok(OutEvent::SessionHibernated { .. }) => break,
            Ok(_) => {}
            // A lagging watcher could miss the child's `Done` and park the parent
            // forever; surface what we have instead of blocking indefinitely.
            Err(RecvError::Lagged(_)) => break,
            Err(RecvError::Closed) => break,
        }
    }
    let text = text.trim();
    match (text.is_empty(), last_error) {
        (false, _) => text.to_string(),
        (true, Some(e)) => format!("sub-agent ended with error: {e}"),
        (true, None) => "sub-agent produced no output".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::EngineConfig;

    fn empty_engine() -> Holly {
        Holly::spawn(EngineConfig::default())
    }

    /// #622: a huge answer keeps a head + tail slice instead of growing the
    /// reply unbounded — the status line always survives intact.
    #[test]
    fn format_agent_answer_bounds_a_huge_answer() {
        use crate::host::MAX_OUTPUT_BYTES;
        let mut answer = String::from("HEAD_MARKER");
        answer.push_str(&"x".repeat(MAX_OUTPUT_BYTES * 2));
        answer.push_str("TAIL_MARKER");
        let retained = RetainedOutputRegistry::new();
        let out = format_agent_answer(
            SessionId::new("child-1"),
            Duration::from_secs(3),
            answer,
            &retained,
            None,
        );
        assert!(
            out.starts_with("sub-agent `child-1` completed in 3.0s:\n\n"),
            "status line dropped: {out}"
        );
        assert!(out.contains("HEAD_MARKER"), "head lost: {out}");
        assert!(out.ends_with("TAIL_MARKER"), "tail lost: {out}");
        assert!(out.contains("omitted from the middle"), "got: {out}");
    }

    #[test]
    fn format_agent_answer_passes_through_small_answer() {
        let retained = RetainedOutputRegistry::new();
        let out = format_agent_answer(
            SessionId::new("child-1"),
            Duration::from_secs(1),
            "hi".to_string(),
            &retained,
            None,
        );
        assert_eq!(out, "sub-agent `child-1` completed in 1.0s:\n\nhi");
    }

    /// #614: a truncated answer isn't just capped — it mints a retained-output
    /// handle so `poll` can page the rest, scoped to the given `owner`.
    #[test]
    fn format_agent_answer_truncation_mints_a_retained_handle() {
        use crate::host::MAX_OUTPUT_BYTES;
        let answer = "x".repeat(MAX_OUTPUT_BYTES * 2);
        let retained = RetainedOutputRegistry::new();
        let owner = SessionId::new("parent-1");
        let out = format_agent_answer(
            SessionId::new("child-1"),
            Duration::from_secs(2),
            answer.clone(),
            &retained,
            Some(&owner),
        );
        let handle = out
            .lines()
            .find_map(|l| l.split("poll(handle=\"").nth(1))
            .and_then(|s| s.split('"').next())
            .expect("handle in output");
        let page = retained
            .page(handle, &owner, 0, 0)
            .expect("retained entry exists for the owner");
        assert!(
            page.text.contains(&answer[answer.len() - 100..]),
            "full answer recoverable via the handle"
        );
        assert!(
            retained
                .page(handle, &SessionId::new("stranger"), 0, 0)
                .is_none(),
            "handle is scoped to the owner"
        );
    }

    /// Drain `sub` for up to a short deadline, returning the first event
    /// matching `pred`. Panics if none arrives — these tests only await events
    /// the code under test is expected to emit.
    async fn expect_event(
        sub: &mut Receiver<OutEvent>,
        pred: impl Fn(&OutEvent) -> bool,
    ) -> OutEvent {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match sub.recv().await.expect("event bus closed before match") {
                    ev if pred(&ev) => return ev,
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for expected event")
    }

    #[tokio::test]
    async fn collect_child_answer_waits_past_an_errored_turn_for_the_next_one() {
        // Repro shape for #562: the child's first turn errors with no usable
        // text (e.g. an exhausted 429 budget). The old code broke on that
        // turn's `Done` and folded the bare error in as the answer, unblocking
        // the parent on a failed child. The fix keeps watching — a second turn
        // (the child steered to "continue") with real text is what should
        // actually resolve the wait.
        let holly = empty_engine();
        let parent = SessionId::new("parent");
        let child = SessionId::new("child");
        let mut parent_events = holly.subscribe();
        let (tx, mut rx) = tokio::sync::broadcast::channel::<OutEvent>(16);

        let handle = {
            let holly = holly.clone();
            let parent = parent.clone();
            let child = child.clone();
            tokio::spawn(
                async move { collect_child_answer(&holly, &parent, &mut rx, &child).await },
            )
        };

        tx.send(OutEvent::Error {
            session: child.clone(),
            seq: 1,
            message: "out of tokens".to_string(),
        })
        .unwrap();
        tx.send(OutEvent::Done {
            session: child.clone(),
            seq: 2,
        })
        .unwrap();

        // The parent is told why it's still parked, not unblocked.
        let note = expect_event(
            &mut parent_events,
            |ev| matches!(ev, OutEvent::Error { session, .. } if session == &parent),
        )
        .await;
        match note {
            OutEvent::Error { message, .. } => {
                assert!(
                    message.contains("child"),
                    "should name the child: {message}"
                );
                assert!(
                    message.to_lowercase().contains("steer")
                        || message.to_lowercase().contains("continue"),
                    "should hint at steering: {message}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(
            !handle.is_finished(),
            "the wait must not have resolved on the errored turn's Done"
        );

        // The child is steered ("continue") and its next turn produces a real
        // answer.
        tx.send(OutEvent::TextDelta {
            session: child.clone(),
            seq: 3,
            text: "done for real".to_string(),
        })
        .unwrap();
        tx.send(OutEvent::Done {
            session: child.clone(),
            seq: 4,
        })
        .unwrap();

        let answer = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("collect_child_answer must resolve once the child truly finishes")
            .unwrap();
        assert_eq!(answer, "done for real");
    }

    #[tokio::test]
    async fn collect_child_answer_ends_on_session_ended_after_an_error() {
        // If the child never gets steered and its session simply ends, the
        // wait must still resolve — not hang forever.
        let holly = empty_engine();
        let parent = SessionId::new("parent2");
        let child = SessionId::new("child2");
        let (tx, mut rx) = tokio::sync::broadcast::channel::<OutEvent>(16);

        tx.send(OutEvent::Error {
            session: child.clone(),
            seq: 1,
            message: "out of tokens".to_string(),
        })
        .unwrap();
        tx.send(OutEvent::Done {
            session: child.clone(),
            seq: 2,
        })
        .unwrap();
        tx.send(OutEvent::SessionEnded {
            session: child.clone(),
            ts: 0,
        })
        .unwrap();

        let answer = tokio::time::timeout(
            Duration::from_secs(2),
            collect_child_answer(&holly, &parent, &mut rx, &child),
        )
        .await
        .expect("SessionEnded must end the wait");
        assert!(
            answer.contains("out of tokens"),
            "the best-effort error answer should surface: {answer}"
        );
    }

    #[tokio::test]
    async fn collect_child_answer_breaks_on_the_first_done_when_it_has_an_answer() {
        // The common case — no error at all — is unaffected: any `Done`
        // carrying usable text still ends the wait immediately.
        let holly = empty_engine();
        let parent = SessionId::new("parent3");
        let child = SessionId::new("child3");
        let (tx, mut rx) = tokio::sync::broadcast::channel::<OutEvent>(16);

        tx.send(OutEvent::TextDelta {
            session: child.clone(),
            seq: 1,
            text: "all good".to_string(),
        })
        .unwrap();
        tx.send(OutEvent::Done {
            session: child.clone(),
            seq: 2,
        })
        .unwrap();

        let answer = tokio::time::timeout(
            Duration::from_secs(1),
            collect_child_answer(&holly, &parent, &mut rx, &child),
        )
        .await
        .expect("a clean Done must resolve immediately");
        assert_eq!(answer, "all good");
    }
}
