//! LLM-summarization core shared by the manual `"compact"` oneshot op
//! (`session/ops.rs`, copy-on-write, ADR-0101) and automatic in-place
//! auto-summarize on context overflow (`session/turn.rs`, #398, ADR-0103).
//! Both callers split the history into a summarized head and a verbatim tail
//! (clamped to a safe turn boundary, #397/ADR-0102), guard both against the
//! session's budget, and ask the model for a dense summary of the head — they
//! differ only in what happens to the result (a report event vs. an in-place
//! `Context` mutation). Which shape carries the head — the session's own
//! cached prefix, or a rendered transcript — is decided here and built by
//! `session/compaction_request.rs` (ADR-0202).

use super::compaction_request::{CompactionRequest, SessionPrefix};
use super::summary_attempt::{self, Head, Knobs};
use super::transcript::render_transcript;
use crate::context::{estimate_text_tokens, Context};
use entanglement_provider::{GenerationParams, Llm, RetryConfig, StopReason, Usage};

/// The [`EngineConfig::aux_llm_resolver`][crate::EngineConfig::aux_llm_resolver]
/// purpose key for compaction (Issue 5). Core knows only this string; the
/// runtime maps it onto its own `Purpose` enum and the managed
/// `aux-models.yml` pin.
pub const AUX_PURPOSE_SUMMARIZE: &str = "summarize";

/// The backend one compaction call runs against: either a purpose-pinned aux
/// model built from [`AUX_PURPOSE_SUMMARIZE`], or the session's own.
///
/// Owning the built `Box<dyn Llm>` here is what lets both call sites hand
/// [`summarize`] a `&mut dyn Llm` that outlives the borrow — the aux client is
/// one-shot and dropped when this value goes out of scope.
pub(crate) struct AuxBackend {
    llm: Option<Box<dyn Llm>>,
    model: Option<String>,
    generation: Option<GenerationParams>,
    /// `Some(RetryConfig::minimal())` when [`Self::llm`] is a genuinely
    /// pinned aux backend, `None` on the session-fallback path (aux
    /// fail-fast, #560 follow-up). A dead `summarize` pin must fail its probe
    /// fast rather than retry-storm like `narrate`/`session_title` — but the
    /// *fallback* case reuses the session's own primary `Llm`, the same
    /// handle the main turn loop calls, so it must keep the ordinary
    /// LLM-tuned retry ladder: weakening it here would regress a legitimate
    /// transient-failure retry on every compaction, pinned or not.
    retry: Option<RetryConfig>,
}

/// Which arm [`AuxBackend::resolve`] took. It decides the request shape
/// (ADR-0202): only the session's own backend shares the session's prompt
/// cache and accepts its history's thinking blocks and tool-call ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackendArm {
    Session,
    PinnedAux,
}

/// A resolved compaction backend, borrowed for one [`summarize`] call.
pub(crate) struct SummarizeBackend<'a> {
    pub llm: &'a mut dyn Llm,
    pub model: Option<&'a str>,
    pub generation: Option<GenerationParams>,
    pub retry: Option<RetryConfig>,
    pub arm: BackendArm,
}

impl AuxBackend {
    /// Resolve the `summarize` purpose against the engine's aux resolver.
    /// A missing resolver, an unset pin, or a pin the catalog no longer knows
    /// all yield the "use the session's own backend" form — byte-identical to
    /// the pre-Issue-5 behavior.
    pub(crate) fn for_summarize(cfg: &crate::EngineConfig) -> Self {
        match cfg
            .aux_llm_resolver
            .as_ref()
            .and_then(|r| r(AUX_PURPOSE_SUMMARIZE))
        {
            Some(resolved) => {
                tracing::debug!(
                    provider = %resolved.provider,
                    model = %resolved.model,
                    "compaction: using the pinned `summarize` aux model"
                );
                Self {
                    llm: Some((resolved.llm_factory)()),
                    model: Some(resolved.model),
                    generation: resolved.generation,
                    retry: Some(RetryConfig::minimal()),
                }
            }
            None => Self {
                llm: None,
                model: None,
                generation: None,
                retry: None,
            },
        }
    }

    /// The backend to summarize with, falling back to `session_*`
    /// field-by-field. The session fallback deliberately reads the *session's
    /// current* binding, so a live `/model` switch keeps applying to
    /// compaction when no aux pin is set.
    pub(crate) fn resolve<'a>(
        &'a mut self,
        session_llm: &'a mut dyn Llm,
        session_model: Option<&'a str>,
        session_generation: Option<GenerationParams>,
    ) -> SummarizeBackend<'a> {
        match &mut self.llm {
            Some(llm) => SummarizeBackend {
                llm: &mut **llm,
                model: self.model.as_deref(),
                generation: self.generation,
                retry: self.retry,
                arm: BackendArm::PinnedAux,
            },
            None => SummarizeBackend {
                llm: session_llm,
                model: session_model,
                generation: session_generation,
                retry: None,
                arm: BackendArm::Session,
            },
        }
    }
}

