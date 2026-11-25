# 0213. Prune redundant env vars and catalog knobs

- Status: Accepted
- Date: 2026-10-11
- Amends: [ADR-0122](0122-per-provider-concurrency-and-rpm-as-catalog-data.md)
  (the `{NAME}_RPM`/`{NAME}_CONCURRENCY`/`ENTANGLEMENT_MAX_CONCURRENCY` env
  tier), [ADR-0144](0144-file-backed-shared-endpoint-state-across-instances.md)
  (its opt-out spelling), [ADR-0196](0196-tool-search-and-lazy-discovery-replace-the-invoke-envelope.md)
  (the `ENTANGLEMENT_TOOL_ADVERTISING` tier),
  [ADR-0204](0204-invoke-fallback-for-client-side-discovery.md) (the
  `config.yml` `discovery:` tier)

## Context

By 0.6 the user surface had grown to 36 `ENTANGLEMENT_*` env vars, seven
individually overridable managed-file paths, and roughly ten
thinking/discovery catalog knobs (#739). Each was added for a good local
reason, but together they made the precedence chains hard to explain
(`env > config > catalog > default` for one knob, `config > catalog` for its
sibling), duplicated settings that already had a `config.yml` key, and turned
wire facts — what an endpoint *accepts* — into things that looked like user
preferences. The audit sorted every knob into keep, internalize, or remove;
the maintainer approved the removal list before any code changed.

## Decision

1. **Removed env vars** (the setting survives where noted):
   - `ENTANGLEMENT_ENABLE_BASH` — dead since ADR-0195; docs only.
   - `{NAME}_BASE` — the legacy alias of `{NAME}_API_BASE`, which stays.
   - `ENTANGLEMENT_SESSION_RETENTION_DAYS`, `ENTANGLEMENT_TOOL_ADVERTISING` —
     the `config.yml` keys `session_retention_days`/`tool_advertising` remain
     and are now the top tier.
   - `{NAME}_RPM`, `{NAME}_CONCURRENCY`, `ENTANGLEMENT_MAX_CONCURRENCY` —
     rate limits are catalog data only (`rpm:`/`concurrency:` in
     `providers.yml`), exactly the property ADR-0122 argued for; the env tier
     above it was a second way to say the same thing.
2. **Folded** `ENTANGLEMENT_NO_SHARED_ENDPOINT_STATE`, `ENTANGLEMENT_ECHO_FULL`
   and `ENTANGLEMENT_LOG_BODIES` into one **`ENTANGLEMENT_DEBUG`** comma list
   (`bodies`, `echo`, `no-shared-state`; trimmed, case-insensitive; an
   unknown token earns one startup warning). Parsed in the leaf crate
   (`entanglement_provider::debug::DebugFlags`) because two of its three
   readers live there. `ENTANGLEMENT_SHARED_STATE_DIR` stays only as an
   undocumented subprocess-test hook; in-process callers inject a
   `SharedStateLocation` (#743).
3. **Moved** `ENTANGLEMENT_TUI_NOTIFY`/`ENTANGLEMENT_TUI_NO_MOUSE` to
   `config.yml` **`tui: {notify, mouse}`** — a new `config::tui::TuiConfig`
   block (`#[serde(default, deny_unknown_fields)]`, deep-merged per field
   across layers, provenance under `tui` in `skutter inspect config`) that
   `theme`/`keybindings` (#735) and `statusline` (#762) extend with further
   fields. The TUI re-resolves the layered config on every definitions-watcher
   notice and re-applies the block live (mouse capture toggles only on a real
   change; `notify` applies to the next signal); a config that stops parsing
   keeps the previous settings.
4. **One `ENTANGLEMENT_STATE_DIR`** replaces the seven managed-state `*_FILE`
   overrides (grants, agent-models, agent-generation, aux-models,
   extra-roots, mcp-tokens, llm-tokens): the files are siblings by design and
   nobody needs to scatter them. Default `${config_dir}/entanglement`. The
   hand-edited inputs keep their own overrides and never move with it:
   `CONFIG`/`PROVIDERS`/`ENV` files, `AGENTS`/`SKILLS` dirs,
   `PREAMBLE`/`BRIEF`. Tests reach the stores through `load_from`/`at`
   injection seams instead of process env (#747). The watcher watches the
   state dir itself.
5. **Dropped the `config.yml` `discovery:` per-provider override map.** The
   client-side discovery strategy says whether a wire accepts an undeclared
   function call — a fact, not a taste — so the catalog decides: model entry
   > provider entry > `append`. The TUI `/set` dialog still re-pins a running
   session's strategy live; its "save as default" now saves only the
   advertising mode.
6. **Wire-fact catalog knobs stay catalog data**: `thinking_style`,
   `thinking_format`, `thinking_control`, `thinking_required`,
   `replay_thinking`, `effort_tiers`, `supports_temperature`,
   `web_search_tool_version`, `prompt_cache_key`, per-model
   `tool_advertising`/`discovery`, `rpm`/`concurrency`. Their user docs move
   to an "advanced catalog" section of the provider architecture doc, and
   `skutter inspect config` lists every non-default value per
   provider/model so they stay visible.
7. **One-release migration warnings** (`config::retired`): every retired env
   var still set at startup — including one parked in the managed `.env` —
   prints exactly one `skutter: warning:` line naming its replacement; the
   per-provider suffixes are checked against the catalog's entry names. A
   retired `config.yml` key is stripped from its layer *before* the merge
   (so `deny_unknown_fields` doesn't turn a stale line into a startup
   failure) and warns once per file. Printed with `eprintln!` like the
   scaffold notices — visible without `--verbose`, and still in the
   scrollback after a TUI session. Remove the table the release after.

## Consequences

- Breaking for anyone setting a removed var; the warning names the fix, and
  the 0.7 migration guide (#729) carries the full env → replacement table.
- Every surviving precedence chain is shorter: `config > catalog > default`
  for tool advertising, `catalog model > catalog provider > append` for
  discovery, `user providers.yml > embedded` for rate limits, and
  `{NAME}_API_BASE > catalog base_url > client default` for endpoints.
- A process-global `AtomicBool` mirrors `tui.mouse` for the `$EDITOR`
  round-trip, which re-enters the alternate screen with whatever the setting
  is now (one TUI per process, like the terminal it configures).

## Alternatives considered

- **Keep the env tiers as hidden overrides.** Rejected: an undocumented
  override is still a precedence tier someone has to reason about when a
  value surprises them.
- **A `[state]` section in `config.yml` for managed-file paths.** Rejected:
  `config.yml` itself is a hand-edited input; one env var keeps "where my
  state lives" independent of which config layers are present.
- **Hard-fail on a retired knob.** Rejected for one release: a warning lets
  an upgraded binary keep working while the user migrates.
