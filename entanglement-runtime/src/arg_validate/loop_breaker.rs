//! The loop-breaker guard (#560, ADR-0196 §6): two identical failing calls
//! in a row get an explicit note — the schema was never the problem.

use std::collections::HashMap;
use std::sync::Mutex;

use entanglement_core::SessionId;

/// The explicit note appended when [`LoopBreaker::note`] reports a repeat —
/// two identical failing calls in a row. Verbatim text (design decision 8).
pub const LOOP_BREAKER_NOTE: &str =
    "same call failed twice — the schema is not the problem; change the arguments or use a \
     different tool";

#[derive(Clone, PartialEq)]
struct LastCall {
    tool: String,
    input: String,
    is_error: bool,
}

/// Per-session "was the immediately preceding call this exact same
/// `(tool, input)`, and did it also fail?" tracker — the loop-breaker guard
/// (design decision 8): a model retrying the identical failing shape twice in
/// a row gets an explicit nudge to change the call instead of the schema
/// alone. Deliberately generic across every failure kind (schema violation,
/// MCP required-param, or a runtime tool error) — the note fires on any
/// `is_error` repeat, not only schema violations.
#[derive(Default)]
pub struct LoopBreaker {
    last: Mutex<HashMap<SessionId, LastCall>>,
}

impl LoopBreaker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record this call's outcome for `session`, returning whether it
    /// repeats the immediately preceding call's exact `(tool, input)` with
    /// both calls having failed. Always records — a success in between
    /// resets the streak, matching "two *consecutive* identical failures".
    pub fn note(&self, session: &SessionId, tool: &str, input: &str, is_error: bool) -> bool {
        let mut map = self.last.lock().expect("loop-breaker mutex poisoned");
        let repeat = is_error
            && map
                .get(session)
                .is_some_and(|prev| prev.is_error && prev.tool == tool && prev.input == input);
        map.insert(
            session.clone(),
            LastCall {
                tool: tool.to_string(),
                input: input.to_string(),
                is_error,
            },
        );
        repeat
    }

    /// Release an ended/hibernated session's entry — mirrors
    /// [`crate::tool_advertising::DeliveredSchemas::forget`].
    pub fn forget(&self, session: &SessionId) {
        self.last
            .lock()
            .expect("loop-breaker mutex poisoned")
            .remove(session);
    }
}