/// Why [`summarize`] couldn't produce (or accept) a summary. Every variant's
/// `Display` text matches what `compact_op` has always surfaced via
/// `OutEvent::Error`, so lifting the logic here changes no user-visible text.
pub(crate) enum SummarizeError {
    NoHistory,
    EntireHistoryKept {
        kept: usize,
    },
    TranscriptTooLarge {
        tokens: usize,
        limit: usize,
    },
    TailTooLarge {
        kept: usize,
        tokens: usize,
        limit: usize,
    },
    Truncated,
    Llm(anyhow::Error),
}

impl std::fmt::Display for SummarizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SummarizeError::NoHistory => {
                write!(f, "cannot compact: no conversation history")
            }
            SummarizeError::EntireHistoryKept { kept } => write!(
                f,
                "cannot compact: kept ({kept}) covers the entire conversation — \
                 nothing left to summarize; use a smaller --keep value"
            ),
            SummarizeError::TranscriptTooLarge { tokens, limit } => write!(
                f,
                "cannot compact: transcript (~{tokens} tokens) exceeds the \
                 {limit}-token context budget — start a new session or shorten \
                 the conversation"
            ),
            SummarizeError::TailTooLarge {
                kept,
                tokens,
                limit,
            } => write!(
                f,
                "cannot compact: the {kept} kept trailing messages (~{tokens} \
                 tokens) alone exceed the {limit}-token context budget — use a \
                 smaller --keep value"
            ),
            SummarizeError::Truncated => write!(
                f,
                "compaction failed: the summary was truncated (stop reason: \
                 max_tokens) — refusing to fork a cut-off summary; the \
                 original session is unchanged"
            ),
            SummarizeError::Llm(e) => write!(f, "{e}"),
        }
    }
}

/// A completed summarization: `summary` already has the verbatim `kept` tail
/// (#397/ADR-0102) rendered separately in `tail_rendered` — deliberately
/// *not* baked into `summary`, since the two callers preserve the tail two
/// different ways: every caller now forks a successor (ADR-0205) whose seed is
/// a single flat prompt, so each composes `summary` + `tail_rendered` itself
/// (`compose_report`). They stay separate here because the prune path seeds
/// from a rendered transcript with no summary at all, and baking the tail into
/// `summary` would leave that caller no way to tell the two apart.
pub(crate) struct SummarizeOutcome {
    pub summary: String,
    pub kept: usize,
    pub tail_rendered: Option<String>,
    pub finish: Option<(Option<StopReason>, Usage)>,
}

/// Compose `summary` and the rendered `tail` (if any) into one flat report —
/// what a copy-on-write fork's single seed prompt carries (#397/ADR-0102).
pub(crate) fn compose_report(summary: &str, kept: usize, tail_rendered: Option<&str>) -> String {
    match tail_rendered {
        Some(tail) => format!(
            "{summary}\n\n---\nThe following {kept} most recent messages are \
             preserved verbatim (not summarized):\n\n{tail}"
        ),
        None => summary.to_string(),
    }
}

