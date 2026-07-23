# 0109. Access outside the project root via per-tool approval — containment becomes subordinate to an explicit grant

- Status: Amended by [0119](0119-rhai-bindings-route-through-the-escape-root-gate.md) (rhai bindings route through the same gate), [0120](0120-once-scoped-escape-root-grant-bound-to-request-id.md) (`Once` grants bound to the approving `request_id`), and [0132](0132-glob-grep-escape-root-search-via-durable-grant.md) (`glob`/`grep` ride an existing durable `read` grant to search outside root)
- Date: 2026-07-17

## Context

Root containment ([ADR-0054](0054-canonicalizing-symlink-safe-root-containment.md),
#163) makes a `read`/`edit`/`write` path — and a `bash`/`call` `workdir` — that
resolves outside the canonicalized project root a **hard error**
(`"path escapes working directory"`), enforced deep inside each tool's `run` by
`host::resolve_under_root`. That check is deliberately *independent of* the
permission ladder: it fires downstream of, and invisible to, the executor's
`Allow | Ask | Deny` decision. So there has been **no way** for the model to read
a file or write to a directory outside the repo, even when the user would happily
allow it — the escape is refused before any approval flow can run.

For a local, single-user tool ([ADR-0047](0047-local-trust-boundary.md)/
[ADR-0048](0048-serve-head-local-trust-model.md)) this is often too strict: a
legitimate task ("read `~/.config/foo`", "write into a sibling checkout") is
impossible without moving the file into the repo. The user asked for an
approval-gated escape hatch: accessing a path outside root should be *possible*,
but only after an explicit **allow-once / session / always** approval, granted
**per tool** (approving `read` on a path must not also let `write` touch it).

## Decision

**Introduce an approval-gated exception to containment, keyed by `(tool,
resolved-absolute-path)`, recorded on approval and consulted by the host tools —
making containment subordinate to an explicit grant instead of absolute.** The
existing `ToolRequest`/`Approve{scope}` machinery ([ADR-0052](0052-approval-scope-and-persisted-grants.md))
carries it; no new wire variant.

- **`ExtraRootStore`** (`entanglement_runtime::extra_roots`) records approved
  out-of-root `(tool, path)` grants at three scopes: `Once` (a single-use token
  consumed by the very next access), `Session` (process-lifetime, in memory), and
  `Always` (persisted to a managed `${config_dir}/entanglement/extra-roots.yml`,
  override `ENTANGLEMENT_EXTRA_ROOTS_FILE` — a sibling of `grants.yml`/the env
  file, not a section of the hand-edited `config.yml`). Per-tool by construction:
  the key is `(tool_name, absolute_path)`, so a `read` grant never satisfies a
  `write` check.
- **Detection lives in the executor** (`tool_runner::dispatch`), where approval
  already lives. A new `permission::escape_root_target(tool, input)` extracts the
  filesystem path each call would touch — the `path` for `read`/`edit`/`write`,
  the `workdir` for `bash`/`call` — **distinct from `permission_arg`** (which
  yields the *command* for `bash`/`call`). `host::escaping_path(root, rel)`
  resolves it with the exact same lexical + symlink-safe normalization the tools
  use and returns `Some(abs)` only when it leaves root, so the grant key is
  byte-identical on the executor and tool sides.
- **An out-of-root access forces an `Ask`** even when the profile resolves to
  `Allow` — escaping root always requires explicit consent the first time —
  *unless* the store already **durably** allows it (`Session`/`Always`), in which
  case the prompt is skipped. A `Deny` floor still wins: the profile forbidding a
  tool outright is never softened by an escape, so escaping can only *raise* the
  bar, never lower it. On approval the grant is recorded into the
  `ExtraRootStore` (every scope, since even `Once` must leave the single-use token
  the tool consumes).
- **The host tools consult the store to relax containment.**
  `host::resolve_under_root_or_grant(root, extra, tool, rel)` returns a contained
  path directly, and for an escaping path returns it **iff**
  `extra.take_allowance(tool, resolved)` succeeds (consuming a `Once`). The grant
  is checked against the **resolved, symlink-canonicalized** target, so a symlink
  under root can't smuggle access to an unapproved path. `extra == None` (the
  standalone/test constructors, and every existing test) reduces to the strict
  `resolve_under_root` — byte-identical to before.
- **Scope wiring is one shared `Arc<ExtraRootStore>`** created once in
  `build_config`, handed to the path-touching tools (`read`/`edit`/`write` via
  `host_tools_with_extra_roots`; `bash`/`call` via `with_extra_roots`) **and** to
  the executor as an `Option<EscapeRoot>` (the canonical `root` + the store).
  Both default 4-arg executor wrappers and every test pass `None` — escape-root
  is opt-in, wired only by the full head.
