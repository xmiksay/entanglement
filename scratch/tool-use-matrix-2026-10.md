# Live tool-use matrix — z.ai GLM models (2026-10-12)

Evidence for #818/#822 (one call convention: native kernel + `mcp(server, tool, args)` +
`rhai`), measured by the opt-in harness `entanglement-provider/tests/live_tool_matrix.rs`
(`make test-live-matrix`, #826). Raw records are the harness's JSONL (not committed);
this file is the summary the ADR cites.

## Setup

- **Wire:** raw z.ai Chat Completions (Coding Plan endpoint), non-streaming, `thinking`
  on and off (`glm-5.3`/`glm-5.3-flash` require it on). The `tools` array is the variable
  under test, so the harness bypasses the provider client.
- **Simulated MCP surface:** three servers (`weather`, `calendar`, `docs`), five tools,
  deterministic stub results, executed in-process (a rhai script runs in a real `rhai`
  engine against stub `mcp::<server>::<tool>` modules).
- **Call encodings (seeded):** the history already holds `list_mcp()` and
  `mcp_tools(server)` with the task server's tools rendered in the encoding's convention.

  | | encoding | `tools` array | tool listing says |
  | --- | --- | --- | --- |
  | V1 | `append` (ADR-0196) | kernel + the server's tools natively | JSON schema |
  | V2 | `undeclared` (`native_first` without fallback) | kernel | JSON schema, "call it by name" |
  | V3 | `invoke {name, args}` (ADR-0204) | kernel + `invoke` | JSON schema |
  | V4 | `mcp(server, tool, args)` (#818) | kernel + `mcp` | JSON schema + `mcp(...)` example |
  | V5 | `rhai`, signature only | kernel + `rhai` | rhai signature + example |
  | V6 | `rhai`, schema + example | kernel + `rhai` | JSON schema + `mcp::srv::tool(#{...})` example |

- **Discovery shapes (unseeded, `mcp` encoding only):** the model starts from the bare
  task and discovers the server itself through `explore(filter)`+`describe(names)` (today)
  or `list_mcp()`+`mcp_tools(server)` (#818).
- **Tasks:** `scalar` (two scalar args), `nested` (nested-object arg), `compose` (search →
  read, answer must carry the read value), `followup` (a second question after an earlier
  call in history, written exactly as that encoding emits it — ADR-0204's history rule).
- **Metrics per run:** reached (the expected tool was called), args ok (correct arguments;
  compose also needs the answer), discovery calls the model made itself, rounds,
  completion tokens, cached share of the first request. Max 5 rounds per run.
- **Runs:** 3 per cell. 2 226 records in total before the run was stopped (every model
  complete except `glm-4.5-flash` thinking-on at 63/84 runs).

## Coverage

| covered (Coding Plan) | not covered |
| --- | --- |
| glm-5.3, glm-5.3-flash (thinking on only), glm-5.2, glm-5.1, glm-5, glm-4.7, glm-4.7-flash, glm-4.6, glm-4.5, glm-4.5-air, glm-4.5-flash — thinking on and off | glm-4.7-flashx, glm-4.5-x (every call `1113 Insufficient balance or no resource package`); glm-4.5-airx and glm-4-32b-0414-128k answered the same on a one-shot probe and were not run |

Errors on covered models (34 runs, all on `glm-4.7-flash`): endpoint overload
(`1305`/`1302` 429s) and three transport failures — not model behaviour. They are excluded
from the denominators below.

## Results — all covered models, thinking on + off

| column | runs | errors | reached % | args ok % | mean discovery calls | mean rounds | mean completion tokens |
| --- | --- | --- | --- | --- | --- | --- | --- |
| append (V1) | 240 | 4 | 100 | 100 | 0.00 | 2.25 | 79 |
| undeclared (V2) | 240 | 5 | 18 | 14 | 4.04 | 4.55 | 150 |
| invoke (V3) | 240 | 1 | 100 | 100 | 0.00 | 2.25 | 92 |
| mcp (V4) | 240 | 12 | 100 | 100 | 0.00 | 2.25 | 97 |
| rhai, signature (V5) | 240 | 0 | 100 | 99 | 0.00 | 2.27 | 104 |
| rhai, schema + example (V6) | 240 | 3 | 100 | 96 | 0.00 | 2.33 | 101 |
| mcp, discovered via explore/describe | 231 | 3 | 100 | 99 | 1.54 | 3.76 | 138 |
| mcp, discovered via list_mcp/mcp_tools | 228 | 5 | 100 | 99 | 1.48 | 3.73 | 128 |

Per task (args ok / non-error runs, mean rounds):

| column | scalar | nested | compose | followup |
| --- | --- | --- | --- | --- |
| append | 59/59 (r2.0) | 57/58 (r2.0) | 59/59 (r3.0) | 60/60 (r2.0) |
| undeclared | 11/59 (r4.3) | 0/58 (r4.9) | 10/58 (r4.6) | 12/60 (r4.4) |
| invoke | 60/60 (r2.0) | 59/60 (r2.0) | 59/59 (r3.0) | 60/60 (r2.0) |
| mcp | 58/58 (r2.0) | 56/57 (r2.0) | 57/57 (r3.0) | 56/56 (r2.0) |
| rhai, signature | 60/60 (r2.0) | 60/60 (r2.0) | 58/60 (r3.0) | 60/60 (r2.0) |
| rhai, schema + example | 60/60 (r2.1) | 59/60 (r2.1) | 50/58 (r3.2) | 59/59 (r2.0) |
| mcp + explore/describe | 58/58 (r4.0) | 57/57 (r4.0) | 54/57 (r5.0) | 56/56 (r2.0) |
| mcp + list_mcp/mcp_tools | 57/57 (r4.0) | 54/55 (r4.0) | 52/54 (r5.0) | 57/57 (r2.0) |

Per model (args ok / non-error runs over every column except `undeclared`):

| model | thinking off | thinking on |
| --- | --- | --- |
| glm-5.3 | — (required on) | 84/84 |
| glm-5.3-flash | — (required on) | 84/84 |
| glm-5.2 | 84/84 | 84/84 |
| glm-5.1 | 84/84 | 84/84 |
| glm-5 | 83/84 | 84/84 |
| glm-4.7 | 84/84 | 84/84 |
| glm-4.7-flash | 59/65 (19 endpoint errors) | 72/75 (9 endpoint errors) |
| glm-4.6 | 84/84 | 84/84 |
| glm-4.5 | 84/84 | 84/84 |
| glm-4.5-air | 83/84 | 84/84 |
| glm-4.5-flash | 80/84 | 58/63 (partial) |

## Findings

1. **The `mcp(server, tool, args)` envelope reaches the tool on every covered GLM model,
   thinking on or off, with correct arguments including a nested object and a follow-up
   turn.** Same for `invoke` and for native `append`. The envelope costs ~15–20 completion
   tokens per call over a native call.
2. **Calling an undeclared tool fails on every model except the two flash models**
   (`glm-4.7-flash`, `glm-4.5-flash`), which call it natively — exactly ADR-0204's
   finding. The others re-run discovery until the round limit.
3. **`rhai` as the call surface works:** signature-only listings reach 100 % / 99 % args ok;
   schema + example listings 100 % / 96 %. The "Rhai is out of distribution" risk did not
   materialise on these models for single calls. A signature-only listing is both shorter and
   slightly more accurate than schema + example.
4. **Scripts did not chain calls.** On `compose`, only 8/60 signature runs and 0/58 schema
   runs did search → read in one script; the rest called one tool per round, like the JSON
   encodings (rounds 3.0 vs 3.0). The system prompt's "one script may chain several calls"
   is not enough — the `rhai` tool description has to show a chained example if the
   one-script-many-calls economy is wanted.
5. **Discovery shape does not change success** (99 % both); `list_mcp`/`mcp_tools` needs
   slightly fewer discovery calls (1.48 vs 1.54) and ~7 % fewer completion tokens than
   `explore`/`describe`. Both are fine; the choice is about schema simplicity, not model
   behaviour.
6. **Prompt cache:** a stable `tools` array keeps the first request's cached share at 60–77 %
   across encodings; the harness does not vary the array mid-session, so this is the
   steady state, not the ADR-0204 miss measurement.

## Implications for #822

- #818 can proceed with `mcp(...)` as the only MCP call convention and `list_mcp`/`mcp_tools`
  as the discovery pair; no GLM model needs the `append` or `native_first` paths.
- #819's `rhai` spec and the embedded `rhai` skill must carry a chained-call example
  (search → read in one script) and say so explicitly; otherwise scripts bring no round
  savings over `mcp(...)`.
- `glm-4.7-flashx`, `glm-4.5-x`, `glm-4.5-airx`, `glm-4-32b-0414-128k` are not usable on the
  Coding Plan key; the catalog keeps them for `zai_paas`.

## Regenerate

```bash
set -a; . ~/.config/entanglement/.env; set +a
ZAI_PROBE_RUNS=3 make test-live-matrix          # every catalog model, ~8 h sequential
ZAI_PROBE_REPORT_ONLY=1 make test-live-matrix   # rebuild scratch/tool-matrix.md from the JSONL
```

## Appendix — per-model tables as generated

Cell: `args_ok/N ok reached · d=mean discovery calls · r=mean rounds · t=mean completion
tokens`; `+explore`/`+list` columns are the unseeded discovery cells. Rows for
`glm-4.7-flashx` and `glm-4.5-x` are all errors (not in the Coding Plan).

## task `scalar`

| model | thinking | append | undeclared | invoke | mcp | rhai_sig | rhai_schema | mcp+explore | mcp+list |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| glm-4.5 | off | 3/3 ok 3 · d0.0 r2.0 t41 | 0/3 ok 0 · d3.3 r3.3 t60 | 3/3 ok 3 · d0.0 r2.0 t50 | 3/3 ok 3 · d0.0 r2.0 t54 | 3/3 ok 3 · d0.0 r2.0 t48 | 3/3 ok 3 · d0.0 r2.0 t47 | 3/3 ok 3 · d2.0 r4.0 t85 | 3/3 ok 3 · d2.0 r4.0 t77 |
| glm-4.5 | on | 3/3 ok 3 · d0.0 r2.0 t44 | 0/3 ok 0 · d5.3 r5.0 t203 | 3/3 ok 3 · d0.0 r2.0 t55 | 3/3 ok 3 · d0.0 r2.0 t56 | 3/3 ok 3 · d0.0 r2.0 t62 | 3/3 ok 3 · d0.0 r2.0 t51 | 3/3 ok 3 · d2.0 r4.0 t125 | 3/3 ok 3 · d2.0 r4.0 t122 |
| glm-4.5-air | off | 3/3 ok 3 · d0.0 r2.0 t40 | 0/3 ok 0 · d4.0 r4.7 t161 | 3/3 ok 3 · d0.0 r2.0 t51 | 3/3 ok 3 · d0.0 r2.0 t55 | 3/3 ok 3 · d0.0 r2.0 t47 | 3/3 ok 3 · d0.0 r2.0 t48 | 3/3 ok 3 · d2.0 r4.0 t81 | 3/3 ok 3 · d2.0 r4.0 t73 |
| glm-4.5-air | on | 3/3 ok 3 · d0.0 r2.0 t51 | 0/3 ok 0 · d4.7 r5.0 t305 | 3/3 ok 3 · d0.0 r2.0 t70 | 3/3 ok 3 · d0.0 r2.0 t63 | 3/3 ok 3 · d0.0 r2.0 t60 | 3/3 ok 3 · d0.0 r2.0 t64 | 3/3 ok 3 · d2.0 r4.0 t121 | 3/3 ok 3 · d2.0 r4.0 t109 |
| glm-4.5-flash | off | 3/3 ok 3 · d0.0 r2.0 t45 | 3/3 ok 3 · d0.0 r2.0 t50 | 3/3 ok 3 · d0.0 r2.0 t61 | 3/3 ok 3 · d0.0 r2.0 t62 | 3/3 ok 3 · d0.0 r2.3 t75 | 3/3 ok 3 · d0.0 r2.3 t72 | 3/3 ok 3 · d2.0 r4.0 t123 | 3/3 ok 3 · d2.0 r4.3 t137 |
| glm-4.5-flash | on | 3/3 ok 3 · d0.0 r2.0 t158 | 3/3 ok 3 · d0.0 r2.0 t96 | 3/3 ok 3 · d0.0 r2.0 t121 | 3/3 ok 3 · d0.0 r2.0 t203 | 3/3 ok 3 · d0.0 r2.7 t238 | 3/3 ok 3 · d0.0 r3.0 t215 | 3/3 ok 3 · d2.0 r4.0 t334 | — |
| glm-4.5-x | off | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err |
| glm-4.5-x | on | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | — | — | — | — |
| glm-4.6 | off | 3/3 ok 3 · d0.0 r2.0 t40 | 0/3 ok 0 · d3.3 r4.0 t202 | 3/3 ok 3 · d0.0 r2.0 t51 | 3/3 ok 3 · d0.0 r2.0 t53 | 3/3 ok 3 · d0.0 r2.0 t47 | 3/3 ok 3 · d0.0 r2.0 t48 | 3/3 ok 3 · d2.0 r4.0 t81 | 3/3 ok 3 · d2.0 r4.0 t76 |
| glm-4.6 | on | 3/3 ok 3 · d0.0 r2.0 t60 | 0/3 ok 0 · d5.0 r5.0 t166 | 3/3 ok 3 · d0.0 r2.0 t59 | 3/3 ok 3 · d0.0 r2.0 t65 | 3/3 ok 3 · d0.0 r2.0 t56 | 3/3 ok 3 · d0.0 r2.0 t60 | 3/3 ok 3 · d2.0 r4.0 t126 | 3/3 ok 3 · d2.0 r4.0 t119 |
| glm-4.7 | off | 3/3 ok 3 · d0.0 r2.0 t41 | 0/3 ok 0 · d5.0 r5.0 t99 | 3/3 ok 3 · d0.0 r2.0 t50 | 3/3 ok 3 · d0.0 r2.0 t55 | 3/3 ok 3 · d0.0 r2.0 t47 | 3/3 ok 3 · d0.0 r2.0 t49 | 3/3 ok 3 · d2.0 r4.0 t82 | 3/3 ok 3 · d2.0 r4.0 t84 |
| glm-4.7 | on | 3/3 ok 3 · d0.0 r2.0 t55 | 0/3 ok 0 · d5.7 r5.0 t216 | 3/3 ok 3 · d0.0 r2.0 t63 | 3/3 ok 3 · d0.0 r2.0 t59 | 3/3 ok 3 · d0.0 r2.0 t61 | 3/3 ok 3 · d0.0 r2.0 t60 | 3/3 ok 3 · d2.0 r4.0 t121 | 3/3 ok 3 · d2.0 r4.0 t116 |
| glm-4.7-flash | off | 2/3 ok 2 · d0.0 r1.7 t26 · 1 err | 3/3 ok 3 · d0.0 r2.0 t39 | 3/3 ok 3 · d0.0 r2.0 t55 | 2/3 ok 2 · d0.0 r1.7 t29 · 2 err | 3/3 ok 3 · d0.0 r2.0 t68 | 3/3 ok 3 · d0.0 r2.0 t51 | 2/3 ok 2 · d2.0 r3.7 t87 · 1 err | 3/3 ok 3 · d2.0 r4.0 t92 |
| glm-4.7-flash | on | 3/3 ok 3 · d0.0 r2.0 t76 | 3/3 ok 3 · d0.0 r2.0 t78 · 1 err | 3/3 ok 3 · d0.0 r2.0 t103 | 3/3 ok 3 · d0.0 r2.0 t103 | 3/3 ok 3 · d0.0 r2.0 t96 | 3/3 ok 3 · d0.0 r2.0 t104 | 2/3 ok 2 · d1.7 r3.3 t211 · 1 err | 3/3 ok 3 · d2.0 r4.0 t273 |
| glm-4.7-flashx | off | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err |
| glm-4.7-flashx | on | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err |
| glm-5 | off | 3/3 ok 3 · d0.0 r2.0 t40 | 0/3 ok 0 · d5.0 r5.0 t68 | 3/3 ok 3 · d0.0 r2.0 t51 | 3/3 ok 3 · d0.0 r2.0 t53 | 3/3 ok 3 · d0.0 r2.0 t48 | 3/3 ok 3 · d0.0 r2.0 t46 | 3/3 ok 3 · d2.0 r4.0 t82 | 3/3 ok 3 · d2.0 r4.0 t74 |
| glm-5 | on | 3/3 ok 3 · d0.0 r2.0 t54 | 0/3 ok 0 · d5.0 r5.0 t88 | 3/3 ok 3 · d0.0 r2.0 t55 | 3/3 ok 3 · d0.0 r2.0 t56 | 3/3 ok 3 · d0.0 r2.0 t49 | 3/3 ok 3 · d0.0 r2.0 t49 | 3/3 ok 3 · d2.0 r4.0 t108 | 3/3 ok 3 · d2.0 r4.0 t107 |
| glm-5.1 | off | 3/3 ok 3 · d0.0 r2.0 t41 | 0/3 ok 0 · d5.0 r5.0 t77 | 3/3 ok 3 · d0.0 r2.0 t51 | 3/3 ok 3 · d0.0 r2.0 t54 | 3/3 ok 3 · d0.0 r2.0 t47 | 3/3 ok 3 · d0.0 r2.0 t48 | 3/3 ok 3 · d2.0 r4.0 t84 | 3/3 ok 3 · d2.0 r4.0 t76 |
| glm-5.1 | on | 3/3 ok 3 · d0.0 r2.0 t49 | 0/3 ok 0 · d5.0 r5.0 t106 | 3/3 ok 3 · d0.0 r2.0 t55 | 3/3 ok 3 · d0.0 r2.0 t58 | 3/3 ok 3 · d0.0 r2.0 t52 | 3/3 ok 3 · d0.0 r2.0 t50 | 3/3 ok 3 · d2.0 r4.0 t111 | 3/3 ok 3 · d2.0 r4.0 t108 |
| glm-5.2 | off | 3/3 ok 3 · d0.0 r2.0 t40 | 0/3 ok 0 · d5.0 r5.0 t77 | 3/3 ok 3 · d0.0 r2.0 t51 | 3/3 ok 3 · d0.0 r2.0 t54 | 3/3 ok 3 · d0.0 r2.0 t47 | 3/3 ok 3 · d0.0 r2.0 t47 | 3/3 ok 3 · d2.0 r4.0 t84 | 3/3 ok 3 · d2.0 r4.0 t75 |
| glm-5.2 | on | 3/3 ok 3 · d0.0 r2.0 t55 | 0/3 ok 0 · d5.0 r5.0 t93 | 3/3 ok 3 · d0.0 r2.0 t56 | 3/3 ok 3 · d0.0 r2.0 t54 | 3/3 ok 3 · d0.0 r2.0 t48 | 3/3 ok 3 · d0.0 r2.0 t48 | 3/3 ok 3 · d2.0 r4.0 t105 | 3/3 ok 3 · d2.0 r4.0 t106 |
| glm-5.3 | on | 3/3 ok 3 · d0.0 r2.0 t42 | 0/3 ok 0 · d5.0 r5.0 t85 | 3/3 ok 3 · d0.0 r2.0 t51 | 3/3 ok 3 · d0.0 r2.0 t58 | 3/3 ok 3 · d0.0 r2.0 t51 | 3/3 ok 3 · d0.0 r2.0 t48 | 3/3 ok 3 · d2.0 r4.0 t108 | 3/3 ok 3 · d2.0 r4.0 t107 |
| glm-5.3-flash | on | 3/3 ok 3 · d0.0 r2.0 t46 | 0/3 ok 0 · d5.3 r5.0 t381 | 3/3 ok 3 · d0.0 r2.0 t64 | 3/3 ok 3 · d0.0 r2.0 t56 | 3/3 ok 3 · d0.0 r2.0 t62 | 3/3 ok 3 · d0.0 r2.0 t58 | 3/3 ok 3 · d2.0 r4.0 t125 | 3/3 ok 3 · d2.0 r4.0 t120 |

## task `nested`

| model | thinking | append | undeclared | invoke | mcp | rhai_sig | rhai_schema | mcp+explore | mcp+list |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| glm-4.5 | off | 3/3 ok 3 · d0.0 r2.0 t89 | 0/3 ok 0 · d3.7 r4.3 t88 | 3/3 ok 3 · d0.0 r2.0 t102 | 3/3 ok 3 · d0.0 r2.0 t103 | 3/3 ok 3 · d0.0 r2.0 t100 | 3/3 ok 3 · d0.0 r2.0 t99 | 3/3 ok 3 · d2.0 r4.0 t131 | 3/3 ok 3 · d2.0 r4.0 t124 |
| glm-4.5 | on | 3/3 ok 3 · d0.0 r2.0 t106 | 0/3 ok 0 · d5.0 r5.0 t124 | 3/3 ok 3 · d0.0 r2.0 t118 | 3/3 ok 3 · d0.0 r2.0 t117 | 3/3 ok 3 · d0.0 r2.0 t143 | 3/3 ok 3 · d0.0 r2.0 t113 | 3/3 ok 3 · d2.0 r4.0 t169 | 3/3 ok 3 · d2.0 r4.0 t174 |
| glm-4.5-air | off | 3/3 ok 3 · d0.0 r2.0 t90 | 0/3 ok 0 · d5.0 r5.0 t73 | 3/3 ok 3 · d0.0 r2.0 t102 | 3/3 ok 3 · d0.0 r2.0 t103 | 3/3 ok 3 · d0.0 r2.0 t98 | 3/3 ok 3 · d0.0 r2.0 t98 | 3/3 ok 3 · d2.0 r4.0 t132 | 3/3 ok 3 · d2.0 r4.0 t125 |
| glm-4.5-air | on | 3/3 ok 3 · d0.0 r2.0 t102 | 0/3 ok 0 · d5.0 r5.0 t248 | 3/3 ok 3 · d0.0 r2.0 t116 | 3/3 ok 3 · d0.0 r2.0 t127 | 3/3 ok 3 · d0.0 r2.0 t125 | 3/3 ok 3 · d0.0 r2.0 t129 | 3/3 ok 3 · d2.0 r4.0 t182 | 3/3 ok 3 · d2.0 r4.0 t168 |
| glm-4.5-flash | off | 2/3 ok 3 · d0.0 r2.0 t101 | 0/3 ok 3 · d0.0 r4.7 t313 | 3/3 ok 3 · d0.0 r2.0 t129 | 3/3 ok 3 · d0.0 r2.0 t119 | 3/3 ok 3 · d0.0 r2.3 t177 | 3/3 ok 3 · d0.0 r2.0 t109 | 3/3 ok 3 · d2.0 r4.0 t194 | 3/3 ok 3 · d2.0 r4.0 t178 |
| glm-4.5-flash | on | 3/3 ok 3 · d0.0 r2.0 t398 | 0/3 ok 3 · d0.0 r5.0 t957 | 2/3 ok 3 · d0.0 r2.0 t367 | 2/3 ok 3 · d0.0 r2.0 t400 | 3/3 ok 3 · d0.0 r2.0 t362 | 3/3 ok 3 · d0.0 r2.0 t313 | — | — |
| glm-4.5-x | off | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err |
| glm-4.5-x | on | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | — | — | — | — | — |
| glm-4.6 | off | 3/3 ok 3 · d0.0 r2.0 t89 | 0/3 ok 0 · d3.3 r4.3 t185 | 3/3 ok 3 · d0.0 r2.0 t102 | 3/3 ok 3 · d0.0 r2.0 t104 | 3/3 ok 3 · d0.0 r2.0 t98 | 3/3 ok 3 · d0.0 r2.0 t100 | 3/3 ok 3 · d2.0 r4.0 t134 | 3/3 ok 3 · d2.0 r4.0 t124 |
| glm-4.6 | on | 3/3 ok 3 · d0.0 r2.0 t126 | 0/3 ok 0 · d5.0 r5.0 t225 | 3/3 ok 3 · d0.0 r2.0 t114 | 3/3 ok 3 · d0.0 r2.0 t127 | 3/3 ok 3 · d0.0 r2.0 t107 | 3/3 ok 3 · d0.0 r2.0 t161 | 3/3 ok 3 · d2.0 r4.0 t189 | 3/3 ok 3 · d2.0 r4.0 t168 |
| glm-4.7 | off | 3/3 ok 3 · d0.0 r2.0 t91 | 0/3 ok 0 · d4.3 r4.7 t109 | 3/3 ok 3 · d0.0 r2.0 t103 | 3/3 ok 3 · d0.0 r2.0 t105 | 3/3 ok 3 · d0.0 r2.0 t100 | 3/3 ok 3 · d0.0 r2.0 t99 | 3/3 ok 3 · d2.0 r4.0 t132 | 3/3 ok 3 · d2.0 r4.0 t126 |
| glm-4.7 | on | 3/3 ok 3 · d0.0 r2.0 t98 | 0/3 ok 0 · d5.0 r5.0 t228 | 3/3 ok 3 · d0.0 r2.0 t122 | 3/3 ok 3 · d0.0 r2.0 t127 | 3/3 ok 3 · d0.0 r2.0 t143 | 3/3 ok 3 · d0.0 r2.0 t122 | 3/3 ok 3 · d2.0 r4.0 t184 | 3/3 ok 3 · d2.0 r4.0 t173 |
| glm-4.7-flash | off | 3/3 ok 3 · d0.0 r2.0 t87 · 1 err | 0/3 ok 2 · d0.0 r3.3 t157 · 1 err | 3/3 ok 3 · d0.0 r2.3 t131 | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 3/3 ok 3 · d0.0 r2.0 t139 | 2/3 ok 3 · d0.0 r2.7 t181 | 3/3 ok 3 · d2.0 r4.0 t178 | 2/3 ok 3 · d1.3 r3.7 t167 |
| glm-4.7-flash | on | 3/3 ok 3 · d0.0 r2.0 t186 · 1 err | 0/3 ok 3 · d0.0 r4.7 t1052 · 1 err | 3/3 ok 3 · d0.0 r2.0 t216 | 3/3 ok 3 · d0.0 r2.0 t253 | 3/3 ok 3 · d0.0 r2.0 t254 | 3/3 ok 3 · d0.0 r2.7 t293 | 3/3 ok 3 · d2.7 r4.3 t416 | 2/3 ok 2 · d1.3 r3.0 t267 · 2 err |
| glm-4.7-flashx | off | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err |
| glm-4.7-flashx | on | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err |
| glm-5 | off | 3/3 ok 3 · d0.0 r2.0 t96 | 0/3 ok 0 · d5.0 r5.0 t67 | 3/3 ok 3 · d0.0 r2.0 t105 | 3/3 ok 3 · d0.0 r2.0 t109 | 3/3 ok 3 · d0.0 r2.0 t104 | 3/3 ok 3 · d0.0 r2.0 t101 | 3/3 ok 3 · d2.0 r4.0 t139 | 3/3 ok 3 · d2.0 r4.0 t130 |
| glm-5 | on | 3/3 ok 3 · d0.0 r2.0 t100 | 0/3 ok 0 · d5.7 r5.0 t72 | 3/3 ok 3 · d0.0 r2.0 t119 | 3/3 ok 3 · d0.0 r2.0 t118 | 3/3 ok 3 · d0.0 r2.0 t117 | 3/3 ok 3 · d0.0 r2.0 t113 | 3/3 ok 3 · d2.0 r4.0 t171 | 3/3 ok 3 · d2.0 r4.0 t159 |
| glm-5.1 | off | 3/3 ok 3 · d0.0 r2.0 t96 | 0/3 ok 0 · d5.0 r5.0 t65 | 3/3 ok 3 · d0.0 r2.0 t109 | 3/3 ok 3 · d0.0 r2.0 t111 | 3/3 ok 3 · d0.0 r2.0 t99 | 3/3 ok 3 · d0.0 r2.0 t98 | 3/3 ok 3 · d2.0 r4.0 t140 | 3/3 ok 3 · d2.0 r4.0 t133 |
| glm-5.1 | on | 3/3 ok 3 · d0.0 r2.0 t108 | 0/3 ok 0 · d5.0 r5.0 t79 | 3/3 ok 3 · d0.0 r2.0 t130 | 3/3 ok 3 · d0.0 r2.0 t113 | 3/3 ok 3 · d0.0 r2.0 t117 | 3/3 ok 3 · d0.0 r2.0 t114 | 3/3 ok 3 · d2.0 r4.0 t183 | 3/3 ok 3 · d2.0 r4.0 t169 |
| glm-5.2 | off | 3/3 ok 3 · d0.0 r2.0 t96 | 0/3 ok 0 · d5.0 r5.0 t65 | 3/3 ok 3 · d0.0 r2.0 t107 | 3/3 ok 3 · d0.0 r2.0 t110 | 3/3 ok 3 · d0.0 r2.0 t105 | 3/3 ok 3 · d0.0 r2.0 t99 | 3/3 ok 3 · d2.0 r4.0 t144 | 3/3 ok 3 · d2.0 r4.0 t133 |
| glm-5.2 | on | 3/3 ok 3 · d0.0 r2.0 t103 | 0/3 ok 0 · d5.0 r5.0 t65 | 3/3 ok 3 · d0.0 r2.0 t117 | 3/3 ok 3 · d0.0 r2.0 t121 | 3/3 ok 3 · d0.0 r2.0 t107 | 3/3 ok 3 · d0.0 r2.0 t119 | 3/3 ok 3 · d2.0 r4.0 t187 | 3/3 ok 3 · d2.0 r4.0 t173 |
| glm-5.3 | on | 3/3 ok 3 · d0.0 r2.0 t105 | 0/3 ok 0 · d5.0 r5.0 t74 | 3/3 ok 3 · d0.0 r2.0 t121 | 3/3 ok 3 · d0.0 r2.0 t118 | 3/3 ok 3 · d0.0 r2.0 t106 | 3/3 ok 3 · d0.0 r2.0 t115 | 3/3 ok 3 · d2.0 r4.0 t174 | 3/3 ok 3 · d2.0 r4.0 t173 |
| glm-5.3-flash | on | 3/3 ok 3 · d0.0 r2.0 t106 | 0/3 ok 0 · d5.0 r5.0 t214 | 3/3 ok 3 · d0.0 r2.0 t114 | 3/3 ok 3 · d0.0 r2.0 t117 | 3/3 ok 3 · d0.0 r2.0 t134 | 3/3 ok 3 · d0.0 r2.0 t130 | 3/3 ok 3 · d2.0 r4.0 t168 | 3/3 ok 3 · d2.0 r4.0 t172 |

## task `compose`

| model | thinking | append | undeclared | invoke | mcp | rhai_sig | rhai_schema | mcp+explore | mcp+list |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| glm-4.5 | off | 3/3 ok 3 · d0.0 r3.0 t67 | 0/3 ok 0 · d5.0 r5.0 t84 | 3/3 ok 3 · d0.0 r3.0 t90 | 3/3 ok 3 · d0.0 r3.0 t92 | 3/3 ok 3 · d0.0 r2.7 t95 | 3/3 ok 3 · d0.0 r3.3 t113 | 3/3 ok 3 · d2.0 r5.0 t133 | 3/3 ok 3 · d2.0 r5.0 t122 |
| glm-4.5 | on | 3/3 ok 3 · d0.0 r3.0 t94 | 0/3 ok 0 · d5.0 r5.0 t141 | 3/3 ok 3 · d0.0 r3.0 t117 | 3/3 ok 3 · d0.0 r3.0 t125 | 3/3 ok 3 · d0.0 r3.0 t107 | 3/3 ok 3 · d0.0 r3.0 t122 | 3/3 ok 3 · d2.0 r5.0 t244 | 3/3 ok 3 · d2.0 r5.0 t214 |
| glm-4.5-air | off | 3/3 ok 3 · d0.0 r3.0 t67 | 0/3 ok 0 · d4.0 r4.7 t239 | 3/3 ok 3 · d0.0 r3.0 t96 | 3/3 ok 3 · d0.0 r3.0 t95 | 3/3 ok 3 · d0.0 r3.0 t92 | 2/3 ok 3 · d0.0 r3.0 t116 | 3/3 ok 3 · d2.0 r5.0 t125 | 3/3 ok 3 · d2.0 r5.0 t119 |
| glm-4.5-air | on | 3/3 ok 3 · d0.0 r3.0 t88 | 0/3 ok 0 · d5.3 r5.0 t149 | 3/3 ok 3 · d0.0 r3.0 t122 | 3/3 ok 3 · d0.0 r3.0 t124 | 3/3 ok 3 · d0.0 r3.0 t126 | 3/3 ok 3 · d0.0 r3.0 t124 | 3/3 ok 3 · d2.0 r5.0 t228 | 3/3 ok 3 · d2.0 r5.0 t255 |
| glm-4.5-flash | off | 3/3 ok 3 · d0.0 r3.0 t68 | 3/3 ok 3 · d0.0 r3.0 t68 | 3/3 ok 3 · d0.0 r3.0 t93 | 3/3 ok 3 · d0.0 r3.0 t102 | 3/3 ok 3 · d0.3 r3.7 t285 | 1/3 ok 3 · d0.0 r4.0 t232 | 2/3 ok 3 · d2.3 r5.0 t163 | 3/3 ok 3 · d2.0 r5.0 t155 |
| glm-4.5-flash | on | 3/3 ok 3 · d0.0 r3.0 t211 | 3/3 ok 3 · d0.0 r3.0 t218 | 3/3 ok 3 · d0.0 r3.0 t241 | 3/3 ok 3 · d0.0 r3.0 t245 | 2/3 ok 3 · d0.0 r3.7 t508 | 1/3 ok 3 · d0.0 r3.7 t404 | — | — |
| glm-4.5-x | off | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err |
| glm-4.5-x | on | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | — | — | — | — | — |
| glm-4.6 | off | 3/3 ok 3 · d0.0 r3.0 t68 | 0/3 ok 0 · d5.0 r5.0 t80 | 3/3 ok 3 · d0.0 r3.0 t91 | 3/3 ok 3 · d0.0 r3.0 t94 | 3/3 ok 3 · d0.0 r2.7 t98 | 3/3 ok 3 · d0.0 r3.0 t106 | 3/3 ok 3 · d2.0 r5.0 t128 | 3/3 ok 3 · d2.0 r5.0 t117 |
| glm-4.6 | on | 3/3 ok 3 · d0.0 r3.0 t98 | 0/3 ok 0 · d5.7 r5.0 t117 | 3/3 ok 3 · d0.0 r3.0 t109 | 3/3 ok 3 · d0.0 r3.0 t121 | 3/3 ok 3 · d0.0 r3.0 t124 | 3/3 ok 3 · d0.0 r3.0 t116 | 3/3 ok 3 · d2.0 r5.0 t209 | 3/3 ok 3 · d2.0 r5.0 t240 |
| glm-4.7 | off | 3/3 ok 3 · d0.0 r3.0 t68 | 0/3 ok 0 · d5.0 r5.0 t79 | 3/3 ok 3 · d0.0 r3.0 t93 | 3/3 ok 3 · d0.0 r3.0 t94 | 3/3 ok 3 · d0.0 r2.7 t98 | 3/3 ok 3 · d0.0 r3.0 t92 | 3/3 ok 3 · d2.0 r5.0 t125 | 3/3 ok 3 · d2.0 r5.0 t119 |
| glm-4.7 | on | 3/3 ok 3 · d0.0 r3.0 t101 | 0/3 ok 0 · d5.0 r5.0 t129 | 3/3 ok 3 · d0.0 r3.0 t121 | 3/3 ok 3 · d0.0 r3.0 t115 | 3/3 ok 3 · d0.0 r3.0 t137 | 3/3 ok 3 · d0.0 r3.0 t125 | 3/3 ok 3 · d2.0 r5.0 t225 | 3/3 ok 3 · d2.0 r5.0 t203 |
| glm-4.7-flash | off | 2/3 ok 2 · d0.0 r2.7 t49 · 1 err | 1/3 ok 3 · d0.0 r3.0 t44 · 2 err | 2/3 ok 2 · d0.0 r2.3 t55 · 1 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 2/3 ok 2 · d0.0 r3.7 t361 | 1/3 ok 3 · d0.0 r3.7 t189 · 1 err | 1/3 ok 2 · d3.0 r5.0 t135 | 2/3 ok 2 · d1.7 r4.0 t96 · 1 err |
| glm-4.7-flash | on | 3/3 ok 3 · d0.0 r3.0 t123 | 3/3 ok 3 · d0.0 r3.0 t164 | 3/3 ok 3 · d0.0 r3.0 t135 | 3/3 ok 3 · d0.0 r3.0 t161 | 3/3 ok 3 · d0.0 r3.7 t181 | 0/3 ok 1 · d0.0 r4.0 t358 · 1 err | 3/3 ok 3 · d2.0 r5.0 t328 | 0/3 ok 0 · d1.0 r2.7 t118 · 2 err |
| glm-4.7-flashx | off | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err |
| glm-4.7-flashx | on | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err |
| glm-5 | off | 3/3 ok 3 · d0.0 r3.0 t68 | 0/3 ok 0 · d5.0 r5.0 t69 | 3/3 ok 3 · d0.0 r3.0 t89 | 3/3 ok 3 · d0.0 r3.0 t94 | 3/3 ok 3 · d0.0 r2.3 t88 | 3/3 ok 3 · d0.0 r3.0 t85 | 3/3 ok 3 · d2.0 r5.0 t128 | 2/3 ok 3 · d2.0 r5.0 t119 |
| glm-5 | on | 3/3 ok 3 · d0.0 r3.0 t94 | 0/3 ok 0 · d5.0 r5.0 t105 | 3/3 ok 3 · d0.0 r3.0 t94 | 3/3 ok 3 · d0.0 r3.0 t100 | 3/3 ok 3 · d0.0 r2.3 t130 | 3/3 ok 3 · d0.0 r3.0 t108 | 3/3 ok 3 · d2.0 r5.0 t214 | 3/3 ok 3 · d2.0 r5.0 t219 |
| glm-5.1 | off | 3/3 ok 3 · d0.0 r3.0 t67 | 0/3 ok 0 · d5.0 r5.0 t69 | 3/3 ok 3 · d0.0 r3.0 t89 | 3/3 ok 3 · d0.0 r3.0 t93 | 3/3 ok 3 · d0.0 r3.0 t84 | 3/3 ok 3 · d0.0 r3.0 t87 | 3/3 ok 3 · d2.0 r5.0 t133 | 3/3 ok 3 · d2.0 r5.0 t131 |
| glm-5.1 | on | 3/3 ok 3 · d0.0 r3.0 t85 | 0/3 ok 0 · d5.0 r5.0 t71 | 3/3 ok 3 · d0.0 r3.0 t104 | 3/3 ok 3 · d0.0 r3.0 t115 | 3/3 ok 3 · d0.0 r3.0 t115 | 3/3 ok 3 · d0.0 r3.3 t183 | 3/3 ok 3 · d2.0 r5.0 t239 | 3/3 ok 3 · d2.0 r5.0 t240 |
| glm-5.2 | off | 3/3 ok 3 · d0.0 r3.0 t67 | 0/3 ok 0 · d5.0 r5.0 t76 | 3/3 ok 3 · d0.0 r3.0 t90 | 3/3 ok 3 · d0.0 r3.0 t95 | 3/3 ok 3 · d0.0 r3.0 t83 | 3/3 ok 3 · d0.0 r3.0 t88 | 3/3 ok 3 · d2.0 r5.0 t125 | 3/3 ok 3 · d2.0 r5.0 t131 |
| glm-5.2 | on | 3/3 ok 3 · d0.0 r3.0 t83 | 0/3 ok 0 · d5.0 r5.0 t117 | 3/3 ok 3 · d0.0 r3.0 t110 | 3/3 ok 3 · d0.0 r3.0 t113 | 3/3 ok 3 · d0.0 r3.0 t129 | 3/3 ok 3 · d0.0 r3.0 t123 | 3/3 ok 3 · d2.0 r5.0 t261 | 3/3 ok 3 · d2.0 r5.0 t248 |
| glm-5.3 | on | 3/3 ok 3 · d0.0 r3.0 t78 | 0/3 ok 0 · d5.0 r5.0 t84 | 3/3 ok 3 · d0.0 r3.0 t109 | 3/3 ok 3 · d0.0 r3.0 t111 | 3/3 ok 3 · d0.0 r3.3 t115 | 3/3 ok 3 · d0.0 r3.0 t116 | 3/3 ok 3 · d2.3 r5.0 t238 | 3/3 ok 3 · d2.0 r5.0 t217 |
| glm-5.3-flash | on | 3/3 ok 3 · d0.0 r3.0 t87 | 0/3 ok 0 · d5.7 r5.0 t187 | 3/3 ok 3 · d0.0 r3.0 t120 | 3/3 ok 3 · d0.0 r3.0 t122 | 3/3 ok 3 · d0.0 r3.0 t135 | 3/3 ok 3 · d0.0 r3.0 t109 | 3/3 ok 3 · d2.0 r5.0 t213 | 3/3 ok 3 · d2.0 r5.0 t213 |

## task `followup`

| model | thinking | append | undeclared | invoke | mcp | rhai_sig | rhai_schema | mcp+explore | mcp+list |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| glm-4.5 | off | 3/3 ok 3 · d0.0 r2.0 t44 | 0/3 ok 0 · d5.0 r5.0 t87 | 3/3 ok 3 · d0.0 r2.0 t51 | 3/3 ok 3 · d0.0 r2.0 t53 | 3/3 ok 3 · d0.0 r2.0 t44 | 3/3 ok 3 · d0.0 r2.0 t49 | 3/3 ok 3 · d0.0 r2.0 t55 | 3/3 ok 3 · d0.0 r2.0 t53 |
| glm-4.5 | on | 3/3 ok 3 · d0.0 r2.0 t58 | 0/3 ok 0 · d5.0 r5.0 t806 | 3/3 ok 3 · d0.0 r2.0 t62 | 3/3 ok 3 · d0.0 r2.0 t63 | 3/3 ok 3 · d0.0 r2.0 t56 | 3/3 ok 3 · d0.0 r2.0 t65 | 3/3 ok 3 · d0.0 r2.0 t83 | 3/3 ok 3 · d0.0 r2.0 t81 |
| glm-4.5-air | off | 3/3 ok 3 · d0.0 r2.0 t42 | 0/3 ok 0 · d5.0 r5.0 t86 | 3/3 ok 3 · d0.0 r2.0 t54 | 3/3 ok 3 · d0.0 r2.0 t56 | 3/3 ok 3 · d0.0 r2.0 t49 | 3/3 ok 3 · d0.0 r2.0 t48 | 3/3 ok 3 · d0.0 r2.0 t57 | 3/3 ok 3 · d0.0 r2.0 t54 |
| glm-4.5-air | on | 3/3 ok 3 · d0.0 r2.0 t61 | 0/3 ok 0 · d5.3 r5.0 t183 | 3/3 ok 3 · d0.0 r2.0 t66 | 3/3 ok 3 · d0.0 r2.0 t80 | 3/3 ok 3 · d0.0 r2.0 t63 | 3/3 ok 3 · d0.0 r2.0 t63 | 3/3 ok 3 · d0.0 r2.0 t64 | 3/3 ok 3 · d0.0 r2.0 t70 |
| glm-4.5-flash | off | 3/3 ok 3 · d0.0 r2.0 t48 | 3/3 ok 3 · d0.0 r2.0 t43 | 3/3 ok 3 · d0.0 r2.0 t65 | 3/3 ok 3 · d0.0 r2.0 t66 | 3/3 ok 3 · d0.0 r2.0 t49 | 3/3 ok 3 · d0.0 r2.0 t48 | 3/3 ok 3 · d0.0 r2.0 t60 | 3/3 ok 3 · d0.0 r2.0 t64 |
| glm-4.5-flash | on | 3/3 ok 3 · d0.0 r2.0 t116 | 3/3 ok 3 · d0.0 r2.0 t121 | 3/3 ok 3 · d0.0 r2.0 t131 | 3/3 ok 3 · d0.0 r2.0 t136 | 3/3 ok 3 · d0.0 r2.0 t120 | 3/3 ok 3 · d0.0 r2.0 t118 | — | — |
| glm-4.5-x | off | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err |
| glm-4.5-x | on | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | — | — | — | — | — |
| glm-4.6 | off | 3/3 ok 3 · d0.0 r2.0 t43 | 0/3 ok 0 · d5.0 r5.0 t69 | 3/3 ok 3 · d0.0 r2.0 t53 | 3/3 ok 3 · d0.0 r2.0 t55 | 3/3 ok 3 · d0.0 r2.0 t50 | 3/3 ok 3 · d0.0 r2.0 t50 | 3/3 ok 3 · d0.0 r2.0 t55 | 3/3 ok 3 · d0.0 r2.0 t53 |
| glm-4.6 | on | 3/3 ok 3 · d0.0 r2.0 t49 | 0/3 ok 0 · d5.0 r5.0 t127 | 3/3 ok 3 · d0.0 r2.0 t74 | 3/3 ok 3 · d0.0 r2.0 t67 | 3/3 ok 3 · d0.0 r2.0 t61 | 3/3 ok 3 · d0.0 r2.0 t64 | 3/3 ok 3 · d0.0 r2.0 t80 | 3/3 ok 3 · d0.0 r2.0 t89 |
| glm-4.7 | off | 3/3 ok 3 · d0.0 r2.0 t42 | 0/3 ok 0 · d5.0 r5.0 t78 | 3/3 ok 3 · d0.0 r2.0 t51 | 3/3 ok 3 · d0.0 r2.0 t55 | 3/3 ok 3 · d0.0 r2.0 t49 | 3/3 ok 3 · d0.0 r2.0 t47 | 3/3 ok 3 · d0.0 r2.0 t53 | 3/3 ok 3 · d0.0 r2.0 t55 |
| glm-4.7 | on | 3/3 ok 3 · d0.0 r2.0 t51 | 0/3 ok 0 · d6.3 r5.0 t184 | 3/3 ok 3 · d0.0 r2.0 t64 | 3/3 ok 3 · d0.0 r2.0 t73 | 3/3 ok 3 · d0.0 r2.0 t74 | 3/3 ok 3 · d0.0 r2.0 t64 | 3/3 ok 3 · d0.0 r2.0 t79 | 3/3 ok 3 · d0.0 r2.0 t72 |
| glm-4.7-flash | off | 3/3 ok 3 · d0.0 r2.0 t36 | 3/3 ok 3 · d0.0 r2.0 t38 | 3/3 ok 3 · d0.0 r2.0 t48 | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 3/3 ok 3 · d0.0 r2.0 t44 | 2/3 ok 2 · d0.0 r1.7 t29 · 1 err | 3/3 ok 3 · d0.0 r2.0 t51 | 3/3 ok 3 · d0.0 r2.0 t49 |
| glm-4.7-flash | on | 3/3 ok 3 · d0.0 r2.0 t96 | 3/3 ok 3 · d0.0 r2.0 t92 | 3/3 ok 3 · d0.0 r2.0 t129 | 3/3 ok 3 · d0.0 r2.0 t91 · 1 err | 3/3 ok 3 · d0.0 r2.0 t117 | 3/3 ok 3 · d0.0 r2.0 t115 | 3/3 ok 3 · d0.0 r2.0 t97 · 1 err | 3/3 ok 3 · d0.0 r2.0 t132 |
| glm-4.7-flashx | off | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err |
| glm-4.7-flashx | on | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err | 0/3 ok 0 · d0.0 r1.0 t0 · 3 err |
| glm-5 | off | 3/3 ok 3 · d0.0 r2.0 t43 | 0/3 ok 0 · d5.0 r5.0 t70 | 3/3 ok 3 · d0.0 r2.0 t53 | 3/3 ok 3 · d0.0 r2.0 t54 | 3/3 ok 3 · d0.0 r2.0 t50 | 3/3 ok 3 · d0.0 r2.0 t50 | 3/3 ok 3 · d0.0 r2.0 t56 | 3/3 ok 3 · d0.0 r2.0 t55 |
| glm-5 | on | 3/3 ok 3 · d0.0 r2.0 t48 | 0/3 ok 0 · d5.0 r5.0 t84 | 3/3 ok 3 · d0.0 r2.0 t52 | 3/3 ok 3 · d0.0 r2.0 t61 | 3/3 ok 3 · d0.0 r2.0 t54 | 3/3 ok 3 · d0.0 r2.0 t60 | 3/3 ok 3 · d0.0 r2.0 t67 | 3/3 ok 3 · d0.0 r2.0 t79 |
| glm-5.1 | off | 3/3 ok 3 · d0.0 r2.0 t45 | 0/3 ok 0 · d5.0 r5.0 t66 | 3/3 ok 3 · d0.0 r2.0 t54 | 3/3 ok 3 · d0.0 r2.0 t55 | 3/3 ok 3 · d0.0 r2.0 t49 | 3/3 ok 3 · d0.0 r2.0 t51 | 3/3 ok 3 · d0.0 r2.0 t56 | 3/3 ok 3 · d0.0 r2.0 t56 |
| glm-5.1 | on | 3/3 ok 3 · d0.0 r2.0 t66 | 0/3 ok 0 · d5.0 r5.0 t82 | 3/3 ok 3 · d0.0 r2.0 t66 | 3/3 ok 3 · d0.0 r2.0 t72 | 3/3 ok 3 · d0.0 r2.0 t60 | 3/3 ok 3 · d0.0 r2.0 t59 | 3/3 ok 3 · d0.0 r2.0 t54 | 3/3 ok 3 · d0.0 r2.0 t68 |
| glm-5.2 | off | 3/3 ok 3 · d0.0 r2.0 t39 | 0/3 ok 0 · d5.0 r5.0 t65 | 3/3 ok 3 · d0.0 r2.0 t53 | 3/3 ok 3 · d0.0 r2.0 t53 | 3/3 ok 3 · d0.0 r2.0 t48 | 3/3 ok 3 · d0.0 r2.0 t47 | 3/3 ok 3 · d0.0 r2.0 t54 | 3/3 ok 3 · d0.0 r2.0 t53 |
| glm-5.2 | on | 3/3 ok 3 · d0.0 r2.0 t54 | 0/3 ok 0 · d5.0 r5.0 t96 | 3/3 ok 3 · d0.0 r2.0 t71 | 3/3 ok 3 · d0.0 r2.0 t69 | 3/3 ok 3 · d0.0 r2.0 t61 | 3/3 ok 3 · d0.0 r2.0 t55 | 3/3 ok 3 · d0.0 r2.0 t61 | 3/3 ok 3 · d0.0 r2.0 t70 |
| glm-5.3 | on | 3/3 ok 3 · d0.0 r2.0 t60 | 0/3 ok 0 · d5.0 r5.0 t104 | 3/3 ok 3 · d0.0 r2.0 t70 | 3/3 ok 3 · d0.0 r2.0 t63 | 3/3 ok 3 · d0.0 r2.0 t59 | 3/3 ok 3 · d0.0 r2.0 t59 | 3/3 ok 3 · d0.0 r2.0 t77 | 3/3 ok 3 · d0.0 r2.0 t68 |
| glm-5.3-flash | on | 3/3 ok 3 · d0.0 r2.0 t55 | 0/3 ok 0 · d5.3 r5.0 t135 | 3/3 ok 3 · d0.0 r2.0 t61 | 3/3 ok 3 · d0.0 r2.0 t69 | 3/3 ok 3 · d0.0 r2.0 t59 | 3/3 ok 3 · d0.0 r2.0 t56 | 3/3 ok 3 · d0.0 r2.0 t75 | 3/3 ok 3 · d0.0 r2.0 t89 |
