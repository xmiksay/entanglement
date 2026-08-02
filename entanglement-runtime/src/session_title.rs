//! Auto session-title generator (Issue 5, the tui-ux-batch plan).
//!
//! On the **first user prompt** of a session, spawns a background task that
//! asks the aux `session_title` LLM for a short title and sets it via
//! [`InMsg::SetSessionMeta`]. Purely runtime-side: core never sees the aux
//! `Llm` — the generator builds its own one-shot client through the
//! [`AuxLlmRegistry`] (falling back to the primary model when no pin is set),
//! drains the stream for text, and sends the result back as a normal
//! `SetSessionMeta` frame the engine folds like any user-authored name.
//!
//! Best-effort throughout: a failed/empty/truncated generation is logged and
//! dropped (the session keeps its default name). Fired *alongside* the main
//! turn by default — except when the resolved aux model's per-model
//! concurrency cap is [`CONTENDED_CONCURRENCY_CEILING`] or below (#589): with
//! only one permit available, the main turn's own request is guaranteed to
//! hold it first, so firing concurrently would just have this generator's
//! call block behind the main turn's for the whole turn regardless — the
//! generator instead waits for the main turn's first `Done`/`Error` (bounded
//! by [`DEFER_TIMEOUT`]) before making its aux call, so the two are sequenced
//! rather than silently contending for a permit neither can win early.
//!
//! A session that already has a name (set via `/name`, or a resumed session)
//! is left alone (#553), two ways:
//! the `SetSessionMeta` this generator sends always carries `if_unset: true`,
//! so a late generated title can never win a race against — or clobber — a
//! name set (or already set before this process started) any other way; and on
//! `Resume` the generator folds the replayed log's `SessionMetaChanged` history
//! per session id and seeds `titled` with every session that already has a
//! name, so an already-named resumed session skips the aux call on its next
//! prompt too, not just the write. Idempotent per session: a `HashSet` of
//! already-titled session ids guards against a late second `Prompt` (a
//! mid-turn follow-up) re-triggering.
//!
//! Behind the `provider` feature: the aux `Llm` is a provider-backed client, so
//! a lean-library build (no providers) compiles the generator out — there is no
//! client to call.

use std::collections::HashSet;
use std::time::Duration;

use entanglement_core::{
    content_text, Holly, InMsg, LlmEvent, LlmRequest, Message, OutEvent, SessionId,
};
use futures::StreamExt;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use crate::aux_llm::AuxLlmRegistry;
use crate::config::aux_models::Purpose;

/// The per-model concurrency cap, at or below which firing the aux call
/// alongside the main turn is treated as guaranteed contention rather than
/// merely possible (#589): at a cap of 1 the main turn's own request already
/// holds the endpoint/model's only permit, so the aux call is certain to
/// block behind it, not just likely to. A cap above this is left to admit
/// concurrently as before — with more than one permit, the aux call has a
/// real chance to slot in for free.
const CONTENDED_CONCURRENCY_CEILING: usize = 1;

/// Upper bound on how long a deferred aux call waits for the main turn to
/// settle before firing anyway (#589). A safety net, not the expected case: a
/// turn that's merely slow still settles well inside this; one that never
/// settles (parked on approval, `Stop`, an engine restart) must not strand the
/// title forever — best-effort still means *eventually* best-effort.
const DEFER_TIMEOUT: Duration = Duration::from_secs(300);

/// The system prompt for the title generator: short, opinionated, asks for a
/// terse label the sidebar can render. A title is display-only metadata, so the
/// model's output never reaches the turn loop or tool execution.
const TITLE_SYSTEM: &str = "\
You generate a short, descriptive title for a coding-agent session from its \
first user prompt. Reply with ONLY the title: 3-8 words, no quotes, no \
trailing punctuation, no preface. A noun phrase summarizing the task, not a \
sentence. Example prompts → titles:\n\
- \"fix the login redirect bug\" → \"Fix login redirect bug\"\n\
- \"add a /compact command to the tui\" → \"Add /compact TUI command\"\n\
- \"why is my build slow\" → \"Investigate slow build\"";

/// Cap on the first-prompt text fed to the title model: a long prompt's tail
/// rarely changes the gist, and capping keeps the one-shot cheap (the whole
/// point of an aux model). ~2000 chars ≈ 500 tokens, well under any model's
/// window.
const TITLE_PROMPT_CHAR_CAP: usize = 2000;

/// Cap on the generated title length: a model that ignores the system prompt
/// and rambles gets truncated rather than flooding the sidebar. 80 chars ≈ a
/// generous one-line title.
const TITLE_OUTPUT_CHAR_CAP: usize = 80;