- **Escape-root grants are process-scoped, not per-`SessionId`.** `Session` scope
  means "for this run" rather than "for this exact session id". A local
  single-user process typically has one session, so the two coincide in practice,
  and keeping the grant off the session dimension lets the host tools consult the
  store from inside `resolve_under_root` without threading a `SessionId` through
  every call site — the tools override no signature, they just hold the `Arc`.

## Consequences

- **Positive.** A path outside the repo is now reachable when — and only when —
  the user approves it, at the granularity they choose, per tool. The common case
  (a profile that `Allow`s `read`, escaping forces one `Always` prompt) becomes
  fully silent thereafter; the log/transcript records the out-of-root path in the
  `ToolRequest` prompt.
- **Positive.** Zero new wire surface: reuses `OutEvent::ToolRequest` +
  `InMsg::Approve{scope}`; the TUI's `y`/`s`/`a` approval keys drive it unchanged.
  The prompt text is annotated ("⚠ accesses a path OUTSIDE the project root: …")
  so a human sees what they are approving.
- **Positive.** Strict containment is preserved by default and everywhere the
  store isn't wired: `resolve_under_root` is now a thin wrapper over the shared
  `resolve_and_contained`, and `escape_root == None` is the exact pre-ADR-0109
  behavior. The symlink defense ([ADR-0054](0054-canonicalizing-symlink-safe-root-containment.md))
  is unchanged — the grant is matched against the canonicalized target.
- **Negative / accepted, amended by [0132](0132-glob-grep-escape-root-search-via-durable-grant.md).**
  `glob`/`grep` were **not** covered at first: they route through `list_files`,
  which silently drops out-of-root matches, and their pattern-relative search
  has no single path to approve. Letting a recursive search descend into an
  approved external directory was deferred until a concrete need (tracked as
  [deferred-work ledger](../deferred-work-ledger.md) row 4, #482) — reading a
  specific external file via `read` + approval covered the practical case.
  ADR-0132 closes that gap without forcing a new prompt: search rides an
  existing durable (`Session`/`Always`) `read`-tool grant instead of
  triggering its own approval.
- **Negative / accepted.** `Session` scope is process-wide, not session-id-scoped,
  so a multi-session process shares out-of-root grants across its sessions. This
  trades a small amount of least-privilege for not threading `SessionId` through
  the tool containment path; acceptable under the local single-user trust model,
  and a multi-tenant embedder that needs per-tenant isolation would supply its
  own `Tool` implementations (the seam already exists via `run_for_session`,
  [ADR-0088](0088-session-aware-tool-execution.md)) rather than the file-backed
  default.
- **Neutral.** Persistence is a dedicated `extra-roots.yml`, **not** folded into
  `grants.yml`. The two are semantically different — a permission grant upgrades
  `Ask→Allow` for a `tool(arg)` key (the command for `bash`/`call`), while an
  escape grant relaxes *containment* for a `(tool, absolute-path)` key — and
  entangling them would force one key space to mean two things. Keeping them
  separate mirrors the managed-file convention (grants / agent-models /
  agent-generation / env each own their file).

## Alternatives considered

- **Reuse `grants.yml` / the permission `GrantStore` for escape approvals.**
  Rejected: the permission grant is keyed by `permission_arg` (the *command* for
  `bash`/`call`, the input `path` — possibly relative — for file tools), while a
  containment grant needs the *resolved absolute path*. Reusing one store would
  either mismatch keys or overload a single `tool(arg)` entry with two meanings
  (permission upgrade **and** containment relaxation). A dedicated store keyed by
  the canonical path is simpler and keeps each concern's key space clean.
- **Thread the approved path into the tool via the execution call instead of a
  shared store.** Rejected: `ToolRegistry::execute` → `run_for_session` carries
  only `(session, input)`; widening it to carry an approved-path set for one
  feature is a heavier, more invasive change than a shared `Arc` the tools hold,
  and would still need the store for the `Session`/`Always` scopes anyway.
- **Make containment configurable (a `root` allow-list in `config.yml`) instead
  of interactive approval.** Rejected: the user asked specifically for the
  allow-once / session / always *prompt* flow, which a static config list can't
  express; a static ceiling also can't distinguish per-tool grants at approval
  time. (A config ceiling that *forbids* escaping entirely still composes on top
  — the profile `Deny` floor already provides it.)
- **Session-id-scoped escape grants (thread `SessionId` through `resolve_under_root`).**
  Rejected for v1: it would require every path-touching tool to override
  `run_for_session` and thread the session into its resolve, for a
  least-privilege gain that's moot in the single-session local case. Process
  scope is the pragmatic default; per-tenant isolation is an embedder concern.
