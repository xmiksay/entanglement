use super::*;

mod fork;
mod notices;
mod tools;

impl SessionView {
    /// The session's working directory (#764), `None` for the project root.
    pub fn workdir(&self) -> Option<&str> {
        self.workdir.as_deref()
    }

    /// Records the user's outgoing prompt into the transcript so it shows up
    /// in the chat scrollback. Unlike engine `OutEvent`s, user prompts carry
    /// no `seq` and bypass the dedupe guard — they originate here, not the
    /// engine broadcast.
    pub fn record_user_message(&mut self, text: String) {
        // First user message doubles as the session's sidebar description —
        // the same snippet shape the resume list derives from persisted logs.
        if self.first_prompt.is_none() {
            self.first_prompt = Some(crate::session_store::first_prompt_snippet(&text));
        }
        self.transcript.push(TranscriptEntry::User {
            text,
            pending: true,
        });
        self.user_pending = true;
    }

    /// Clears the `pending` (dimmed) flag on the most recent user prompt. Called
    /// on the first content event of a turn — the model often opens with a
    /// reasoning block or tool call rather than text, so keying this off text
    /// alone would leave the prompt greyed out for the whole turn (issue #103).
    fn clear_pending_user(&mut self) {
        if !std::mem::take(&mut self.user_pending) {
            return;
        }
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
            is_error: false,
            nested: Vec::new(),
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
            OutEvent::SessionStarted {
                parent,
                predecessor,
                ts,
                ..
            } => {
                self.parent = parent;
                self.predecessor = predecessor;
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
            // Supervisor-global query replies (ADR-0028 #160, #515, #607,
            // #726): `handle_out_event` filters them out before they reach a
            // view, so this arm only keeps the match exhaustive.
            OutEvent::SessionList { .. }
            | OutEvent::QuestionList { .. }
            | OutEvent::OperationList { .. }
            | OutEvent::PendingList { .. } => false,
            // MCP ops (#375) are engine-global — never routed to a per-session
            // view (`handle_out_event` filters them out, same as SessionList).
            OutEvent::McpList { .. } => false,
            OutEvent::McpChanged { .. } => false,
            // MCP OAuth progress (ADR-0153) is engine-global like `McpChanged`;
            // the TUI renders it as a toast/status line from the event loop, not
            // through this per-session fold.
            OutEvent::McpAuthChanged { .. } => false,
            // LLM endpoint throttle transitions (#517) are engine-global too —
            // the TUI already renders them directly via `HttpClient::throttle_status()`
            // polling (`input_panel.rs`), never through this per-session fold.
            OutEvent::Throttle { .. } => false,
            // Session-log outage transitions (#767) are engine-global: the
            // App folds them into the status-line badge, not a session view.
            OutEvent::PersistenceDegraded { .. } | OutEvent::PersistenceRecovered { .. } => false,
            OutEvent::History { .. } => false,
            OutEvent::Status { state, .. } => {
                // Known cosmetic flap (#273): with two parked Asks, resolving
                // the first flips Status WaitingApproval→Thinking while the
                // second still waits. Deliberately not special-cased — only
                // terminal states drop the queues (a background script's
                // cards excepted, #872).
                self.state = state;
                if state == AgentState::Idle
                    || state == AgentState::Done
                    || state == AgentState::Error
                {
                    self.drop_turn_approvals();
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
            // Permission mode (ADR-0207): folded the same way `AgentChanged`
            // folds `agent` — every session gets a `ModeChanged` right after
            // its `AgentChanged` unconditionally at start (`session.rs`), so
            // `mode()` is never stale for long. `/allow` (#634) is the first
            // reader: a `SessionDir` grant is mode-scoped, so recording one
            // needs to know the session's current mode. Full `/mode`
            // display/switching is a later change.
            OutEvent::ModeChanged { mode, .. } => {
                self.mode = mode;
                true
            }
            // The session's workdir (#764): the status line names it.
            OutEvent::WorkdirChanged { dir, .. } => {
                self.workdir = Some(dir);
                true
            }
            // Re-nested under the compaction successor of its old parent
            // (ADR-0234): the sidebar tree reads `parent()`.
            OutEvent::ParentChanged { parent, .. } => {
                self.parent = Some(parent);
                true
            }
            // The model switch (#218) shows in the app-global context bar, not the
            // per-session transcript; the ledger only notes which model the
            // next `Usage` rounds bill to (#560 `/cost` by-model split).
            OutEvent::ModelChanged {
                provider, model, ..
            } => {
                self.cost.set_model(&provider, &model);
                false
            }
            // A cache-keyed knob change warns once and excuses the next full
            // miss (`generation.rs`); `App` owns the `/set`/`/show` line (#376).
            OutEvent::GenerationChanged { generation, .. } => self.fold_generation(generation),
            // The live tool overlay (#539): the event loop renders the
            // confirmation status line and tracks the per-session list on the
            // `App` (for `/enable`/`/disable` to compute replacements) — no
            // per-session view state to fold here.
            OutEvent::ToolOverlayChanged { .. } => false,
            // Display metadata: the ack carries full merged state — overwrite
            // both fields (the resume path replays these records through this
            // same fold, so a restored view keeps its name/action for free).
            OutEvent::SessionMetaChanged { name, action, .. } => {
                self.name = name;
                self.action = action;
                true
            }
            OutEvent::Plan {
                seq, content, path, ..
            } => {
                if seq > self.last_seen_seq {
                    self.plan = Some(content);
                    self.plan_path = Some(path);
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
            } => self.fold_tool_call_delta(seq, request_id, tool, delta),
            OutEvent::ToolCall {
                seq,
                request_id,
                tool,
                input,
                ..
            } => self.fold_tool_call(seq, request_id, tool, input),
            OutEvent::ToolRequest {
                seq,
                request_id,
                tool,
                input,
                scopes,
                detached,
                ..
            } => self.fold_tool_request(
                seq,
                PendingRequest {
                    request_id,
                    tool,
                    input,
                    scopes,
                    detached,
                },
            ),
            OutEvent::ApprovalResolved {
                seq,
                request_id,
                outcome,
                ..
            } => self.fold_approval_resolved(seq, &request_id, outcome),
            OutEvent::UserQuestion {
                seq,
                request_id,
                questions,
                ..
            } => self.fold_user_question(seq, request_id, questions),
            // Runtime plumbing (#58): the tool executor answers this, not the
            // UI. The call is already shown via `ToolCall`.
            OutEvent::ToolExec { .. } => false,
            OutEvent::ToolOutput {
                seq,
                request_id,
                tool,
                output,
                is_error,
                ..
            } => self.fold_tool_output(seq, request_id, tool, output, is_error),
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
            ev @ OutEvent::Usage { .. } => {
                self.cost.fold_event(&ev);
                true
            }
            OutEvent::Error { seq, message, .. } => {
                // A supervisor lifecycle error for an id with no sequence (one
                // that never started, or whose retired entry was evicted, #891;
                // ADR-0239) carries seq `0` — a value core never mints — so it can't satisfy
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
            // Every compaction forks a successor and retires this session
            // (ADR-0205); `auto`/`mode` only say which path got here. The view
            // switch is `App`'s job (it needs the successor's id, which
            // arrives on a separate event) — this is just the notice left
            // behind on the session that compacted.
            OutEvent::Compacted {
                seq,
                summary,
                auto,
                mode,
                ..
            } => self.fold_compacted(seq, summary, auto, mode),
            // The session's active-skill posture changed (#400, ADR-0106;
            // posture-only since ADR-0194 — skills are additive-only, never a
            // tool-set restriction): reuses the tool-output entry, like
            // `record_status`'s and `Compacted`'s out-of-band notices, so
            // the loaded skill is visible in the transcript. `allowed_tools`
            // is vestigial (wire/log-replay compat only) and no longer
            // implies any restriction — not rendered as one.
            OutEvent::SkillActive { seq, skill_id, .. } => self.fold_skill_active(seq, skill_id),
            // A bound plan file changed out of band (#627, ADR-0145
            // "Consequences"): the watcher-driven live counterpart to
            // `propose_plan`'s staleness guard, which otherwise only surfaces
            // as a refusal at the *next* `propose_plan(path=...)` call.
            // Reuses the tool-output entry, like `SkillActive`'s notice.
            OutEvent::PlanChanged { seq, path, .. } => self.fold_plan_changed(seq, path),
            // `/undo`/`/rewind` outcome (ADR-0212): a notice like `PlanChanged`'s.
            OutEvent::FilesReverted {
                seq,
                to_turn,
                restored,
                skipped,
                ..
            } => self.fold_notice(
                seq,
                "undo",
                crate::file_backup::summary(to_turn, &restored, &skipped),
            ),
            // `/fork` (#786): render the inherited history under a lineage notice.
            OutEvent::SessionForked {
                seq,
                forked_from,
                at_turn,
                context,
                ..
            } => self.fold_forked(seq, forked_from, at_turn, &context),
            // Ambiguous-stop bounded retry (#ADR-0118): a distinct out-of-band
            // notice (like `Compacted`/`SkillActive`) reporting the in-place
            // retry. Advancing `last_seen_seq` past this event also closes the
            // preceding partial `TextDelta` segment, so the retry's re-streamed
            // text renders as its own bubble instead of concatenating onto the
            // truncated one.
            OutEvent::AmbiguousRetry { seq, .. } => self.fold_ambiguous_retry(seq),
            // Persisted provider-side web-search block (#481): already rendered
            // live via `ReasoningDelta`'s query/source lines — this is the
            // persisted-to-history echo, so advance the seq dedupe guard but
            // don't push a second, duplicate transcript entry.
            // Same shape for the captured extended-thinking block: the live
            // `ReasoningDelta` lines already rendered this reasoning, so advance
            // the dedupe guard without adding a duplicate transcript entry.
            ev @ (OutEvent::NestedCall { .. } | OutEvent::NestedCallSummary { .. }) => {
                self.fold_nested_call(&ev)
            }
            OutEvent::SearchResult { seq, .. } | OutEvent::ReasoningBlock { seq, .. } => {
                if seq > self.last_seen_seq {
                    self.last_seen_seq = seq;
                    true
                } else {
                    false
                }
            }
        }
    }
}
