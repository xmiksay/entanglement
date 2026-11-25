# Architecture Decision Records

An ADR records *why* a hard-to-reverse decision was made. The
[architecture docs](../architecture.md) describe *what is*; ADRs carry the
reasoning and the rejected alternatives they cannot.

## When to write one

Only for a **hard-to-reverse boundary, protocol or security decision**: a crate
boundary or the embedder API, a wire/protocol shape, the permission or trust
model. Not for a feature, a refactor, a bug fix or local naming, and not one
per PR.

ADRs are **topic ADRs**: one record per subject (the agent loop, the tool
surface, scripting, the provider layer, …). A topic ADR is amended by a new
ADR that links back to it, never edited per PR. It stays `Proposed` until the
decision is verified in practice (adoption or live runs), then `Accepted`.

## Format

File `NNNN-kebab-case-title.md`, numbered after the archive (the next is
`0250`), so `ADR-NNNN` stays unambiguous:

```
# NNNN. Title
- Status: Proposed | Accepted | Amended by ADR-XXXX | Superseded by ADR-XXXX
- Date: YYYY-MM-DD

## Context
## Decision
## Consequences
## Alternatives considered
```

## Index

One line per ADR.

| # | Title | Status |
| --- | --- | --- |

_No topic ADRs yet._

## Archive

ADR-0001 … ADR-0249, the log before the 0.8 cleanup, live unchanged in
[`archive/`](archive/). [`archive/MAPPING.md`](archive/MAPPING.md) maps each
one to its title, current status (current / superseded / removed / changed /
inconsistent) and the topic ADR that replaces it.
