# 0059. `Tool` trait + `ToolRegistry` live in the runtime, not core

- Status: Accepted
- Date: 2026-07-13
- Refines the crate-boundary split of [0006](0006-core-dependency-hygiene-gate.md)/[0010](0010-single-head-crate-and-bash-opt-in.md); resolves the `ToolSpec`/`ToolCall` placement question left open by [0053](0053-invert-core-provider-seam.md).

## Context

Execution (#58) and permission dispatch (#59) moved out of `entanglement-core`
long ago: core emits `OutEvent::ToolExec` for every tool and parks on
`InMsg::ToolResult`, and the runtime's `tool_runner` owns the registry, the
policy decision, and the approval UX. Yet the **`Tool` trait and `ToolRegistry`
struct still lived in `entanglement-core/src/tools.rs`** — with a stale header
claiming the engine "can already dispatch, advertise tools to the model, and
report unknown tools." No core code called them: the turn loop's per-call
dispatch (`session/tools.rs`) only touches `ToolCall`. The only consumers were
`entanglement-runtime` (host tool impls, `tool_runner`, `script`) and the tests.

The dead surface trailed dead fields: `Holly.cfg`/`Holly.root` were carried
behind `#[allow(dead_code)]` (never read after spawn), and `Session::replay`
took a `_root: &Path` it documented as "unused in core but required for
consistency." All of it undercut the "core holds no executable tools" story and
invited drift.

[ADR-0053](0053-invert-core-provider-seam.md) inverted the core↔provider seam and
flagged the adjacent placement question: `ToolSpec`/`ToolCall` moved into
`entanglement-provider` (because `LlmRequest` carries tools), yet "tools" are an
engine/runtime vocabulary — should they get a neutral home when `Tool`/
`ToolRegistry` move to the runtime?

## Decision

**Move the `Tool` trait and `ToolRegistry` into `entanglement-runtime`
(`entanglement-runtime/src/tools.rs`, re-exported at the crate root). Delete the
dead core fields.**

- `entanglement-core` no longer defines or re-exports `Tool`/`ToolRegistry`. It
  keeps only the tool *schema* vocabulary — `ToolSpec` (carried by
  `EngineConfig.tool_specs`) and `ToolCall` (carried by the turn loop / replay) —
  which it re-exports from `entanglement-provider` per ADR-0053.
- The runtime's `tools.rs` imports `ToolSpec`/`ToolCall` from
  `entanglement_core` (the provider re-export), **not** from
  `entanglement-provider` directly, so the lean (`--no-default-features`) library
  keeps compiling without a direct provider dependency (`entanglement-provider`
  is optional, behind the `provider` feature — ADR-0025).
- `Holly.cfg`/`Holly.root` and the `#[allow(dead_code)]` on `Holly` are removed;
  `Session::replay` drops its unused `_root: &Path` parameter (and the supervisor
  stops threading a cwd it never used).

### `ToolSpec`/`ToolCall` stay in provider (re-exported)

The placement question from ADR-0053 is resolved by **keeping them where they
are**: they are DTOs of the LLM wire contract (`LlmRequest`/`LlmResponse` carry
them), the provider crate is their natural owner, and both core and runtime
already reach them through core's re-export. Extracting a neutral
`entanglement-llm`-style types crate for two small structs adds a crate and a
seam for no present benefit (KISS) — ADR-0053 itself judged this "acceptable
today; if it grates, the fix is a small neutral types module, not a
re-inversion." That fallback remains available if a future consumer needs the
tool DTOs without the provider crate.

## Alternatives considered

- **Extract a neutral `entanglement-types` crate** for `ToolSpec`/`ToolCall`
  (and possibly `Tool`). Rejected for now: premature — two DTOs don't justify a
  fourth crate, and nothing today needs them outside the provider→core→runtime
  line.
- **Leave `Tool`/`ToolRegistry` in core.** Rejected: that is the defect. Core
  holds no executable tools; the trait/registry are pure runtime vocabulary, and
  their presence in core is exactly the drift-inviting dead surface this ADR
  removes.

## Consequences

- Core's dependency story matches its code: it owns the protocol and the turn
  loop, advertises tool schemas, and holds nothing executable. `make tree`/
  `make check-lean` are unaffected (no dependency direction changed).
- Runtime consumers import `Tool`/`ToolRegistry` from `entanglement_runtime`
  (re-exported at the crate root); the runtime's own modules use
  `crate::tools::*`.
- Core integration tests no longer reach a `ToolRegistry`: the shared
  `spawn_tool_executor` test helper now takes a plain `Fn(&str, &str) -> String`
  closure instead, matching a world where the tool vocabulary is the runtime's.
- The change is mechanical and reversible; no wire type or protocol shape moved.
