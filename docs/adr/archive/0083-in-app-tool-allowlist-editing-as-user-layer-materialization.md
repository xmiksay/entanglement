# 0083. In-app tool-allowlist editing materializes a user-layer override

- Status: Accepted
- Date: 2026-07-15
- Builds on the layered agent loader of
  [0034](0034-file-based-agent-definitions.md) (embedded < user < project, later
  wins on `name`), the physical tool mask of
  [0038](0038-physical-per-agent-tool-restriction.md) (`tools`/`disallowed_tools`),
  the shared atomic writer of
  [0081](0081-per-profile-model-pinning-and-rebind-on-set-agent.md)
  (`config::atomic`), and the local trust boundary of
  [0047](0047-local-trust-boundary.md) (a written user-layer file is trusted like
  any other definition). Issue #330.

## Context

There is no way to change a primary agent's `tools:`/`disallowed_tools:` mask
without hand-authoring a markdown definition file — and a built-in (`build`,
`plan`, `explore`) has no file to edit at all, since its definition is
`include_str!`-embedded into the binary. Meanwhile the layered loader already
supports shadowing any definition, built-in included, by dropping a same-`name`
file into a higher-precedence layer. The gap is purely one of ergonomics: no
in-app way to *write* that shadow.

## Decision

Editing a profile's tool allowlist in the TUI **materializes a user-layer
override file**, not a new config surface. It reuses the existing precedence
system end to end rather than adding a parallel mask store (unlike the
per-profile model pin of [0081](0081-per-profile-model-pinning-and-rebind-on-set-agent.md),
which needed a *managed* sibling file because a model pin isn't itself a
layerable definition — a tool mask already is one).

### Materializer (`entanglement-runtime::agents::materialize`)

- `winning_raw_text(root, name)` resolves the *currently effective* definition's
  raw text using the same precedence `load_registry` applies (built-in < user <
  project, later wins) — a built-in's embedded source, or an existing
  user/project file's exact text. This is the seed: editing a shadowed
  definition further edits the shadow, not the original.
- `rewrite_tools(raw, allowed: Option<&[String]>)` is a pure text transform: it
  parses only the frontmatter into a `serde_yaml::Mapping` (order-preserving —
  `serde_yaml::Mapping` is `indexmap`-backed) and rewrites just the `tools:` /
  `disallowed_tools:` keys, leaving every other key (including nested ones like
  `permission:`) and the body untouched. `None` drops the `tools:` key
  entirely (inherit-all); `Some(list)` sets an explicit allowlist (an empty
  list is a valid "deny everything" mask, distinct from `None`).
  `disallowed_tools:` is always dropped — the TUI checklist resolves straight to
  the final allowed set, so a separate denylist would be redundant and could
  silently re-subtract from it on a later hand-edit.
- `save_tools_override(root, name, allowed)` writes the rewritten text to
  `${config_dir}/entanglement/agents/<name>.md` (or wherever
  `ENTANGLEMENT_AGENTS_DIR` points, which replaces the whole user layer) via the
  shared `config::atomic::atomic_write` — the same primitive the agent-models
  and env-key writers use.

### TUI flow

From the `/agent` picker, `e` on the highlighted profile opens a single-stage
checklist dialog (`tui::tools_dialog::ToolsDialog`, state-only, mirroring the
`/key` dialog's dedicated-module pattern) over the full advertised tool roster.
The roster is `EngineConfig.tool_specs` captured in the runtime head right after
`build_config` assembles it (before `Holly::spawn` consumes the config) — not
`ToolRegistry` alone, so it also covers the runtime-owned specs
(`update_tasks`/`ask_user`/`rhai`) that are maskable via `tools:` but aren't
registry tools. Each row's checkbox seeds from the profile's current effective
mask (an omitted allowlist starts every row checked); `Space` toggles, `Enter`
saves via `save_tools_override` + a transcript status line, `Esc` discards. The
dialog draws over the still-open picker rather than closing it, so editing
several profiles in a row doesn't require re-opening `/agent` each time.

### What this deliberately does not do

- **No live reload.** The engine's loaded `ProfileRegistry` is immutable for the
  process lifetime; the new mask applies on the next restart (or once the
  companion inotify-watcher issue lands and the registry becomes live-reloadable
  — a separate concern from *writing* the override). The saved status line says
  so explicitly.
- **No editing of prompts/permissions/skills in the dialog.** The materialized
  file is an ordinary hand-editable definition — `skutter inspect agents`
  already reports which layer won and what it shadowed, so provenance stays
  visible for anyone editing it further by hand.

## Consequences

- One mechanism (the layered loader) serves both hand-authored and in-app-edited
  definitions; there is no second "effective mask" representation to keep in
  sync with the frontmatter.
- A user who has already project-layer-shadowed a profile (higher precedence
  than the user layer this writes) will find the in-app edit has no visible
  effect until the project shadow is also updated — an accepted MVP gap, not
  silently hidden (`skutter inspect agents` surfaces the shadowing).
- Reusing `config::atomic::atomic_write` means the write is crash-safe (temp
  file + rename) for free, consistent with every other managed/materialized file
  in the runtime.

## Rejected alternatives

- **A managed sibling file** (`agent-tools.yml`), mirroring
  [0081](0081-per-profile-model-pinning-and-rebind-on-set-agent.md)'s
  `agent-models.yml`. Rejected because a tool mask, unlike a model pin, is
  already part of the *definition* — a second store would either have to also
  win over frontmatter (redefining `load_registry`'s precedence rules for just
  this one field) or fight the layered loader for authority over the same
  logical value. Materializing an override file keeps one source of truth.
- **Byte-level line-splicing** (mirroring `config::env_key::upsert`'s exact
  line-preserving replace) instead of a `serde_yaml::Mapping` round-trip.
  Rejected: `tools:`/`disallowed_tools:` are YAML sequences that can be written
  in block or flow style, span multiple lines, or not exist yet, unlike
  `env_key`'s single `KEY=VALUE` line — a general YAML-aware key rewrite is
  simpler and more robust than hand-rolled multi-line list splicing, and
  `serde_yaml::Mapping`'s insertion-order preservation already delivers the same
  "touch only what changed" property for the keys that matter.
