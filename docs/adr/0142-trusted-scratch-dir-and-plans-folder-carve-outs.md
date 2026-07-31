# 0142. Trusted scratch dir + plans-folder carve-out

- Status: Accepted (amends [0109](0109-escape-root-access-via-approval.md); amends the physical
  read-only mask [0041](0041-update-plan-ownership-default-closed.md) established for the built-in
  `plan` profile, carried forward by [0049](0049-plan-task-tools-as-runtime-state-tools.md))
- Date: 2026-07-31

## Context

Epic child 3 (#512, #524) named two everyday flows the engine still made
needlessly painful:

1. **Scratch space.** Any `read`/`write`/`workdir` outside the project root —
   including plain `/tmp` usage — trips the ADR-0109 escape-root approval gate
   until the user grants `Always` for that exact `(tool, path)`. There is no
   built-in trusted scratch location the model can just use, even though the
   runtime already owns one: `session_store::scratch_dir` (`<data_dir>/
   entanglement/sessions/<cwd>/tmp/`), the default `call`-output target since
   ADR-0109 landed. A model reaching for a genuine scratch file (a temp diff,
   an intermediate JSON blob) pays the same out-of-root approval tax as
   reading an arbitrary system file it has never touched before.
2. **Plan files in read-only plan mode.** The built-in `plan` profile is
   physically read-only (ADR-0041/ADR-0049: no `edit`/`write`/`bash` in its
   tool mask), so it cannot write its plan to a file at all — a gap the
   unified plan-tool design (#513) needs closed: it materializes a plan into a
   markdown file rather than only the in-memory `update_plan` snapshot.

Prior art, compared in the 2026-07-31 review that decided this ADR:

- **opencode** blocks all edits in plan mode *except* `.opencode/plans/*.md` —
  a designated, always-writable plans folder. The carve-out is load-bearing
  enough that opencode issues #11078/#10883 exist because users hit its
  absence.
- **Claude Code** disallows file writes in plan mode entirely (the plan
  travels through the approval UI instead), but gives every session an
  out-of-repo scratchpad directory that is always writable with no prompt.

Neither prior-art mechanism transfers wholesale: this engine's escape-root
gate (ADR-0109) is a *general* out-of-root approval flow with no notion of a
pre-trusted directory, and its `plan` profile's mask (ADR-0041/ADR-0049) is a
blanket denylist with no path-scoped exception mechanism, even though the
underlying permission-rule language already supports argument-scoped keys
(`tool(pattern)`, #173).

## Decision

Adopt both carve-outs, each wired through machinery that already exists
rather than a new mechanism:

### 1. Trusted scratch dir (amends ADR-0109)

`ExtraRootStore` (`entanglement-runtime/src/extra_roots.rs`) gains an
optional `scratch: Option<PathBuf>` field, set once at startup via
`.with_scratch(session_store::scratch_dir(&root))` in `main.rs::build_config`
— the same path `CallTool`'s default output target already used. A new
private `is_trusted_scratch(&self, path) -> bool` checks whether a resolved,
canonicalized path is the scratch dir or a descendant of it; it is consulted
*first*, ahead of the ordinary per-`(tool, path)` grant lookup, in both:

- `is_durably_allowed(tool, path)` — the check the tool executor's escape-root
  gate (`tool_runner::dispatch`) uses to decide whether an out-of-root access
  needs an `Ask` at all;
- `take_allowance(tool, path, request_id)` — the check the six escape-root
  host tools (`read`/`edit`/`write`/`apply_patch`/`bash`/`call`) make when
  actually resolving a path/`workdir` outside root.

Because both call sites already existed and already threaded `ExtraRootStore`
through every escape-root-capable tool, the scratch dir needed **no new
plumbing** — it composes for free with `is_durably_allowed_under`'s ancestor
walk (ADR-0132), so `glob`/`grep` search inside the scratch dir works too with
no separate wiring.

Unlike an ordinary escape-root grant, scratch trust is:

- **Not per-tool.** The scratch dir is a trusted *location*: `read`, `write`,
  `bash`'s `workdir`, all of it, in one field — not a `(tool, path)` pair a
  user approved once.
- **Not persisted.** It is re-derived from the cwd at every startup (mirroring
  `session_store::scratch_dir` itself), never written to `extra-roots.yml`.
  There is nothing to revoke — it is a property of the runtime's own scratch
  location, not a user decision.
- **Directory-prefix, not exact-path.** `is_trusted_scratch` is a
  `starts_with` check, so every file under the scratch dir is covered with no
  per-file grant — a deliberate difference from the exact-path
  `(tool, resolved-path)` key every other grant in the store uses.

The system prompt's generated `<env>` block (`system_prompt::EnvBlock`) now
names the scratch dir and states it needs no approval, steering the model to
use it instead of reaching for `/tmp` (which still pays the full escape-root
tax the first time, same as any other out-of-root path).

**What this does *not* change:** the scratch dir carve-out only removes the
*escape-root* forced-`Ask` layer. A profile's own permission grade for the
tool (e.g. `explore`'s `bash: ask`) is untouched — reaching into the scratch
dir doesn't make an otherwise-`Ask`-graded `bash` call silent, since the
command executed there can still be arbitrary. Only the "is this path outside
root" tax goes away.

### 2. Plans folder (amends the `plan` profile's mask)

`.entanglement/plans/*.md` is carved out of the `plan` profile's physical
read-only mask — in-root, so no escape-root machinery is involved at all.
This reuses the existing **argument-scoped permission rule** language (#173)
rather than inventing a new mask-exception mechanism: `plan.md`'s frontmatter
now reads

```yaml
tools: [read, glob, grep, agent, agent_spawn, agent_poll, ask_user, load_skill, update_plan, propose_plan, write, edit]
permission:
  default: ask
  read: allow
  update_plan: allow
  write: deny
  write(.entanglement/plans/*.md): allow
```

`write` is a **capability key** (#418, ADR-0114): the bare `write: deny` fans
out, at parse time, to literal `edit: deny` / `write: deny` / `apply_patch:
deny`, and the argument-scoped `write(.entanglement/plans/*.md): allow` fans
out identically to `edit(...)`/`write(...)`/`apply_patch(...)`, all `allow`.
One YAML key covers every write-shaped tool. `write`/`edit` also join the
profile's `tools:` allowlist — the #116 mask is a separate, path-blind
existence check (`AgentProfile::advertises_tool`), so both tools must be
unmasked *and* individually graded by the permission ladder before a call
resolves; the ladder is where the path-scoped carve-out actually lives.

Root-relative grading (#485, ADR-0125) means the pattern matches whether the
model's `path` argument is written relative (`.entanglement/plans/foo.md`) or
as an absolute in-root path — `permission_path::grading_arg` strips the root
prefix before the rule ever sees it.

## Consequences

- **Positive.** A model can write genuine scratch artifacts and use the
  scratch dir as a `bash`/`call` `workdir` with zero approval friction, in
  every profile, from the first call — closing the gap where even an
  allow-everything `build` profile paid an escape-root tax for `/tmp`.
- **Positive.** `plan` can now materialize its plan to a durable file (the
  storage location #513's unified plan tool writes into) while staying
  physically unable to touch anything else in the tree — the mask-exception
  is a single pattern, easy to audit, not a code-level special case.
- **Positive.** Both carve-outs ride existing mechanisms
  (`ExtraRootStore`/`is_durably_allowed`, argument-scoped permission rules) —
  no new wire surface, no new store, no new YAML schema.
- **Positive / interaction documented.** The plans-folder carve-out **survives
  or is overridden by the config permission ceiling exactly like any other
  rule** (#172, `clamp_to_base`): the ceiling is a pure least-privilege clamp
  applied *after* the agent's own permission resolves, so a user config with a
  bare `permissions: { write: deny }` ceiling clamps the plan agent's
  `write(.entanglement/plans/*.md): allow` down to `Deny` too — the same as it
  would clamp any other agent's write access. A ceiling that is itself
  argument-scoped (`write(.entanglement/plans/*.md): deny`) has the identical
  effect, more narrowly. There is no special-casing to preserve the carve-out
  against an explicit ceiling — a user who globally denies writes gets exactly
  that, plan agent included. Conversely, the scratch-dir carve-out lives
  *inside* `ExtraRootStore.is_durably_allowed`/`take_allowance`, which the
  config ceiling's `clamp_to_base` never consults (the ceiling clamps
  `Allow`/`Ask`/`Deny` grades, not escape-root containment) — so a ceiling
  cannot re-impose the escape-root prompt for the scratch dir. It *can* still
  deny the underlying tool outright (`write: deny` at the ceiling still denies
  `write` everywhere, scratch dir included) — the ceiling's authority over
  whether a tool runs at all is untouched; only the *escape-root tax* is
  scratch-exempt.
- **Negative / accepted.** The scratch-dir carve-out is directory-prefix, not
  per-tool — a durable escape-root grant is deliberately narrower
  (`(tool, path)`) so a `read` approval never implies `write`. The scratch dir
  breaks that symmetry on purpose: it is the runtime's own throwaway space,
  not a system path the model earned incremental trust into, so the usual
  per-tool caution doesn't apply.
- **Negative / accepted.** Arbitrary user-declared trusted directories (e.g. a
  literal `/tmp` the user wants pre-trusted) stay out of v1 — the existing
  `Always`-scoped escape-root grant already covers that case with one prompt;
  a config-level trusted-dirs list is deferred until the two built-ins prove
  insufficient.

## Alternatives considered

- **A config-level list of trusted directories** instead of one hardcoded
  scratch dir. Rejected for v1: broader surface (schema, validation, docs) for
  a need the existing `Always` escape-root grant already answers for anything
  outside the runtime's own scratch space; revisit only if the two built-ins
  prove insufficient.
- **A dedicated mask-exception mechanism for the plans folder** (a new
  `AgentProfile` field, e.g. `writable_paths: Vec<String>`), mirroring the
  skill-scoped `allowed_tools` mask (ADR-0106) instead of reusing
  argument-scoped permission rules. Rejected: the permission ladder already
  supports exactly this shape (#173's `tool(pattern)`) and the capability
  fan-out (#418) already expands one `write(pattern)` key to every write-shaped
  tool — a new field would duplicate matching logic core already owns for no
  behavioral gain.
- **Widening `ExtraRootStore`'s per-tool grants to accept a directory-only
  key** (record a `Session`/`Always` grant on the scratch dir itself instead
  of adding a dedicated `scratch` field). Rejected: every existing grant in
  the store is exact-path, matched one-to-one against a resolved target — a
  grant that widens to descendants only for search already exists as a
  distinct, narrower mechanism (`is_durably_allowed_under`, ADR-0132) scoped
  specifically to `glob`/`grep`; conflating "widens to descendants" into the
  general per-tool grant path would change that mechanism's semantics for
  every existing grant, not just the scratch dir's.
- **Bypassing the whole permission ladder for scratch-dir calls**, not just
  the escape-root gate. Rejected: a profile's own `Ask`/`Deny` grade for
  `bash`/`call` exists to gate the *command*, which the scratch dir's
  workdir tells you nothing about; only the containment-specific tax should
  disappear.
