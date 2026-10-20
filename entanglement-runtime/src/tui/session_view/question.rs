//! `ask_user` question state (#488/#518): the queued [`PendingQuestion`]
//! calls and the [`SessionView`] methods that walk, draft and submit them.
//! Split out of `session_view.rs` (#745).

use std::collections::HashSet;

use entanglement_core::Question;

use super::SessionView;

/// A model-driven `ask_user` **call** awaiting the user's answers (#488,
/// supersedes parts of ADR-0027; draft-until-submit #518). One call can carry
/// several `questions`, walked in order: `current` is the one on screen,
/// `answers` holds a **draft** per question (parallel to `questions`, `None`
/// until answered) that stays revisable — the user can step back with
/// [`SessionView::question_retreat`] and recommit a different choice — and
/// `picked` holds the current question's checked options while it's a
/// multi-select. `current == questions.len()` is the terminal **review/submit**
/// step: every question has a draft, nothing has been sent yet, and only an
/// explicit Submit (`Enter` on that step) turns the drafts into the single
/// `AnswerQuestion` frame. Distinct from
/// [`ApprovalMode`](super::ApprovalMode): approval is binary, this carries
/// labelled choices plus an always-available free-text "Other" escape.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingQuestion {
    pub request_id: String,
    pub questions: Vec<Question>,
    /// Index into `questions` of the one currently on screen; `questions.len()`
    /// once every question has a draft, meaning the review/submit step.
    pub current: usize,
    /// Draft answer per question, in `questions` order — `None` until that
    /// question has been committed at least once. Revisable: stepping back and
    /// recommitting overwrites the entry in place rather than appending.
    pub answers: Vec<Option<Vec<String>>>,
    /// Options checked for the current question so far (multi-select only).
    pub picked: HashSet<usize>,
    /// Highlighted choice for the current question. Indices
    /// `0..options.len()` are the options; index `options.len()` is the
    /// always-available "Other" free-text entry.
    pub selected: usize,
    /// True while the "Other" free-text field is being typed into (answer flows
    /// through the shared input box, like a reject reason).
    pub entering_free_form: bool,
}

impl PendingQuestion {
    pub fn new(request_id: String, questions: Vec<Question>) -> Self {
        let answers = vec![None; questions.len()];
        Self {
            request_id,
            questions,
            current: 0,
            answers,
            picked: HashSet::new(),
            selected: 0,
            entering_free_form: false,
        }
    }

    /// The question currently on screen. Only valid while
    /// [`Self::is_reviewing`] is false.
    pub fn current_question(&self) -> &Question {
        &self.questions[self.current]
    }

    /// Whether every question has a draft and the call is parked on the final
    /// review/submit step rather than any individual question.
    pub fn is_reviewing(&self) -> bool {
        self.current >= self.questions.len()
    }

    /// Total selectable choices for the current question: its options plus
    /// the always-available "Other" free-text entry.
    pub fn choice_count(&self) -> usize {
        self.current_question().options.len() + 1
    }

    /// Whether the highlighted choice is the free-text "Other" entry.
    pub fn free_form_selected(&self) -> bool {
        self.selected == self.current_question().options.len()
    }

    /// Whether the current question accepts more than one checked option.
    pub fn is_multi_select(&self) -> bool {
        self.current_question().multi_select
    }

    /// 1-based `(position, total)` among the call's questions, for a
    /// "(2/3)" progress indicator. Only meaningful while `!is_reviewing()`.
    pub fn progress(&self) -> (usize, usize) {
        (self.current + 1, self.questions.len())
    }

    /// The final answers, ready to send, once every question has a draft.
    /// `None` while any question is still unanswered (i.e. before the review
    /// step is reachable).
    pub fn answers_for_submit(&self) -> Option<Vec<Vec<String>>> {
        self.answers.iter().cloned().collect()
    }

    /// Restores the on-screen selection state (and free-form input text, if
    /// any) from `current`'s existing draft, or resets to blank when it has
    /// none. Called after navigating to a different question so a previously
    /// answered one shows its answer instead of a blank slate. Returns the
    /// free-text draft to load into the shared input box, when the draft was a
    /// custom "Other" answer — the caller (which owns the input widget) must
    /// load it; otherwise the caller should clear the input box.
    fn sync_from_draft(&mut self) -> Option<String> {
        self.picked.clear();
        self.selected = 0;
        self.entering_free_form = false;
        if self.is_reviewing() {
            return None;
        }
        let draft = self.answers[self.current].clone()?;
        let options = &self.questions[self.current].options;
        if self.questions[self.current].multi_select {
            if draft.is_empty() {
                // An explicit "nothing checked" submission — `picked` is
                // already cleared above, not a free-form answer.
                return None;
            }
            let idxs: HashSet<usize> = draft
                .iter()
                .filter_map(|label| options.iter().position(|o| &o.label == label))
                .collect();
            if idxs.len() == draft.len() {
                self.picked = idxs;
                return None;
            }
        } else if let Some(first) = draft.first() {
            if let Some(idx) = options.iter().position(|o| &o.label == first) {
                self.selected = idx;
                return None;
            }
        }
        // A free-form ("Other") answer isn't among the options — highlight
        // "Other" and hand the text back so it reloads into the input box.
        self.selected = options.len();
        self.entering_free_form = true;
        draft.into_iter().next()
    }
}

impl SessionView {
    /// The front of the question queue — the question currently prompted.
    pub fn pending_question(&self) -> Option<&PendingQuestion> {
        self.pending_questions.front()
    }

    /// Whether an `ask_user` question is awaiting an answer.
    pub fn is_asking(&self) -> bool {
        !self.pending_questions.is_empty()
    }

