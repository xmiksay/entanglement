//! The streamed-turn DTOs: tool calls/specs, the normalized stop reason and
//! usage, and the [`LlmEvent`] stream item.

/// A tool the model asked to run. `input` is the raw JSON argument string.
///
/// `provider_meta` is an opaque, provider-private slot (e.g. Gemini's
/// `thoughtSignature` on a thinking model's function call) that must round-trip
/// **verbatim** through history persistence + replay: the provider stashes it on
/// the way out and restores it when rebuilding the request from history, and core
/// never inspects it. It carries `serde_json::Value` (which is not `Eq`), so
/// `ToolCall` is `PartialEq` but not `Eq`. Persisted with the ADR-0064 back-compat
/// shim (`#[serde(default, skip_serializing_if = …)]`) so pre-#309 logs — which
/// have no `provider_meta` — still deserialize and replay unchanged.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_meta: Option<serde_json::Value>,
}

impl ToolCall {
    /// A tool call with no provider-private metadata — the common case for every
    /// wire that has nothing to round-trip (OpenAI-compat, most Anthropic turns).
    pub fn new(id: impl Into<String>, name: impl Into<String>, input: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            input: input.into(),
            provider_meta: None,
        }
    }
}

/// One tool the engine advertises to the model (name + short description so the
/// model knows when to call it).
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input object (surfaces as Anthropic's
    /// `input_schema`). Defaults to a permissive empty-object schema.
    pub schema: serde_json::Value,
}

impl ToolSpec {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            schema: serde_json::json!({ "type": "object", "properties": {} }),
        }
    }

    pub fn with_schema(
        name: impl Into<String>,
        description: impl Into<String>,
        schema: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            schema,
        }
    }
}

/// Why a model stopped generating, normalized across providers (#192). Both the
/// OpenAI-compat `finish_reason` and Anthropic's `stop_reason` map onto this so a
/// consumer never has to know a provider's wire vocabulary. [`MaxTokens`] is the
/// load-bearing case — a reply truncated by the output cap that would otherwise
/// commit silently as a clean turn.
///
/// [`MaxTokens`]: StopReason::MaxTokens
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Natural completion (OpenAI `stop`, Anthropic `end_turn`).
    EndTurn,
    /// The model wants to run tools (OpenAI `tool_calls`, Anthropic `tool_use`).
    ToolUse,
    /// Output was cut off at the max-token limit (OpenAI `length`, Anthropic
    /// `max_tokens`) — the reply is truncated.
    MaxTokens,
    /// A configured stop sequence matched (Anthropic `stop_sequence`).
    StopSequence,
    /// Anything else the provider reports (e.g. `content_filter`) or an
    /// unrecognized token.
    Other,
}

impl StopReason {
    /// Map an OpenAI-compat `finish_reason` string onto the normalized reason.
    pub fn from_openai(reason: &str) -> Self {
        match reason {
            "stop" => StopReason::EndTurn,
            "tool_calls" | "function_call" => StopReason::ToolUse,
            "length" => StopReason::MaxTokens,
            _ => StopReason::Other,
        }
    }

    /// Map an Anthropic `stop_reason` string onto the normalized reason.
    pub fn from_anthropic(reason: &str) -> Self {
        match reason {
            "end_turn" => StopReason::EndTurn,
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            _ => StopReason::Other,
        }
    }

    /// Map a Gemini `finishReason` string onto the normalized reason. Gemini has
    /// no distinct tool-use reason — it reports `STOP` even when the turn is
    /// function calls — so the caller upgrades `EndTurn` to [`ToolUse`] when it
    /// actually emitted a tool call this turn.
    ///
    /// [`ToolUse`]: StopReason::ToolUse
    pub fn from_gemini(reason: &str) -> Self {
        match reason {
            "STOP" => StopReason::EndTurn,
            "MAX_TOKENS" => StopReason::MaxTokens,
            _ => StopReason::Other,
        }
    }

    /// A confident, deliberate stop — the only kind that ends a turn with no
    /// tool calls (ADR-0118). `session::turn::run_round` treats every other
    /// reason (`ToolUse` with zero actual tool calls, or `Other`) as ambiguous
    /// and retries instead of ending the turn; a caller with no `StopReason` at
    /// all (a stream that closed without reporting one) treats that as
    /// ambiguous too. Written as an exhaustive `match` (no `_` wildcard) so
    /// adding a variant here is a compile error until it is explicitly
    /// classified, instead of silently defaulting to ambiguous.
    pub fn is_confident_stop(self) -> bool {
        match self {
            StopReason::EndTurn | StopReason::MaxTokens | StopReason::StopSequence => true,
            StopReason::ToolUse | StopReason::Other => false,
        }
    }
}

