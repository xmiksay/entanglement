//! Out-of-band transcript notices folded by [`SessionView::apply_event`]:
//! compaction, skill posture, out-of-band plan edits, ambiguous-stop retries.
//! Each reuses the tool-output entry, like `record_status`'s notices.

use super::*;

impl SessionView {
    /// `OutEvent::Compacted`.
    pub(super) fn fold_compacted(
        &mut self,
        seq: u64,
        summary: String,
        auto: bool,
        mode: CompactionMode,
    ) -> bool {
        if seq > self.last_seen_seq {
            let how = match (auto, mode) {
                (false, _) => "Compacted".to_string(),
                (true, CompactionMode::Summary) => "Auto-compacted: the context \
                     overflowed the model's window, so it was summarized"
                    .to_string(),
                (true, CompactionMode::Prune) => "Auto-compacted: the context \
                     overflowed the model's window and no summary was available, \
                     so the oldest tool output was pruned"
                    .to_string(),
            };
            let output = format!(
                "{how} — continuing in a new session. This one is retired; \
                 its history is preserved.\n\n{summary}"
            );
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

    /// `OutEvent::SkillActive`.
    pub(super) fn fold_skill_active(&mut self, seq: u64, skill_id: Option<String>) -> bool {
        if seq > self.last_seen_seq {
            let output = match skill_id {
                Some(id) => format!("skill `{id}` active"),
                None => "skill cleared".to_string(),
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

    /// `OutEvent::PlanChanged`.
    pub(super) fn fold_plan_changed(&mut self, seq: u64, path: String) -> bool {
        if seq > self.last_seen_seq {
            self.transcript.push(TranscriptEntry::ToolOutput {
                tool: Some("plan".to_string()),
                output: format!(
                    "plan file `{path}` changed on disk (not through this \
                     session) — re-read it before submitting it again"
                ),
            });
            self.last_seen_seq = seq;
            true
        } else {
            false
        }
    }

    /// `OutEvent::AmbiguousRetry`.
    pub(super) fn fold_ambiguous_retry(&mut self, seq: u64) -> bool {
        self.fold_notice(
            seq,
            "retry",
            "model stop was ambiguous — retrying in place".to_string(),
        )
    }

    /// A seq-deduped one-line notice rendered as a `tool`-tagged output entry.
    pub(super) fn fold_notice(&mut self, seq: u64, tool: &str, output: String) -> bool {
        if seq > self.last_seen_seq {
            self.transcript.push(TranscriptEntry::ToolOutput {
                tool: Some(tool.to_string()),
                output,
            });
            self.last_seen_seq = seq;
            true
        } else {
            false
        }
    }
}
