# Gates & host tools §8 — `web_fetch` and undo/rewind

> Part of the [gates & host tools module](../gates-and-host-tools.md) of the [architecture overview](../../architecture.md).

### `web_fetch` — SSRF-guarded page fetch — [ADR-0220](../../adr/0220-web-fetch-ssrf-guard-and-clipboard-image-paste.md) (#736), [ADR-0225](../../adr/0225-web-fetch-grades-redirect-hops.md) (redirect grading, #808)

`web_fetch { url, format?: markdown|text|raw, max_bytes?, offset? }`
(`host::web_fetch`, behind the default-on `web-fetch` feature) fetches one
http(s) URL. HTML becomes Markdown (`htmd`; script/style/head/svg dropped),
`text` also strips link/image targets, `raw` is verbatim. Output is paged at
the 32 KiB host-tool cap with an `offset=` pointer. A non-text body is never
inlined: `[binary omitted: mime, size, url]`. Registered at startup and part
of the advertised kernel ([ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md): with `describe` gone every model-facing host
tool must be).

**Grading.** It declares `Capability::Exec`: outbound network to a
model-chosen URL is graded like `curl`, not like a host `read`. The shipped
modes give `build` → allow and `research`/`plan`/`auto` → prompt. The
permission argument is the **normalized** URL (`web_fetch::normalize_url`:
WHATWG-parsed with the host's trailing dot stripped), the same string the
fetcher requests. So `modes: research: allow:
["web_fetch(https://docs.rs/*)"]` widens it for one site, and a deny cannot be
dodged with `HTTPS://`, a trailing dot, or `:443`. Write rules as a literal
`scheme://host/` prefix: rule globs let `*` match `/`, so a host wildcard
(`https://*.example.com/*`) is unsafe.

**Redirect hops are graded like the initial URL**
([ADR-0225](../../adr/0225-web-fetch-grades-redirect-hops.md), #808). The
executor scopes the call's own grading context around every graded dispatch.
It is a tokio task-local, `tool_runner::call_grader`, and holds the same
resolver, ancestor chain, grants, overlay and ceiling as `dispatch`, sharing
one `grade::grade_call`. `web_fetch` grades each normalized hop through it
before following the hop (`WebFetcher::fetch_checked`):

- `Allow` follows the hop.
- `Deny` stops the fetch with an error naming the mode.
- `Ask` stops the fetch with a hint to call `web_fetch` on the target
  directly, which goes through ordinary approval. Nothing parks mid-fetch.

`tool_runner::grade_in_scope` is the public seam for any tool that reaches
targets beyond its own argument. Outside an executor no grader is in scope,
and hops are followed under the SSRF guard alone.

**Hostile HTML.** `htmd` recurses without a depth limit, and html5ever's
tree builder is quadratic in nesting depth. The conversion is guarded in
layers:

- **Tokenizer pre-scan.** html5ever's linear spec tokenizer runs first and
  builds no tree. An open-element stack estimates the nesting and collects
  the page text. A quoted `>` can't fool it. Inside `<svg>`/`<math>` it
  never switches to raw text, just as the tree builder doesn't. The scan is
  amortized O(1) per tag. A page above 256 levels, or with more than 50,000
  start tags, is answered with that text and is never parsed into a tree.
- **Real-tree check.** Otherwise the rcdom tree is built and its real depth
  is measured with an iterative walk. Above the limit, its text is extracted
  iteratively instead of handing the tree to `htmd`.
- **Isolation and limits.** Everything runs on its own 64 MiB-stack thread,
  never on a tokio worker. At most 2 conversions run at once, and each holds
  its slot until its thread really exits, even after a timeout. Waiting for
  a slot and converting share one 5 s budget. When that budget runs out, the
  tool falls back to the tokenizer text.

The code is in `host/web_fetch/html.rs`.

**SSRF policy** (`entanglement_provider::web_fetch`, re-exported as
`entanglement_core::web_fetch`):

- Only `http`/`https`. URLs with userinfo credentials are refused.
- Blocked: unspecified, `0/8`, loopback, RFC 1918, CGNAT `100.64/10`,
  link-local `169.254/16` (cloud metadata), `192.0.0/24`, documentation and
  benchmarking ranges, multicast, broadcast, `240/4`, `::`, `::1`, ULA
  `fc00::/7`, `fe80::/10`, `fec0::/10`, `2001:db8::/32` and Teredo. An
  IPv6 address that embeds an IPv4 one is judged by that IPv4 address. That
  covers IPv4-mapped, IPv4-translated (`::ffff:0:0:0/96`), IPv4-compatible,
  NAT64 (`64:ff9b::/96`), NAT64 local-use (`64:ff9b:1::/48`, where both the
  `/48` and `/96` embeddings are checked) and 6to4.
- An IP-literal host is checked after WHATWG parsing, so `2130706433`,
  `0x7f.1` and `[::ffff:127.0.0.1]` are all caught. `localhost` and
  `*.localhost` are refused by name.
- The client's own DNS resolver refuses the whole answer if any address is
  blocked. The addresses it vets are exactly the addresses the connector
  dials, so DNS rebinding has no second lookup to exploit.
- Redirects are followed manually, at most 5, and every hop is validated
  again.
- No proxy is ever used, and proxy environment variables are ignored: a
  proxy would resolve the name itself and bypass the guard.
- Timeouts are 10 s to connect, 30 s per request and 60 s for the whole
  chain. The byte cap defaults to 2 MiB and is clamped to 10 MiB.
- The client is built from `client_builder()` (shared TLS) but is not the
  pooled `HttpClient`, because the resolver is a client-wide setting. The
  endpoint pool would also create a persisted state entry for every site
  fetched.
- Tests reach `127.0.0.1` fixtures through a `#[cfg(test)]`-only guard
  field. No production build has a bypass.

### Undo / rewind — per-write file backups — [ADR-0212](../../adr/0212-undo-rewind-via-per-write-file-backups.md) (#731)

`write`, `edit` and `apply_patch` — and therefore the `rhai` `write`/`edit`
bindings, which run those same registry tools — back up what they replace
when built `with_backups(FileBackups)` (`host::host_tools_with` + `HostToolOptions::backups`;
`skutter` always wires it, an embedder opts in). Only a session's run
(`run_for_session`) is recorded; a bare `run` is not.

- **Storage** (`file_backup.rs`): under the root session's
  `SessionStore::backups_dir` — `<store>/backups/<root_id>/` — a
  content-addressed `blobs/<sha256>` (write-then-rename, deduped per root) and
  an append-only `manifest.jsonl`: one `change` line per mutation
  `{id, ts, session, request_id, exec_seq?, path (absolute, resolved),
  display, before: absent | stored{hash,size} | skipped{reason}, after:
  sha256 | null, mode?}` plus `reverted{ids, attempted}` markers; the id
  counter and blob set are cached per root, so a write never re-reads the
  manifest. A spawned child's changes land in its root's directory
  (`FileBackups::track_roots` folds `SessionStarted` lineage, and `Status`
  for the busy check below).
  Backups ride the log's lifecycle: `SessionStore::delete` removes them with
  the `.jsonl`, and `prune` removes any backup directory whose log is gone —
  session retention is the only retention.
- **Caps**: a prior version over `max_file_bytes` (default 8 MiB), or past a
  root's `max_root_bytes` stored budget (default 256 MiB), is recorded as
  `skipped` (the change is still listed, so a revert names it as "can't
  restore … no backup (…)"). A backup failure is logged, never a tool error —
  the edit already happened.
- **Configuration** (#787): `config.yml` `undo: {enabled, max_file_bytes,
  max_root_bytes}` (`config/undo.rs` owns the size grammar: bytes or
  `B`/`KB`/`MB`/`GB`/`KiB`/`MiB`/`GiB`, 1 byte – 64 GiB, `0` rejected in
  favor of `enabled: false`) deserializes into
  `file_backup::BackupLimits`; `boot.rs` builds `FileBackups::with_backup_limits`
  from it, and the definitions watcher re-applies it on every config change
  via `FileBackups::set_limits` (a config that no longer parses keeps the
  previous limits). Limits are read per record, so a change applies to the
  next write; nothing already recorded is re-evaluated. `enabled: false`
  stops recording (no manifest line at all — a later revert treats such a
  file as "changed since"), while backups taken before stay restorable; a
  revert that finds nothing left to restore while disabled fails with
  "undo disabled" instead of an empty report. `skutter inspect config` shows
  the effective block and its winning layer.
- **Turns**: the executor runs each permission-graded/`rhai` call task under
  `file_backup::with_exec_seq(ToolExec.seq)`, so a change records the exact
  `(session, seq)` of its call and belongs to the turn of the session's last
  user `Prompt` logged before that record. Request ids are never the key
  (Gemini's `name#ordinal` repeats every turn). Without a seq (a detached
  `rhai` script): the latest `(session, request_id)` call logged no later
  than the change (`<id>:rhai:<tool>` → outer call), then timestamps.
- **Revert** (`FileBackups::revert`, behind core's `EngineConfig::file_reverter`
  seam — see [protocol](../protocol.md) `RevertFiles`/`FilesReverted`): refused
  with an `Error` while any session of the sub-tree is mid-turn or running
  detached work (`file_backup::detached_work_probe` over the job and script
  registries, wired in `boot.rs`). Otherwise every unreverted change of the
  session and its spawn sub-tree after the target turn is grouped per file;
  the file goes back to the *first* change's `before` (rewritten through a
  synced, randomly named temp file created `create_new` + `O_NOFOLLOW` — no
  planted symlink is ever written through — then rename + directory fsync,
  keeping its mode; a file whose directory was since swapped for a symlink
  is refused as "its directory moved"; or deleted when the agent
  created it; recreated when the agent deleted it) **only if** it still
  hashes to the *last* change's `after` — or counts as restored if it is
  already at `before`. A user edit or a `bash` change is reported as skipped,
  left untouched, and marked `attempted` (still revertable, but undo steps
  past it); a change with no backup is reported once and marked `reverted`.
  A failed marker append is only logged. Out-of-root writes (an approved
  escape-root grant) are covered the same way — the manifest holds the
  resolved absolute path.
- **Not covered**: anything `bash`, `call` or a `rhai` `exec`/`bash` binding
  does to the filesystem, plan files written by `propose_plan`, and
  `!bash` passthrough commands. Every report and the model's note say so.

---

Previous: [§8 — Host tools: output, wiring, orchestration & `poll`](host-tools-contracts.md) · Next: [§8 — One advertised surface, error taxonomy & removed sources](one-surface.md)
