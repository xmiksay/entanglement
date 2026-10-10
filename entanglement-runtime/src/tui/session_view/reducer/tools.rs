//! The tool-call family of [`SessionView::apply_event`]'s fold: streamed
//! args, the assembled call, approval/question queues, and output folding.

use entanglement_core::Questions;

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
    pub(super) fn fold_tool_request(
        &mut self,
        seq: u64,
        request_id: String,
        tool: String,
        input: String,
    ) -> bool {
        if seq > self.last_seen_seq {
            self.last_seen_seq = seq;
            // Core batch-emits a turn's tool calls (#270, ADR-0061), so
            // more requests can land while one is already prompted —
            // queue them; only the front drives `approval_mode` (#273).
            self.pending_tool_requests
                .push_back((request_id.clone(), tool, input));
            if matches!(self.approval_mode, ApprovalMode::Normal) {
                self.approval_mode = ApprovalMode::WaitingForApproval { request_id };
            }
            true
        } else {
            false
        }
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
            // Queued like approvals (#273): the front is the one
            // rendered; answering every question in it promotes the
            // next call (#488).
            self.pending_questions
                .push_back(PendingQuestion::new(request_id, questions.0));
            true
        } else {
            false
        }
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
