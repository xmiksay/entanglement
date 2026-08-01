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
//! dropped (the session keeps its default name). A session that already has a
//! name (set via `/name`, or a resumed session) is left alone — the generator
//! only titles the *first* prompt of an unnamed session. Idempotent per session:
//! a `HashSet` of already-titled session ids guards against a late second
//! `Prompt` (a mid-turn follow-up) re-triggering.
//!
//! Behind the `provider` feature: the aux `Llm` is a provider-backed client, so
//! a lean-library build (no providers) compiles the generator out — there is no
//! client to call.

use std::collections::HashSet;

use entanglement_core::{content_text, Holly, InMsg, LlmEvent, LlmRequest, Message, SessionId};
use futures::StreamExt;
use tokio::sync::broadcast::error::RecvError;

use crate::aux_llm::AuxLlmRegistry;
use crate::config::aux_models::Purpose;

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
/// result back as `SetSessionMeta`. Aborted at shutdown alongside the other
/// runtime responders (`main.rs`) — it holds only a `Holly` clone + the
/// registry (both cheap, neither needs a graceful drain).
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
        loop {
            match inbound.recv().await {
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
                    // Detached: the generator must not block the inbound
                    // fan-out (a slow aux model would stall every later
                    // `InMsg`). A failure is logged + dropped.
                    tokio::spawn(async move {
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
                                })
                                .await;
                        }
                    });
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    })
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
