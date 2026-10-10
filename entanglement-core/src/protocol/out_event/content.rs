//! Seq-bearing content [`OutEvent`] variants; this file is where the full
//! enum is expanded (see `out_event.rs`).

use entanglement_provider::{ContentPart, GenerationParams, UserId};
use serde::{Deserialize, Serialize};

use super::{CompactionMode, ToolEnvelope, UsagePurpose};
use crate::protocol::*;

out_event_enum! {
    /// The agent's strategy plan (markdown prose), full snapshot on every change.
    /// Emitted by the runtime when it resolves a `propose_plan` call (#513,
    /// ADR-0145, superseding ADR-0049's `update_plan` half); the engine never
    /// stores or consumes the plan. `path` is the plan file's location
    /// root-relative to the project (e.g. `.entanglement/plans/<id>.md`) — the
    /// runtime always resolves one, since #513 makes a plan a file. `#[serde(default)]`
    /// so a pre-#513 persisted log (no `path` field) still deserializes, with an
    /// empty string standing in for "unknown".
    Plan {
        session: SessionId,
        seq: u64,
        content: String,
        #[serde(default)]
        path: String,
    },
    /// Incremental assistant text.
    TextDelta {
        session: SessionId,
        seq: u64,
        text: String,
    },
    /// Incremental reasoning/thinking text.
    ReasoningDelta {
        session: SessionId,
        seq: u64,
        text: String,
    },
    /// Incremental tool-call argument fragment (#194), correlated to the
    /// eventual [`ToolCall`][OutEvent::ToolCall]/[`ToolExec`][OutEvent::ToolExec]
    /// by `request_id`. Display-only: lets a head render file-sized `edit`/
    /// `write` arguments as they stream, before the assembled `ToolCall` (which
    /// core emits once at round end, ADR-0061) arrives. `tool` labels the stream;
    /// `delta` is a raw JSON-argument substring — fragments concatenated in
    /// arrival order rebuild the call `input`. Additive: a head that ignores it
    /// still gets the full `ToolCall`, so replay reconstructs state from that
    /// (this variant is skipped on the replay fold).
    ToolCallDelta {
        session: SessionId,
        seq: u64,
        request_id: String,
        tool: String,
        delta: String,
    },
    /// A tool is being called or about to be approved (display-only). Emitted
    /// for every tool call, before execution, so heads can show what's being called
    /// (tool name + input arguments). Separate from `ToolRequest` which handles
    /// approval mode.
    ToolCall {
        session: SessionId,
        seq: u64,
        request_id: String,
        tool: String,
        input: String,
        /// The call's opaque provider metadata (e.g. a Gemini thought
        /// signature), so replay rebuilds the call the provider signed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_meta: Option<serde_json::Value>,
        /// The call exactly as the model emitted it, when core unwrapped an
        /// `invoke` envelope (ADR-0204): `tool`/`input` then name the inner
        /// call. Absent for every ordinary call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        envelope: Option<ToolEnvelope>,
    },
    /// Engine wants to run a host tool (permission `Ask`) and is pausing for approval.
    ToolRequest {
        session: SessionId,
        seq: u64,
        request_id: String,
        tool: String,
        input: String,
    },
    /// Core asks the runtime to execute a host tool that is cleared to run
    /// (permission `Allow`, or `Ask` after approval). The runtime executes it
    /// and replies with [`InMsg::ToolResult`]. Distinct from
    /// [`ToolRequest`][OutEvent::ToolRequest] (human approval) and
    /// [`ToolCall`][OutEvent::ToolCall] (display-only): only `ToolExec` drives
    /// execution, so a denied tool never runs (ADR-0006/0010).
    ///
    /// `agent` carries the emitting session's active profile name (#156): the
    /// runtime executor resolves permission/mask against it *authoritatively*,
    /// self-healing its per-session profile map from this field instead of the
    /// lossy `SessionStarted`/`AgentChanged` broadcast fold — so a dropped
    /// lifecycle event under burst can no longer silently downgrade a restricted
    /// session to allow-all/unmasked. `#[serde(default)]` keeps pre-#156 logs
    /// deserializable (empty ⇒ the executor falls back to its folded state).
    ToolExec {
        session: SessionId,
        seq: u64,
        request_id: String,
        tool: String,
        input: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        agent: String,
        /// The call exactly as the model emitted it, when core unwrapped an
        /// `invoke` envelope (ADR-0204): `tool`/`input` then name the inner
        /// call. Absent for every ordinary call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        envelope: Option<ToolEnvelope>,
    },
    /// The model asked the user one or more decision questions in a single
    /// `ask_user` call (#488, supersedes parts of ADR-0027: one event now
    /// carries a `questions` array, and a free-text answer is always
    /// available rather than gated by a per-call `allow_free_form`). A head
    /// walks `questions` in order and replies once with
    /// [`InMsg::AnswerQuestion`]; the runtime folds every answer back as the
    /// one tool call's output. Dedicated (not
    /// [`ToolRequest`][OutEvent::ToolRequest]) so choices render cleanly.
    /// `questions` flattens onto the wire via [`Questions`] so a legacy
    /// single-question log still deserializes.
    UserQuestion {
        session: SessionId,
        seq: u64,
        request_id: String,
        #[serde(flatten)]
        questions: Questions,
    },
    /// Result of an executed tool, a denied tool, or a built-in tool. `output` is
    /// the text rendering heads display (for an image result, a short
    /// placeholder). `content` carries the full multimodal result (#221) — empty
    /// for the common text-only case (heads read `output`), populated with the
    /// image block(s) when `read` opens an image so **replay** reconstructs the
    /// model's view faithfully instead of degrading it to the placeholder.
    ///
    /// `is_error`/`duration_ms` (#636, ADR-0176) mirror the same fields on
    /// [`InMsg::ToolResult`] — the display-side half of the structured side
    /// channel, so a head can style a failed call (denied/masked/refused, or the
    /// tool itself erroring) without pattern-matching `output`'s text. Both
    /// default off so a pre-#636 log replays unchanged.
    ///
    /// `exit_code` (#681, ADR-0186) mirrors [`InMsg::ToolResult`]'s field: the
    /// numeric exit status when the call ran a process (`bash`/`call`, or a
    /// `poll` observing a job exit), `None` otherwise — so a wire consumer can
    /// branch on the code without parsing the `[exit N]` text.
    ToolOutput {
        session: SessionId,
        seq: u64,
        request_id: String,
        tool: String,
        output: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content: Vec<ContentPart>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        /// The call exactly as the model emitted it, when core unwrapped an
        /// `invoke` envelope (ADR-0204): `tool`/`input` then name the inner
        /// call. Absent for every ordinary call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        envelope: Option<ToolEnvelope>,
    },
    /// Full snapshot of the session's task outline (sent on every change).
    /// Markdown, typically a `- [ ]`/`- [x]` checklist — displayed to the user
    /// as progress info, never parsed by the engine (mirrors
    /// [`Plan`][OutEvent::Plan]). Emitted by the runtime on an `update_tasks`
    /// state tool call (#231, ADR-0049).
    TaskList {
        session: SessionId,
        seq: u64,
        content: String,
    },
    /// Token usage + cost for one model round-trip, folded from the provider's
    /// `LlmEvent::Finish` (#192, ADR-0054). Counts are the normalized per-round-trip
    /// deltas (not cumulative); a head accumulates them for a session total.
    /// `input_tokens` is the *uncached* prompt portion — the whole prompt as
    /// billed is `input_tokens + cached_input_tokens + cache_write_tokens`.
    /// `cost_usd` is `None` when no catalog pricing covers the active model.
    /// `purpose` says which request the round was (a turn round, or a
    /// compaction summary) so a head can total spend while reading context
    /// size only off turn rounds; absent on the wire ⇒ `turn`.
    Usage {
        session: SessionId,
        seq: u64,
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
        cache_write_tokens: u64,
        cost_usd: Option<f64>,
        #[serde(default)]
        purpose: UsagePurpose,
    },
    /// Recoverable error surfaced to the UI; the engine stays alive.
    Error {
        session: SessionId,
        seq: u64,
        message: String,
    },
    /// Turn finished cleanly. Heads waiting on a one-shot turn exit on this.
    Done { session: SessionId, seq: u64 },
    /// Session compaction ran (#324, ADR-0082 → ADR-0101/0103/0205): this
    /// session was compacted into a **successor session**, and this event is
    /// the announcement. Since [ADR-0205](../../docs/adr/0205-every-compaction-forks-a-successor-session.md)
    /// **every** compaction forks — the source session's `Context` is never
    /// mutated, on any path — so this is always a *report*, never a
    /// confirmation of mutation, and `Session::replay`'s fold is always a
    /// **no-op**: there is nothing to reconstruct, because nothing was
    /// rewritten behind the log.
    ///
    /// The successor itself is announced separately by its own
    /// [`SessionStarted`][OutEvent::SessionStarted] carrying
    /// `predecessor: Some(this session)` — that pre-existing lineage field,
    /// not a field here, is how a head learns the successor's id and follows
    /// it. The source is retired (`CloseSession`) right after, so its log
    /// ends exactly at this event.
    ///
    /// `auto` and `mode` together name which of the three compaction paths ran:
    ///
    /// - `auto: false`, `mode: Summary` (both defaults) — **manual
    ///   `/compact`** (`InMsg::Oneshot`, ADR-0101/0110). Every pre-#398 record
    ///   on the wire is this case.
    /// - `auto: true`, `mode: Summary` — **auto-summarize on context
    ///   overflow** (#398, ADR-0103, amended by ADR-0205: it forks now rather
    ///   than mutating the live context).
    /// - `auto: true`, `mode: Prune` — the **prune-only fallback**, which
    ///   placeholder-prunes the oldest tool outputs when no LLM summary is
    ///   available. `summary` is then the pruned transcript that seeds the
    ///   successor, not an LLM-authored summary. ADR-0121's silence for this
    ///   path is retired by ADR-0205: a head must follow the fork, so it has
    ///   to be told.
    ///
    /// A persisted, seq-bearing content event in every case (persistence is
    /// variant-agnostic; seq-bearing ⇒ folded into `ReplayFrom` history).
    /// `kept` is how many trailing messages — clamped to the nearest safe
    /// turn boundary (#397, ADR-0102) — ride verbatim inside `summary`,
    /// appended after the LLM-generated summary of everything before them;
    /// `0` (the default) means the whole history was summarized with no
    /// verbatim tail, matching every pre-#397 record.
    Compacted {
        session: SessionId,
        seq: u64,
        summary: String,
        #[serde(default)]
        kept: u64,
        #[serde(default)]
        auto: bool,
        #[serde(default)]
        mode: CompactionMode,
    },
    /// File change record (audit log entry). Emitted by the runtime's tool
    /// executor after each successful `edit`/`write`/`apply_patch` (#202, #455).
    /// The record carries
    /// the `path`, `change_kind`, and a content **hash** (lowercase hex SHA-256
    /// of the after-content) — not the whole-file bytes: the event fans out to
    /// every subscriber, so a large edit must not clone its contents per head.
    FileChange {
        session: SessionId,
        seq: u64,
        path: String,
        change_kind: FileChangeKind,
        hash: String,
    },
    /// A bound plan file changed on disk **out of band** — i.e. not through this
    /// session's own `edit`/`write`/`apply_patch` (#627, ADR-0145
    /// "Consequences"). `propose_plan`'s staleness guard already refuses a stale
    /// `path` resubmit at the *next* `propose_plan` call; this is the same
    /// detection surfaced live, off a dedicated plans-folder watch, so a head
    /// doesn't have to wait for the next tool call to learn the bound file moved
    /// under it. `path` is the plan file's root-relative location; `hash` is its
    /// new content hash — informational only, mirroring `FileChange`'s "hash not
    /// bytes" fan-out discipline. No core replay-fold semantics, like
    /// `SkillActive`: a head just renders a notice and moves on.
    PlanChanged {
        session: SessionId,
        seq: u64,
        path: String,
        hash: String,
    },
    /// The session's active-skill posture changed (#400, ADR-0106). Emitted by
    /// the runtime's tool executor: `Some(skill_id)` when a `load_skill` call
    /// activates a skill, `None` when the skill's scope ends — the current turn's
    /// `Done`, or the session ending. `allowed_tools` is **vestigial** since
    /// ADR-0194 (skills are additive-only, never a tool-set restriction): still
    /// populated from the skill's frontmatter and serialized when present, for
    /// wire/log-replay compatibility, but never interpreted as an enforced mask —
    /// by core (which never did, ADR-0037) or the runtime. Wire-facing posture
    /// only; a head renders it as a notice. Mirrors `FileChange`: a fresh
    /// per-session seq (#157), no core replay-fold semantics (a head just tracks
    /// the latest value).
    SkillActive {
        session: SessionId,
        seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skill_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        allowed_tools: Option<Vec<String>>,
    },
    /// An ambiguous LLM stop (ADR-0118) triggered a bounded in-place retry: the
    /// round ended with no tool calls and no confident finish signal, so core
    /// committed whatever partial text streamed, injected `nudge` as a synthetic
    /// user-role steering message, and re-queried the model within the same
    /// turn. Persisted + seq-bearing (like `Compacted`) so `Session::replay`
    /// reconstructs the exact round boundary — flushing the partial assistant
    /// message and pushing the nudge — instead of merging every retry round's
    /// `TextDelta`s into one assistant message and dropping the nudge, which
    /// would resume a session from a history the live model never saw. Its
    /// non-delta arrival also delimits the re-streamed partial text so a head
    /// (the TUI transcript, a sub-agent answer collector) starts a fresh
    /// segment rather than concatenating consecutive rounds' text. Heads render
    /// it as a one-line "retrying" notice.
    AmbiguousRetry {
        session: SessionId,
        seq: u64,
        nudge: String,
    },
    /// A provider-side web-search result block was minted this round (#481,
    /// follow-up to #305/ADR-0075's "not persisted" MVP limitation). Persisted
    /// and seq-bearing (like `AmbiguousRetry`) so `Session::replay` reconstructs
    /// the assistant message's content verbatim — including the provider-native
    /// `data` payload `part` carries, needed to replay the search back to the
    /// same provider on a later turn (mirrors `ToolCall.provider_meta`'s opaque
    /// round-trip contract). A different provider's converter (and any
    /// renderer) reads only `part`'s `summary`, never its opaque `data`. Heads
    /// render `summary` as a one-line notice, mirroring how `ReasoningDelta`
    /// already renders the live query/source lines for this same search.
    SearchResult {
        session: SessionId,
        seq: u64,
        part: ContentPart,
    },
    /// A model's extended-thinking block was captured this round. Persisted and
    /// seq-bearing for the same reason as `SearchResult`: `Session::replay` must
    /// reconstruct the assistant message's content verbatim, including the
    /// provider-native `data` payload, or a resumed parked turn cannot present
    /// the block the provider requires on the next request.
    ///
    /// Distinct from `ReasoningDelta`, which carries the same reasoning as live
    /// *display* text and is deliberately not folded into `Context`. This event
    /// is the persistence rail; the delta is the rendering one, and both are
    /// emitted for the same thinking.
    ///
    /// Capture — and therefore this event — is unconditional. Whether the block
    /// is actually sent back is a separate, catalog-level decision
    /// (`ModelEntry::replay_thinking`), so toggling replay never rewrites a
    /// session log or changes what a head renders.
    ReasoningBlock {
        session: SessionId,
        seq: u64,
        part: ContentPart,
    },
}
