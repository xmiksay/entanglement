# entanglement Architecture — Agent profiles, permissions, skills & system prompt

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

This module is split into the pages below, one per seam; each stays under the 400-line doc cap (`make doc-cap`). Every section heading the single-file version had is kept here, pointing at where that section now lives.

| page | covers |
| --- | --- |
| [modes](agents/modes.md) | §3 — Identity vs authority, starting mode & permission modes |
| [approvals-and-grants](agents/approvals-and-grants.md) | §3 — Mode tuning, approvals, grants & the permission ceiling |
| [definitions](agents/definitions.md) | §3 — Agent definitions, built-in modes, pinning & live reload |
| [advertisement-and-enforcement](agents/advertisement-and-enforcement.md) | §3 — Advertisement, enforcement, overlay & discovery |
| [spawn-tasks-plans](agents/spawn-tasks-plans.md) | §3 — Spawn bounds, task state & the plan tool |
| [prompt-and-skills](agents/prompt-and-skills.md) | §3 — System prompt, skills, slash commands & dispatch |

## 3. Agent profiles + permission modes — [ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)

Now in [§3 — Identity vs authority, starting mode & permission modes](agents/modes.md#3-agent-profiles--permission-modes--adr-0207).
Continued in [§3 — Mode tuning, approvals, grants & the permission ceiling](agents/approvals-and-grants.md).
Continued in [§3 — Agent definitions, built-in modes, pinning & live reload](agents/definitions.md).
Continued in [§3 — Advertisement, enforcement, overlay & discovery](agents/advertisement-and-enforcement.md).
Continued in [§3 — Spawn bounds, task state & the plan tool](agents/spawn-tasks-plans.md).
Continued in [§3 — System prompt, skills, slash commands & dispatch](agents/prompt-and-skills.md).
