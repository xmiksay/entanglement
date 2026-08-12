# Releasing (maintainers)

How to cut a tagged release and how the automated crates.io publish works
(issue #362). Two audiences: tagging is routine (any maintainer, any release);
the crates.io Trusted Publishing setup below is a **one-time** step done once
per crate by whoever owns them on crates.io.

## Pre-tag audit (standing step)

Before cutting *any* tag — not just major/minor releases — sweep for the
failure modes that keep recurring (each observed at least once by past
audits): CHANGELOG entries
lagging shipped commits, the README/`protocol.md` contract block lagging
`protocol.rs`, and ADR back-links missing on amendment. A green `make verify`
is **not** a release signal by itself — it proves the code compiles and tests
pass, not that docs/ADRs/open issues reflect what actually shipped. Concretely, before the version bump below:

1. Diff the wire contract (`InMsg`/`OutEvent` variants in `protocol.rs`)
   against the contract blocks in `README.md` and `docs/architecture/protocol.md`.
2. Skim `git log <last-tag>..HEAD` for any ADR that superseded/amended another
   and confirm the back-link landed on both sides.
3. Review open issues for stale blockers or entries that shipped since they
   were filed.
4. Re-check the six audit tracks from issue #560 (stability/robustness,
   provider/endpoint sharing, default-setup usability, design gaps, release
   mechanics, and general docs drift) empirically — by inspection and repro,
   not just by reading code — since a passing test suite doesn't exercise
   every one of these paths.

## Cutting a tag

Two mandatory steps come first, committed before tagging:

1. **Bump the version** in the workspace `Cargo.toml` — `workspace.package.version`
   **and** the two `workspace.dependencies` pins for `entanglement-core` /
   `entanglement-provider` (they carry an explicit `version` alongside `path` so
   `cargo publish` resolves the registry dep). All three must agree, or `make tag`
   refuses on the mismatch.
2. **Write the changelog — at release time only.** `CHANGELOG.md` is **never**
   edited by feature/fix PRs (concurrent PRs each appending to an `[Unreleased]`
   section conflict on every merge). Instead, generate the whole `[X.Y.Z] -
   YYYY-MM-DD` section now, in the release commit, from `git log
   <last-tag>..HEAD --oneline` plus the release's closed epic/issues — group
   into `### Added` / `### Changed` / `### Fixed`, call out wire-shape changes
   explicitly, and add the version's link reference in the footer. No
   `[Unreleased]` section accumulates between releases.

`make tag` enforces both: it refuses if the `workspace.dependencies` pins
don't match `workspace.package.version`, and if `CHANGELOG.md` has no
`## [X.Y.Z]` section for the target `VERSION`.

Then:

```bash
make tag VERSION=v0.1.1   # refuses on a dirty tree, red `make verify`, or a
                           # VERSION that doesn't match workspace.package.version
git push origin v0.1.1    # separate, explicit — `make tag` never pushes
```

The pushed tag triggers `.github/workflows/release.yml`: `verify` (the full
`make verify` gate) → `coverage` (`make coverage`, fails under `COV_MIN`) →
`publish` (crates.io, see below) — each gated on the previous job.

Convention: tag a `v0.1.x` on any merge an embedder needs, or on a lightweight
cadence — see [`embedding.md`](embedding.md)'s "Pinning a dependency" section
for why embedders want a tag over a raw `rev`.

## Publishing to crates.io (Trusted Publishing)

The `publish` job authenticates via **OIDC Trusted Publishing**
([crates.io docs](https://crates.io/docs/trusted-publishing),
[RFC 3691](https://rust-lang.github.io/rfcs/3691-trusted-publishing-cratesio.html)) —
`rust-lang/crates-io-auth-action` exchanges a GitHub-issued OIDC token for a
30-minute crates.io publish token. No `CARGO_REGISTRY_TOKEN` secret lives in
this repo.

It publishes all three crates **leaf-first** — `entanglement-provider` →
`entanglement-core` → `entanglement-runtime` (the dependency order from
`.claude/CLAUDE.md`'s crate table) — waiting after each for the crate to land
on the crates.io sparse index (`scripts/wait-for-crate.sh`) before packaging
the crate that depends on it; skipping the wait makes the dependent's
`cargo publish` fail resolving the just-published version (confirmed locally:
`cargo publish --dry-run -p entanglement-core` errors with "no matching
package named `entanglement-provider`" until provider is actually on the
registry — a workspace `path` dep only resolves locally for the crate you're
packaging itself, never transitively).

### One-time setup (crates.io owner only)

Trusted Publishing has a chicken-and-egg constraint: **a crate must already
exist on crates.io before you can configure a trusted publisher for it**
(crates.io's own prerequisite — the very first publish of a new crate always
needs a real API token). So bringing this up for the first time is:

1. **Publish all three crates once, manually, with an API token**, in the same
   leaf-first order the workflow uses:
   ```bash
   cargo login   # paste a token from https://crates.io/settings/tokens
   cargo publish -p entanglement-provider
   # wait until it shows up: https://crates.io/crates/entanglement-provider
   cargo publish -p entanglement-core
   cargo publish -p entanglement-runtime
   ```
   Do this from a clean, verified checkout of the tagged commit — whatever you
   publish this way becomes the permanent v0.1.0 on the registry (crates.io
   has no delete, only `cargo yank`).
2. For **each** of the three crates, on crates.io: **Settings → Trusted
   Publishing → Add → GitHub**, and fill in:
   - Repository owner: `xmiksay`
   - Repository name: `entanglement`
   - Workflow filename: `release.yml`
   - Environment: `release` (matches the `publish` job's `environment:` —
     optional, but lets you add required-reviewer/branch protection rules on
     that GitHub environment for extra safety on future publishes)
3. Revoke the manual API token used in step 1 (`cargo logout` locally, and
   delete it from crates.io) — from here on, every subsequent version is
   published exclusively through the `release.yml` `publish` job.

After that one-time setup, every future `vX.Y.Z` tag push publishes all three
crates automatically — no further manual `cargo publish` needed.
