# 20. Script-facing structured search bindings: `glob_json` / `grep_json`

Date: 2026-09-16

## Status

Accepted (amends [ADR-0046](0046-rhai-sandboxed-script-tool.md); extends
[ADR-0098](0098-rhai-json-yaml-loader-and-read-raw.md)'s principle to the
search pair).

## Context

A `rhai` script binding is only as good as the value it hands the script.
Since [ADR-0115](0115-rhai-exec-bindings-call-bash.md) the `glob` and `grep`
bindings have delegated to their host tools and returned the tool's text
output verbatim. That text is *model-facing prose*: newline-joined paths,
zero-match explanation sentences, bracketed skip/cap notices. For the model
reading a tool result that shape is right ([ADR-0150](0150-search-tool-cli-ergonomics.md)).
For a script it is a trap:

- Rhai's string indexing yields **single characters**, and `.len()` a char
  count — `for f in glob("*.rs")` iterates characters, not paths. The
  engine's stdlib `split`/`lines` are registered (the standard package is
  loaded), but a script that guesses the wrong shape gets a wrong-but-not-
  error result, the worst failure mode there is.
- The shipped `rhai` skill's own worked example (`for f in glob("*.rs")`)
  had this bug: it only *looked* plausible because the string happened to
  contain path characters.
- `grep`'s `path:lineno:line` text forces every consumer to re-parse what
  the tool already structured internally (`FileList`, `MatchRecord`).

The in-repo precedent is [`read_raw`](0098-rhai-json-yaml-loader-and-read-raw.md):
`read`'s `{lineno}: {line}` model-facing format is unusable from a script, so
a registered-but-unadvertised raw variant exists, graded and masked as an
alias of `read`. The search pair had the same disease.

## Decision

1. **Two script-facing tools, `glob_json` and `grep_json`** —
   registered into the same `ToolRegistry` the rhai bridge executes
   against, *after* the specs snapshot so they are never advertised to the
   model, exactly like `read_raw`. Same input surface as their
   model-facing counterparts (plus the `exclude` list for `glob_json`,
   `path`/`case_insensitive` for `grep_json`); same walk and scan — the
   scan loop (`scan_matches`), regex compilation (`compile_regex`), and
   walk enumeration are now *shared* between prose and JSON variants, so
   behavioral parity is by construction, not by mirrored code.
2. **Output is a JSON document**, not prose:
   `glob_json` → `{"files": ["rel/path", …], "notices": […]}`;
   `grep_json` → `{"matches": [{"path": …, "lineno": …, "line": …}, …],
   "notices": […]}`. The binding layer parses it into Rhai values via the
   serde bridge, so `glob_json("*.rs").files` is a real array and
   `grep_json("x").matches[0]["lineno"]` a real integer.
3. **Zero-match stays explained** ([ADR-0150](0150-search-tool-cli-ergonomics.md)
   semantics preserved): the array is empty and the *why* rides in
   `notices` — "matched no files", "N files scanned, none matched",
   "matched only directories — try `X/*`", cap hits, skipped-binary/
   too-large files. A script that ignores `notices` loses diagnostics, not
   data.
4. **Grading and masking alias the prose tools.**
   `BindingPolicy::decide` maps `glob_json` → `glob` and `grep_json` →
   `grep` (the existing `read_raw` → `read` mechanism, now factored as
   `graded_name`): a structured-output escape hatch must not be a
   permission escape hatch. `escape_root_target` is unchanged — search
   bindings never force an approval ([ADR-0132](0132-glob-grep-escape-root-search-via-durable-grant.md)
   semantics ride through the shared walk).
5. **The prose bindings stay.** `glob`/`grep` bindings remain for scripts
   that echo text at a human; the spec's binding reference now documents
   both shapes and steers scripts to the structured pair when they consume
   the result.
6. **Budget, not byte-cut.** The prose tools can be truncated mid-line by
   the output cap and stay readable; a JSON document cut mid-string is
   unparseable. The JSON tools therefore *budget*: an over-cap result set
   is trimmed from the tail with a `[output truncated: first N of M
   results]` notice, and the document always arrives parseable. The
   binding treats an unparseable reply as a catchable binding error.

## Consequences

- Scripts iterate arrays, not characters. The skill's worked example now
  uses `glob_json(...).files` and says why.
- The spec's `BINDING_REFERENCE` (pinned to the registration code by
  `spec_description_lists_every_registered_binding`) documents both pairs;
  a wrong-guess error still self-corrects via the appended catalogue.
- `KNOWN_TOOL_NAMES` lists the new names so a stale-config check doesn't
  flag them, but they never appear in an advertised roster or
  `TOOL_SEARCH_KERNEL`.
- Registry bloat is accepted: two more unadvertised tools with tiny
  surface, mirroring `read_raw`'s cost/benefit verdict.
- A future `apply_patch`-shaped or `read`-shaped script need (raw ranges,
  image decode) should follow this same pattern: registered variant +
   policy alias + structured payload, never prose for scripts.
