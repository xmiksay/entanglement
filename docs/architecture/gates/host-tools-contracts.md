# Gates & host tools §8 — Host tools: output, wiring, orchestration & `poll`

> Part of the [gates & host tools module](../gates-and-host-tools.md) of the [architecture overview](../../architecture.md).

- **Bounded output:** 32 KiB byte cap with a truncation notice; `read` defaults
  to 2000 lines; `glob`/`grep` cap at 1000 results **with a one-line notice
  when the cap fires** (`FileList.capped`, [ADR-0150](../../adr/0150-search-tool-cli-ergonomics.md)
  — a silently truncated broad search used to read as a clean no-match past
  the first 1000 files). Prevents a huge file/tree
  from blowing the context window. `read`/`glob`/`grep`/MCP tools cap
  **head-only** (`truncate_output`) — the head is what matters for a file or a
  search hit list. The exec/agent-shaped tools instead cap **head + tail**
  (`bounded_result`, built on `truncate_head_tail`'s ¼ head / ¾ tail split) —
  build/test output, a script's return value, or a sub-agent's answer put the
  load-bearing part (the error, the result, the conclusion) at the end, so
  head-only truncation would drop exactly what the model needs (#170, unified
  across `bash`/`call`/`rhai`/`agent`/`agent_send`/`poll`
  as one shape with a stated rule, #622); a `bounded_result` status line (an
  exit/job header, a "completed in Ns" line) is kept verbatim and uncounted
  against the cap, only the body is split. `grep`'s per-file **scan** cap (how
  much of a candidate file it reads and searches) is a separate, grep-local 1
  MiB bound (`MAX_SCAN_BYTES`), not the 32 KiB output cap — conflating the two
  meant any file over 32 KiB was silently skipped regardless of the
  match-output size ([ADR-0091](../../adr/0091-grep-file-scan-size-cap-decoupled-from-output-cap.md), #380).
- **Empty-result contract (ADR-0016, extended by [ADR-0150](../../adr/0150-search-tool-cli-ergonomics.md)):**
  a host tool may not return a silent zero-output — **`glob`/`grep` never
  return the empty string at all**. `list_files` returns `FileList { files,
  matched_dirs, skipped_errors, capped, out_of_root }`; per-entry walk errors
  are `warn!`-logged and counted, not swallowed, and containment drops are
  counted (`out_of_root`) so an absolute-path typo doesn't read as a clean
  no-match. When `glob`'s result would be empty but the pattern matched
  something (the bare-`**` trap, which matches only directories), it returns
  a hint like *"`**` matched 7 directories but no files — try `**/*`"*; a
  clean no-match returns *"pattern `X` matched no files."*. `grep`'s
  zero-match reply distinguishes *"path filter `X` matched no files — nothing
  was searched"* (with the same dir-glob suggestion when the filter matched
  only directories) from *"no matches for `X` in N file(s) scanned"* —
  ADR-0016 originally exempted `grep` from any hint; ADR-0150 supersedes that
  after 57% of real grep calls returned the empty string. `grep` is also
  **not** silent about files it excluded from the scan — a file over
  `MAX_SCAN_BYTES` or sniffed as binary (NUL byte in its content) is tracked
  by skip reason (`TooLarge`/`Binary`) and, whenever that list is non-empty,
  surfaced as a labeled notice (capped preview, `... and N more` past 20
  entries per reason) appended to the result regardless of match count —
  otherwise a match that exists only in an excluded file would look identical
  to a genuine no-match ([ADR-0091](../../adr/0091-grep-file-scan-size-cap-decoupled-from-output-cap.md)).
- **Schema advertisement:** `Tool::schema()` feeds `ToolRegistry::specs()`, so
  the model sees a real `input_schema` per host tool (not an empty object).
