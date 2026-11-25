# 0248. Importable `rhai` library modules

- Status: Accepted
- Date: 2026-10-22
- Issue: #835
- Amends: [ADR-0046](0046-rhai-sandboxed-script-tool.md) (the sandbox had
  "no module resolver"; it now has one that serves only discovered,
  precompiled modules by bare name),
  [ADR-0228](0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)
  (its "Future work" line naming #835 is resolved here, and its write floor
  gains the module dirs)

## Context

A `rhai` script could reuse code only through a skill's `lib.rhai`
(`skill::<skill>::<fn>`, [ADR-0227](0227-one-binding-path-namespaces-http-skill-libraries.md) §5).
That couples a library to a skill and its instructions, so the runtime
could not ship a general helper, and a user could not keep one helper for
all their scripts. `import` failed with "module not found", because
ADR-0046 kept the engine resolver-free so that `import` could not reach the
filesystem.

## Decision

1. **Layers.** Modules come from three layers. Later layers win on a name:
   - built-in modules, embedded in the binary with `include_str!`;
   - user: `${config_dir}/entanglement/rhai/modules/`;
   - project: `<root>/.entanglement/rhai/modules/`.

   A module is a `<name>.rhai` file, and a sub-directory gives a slash name
   (`net/util.rhai` is `"net/util"`). There is no versioning and no env
   override. [ADR-0213](0213-prune-env-vars-and-catalog-knobs.md) pruned the
   env vars, and these dirs need none. An explicit `UserLayer::Dir` (a test
   or an embedder) has no user module layer, so it stays off the real user
   dirs. Discovery lives in `skills::rhai_modules`. The result rides in
   `SkillRegistry::modules`, so the existing definitions watcher reloads it,
   and `load_skill` and every run read the same snapshot. Of the `*.rhai`
   files, only a skill's `lib.rhai` and a file under a `rhai/modules/` dir
   count as definition content. A scratch or trusted-folder script triggers
   no reload.

2. **Import by bare name only.** The import form is
   `import "json_schema" as js;`.
   - A name is `[a-z0-9_]+` segments joined by `/`. `..`, absolute paths,
     extensions and anything else are refused with an error that says so.
   - Each run installs `script::modules::Resolver`. It looks a name up in
     the discovered set and never touches a file, so inline and `path`
     scripts behave the same and there are no relative imports.
   - An unknown name and a rejected module are refused with a catchable
     error. The unknown-name error points at `load_skill rhai`.
   - `set_max_modules(32)` caps the `import`s one run executes. Rhai counts
     every execution, so an `import` inside a function counts on every
     call.

3. **Module rules.** A module may hold `fn` definitions and top-level
   `import "<name>" as x;` statements (literal name, alias required). Any
   other top-level statement rejects the whole module with a warning, as
   for `lib.rhai`: code that ran at import time would run with nobody having
   asked for it. An `import` inside a module function is rejected too. It
   would skip the discovery graph: it would not be listed, not be
   cycle-checked, and fail only when called.
   - A **typed method** (`fn int.dbl()`) rejects the module, in every
     module. An importing module's functions are registered by name and
     arity (§4), which leaves a typed method uncallable in every form while
     `load_skill` still listed it. One rule for all modules is simpler than
     one that flips when a module gains its first import. A skill's
     `lib.rhai` keeps typed methods: it has no top-level import (a
     top-level statement rejects it), so it is always built by
     `eval_ast_as_new`, where they work. An `import` inside one of its
     functions is allowed: it is not part of the discovery graph and
     resolves per run, through the run's resolver and under its
     `max_modules` cap.
   - A **repeated alias** (`import "one" as x; import "two" as x;`)
     rejects the module: a module's environ would answer `x` with the last
     import, a script's scope with the first.
   - Modules are compiled once at discovery, with parse depths pinned to
     Rhai's release defaults so debug and release builds agree.
   - Their functions are encapsulated, so they resolve sibling functions
     inside the module. They resolve the engine's bindings (`read`,
     `http`, `mcp::…`) when called. Module code therefore runs in the
     calling script's posture: every binding it calls is graded as the
     caller's own call, however deep the import chain. Nothing is graded at
     discovery.
   - A file that fails these rules still claims its name. It shadows the
     lower layer and is listed as not importable, so a broken override is
     loud instead of silently falling back.

