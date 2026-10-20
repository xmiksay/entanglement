//! The tool-call family of [`SessionView::apply_event`]'s fold: streamed
//! args, the assembled call, approval/question queues, and output folding.

use entanglement_core::{ApprovalScope, Questions};

use super::*;

impl SessionView {
    /// `OutEvent::ToolCallDelta` (#194).
    pub(super) fn fold_tool_call_delta(
        &mut self,
        seq: u64,
        request_id: String,
        tool: String,
        delta: String,
    ) -> bool {
        if seq > self.last_seen_seq {
            self.last_seen_seq = seq;
            self.clear_pending_user();
            match self.streaming_tool_calls.get(&request_id).copied() {
                Some(idx) => {
                    if let Some(TranscriptEntry::ToolCall { input, .. }) =
                        self.transcript.get_mut(idx)
                    {
                        input.push_str(&delta);
                    }
                }
                None => {
                    let idx = self.transcript.len();
                    self.transcript.push(TranscriptEntry::ToolCall {
                        request_id: Some(request_id.clone()),
                        tool,
                        input: delta,
                        output: None,
                        is_error: false,
                        nested: Vec::new(),
                    });
                    self.streaming_tool_calls.insert(request_id, idx);
                }
            }
            true
        } else {
            false
        }
    }

    /// `OutEvent::ToolCall`.
    pub(super) fn fold_tool_call(
        &mut self,
        seq: u64,
        request_id: String,
        tool: String,
        input: String,
    ) -> bool {
        if seq > self.last_seen_seq {
            self.clear_pending_user();
            // If the call streamed its args, finalize the in-progress
            // entry with the authoritative input rather than duplicating
            // it (#194); otherwise push a fresh entry (non-streaming
            // providers land here directly).
            match self.streaming_tool_calls.remove(&request_id) {
                Some(idx) => {
                    if let Some(TranscriptEntry::ToolCall {
                        tool: t,
                        input: buf,
                        ..
                    }) = self.transcript.get_mut(idx)
                    {
                        *t = tool.clone();
                        *buf = input.clone();
                    }
                }
                None => {
                    self.transcript.push(TranscriptEntry::ToolCall {
                        request_id: Some(request_id.clone()),
                        tool: tool.clone(),
                        input: input.clone(),
                        output: None,
                        is_error: false,
                        nested: Vec::new(),
                    });
                }
            }
            self.last_seen_seq = seq;
            true
        } else {
            false
        }
    }

    /// `OutEvent::ToolRequest`.
    pub(super) fn fold_tool_request(&mut self, seq: u64, mut req: PendingRequest) -> bool {
        if seq > self.last_seen_seq {
            self.last_seen_seq = seq;
            // An older producer sent no scopes: offer only what every
            // approval honors (ADR-0230).
            if req.scopes.is_empty() {
                req.scopes.push(ApprovalScope::Once);
            }
            let request_id = req.request_id.clone();
            // One id is one answerable card: the runtime keeps a session's
            // live ids unique (ADR-0238), so a newer card with a queued id
            // replaces that entry in place and the prompt stays on it.
            if let Some(queued) = self
                .pending_tool_requests
                .iter_mut()
                .find(|r| r.request_id == request_id)
            {
                let changed = *queued != req;
                *queued = req;
                // A half-typed reject reason was for the old content.
                let front = self.pending_tool_requests.front();
                if changed && front.is_some_and(|r| r.request_id == request_id) {
                    self.approval_mode = ApprovalMode::WaitingForApproval { request_id };
                }
                return true;
            }
            // Core batch-emits a turn's tool calls (#270, ADR-0061), so
            // more requests can land while one is already prompted —
            // queue them; only the front drives `approval_mode` (#273).
            self.pending_tool_requests.push_back(req);
            if matches!(self.approval_mode, ApprovalMode::Normal) {
                self.approval_mode = ApprovalMode::WaitingForApproval { request_id };
            }
            true
        } else {
            false
        }
    }

