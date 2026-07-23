---
name: commit
description: >-
  Write a well-formed Conventional Commit message for a set of changes. Use when
  the task is to "commit", "write a commit message", "make a commit", or to
  describe changes for the git history and you need the type/scope/subject rules
  and body/footer structure.
allowed_tools:
  - bash
  - call
  - read
  - grep
---

# Conventional Commit messages

Compose the commit header as `type(scope): subject`:

- **type** — one of `feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `perf`,
  `build`, `ci`. Pick the one that matches the *primary* intent of the change.
- **scope** — the affected area in parentheses (a crate, module, or subsystem).
  Omit it only when the change is genuinely cross-cutting.
- **subject** — imperative mood, lower-case, no trailing period, ≤ 72 chars
  ("add", not "added"/"adds").

Then, when the change is not self-explanatory:

- A blank line, then a **body** wrapped at ~72 columns explaining *why* the
  change was made and any consequence a reader should know — not a restatement
  of the diff.
- A **footer** for issue links (`Closes #123`) or `BREAKING CHANGE:` notes.

Before writing the message, inspect what is actually staged (`git diff --cached`)
so the type, scope, and subject describe the real change. Never invent a scope
you have not confirmed exists.

When the `bash` tool is unavailable (it is opt-in), run git through the `call`
tool instead — single argv commands like `git diff --cached` or `git log
--oneline -10` need no shell. Shell composition (pipes, `&&`) still requires
`bash` (`/bash on`).
