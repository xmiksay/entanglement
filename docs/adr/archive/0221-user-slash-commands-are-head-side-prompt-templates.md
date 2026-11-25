# 0221. User slash commands are head-side prompt templates

- Status: Accepted
- Date: 2026-10-11
- Extends: [ADR-0074](0074-cross-vendor-skill-and-agent-discovery.md)
  (cross-vendor layered discovery), [ADR-0194](0194-skills-are-additive-only.md)
  (definitions never narrow or widen authority)

## Context

Claude Code (`.claude/commands/*.md`) and opencode (`.opencode/command/*.md`)
let a user save a prompt as a file and run it as `/name args`. Issue #733 asks
for the same in entanglement: layered like skills, cross-vendor, shown in the
TUI slash popup and `skutter inspect`. Both tools also let a command do more:
pick an agent or model, run shell (`` !`cmd` ``), inline files (`@path`), and
(Claude Code) set `allowed-tools`.

## Decision

1. **A command is head-side sugar for typing a prompt.** The head expands
   `/name args` and sends the result as an ordinary `InMsg::Prompt`. Nothing
   reaches core or the wire protocol, and the session log records the
   expanded text, so replay and resume need no knowledge of commands. The
   TUI shows a one-line notice naming the command's file, then the expanded
   prompt (the same text the log replays). `skutter run "/name args"` expands
   too, so a command means the same in every head that takes typed input.
   `pipe`/`serve` take protocol frames, not typed text, and do not expand.

2. **Discovery reuses the skill/agent layers.** `layers::candidate_dirs` with
   kind `commands`: user `~/.claude/commands`, `${config_dir}/opencode/command`
   and `…/commands` (lenient), then `${config_dir}/entanglement/commands`
   (strict); project `.claude/commands`, `.agents/commands`,
   `.opencode/command` and `…/commands` (lenient), then
   `.entanglement/commands` (strict). `ENTANGLEMENT_COMMANDS_DIR` replaces the
   whole user layer. Later wins by name. No embedded built-ins.

3. **Names are relative paths with `:` namespaces.** Every `*.md` under a dir
   is a command; `frontend/review.md` is `/frontend:review`, Claude Code's
   separator. A name with whitespace is invalid.

4. **Frontmatter is optional.** Keys: `description` (default: the body's first
   non-empty line) and `argument-hint` (`argument_hint` accepted). `agent`,
   `model` and `allowed-tools`/`allowed_tools` parse but are ignored: an
   agent is fixed for a session's life (ADR-0207 §9), `/model` is the one
   model switch, and the permission mode is the only authority (ADR-0194's
   rule, applied to commands). A native file carrying one gets a load
   warning; a foreign file does not, since those keys belong to its own tool.
   Native files reject unknown keys; foreign files ignore them.

5. **The body is pure text.** `$ARGUMENTS` is the whole trimmed argument
   string; `$1`..`$9` are shell-style positional args (whitespace splits,
   quotes group, backslash escapes), empty when missing. Substituted text is
   never rescanned. A body with no placeholder gets non-empty args appended
   as `ARGUMENTS: …`. `` !`cmd` `` and `@file` are not supported and are sent
   literally — invoking a command can never run or read anything.

6. **Built-in TUI commands always win.** A user command named like one
   (`slash_commands::RESERVED_NAMES`, kept equal to the TUI roster by a unit
   test) is dropped with a load warning in every head, and `inspect commands`
   marks it as hidden.

7. **Live reload** rides the definitions watcher (ADR-0084): the command dirs
   are watched and a reload swaps the runtime-held registry the heads read on
   every use.

## Consequences

- `skutter inspect commands [name]` and a Commands tab in the TUI `/inspect`
  overlay list name, layer, source and description.
- A malformed native command file aborts startup, as a malformed skill does.
- A user who wants shell output or a file in a command's prompt must ask the
  model for it; the permission mode then grades the tool call as usual.

## Rejected

- Running `!` shell / `@file` inclusion at expansion time: it would execute
  outside the permission mode and approval flow.
- Honouring `agent`/`model`: a command would need to spawn a new session or
  switch the model for one turn — new protocol surface for sugar.
- Expanding inside core: commands would leak into the protocol and the log
  would need to record the template and the arguments to replay them.
- `/` as the namespace separator (opencode's choice): it clashes with the
  slash popup's path-like token handling and with Claude Code's documented
  `:` form.
