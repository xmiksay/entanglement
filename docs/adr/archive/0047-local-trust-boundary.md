# 0047. Local trust boundary — the repository is trusted; config precedence system < user < repo; inspection over enforcement

- Status: Accepted
- Date: 2026-07-12

## Context

entanglement is a **local, single-user** developer tool. Its layered
definitions — agents (ADR-0034), skills (ADR-0036), the provider/model catalog
(ADR-0032), and the new user config/settings file (#172) — all resolve
**embedded default < user (`${config_dir}/entanglement/`) < project
(`<root>/.entanglement/`)**, later wins by `name`/`id`. A project-layer file can
therefore redefine a built-in agent's `permission`, `mode`, tool mask
(ADR-0038), and system-prompt body; a missing `permission` key currently falls
back to `Allow`.

A security review (#162) framed this as a hole: a hostile repository controls an
agent's privileges and system prompt merely by being opened and run, with no
signing, no trust-on-first-use, and no "project may only narrow" clamp. The
proposed fix was an in-repo trust boundary.

But the **deployment reality** is decisive. Running a coding agent inside a
repository already means executing that repository's toolchain — build scripts,
git hooks, test harnesses, `bash`/`call`/`rhai` invocations — with the user's
full privileges. The trust boundary of a local dev tool is **the user's
machine**, not a directory inside it. A repo that can run a build script does not
need to redefine an agent to do harm; enforcing a within-repo boundary is
security theater against a threat the surrounding workflow already accepts.

Separately, the user-level settings file (#172) needs a precedence rule against
that same project layer, and it must be the *same* rule users already learned for
agents/skills/providers — divergence here is a support cost and a least-surprise
violation.

## Decision

1. **The repository is trusted.** Running inside a repo means its
   `.entanglement/` definitions are trusted. No project-layer trust boundary is
   enforced — no signing, no trust-on-first-use, no "project may only narrow"
   clamp. Issue #162 is closed as **by-design**.
2. **One uniform precedence for every layered definition:** system default
   (embedded) **<** user profile (`${config_dir}/entanglement/`) **<** repository
   (`<root>/.entanglement/`), later wins. The user config/settings file (#172 —
   permissions plus general config) follows the identical rule, so a repository
   may override the user's configuration for work in that repository. This mirrors
   git's `system < global < local` and every existing registry in the codebase.
3. **The mitigation is inspectability, not restriction.** "What did this repo
   change?" is answered by surfacing state, not forbidding it: the assembled
   system prompt, the resolved agent/skill registries (including *which layer won*
   an override and its source path), and the effective clamped profile are
   exposed by `skutter inspect prompt|agents|skills` and the in-session TUI
   inspection view (#214). Inspection is thereby a **security control**, not only
   a debugging aid.
4. **The ancestor privilege ceiling (ADR-0024) is unchanged and orthogonal.**
   "Trusted source" means the *definition* is trusted to load; it does not lift
   the clamp — a spawned child still cannot exceed its parent's permission/mask.

## Consequences

- **Positive.** No first-run friction and no signing/attestation infrastructure.
  The override model is uniform and predictable across agents, skills, providers,
  and user config — least astonishment, and identical to a tool users already
  know (`git config`).
- **Positive.** Inspection (#183, largely landed) is elevated from "nice for
  debugging" to a load-bearing part of the trust story, which justifies its P0
  standing.
- **Accepted risk.** Opening and running an agent in a genuinely hostile
  repository can redefine agent behaviour and permissions. This is accepted
  because the surrounding workflow already grants that repo code execution; the
  operational guidance is the same as for running `make` in an unknown repo — do
  not run privileged agents/tools in repositories you would not run.
- **Coupling — important.** This decision is coupled to
  [ADR-0048](0048-serve-head-local-trust-model.md) (the `serve` head is
  local-only). `repo > user` precedence and repo-trust are safe *only because
  there is no second party*. If a public or multi-tenant head is ever considered,
  **both** ADRs must be revisited (superseded, not edited).
- **Neutral / deferred.** The `permission`-missing-defaults-to-`Allow` nuance is
  left as-is; a later small hardening (default a missing key to `Ask`) can be made
  without disturbing this trust model.

## Alternatives considered

- **Enforce a project-layer boundary (project may only *narrow* permission/mode/
  mask, never loosen).** Rejected: it fights the deployment reality — the repo's
  code already runs — so it adds friction while giving false assurance.
- **Trust-on-first-use per repository (prompt before loading `.entanglement/`).**
  Rejected: prompt fatigue on the overwhelmingly-common trusted case, and it
  guards the wrong line — the real boundary is the machine, not the directory.
- **Put user config *above* repo in precedence.** Rejected: it breaks the
  most-specific-wins model every other layered definition uses and would prevent a
  repository from tailoring an agent to its own codebase — the primary reason the
  project layer exists.
- **Leave it as an open, undocumented security bug.** Rejected: the behaviour is
  intentional, and without a record reviewers keep re-filing it (they did, #162).
  A hard-to-reverse security/permission decision is exactly what an ADR is for.
