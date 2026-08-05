# 0171. z.ai streaming `web_search` placement confirmed — top-level, final chunk

- Status: Accepted
- Date: 2026-08-06
- Amends [0131](0131-web-search-post-mvp-follow-ups.md) §3 (the one item its
  own "post-MVP follow-ups" left open), which itself amends
  [0075](0075-provider-side-web-search-mvp.md). Issue
  [#625](https://github.com/xmiksay/entanglement/issues/625) (orig. part of
  [#481](https://github.com/xmiksay/entanglement/issues/481), tracked by the
  #396 ledger epic via [#624](https://github.com/xmiksay/entanglement/issues/624)).

## Context

Since the #305 MVP ([0075](0075-provider-side-web-search-mvp.md)), the z.ai
(OpenAI-compat) client's `handle_chunk` has scanned for a `web_search` source
array at **two** candidate sites in every streamed chunk — the chunk's top
level (`data.get("web_search")`) and inside the delta
(`choices[0].delta.web_search`) — because the actual placement had never been
confirmed against a live Coding Plan key. [0131](0131-web-search-post-mvp-follow-ups.md)
§3 recorded that its own attempt was blocked by no key/network access and
left the item open for "whoever closes it."

This ADR is that closure: verified live, twice over, using a working
`ZAI_API_KEY` for the same Coding Plan endpoint
(`https://api.z.ai/api/coding/paas/v4/chat/completions`) this project's
catalog defaults to (`entanglement-provider/src/defaults.yml`).

## Verification result

**Request shape.** Mirrored `openai::request::web_search_tool_entry` exactly:
a `{"type":"web_search","web_search":{"enable":true,"search_result":true}}`
entry in the `tools` array, `model: glm-5.2`, `stream: true`.

**First finding — invocation is model-decided, not guaranteed by the tool
being offered.** Several early attempts streamed a complete response with no
`web_search` key anywhere and the model's own `reasoning_content` explicitly
reasoning about *not* having a search tool available ("I don't see a schema
… I must inform the user I cannot perform live web searches"), despite the
`tools` entry being present and the server returning `200` with no error.
This is not a request-shape defect: five separate attempts across several
prompts *did* trigger a real search, each returning a populated `web_search`
array with real, current results (confirmed against `docs.z.ai`'s own
`chat.completions` + `web_search`-tool example, whose non-streaming response
places `web_search` the same way). Whether the model actually calls the
server-side tool for a given turn is model discretion, the same as any other
tool — it is not deterministically forced by `enable: true`. This matches,
and further explains, the "worst case is cited-text-only" floor both 0075 and
0131 already accepted: the floor isn't only a parser miss, it's also just the
model choosing not to search.

**Second finding — the confirmed placement.** Across all five live
invocations that did fire (one non-streaming, four streaming), `web_search`
consistently landed as a **top-level sibling of `choices`**, delivered
**exactly once**, on the **same final chunk** that carries
`finish_reason: "stop"` and the `usage` object:

```json
{"id":"…","object":"chat.completion.chunk","choices":[{"index":0,
  "finish_reason":"stop","delta":{"role":"assistant","content":""}}],
  "usage":{"prompt_tokens":353,"completion_tokens":1389,…},
  "web_search":[{"refer":"ref_1","title":"…","link":"https://…",
    "content":"…","media":"","icon":"","publish_date":""}, …]}
```

It **never** appeared nested under `choices[0].delta` in any of the five
confirmed instances. The existing `data.get("web_search")` (top-level) scan
site in `openai::sse::handle_chunk` already covers this placement correctly
and needs no change; the `choices[0].delta.web_search` scan site, carried
defensively since the #305 MVP, has never matched a real payload and is
removed.

## Decision

- `openai::sse::handle_chunk` drops the `choices[0].delta.web_search` scan
  site, keeping only the confirmed top-level `data.get("web_search")` one.
- A regression test (`web_search_array_nested_under_delta_is_not_scanned`)
  locks in that a delta-nested `web_search` array is no longer treated as a
  search result, documenting why the site was removed rather than leaving it
  silently absent.
- `ContentPart::ProviderSearch`'s persisted `data` payload
  ([0131](0131-web-search-post-mvp-follow-ups.md) §1) is unaffected: the
  entries array itself is unchanged, only the scan site that finds it is
  narrowed. No new wire shape, no protocol change.
- [0131](0131-web-search-post-mvp-follow-ups.md) §3 and
  [0075](0075-provider-side-web-search-mvp.md)'s "Accepted MVP limitations"
  are annotated (not rewritten) to record this as closed, per each ADR's own
  standing convention of an inline amendment note rather than a body rewrite.
  [docs/deferred-work-ledger.md](../deferred-work-ledger.md) row 3 moves to
  Resolved.

## Consequences

- The web-search MVP's four originally-accepted limitations
  ([0075](0075-provider-side-web-search-mvp.md) "Accepted MVP limitations")
  are now all closed.
- `handle_chunk` is very slightly simpler (one scan site instead of two) with
  identical behavior on every payload actually observed from the live API.
- The "model may simply not call the tool" behavior is not new — it was
  already the accepted worst case — but is now an *observed*, not merely
  *theoretical*, failure mode, worth knowing if `web_search` results seem to
  go missing in practice: check whether the model's reasoning shows it
  deciding not to search before assuming a parser regression.

## Rejected alternatives

- **Keeping both scan sites "just in case."** Rejected: the whole point of
  item 3 was to replace a defensive guess with a confirmed fact once
  verification became possible: an unreachable branch that can never be
  proven unreachable again isn't defensive, it's dead code the file-cap and
  clippy gates would otherwise be right to flag if it grew any further.
- **A new top-level ADR rewriting 0075/0131's bodies.** Both ADRs already
  pre-authorized exactly this kind of closure note (0075's own "Accepted MVP
  limitations" section already carries a prior inline amendment blockquote
  from 0131; 0131 §3 explicitly invites "whoever closes it" to update its
  status line). A full rewrite would violate the numbered/immutable ADR
  convention for no benefit over a short annotation.
