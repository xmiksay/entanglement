# LLM I/O §5b — Extended thinking, provider selection & multi-user

> Part of the [llm i/o module](../provider.md) of the [architecture overview](../../architecture.md).

### Extended thinking: capture and replay

With thinking enabled, Anthropic requires the unmodified `thinking` /
`redacted_thinking` block — signature intact — on the **final** assistant
message whenever tool results are returned, which is exactly a parked turn
([ADR-0061](../../adr/0061-parked-turn-state-batch-tool-resolution.md)). So
reasoning cannot be display-only on that wire
([ADR-0160](../../adr/0160-extended-thinking-round-trip.md)).

`ContentPart::Reasoning { provider, text, data }` carries it in history: `data`
is the provider's own wire shape (Anthropic's `signature`, and whether the block
was redacted, live inside it), `text` is the human-readable rendering and may be
empty — current models omit thinking text by default while still signing the
block, and such a block still has to replay.

**Capture is unconditional. Replay is gated by `ModelEntry::replay_thinking`**
(`Option<bool>`; `None` derives from the wire — Anthropic on whenever thinking
is enabled, others off — and an explicit value always wins). The flag never
affects capture, persistence, or rendering, so toggling it cannot rewrite a
session log.

| Wire | Capture | Replay when enabled |
| --- | --- | --- |
| Anthropic | `thinking` assembled across `thinking_delta` + `signature_delta`; `redacted_thinking` whole | verbatim, **first** in the block list, on **every** assistant message ([ADR-0202](../../adr/0202-prompt-cache-discipline-anchors-deferral-replay-compaction-date.md): preserved-thinking models keep prior-turn thinking, so stripping it edits history — a cache bust per round, a 400 on Fable 5.1; older models ignore the blocks unbilled) |
| Gemini | thought-text parts | none — the load-bearing `thoughtSignature` round-trips via `ToolCall::provider_meta` (ADR-0085) |
| OpenAI-compat | structured `reasoning`/`reasoning_content` deltas (`thinking_format: fields`, the default) are accumulated per round and captured into one block at stream finish (ADR-0200), same as a `thinking_format: inline_tags` model's `<think>…</think>` spans split out of `content` (ADR-0191) | as the assistant message's `reasoning_content` field, gated by `replay_thinking` (default **off** on this wire); legacy spans already in history text are stripped regardless |

