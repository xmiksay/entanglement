use super::*;

impl SessionView {
    /// Records the user's outgoing prompt into the transcript so it shows up
    /// in the chat scrollback. Unlike engine `OutEvent`s, user prompts carry
    /// no `seq` and bypass the dedupe guard — they originate here, not the
    /// engine broadcast.
    pub fn record_user_message(&mut self, text: String) {
        self.transcript.push(TranscriptEntry::User {
            text,
            pending: true,
        });
    }

    /// Clears the `pending` (dimmed) flag on the most recent user prompt. Called
    /// on the first content event of a turn — the model often opens with a
    /// reasoning block or tool call rather than text, so keying this off text
    /// alone would leave the prompt greyed out for the whole turn (issue #103).
    fn clear_pending_user(&mut self) {
        for entry in self.transcript.iter_mut().rev() {
            if let TranscriptEntry::User { pending, .. } = entry {
                *pending = false;
                break;
            }
        }
    }

    /// Records a head-side `!bash` passthrough (ADR-0030): the command and its
    /// captured output, reusing the tool call/output entries so it renders like
    /// any other tool run. Local only — not sent to the engine or the model.
    pub fn record_bash_passthrough(&mut self, command: String, output: String) {
        // Head-side, no engine round-trip: `request_id` is `None` and the output
        // is folded in immediately, so it renders as one collapsible op (#340).
        self.transcript.push(TranscriptEntry::ToolCall {
            request_id: None,
            tool: "!bash".to_string(),
            input: command,
            output: Some(output),
        });
    }

    /// Records a head-side status line into the transcript — e.g. the `/key`
    /// dialog's save/failure notice (#304). Local only (never sent to the engine
    /// or the model); it reuses the tool-output entry so it renders like other
    /// out-of-band notices. The caller must never pass a secret here.
    pub fn record_status(&mut self, label: &str, message: String) {
        self.transcript.push(TranscriptEntry::ToolOutput {
            tool: Some(label.to_string()),
            output: message,
        });
    }

