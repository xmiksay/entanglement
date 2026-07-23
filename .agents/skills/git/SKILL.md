---
name: git
description: Drive a GitHub issue from read-through to merged PR in this repo — read the issue, branch off the latest `master`, implement following the project brief (Conventional Commits, tests ship with the change, `make verify`), push (`--force-with-lease` after any rebase), open a PR that closes the issue, then read and address review comments in a loop until the PR is approved/merged. Use for the full issue→PR lifecycle, or any phase of it (branch, push, open PR, address review). When invoked, first detect which phase the repo is already in (current branch, open PR, outstanding reviews) and resume from there.
---

# /git — issue → branch → PR → merge (the GitHub loop)

Walk an issue all the way to a merged PR, one phase at a time. Each phase is **resumable**: re-running `/git` should detect where things stand (am I on a feature branch? is there an open PR? are there unanswered reviews?) and pick up there, not start over.

The loop is the point — phase 6 returns to phase 4 (push) until the review is satisfied. The default branch here is **`master`**.

## Guardrails (non-negotiable)

- **Requires the `bash` tool.** Every phase below is shell one-liners with
  pipes/heredocs that `call` (single argv, no shell) cannot run. If `bash` is
  not registered, don't flail with `call` — ask the user to enable it: `/bash
  on` in the TUI (live, #498/ADR-0133) or restart with
  `ENTANGLEMENT_ENABLE_BASH=1`.
- **Never commit to / push to `master`.** Always work on a feature branch. (Project brief: fast-forward only, never commit to `master`.)
- **Never work on a stale repo.** `git fetch origin` first — before branching, before pushing, before reading review state, and before rebasing. Branch/rebase off **`origin/master`**, not your local `master` (which may lag). Stale locals are how you build onto outdated code and manufacture avoidable conflicts; a fetch is cheap, a bad rebase isn't.
- **Don't assume you start on `master`.** Orient first (Phase 0), then create the feature branch off `origin/master` from *whatever* branch you're on (`git switch -c <branch> origin/master`). Never `git switch master` as a step — it fails the moment you're mid-work elsewhere or the tree is dirty. Never switch branches over uncommitted work.
- **After a rebase, push `--force-with-lease` — NEVER plain `--force`.** `--force-with-lease` aborts if the remote moved since your last fetch (someone else pushed); `--force` would clobber their work.
- **Conventional Commits only** with a real scope (`feat(engine): …`, `fix(cli): …`, `docs: …`, …). No `Co-Authored-By` trailer.
- **Tests ship with the change.** Run `make verify` (check-fmt + tree + check-lean + lint + test) before every push and before opening the PR.
- **Keep history linear:** rebase onto `origin/master`; don't `git merge` it into your branch.
- **Never auto-merge the PR.** Merge is the maintainer's (or the user's explicit) call. Stop when approved + no outstanding threads.

## Phase 0 — Orient (always run first)

You may be invoked from **any** branch — `master`, a half-done feature branch, or an unrelated one. Figure out where things stand *before* touching anything, so you resume instead of restarting or failing:

```bash
git fetch origin
git branch --show-current                                    # master? a feature branch? something else?
git status --porcelain                                       # uncommitted work to preserve?
gh pr list --head "$(git branch --show-current)" --json number,url,state   # is there already a PR?
```

Route from what you find:

| State | Go to |
|---|---|
| On `master` (or elsewhere), no feature branch yet | **Phase 2** — branch |
| On the feature branch, no PR | **Phase 3/4** — implement / push, then Phase 5 |
| On the feature branch, PR already open | **Phase 6** — review loop (do **not** re-create the PR) |
| Dirty working tree | resolve first (commit or `git stash`) — never switch branches over uncommitted work |

## Phase 1 — Read the issue

```bash
gh issue view <number>                 # title, body, labels, assignee
gh issue view <number> --comments      # the spec is often refined in the discussion
```

Extract: a **one-line summary**, the **acceptance criteria / definition of done**, any linked/duplicate issues, and whether an ADR is likely needed (hard-to-reverse choices — protocol shape, crate boundary, permission/security model). The comments frequently sharpen or change the requirement — read them. If no issue number was given, `gh issue list` (open; `--assignee @me` / `--label <l>` to narrow) and **confirm with the user** which one before proceeding.

## Phase 2 — Branch

Branch **directly off the freshly-fetched `origin/master`**, from whatever branch Phase 0 found you on — don't route through local `master`. Confirm the tree is clean first.

```bash
git fetch origin                                           # never branch off a stale ref
git status --porcelain                                     # MUST be empty — commit/stash first if not

# Starting fresh — create the branch off origin/master; works from ANY current branch:
git switch -c <type>/<issue#>-<short-slug> origin/master   # e.g. feat/123-token-retry

