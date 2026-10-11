# 0220. `web_fetch` is an Exec-graded, SSRF-guarded host tool; the TUI pastes clipboard images for vision models

- Status: Accepted
- Date: 2026-10-11
- Relates to: [ADR-0207](0207-permission-modes-replace-agent-borne-authority.md)
  (capability grading), [ADR-0196](0196-tool-search-and-lazy-discovery-replace-the-invoke-envelope.md)
  (discoverable, not kernel), [ADR-0053](0053-invert-core-provider-seam.md)
  (`reqwest` stays provider-owned), [ADR-0025](0025-runtime-cargo-feature-gates.md)
  (feature gates)

## Context

#736 asks for two things. First, a `web_fetch` tool so a model can read a
documentation page without a shell (`curl` through `bash` works but shows the
model raw HTML and has no address policy). Second, pasting a screenshot from
the clipboard into the TUI prompt.

A model-chosen URL is an SSRF vector: it can point at loopback services, the
LAN, or the cloud metadata endpoint (`169.254.169.254`). Checking the host
name is not enough. A name can resolve to a private address, the answer can
change between a check and the connect (DNS rebinding), and a redirect can
point anywhere. IP literals have many spellings (`2130706433`, `0x7f.1`,
`[::ffff:127.0.0.1]`).

## Decision

### 1. Grading: `Capability::Exec`, no new capability class

`web_fetch` declares `Exec`, not `Read`. In the ADR-0207 vocabulary, `Read`
means reading the host. `research`, `plan` and `auto` allow it without a
prompt. An outbound request to a model-chosen URL is an outward effect: a
query string can carry data out. Every other outbound-network tool already
declares `Exec` (`endpoint__*`, `call`). With `Exec`, the shipped modes grade
`web_fetch` exactly like `curl`, with no new rule entries: `build` allows it,
and `research`/`plan`/`auto` prompt (`auto`'s prompt is bounded by its
question timeout). `permission_arg` yields the URL, so a user can widen with
an argument-scoped rule such as `web_fetch(https://docs.rs/*)` under
`config.yml` `modes:`.

A separate `Network` class was rejected. It would need a new vocabulary
member, an entry in every mode table, and tuning-guard changes, all to
reproduce the outcome `Exec` already gives.

### 2. SSRF guard: the guard *is* the resolver

The mechanism lives in `entanglement-provider::web_fetch` (`reqwest` is
provider-owned) and is re-exported by core as `entanglement_core::web_fetch`.

- **Address policy** (`is_blocked`): everything except plainly public
  unicast is blocked. That covers unspecified, `0/8`, loopback, RFC 1918,
  CGNAT `100.64/10`, link-local `169.254/16` (metadata), `192.0.0/24`,
  documentation and benchmarking ranges, multicast, broadcast, `240/4`, and
  on IPv6 `::`, `::1`, ULA `fc00::/7`, link-local `fe80::/10`, site-local
  `fec0::/10`, multicast, `2001:db8::/32` and Teredo `2001::/32`. IPv6 forms
  that embed an IPv4 address (mapped `::ffff:0:0/96`, compatible `::/96`,
  NAT64 `64:ff9b::/96`, 6to4 `2002::/16`) are judged by the embedded
  address.
- **URL validation** runs on the first URL and on every redirect target. It
  allows only `http`/`https` and refuses userinfo credentials. An IP-literal
  host is checked directly, because reqwest never calls a resolver for one.
  The host string is checked after WHATWG parsing, so decimal, hex and octal
  IPv4 spellings are already canonical. `localhost`-style names are refused
  by name so the error is clear.
- **Pinned resolution**: the fetch client's DNS resolver resolves the name
  and refuses the whole answer if *any* address is blocked. The addresses it
  vets are the ones the connector dials, so there is no second lookup to
  rebind.
- **Redirects** are followed manually (`Policy::none()`), at most 5, and each
  hop is validated again.
- **No proxy, ever** (`no_proxy()`). A proxy resolves the name itself, which
  would bypass the resolver guard. Proxy environment variables are ignored on
  purpose.
- **Tests reach `127.0.0.1` fixtures** through a `#[cfg(test)]`-only guard
  field (and a test-only fake DNS table). No bypass exists in a non-test
  build.

### 3. Client, caps and output

The client starts from `client_builder()` (the shared ring + webpki TLS
config, #759). It is its own `reqwest::Client` rather than the shared
`HttpClient`, because the resolver is a client-wide setting. It also does not
go through the endpoint pool. That pool keys resilience state per base URL
and persists it across processes (ADR-0144), so every arbitrary site would
create its own entry and state file. A fetch is a single attempt, with a 10 s
connect timeout, 30 s per request and 60 s for the whole redirect chain.

The body is streamed up to `max_bytes` (default 2 MiB, clamped to 10 MiB).
Content is classified by `Content-Type`. When the header is missing, a NUL in
the first KiB marks the body as binary. A non-text body is never read and
never inlined. The result is `[binary omitted: mime, size, url]`, the same
rule #727 set for MCP results. Text is lossily decoded as UTF-8. HTML is
converted to Markdown with `htmd`, dropping script/style/head/svg. The
`text` format also strips link and image targets, and `raw` returns the
body as served. The result is paged at the shared 32 KiB host-tool output cap
with an `offset` pointer to the next page.

`htmd` pulls in `html5ever`, so the runtime gates the tool behind a new
default-on **`web-fetch`** feature, the same way `rhai` is gated. The fetch
mechanism is small and always compiled.

`web_fetch` is registered at startup and is not in the tool-search kernel.
Under `tool_search` it is reached through `explore`/`describe`. Registering
it never changes the advertised array mid-session.

### 4. Clipboard image paste (TUI)

- `ModelEntry` gains `supports_vision: bool` (default `false`). It is set on
  the embedded Claude, GPT-4o/4-turbo/5.4 and Gemini 2.5 entries. An unknown
  model fails closed.
- `Ctrl+V`, or an empty bracketed paste (what terminals send when the
  clipboard holds only an image), reads the clipboard image by shelling out
  to the platform tool: `wl-paste` on Wayland, `xclip` on X11, `osascript`
  on macOS. These run with a 3 s timeout and a 32 MiB read cap. This avoids
  linking `arboard`'s X11/Wayland client stack. A headless session or a
  missing tool produces a toast and never a panic.
- The bytes decide the format: PNG, JPEG, GIF or WebP magic. Anything else
  is refused. The encoded size is capped at 5 MiB (Anthropic's per-image
  limit, the strictest of the wires) and checked *before* encoding.
- A non-vision model refuses the paste with a toast. The check runs again at
  submit, because `/model` can switch models in between.
- A vetted image is parked on the input buffer and an `[Image #n]` chip is
  inserted at the cursor. On submit, each image whose chip is still in the
  text is sent as a `ContentPart::Image` after the text part. Deleting a chip
  drops its image. Replacing the buffer (history recall, `$EDITOR`) drops all
  parked images along with their chips.

## Consequences

- `research`/`plan` users are asked before every fetch until they add an
  argument-scoped allow. This is intended: the posture matches `curl`.
- `read`'s image path (#221) is still not gated on `supports_vision`. That is
  a separate, pre-existing behavior and out of scope here.
- The workspace gains `htmd` and its html5ever tree. It is only compiled with
  the `web-fetch` feature.
