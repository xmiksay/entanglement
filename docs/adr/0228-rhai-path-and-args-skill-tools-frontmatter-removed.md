# 0228. `rhai` gains `path` and `args`; skill `tools:` frontmatter is removed; `load_skill` lists documented `lib.rhai` signatures

- Status: Accepted
- Date: 2026-10-13
- Issue: #820 (part 3 of epic #822; follows [ADR-0226](0226-one-advertised-surface-mcp-envelope-registered-is-available.md)
  and [ADR-0227](0227-one-binding-path-namespaces-http-skill-libraries.md))
- Amends: [ADR-0046](0046-rhai-sandboxed-script-tool.md) (the tool's input
  schema: `script` XOR `path`, plus `args`; how a launch is gated),
  [ADR-0227](0227-one-binding-path-namespaces-http-skill-libraries.md) §4
  (a project-layer `rhai.trusted` folder unlocks credentialed `http()` for
  its scripts) and §5 (`load_skill` lists doc comments, not just
  signatures), [ADR-0109](0109-escape-root-access-via-approval.md) (`rhai`'s
  script path joins the escape-root targets; an escape card that also
  cleared a graded `Ask` records both grants, every tool),
  [ADR-0185](0185-rhai-joins-background-and-poll.md) (a background run may
  name a `path`), [ADR-0037](0037-load-skill-tool-deterministic-resolution.md)
  (the `load_skill` result shape), [ADR-0002](0002-session-multiplexed-protocol.md)'s
  `InMsg::Approve` (an additive `run_all` field),
  [ADR-0209](0209-auto-mode-defaults-to-prompt-not-deny.md) (`auto`'s
  default grade becomes `allow`),
  [ADR-0207](0207-permission-modes-replace-agent-borne-authority.md) §4 (a
  rule naming the tool outranks a capability-class rule) and §11 (`auto`'s
  default grade, with ADR-0209)
- Supersedes: the #560 P8 skill-tool kinds — the `SKILL.md` `tools:`
  frontmatter (`rhai` preset-script and `alias` kinds; the `endpoint` kind
  went in ADR-0227) introduced as ADR-0193's Phase-8 surface and kept
  strict-layers-only by [ADR-0194](0194-skills-are-additive-only.md) — and
  ADR-0227 §6's "a native `kind: endpoint` entry is skipped at registration"
  (the whole key goes)

## Context

After ADR-0227 a skill extends what a script can call through `lib.rhai`,
but two older mechanisms still sat beside it. A `SKILL.md` `tools:` list
registered `skill__<skill>__<tool>` entries — a `rhai` kind (a preset script
read at discovery, rewritten into a `rhai` call before grading) and an
`alias` kind (a renamed/preset-args wrapper over any tool) — both built on
`AliasTool`'s pre-grading rewrite. ADR-0226 had to append them to the
otherwise fixed kernel surface as an interim exception, since nothing else
reached them. Every one of them is a second way to say something `lib.rhai`
or a script file already says, with its own advertisement exception, its own
rewrite step in the dispatch ladder, and its own schema arithmetic
(`schema_minus_preset`).

What a preset-script tool really offered was "run this predefined script with
these values" — and `rhai` could not do that: it took inline source only, so
the model had to read a script file and paste it back, and a value had to be
spliced into the source text.

A predefined script also needs a trust story, and a launch needs a grade.
The review rounds of 2026-10-13 tried two extremes — grading the launch as
`rhai(<pattern>)` rules plus grants only, then a folder-only trust model
that took the launch out of grading entirely (no rules, no grants, no
overlay). The user's final decision (#820, comment of 2026-10-13) keeps
both halves: **the launch is graded like any other tool** — mode rules on
its path, the ceiling, the overlay, grants, hooks — and a `rhai.trusted`
folder is the one place where a vetted script runs without asking. One
rule set grades a call whether the model or a script makes it, and `auto`
becomes "run what is not denied".

#826 showed that the exact signatures in a `load_skill` reply are
load-bearing for how well a model calls a lib function; a bare
`skill::git::log(n)` says nothing about what `n` is.

## Decision

### 1. `rhai` takes `script` XOR `path`, plus `args`, `timeout`, `background`

