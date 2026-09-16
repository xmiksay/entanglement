//! Pairs a root log's raw records into the `(Option<InMsg>, OutEvent)` stream
//! `Holly::resume` folds. Split out of `session_store.rs` (400-line cap).

use std::collections::VecDeque;

use entanglement_core::{InMsg, OutEvent};

use super::{LogPayload, LogRecord};

/// Pairs each `Out` record with an earlier `In` record, in order.
///
/// The tap drains inbound first, so an `In` lands ahead of the events the
/// engine emits after seeing it — but several can land ahead of one event: a
/// `Prompt` sent while a tool batch is parked, then the executor's
/// `ToolResult`. The messages replay folds (`Prompt`, `Stop`) therefore queue
/// first in, first out, one per following `Out`; any other `In` carries no
/// state to restore, and letting it overwrite a queued prompt used to drop that
/// prompt from the resumed history. `In` records with no following `Out` are
/// dropped.
pub fn pair_records(records: &[LogRecord]) -> Vec<(Option<InMsg>, OutEvent)> {
    let mut queued: VecDeque<InMsg> = VecDeque::new();
    let mut paired = Vec::new();
    for record in records {
        match &record.payload {
            LogPayload::In(msg @ (InMsg::Prompt { .. } | InMsg::Stop { .. })) => {
                queued.push_back(msg.clone());
            }
            LogPayload::In(_) => {}
            LogPayload::Out(event) => paired.push((queued.pop_front(), event.clone())),
            // A gap tombstone carries no state to restore. Resume paths call
            // `integrity_gap` and refuse before reaching here; this arm only
            // keeps pairing total-ordered if a caller pairs a gapped log anyway.
            LogPayload::Gap { .. } => {}
        }
    }
    paired
}
