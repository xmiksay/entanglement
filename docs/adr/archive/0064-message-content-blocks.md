# 0064. `Message`/`Prompt` carry multimodal content blocks

- Status: Accepted
- Date: 2026-07-14
- Builds on the seam inversion of [0053](0053-invert-core-provider-seam.md) (which
  moved `Message`/`MessageRole` into `entanglement-provider`). Issue #197, epic
  #196. Unblocks #221 (`read` emits image blocks).

## Context

`Message` was `role + text: String`, and `InMsg::Prompt` was `session + text:
String`. Both OpenAI and Anthropic converters built text-only request bodies.
There was no way to carry an image — no screenshot prompts, no image tool
results, no replayed thinking blocks. Images are confirmed on the roadmap
(decision 2026-07-12), and the concrete first consumer is #221 (`read` on an
image emits an image block as its tool result).

Migrating `text: String` → `content: Vec<ContentPart>` is a breaking change
across the wire, the replay log, and both converters. The persisted event log
stores `InMsg::Prompt` records verbatim, so once real logs accumulate the change
gets a back-compat tax forever. Doing it **before** logs pile up is cheapest —
hence landing the type migration now, ahead of wiring image capture.

## Decision

Introduce a content-block enum in **`entanglement-provider`** and thread it
through `Message` and `InMsg::Prompt`, with a serde back-compat shim on both.

- **`ContentPart`** — `#[serde(tag = "type")]`, `Text { text }` | `Image { source
  }`. **`ImageSource`** — `#[serde(tag = "type")]`, `Base64 { media_type, data }`
  today (a `Url` variant can be added later without breaking the wire). Tagging by
  `type` lets the enum grow (audio, documents) without breaking readers of the
  existing variants.
- **`Message.text: String` → `Message.content: Vec<ContentPart>`.** The
  constructors (`user`/`assistant`/`tool`) still take text and normalize an empty
  string to *no* parts (so a tool-calls-only assistant turn carries no stray empty
  text block); `user_content`/`tool_content` take explicit parts. A `Message::text()`
  helper (and the free `content_text`) concatenate the `Text` parts — the token
  estimator, compaction, and text-only converter paths read that.
- **`InMsg::Prompt { text }` → `InMsg::Prompt { content: Vec<ContentPart> }`**, with
  `InMsg::prompt(session, text)` for the common text case. `SessionCmd::Prompt`
  and the mid-turn steering stash carry `Vec<ContentPart>`; `Context` grows
  `push_user_content`.
- **Serde back-compat shim** (read-only, forward migration):
  - `Message` deserializes via a `MessageRepr` intermediary (`#[serde(from = …)]`)
    that accepts either `content: [ContentPart]` or the legacy `text: "…"`.
  - `InMsg::Prompt::content` uses `#[serde(alias = "text", deserialize_with = …)]`
    where the deserializer accepts either a `[ContentPart]` array or a bare
    `String` (an empty legacy string → no parts).

  New writes always emit `content`; old persisted logs still replay.
- **Converters render images natively.** OpenAI: all-text user content collapses
  to a plain string (unchanged wire), any image switches to the `text` /
  `image_url` (`data:` URL) block array. Anthropic: user/assistant content maps to
  `text` / `image` (base64 source) blocks; a `tool_result` stays a plain string
  when text-only, becoming a block array when it carries an image (the #221 path).

Spawn stays text-only: `InMsg::Spawn.prompt` is still `String` — sub-agents are
launched with text, not screenshots — and holly wraps it into a single text part
when queuing the child's first turn.

## Consequences

- The wire, replay, and both converters now speak content blocks. The migration is
  complete even though image *capture* isn't wired yet — #221 (and any screenshot
  head) extends the existing rendering rather than re-touching the type.
- Old logs deserialize through the shim; the shim is the only place the legacy
  `text` shape survives. If the shim is ever dropped, pre-#197 logs stop replaying.
- `Message::text()` allocates (concatenates parts); it is called on
  estimate/compaction/echo paths, none hot enough to matter. Borrow-sensitive
  callers read `content.iter().find_map(ContentPart::as_text)` for a `&str`.

## Alternatives rejected

- **Keep `text: String`, add a parallel `images` field.** Splits one logical body
  across two fields, and every converter still special-cases ordering; a single
  ordered `Vec<ContentPart>` is the shape both provider wires already use.
- **Defer until image capture lands.** Pays the persisted-log back-compat tax
  forever; the whole point is to migrate the type before logs accumulate.
- **A new `LlmSession`/message newtype.** Unneeded — the content vec lives on the
  existing struct, mirroring [0062](0062-collapse-llmsession-placeholder-newtype.md)'s
  "fields, not a newtype" call.
