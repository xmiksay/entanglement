# 0233. Live-only `NestedCall` events report a script's binding calls

- Status: Accepted
- Date: 2026-10-14
- Issue: #821 (deferred from epic #822, part 2)
- Amends: [ADR-0227](0227-one-binding-path-namespaces-http-skill-libraries.md)
  (the binding bridge now also reports each call it services) and
  [ADR-0002](0002-session-multiplexed-protocol.md)'s `OutEvent` (two additive
  variants, the first session-routed events that are never persisted)

## Context

Since ADR-0227 every `rhai` binding climbs the same dispatch seam as a direct
call. That covers host tools, `exec`/`bash`, `http()`, the generated `mcp::`
namespaces and the bindings a skill's `lib.rhai` function makes. Each call is
graded, hooked, granted and audited like a model-issued call. Heads and the
`narrate` aux model still saw only the one `rhai` `ToolExec`/`ToolOutput`.
Nested activity showed up only indirectly: as a `ToolRequest` on an `Ask`, a
`FileChange`, a hook payload or the script's own result text. A script that
looped for twenty seconds looked idle.

Core's `ToolExec`/`ToolOutput` cannot carry nested calls. Core parks a turn
only on ids it minted (ADR-0061), so a runtime-minted `ToolExec` would be
either ignored or mistaken for a call of the turn. Those events are also
content: they are persisted, replayed and folded into `Context`. A nested call
is not part of the model's history. The model sees the script's result, not
its steps.

## Decision

1. **Two new `OutEvent` variants**, emitted by the runtime:

   ```
   NestedCall{session, parent_request_id, index, tool, args?, duration_ms, is_error?}
   NestedCallSummary{session, parent_request_id, suppressed, errors}
   ```

   `parent_request_id` is the `rhai` call's id. It is deliberately not
   `request_id`, so no head mistakes a report for a call of its own and pairs
   an output with it. `index` is the call's 1-based position in the run.
   `duration_ms` is the wall-clock time the script was blocked on the call,
   approval waits included, because that is what the script experienced.
   `is_error` is set for a refusal, a budget timeout, a stopped run and a run
   that failed.

2. **Live only.** Both variants carry `session`, so every head routes them
   like any session event. They carry no `seq`, so they consume no content
   sequence number. `OutEvent::is_live_only()` is true for exactly these two.
   The persistence tap skips them, so they never reach a session log,
   `ReplayFrom`'s `History` (which only returns seq-bearing events) or
   `Context`. The runtime emits them through `Holly::emit_live`, which refuses
   any other variant, so a content event cannot slip past the log through
   that call. Replay ignores them even when a custom sink kept them. A core
   replay-equality test runs with them in the log and checks this.

3. **Emitted at the bridge.** `script::service`'s loop is the one place every
   binding of every run passes, for blocking and `background: true` runs, skill
   libraries and `mcp::` calls alike. It times each `Dispatcher::run` and
   reports it (`script::activity`). A background script's reports keep
   arriving after its `rhai` reply, still carrying the parent id. The
   reporter emits its closing summary on **drop**, not by an explicit call: a
   `Stop` aborts a blocking script's bridge task mid-await, and only `Drop`
   still runs then. A call the `Stop` cut off is reported as failed, so the
   summary still accounts for every call the run started.

4. **Flood control: cap + summary.** The first `NESTED_CALL_EVENT_CAP` (50)
   calls of a run are each reported. Later calls are only counted. When the
   run ends, one `NestedCallSummary` follows if any call was held back:
   `suppressed` calls, `errors` of which failed. This holds for a run that
   finishes, fails or is stopped (§3). A loop of thousands of bindings
   therefore costs at most 51 events per run on every head. The summary is a sibling variant, not a `summary` flag on
   `NestedCall`. A summary has no tool, args, index or duration, so one shared
   shape would need four fields that only sometimes exist. `kind` is also
   already the serde tag. Two variants keep each shape exact, and the
   exhaustive matches in the heads force both to be handled.