- **`script` XOR `path`.** `path` names a `.rhai` file anywhere the session
  can read — project-relative (rewritten onto the session workdir like every
  path argument, ADR-0224) or absolute. The **runtime reads it; the model
  never does**. The source is read once, after the launch is graded (a deny or
  a `pre_tool_use` veto refuses before the file is opened) and before the
  trust decision and the card; that exact text is shown on the card and
  evaluated — no second read, so what the user approved is what runs, even if
  the file changes afterwards. The read canonicalizes the path once, opens
  that canonical path (non-blocking, so a FIFO cannot hang the launch; a
  non-regular file is refused), refuses if the opened file's `(dev, ino)` is
  not the canonical path's, and reads from that descriptor; on Linux the
  canonical path is then re-derived from the descriptor (`/proc/self/fd`),
  closing an intermediate-directory swap between the canonicalize and the open
  (elsewhere that narrow window remains). Trust is judged on that canonical
  path, so the trust decision and the text can never come from two different
  files, and it must be the file the grade was computed on (a swap in between
  is refused). **Only `.rhai` files run by `path`** (extension, any case): any
  other `path` is declined at validation, and a canonical target without the
  extension (a `x.rhai` symlink to `x.txt`) is refused at the read without
  naming the target — trust is for scripts, so a non-script file in a trusted
  folder can never run trusted. A read failure still passes through the card,
  so an out-of-root path's existence is not disclosed before approval. The
  read runs off the async runtime and is capped at the engine's string cap
  (256 KiB) on the bytes actually read. The card quotes the model-supplied
  path, so a newline in it cannot forge a card line; an inline script's card
  opens with the runtime's own `// inline script` line, so script text cannot
  pose as a path card (whose approval a head may offer as a path grant).
- **Trusted folders.** `config.yml` `rhai: { trusted: [<dir>, …] }`, in the
  user **and** the project layer — the lists add up (project entries relative
  to the project root, user entries to the user config file's folder, `~`
  expanded), each canonicalized at load (ADR-0054), read at startup only. A
  `path` whose canonical file lies inside one runs with **no card**, and the
  whole run is **force-allowed** (Run all): every nested `Ask` passes, the
  escape-root gate passes (a single-use token the host tool spends), the
  credentialed `http()` per-host approval and its redirect hops pass. Explicit
  denies still win, as a catchable exception: a deny of the launch itself
  (mode rule, ceiling, overlay — trusted paths included), and for a nested
  call a mode `deny` rule or the ceiling, an overlay deny (`/disable <tool>`),
  a `pre_tool_use` veto. A symlink inside a trusted folder pointing out of it
  is judged by its target, so it is not trusted. **User-layer trust yields to
  the project's prompts**: when the project config gives the session's mode
  `prompt:` rules — or its `permissions:` ceiling a `prompt` list or `default:
  prompt`, which count for every mode — a user-layer folder is not trusted in
  that mode — for the whole script, which is then graded and carded like any
  other (the repository asked to be consulted there; a folder of the user's
  must not wave its rules through). Project-layer trust always applies; a
  folder both layers list is the project's. `skutter inspect config` names the
  suppressed modes. **This deliberately amends ADR-0227 §4**: a repository can
  now unlock credentialed calls for the scripts in a folder its own
  `.entanglement/config.yml` trusts. The project layer is trusted code already
  (ADR-0047); trusting its `scripts/` is the point of the feature, and the
  template documents that whoever can write to a trusted folder runs code with
  the user's credentials.
