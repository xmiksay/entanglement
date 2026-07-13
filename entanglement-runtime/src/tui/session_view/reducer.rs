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
        self.transcript.push(TranscriptEntry::ToolCall {
            tool: "!bash".to_string(),
            input: command,
        });
        self.transcript.push(TranscriptEntry::ToolOutput {
            tool: Some("!bash".to_string()),
            output,
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
            OutEvent::SessionEnded { ts, .. } => {
                self.ended_ms = Some(ts);
                true
            }
            // Supervisor-global enumeration reply (ADR-0028): not a per-session
            // view update — the app handles it, if at all.
            OutEvent::SessionList { .. } => false,
            OutEvent::Status { state, .. } => {
                self.state = state;
                if state == AgentState::Idle
                    || state == AgentState::Done
                    || state == AgentState::Error
                {
                    self.clear_approval();
                    self.clear_question();
                }
                true
            }
            OutEvent::AgentChanged { agent, .. } => {
                self.agent = agent;
                true
            }
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
            OutEvent::ToolCall {
                seq, tool, input, ..
            } => {
                if seq > self.last_seen_seq {
                    self.clear_pending_user();
                    self.transcript.push(TranscriptEntry::ToolCall {
                        tool: tool.clone(),
                        input: input.clone(),
                    });
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
                    self.pending_tool_request = Some((request_id.clone(), tool, input));
                    self.approval_mode = ApprovalMode::WaitingForApproval { request_id };
                    true
                } else {
                    false
                }
            }
            OutEvent::UserQuestion {
                seq,
                request_id,
                question,
                options,
                allow_free_form,
                ..
            } => {
                if seq > self.last_seen_seq {
                    self.last_seen_seq = seq;
                    self.pending_question = Some(PendingQuestion {
                        request_id,
                        question,
                        options,
                        allow_free_form,
                        selected: 0,
                        entering_free_form: false,
                    });
                    true
                } else {
                    false
                }
            }
            // Runtime plumbing (#58): the tool executor answers this, not the
            // UI. The call is already shown via `ToolCall`.
            OutEvent::ToolExec { .. } => false,
            OutEvent::ToolOutput {
                seq, tool, output, ..
            } => {
                if seq > self.last_seen_seq {
                    self.transcript.push(TranscriptEntry::ToolOutput {
                        tool: Some(tool.clone()),
                        output,
                    });
                    self.last_seen_seq = seq;
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
            OutEvent::Error { seq, message, .. } => {
                if seq > self.last_seen_seq {
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
        }
    }
}
