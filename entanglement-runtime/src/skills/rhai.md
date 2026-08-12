---
name: rhai
description: >-
  Binding reference for the `rhai` tool: every host function it exposes
  (read/glob/grep/edit/write, exec/bash, parse_json/to_json/parse_yaml/to_yaml),
  their signatures, permission grading, and a worked example. Load this before
  writing a Rhai script — binding names are not guessable and a wrong one
  throws.
---

# `rhai` binding reference

Rhai (https://rhai.rs) syntax resembles Rust (`fn`, `let`) but is **not**
Rust: no `use`/crates/`std`. Rhai's own stdlib (strings, arrays, maps, math,
loops) is built in, no `import`. `import`/`eval` are disabled — a script
cannot pull in modules or re-enter the parser.

## Host I/O bindings

Each binding passes the **same permission check as the equivalent tool
call** (mask, agent/skill/config chain, escape-root gate for an
out-of-project path) and returns that tool's text output. Denial or failure
**throws** — wrap in `try`/`catch` if the script should degrade gracefully
instead of aborting.

- `read(path)`, `read(path, offset, limit)` — returns `"{lineno}: {line}"`
  text, **not** parseable as JSON/YAML.
- `read_raw(path)` — exact file content, no line-number prefix; use this
  before `parse_json`/`parse_yaml`. Graded and masked as an alias of `read`,
  not a distinct permission surface.
- `glob(pattern)`
- `grep(pattern)`, `grep(pattern, path)`
- `edit(path, old, new)`, `edit(path, old, new, replace_all)`
- `write(path, content)`
- `exec(command)`, `exec(command, args)`, `exec(command, args, workdir)` —
  argv exec, no shell, graded as the `call` tool (named `exec` because
  `call` is a reserved Rhai keyword).
- `bash(command)`, `bash(command, workdir)` — `sh -c`, only when the host has
  `bash` registered/enabled; otherwise an unknown-function error, not a
  graded-then-refused binding.

`workdir` (on `exec`/`bash`) is what a workdir-scoped permission rule
(`tool{pattern}`) matches, and also feeds the escape-root gate. `exec`/`bash`
timeouts are clamped to the script's own remaining wall-clock budget, not the
tool's much longer default.

All bindings are callable as methods, e.g. `read_raw(path).parse_json()`.

## Pure converters

No IO, no permission check:

- `parse_json(str)`, `to_json(v)`
- `parse_yaml(str)`, `to_yaml(v)`

`parse_*` throws on invalid input (usable as a validator). JSON/YAML `null`
becomes `()`. An integer outside the `i64` range silently widens to an
approximate float — put large IDs in JSON as strings if exact round-tripping
matters.

## Output and limits

The last expression's value is returned (serialized); `print(...)` output is
captured alongside it. Bounded by `max_operations`, string/array/map size
caps, and a wall-clock timeout (`timeout` argument, default 5s, max 30s) — a
runaway script terminates deterministically, never an OOM.

## Background scripts

`background: true` returns an `x-` handle immediately instead of the result;
join with `poll`, which drains `print` output incrementally and, once the
script finishes, its final `=> value` (or error) line. A background script
gets a longer budget (`timeout` default 120s, max 600s). `poll kill=true`
requests a **cooperative** stop: the script ends at its next operation, so an
in-flight `exec`/`bash` call finishes its own (budget-clamped) timeout first.
Bindings grade exactly as in a blocking run — an `Ask`-graded binding still
prompts for approval mid-run, and the deadline keeps counting while it waits.
Use `background` for long multi-step scripts; prefer the default blocking form
for anything that fits the 30s cap — it returns the result in one round-trip.

## Worked example

```rhai
let cfg = read_raw("config.json").parse_json();
let out = "";
for f in glob("*.rs") {
    if f.contains("test") {
        out += f + "\n";
    }
}
out
```
