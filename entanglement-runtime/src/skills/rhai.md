---
name: rhai
description: >-
  Long-form guide to the `rhai` tool: worked examples plus the detail behind the
  signature list the tool spec already carries — permission grading, escape-root
  gating, running a script file by `path` with `args`, `http()`, the
  `mcp::`/`skill::` namespaces, chaining calls, background runs, output and
  limits. Load it when a script needs more than the signatures.
---

# `rhai` guide

The tool spec itself carries the full signature list — this is the detail behind
it, plus worked examples.

Rhai (https://rhai.rs) syntax resembles Rust (`fn`, `let`) but is **not**
Rust: no `use`/crates/`std`. Rhai's own stdlib (strings, arrays, maps, math,
loops) is built in, no `import`. `import`/`eval` are disabled — a script
cannot pull in modules or re-enter the parser.

## `script` or `path`, and `args`

Pass exactly one of `script` (inline source) or `path` (a `.rhai` file —
project-relative or absolute). With `path` the runtime reads and runs the
file; you never need to `read` it first. Launching a script is
permission-checked like any other tool call, on its `path` (an inline
script only by a plain `rhai` rule): denied, it fails with an error you can
read; allowed, it runs and each call inside it is checked as if you made it
directly; otherwise the user sees it once and runs it whole, has each
command asked as it comes, or rejects it. A script in a folder the user
trusts (their `rhai.trusted` config) runs **without asking**, every call in
it allowed except what the user's rules explicitly deny. Writing or editing
a `.rhai` file inside a trusted folder, or a skill's `SKILL.md`/`lib.rhai`,
always asks the user first.

`args` (an object) is available in the script as the constant map `args`,
for both kinds — `#{}` when you pass none. Read it, don't assign to it
(that throws), and pass values into your own functions explicitly — a
function body cannot see `args`:

```json
{"path": "scripts/report.rhai", "args": {"days": 7, "author": "me"}}
```

```rhai
// scripts/report.rhai
fn since(days) { `--since=${days}.days` }
exec("git", ["log", since(args.days), `--author=${args.author}`, "--oneline"])
```

A skill's own scripts run the same way: `load_skill` prints the skill's
`skill_dir` and lists its files, so `{"path": "<skill_dir>/check.rhai"}`.

## Host I/O bindings

Each binding call goes through **the same dispatch path as the equivalent
tool call** (the session's permission mode, its tool overlay, the config
ceiling, grants, `pre_tool_use`/`post_tool_use` hooks, the escape-root gate
for an out-of-project path, argument validation, `FileChange` events) and
returns that tool's text output. A refusal (denied, rejected, vetoed by a
hook) **throws** — wrap in `try`/`catch` if the script should degrade
gracefully instead of aborting. An approval covers a repeat of the exact same
call in the same run; any other target asks again.

- `read(path)`, `read(path, offset, limit)` — returns `"{lineno}: {line}"`
  text, **not** parseable as JSON/YAML.
- `read_raw(path)` — exact file content, no line-number prefix; use this
  before `parse_json`/`parse_yaml`. Graded as an alias of `read`,
  not a distinct permission surface.
- `glob(pattern)`
- `grep(pattern)`, `grep(pattern, path)`
- `glob_json(pattern)`, `glob_json(pattern, exclude)` — the same walk as
  `glob`, but structured: returns `#{files: […], notices: […]}` (an array of
  path strings + diagnostics) instead of newline-joined text (ADR-0206).
  Prefer it over `glob` whenever the script consumes the result rather than
  echoing it.
- `grep_json(pattern)`, `grep_json(pattern, path)` — the same scan as
  `grep`, structured: returns `#{matches: […], notices: […]}` where each
  match is `#{path: …, lineno: …, line: …}` (ADR-0206).
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
tool's much longer default; every other nested call except `write`/`edit`
is cut off shortly before the script's budget runs out — the cut throws, and
its outcome (did it take effect?) is unknown.

## `http(method, url, #{headers, body})`

`http("GET", url)` / `http("POST", url, #{headers: #{"Accept": "application/json"}, body: #{q: 1}})`
returns the response body as text; a non-2xx status **throws** (the message
carries the status and body). A map/array `body` is sent as JSON with
`Content-Type: application/json` (unless you set one). Methods:
GET, HEAD, POST, PUT, PATCH, DELETE. Private and loopback addresses are
refused, like `web_fetch`.

Graded as `http` with the argument `"{METHOD} {url}"`, so a rule can allow
reads only: `http(GET https://api.github.com/*)`. Every redirect hop is graded
the same way and stops the request if it would need approval.

Credentials are never the script's business: the user maps hosts to headers
or cookies in their own config (`http_auth:`), and the runtime adds them at
send time over https — never visible to the script or in the environment of
a process it starts, never sent to another host on a redirect. Setting
`Authorization`/`Cookie`/`Host`/`X-Forwarded-Host` yourself, or putting
`user@` in the URL, is refused. The
first authenticated call to a host in a session always asks the user (the
card says `authenticated for <host>`).

## Namespaces: `mcp::<server>::<tool>` and `skill::<skill>::<fn>`

Every MCP tool the session can see is callable as
`mcp::<server>::<tool>(#{arg: value})` (or `()` with no arguments), graded as
the `mcp__<server>__<tool>` tool it is; its text result comes back (call
`.parse_json()` on JSON) and a failure throws. Names are mapped to
identifiers: other characters become `_`, a name that does not start with a
letter (after any leading `_`) gets a `t_` prefix (`2fa` → `t_2fa`), a
reserved word (`call`, `print`, `type_of`, …) a `_` suffix — `mcp_tools`
shows each tool's rhai name. A server not yet connected connects on the
first call.

A skill that ships a `lib.rhai` next to its `SKILL.md` adds its functions as
`skill::<skill>::<fn>(...)`; `load_skill` lists their signatures with the
`///` doc comments written above them. A lib function can call every binding
and namespace above.

Nested `rhai`, the `mcp` envelope and the session tools (`agent`,
`agent_send`, `poll`, `ask_user`, `propose_plan`, `request_mode`,
`update_tasks`, `list_*`, `load_skill`, `mcp_tools`) cannot be called from a
script.

## Chain calls in one script

The point of a script is to do several calls in **one** round: pass one
call's result straight into the next instead of returning it to yourself.

```rhai
// search, then read the best hit — one tool call, not two rounds
let hits = mcp::docs::search(#{query: "retry policy"}).parse_json();
let page = mcp::docs::read(#{id: hits[0].id});
page
```

```rhai
// find the files, then edit each
for f in glob_json("src/**/*.rs").files {
    if read_raw(f).contains("old_name(") {
        edit(f, "old_name(", "new_name(", true);
    }
}
"renamed"
```

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
// `glob` returns newline-joined text (looping over it walks characters);
// `glob_json(...).files` is an array of path strings.
let out = "";
for f in glob_json("*.rs").files {
    if f.contains("test") {
        out += f + "\n";
    }
}
out
```