    /// `OutEvent::ApprovalResolved` (ADR-0240): the card left the runtime —
    /// answered by any head, expired or unwound — so drop it here. Seq-gated
    /// like the card itself: a stale end must not drop a newer card that
    /// reuses the id. Seq 0 is an end for a session with no sequence
    /// (retired before #884, evicted under #891): dropped by id without
    /// moving the watermark, like a seq-0 `Error` — a seq-0 card never
    /// folds, so no newer card can carry the id.
    pub(super) fn fold_approval_resolved(&mut self, seq: u64, request_id: &str) -> bool {
        if seq != 0 && seq <= self.last_seen_seq {
            return false;
        }
        self.last_seen_seq = self.last_seen_seq.max(seq);
        self.drop_approval(request_id);
        true
    }

    /// `OutEvent::UserQuestion`.
    pub(super) fn fold_user_question(
        &mut self,
        seq: u64,
        request_id: String,
        questions: Questions,
    ) -> bool {
        if seq > self.last_seen_seq {
            self.last_seen_seq = seq;
            self.queue_question(PendingQuestion::new(request_id, questions.0));
            true
        } else {
            false
        }
    }

    /// `OutEvent::NestedCall`/`NestedCallSummary` (ADR-0233): live-only, no
    /// `seq` to dedupe on. Folded into its `rhai` op like `fold_tool_output`
    /// folds an output — by a reverse scan on `parent_request_id`, since two
    /// scripts can run at once and a background one reports after later
    /// turns — or kept standalone when that op is not in the transcript.
    pub(super) fn fold_nested_call(&mut self, ev: &OutEvent) -> bool {
        use crate::run::summary::{nested_call_line, nested_summary_line};
        let (parent, line, is_error, calls) = match ev {
            OutEvent::NestedCall {
                index,
                tool,
                args,
                duration_ms,
                is_error,
                parent_request_id,
                ..
            } => (
                parent_request_id,
                nested_call_line(*index, tool, args, *duration_ms),
                *is_error,
                1,
            ),
            OutEvent::NestedCallSummary {
                suppressed,
                errors,
                parent_request_id,
                ..
            } => (
                parent_request_id,
                nested_summary_line(*suppressed, *errors),
                *errors > 0,
                *suppressed,
            ),
            _ => return false,
        };
        let entry = NestedLine {
            line,
            is_error,
            calls,
        };
        let op = self.transcript.iter_mut().rev().find_map(|e| match e {
            TranscriptEntry::ToolCall {
                request_id: Some(id),
                nested,
                ..
            } if id == parent => Some(nested),
            _ => None,
        });
        match op {
            Some(nested) => nested.push(entry),
            None => self.transcript.push(TranscriptEntry::NestedCall(entry)),
        }
        true
    }

    /// `OutEvent::ToolOutput`.
    pub(super) fn fold_tool_output(
        &mut self,
        seq: u64,
        request_id: String,
        tool: String,
        output: String,
        is_error: bool,
    ) -> bool {
        if seq > self.last_seen_seq {
            self.last_seen_seq = seq;
            // An `ask_user` call's result ends its question however it was
            // resolved — answered by any head, retracted, or a mode's
            // `question_timeout` (#894). A question is turn-bound and never
            // moves, so the result lands on the view that shows it.
            if tool == entanglement_runtime::tool_names::ASK_USER_TOOL {
                self.drop_question(&request_id);
            }
            // Fold the output (and its is_error classification, #672,
            // ADR-0176) into its call so one op is one entry (#340).
            // Batch results resolve out of order (#270), so scan from the
            // back for the unfilled `ToolCall` with this `request_id`.
            let folded = self.transcript.iter_mut().rev().find_map(|e| match e {
                TranscriptEntry::ToolCall {
                    request_id: Some(id),
                    output: output_slot @ None,
                    is_error: error_slot,
                    ..
                } if *id == request_id => Some((output_slot, error_slot)),
                _ => None,
            });
            match folded {
                Some((slot, error_slot)) => {
                    *slot = Some(output);
                    *error_slot = is_error;
                }
                // No matching call (e.g. a stray/duplicate output): keep
                // the standalone notice rather than dropping it.
                None => self.transcript.push(TranscriptEntry::ToolOutput {
                    tool: Some(tool.clone()),
                    output,
                }),
            }
            true
        } else {
            false
        }
    }
}
