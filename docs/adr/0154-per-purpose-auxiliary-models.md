# 0154. Per-purpose auxiliary models for side transformations

- Status: Accepted — Amended by [0158]
- Date: 2026-08-01
- Issue: tui-ux-batch plan, Issue 5

## Context

One provider+model served everything: the turn loop *and* every side
transformation around it. A compaction summary and a session title are not
reasoning work — they are cheap, bounded, throwaway calls — but they burned the
same (often expensive, often rate-limited) model the agent reasons with.

The existing per-agent pins don't help. `agent-models.yml` (ADR-0081) pins a
model *per profile*, and `agent-generation.yml` (ADR-0094) pins generation knobs
per profile; both are about how a profile's **turn loop** runs. A side
transformation happens *within* whatever profile is active, so neither seam can
express "summarize with the cheap model, reason with the strong one."

Session compaction made this concrete and awkward. `/compact`
(`InMsg::Oneshot { op: "compact" }`) and the auto-summarize overflow path
(ADR-0103) both run **inside core**, driven by the session's own `&mut s.llm`.
The runtime holds the pin store; core holds the call site. Nothing connected
them.

## Decision

### A managed pin file keyed by purpose, not by profile

`${config_dir}/entanglement/aux-models.yml` (override
`ENTANGLEMENT_AUX_MODELS_FILE`) maps `purpose → { provider, model }` — a
sibling of `agent-models.yml`/`grants.yml`, same managed-file pattern (atomic
write, `fd-lock`, fail-open on a malformed file). Purposes are a closed enum:
`summarize` and `session_title`. Written by `/aux-model <purpose>
<provider>/<model>`; a bare `/aux-model` lists the current pins.

A runtime-owned `AuxLlmRegistry` resolves a purpose against the pin store using
**the same catalog `ModelResolver` closure `InMsg::SetModel` drives**, so an aux
client binds exactly as a fresh launch would and inherits the warm per-endpoint
pool (ADR-0050) rather than opening its own.

### Two consumers, two deliberately different fallbacks

This is the part worth recording, because the asymmetry looks like an
inconsistency and isn't.

- **The session-title generator** (runtime-side, on the first prompt of an
  unnamed session) has no session backend in hand. It calls
  `AuxLlmRegistry::resolve`, which falls back to the **primary model**.
- **Session compaction** runs inside core, which *does* have the session's
  backend. It reaches the pin through `AuxLlmRegistry::resolver`, where `None`
  means **use the session's own `llm`/`model`/`generation`**.

The second fallback is strictly better where it applies: a live `/model` switch
(ADR-0063) keeps applying to compaction. Falling back to a fixed primary there
would silently ignore the user's current model choice. The title generator
can't have that behavior because it has no session to fall back to.

### The core seam is a purpose *string*, not a registry

`EngineConfig::aux_llm_resolver: Option<AuxLlmResolver>` where

```rust
pub type AuxLlmResolver = Arc<dyn Fn(&str) -> Option<ResolvedModel> + Send + Sync>;
```

Shaped like `GenerationResolver` (ADR-0094) — purely local, a managed-file
lookup, so `Option` not `Result` — but carrying `ResolvedModel` so the caller
builds its one-shot client from the same `llm_factory` a `SetModel` switch
would.

Core knows only the string (`session::summarize::AUX_PURPOSE_SUMMARIZE ==
"summarize"`), never the `Purpose` enum, the pin file, or the catalog. The
runtime's closure maps the string onto its own typed enum; an unrecognized key
resolves to `None`, so a future core purpose an older runtime doesn't know is
inert rather than fatal.

`summarize::AuxBackend` owns the built `Box<dyn Llm>` at each call site so the
borrow outlives the `&mut dyn Llm` handed to `summarize`, and both the manual
and automatic compaction paths share it — an overflow recovery is a side
transformation too.

## Consequences

- A user can run summaries on `glm-4.7-flash` while reasoning on `glm-5.2`, or
  keep summaries local on Ollama while reasoning against a hosted model.
- Sessions get LLM-generated titles instead of a truncated first-prompt
  snippet. Best-effort and detached: a failure is logged and dropped, and
  `/name` (ADR-0151) always overrides.
- A second provider means a second endpoint pool/key — already supported, since
  ADR-0050 keys the pool by base URL + API-key hash.
- Core gains a public `EngineConfig` field. It is `Option` and defaults to
  `None`, so every existing embedder is unaffected and the unset path is
  byte-identical to pre-Issue-5 behavior.
- The pin is consulted per call (not cached at session start), so an
  `/aux-model` change applies to the next compaction with no restart.
- Rejected: threading an aux `Llm` through the protocol (core would have to
  carry a second backend per session for a call that happens rarely);
  intercepting compaction runtime-side (would mean duplicating the transcript
  render, the context-budget guard, and the keep-tail clamp outside core);
  extending `AgentProfile` with the pin (a side transformation is not a profile
  trait — the same profile wants the same cheap summarizer regardless).
- Not covered: `narrate` as a purpose (the plan floated it; rendering "what the
  agent is doing" is a stream concern, not an LLM call), and per-user aux pins
  under the multi-user embedder API (ADR-0147) — the store is process-global.