4. **Module-to-module imports are resolved at discovery**
   (`skills::rhai_modules::graph`), not per run.
   - Modules load in dependency order. Each one is handed exactly its
     already-loaded imports, so a diamond's shared module is a single
     `Shared<Module>` that every importer holds.
   - Names resolve through the winning layer at every level. If the project
     defines `strings`, a user or built-in module that imports `strings`
     gets the project's copy.
   - A **cycle** rejects every module in it, a self-import included. One
     warning names the cycle's path (`a → b → c → a`).
   - An import chain deeper than **8** rejects the module.
   - A module that imports a missing or rejected module is rejected too.
     The warning names that dependency and its winning layer.
   - Resolution is iterative and linear. Modules load in Kahn's order: each
     waits on its pending imports and becomes ready when none is left. When
     nothing is ready there is a cycle, which a walk of at most one step per
     pending module finds. Nothing recurses.
   - A module that imports is **not** built with Rhai's
     `eval_ast_as_new`. That call makes every import a sub-module, and
     indexing recurses through every sub-module path. Across an import
     lattice that is `k^depth` entries even when the shared modules are one
     instance: 9 levels of 6 took 8 s, enough to hang boot or a reload on a
     cloned repository. Instead `lib_rhai::with_imports` registers each
     public function as a `RhaiFunc::Script` (by name and arity, so typed
     methods are refused, §3) whose encapsulated environ holds the AST's own
     functions and the aliased imports. That is all
     `alias::f()` needs. The module has no sub-modules, so indexing is
     linear. A regression test loads 9 levels of 7 in well under a second.
     Modules without imports, and skill libs, still use `eval_ast_as_new`.
     That code rests on Rhai internals, so the workspace pins `rhai =1.25.1`.

5. **Discovery goes through `load_skill rhai`.** The `rhai` guide's
   `load_skill` result ends with the modules. Each entry gives the name,
   the winning layer, the module's imports, and each public function's
   signature with its `///` doc (or the reason it is not importable). The
   tool spec stays static. `BINDING_REFERENCE` and the guide drop "import
   is disabled" and gain one line: "`import` a module by name;
   `load_skill rhai` lists them". There is no new tool, and the advertised
   surface is unchanged
   ([ADR-0226](0226-one-advertised-surface-mcp-envelope-registered-is-available.md)).
   The spec text changes once, at a release boundary.

6. **The write floor gains the module dirs.** An agent `write`, `edit` or
   `apply_patch` of a `*.rhai` file under the user or project module dir
   always asks, even before the dir exists (it is matched through its
   nearest existing ancestor). The dirs are wired by
   `ToolExecutorBuilder::module_dirs`, fed from `skills::module_dirs`.
   `bash` and MCP filesystem servers stay the documented gap
   (ADR-0228).

7. **One built-in module, `json_schema`.** It is written in Rhai and
   embedded. `js::validate(value, schema)` returns an array of
   `"<path>: <problem>"` strings with JSON-pointer paths (`/` is the root);
   an empty array means the value is valid.
   - Supported subset: `type` (one type or a list), `required`,
     `properties`, `additionalProperties` (bool), `items`, `enum`, `const`,
     `minimum`/`maximum`, `minLength`/`maxLength`, `minItems`/`maxItems`,
     and `true`/`false` schemas.
   - Not supported: `pattern`, `$ref` and formats.
   - A supported keyword of the wrong shape is reported as an error rather
     than ignored. Examples: `"minimum": "0"`, a non-string `required`
     entry, an unknown or empty `type`, an `items` that is neither a schema
     nor a bool. Each sub-schema is checked when validation reaches it, so
     a malformed schema under a property the value lacks goes unreported.
   - It runs on the caller's engine, so the sandbox limits bind it.

## Alternatives considered

- **A filesystem resolver (`FileModuleResolver`) rooted at a modules
  dir.** Rejected. Path resolution, relative imports and per-run
  compilation would reopen what ADR-0046 closed, and the import would bypass
  the write floor's notion of "trusted code".
- **Modules are leaves (no module-to-module import).** Rejected by the
  user. Shared helpers would then be copy-pasted between modules.
- **Imports only to the same or a lower layer.** Rejected by the user.
  One name would mean different modules at different levels. "Shadowing
  applies everywhere" is the simpler rule: a broken shadow breaks its
  importers loudly, and the warning names the culprit.
- **Resolving module imports per run, through the run's resolver.**
  Rejected. Cycles would be found only when hit, they would cost
  `max_modules` budget on every call, and a diamond would load twice. For
  the same reason, an `import` inside a module function is refused.
- **Capping the transitive sub-module paths instead of avoiding them.**
  Rejected. It would reject legitimate wide modules, and it would still pay
  for indexing up to the cap.
- **Listing modules in the tool spec.** Rejected. A dynamic spec busts the
  prompt cache on every module change
  ([ADR-0202](0202-prompt-cache-discipline-anchors-deferral-replay-compaction-date.md)).

## Consequences

- Scripts and skill libs can share vetted helpers without a skill, and
  the runtime can ship helpers such as `json_schema`.
- A module dir is trusted code, as `rhai.trusted` folders and skill dirs
  are. It must not be agent-writable through `bash`.
- A `lib.rhai` edit now also triggers a definitions reload. Before, no
  `*.rhai` file was fingerprinted.
- Embedders gain `skills::{module_dirs, RhaiModules, rhai_modules}`,
  `SkillRegistry::modules`, `ToolExecutorBuilder::module_dirs` and
  `LiveDefinitions::with_sessions`. With no module dirs wired, there is no
  floor for them. With no sessions store, a reload's prompt context names
  the project's default scratch dir, as before. The injection lets the
  watcher's end-to-end test stay off the real data dir.
