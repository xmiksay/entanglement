# 0065. `read` emits image files as image content blocks

- Status: Accepted
- Date: 2026-07-14
- Builds on [0064](0064-message-content-blocks.md) (multimodal `Message` /
  `InMsg::Prompt`), which named this its first consumer. Issue #221, epic #196.

## Context

[0064](0064-message-content-blocks.md) migrated `Message` and `InMsg::Prompt` to
`Vec<ContentPart>` and taught both converters to render an image block —
including an Anthropic image `tool_result`. But the tool-result *path* was still
`String`-only end to end: `Tool::run -> String`, `InMsg::ToolResult { output:
String }`, `OutEvent::ToolOutput { output: String }`, `Context::push_tool(id,
String)`. So `read` on an image had nowhere to put the image; it decoded the
bytes as UTF-8 and errored.

`read` is the concrete first image producer: opening an image should hand the
model the picture, not a "not valid UTF-8" error or a base64 wall of text.

## Decision

Thread `Vec<ContentPart>` through the whole tool-result path, mirroring the
`InMsg::Prompt` migration, and detect images in `read`.

- **`read` detects images by extension** (`png`/`jpg`/`jpeg`/`gif`/`webp` — the
  formats both provider wires accept inline). A match reads the raw bytes,
  base64-encodes them, and returns `[ContentPart::Image { Base64 { media_type,
  data } }]`; `offset`/`limit` don't apply. Every other file keeps the text path.
- **`Tool` grows a `run_content -> Vec<ContentPart>` method**, defaulting to
  wrapping `run`'s text in a single text part (empty → no parts). Only `read`
  overrides it, so the other ten tools are untouched. `ToolRegistry::execute`
  returns `Vec<ContentPart>`.
- **`InMsg::ToolResult { output: String }` → `{ content: Vec<ContentPart> }`**,
  with the same serde back-compat shim [0064](0064-message-content-blocks.md)
  used (`alias = "output"` + a string-or-array `deserialize_with`). `SessionCmd`,
  routing, and `Context::push_tool_content` carry the vec; `InMsg::tool_result`
  keeps the text ergonomics.
- **`OutEvent::ToolOutput` grows a `content: Vec<ContentPart>` field** alongside
  `output: String`. `output` stays the head-facing display text (an image is a
  `[image: <media_type>]` placeholder — base64 is useless on a terminal);
  `content` rides only when the result carries an image and is `#[serde(default,
  skip_serializing_if)]` so the common text case adds nothing to the wire or log.
  **Replay** prefers `content` when present so a resumed session rebuilds the
  model's image view instead of degrading it to the placeholder.
- **OpenAI tool results.** OpenAI's `role: "tool"` message accepts only string
  content, so an image can't ride inside it. The converter keeps the text (or a
  short placeholder) on the tool message and appends a following `role: "user"`
  message carrying the `image_url` block. Anthropic's image `tool_result` block
  array (already built in [0064](0064-message-content-blocks.md)) is used as-is.
- **`rhai`** is a text context: a delegated tool's result collapses to its text
  parts (`content_text`), so an image `read` inside a script yields no base64.

## Consequences

- `read` on an image now feeds the model the picture on Anthropic and every
  OpenAI-compatible endpoint (z.ai/GLM primary, OpenAI, Ollama), through the same
  `ToolResult` round-trip as any other tool.
- The tool-result path is multimodal end to end; a future image producer (a
  screenshot tool, a `call` that returns an image) reuses it without new plumbing.
- Old logs deserialize through the shims (`output` on `ToolResult`, absent
  `content` on `ToolOutput`); resume of a pre-#221 image read has no image to
  recover (there was none), so it degrades to the logged text, which is correct.
- Text tool results are byte-identical to before: an empty result still folds to
  no content parts, and `ToolOutput.content` stays empty (only `output` is set).

## Alternatives rejected

- **Change `Tool::run` to return `Vec<ContentPart>` for every tool.** Ripples
  through all ten text tools for one image producer; a defaulted `run_content`
  keeps the common case a plain-text `run`.
- **Keep `ToolOutput` text-only; reconstruct images elsewhere on replay.** The
  event log is the only per-result record replay reads, so an image would be lost
  on resume — a silent fidelity regression. A skip-when-empty `content` field
  fixes replay without bloating the text case.
- **Encode the image as base64 text in the existing `String`.** Ships a token
  wall the model can't view as an image and defeats the whole feature; the point
  is a native image block.
- **Sniff magic bytes instead of the extension.** More code for negligible gain —
  the issue specifies MIME/extension detection, and a mislabeled image is a
  user-authored edge case, not the common path.