    /// Move the highlighted choice by `delta`, wrapping around all choices
    /// (options plus the always-available "Other" entry).
    pub fn question_move(&mut self, delta: isize) {
        if let Some(q) = self.pending_questions.front_mut() {
            let count = q.choice_count() as isize;
            if count > 0 {
                q.selected = (q.selected as isize + delta).rem_euclid(count) as usize;
            }
        }
    }

    /// Move the highlighted choice by `delta`, **clamping** at the first/last
    /// choice rather than wrapping (unlike [`question_move`][Self::question_move]).
    /// Used by `PageUp`/`PageDown` so paging past the end stays put.
    pub fn question_page(&mut self, delta: isize) {
        if let Some(q) = self.pending_questions.front_mut() {
            let count = q.choice_count();
            if count == 0 {
                return;
            }
            let last = count - 1;
            let next = (q.selected as isize + delta).clamp(0, last as isize) as usize;
            q.selected = next;
        }
    }

    /// Toggle the highlighted option's checkbox (`Space`), for a multi-select
    /// question only — a no-op on a single-select question or the "Other" row
    /// (#488).
    pub fn question_toggle(&mut self) {
        if let Some(q) = self.pending_questions.front_mut() {
            if q.current_question().multi_select && q.selected < q.current_question().options.len()
            {
                let idx = q.selected;
                if !q.picked.remove(&idx) {
                    q.picked.insert(idx);
                }
            }
        }
    }

    /// Toggle a specific option's checkbox by index (number-key quick toggle
    /// on a multi-select question) and move the cursor to it.
    pub fn question_toggle_at(&mut self, idx: usize) {
        if let Some(q) = self.pending_questions.front_mut() {
            if q.current_question().multi_select && idx < q.current_question().options.len() {
                q.selected = idx;
                if !q.picked.remove(&idx) {
                    q.picked.insert(idx);
                }
            }
        }
    }

    /// Enter free-text mode for the always-available "Other" entry.
    pub fn question_begin_free_form(&mut self) {
        if let Some(q) = self.pending_questions.front_mut() {
            q.selected = q.current_question().options.len();
            q.entering_free_form = true;
        }
    }

    /// Leave free-text mode, returning to choice selection.
    pub fn question_cancel_free_form(&mut self) {
        if let Some(q) = self.pending_questions.front_mut() {
            q.entering_free_form = false;
        }
    }

    /// Records `answer` as a **draft** for the current question of the front
    /// call (overwriting any prior draft in place) and steps forward — to the
    /// next question if any remain, otherwise to the review/submit step
    /// (#518: drafts are revisable and nothing is sent until an explicit
    /// Submit — see [`Self::question_answers_for_submit`]). Returns the
    /// free-text draft to reload into the shared input box, when stepping
    /// onto a question last answered free-form (e.g. one revisited after an
    /// earlier retreat); the caller should clear the input box otherwise.
    /// Check [`Self::pending_question`]`().is_reviewing()` to tell whether the
    /// call just reached the review step.
    pub fn commit_question_answer(&mut self, answer: Vec<String>) -> Option<String> {
        let q = self.pending_questions.front_mut()?;
        q.answers[q.current] = Some(answer);
        q.current += 1;
        q.sync_from_draft()
    }

    /// Steps back to the previous question (a no-op on the first question),
    /// restoring its existing draft into the on-screen selection state so it
    /// stays revisable (#518). Also leaves the review/submit step, if that's
    /// where the call was parked. Returns the free-text draft to reload into
    /// the shared input box, when stepping onto a question last answered
    /// free-form; the caller should clear the input box otherwise.
    pub fn question_retreat(&mut self) -> Option<String> {
        let q = self.pending_questions.front_mut()?;
        if q.current == 0 {
            return None;
        }
        q.current -= 1;
        q.sync_from_draft()
    }

    /// The final answers, ready to send as one `AnswerQuestion`, once the
    /// front call's review/submit step has been reached. `None` otherwise —
    /// never sent implicitly; the caller must act on `Some` explicitly.
    pub fn question_answers_for_submit(&self) -> Option<Vec<Vec<String>>> {
        self.pending_questions.front()?.answers_for_submit()
    }

    /// Pops the front call (answered and submitted) so the next queued one, if
    /// any, surfaces with its own fresh selection state (#273).
    pub fn advance_question(&mut self) {
        self.pending_questions.pop_front();
    }

    /// Drops the whole question queue (turn interrupt / terminal status).
    pub fn clear_question(&mut self) {
        self.pending_questions.clear();
    }

    /// Queue an `OutEvent::UserQuestion` (#273): the front is the one
    /// rendered. One id is one parked call, so a `UserQuestion` for a
    /// queued id — a `ReplaceQuestion`, or a resume's re-offer — replaces
    /// that entry in place. New content starts fresh drafts; the same
    /// content keeps the user's progress.
    pub(super) fn queue_question(&mut self, question: PendingQuestion) {
        match self
            .pending_questions
            .iter_mut()
            .find(|q| q.request_id == question.request_id)
        {
            Some(queued) if queued.questions != question.questions => *queued = question,
            Some(_) => {}
            None => self.pending_questions.push_back(question),
        }
    }

    /// The call `request_id` was resolved (its `ask_user` `ToolOutput`,
    /// #894): drop it, so the next queued call, if any, takes the prompt.
    pub fn drop_question(&mut self, request_id: &str) {
        self.pending_questions
            .retain(|q| q.request_id != request_id);
    }
}

#[cfg(test)]
#[path = "question_end_tests.rs"]
mod end_tests;