/// Normalized token usage for one model round-trip (#192). Every field is
/// optional — a provider may not report a given dimension. Counts are normalized
/// so each maps to exactly one catalog pricing dimension without double-counting:
/// `input_tokens` is the *uncached* input (OpenAI's `prompt_tokens` minus its
/// cached reads; Anthropic already reports these separately), `cached_input_tokens`
/// is the cache-read portion, and `cache_write_tokens` is the cache-creation
/// portion (Anthropic only; OpenAI does not bill cache writes).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}

/// One event in a streamed model response.
#[derive(Debug, Clone, PartialEq)]
pub enum LlmEvent {
    /// Incremental assistant text (a chunk, not the whole reply).
    Text(String),
    /// Incremental reasoning/thinking text (extended thinking from models).
    Reasoning(String),
    /// Incremental tool-call argument fragment (#194), streamed as the model
    /// emits a tool call's JSON input *before* the assembled [`ToolCall`].
    /// Correlated to that final call by `id`; `name` rides every fragment so a
    /// head can label the stream before the args finish. `delta` is a raw
    /// substring of the JSON argument text — the fragments concatenated in
    /// arrival order rebuild [`ToolCall::input`]. Additive: a consumer that only
    /// needs the assembled call can ignore it and still get the terminal
    /// [`ToolCall`].
    ToolCallDelta {
        id: String,
        name: String,
        delta: String,
    },
    /// A tool the model wants to run, fully assembled (id + name + JSON input).
    ToolCall(ToolCall),
    /// A persisted content block minted server-side this round — a provider-side
    /// web search's call/result (#481, follow-up to #305/ADR-0075's "not
    /// persisted" MVP limitation). Additive: a consumer that ignores it still
    /// gets the existing [`Reasoning`][LlmEvent::Reasoning] live-display lines
    /// for the same search. The turn loop appends these, in arrival order, after
    /// the round's text when it assembles the committed
    /// [`Message`][crate::Message].
    ContentBlock(crate::ContentPart),
    /// Stream ended cleanly. Carries the normalized [`StopReason`] and [`Usage`]
    /// when the provider reports them (#192).
    Finish {
        stop_reason: Option<StopReason>,
        usage: Usage,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_tool_call_without_provider_meta_deserializes() {
        // A tool call persisted before #309 has no `provider_meta` field; the
        // `#[serde(default)]` shim must still deserialize it (→ None) so old logs
        // replay unchanged.
        let legacy = r#"{"id":"c1","name":"read","input":"{}"}"#;
        let tc: ToolCall = serde_json::from_str(legacy).unwrap();
        assert_eq!(tc.id, "c1");
        assert_eq!(tc.provider_meta, None);
    }

    #[test]
    fn provider_meta_roundtrips_and_is_omitted_when_none() {
        // None → the field is skipped on the wire (byte-identical to a pre-#309 log).
        let plain = ToolCall::new("c1", "read", "{}");
        assert_eq!(
            serde_json::to_string(&plain).unwrap(),
            r#"{"id":"c1","name":"read","input":"{}"}"#
        );
        // Some(..) → round-trips verbatim.
        let with_meta = ToolCall {
            provider_meta: Some(serde_json::json!({ "sig": "abc" })),
            ..ToolCall::new("c2", "search", "{}")
        };
        let json = serde_json::to_string(&with_meta).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(back, with_meta);
    }

    #[test]
    fn is_confident_stop_matches_adr_0118_classification() {
        // EndTurn/MaxTokens/StopSequence are deliberate, confident stops.
        assert!(StopReason::EndTurn.is_confident_stop());
        assert!(StopReason::MaxTokens.is_confident_stop());
        assert!(StopReason::StopSequence.is_confident_stop());
        // ToolUse (a contradictory reason when no tool calls actually landed)
        // and Other are ambiguous.
        assert!(!StopReason::ToolUse.is_confident_stop());
        assert!(!StopReason::Other.is_confident_stop());
    }
}