- **Wiring (ADR-0010, amended by [ADR-0093](../../adr/0093-call-registration-independent-of-bash-opt-in.md);
  bash posture superseded by [ADR-0195](../../adr/0195-bash-is-the-default-exec-and-curated-read-only-rules.md)):**
  `host_tools(root)` registers the **root-contained sextet**
  (`read`/`glob`/`grep`/`edit`/`write`/`apply_patch`; `write` added in
  ADR-0031, `apply_patch` in #455). The
  `skutter` binary registers `CallTool` and **`BashTool` unconditionally**
  at startup — `call` because no shell means no injection surface (#386),
  `bash` because registration is not where the security story lives (the
  session's permission mode, ceiling, and sandbox are; ADR-0195 reverses
  ADR-0010's opt-in). The retired opt-in machinery —
  `ENTANGLEMENT_ENABLE_BASH`, the closed lazily-registrable-built-in table
  and its `/enable tool bash` registration fold, and
  `decline::disabled_builtin_decline` — is gone: there is no
  advertised-but-unregistered built-in left. An overlay **grade** entry
  naming `bash` is still meaningful (a pure grade override now that the
  tool is always registered) and a deny entry still withdraws it at
  dispatch, same as any tool. `bash` shares its long-lived `JobRegistry`
  with the `poll` tool (#605; one registry, never minted fresh —
  ADR-0163 §3). `EngineConfig::default()` ships an
  empty registry (embedders opt in via `host_tools`).
  **Curated read-only Allow rules (ADR-0195, relocated by ADR-0207)** ship
  as the shared read-only exec set spliced into the `research`/`plan`/`auto`
  modes (`mode/builtin/readonly_exec.yml` — `bash(pattern)` keys, which
  `call` shares since both exec tools use one rule set; `build` needs none
  of it, it class-allows `exec`) — see
  [agents & permissions](../agents/definitions.md) §the shared read-only
  exec set for the list and its deliberate exclusions — so read-only
  commands stop costing an approval round-trip. The config ceiling still
  clamps least-privilege over every grade: a user ceiling of `bash: ask`
  (or `bash(find *): ask`) forces the prompt back over any curated Allow,
  exactly as it clamps a mode's own `Allow`. Graded per
  top-level segment of a compound command, not the whole raw string, since
  [ADR-0197](../../adr/0197-compound-bash-commands-grade-per-segment.md). See
  [agents & permissions](../agents/approvals-and-grants.md) §permission ceiling.
  **Advertisement ([ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)):** `call` (like `glob`/`grep`) is registered but
  **never advertised** — a script-only `rhai` binding, still graded under its
  own name; `bash` is in the kernel.
  The overlay grade lookup that finds the applicable entry
  (`permission::overlay_grade_entry`)
  walks the session's ancestor chain nearest-first (✅ #628), so a `bash()`
  binding run from inside a `rhai` script grades identically to a direct
  `bash` call — both run the same seam (ADR-0227) — and a
  spawned child with no overlay of its own inherits its parent's grade, not
  just the enable entry that lets it use the tool.

Every session is advertised the same tool set — there is no more per-agent
mask to narrow advertisement or dispatch with
([ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md),
superseding [ADR-0038](../../adr/0038-physical-per-agent-tool-restriction.md);
see [agents & permissions](../agents/modes.md) §permission modes).
What differs now is the session's **mode**, not the agent, and it carves the
exec surface differently per mode: `build` mode auto-allows
`edit`/`write`/`apply_patch`/`bash`/`call` (`allow: [read, write, exec]`) bar
a short destructive-command deny list; `plan` mode denies the `write` class
outright except the plans-folder carve-out (ADR-0142), with exec ungraded
(falls through to `default: prompt`); `research` mode denies `write` and
`plan` outright while curated-allowing a read-only `bash`/`call` verb list
and prompting on everything else; `auto` mode allows by default, bar a
destructive and network-mutating deny list and a prompt list of
destructive deletions — [ADR-0228](../../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)
amends ADR-0209 here. Registration and mode grading are two
orthogonal axes: registration decides whether the tool can *execute* at all
(unconditional for both `call` and `bash`, ADR-0195), and the mode's rule
table decides `Allow`/`Ask`/`Deny` for a given call — so `bash` being
always-registered does not change what a non-`build` mode can do with it.

Six **runtime-owned orchestration tools** are *not* in the registry — the
`tool_runner` intercepts them on `ToolExec` before permission resolution (they
touch no host resource) and advertises their schemas separately: `agent {
agent, prompt, background? }` (renamed from `spawn_agent`, ADR-0022; §5,
ADR-0033; the separate `agent_spawn` tool it was later split into is retired
again, #606, ADR-0161 §1 — one tool, `background: bool` picks the return
shape) blocks by default (spawn-and-wait in one call) and, with `background:
true`, hands back a handle immediately instead — both paths reply with `` sub-agent `id` completed in Ns:\n\n{answer} `` bounded by
`bounded_result` (#622): the "completed in Ns" status line is kept verbatim,
the answer gets the same head + tail byte cap as `bash`/`call`/`rhai` so a
long answer's conclusion survives instead of growing the reply unbounded. A
truncated answer also mints a retained-output handle (#614) exactly like a
truncated `call`/`bash` result, so the dropped middle isn't silently lost —
`poll` pages it back. (`propose_plan`'s own approval reply is a much smaller
`InMsg::SetMode` mode-switch acknowledgment now — no build child, no
result to bound —
[ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
§7 retires [ADR-0138](../../adr/0138-sponsored-build-child-and-propose-plan-cycle.md)
wholesale; see [engine](../engine/subagents-and-plans.md) §plan acceptance.)

`agent_send { agent_id, prompt, background? }` (#609, ADR-0162) is a launcher
against an *existing* session instead of a fresh one: it sends
`InMsg::Prompt` to a child `agent`
already produced, so the same session picks its turn back up with its
accumulated context rather than starting over. It carries the same
`background` flag and shares `agent`'s guard path — [`AgentRegistry::begin_send`]
resolves ownership (only the launching session may send — the ADR-0161 §4
descendant check, generalized: `agent_send` is a *write* verb on the
registry, so this scoping is a hard prerequisite rather than a hardening
pass) and the child's session lifecycle in one lock acquisition before
anything is sent. A handle that isn't the caller's own, or was never
launched at all, refuses with the same "unknown agent_id" message either
way. A **closed** (tombstoned) child refuses clearly — its id is spent
(ADR-0028). A **hibernated** child refuses loudly rather than silently: a
fresh `Prompt` at a hibernated id would otherwise fall into the supervisor's
lazy-respawn path and come back as a *blank* session wearing the right id,
discarding its context — the idle-TTL sweep (ADR-0090) makes this the likely
failure, not a corner case, so `agent_send` never sends until the lifecycle
check confirms the child is still live. Only a **live** child is actually
sent the prompt; the default (blocking) path then reuses
`collect_child_answer` to wait for its *next* answer (not its first) and
folds it back the same way the blocking `agent` path does; `background: true`
returns immediately and the follow-up answer is collected later with `poll`,
exactly like a `background: true` launch. The lifecycle itself is folded from
the engine-wide `SessionStarted`/`SessionHibernated`/`SessionEnded` broadcast
— any session's transition, not just this executor's own — into the same
`AgentRegistry` entry `poll` already reads.

`poll { handle?, timeout_secs?, kill?, offset?, tail? }` (#605, ADR-0161
§1-4, replacing `bash_output`/`agent_poll` outright — no aliases) is the
single join tool for all four: it dispatches on the handle's kind prefix
(ADR-0164) to the job registry (`j-`), the background-script registry (`x-`,
a `rhai background=true` launch — #637,
[ADR-0185](../../adr/0185-rhai-joins-background-and-poll.md)), the
retained-output registry (`o-`, a
completed operation whose result overflowed its cap — #608, ADR-0161 §7), or
the sub-agent registry (anything else, a `s-` session id), waiting up to
`timeout_secs` (default 60, cap 600, `0` = wait until terminal) for new
output/exit (job), new print output/finish (script — the terminal poll also
carries the script's final `=> value`/error line), or the final answer
(agent) — the delta-vs-final
distinction rides the `running`/`complete` status, not the tool name. A
retained-output poll never waits (the operation already finished) and
instead pages the text: `offset` (default 0) is a 0-based line index into
the retained text, `tail` (default 30, matching `call`'s own default) is how
many lines to return from there — `0` returns the rest, still byte-capped.
An entry whose operation had an explicit `output_file` is named instead of
paged, since the file already holds the full text — the one place a path
still appears. `kill: true` SIGKILLs a job's process group; on a script
handle it is **cooperative** (ADR-0185) — it trips the stop flag the
engine's progress callback polls, the script ends at its next operation
(an in-flight `exec`/`bash` binding finishes its own budget-clamped timeout
first), the kill poll reports "stop requested", and the *next* poll reports
`stopped (killed)`; refused on a
sub-agent or retained-output handle (nothing to cancel/kill in the latter
case). An unknown (or not-the-caller's-own) handle is an *error*, adopting
`agent_poll`'s convention over `bash_output`'s return-it-as-text — and these
model-mistake branches (unknown handle, refused `kill`) plus a script's own
terminal error also set the structured `is_error` flag on the `ToolResult`
(#695, closing the hand-audit remainder ADR-0176 deferred for this
runtime-owned route); every outcome of a poll that ran —
running/complete/list/paged, a job exiting nonzero (`exit_code` stays
orthogonal, ADR-0186), a killed job or cooperatively-stopped script — stays
`false`. Retention
is a `RetainedOutputRegistry` shared with `call` and `agent` (#614) the same
way `JobRegistry` is shared with `bash`/`call` — capped per-operation (256 KiB, keeping the
tail) and evicted on the same 15-minute-TTL/200-entry shape `JobRegistry`
uses (`ScriptRegistry` mirrors all of it: capped streamed output with
reported drops, owner scoping, the same TTL/count sweep); overflow past that
cap is reported in the poll result, never silent.
Called with
**no `handle`** (#607, ADR-0161 §6), `poll` instead lists this session's own
pending operations — every outstanding job/script/sub-agent it launched, with
kind,
handle, launcher, elapsed time, and status — via
`entanglement_runtime::operations::list_operations`, shared with the
head-facing `InMsg::ListOperations` → `OutEvent::OperationList` (wire-allowed,
same read-only-snapshot rationale as `ListQuestions`). Attribution rides the
*same* ownership bookkeeping the single-handle join already needs: `JobRegistry`
tracks each job's owning `SessionId` (#605), `ScriptRegistry` each script's
launching session (#637), and `AgentRegistry` each child's
launching parent + profile (#618, #607) — one mechanism serving both. Lifetime
is deliberately not uniform across kinds: an agent handle is itself a session,
so a completed entry stays listed until polled by handle (never evicted,
unlike a finished job's TTL-bounded entry); a background job is an OS process
owned by this engine process — and a background script an in-process task with
the same engine-bound lifetime — so neither can outlive it; a resumed session
can therefore legitimately show agents and no jobs/scripts. When anything
is pending in the caller's spawn sub-tree (#829 — the half of the retired
`explore(kind: "pending")` recovery report that
[ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)
deferred), the listing is followed by a `subtree (this session and its
sub-agents):` section (`poll/subtree.rs`) with up to three groups: retained
outputs (owned by the caller, so pollable, or by a descendant, named for
awareness only since the registry pages an entry to its owner alone), open
`ask_user` questions (`OpenQuestions`), and parked approvals
(`PendingDecisions`, `ask_user` parks excluded because the question row
already reports them). No argument turns it on and the input schema is
unchanged; an idle sub-tree adds nothing, so the reply is then byte-identical
to the plain listing. Scope is the downward spawn walk
(`discover::subtree_sessions`, the one `InMsg::ListPending`'s `root` filter
uses), so the non-maskable, ungraded `poll` (ADR-0190) covers only sessions
in the caller's own spawn sub-tree, never a sibling or an ancestor. Ownerless
retained entries (the session-less `Tool::run` path) are pollable by anyone
holding the handle, so they belong to no sub-tree and are never listed.
**A compaction successor owns its predecessor's work**
([ADR-0231](../../adr/0231-compaction-successor-inherits-background-work.md)):
on `SessionStarted { predecessor: Some(p) }` the executor's lifecycle loop
calls `reassign_owner(p, successor)` on `JobRegistry`, `ScriptRegistry` and
`RetainedOutputRegistry`, so the successor's handle-less `poll` lists the
still-running jobs and scripts, shows the retained outputs as "yours", and
`poll { handle }` resolves them. That fold reads the lossy shared outbox, so
every successor `poll`, with or without a handle, first re-runs the
idempotent move from the supervisor's directory. A lagged-away
`SessionStarted`, or work the predecessor registered after the fold, is
adopted before the poll reads anything. The fold and every heal take the
whole chain back (`Holly::predecessors_of`), not one hop, so two compactions
before either runs still reach the oldest ancestor
([ADR-0242](../../adr/0242-compaction-heals-walk-the-whole-chain.md)).
**A script's parked card moves with it**
([ADR-0238](../../adr/0238-compaction-moves-a-background-scripts-card.md)):
a parked card keeps its key and gains a `home`, and
`PendingDecisions::reassign_detached` re-homes every detached card from the
ancestors onto the live end of the chain (`pending::live_end`, which also
homes a fresh detached park); the registry keeps no lineage of its own. That
runs in the lifecycle fold, before `poll`, before every approval park, and
before the decision router handles a `Stop` or a decision
(`pending::heal_before`); `poll` and a rooted `ListPending` heal every
session of the walked sub-tree. Each moved card is re-announced as a fresh
`ToolRequest` on the successor. A decision takes the live card among the
exact key, the card moved onto that session, and a card whose earlier homes
(`Parked.via`, the homes a move took it off) include it (ADR-0242), so the
predecessor's id still answers it and never reaches the successor's own
card; a timeout's `forget` removes only its own closed entry. Every
approval park heals first, and a home never holds two live cards with one
id: only a turn card supersedes a live turn card, any other park sharing a
live card's key or id is refused with a reason. **The script follows too**
([ADR-0241](../../adr/0241-background-script-follows-compaction.md)): before
each nested call a background run's dispatcher switches to the live end of
its session's lineage (`Holly::successor_of`, once the executor folded that
session's mode), so later calls grade, grant and park as the successor, and
re-reads the session's mode (and drops a stale mode-scoped overlay entry)
before every call; a
call in flight finishes as the predecessor, but its `Session` grant or `http`
unlock is recorded where the next call will be graded (the same switch
condition). Closing the predecessor never
killed these entries, so ordering against its `SessionEnded` does not
matter. A `/fork` copy carries no `predecessor` and leaves its live source
owning everything. **Sub-agents follow too**
([ADR-0234](../../adr/0234-live-sub-agents-survive-compaction.md)): core moves the
predecessor's live children under the successor (`OutEvent::ParentChanged`),
and an `AgentRegistry` entry stays keyed by the handle its parent was given
while aliasing the live `session` that answers for it. The lifecycle loop
folds `ParentChanged` (`reparent`) and a successor's `SessionStarted`
(`follow_successor`, for a compacted child) eagerly; `poll`, `agent_send`,
`ListOperations` and `ListPending` first run `AgentRegistry::heal_from`, which
re-derives every entry's live session (`Holly::successor_of`) and parent
(`Holly::session_info`) from the supervisor's directory. So after a parent
compacts, the successor's handle-less `poll` lists the running children,
`poll { handle }` collects their answers and `agent_send` reaches them; after
a child compacts, its parent's handle resolves to the successor's answer and
the subtree walk (handle, then live session) keeps its sub-tree. `poll` is
intercepted before permission resolution exactly like `agent`
— it starts nothing and touches no host resource, only reading state a
previously-graded launch produced —
`ask_user { questions: [{question, options, multi_select}] }` (§5, ADR-0027;
v2 #488, ADR-0127 — batched questions, `multi_select` per question, an
unconditional free-text "Other" answer; #515, ADR-0146 — a head can list every
open question (`InMsg::ListQuestions`), withdraw one without cancelling the
turn (`InMsg::RetractQuestion`), or swap its content in place
(`InMsg::ReplaceQuestion`), tracked by a runtime-owned `OpenQuestions`
registry beside `PendingDecisions`), and
`propose_plan { plan }`, the plan agent's finalize step, force-parked on the
user-approval round-trip since acceptance *is* its semantics (#141,
[ADR-0042](../../adr/0042-plan-acceptance-via-propose-plan-approval-roundtrip.md);
advertised only to a profile that explicitly allowlists it, #231). The `rhai`
script tool (table in [host tools](host-tools.md)) is intercepted the same way but is **not** a bypass:
it resolves its own `Allow`/`Ask`/`Deny` live inside the sandboxed script task
(#122, [ADR-0046](../../adr/0046-rhai-sandboxed-script-tool.md)).

---

Previous: [§8 — Host tools: the tool set, containment & sandbox](host-tools.md) · Next: [§8 — `web_fetch` and undo/rewind](web-fetch-and-undo.md)
