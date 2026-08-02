# 0156. Normalize and stabilize the endpoint pool key

- Status: Accepted
- Date: 2026-08-02
- Amends: [ADR-0050](0050-per-endpoint-connection-pool-retry-rate-limit.md) (both
  "Negative / neutral" consequences it accepted — endpoint-spelling splits and
  `DefaultHasher` instability — are fixed here, not merely documented),
  [ADR-0144](0144-file-backed-shared-endpoint-state-across-instances.md) (the
  shared-state file name derives from `pool_key`, so its stability now actually
  holds across processes, not just in principle)

## Context

Issue #551 found four related defects in how `client::pool_key(endpoint,
api_key)` — the identity for RPM/concurrency budgets and, since ADR-0144, for
the cross-process shared-state **file name** — is computed and used:

1. **Trailing slash / host spelling split the pool.** ADR-0050 explicitly
   accepted this as a "Negative / neutral" consequence: "two spellings of the
   same endpoint (trailing slash, host casing) would get separate budgets."
   The OpenAI-compat client trims its own request URL
   (`self.base_url.trim_end_matches('/')`) but passed the raw, untrimmed field
   into `execute_with_retry` for the pool key — so `ZAI_API_BASE=…/v4/` and the
   catalog's `…/v4` produced two `EndpointState`s, two RPM budgets, and (after
   ADR-0144) two shared-state files for one real endpoint, each enforcing only
   half the configured rate.
2. **`/key` mid-process rotation left the old bucket orphaned.** Priming
   `std::env` for a new key does nothing for a session already bound to the
   old one; nothing ever evicted the old key's `EndpointState` or its
   `.state`/`.lock` files once every session using it disconnected.
3. **Anthropic ignored `base_url` entirely.** `ANTHROPIC_API_URL` was a
   hard-coded constant; a catalog `wire: anthropic` entry with a `base_url`
   (a proxy/gateway) was silently sent to `api.anthropic.com` instead, and
   pool-keyed as if it were the real Anthropic endpoint.
4. **The pool key's hash was documented unspecified, now load-bearing.**
   ADR-0050 also accepted this: "the hash is process-local (bucket
   partitioning only), so cross-run stability is irrelevant" — true until
   ADR-0144 made the pool key double as a cross-process file name. `pool_key`
   still used `DefaultHasher`, whose output is explicitly unspecified across
   Rust versions/toolchains: two `skutter` binaries built differently could
   compute different shared-state file names for the identical endpoint and
   silently stop coordinating — exactly the N×-overshoot ADR-0144 exists to
   prevent.

Also: nothing ever pruned `${data_dir}/entanglement/endpoints/`, so orphaned
`.state`/`.lock` pairs (item 2, and any catalog `base_url` edit) accumulated
indefinitely; and `entanglement-provider/tests/streaming.rs` drove real
`execute_with_retry` calls against a local mock server without disabling
cross-process sharing, writing real state files into the developer's actual
data directory on every test run.

## Decision

**Normalize inside `pool_key` itself**, not in each wire client. A new
`normalize_endpoint` trims trailing `/` and lowercases the host (path segment
left case-sensitive, per RFC 3986) before the API-key hash is appended — every
caller (`openai`, `gemini`, `anthropic`) benefits with no per-client change,
and a caller no longer needs to independently agree to pre-trim its own URL.

**Replace `DefaultHasher` with `sha2::Sha256`** for the API-key hash component
— `sha2` was already a dependency (`shared_store::hash_key` uses it for the
same file-naming reason). The pool key is now stable across processes and
Rust toolchains, matching what `multi_user::provider`'s doc comment already
(and, before this change, inaccurately) claimed.

**Thread a `base_url` through `AnthropicLlm`**, mirroring
`OpenAiLlm`/`GeminiLlm`: a new `ANTHROPIC_BASE` constant (no path — `/v1/messages`
is appended per request, same shape as `OPENAI_BASE`/`GEMINI_BASE`) is the
default; `anthropic_factory_for` (single-user) and `resolve_for_user`
(multi-user) resolve it the same way the other two wires do (env
`{NAME}_API_BASE`/`{NAME}_BASE` > `entry.base_url` > client default).

**Sweep orphaned shared-state files.** `client::prune_stale(max_idle)` walks
the state directory and removes any `.state`/`.lock` pair that is both idle
(untouched for `max_idle`) and empty (no live lease, no pending cool-down, no
request in the trailing RPM window) — an endpoint still in real use always
fails the "empty" check regardless of file age, so this can never evict a live
budget. Wired in two places: a best-effort startup sweep in `main.rs`
(`max_idle = 1h`, mirroring `session_store::prune`'s startup-prune role for
session logs), and a short-`max_idle` (5s) sweep fired from `/key`'s submit
handler (`tokio::task::spawn_blocking`, off the keypress path — the sweep
locks and `fsync`s files) so a deliberate key rotation doesn't wait a full
hour to reclaim its old bucket's file once every session has moved off it.
This does not force an already-bound session to rebind — that remains an
explicit `/model` switch (ADR-0063) — it only stops the *file* from lingering
forever once nothing references it.

**Isolate integration tests.** `streaming.rs` gains a `Once`-guarded
`ENTANGLEMENT_NO_SHARED_ENDPOINT_STATE=1` set before the first `HttpClient` is
constructed, via `test_http_client()`/`test_http_client_with()` wrappers
replacing every direct `HttpClient::new`/`with_config` call in the file.

## Consequences

### Positive

- One real endpoint gets one budget regardless of trailing-slash/casing
  spelling differences across env vars and the catalog.
- The shared-state file name is now genuinely stable cross-process/toolchain,
  closing the exact coordination gap ADR-0144 was written to prevent.
- A proxy/gateway catalog entry on `wire: anthropic` is finally honored end to
  end (request URL *and* pool identity).
- The state directory no longer grows without bound across the lifetime of a
  long-running installation.
- `cargo test -p entanglement-provider` no longer touches the developer's real
  `${data_dir}`.

### Negative / neutral

- `pool_key`'s output string changed shape (raw endpoint → normalized
  endpoint; `DefaultHasher` hex → SHA-256 hex) — an existing on-disk
  shared-state file from before this change becomes orphaned under its old
  name rather than being "migrated"; `prune_stale` reclaims it once it goes
  idle. No code path depends on the old pool-key string's exact bytes, so
  nothing breaks, only a one-time silent rename-in-effect.
- `AnthropicLlm::new`/`anthropic_factory` gained a leading `base_url`
  parameter — a breaking signature change for both crates' public API. Both
  in-tree call sites (`main.rs`, `multi_user/provider.rs`) were updated in the
  same change; an out-of-tree embedder would need the same one-line update.
- `/key`'s rotation sweep is best-effort and fire-and-forget: it does not
  block the dialog closing, does not surface failures to the user, and (by
  design) skips anything not yet idle for 5s — a session still draining a
  response on the old key is correctly left alone.

## Alternatives considered

- **Normalize in each wire client instead of centrally in `pool_key`.**
  Rejected: this is exactly the bug — `openai`/`gemini` already trimmed their
  own request URL but not the value handed to the pool, and a third client
  (Anthropic) had neither. Normalizing once, centrally, removes the
  opportunity for a client to forget.
- **Force every live session to rebind on `/key` rotation.** Rejected as
  out of scope here: that would mean threading a live key-change notification
  through every session's bound `Llm` client, a materially larger change than
  this issue's actual complaint (the abandoned *file*, not the still-correct
  in-memory isolation between old and new sessions — two buckets during a
  rotation is expected and harmless, right up until the old one should be
  cleaned up).
- **Evict the in-process `EndpointState` map entry on rotation too.**
  Rejected: the map is keyed by every distinct pool key ever seen in-process,
  bounded by the number of distinct provider/key/base combinations actually
  used — not the unbounded, ever-growing artifact the on-disk file is. No
  observed leak to fix there (ADR-0050 already accepted this as unbounded-but-
  tiny).
- **A background periodic prune task instead of startup + rotation
  triggers.** Rejected for v1: a long-running `skutter serve` process could in
  principle accumulate orphans between restarts, but adding a recurring
  background task is more machinery than the issue's evidence (accumulation
  "on every key rotation / base change") calls for; a future issue can add a
  timer if that proves insufficient in practice.

## References

- Issue #551: endpoint pool-key inconsistencies (trailing slash, `/key`
  duplication, Anthropic `base_url`, unstable shared-state hash)
- [ADR-0050](0050-per-endpoint-connection-pool-retry-rate-limit.md): the
  per-endpoint pool this amends
- [ADR-0144](0144-file-backed-shared-endpoint-state-across-instances.md): the
  cross-process shared state whose file name depends on pool-key stability
- [ADR-0147](0147-multi-user-mode-embedder-api.md): documents the pool as
  keyed by `(base_url, sha256(api_key))` — now actually true, not aspirational