/// Spawns the session-title generator (Issue 5). Subscribes to the engine's
/// inbound `InMsg` fan-out and, on the first `Prompt` of an unnamed session,
/// spawns a detached task that calls the aux `session_title` LLM and sends the
/// result back as `SetSessionMeta`. The outer task is aborted at shutdown
/// alongside the other runtime responders (`main.rs`); each inner per-prompt
/// task is tracked in a `JoinSet` rather than a bare detached `tokio::spawn`
/// (#545) — one still holding its own `Holly` clone while parked on a
/// throttled/slow aux call would otherwise keep the engine's channels open
/// indefinitely, since aborting the *outer* task never reaches a child spawned
/// from inside it. Parking it here means it's aborted along with the outer
/// task when this future's local variables drop.
#[cfg(feature = "provider")]
pub fn spawn_session_title_generator(
    holly: &Holly,
    registry: AuxLlmRegistry,
) -> tokio::task::JoinHandle<()> {
    let holly = holly.clone();
    tokio::spawn(async move {
        let mut inbound = holly.subscribe_inbound();
        // Sessions this generator has already titled (or decided to skip). A
        // late second `Prompt` for a titled session is a no-op.
        let mut titled: HashSet<SessionId> = HashSet::new();
        let mut inflight: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                // Reap finished per-prompt tasks so `inflight` doesn't grow
                // unbounded over a long-running process; guarded so this branch
                // never fires (and busy-loops) while the set is empty.
                Some(_) = inflight.join_next(), if !inflight.is_empty() => {}
                msg = inbound.recv() => {
                    match msg {
                        Ok(InMsg::Prompt { session, content }) => {
                            // Already titled/skipped → leave it alone.
                            if titled.contains(&session) {
                                continue;
                            }
                            let prompt_text = content_text(&content);
                            if prompt_text.trim().is_empty() {
                                continue;
                            }
                            titled.insert(session.clone());
                            let holly = holly.clone();
                            let registry = registry.clone();
                            // Guaranteed contention (#589): the aux call would
                            // certainly queue behind the main turn's own
                            // request for the model's one permit, so subscribe
                            // now — before the main turn's `Done` can possibly
                            // fire — and wait for it rather than racing.
                            let contended = registry
                                .concurrency_cap(Purpose::SessionTitle)
                                .is_some_and(|cap| cap <= CONTENDED_CONCURRENCY_CEILING);
                            let settle_wait = contended.then(|| holly.subscribe());
                            // Tracked (not bare-detached): the generator must not
                            // block the inbound fan-out (a slow aux model would
                            // stall every later `InMsg`), but the task must still
                            // be reachable for abort at shutdown. A failure is
                            // logged + dropped.
                            inflight.spawn(async move {
                                if let Some(mut outbound) = settle_wait {
                                    wait_for_turn_settled(&mut outbound, &session).await;
                                }
                                let title = match generate_title(&registry, &prompt_text).await {
                                    Ok(t) => t,
                                    Err(e) => {
                                        tracing::debug!(
                                            "session-title: generation failed for {}: {e:#}",
                                            session
                                        );
                                        return;
                                    }
                                };
                                if let Some(title) = title {
                                    let _ = holly
                                        .send(InMsg::SetSessionMeta {
                                            session,
                                            name: Some(title),
                                            action: None,
                                            if_unset: true,
                                        })
                                        .await;
                                }
                            });
                        }
                        // A resumed session's replay log carries its own
                        // history — including any earlier `SetSessionMeta`
                        // fold (`/name`, or a title this generator already
                        // set in a prior process). Seed `titled` from it so
                        // the first post-resume prompt of an already-named
                        // session skips the aux call entirely, rather than
                        // firing one this generator's own `if_unset` guard
                        // would just discard downstream (#553). The records
                        // may interleave a whole spawned sub-tree, so this
                        // folds per session id, last-write-wins, exactly like
                        // core's own `Session::replay`.
                        Ok(InMsg::Resume { records, .. }) => {
                            let mut last_name: std::collections::HashMap<
                                SessionId,
                                Option<String>,
                            > = std::collections::HashMap::new();
                            for (_, ev) in &records {
                                if let OutEvent::SessionMetaChanged { session, name, .. } = ev {
                                    last_name.insert(session.clone(), name.clone());
                                }
                            }
                            titled.extend(
                                last_name
                                    .into_iter()
                                    .filter(|(_, name)| name.is_some())
                                    .map(|(session, _)| session),
                            );
                        }
                        Ok(_) => {}
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => break,
                    }
                }
            }
        }
    })
}

/// Waits until `session`'s current turn settles (`Done` or `Error`) on
/// `outbound`, or [`DEFER_TIMEOUT`] elapses, whichever comes first (#589).
/// Best-effort like the rest of this module: a lagged receiver just keeps
/// waiting (a missed delta can't be the settle signal itself) and a closed
/// channel or an expired deadline both fall through to letting the caller
/// fire its aux call anyway rather than stranding it.
#[cfg(feature = "provider")]
async fn wait_for_turn_settled(outbound: &mut broadcast::Receiver<OutEvent>, session: &SessionId) {
    let deadline = tokio::time::sleep(DEFER_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return,
            ev = outbound.recv() => match ev {
                Ok(OutEvent::Done { session: s, .. } | OutEvent::Error { session: s, .. })
                    if s == *session =>
                {
                    return;
                }
                Ok(_) => continue,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            },
        }
    }
}

