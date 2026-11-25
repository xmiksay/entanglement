# 0247. Attended sessions wait for their approver

- Status: Accepted
- Date: 2026-10-22
- Issues: #828
- Amends: [ADR-0207](0207-permission-modes-replace-agent-borne-authority.md)
  §11 (the unattended collapse and timeout, the "identical repeat"),
  [ADR-0209](0209-auto-mode-defaults-to-prompt-not-deny.md) and
  [ADR-0228](0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)
  (their "unattended" wording for `auto`),
  [ADR-0240](0240-approval-resolved-event.md) (an `expired` end is shown,
  not only dropped) and
  [ADR-0245](0245-a-decision-only-answers-its-own-kind-of-card.md) §1 (an
  `Absent` decision is no longer a silent no-op)

## Context

ADR-0207 §11 made a mode with a finite `question_timeout` unattended: a
first `Ask` collapses to a refusal, an identical repeat parks a card that
expires after `question_timeout`, and an `ask_user` question falls back to
its first option. `run_limits::collapses_ask` read only the mode.

#828 showed the cost. A sub-agent in `auto`, implementing an issue, hit
`auto`'s `prompt: bash(rm -r*)`-style rules on routine repo work. The user
had the TUI open and was approving, but:

1. The child was treated as unattended although its root was watched. Its
   first call collapsed; its repeat parked a 60s card the user only saw
   late.
2. The model's retry dropped `2>&1 | tail -3`, so the exact `(tool, arg)`
   key never matched and the retry was denied again with no card.
3. Two approvals arrived after their cards expired and were dropped with no
   word to the user. The model started the ladder over.

The other points of the issue were fixed elsewhere: ADR-0228 made `auto`
`default: allow`, `mode::rules` canonicalizes `call` to `bash`, and
ADR-0226 removed `invoke`.

## Decision

1. **"Attended" is a fact about the session.** A session is attended while
   a head that can answer a card watches it or any of its ancestors. The
   runtime keeps the registry (`attendance::Attendance`), since the gate
   lives there. A head registers a `Watcher`, which unregisters on drop:
   - the TUI watches every session (`Attendance::watch_all`): it shows each
     session its engine runs, so this equals "the sessions it shows";
   - a `serve` connection watches the sessions it owns (ADR-0107's
     first-writer ownership), since only the owner may answer their cards.
     A reconnecting client's `ListPending{root}`/`ListQuestions{session}`
     claims the filtered session like a routed frame, so listing alone
     attends it; a bare-text prompt claims the connection's default
     session;
   - `run` and `pipe` never watch.

   The ancestor walk is the dispatcher's existing chain (`SpawnGuard`'s
   parent links, ADR-0024), so a watched root attends every sub-agent it
   spawns. An embedder passes its registry with
   `ToolExecutorBuilder::attendance`; one that registers nobody keeps the
   mode-only posture.
2. **An attended session waits.** `run_limits::unattended` is
   `question_timeout != 0 && !attended`. Attended, a prompt-graded call
   parks a card as in `build`, with no first-call collapse; the card, a
   `rhai` launch card and an `ask_user` question have no deadline. The
   credentialed-`http` gate follows the same fact. Unattended behaviour is
   unchanged.
3. **A watcher arriving waives a running deadline for good.** A parked item
   waits on an `Expiry`: none when attended at park time, else the mode's
   `question_timeout`, cancelled the moment a watcher of the chain arrives.
   A waived item never times out again, even after that watcher leaves. A
   replaced `ask_user` question shares its predecessor's waiver. A watcher
   arriving as the timer fires still wins (the wait polls the arrival
   signal first, and the deadline re-checks attendance before it fires).
4. **A waiting item survives its watcher.** It is listed by `ListPending`/
   `ListQuestions` and answerable in place. A hibernate unwinds it as today
   and resume re-offers the parked `ToolExec` under the same request id;
   the registry keeps each waiver per `(session, request_id)` until the
   park is answered, the user `Stop`s the session (core never re-offers a
   stopped call) or the session ends — a hibernate's unwind keeps it — so the
   re-offered card still waits, watched or not. The waiver is in memory:
   after a process restart a re-offered card parks under whoever watches
   then — the relaunched TUI watches every session, so its cards wait;
   carrying a waiver into a fresh process with nobody watching would need
   it recorded in the session log, a protocol change left out here.
   `CloseSession` ends it.