# Resuming an existing branch instead — check it out and rebase onto fresh master:
git switch <branch> && git rebase origin/master
```

- `<type>` from Conventional Commits (`feat`/`fix`/`docs`/`refactor`/`chore`/`test`/`perf`).
- `<short-slug>` = 2–4 kebab-case words summarizing the issue.
- `git switch -c <branch> origin/master` gets a fresh base **without** switching to (or disturbing) local `master`, so it works no matter which branch you started on — this is the fix for "only works from master".

## Phase 3 — Implement

Frame only — this is the actual coding work, governed by `.claude/CLAUDE.md` + the `docs/architecture/` module docs. Non-negotiables for this repo:

- **No panicking operators on I/O/user/network/config paths** in `entanglement-core` — propagate with `?` + `.context()`. `.unwrap()`/`.expect()` only in tests or provably-unreachable spots.
- **Comments: WHY, not WHAT.**
- **Files stay under 400 lines of code** — split into modules when a file exceeds the cap (a hard project rule; applies to docs too).
- **Tests ship with the change** — pure logic in-module `#[cfg(test)]`; actor/protocol behavior in `entanglement-core/tests/`.
- **Hard-to-reverse choices get an ADR** in `docs/adr/` (numbered, immutable; supersede, never edit in place) + an arch-doc update in the same change.
- Commit in **coherent steps** as pieces land (each ideally passing `make verify`), not one dump at the end.
- Stop when the **acceptance criteria from Phase 1** are met, not when it "looks done."
- Don't amend pushed commits. Add new commits now; squash later if desired.

## Phase 4 — Push

Stay linear before sending anything up:

```bash
git fetch origin
git rebase origin/master               # resolve conflicts, re-run verify if so
make verify                            # check-fmt + tree + check-lean + lint + test
```

Then:

| Situation | Command |
|---|---|
| First push (sets upstream) | `git push -u origin <branch>` |
| History unchanged since last push | `git push origin <branch>` |
| After a rebase (history diverged) | `git push --force-with-lease origin <branch>` |

`--force-with-lease` compares the remote to what you last fetched; if someone pushed in between it **refuses** and tells you to fetch first. That's the safety property — never trade it for `--force`.

## Phase 5 — Open the PR

**First check whether a PR already exists for this branch** — re-running `/git` to push follow-up work must not try to open a second PR (`gh pr create` errors if one exists):

```bash
gh pr list --head <branch> --json number,url,state    # non-empty ⇒ PR exists: skip create, go to Phase 6
```

If a PR already exists, you're resuming — push (Phase 4) and drop into the review loop (Phase 6). Otherwise create it:

```bash
gh pr create --base master --head <branch> \
  --title "<conventional, human-readable summary>" \
  --body "$(cat <<'EOF'
## What & why
<tie to #<issue>; one or two sentences>

## Changes
- <bullet summary of each meaningful change>

## Verification
- `make verify` (check-fmt + tree + check-lean + lint + test) passes
- <how a reviewer can confirm it works>

## Follow-ups
- <out of scope; or "none">

Closes #<issue>
EOF
)"
```

- **Title** is Conventional Commits but human-readable (`feat(engine): retry token refresh on 401`).
- **`Closes #<issue>` must be in the body** — the issue auto-closes when the PR merges.
- If an ADR was added, link it in the body (`docs/adr/00NN-…`).
- Capture the **PR number and URL** from the output; the review loop needs it.

## Phase 6 — Review loop  (↻ back to Phase 4 until approved)

Gather every signal — the pretty view misses inline review comments:

```bash
gh pr view <number> --comments        # the readable thread
# structured: one object per review (APPROVED / REQUEST_CHANGES / COMMENTED)
gh api repos/{owner}/{repo}/pulls/<number>/reviews -q '.[] | {state,user:.user.login,body}'
# structured: inline comments tied to file:line
gh api repos/{owner}/{repo}/pulls/<number>/comments -q '.[] | {user:.user.login,path,line,body}'
```

`{owner}/{repo}` is expanded by `gh` from the git remote — don't hard-code them.

For each unresolved comment → map it to a concrete change. Address review as **new commits** (`fix(engine): address review — handle empty token`), never by rewriting history that's already pushed. When all addressable comments are handled: rebase if `master` moved, `make verify`, then **re-enter Phase 4** (push, `--force-with-lease` if you rebased).

Reply in-thread only if that's the norm here (optional):

```bash
gh api -X POST repos/{owner}/{repo}/pulls/<number>/comments/<comment_id>/replies -f body="..."
```

## When is the loop "done"?

Finish when **any** holds:

- **Latest review is `APPROVED` and no unresolved change-request threads remain** → the code side is finished. Stop; leave the merge to the maintainer (or an explicit user instruction). Don't merge your own PR.
- **The PR is merged or closed** → done.
- **The user says stop.**

If review is still `REQUEST_CHANGES` or there are open threads you haven't answered, you are not done — go back to Phase 4.

## Reporting

- **Per phase:** what you did + the exact `git`/`gh` commands, so the user can re-run or audit.
- **Review loop:** list each comment (who, `file:line`, what they asked), how you addressed it, what you pushed, and the review state afterward.
- **Always end with the PR URL and current state** (`open`/`merged`, last review state, outstanding thread count).