    /// Applies an `OutEvent` already routed to this session. Returns `true`
    /// if it changed anything the UI needs to redraw for.
    pub fn apply_event(&mut self, event: OutEvent) -> bool {
        match event {
            OutEvent::SessionStarted { parent, ts, .. } => {
                self.parent = parent;
                self.started_ms = Some(ts);
                true
            }
            // A hibernated session (#318) tore down like an ended one — render it
            // the same way (the span from start to teardown); the id stays
            // resumable, but the live view has no more state to fold.
            OutEvent::SessionEnded { ts, .. } | OutEvent::SessionHibernated { ts, .. } => {
                self.ended_ms = Some(ts);
                true
            }
            // Supervisor-global query replies (ADR-0028, #160): not per-session
            // view updates — `handle_out_event` filters them out before they
            // reach a view, so these arms only keep the match exhaustive.
            OutEvent::SessionList { .. } => false,
            // MCP ops (#375) are engine-global — never routed to a per-session
            // view (`handle_out_event` filters them out, same as SessionList).
            OutEvent::McpList { .. } => false,
            OutEvent::McpChanged { .. } => false,
            OutEvent::History { .. } => false,
            OutEvent::Status { state, .. } => {
                // Known cosmetic flap (#273): with two parked Asks, resolving
                // the first flips Status WaitingApproval→Thinking while the
                // second still waits. Deliberately not special-cased — only
                // terminal states drop the queues.
                self.state = state;
                if state == AgentState::Idle
                    || state == AgentState::Done
                    || state == AgentState::Error
                {
                    self.clear_approval();
                    self.clear_question();
                    // A finished/interrupted turn leaves no call still streaming;
                    // drop trackers for any that never got their assembled call.
                    self.streaming_tool_calls.clear();
                }
                true
            }
            OutEvent::AgentChanged { agent, .. } => {
                self.agent = agent;
                true
            }
            // The model switch (#218) shows in the app-global context bar, not the
            // per-session transcript — no view state to fold here.
            OutEvent::ModelChanged { .. } => false,
            // Generation-parameter changes (#374) have no dedicated TUI surface
            // yet (#376 owns the `/set`/`/show` display) — no view state to fold.
            OutEvent::GenerationChanged { .. } => false,
            OutEvent::Plan { seq, content, .. } => {
                if seq > self.last_seen_seq {
                    self.plan = Some(content);
                    self.last_seen_seq = seq;
                    true
                } else {
                    false
                }
            }
            OutEvent::TextDelta { seq, text, .. } => {
                if seq > self.last_seen_seq {
                    self.clear_pending_user();
                    self.transcript.push(TranscriptEntry::TextDelta { text });
                    self.last_seen_seq = seq;
                    true
                } else {
                    false
                }
            }
            OutEvent::ReasoningDelta { seq, text, .. } => {
                if seq > self.last_seen_seq {
                    self.clear_pending_user();
                    self.transcript
                        .push(TranscriptEntry::ReasoningDelta { text });
                    self.last_seen_seq = seq;
                    true
                } else {
                    false
                }
            }
            // Streamed tool-arg fragment (#194): open a `ToolCall` entry on the
            // first fragment and grow its `input` in place as more arrive, so a
            // file-sized `edit`/`write` argument renders live instead of popping
            // in whole at round end.
            OutEvent::ToolCallDelta {
                seq,
                request_id,
                tool,
                delta,
                ..
            } => {
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
                            });
                            self.streaming_tool_calls.insert(request_id, idx);
                        }
                    }
                    true
                } else {
                    false
                }
            }
            OutEvent::ToolCall {
                seq,
                request_id,
                tool,
                input,
                ..
            } => {
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
                            });
                        }
                    }
                    self.last_seen_seq = seq;
                    true
                } else {
                    false
                }
            }
            OutEvent::ToolRequest {
                seq,
                request_id,
                tool,
                input,
                ..
            } => {
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
            OutEvent::UserQuestion {
                seq,
                request_id,
                questions,
                ..
            } => {
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
            // Runtime plumbing (#58): the tool executor answers this, not the
            // UI. The call is already shown via `ToolCall`.
            OutEvent::ToolExec { .. } => false,
            OutEvent::ToolOutput {
                seq,
                request_id,
                tool,
                output,
                ..
            } => {
                if seq > self.last_seen_seq {
                    self.last_seen_seq = seq;
                    // Fold the output into its call so one op is one entry (#340).
                    // Batch results resolve out of order (#270), so scan from the
                    // back for the unfilled `ToolCall` with this `request_id`.
                    let folded = self.transcript.iter_mut().rev().find_map(|e| match e {
                        TranscriptEntry::ToolCall {
                            request_id: Some(id),
                            output: slot @ None,
                            ..
                        } if *id == request_id => Some(slot),
                        _ => None,
                    });
                    match folded {
                        Some(slot) => *slot = Some(output),
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
            OutEvent::TaskList { seq, content, .. } => {
                if seq > self.last_seen_seq {
                    self.task_list = Some(content);
                    self.last_seen_seq = seq;
                    true
                } else {
                    false
                }
            }
            // Token totals live per-view so a resumed session restores its
            // accumulated counts (#192): the resume path replays persisted
            // records through here.
            OutEvent::Usage {
                input_tokens,
                output_tokens,
                cost_usd,
                ..
            } => {
                self.input_tokens += input_tokens;
                self.output_tokens += output_tokens;
                if let Some(cost) = cost_usd {
                    self.cost_usd += cost;
                }
                true
            }
            OutEvent::Error { seq, message, .. } => {
                // A supervisor lifecycle error for an id with no live session
                // (refused resume/spawn of a closed/unknown id) carries seq `0` —
                // a value core never mints (#157) — so it can't satisfy
                // `seq > last_seen_seq` and would otherwise be dropped, leaving the
                // refusal structurally invisible (ex-#159). Render it
                // unconditionally; it doesn't advance the dedupe watermark.
                if seq == 0 {
                    self.transcript.push(TranscriptEntry::Error { message });
                    true
                } else if seq > self.last_seen_seq {
                    self.transcript.push(TranscriptEntry::Error { message });
                    self.last_seen_seq = seq;
                    true
                } else {
                    false
                }
            }
            OutEvent::Done { seq, .. } => {
                if seq > self.last_seen_seq {
                    self.transcript.push(TranscriptEntry::Done);
                    self.last_seen_seq = seq;
                    true
                } else {
                    false
                }
            }
            OutEvent::FileChange { .. } => true,
            // Session compaction (#324, ADR-0082 → ADR-0101/0103): two
            // notices share this arm, told apart by `auto`. `auto: false` —
            // copy-on-write — the source `Context` is untouched; the summary
            // was forked into a new session by `App::handle_compacted`, so the
            // notice here just points at that fork. `auto: true` (#398) — the
            // live engine already mutated this session's context in place
            // before continuing the turn; there is no fork, so the notice
            // reports the in-place summary instead. Reuses the tool-output
            // entry, like `record_status`'s out-of-band notices.
            OutEvent::Compacted {
                seq, summary, auto, ..
            } => {
                if seq > self.last_seen_seq {
                    let output = if auto {
                        format!(
                            "Auto-compacted: the context overflowed the model's \
                             window, so it was summarized in place to keep the \
                             turn going.\n\n{summary}"
                        )
                    } else {
                        format!(
                            "Compacted: forked the summary into a new session. \
                             The original is preserved.\n\n{summary}"
                        )
                    };
                    self.transcript.push(TranscriptEntry::ToolOutput {
                        tool: Some("compact".to_string()),
                        output,
                    });
                    self.last_seen_seq = seq;
                    true
                } else {
                    false
                }
            }
            // The session's active-skill tool mask changed (#400, ADR-0106):
            // reuses the tool-output entry, like `record_status`'s and
            // `Compacted`'s out-of-band notices, so the combined posture (this
            // mask layered on top of the profile's #116 agent mask) is visible
            // in the transcript rather than only inferable from a refused call.
            OutEvent::SkillActive {
                seq,
                skill_id,
                allowed_tools,
                ..
            } => {
                if seq > self.last_seen_seq {
                    let output = match (skill_id, allowed_tools) {
                        (Some(id), Some(tools)) => {
                            format!(
                                "skill `{id}` active — tools restricted to: {}",
                                tools.join(", ")
                            )
                        }
                        (Some(id), None) => {
                            format!("skill `{id}` active — no additional restriction")
                        }
                        (None, _) => "skill mask cleared".to_string(),
                    };
                    self.transcript.push(TranscriptEntry::ToolOutput {
                        tool: Some("skill".to_string()),
                        output,
                    });
                    self.last_seen_seq = seq;
                    true
                } else {
                    false
                }
            }
            // Ambiguous-stop bounded retry (#ADR-0118): a distinct out-of-band
            // notice (like `Compacted`/`SkillActive`) reporting the in-place
            // retry. Advancing `last_seen_seq` past this event also closes the
            // preceding partial `TextDelta` segment, so the retry's re-streamed
            // text renders as its own bubble instead of concatenating onto the
            // truncated one.
            OutEvent::AmbiguousRetry { seq, .. } => {
                if seq > self.last_seen_seq {
                    self.transcript.push(TranscriptEntry::ToolOutput {
                        tool: Some("retry".to_string()),
                        output: "model stop was ambiguous — retrying in place".to_string(),
                    });
                    self.last_seen_seq = seq;
                    true
                } else {
                    false
                }
            }
        }
    }
}
