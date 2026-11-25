# 0128. MCP HTTP `${VAR}` header expansion is a documented, consented leak surface

- Status: Accepted
- Date: 2026-07-22
- Amends [0080](0080-mcp-streamable-http-transport.md); doc gap filed by the
  2026-07-21 audit ledger row 6 (#396, #478).

## Context

[ADR-0080](0080-mcp-streamable-http-transport.md) describes `${VAR}` expansion
in a streamable-HTTP MCP server's static `headers` as a safety feature — "a
token is never written into the config file in the clear" — but never states
the flip side: `expand_env()` (`entanglement-runtime/src/mcp/http.rs`) resolves
`${VAR}` from the engine's **whole process environment**, with no allowlist. A
config that names a provider secret in a header —
`Authorization: Bearer ${ZAI_API_KEY}` — sends that key's live value to
whatever server the `url:` points at, on every request. Unlike the sibling
stdio leak ADR-0124 fixed (a subprocess inheriting the full env by accident),
this is not a bug: the header's whole purpose is to carry a secret value into
an outbound request. But the *scope* of what env vars are reachable this way
was never written down, and nothing warns a future contributor that logging
resolved headers (e.g. debugging a handshake failure) would leak the same
secrets `ENTANGLEMENT_LOG_BODIES` and the #164/#472 scrubs were built to keep
out of logs.

## Decision

Record the leak surface as accepted, not deferred:

- **Any process env var is reachable.** `expand_env` has no allowlist —
  `${ANYTHING}` resolves against `std::env::var`, not just the provider
  `key_env`s the catalog knows about. A config author who writes
  `${ZAI_API_KEY}` (or any other secret-shaped var already in the process env)
  into a server's `headers` sends its value to that server.
- **This is consent, not a gap.** Per [0047](0047-local-trust-boundary.md) and
  ADR-0080's own "Trust" section, the config file is trusted and enabling a
  server is explicit consent — naming a var in a header is the operator
  deliberately routing that secret to that server, the same posture as the
  stdio transport's `env:` block ADR-0124 confirmed "still wins" over the key
  scrub. No code change follows from this ADR.
- **Constraint on future logging:** any future logging of resolved MCP HTTP
  request headers (debugging the handshake, request tracing) **must redact
  expanded values** — log the header *name*, never the post-`expand_env`
  value. Today no such logging exists (`build_headers`/`expand_env` are
  called, never traced), so there is nothing to fix; this is a guardrail for
  the next contributor who adds one, not a currently-open hole.

## Consequences

- Positive: the leak surface and its consent basis are now written down
  somewhere a reviewer can find before adding header logging or extending
  `expand_env`'s reach (e.g. reading from a secrets file instead of the
  process env — out of scope here).
- Neutral: no behavior changes. `expand_env`/`build_headers` are unmodified.
- Negative: the surface remains real — a misconfigured `headers:` block (a
  copy-pasted `${VAR}` naming the wrong secret) still sends that secret to a
  remote server with no config-time validation catching it. Accepted per the
  same trust model as every other config-level secret handling in this
  project.

## Rejected alternatives

- **Allowlist `expand_env` to only the catalog's known `key_env`s.** Would
  block a legitimate use case (a custom header value pulled from an unrelated
  env var the operator manages themselves) for a surface that is already
  gated by "the config file is trusted." The stdio transport's `env:` block
  has the identical unrestricted reach, and ADR-0124 kept that intentional
  too.
- **Redact-by-default logging with an opt-in unredacted flag.** No header
  logging exists yet, so there is nothing to gate — the constraint is
  captured here for whoever adds it later instead of speculatively building
  the flag now.