5. **The args summary carries no secrets.** For `bash`/`call` it is only
   the program's basename (the first word of the command that is not a
   `NAME=value` assignment, or `call`'s argv[0]), followed by ` …` when more
   follows. The rest of a command line is where a `curl -H 'Authorization:
   …'` or a `?token=` rides. Every other tool shows its permission argument
   (a path, `{METHOD} {url}`), and every URL in it loses its userinfo, query
   and fragment. A file write's content is never the permission argument, so
   it never appears. For a tool with no permission argument (an MCP tool, for
   one) the summary is the input's top-level key names only, never their
   values. It is bounded to one 80-character line, and computed only for a
   call that is reported, so a large input past the cap is never copied. `http_auth` credentials cannot
   appear, because the `http` tool adds them at send time and they are never
   part of a script's input.

6. **Rendering.** The TUI folds each report into its `rhai` op, the way an
   output folds into its call: a reverse scan of the transcript for the
   `ToolCall` whose `request_id` is the report's `parent_request_id`. The op
   keeps its lines (`nested`) and shows them as dim, indented
   `↳ #i tool  args  Nms` lines (`✗` for an error) under its header. While
   the op is expanded it shows every line. While it is collapsed it shows only
   the latest line, and the header gains a `· N calls` count, so a running
   script shows live progress without opening it. Two scripts running at once therefore keep their own
   lines, and a background script's late reports land under the op that
   launched it, not under a later turn. A report whose op is not in the
   transcript is kept as a standalone line. The TUI's markdown export skips
   the lines, since an export mirrors the replayable transcript. The `run`
   text head prints the same compact line. `run --format json`, `pipe` and
   `serve` relay the event as NDJSON/WS frames like any `OutEvent`. `run`
   (text and json) stops relaying at the turn's `Done`, because `run_one`
   exits then, so a background script's reports after that are dropped there.
   `narrate` narrates only a run's first `NestedCall` (`index == 1`), from its
   args summary. Its one-in-flight guard serializes aux calls but does not
   cap them, so a script of slow bindings would otherwise pay for an aux call
   per binding.

## Consequences

- A resumed session's transcript shows the script's call and result, but
  not its nested steps. This is accepted: the steps are derived data, and the
  log stays exactly what replay needs.
- `seq`-based dedup does not apply. A head that re-subscribes mid-run sees
  only the reports emitted after it joined, which matches the live-only
  scope.
- A command line is never shown past its program name. Under Allow, `auto`
  or Run all no approval card ever shows a nested command, and the values a
  script builds at run time never appear in its source. So the full command
  in every head's stream, including a CI-logged `run --format json`, would
  be a new exposure. The cost is a less specific line (`curl …`).
- The live events share the engine's lossy 1024-slot outbox with every other
  event. A head that lags far enough misses some of them like it misses any
  event, and the cap keeps one script from crowding the rest out.
- The nested call's internal id (`<parent>:rhai:<tool>`) is not on the wire.
  `index` is the stable per-run handle.

## Alternatives considered

- **Persist and replay the reports.** Rejected by the user. They are
  derived, not load-bearing for `Context`, and a long script would grow every
  log by thousands of records that replay must then skip.
- **Session-less events (the `Throttle` pattern).** These are kept out of the
  log for free, but they lose routing. Every multiplexed head would have to
  route by `parent_request_id`, and `run`'s session filter would drop them.
- **Reuse `ToolCall`/`ToolOutput` with a runtime-minted id.** Rejected because
  those events are content: they are persisted, replayed into `Context`, and
  paired by `request_id` in every head.
- **No cap, or a cap with no summary.** No cap floods heads and the narrator
  on a loop. A cap without a summary hides that anything was dropped.
- **A `summary` flag on `NestedCall`.** Rejected; see Decision §4.
