# 0005. NDJSON stdio head (`run` + `pipe`)

- Status: Accepted
- Date: 2026-07-04

## Context

We need a head for scripting, editor integration, and CI — driving the engine
from a shell pipe. OpenCode's `opencode run --format json` emits raw JSON events,
one per line, as the turn streams.

## Decision

The stdio head (crate `entanglement-stdio`, binary `skutter`) has two subcommands, both driving
`Holly` directly (the ABI from ADR-0001):

- **`skutter run [--format text|json] [--agent <name>] "<prompt>"`** — one-shot:
  send a `Prompt`, stream `OutEvent`s until `Done`. `--format json` emits raw
  NDJSON (one event per line); `--format text` renders human-friendly output.
- **`skutter pipe`** — bidirectional NDJSON relay: `InMsg` lines on stdin,
  `OutEvent` lines on stdout. A line that isn't valid `InMsg` JSON falls back to
  being treated as a `Prompt` on the default session (so you can chat blind).

**NDJSON** (one JSON object per line) over **JSON-RPC**: there is no
request/response correlation. The protocol is fire-and-forget event push
(ADR-0001), and `seq` handles ordering, not correlation.

## Consequences

- **(+)** Pipes straight into `jq`, `awk`, `grep`; trivial to script and test.
- **(+)** Editors can speak NDJSON to `skutter pipe` like an LSP-ish subprocess.
- **(+)** `--format json` mirrors `opencode run --format json`, so muscle memory
  transfers.
- **(−)** No request/response id matching over stdio (by design — use `seq` and
  `Done` to know when a turn ends).

## Alternatives considered

- **JSON-RPC 2.0 with id correlation.** Rejected: the engine is event-stream
  oriented (ADR-0001); bolting on request/response ids adds machinery for no
  gain, since `Done` already bounds a turn.
- **Length-prefixed binary framing.** Rejected: not human-readable, hostile to
  `jq`/pipes, no editor benefit over NDJSON.
- **Text-only output (no JSON mode).** Rejected: scripting needs machine-readable
  events; `--format json` is the whole point for CI/editors.