/// Summarize `ctx`'s head on `backend`, preserving the tail (clamped to
/// `ctx.safe_kept(requested_kept)`) verbatim. `prefix` is what the session
/// sends ahead of its history this round (ADR-0202): the session's own backend
/// replays it with the head verbatim when that fits the real window, otherwise
/// — or on a pinned aux backend — the head goes out as a rendered transcript
/// guarded by the input budget. See the module doc for what differs after
/// this returns.
pub(crate) async fn summarize(
    ctx: &Context,
    backend: SummarizeBackend<'_>,
    prefix: SessionPrefix<'_>,
    requested_kept: usize,
    instructions: Option<&str>,
) -> Result<SummarizeOutcome, SummarizeError> {
    if ctx.messages().is_empty() {
        return Err(SummarizeError::NoHistory);
    }

    let kept = ctx.safe_kept(requested_kept);
    let split = ctx.messages().len() - kept;
    let (head, tail) = ctx.messages().split_at(split);

    if head.is_empty() {
        return Err(SummarizeError::EntireHistoryKept { kept });
    }

    let structured = match backend.arm {
        BackendArm::Session => CompactionRequest::structured(
            prefix,
            head,
            instructions,
            ctx.window(),
            backend.generation,
        ),
        BackendArm::PinnedAux => None,
    };
    let request = match structured {
        Some(request) => request,
        None => CompactionRequest::rendered(head, instructions, ctx.limit())?,
    };

    // The kept tail rides verbatim (unsummarized) into the compacted context,
    // so it must fit the input budget on its own too.
    let tail_transcript = (!tail.is_empty()).then(|| render_transcript(tail));
    if let Some(tail_transcript) = &tail_transcript {
        let tail_tokens = estimate_text_tokens(tail_transcript);
        if tail_tokens > ctx.limit() {
            return Err(SummarizeError::TailTooLarge {
                kept,
                tokens: tail_tokens,
                limit: ctx.limit(),
            });
        }
    }

    tracing::debug!(
        structured = request.is_structured(),
        arm = ?backend.arm,
        head = head.len(),
        kept,
        "compaction: summarizing"
    );
    let SummarizeBackend {
        llm,
        model,
        generation,
        retry,
        ..
    } = backend;
    let knobs = Knobs {
        model,
        generation,
        retry,
    };
    let head = Head {
        messages: head,
        instructions,
        limit: ctx.limit(),
    };
    let (summary, finish) = summary_attempt::run(llm, request, knobs, head).await?;

    // Refuse a truncated summary: a `max_tokens`-cut-off fragment must not
    // replace (or report as replacing) real history.
    if let Some((Some(StopReason::MaxTokens), _)) = &finish {
        return Err(SummarizeError::Truncated);
    }

    Ok(SummarizeOutcome {
        summary,
        kept,
        tail_rendered: tail_transcript,
        finish,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #560 aux fail-fast follow-up: a resolved `summarize` pin must carry
    /// `RetryConfig::minimal()` so a dead pinned endpoint fails its probe
    /// fast rather than retry-storming — `max_attempts` is the cheapest
    /// field to assert the right shape landed (`RetryConfig` has no
    /// `PartialEq`). The pinned arm is also what routes the rendered shape
    /// (ADR-0202).
    #[test]
    fn aux_backend_carries_minimal_retry_only_when_a_pin_resolves() {
        let cfg = crate::EngineConfig {
            aux_llm_resolver: Some(std::sync::Arc::new(|purpose: &str| {
                assert_eq!(purpose, AUX_PURPOSE_SUMMARIZE);
                Some(entanglement_provider::ResolvedModel {
                    provider: "aux".to_string(),
                    model: "aux-model".to_string(),
                    llm_factory: std::sync::Arc::new(|| {
                        Box::new(entanglement_provider::DummyLlm::default()) as Box<dyn Llm>
                    }),
                    generation: None,
                    context_window: None,
                })
            })),
            ..Default::default()
        };
        let mut aux = AuxBackend::for_summarize(&cfg);
        let mut session_llm = entanglement_provider::DummyLlm::default();
        let backend = aux.resolve(&mut session_llm, None, None);
        assert_eq!(backend.arm, BackendArm::PinnedAux);
        assert_eq!(backend.model, Some("aux-model"));
        let retry = backend
            .retry
            .expect("a resolved pin must carry the fail-fast retry override");
        assert_eq!(retry.max_attempts, 2);
    }

    /// #560 follow-up: falling back to the session's own backend (no pin, or
    /// an unresolvable one) must keep the ordinary LLM-tuned retry ladder —
    /// weakening it here would regress every legitimate transient-failure
    /// retry on a compaction that never touched a pin at all.
    #[test]
    fn aux_backend_carries_no_retry_override_on_session_fallback() {
        let cfg = crate::EngineConfig {
            aux_llm_resolver: Some(std::sync::Arc::new(|_purpose: &str| None)),
            ..Default::default()
        };
        let mut aux = AuxBackend::for_summarize(&cfg);
        let mut session_llm = entanglement_provider::DummyLlm::default();
        let backend = aux.resolve(&mut session_llm, Some("session-model"), None);
        assert_eq!(backend.arm, BackendArm::Session);
        assert_eq!(backend.model, Some("session-model"));
        assert!(
            backend.retry.is_none(),
            "the session-fallback path must not override the endpoint's own retry policy"
        );
    }

    #[test]
    fn compose_report_is_the_bare_summary_with_no_tail() {
        assert_eq!(
            compose_report("a dense summary", 0, None),
            "a dense summary"
        );
    }

    #[test]
    fn compose_report_appends_the_rendered_tail() {
        let report = compose_report("a dense summary", 2, Some("[user]\nsecond\n\n"));
        assert!(report.starts_with("a dense summary"));
        assert!(report.contains("2 most recent messages"));
        assert!(report.contains("[user]\nsecond"));
    }
}
