//! Keyboard handling for a pending `ask_user` call (#488, #518).

use anyhow::Result;
use entanglement_core::{Holly, InMsg};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::app::App;

use super::DIALOG_PAGE_SIZE;

/// Records `answer` as a draft for the pending call's current question and
/// steps forward — to its next question, or to the review/submit step once
/// every question in the call has a draft (#518). Nothing is sent here.
pub(crate) fn commit_current_answer(app: &mut App, answer: Vec<String>) {
    app.commit_question_answer(answer);
}

/// Sends the front call's drafted answers as one [`InMsg::AnswerQuestion`] and
/// promotes the next queued call, if any (#273, #488, #518) — the explicit
/// Submit action, only reachable once every question has a draft.
async fn submit_pending_question(
    app: &mut App,
    holly: &Holly,
    session: &entanglement_core::SessionId,
    request_id: &str,
) {
    if let Some(answers) = app.question_answers_for_submit() {
        let _ = holly
            .send(InMsg::answer_question(
                session.clone(),
                request_id.to_string(),
                answers,
            ))
            .await;
        app.advance_question();
    }
}

/// Drive a pending `ask_user` call (#488, supersedes parts of ADR-0027; #518
/// draft-until-submit): arrow/number selection over the labelled options
/// (checkboxes + `Space` toggle for a multi-select question), plus an
/// always-available "Other" entry that opens the shared input box for a
/// free-text answer. Answers are drafts — `Left`/`Backspace` steps back to any
/// earlier question in the batch to revise it, and once every question has a
/// draft the call parks on a review step whose own `Enter` is the one explicit
/// Submit that sends the batch as a single [`InMsg::AnswerQuestion`]. `Esc`
/// while editing a question interrupts the turn like an approval; `Esc` on
/// the review step just steps back to revise instead (sends nothing, keeps
/// the call parked).
pub(crate) async fn handle_question_event(
    app: &mut App,
    holly: &Holly,
    key: KeyEvent,
) -> Result<bool> {
    let Some(q) = app.pending_question() else {
        return Ok(false);
    };
    let request_id = q.request_id.clone();
    let session = app.active_session_id().clone();

    if q.is_reviewing() {
        match key.code {
            KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
                return Ok(true);
            }
            KeyCode::Enter => {
                submit_pending_question(app, holly, &session, &request_id).await;
            }
            KeyCode::Left | KeyCode::Backspace | KeyCode::Esc => {
                app.question_retreat();
            }
            _ => {}
        }
        return Ok(false);
    }

    let entering = q.entering_free_form;
    let free_form_selected = q.free_form_selected();
    let multi_select = q.is_multi_select();
    let can_retreat = q.current > 0;
    let selected_label = q
        .current_question()
        .options
        .get(q.selected)
        .map(|o| o.label.clone());
    let picked_labels: Vec<String> = {
        let mut idxs: Vec<usize> = q.picked.iter().copied().collect();
        idxs.sort_unstable();
        let options = &q.current_question().options;
        idxs.into_iter()
            .filter_map(|i| options.get(i).map(|o| o.label.clone()))
            .collect()
    };

    if entering {
        // Shared input-edit keys (Ctrl+arrows, Home/End, doc jumps, Alt+Enter
        // newline) — Enter stays = submit below.
        if crate::tui::event_loop::apply_input_edit_key(app, &key) {
            return Ok(false);
        }
        match key.code {
            KeyCode::Esc => {
                let _ = app.take_input_text();
                app.question_cancel_free_form();
            }
            KeyCode::Enter => {
                let text = app.take_input_text();
                if !text.is_empty() {
                    commit_current_answer(app, vec![text]);
                }
            }
            KeyCode::Char(c) => app.input().insert_char(c),
            KeyCode::Backspace => app.input().delete_char(),
            KeyCode::Left => app.input().move_cursor_left(),
            KeyCode::Right => app.input().move_cursor_right(),
            _ => {}
        }
        return Ok(false);
    }

    match key.code {
        KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        KeyCode::Up | KeyCode::Char('k') => app.question_move(-1),
        KeyCode::Down | KeyCode::Char('j') => app.question_move(1),
        KeyCode::PageUp => app.question_page(-(DIALOG_PAGE_SIZE as isize)),
        KeyCode::PageDown => app.question_page(DIALOG_PAGE_SIZE as isize),
        KeyCode::Char(' ') => app.question_toggle(),
        // Back to the previous question to revise it (#518) — a no-op on the
        // batch's first question.
        KeyCode::Left | KeyCode::Backspace if can_retreat => {
            app.question_retreat();
        }
        // Quick-pick by number: options are 1-based; the "Other" entry follows.
        // Multi-select toggles the option; single-select commits immediately.
        KeyCode::Char(c @ '1'..='9') => {
            let idx = (c as u8 - b'1') as usize;
            let opt_count = app
                .pending_question()
                .map(|q| q.current_question().options.len())
                .unwrap_or(0);
            if idx < opt_count {
                if multi_select {
                    app.question_toggle_at(idx);
                } else if let Some(label) = app.pending_question().and_then(|q| {
                    q.current_question()
                        .options
                        .get(idx)
                        .map(|o| o.label.clone())
                }) {
                    commit_current_answer(app, vec![label]);
                }
            } else if idx == opt_count {
                app.question_begin_free_form();
            }
        }
        KeyCode::Enter => {
            if free_form_selected {
                app.question_begin_free_form();
            } else if multi_select {
                commit_current_answer(app, picked_labels);
            } else if let Some(label) = selected_label {
                commit_current_answer(app, vec![label]);
            }
        }
        KeyCode::Esc => {
            let _ = holly.send(InMsg::Stop { session }).await;
            app.clear_question();
        }
        _ => {}
    }
    Ok(false)
}