5. **A repeat is matched by rule.** Unattended, a retry is "the same call"
   when the same argument- or workdir-scoped rule naming the tool graded it
   `Ask` (`bash(rm -r*)`: `rm -r x` then `rm -r x 2>&1`). With no such rule
   — a default grade, a class rule (`exec`), a `*` rule, a bare tool name,
   or an `Ask` from an ancestor's clamp or an overlay — the key stays the
   exact `(tool, grading_arg)`. A wide rule would make every call of the
   tool the same call, so it does not count. The grader exposes the
   deciding rule (`Mode::deciding_rule`), computed by the same ranking that
   grades.
6. **A late or stray decision is refused.** When an `Approve`/`Reject`
   matches no card (`Resolution::Absent`), the router emits
   `OutEvent::Error` on the addressed session:
   `` `Approve` refused: no pending card `<id>` (it expired, was answered,
   or never existed) ``. Question frames get the symmetric refusal
   (`no open question`). This is the same branch of the same `resolve`, so
   the extension is free. A second head's late approve is refused this way.
   The heal (ADR-0238) runs before the lookup, so a moved card still
   answers.
7. **The TUI shows an expired card.** On `ApprovalResolved { outcome:
   expired }` the view appends "approval for `<tool>` expired — nobody
   answered within the mode's question_timeout; approving it now has no
   effect". The line comes from the logged event, so it renders on replay.
   The event carries no duration, so the line names none.

## Consequences

- `auto` still means "run" for what it allows. What it asks now waits for
  a watching user instead of failing after 60s. Unattended (`skutter run`,
  `pipe`, an embedder with no watcher) it behaves as before.
- A user who walks away from a watched session can leave a card parked
  indefinitely. It holds only the turn, and hibernation (idle TTL) still
  evicts the session.
- Embedders: `PendingDecisions::await_timed` and `ask_user::run_ask_user`
  take an `attendance::Expiry` instead of `Option<Duration>`
  (`Expiry::after` is the old shape; `Expiry::unless_attended` takes the
  park's request id, the waiver's key). `serve::router`/`serve::serve` take
  the `Attendance` the executor was built with and an `Option<Rejoin>`. An
  embedder router of its own should report `Absent` as well as `Misaimed`.
- A `serve` client's filtered listing wakes a session this process
  hibernated (`serve::Rejoin`), so a returning user gets everything back:
  - serve resumes it from the session log (`load_for_resume` +
    `resume_with_mode`, the TUI's path), off the read loop;
  - then it resumes, parents first and from the same records, each
    descendant the root's own hibernate put down
    (`session_store::revived_with`, applied by `resume_with_mode` on every
    resume path: the TUI, `run --resume` and this wake). Core's
    resume cascade only revives a child its log still shows live, and the
    cascade logs `SessionHibernated` for every victim. A descendant counts
    as the root's cascade when the nearest `HibernateSession` logged since
    it last started, naming it or an ancestor, names the root, or none does
    (the idle sweep hibernates roots only). A sub-agent hibernated on its
    own (or under one) stays down, a closed one stays closed (a hibernated
    id closes with only its inbound `CloseSession` logged) — ADR-0246's
    rules are untouched: core's cascade is unchanged, these are explicit
    resumes, mode-clamped like any (ADR-0246 §1);
  - it forwards the listing once every call core re-offers in the woken
    tree is back as a card, a question or a result, bounded at 5s.
    "Parked" is core's own answer, each replayed turn's unresolved batch
    (`Session::replay(..).turn`): a stopped, finished or retried turn waits
    for nothing, and a re-offered `agent`/`agent_send`/`poll` call, which
    waits on its sub-agent instead of answering, is not awaited;
  - a revived child is counted as resumed by `check_for_resume`, so it
    gets ADR-0215's fallback note and forced mode like a live one;
  - only roots are wake targets: a listing naming a sub-agent wakes its
    root (the parent links serve folds off the engine), which brings the
    child back;
  - one wake per root: a later listing waits for it, and the connection's
    frames for any session in the woken tree queue behind the listing in order (bounded,
    with backpressure); a wake that fails leaves the root asleep, so the
    next listing retries; past 4096 tracked roots the oldest sleeping one
    is forgotten, with one warning;
  - a live or unknown session, a closed one, or a listing from a connection
    that does not own the session is listed as before.
- Not changed: `auto`'s allow, prompt and deny lists; inline child cards in
  the parent's TUI view (the attention jump stays).

## Alternatives considered

- **A longer timeout for children.** It still expires, and a late approval
  is still lost.
- **Only matching the repeat on a normalized command head.** This
  re-implements a second command matcher next to the rules. The deciding
  rule already captures what a human would call the same command.
- **Attending through the TUI's per-session views.** That needs a hook at
  every view insertion and races a child's first card. Every session in a
  TUI process is shown, so a process-wide watch is equivalent.
- **Re-timing a waived item when its watcher leaves.** The user wanted a
  card they were shown to stay answerable, so a waiver is final.