/// Ask the aux `session_title` LLM for a title for `first_prompt`. Returns
/// `Ok(None)` when the model returned no usable text (empty / only whitespace
/// after trimming); `Err` for a stream/transport failure. The prompt is capped
/// at [`TITLE_PROMPT_CHAR_CAP`] and the output at [`TITLE_OUTPUT_CHAR_CAP`].
#[cfg(feature = "provider")]
async fn generate_title(
    registry: &AuxLlmRegistry,
    first_prompt: &str,
) -> anyhow::Result<Option<String>> {
    let capped_prompt: String = first_prompt.chars().take(TITLE_PROMPT_CHAR_CAP).collect();
    let messages = [Message::user(&capped_prompt)];
    let req = LlmRequest {
        system: TITLE_SYSTEM,
        model: None,
        messages: &messages,
        tools: &[],
        generation: None,
    };
    let mut llm = registry.resolve(Purpose::SessionTitle);
    let mut stream = llm.stream(req).await?;
    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(LlmEvent::Text(delta)) => text.push_str(&delta),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
    let title = clean_title(&text);
    Ok((!title.is_empty()).then_some(title))
}

/// Normalize a raw model reply into a sidebar title: collapse inner whitespace
/// to single spaces, strip surrounding quotes/leading labels ("Title: …"),
/// drop trailing punctuation, cap at [`TITLE_OUTPUT_CHAR_CAP`] chars on a word
/// boundary. An empty/whitespace-only result stays empty (the caller treats it
/// as "no title").
fn clean_title(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    // Strip a leading "Title:" label a model might add despite the system prompt.
    for prefix in ["Title:", "title:"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.trim_start().to_string();
            break;
        }
    }
    // Collapse internal whitespace (a streaming model may emit newlines).
    s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    // Drop a trailing sentence terminator; keep internal punctuation.
    while let Some(stripped) = s.strip_suffix(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!')) {
        s = stripped.trim_end().to_string();
    }
    // Strip surrounding quotes if the whole title is wrapped in them. The
    // curly-quote pair is multi-byte, so strip from the front + back by char
    // rather than by byte index.
    let stripped = |s: &str| -> Option<String> {
        let (open, close) = (s.chars().next()?, s.chars().last()?);
        let matched =
            (open == close && (open == '"' || open == '\'')) || (open == '“' && close == '”');
        if matched && s.chars().count() >= 2 {
            let inner: String = s.chars().skip(1).take(s.chars().count() - 2).collect();
            Some(inner.trim().to_string())
        } else {
            None
        }
    };
    if let Some(inner) = stripped(&s) {
        s = inner;
    }
    // Cap on a word boundary so the title doesn't end mid-word.
    if s.chars().count() > TITLE_OUTPUT_CHAR_CAP {
        let truncated: String = s.chars().take(TITLE_OUTPUT_CHAR_CAP).collect();
        s = match truncated.rfind(' ') {
            Some(idx) => truncated[..idx].trim_end().to_string(),
            None => truncated.trim_end().to_string(),
        };
    }
    s
}

#[cfg(all(test, feature = "provider"))]
mod tests {
    use super::*;

    #[test]
    fn clean_title_normalizes_common_model_quirks() {
        // Strips a "Title:" label.
        assert_eq!(clean_title("Title: Fix the bug"), "Fix the bug");
        assert_eq!(clean_title("title: Add a command"), "Add a command");
        // Collapses internal whitespace + newlines.
        assert_eq!(clean_title("Fix\nthe   \n  bug"), "Fix the bug");
        // Drops trailing punctuation.
        assert_eq!(clean_title("Fix the bug."), "Fix the bug");
        assert_eq!(clean_title("Fix the bug!"), "Fix the bug");
        // Strips surrounding quotes.
        assert_eq!(clean_title("\"Fix the bug\""), "Fix the bug");
        assert_eq!(clean_title("“Fix the bug”"), "Fix the bug");
        // Empty / whitespace-only → empty.
        assert_eq!(clean_title("   "), "");
        assert_eq!(clean_title(""), "");
    }

    #[test]
    fn clean_title_caps_on_a_word_boundary() {
        let long = "This is an excessively long title that goes well past the output cap and must be truncated";
        let cleaned = clean_title(long);
        assert!(cleaned.chars().count() <= TITLE_OUTPUT_CHAR_CAP);
        assert!(!cleaned.ends_with(' '), "no trailing space after cap");
        // The cap lands on a word boundary, not mid-word.
        assert!(long.starts_with(&cleaned));
    }
}
