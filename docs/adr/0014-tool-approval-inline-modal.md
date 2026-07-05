# 0014. Tool approval UX: inline card vs modal

- Status: Accepted
- Date: 2026-07-05

## Context

When a `Permission::Ask` tool call arrives (`OutEvent::ToolRequest`), the user must approve or reject it. Two UI approaches:

1. **Inline card:** Render the approval UI directly in the transcript, just above the prompt input. It's always visible but may clutter the view.
2. **Modal dialog:** Pop up a centered overlay. More focused but hides the conversation context.

opencode uses an inline card. The `agent` reference uses a modal (approval blocks the turn with a blocking call, so a modal is natural in that pattern). Our engine is async and event-driven; either could work.

## Decision

Use an **inline approval card**.

Rationale:

- **Always visible:** The user never loses sight of what they're approving. No risk of context loss.
- **Lightweight:** Modals feel heavier and require explicit dismissal even when the user just wants to read.
- **opencode precedent:** Most users coming from opencode expect this flow.
- **Simpler state:** The TUI's "active focus" remains on the approval keys (`y`/`n`/`e`) without entering a nested UI mode.

The card shows:

```
? edit src/main.rs
  { "path": "src/main.rs", "oldString": "...", "newString": "..." }

[y] approve  [n] reject  [e] edit reason  [Esc] interrupt
```

While the card is active, the prompt input is disabled (or typing sends a rejection reason, if we choose that UX). The card disappears once the engine emits `ToolOutput` or `AgentChanged` (new turn).

## Consequences

- **(+)** Context is preserved (the plan/messages surrounding the tool are visible).
- **(+)** Familiar flow for opencode users.
- **(−)** Can clutter the transcript if many approvals stack. Mitigation: collapse old cards or limit visible count (future).
- **(−)** If the transcript is long, the user may have to scroll to see the card. Mitigation: auto-scroll to the newest approval card on arrival.

## Alternatives considered

- **Modal dialog:** Rejected because hiding the conversation makes it harder to assess whether the tool call is correct. Also, modals add UI mode complexity (focus, escape handling).
- **Prompt-based command (`y`/`n` in the prompt):** Rejected because it's ambiguous—is the user typing a message or responding? A dedicated card is clearer.