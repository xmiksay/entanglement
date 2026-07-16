# 0098. `rhai` JSON/YAML loader (`parse_json`/`to_json`/`parse_yaml`/`to_yaml`) and the `read_raw` binding

- Status: Accepted
- Date: 2026-07-16

## Context

[ADR-0046](0046-rhai-sandboxed-script-tool.md) bound the sandboxed `rhai` tool
to exactly the root-contained quintet (`read`/`glob`/`grep`/`edit`/`write`). A
natural follow-up need is a script that reads a JSON/YAML config file and
traverses it — auditing `providers.yml`, computing over a `package.json`, that
kind of thing — without shelling out.

Two designs were considered and rejected before landing on the one below:

1. **A standalone `validate` host tool** (`validate(path, format?)`, syntax-only
   check, model-callable directly). Rejected: `parse_json`/`parse_yaml` throwing
   on malformed input already gives syntax validation for free via
   `try { parse_json(x) } catch(e) {...}` — a dedicated tool would duplicate
   that for a narrower win (mainly discoverability without touching `rhai`
   syntax), not worth the extra permission-surface/mask/test cost for a
   near-redundant capability.
2. **XML/TOML support alongside JSON/YAML.** Rejected: `serde_json`/`serde_yaml`
   are already workspace dependencies (zero new cost); XML/TOML would each add a
   new crate for formats this codebase doesn't otherwise touch. JSON+YAML covers
   the actual use case (this project's own config/catalog files).

A third issue surfaced only empirically, after implementation: `read`'s bound
output is `"{lineno}: {line}"` text (ADR-0046 inherited `ReadTool::run`'s
model-readability format verbatim), which is not valid JSON/YAML. Composing
`read(path).parse_json()` — the obvious, intended usage — fails on every input.

## Decision

**The loader is four pure functions, not bindings.** `parse_json(str)`,
`to_json(value)`, `parse_yaml(str)`, `to_yaml(value)` are registered directly on
the engine (`register_data_functions`, alongside but distinct from
`register_bindings`) — no `BindingCall`/bridge round-trip, no permission
resolution, because they only transform a value already in the script's own
memory. Built on `rhai::serde::{to_dynamic, from_dynamic}` (the `serde` feature
was already enabled on the `rhai` dependency), so the JSON/YAML-`Value` ↔
`Dynamic` mapping is Rhai's own tested behavior rather than a hand-rolled
converter:

- `null` → `()`.
- An integer outside `i64` range **silently widens to an approximate `FLOAT`**
  rather than erroring — verified empirically against `rhai`'s
  `serialize_u64` (`ser.rs`: tries `i64`, then `decimal` if that feature is on,
  else falls back to `FLOAT`; only errors if floats are also disabled). This
  matches JS's `JSON.parse` for the same case, and matches the reason the
  "encode big IDs as JSON strings" convention exists in the first place — a raw
  out-of-range integer literal in source JSON is already the atypical case.
  Accepted as-is rather than adding a recursive pre-walk to force a throw.
- Rhai's UFCS means each function is also callable as a method:
  `read_raw(path).parse_json()`.

**`read_raw(path)` is a new binding — file content, no line-number prefix** —
added specifically because `read`'s output can't feed `parse_json`/`parse_yaml`.
It is a genuine capability binding (does IO, needs root containment and
permission resolution), implemented as `entanglement_runtime::host::ReadRawTool`
(`host/read.rs`), registered into the same `ToolRegistry` the `rhai` bridge
already uses.

**`read_raw` is never advertised as a standalone tool.** `main.rs`'s
`build_config` snapshots `cfg.tool_specs = tools.specs()` and only *afterward*
registers `ReadRawTool` into `tools` — present for execution (the rhai bridge
and any other registry consumer see it), absent from the model's tool list.
This keeps `EngineConfig.tool_specs` accurate to "what the model can call
directly" without a special-case flag on `Tool`/`ToolRegistry`.

**`read_raw` is graded and masked as an alias of `read`, not a distinct
permission surface.** `BindingPolicy::decide` remaps `"read_raw"` → `"read"`
before consulting the mask set or resolving the permission chain. This is
load-bearing, not cosmetic: `read_raw` is absent from `BINDING_TOOLS` and from
`tool_specs`, so a profile author restricting file reads only ever writes
`disallowed_tools: ["read"]` — they have no way to know `"read_raw"` exists to
list it separately. Grading it independently (or forgetting to grade it at all)
would let a script bypass a `read` restriction through the unlabeled raw path.
Aliasing closes that gap without adding a name for profile authors to have to
know about.

## Consequences

- **Positive.** The flagship use case — read a JSON/YAML file, traverse it,
  compute over it, all in one `rhai` call — now actually works end-to-end
  (`read_raw(path).parse_json()`), which it did not with `read` alone.
- **Positive.** Zero new dependencies (`serde_json`/`serde_yaml` already in the
  workspace; `rhai`'s `serde` feature was already on). Zero new permission
  surface visible to a profile author (`read_raw` piggybacks entirely on
  `read`'s grade).
- **Positive.** `parse_json`/`parse_yaml` throwing on malformed input is a
  working syntax validator for free (`try`/`catch`), so the standalone
  `validate` tool alternative costs nothing by being skipped.
- **Negative / cost.** One more binding-shaped special case in
  `BindingPolicy::decide` (the `"read_raw"` → `"read"` alias) — small, but a
  reader of `service_binding`/`exec` needs to know `call.tool` can carry a name
  that isn't 1:1 with what gets permission-graded.
- **Neutral / known limitation.** Map/object key order is not guaranteed to
  round-trip through `parse_json`/`to_json`/`parse_yaml`/`to_yaml` — matches
  JSON's own ordering-agnostic spec stance; not tested for, not promised in the
  tool description.
- **Neutral / known limitation.** An out-of-`i64`-range JSON/YAML integer
  literal silently loses precision (widens to `FLOAT`) rather than erroring.
  Acceptable per the Decision section; revisit only if it causes real confusion
  in practice, at which point a recursive pre-walk rejecting such literals
  before `to_dynamic` is the fix (deliberately not built now — no evidence it's
  needed, and it doubles the size-cap-adjacent code for a case well-formed JSON
  avoids by convention).

## Alternatives considered

- **Standalone `validate(path, format?)` host tool.** See Context — rejected as
  near-redundant with `try { parse_json(...) } catch`, for a real but narrower
  win (discoverability without `rhai`, independence from `rhai` being masked)
  that didn't justify the added permission/mask/test surface.
- **XML/TOML support.** Rejected — no existing dependency, no evidence of need
  in this codebase's own files.
- **JSON Schema validation.** Rejected for v1 — syntax-only covers the request;
  schema validation is a bigger surface (a schema crate, a way for the model to
  supply/reference a schema) with no current concrete use case.
- **Force a throw on out-of-`i64`-range integers via a recursive pre-walk.**
  Rejected — see Consequences; `rhai`'s own fallback (silent float-widening)
  already matches mainstream JSON-tooling convention (JS), and the walk is real
  additional code (one walker per format) for an input shape well-formed JSON
  is expected to avoid.
- **Give `read_raw` its own `BINDING_TOOLS` entry / independent grading.**
  Rejected — see Decision: it would let a profile's `read` restriction be
  silently bypassed by a script, since profile authors have no visibility into
  `read_raw`'s existence to restrict it separately.