- **Writing trusted code asks** (user decision, 2026-10-13 review). The
  agent itself must not author trusted code unseen: a `write`/`edit`/
  `apply_patch` whose target is a `*.rhai` file inside a trusted folder, or
  a skill's `SKILL.md`/`lib.rhai` inside any user or project skill layer dir
  (`.entanglement/skills`, `.claude/skills`, `.agents/skills`, the user
  dirs or the `ENTANGLEMENT_SKILLS_DIR` override — `skills::skill_dirs`;
  canonical paths, a not-yet-existing file by its nearest existing
  ancestor) grades at least `Ask` — never auto-allowed by a mode rule, a
  grant or an overlay enable, and no wider grant is recorded for it; an
  explicit `Deny` stays `Deny`, and unattended the `Ask` collapses per the
  mode. The floor holds **inside a Run-all script** too — that write gets
  its own nested card, so approving a script once never lets it author
  trusted code — while a **trusted** script's own writes pass (they are the
  folder owner's). It applies in **every mode**, `auto` included (user
  decision): unattended, the write collapses to a refusal and an
  identical repeat parks up to `question_timeout`, then refuses.
  Other files in a trusted folder grade normally — they can never run by
  `path`. An escape-root card that parks a floored write names both
  reasons. A `bash` command line cannot be parsed reliably, and an MCP
  filesystem server's writes are not host-tool writes, so neither is
  covered: a trusted folder the agent can reach that way is effectively an
  auto-allow for whatever it writes there. Trusted folders should therefore
  not be agent-writable; project-layer entries may still name any folder
  (no root restriction) — that is the repository owner's call, documented
  in the template.