**Inline think-tags (`ModelEntry::thinking_format`, ADR-0191).** The one wire
where thinking can arrive *as assistant speech*: a parser-less OpenAI-compat
server inlines a reasoning model's thinking in `delta.content` between
`<think>`/`</think>` tags. Declaring `thinking_format: inline_tags` on the
model's catalog entry routes those spans to the reasoning rail at capture (a
streaming state machine in `openai/think.rs` that survives tags straddling
network chunks), captures one `ContentPart::Reasoning { provider: "openai" }`
block for the round, and strips any `<think>` spans already sitting in history
text before they are sent back — so a session written before the flag exists
is fixed retroactively, without rewriting its log. Replay of the captured
block is the same `replay_thinking` knob (off by default on this wire —
nothing is sent back, the reported qwen3.5 failure mode); a block minted by
another provider drops rather than degrading to text. The format+replay pair
resolves per request (`Catalog::thinking_spec_resolver`, the #550 property)
so a `model:`-only pin under a different model id behaves correctly.

**Fields-format capture (ADR-0200).** The other, more common OpenAI-compat
shape — structured `reasoning`/`reasoning_content` deltas, `thinking_format:
fields` (the catalog default) — used to be display-only: streamed as
`LlmEvent::Reasoning` for rendering but never captured, so a session's next
request replayed the assistant turn without the reasoning that server had
already generated and cached for that exact prefix. The client now
accumulates those deltas per round and mints one
`ContentPart::Reasoning { provider: "openai", data: {"format":"fields",...} }`
at stream finish — unconditional capture, same shape and same replay path as
the `inline_tags` block above. This is **not** a signal to enable
`replay_thinking` broadly: a qwen3.5/ornith-class model is typically trained
with prior-turn thinking stripped from its own history, so `replay_thinking:
false` (the default) is the *correct*, not merely conservative, posture for
that class of model — capture exists so the block is available at all, not so
replay becomes viable. A stripped-history model's client still sees its own
last assistant turn land one byte later than wherever the server's cache
entry actually ended (through generation, not just the replayed text) —
that residual tail divergence is a server-side snapshot-placement question
(snapshotting at end-of-prefill, before generation), out of this codebase's
scope; see [ADR-0200](../../adr/0200-fields-reasoning-capture-and-advertise-discovered.md).

Three rules hold regardless of the flag: a block whose `provider` differs from
the target renders **nothing** (stricter than `ProviderSearch`'s summary
fallback — reasoning is not answer content); `ContentPart::as_text` returns
`None` so reasoning stays out of `content_text`, compaction, and token
estimation; and an unsigned thinking block is display-only, since Anthropic
rejects one. Capturing the block also closes the older `pause_turn`
mid-thinking-block gap — `assembled_blocks` now includes thinking.

**Ollama `max_output_tokens` catalog default (#483):** the embedded `ollama`
entries set `max_output_tokens` explicitly (8192/2048/4096 for
`llama3.1`/`llama3`/`mistral`) rather than leaving it unset like every other
built-in entry. Unset, `OpenAiLlm` sends no `max_tokens`, so Ollama falls back
to its own `num_predict` default — 128 tokens — which was the leading cause of
the ADR-0118 "announced intent then stream died" ambiguous-stop symptom on
local models. Local generation has no per-token cost, so the values are
generous, just kept under each model's `context_window`; a `providers.yml`
override wins as usual.

**Provider selection (`skutter`):** the catalog loads once at startup; a
malformed user file is a loud error, never a silent fallback — and so is an
explicit `ENTANGLEMENT_PROVIDERS_FILE` that points at a missing file (a mistyped
override no longer silently yields the embedded defaults, #204; the *default*
`${config_dir}` path being absent stays the normal "no user override" case).
`ENTANGLEMENT_PROVIDER=<name>`
looks `<name>` up **in the catalog** (so custom providers work; `echo` stays a
built-in stub), erroring loudly if its key env is missing; if unset, auto-detect
by iterating catalog order and picking the first provider whose `key_env` is set
and non-empty (keyless Ollama is skipped) — preserving z.ai → OpenAI → Anthropic.
Nothing found is **never** a silent `EchoLlm` fallback
([ADR-0222](../../adr/0222-no-silent-echo-first-run-onboarding-and-init.md)):
`run`/`pipe`/`serve` print `provider_setup::no_provider_hint` (the catalog key
vars, `skutter config set-key <keyed providers>`, `ENTANGLEMENT_PROVIDER=ollama`
/ `=echo`) and exit 2, and the TUI runs its pre-boot onboarding dialog
(`tui::onboarding`) so `select_provider` never reaches that branch from it.
`echo` is explicit-only. Precedence overall is **env > user YAML > embedded defaults**.

The `EchoLlm` stub echoes a one-line summary of the request it received —
message count, user-text snippets, the assembled system prompt (`system_len` +
an 8-hex `system_sha` SHA-256 fingerprint) and the advertised `tools=[names]` —
so `ENTANGLEMENT_PROVIDER=echo skutter run` doubles as a prompt-assembly smoke
test (which prompt/tool set actually reached the backend). Set
`ENTANGLEMENT_DEBUG=echo` to append the full system text.
Per-provider env still wins: `<PROV>_API_KEY` (name from the entry's `key_env`),
`<PROV>_MODEL`, `<PROV>_API_BASE` (the legacy `<PROV>_BASE` spelling is
retired, ADR-0213 — a still-set one only earns a startup warning). Default models come from each
provider's `default_model` (`glm-5.3` / `gpt-4o` / `llama3.1` /
`claude-sonnet-5`). The TUI model picker + context bar read the same catalog.

**Multi-user provider context (#522, [ADR-0147](../../adr/0147-multi-user-mode-embedder-api.md)):**
everything in [provider](../provider.md) up to here is the **single-user** story — one process-global `Catalog`,
API keys loaded from the managed `.env` file (#220) into `std::env`. A
multi-user embedder instead builds `ModelResolver`s per user via
**`entanglement-provider::multi_user`** ([ADR-0184](../../adr/0184-provider-hosted-multi-user-seams.md)
relocated it out of the runtime — the runtime crate never names `UserId`,
ADR-0181, enforced by `make userid`):
`ModelResolver` itself widened to `Fn(Option<&UserId>, &str, &str) -> Result<ResolvedModel, String>`
so its call sites in `entanglement-core/src/session/` (session-start
pin and resume re-announce, replay's `ModelChanged` fold, `SetModel`; the
former `SetAgent` pin rebind is gone, ADR-0207 §9) can resolve against the *resolving
session's own user* — single-user callers (`factories.rs::build_model_resolver`)
simply ignore the parameter. `build_user_model_resolver` looks the session's
`UserId` up in an embedder-supplied `UserProviderStore`, resolving each
`UserProviderContext`'s own `Catalog` (same shape as `providers.yml` — so
**per-user RPM/concurrency budgets are just per-user catalog data**, no new
plumbing) and API keys (an in-memory map, **never** written to `std::env`).
The shared `HttpClient` connection pool still isolates rate-limit state per
`(base_url, sha256(api_key))` (ADR-0050), so two users with distinct keys on
the same provider already get independent `EndpointState`s with no further
change. `serve` is unaffected — it stays single-user (ADR-0048); this seam is
reachable only through the embedder library API. Per-user **aux-model** pins
take no runtime module at all (#635,
[ADR-0183](../../adr/0183-narrate-purpose-and-per-user-aux-pins.md), conforming
to [ADR-0181](../../adr/0181-userid-leaves-the-runtime-crate.md)): the embedder
builds a per-user `AuxLlmRegistry` from `AuxModelStore::in_memory` + a
resolver closure bound to its user — see the [heads & persistence
doc](../heads/keys-and-aux-models.md)'s §6d.

**Per-user admission gate on a shared literal key** (#632,
[ADR-0175](../../adr/0175-per-user-admission-gate-on-a-shared-literal-key.md)):
two users configured with the *same* literal key land in one `EndpointState`
(ADR-0050's pool key is blind to caller identity), so
`resolve_for_user` additionally calls `HttpClient::with_user_budget`,
returning a client clone carrying that user's own catalog `rpm`/`concurrency`
as a `UserBudget` before building their `Llm` factory — the endpoint pool
itself (`Arc<EndpointPool>`) stays shared. `EndpointState` gains a third
lazily-created gate, `user_budgets: Mutex<HashMap<UserId, Arc<UserSlot>>>`,
mirroring `model_concurrency`/`ModelSlot` (ADR-0140) exactly — including the
same self-correcting behavior on a changed budget — except a `UserSlot`'s
pacing gate is a fixed-rate limiter, **not** AIMD-adaptive: a 429 is a
property of the whole endpoint, not one user's slice of it, so
`penalize`/`relax` stay endpoint-wide only. `execute_with_retry` acquires
**user, then model, then endpoint, then the cross-process shared lease**
(`StreamGuard` widened to hold all four) — the same "narrowest gate first"
reasoning ADR-0140 established for model-before-endpoint, extended one level
further. A handle with no attached budget (every existing single-user caller)
admits exactly as before this ADR. Per-user budgets are **in-process only** —
not mirrored to the cross-process shared-state file (ADR-0144) — an accepted
v1 gap for a still-single-process embedder API.

---

Previous: [§5b — Provider/model catalog & advanced wire facts](catalog.md)