- **The launch is graded like any tool** (final user decision). Overlay
  deny, then the overlay enable entry or the mode's rules, the ceiling and
  grants — the same `Dispatcher::grade` every call climbs — then the
  `pre_tool_use` veto. The rule/grant argument is the script's **canonical
  `path`**, root-relative inside the root and absolute outside it
  (`permission_arg` + ADR-0125's `rooted_arg`, which canonicalizes for
  `rhai` alone so a link in `scripts/deploy/` grades as where it points;
  the launch rewrites its input's `path` to that form before grading, so a
  resolver with no root of its own — no escape-root policy wired — matches
  the same string, against the launch's project root), so
  `rhai(scripts/deploy/*)` works; an inline script has no argument and
  matches only a bare `rhai` rule (or a class rule).
  - **Deny** → a catchable permission error, trusted paths included.
  - **Trusted** (and not denied) → no card, Run all.
  - **Allow** → no card, the script runs **Ask per command**.
  - **Ask** → **one launch card** (`ApprovalKind::Rhai`, tool `rhai`, the
    source as its body): **Run all** (force-allow this run exactly like a
    trusted script), **Ask per command** (nested calls graded exactly like
    direct calls), **Reject** — plus the usual scope: a `Session`/`Always`
    approval records a grant for the **exact path**, so the next launch
    grades Allow (Ask per command, no card). An inline script's approval is
    once only (nothing to key a grant on). `run_all` is never persisted: it
    is this run's posture only.
  - A non-trusted path outside the root adds the escape-root warning to the
    same card — never a second prompt — and an Allow grade then asks too; a
    wider approval also records the escape grant.
- **Built-in grades.** `build` asks (`prompt: [rhai]`), so an inline script
  in `build` shows its card; `research` and `plan` deny it (their class
  `write` deny covers a read+write+exec script) and **cannot be tuned to
  run scripts**: a `modes:` rule granting `rhai` there is a load error
  (`tune.rs`'s class-deny guard, ADR-0207 §5) — switch mode instead; `auto`
  allows it.
  Making `build` ask needed a rule-precedence change (amends ADR-0207 §4):
  **a rule naming the tool outranks a capability-class rule; a `*` rule
  competes with a named rule by key length and with a class rule by
  strictness (the stricter wins); within each kind the longest key wins;
  ties go to the stricter grade.** Named over class: before, `write` (5
  chars) beat a bare `rhai` (4) by length alone, so no mode could ask for
  scripts while allowing writes. `*` vs class by strictness: ADR-0207 §5's
  tuning guard cannot see what a wildcard reaches, so a long `*(…)` allow
  must not out-rank a class deny. `*` vs named keeps length, so
  `deny: ["*(*credentials*)"]` still beats `bash(cat*)`. A key spelling a
  tool's own name counts as naming it even when it is also a class (`read`
  for the `read` tool), so `deny: ["*"]` + `allow: [read]` still runs
  `read`; because that lets a scoped `*` rule out-length `deny: [write]` for
  the `write` tool, the tuning guard refuses a scoped `*` allow/prompt in a
  mode class-denying a class some tool is named after. No built-in mode
  changes grade for any other tool. The `permissions:` ceiling grades
  through the same engine (`Mode::from_permission_profile`), so the
  precedence holds there too: a ceiling `deny: [write]` + `allow: [edit]`
  lets `edit` through. The ceiling has no tuning guard, so a ceiling
  `deny: [write]` + `allow: ["*(scripts/*)"]` still lets the `write` tool
  through on `scripts/*` (as before this ADR) while `edit` is denied.
- **`auto` → `default: allow`** (amends ADR-0209 and ADR-0207 §11). `auto`
  lets everything run that its `deny` list does not refuse — direct calls
  and a script's nested calls alike, a launch included. What still asks
  there: its own `prompt` list — destructive families that must not run
  silently (`gh`/`glab` `… delete`, `cargo yank`, `npm unpublish`,
  `docker push`, recursive `rm` spellings; a user decision) — what
  no rule grades (the escape-root gate and a credentialed `http()` call's
  per-host approval; Run all and trust pass both), the write floor, and a
  user's own `prompt:` rule. Unattended, a `prompt` rule's call collapses
  to a refusal and parks up to `question_timeout` on a repeat; an escape
  card parks up to `question_timeout`, then refuses. Exec stops being an
  explicit command list in `auto`; its `deny` list is what an unattended
  run refuses outright with no escalation.
- **No answerer.** Interactive heads (`tui`/`pipe`/`serve`) wait for a
  client; only a mode with `question_timeout` (`auto`) times out. A launch
  card that times out runs the script **Ask per command** (silence never
  approves an escape-root launch — that one is refused); a nested `Ask`
  follows the mode's unattended collapse (a first one refused, a repeat
  parked and timed) — a catchable error, never a hang. `skutter run` starts
  a launch card at once as Ask per command (an out-of-root launch is
  refused) and refuses every nested card at once. With no escape-root
  policy wired (an embedder's choice), every out-of-root path is refused
  outright, trusted or not.
- **Overlay deny of `rhai`** (`/disable rhai`, the `/tools` dialog, a wire
  `SetToolOverlay`) is an ordinary deny of the launch; there is no special
  refusal. Nested calls are graded through the dispatch seam; a trusted /
  Run all script's in-call grader passes an `Ask`-graded redirect hop of its
  `http()` call the same way (a `Deny` hop still stops).
- **Protocol.** `InMsg::Approve` gains `run_all: bool` — serde-defaulted and
  omitted when false, so an older head's frame parses and means **Ask per
  command**, the least-privileged answer. It is meaningful only on a `rhai`
  launch card and is wire-allowed like the rest of `Approve`. The TUI offers
  `[r]` run all (this run) and `[y]`/`[s]`/`[a]` ask per command with that
  grant scope on that card; `pipe`/`serve` pass the field through.
- **Escape + `Ask`, every tool.** When an escape-root card also cleared a
  graded `Ask` (a direct call, or a script's nested call), a
  `Session`/`Always` approval records the escape grant *and* the grade's
  grant — otherwise the next identical call would clear the escape and ask
  again for the grade alone.
- **`args`** (an object) is pushed into the run's scope as the **constant**
  `args` for both run kinds — `#{}` when absent — so `args.days` reads it and
  `args = …`/`args.days = …` throws. The script is compiled without that
  scope: given it, Rhai's optimizer inlines the constant and then panics on a
  property assignment instead of refusing it. Functions defined in the script
  (and lib functions) do not see `args` — Rhai functions are pure — so the
  value is passed in. One Rhai quirk remains: indexing *into* a string value
  (`args.s[0] = 'Z'`) mutates it in place despite the constant. Accepted:
  it changes only the run's own copy of the caller's input.
- **Pre-dispatch validation.** XOR, `path` non-empty, `args` an object,
  `timeout` a whole number, `background` a bool, no unknown key (a misspelled
  `arguments` would otherwise vanish) — checked before the launch, so an
  ill-formed call is declined with the fix and never reaches a card.
- **`background`/`poll`** (ADR-0185) is unchanged; a `path` run's operation
  label is `rhai: <path>`, and the launch (card and file read) runs before
  the detach.
- **The kernel spec** documents both kinds, `args` and the launch model
  (graded like any tool on its `path`; trusted scripts run without asking);
  `required` is empty (the XOR is not expressible in every wire's schema
  dialect). The spec is compiled in, so the advertised array stays byte-stable
  within a session.

### 2. Skill `tools:` frontmatter removed

`skills::tools`, `AliasTool`, `schema_minus_preset`, `register_skill_tools`,
`SkillMeta::tools`, the `Tool::alias_rewrite` hook and the dispatch ladder's
alias-rewrite step are deleted, and with them ADR-0226's interim `skill__*`
advertisement. The replacements:

| was | now |
| --- | --- |
| `kind: rhai` (preset script) | keep the script in the skill dir; `rhai {"path": "<skill_dir>/x.rhai", "args": {…}}` (`load_skill` prints `skill_dir`, and lists the file among the refs) |
| `kind: alias` (rename / preset args) | a `lib.rhai` function calling the tool's binding with the preset values |
| `kind: endpoint` | a `lib.rhai` function calling `http()` (ADR-0227) |

**A `tools:` key in a native-layer `SKILL.md` rejects that skill** — by key
presence, so an empty `tools:` or `tools: ~` counts — logged at error level
with the replacement named and listed under "rejected" by `skutter inspect
skills` — but it never aborts the registry
load: the other skills (in every layer) still load, and so does the engine.
ADR-0213/0227's rule is that a retired knob never bricks startup, and a repo
carrying an old `SKILL.md` would otherwise break for everyone who opens it.
Skipping only the skill keeps it loud (the skill is visibly gone, the log
says why and what to write instead) without that failure mode.

**Foreign (cross-vendor) skill dirs** keep ignoring unknown keys (ADR-0074):
a `tools:` there is dropped like Claude's `allowed-tools`, as it always was.

A config `modes:` rule naming a `skill__*` tool is now reported by the stale
rule-name check at load, like any other unknown tool name.

### 3. `load_skill` and `list_skills`

`load_skill` returns, under the `skill_id`/`skill_dir` header: the
path-substituted body, then the lib's **public function signatures each
followed by its doc comment** (`///` lines or a `/** … */` block directly
above the `fn`, comment leaders stripped, indented below the signature), then
the `available_refs` listing (which now says a `.rhai` file runs via `rhai`
`path`). A lib whose `skill::` segment collides keeps ADR-0227's "no rhai
name (collides with …)" line instead of signatures. `list_skills` is
unchanged: name + description, `user_only` skills withheld.

Doc comments are read with a small **text pass** over the lib source
(`skills::lib_docs`): the contiguous `///` lines or the one `/** … */` block
directly above each public `fn name(`, overloads told apart by parameter
count; only a block's `*` margin is stripped, never a `*` in the text. Not
Rhai's `metadata` feature, which would expose them through
`AST::iter_functions`: it is crate-wide and makes the parser *reject* any
`///` or `/** */` not directly followed by a `fn`, breaking ordinary inline
scripts, script files and libs with a header comment.

## Consequences

- One way for a skill to ship callable behavior (`lib.rhai`) and one way to
  ship a predefined script (a file run by `path`); the dispatch ladder loses
  its rewrite step, and the advertised surface is exactly `KERNEL` again.
- A project keeps vetted scripts in a trusted folder and they simply run;
  everything else is graded like any tool call — denied, run Ask per
  command, or seen once with the choice to run it whole or step by step.
  One rule set governs a call whoever makes it; `rhai(<path pattern>)`
  rules and per-path grants tune the launch, trust is the folder.
- A trusted folder is a credential boundary: a repository trusting its own
  folder can make authenticated `http()` calls with the user's `http_auth`
  without a per-host card. That is the decision, documented where the key is.
- A user-layer skill's scripts run unasked once the user trusts the skill's
  folder — except in a mode the project config gives `prompt:` rules;
  otherwise each launch is graded.
- `auto` now runs every call its deny and prompt lists do not catch; anyone
  relying on its old collapse-then-park for other unenumerated commands
  must add `prompt:` rules (or `deny:`) for them.
- A user config pairing a bare tool rule with a longer class rule of the
  opposite grade (`deny: [write]` + `allow: [edit]`) now resolves to the
  tool rule — the more specific statement — where it used to follow length.
- A grant (or trust) is for the path, never the content: a script rewritten
  after the fact runs as rewritten. Accepted — each nested call still meets
  every explicit deny (a granted launch still asks per command), and the
  agent's own `write`/`edit`/`apply_patch` of a trusted script or skill file
  always asks, in every mode. Its `bash` and MCP filesystem servers are the
  gap: a trusted folder the agent can write to that way is an auto-allow,
  so trusted folders must not be agent-writable.
- Future work: importable library modules for scripts (`import` of a
  vetted system library rather than one `lib.rhai` per skill) is #835.
- Embedders lose `skills::tools` (`register_skill_tools`, `SkillToolDef`),
  `SkillMeta::tools`, `Tool::alias_rewrite`; `SkillLib::signatures` becomes
  `SkillLib::functions` (`LibFn { signature, doc }`); they gain
  `skills::rejected_skills`/`rejected_skills_in`. See the migration note
  in [embedding](../embedding.md).
- Skill authors with a `tools:` list must migrate; until they do, that skill
  is skipped with an error naming the replacement.
- Embedders: `InMsg::Approve`/`seam::Decision::Approve` gain `run_all`,
  `Config` gains `rhai_trusted` (`rhai_trust::TrustedFolders`, by layer),
  `ToolExecutorBuilder::rhai_trusted` wires it (a plain `Vec` is trusted
  like the project layer) and `ToolExecutorBuilder::skill_dirs` the
  skill-file floor (`skills::skill_dirs`); an `Ask`-graded launch parks a
  card a head must answer (`skutter run` starts it Ask per command).

## Alternatives considered

- **Rhai's `metadata` feature for doc comments.** Tried first: it rejects
  doc-style comments that are not directly above a `fn` in *every* script,
  so a header comment broke inline scripts, path scripts and libs alike.

- **Keep the `rhai`/`alias` kinds as sugar over `path`/`lib.rhai`.** Two
  spellings of one thing, plus the advertisement exception the kinds need to
  be reachable at all — the reason to remove them.
- **A fatal parse error for `tools:`** (the issue's "loud parse error",
  read literally). A strict-layer parse failure aborts the whole registry
  load, so one stale project skill would brick startup for everyone opening
  the repo; skipping the skill with an error-level log is just as loud about
  the skill and leaves the rest working.
- **Launch grading alone — `rhai(<pattern>)` rules plus grants** (the
  issue's first draft). A vetted script still prompted for every nested
  `Ask`; a trusted folder says what the user means — "these scripts are
  mine" — so the final design keeps both: graded launches, plus trust.
- **Trust alone — the launch outside grading** (the second review draft:
  no launch rules, no grants, an overlay deny of `rhai` refused with an
  error naming `deny: [rhai]`). It made `rhai` the one tool the overlay,
  grants and path rules could not reach, and needed core to special-case
  a tool name; the user chose one rule set for every tool.
- **A dedicated `auto` Run-all posture** (an interim request). With `auto`
  at `default: allow`, a launch is Allow and its nested calls pass by the
  same rule set; a separate force-allow would only add a second way to say
  it.
- **Keep length-only rule precedence and spell `build`'s ask as a longer
  key** (e.g. a `rhai*` glob tying with `write`). It works by coincidence of
  key lengths and stays invisible to a reader; "a rule naming the tool beats
  one naming its class" is the rule a reader already assumes.
- **Grade `rhai(path)` as `read` of the file.** It is not a read the model
  sees; it is code that runs.
- **A new `ApprovalKind` or `InMsg` variant for the third choice.** An
  unknown enum variant breaks an older head reading the event; one
  serde-defaulted bool on `Approve` costs nothing and defaults safe.
- **Pass `args` by splicing into the source, or as a mutable variable.**
  Splicing is an injection hazard; a mutable `args` invites scripts to use
  the caller's input as scratch state and makes the approval card's `args`
  line a lie mid-run.
- **Enforce the XOR with a JSON Schema `oneOf`.** Not every wire accepts it,
  and the spec must be one byte-identical schema everywhere (ADR-0226).
