//! Every `Stop` key path clears the stopped view's cards (ADR-0237): the
//! runtime's `stop_session` unwinds a background script's card, which a
//! terminal `Status` no longer clears.

use entanglement_core::{EngineConfig, OutEvent, Question, QuestionOption, Questions, SessionId};
use ratatui::crossterm::event::KeyEvent;

use super::*;

fn engine() -> Holly {
    Holly::spawn(EngineConfig::default())
}

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn detached_card(session: &SessionId) -> OutEvent {
    OutEvent::ToolRequest {
        session: session.clone(),
        seq: 1,
        request_id: "bg".into(),
        tool: "edit (rhai)".into(),
        input: "{}".into(),
        reason: None,
        scopes: Vec::new(),
        detached: true,
    }
}

fn has_card(app: &App, session: &SessionId) -> bool {
    app.sessions()
        .into_iter()
        .any(|(id, v)| id == session && v.pending_tool_request().is_some())
}

async fn press(app: &mut App, holly: &Holly, ev: Event) {
    let mut attention = Attention::new(false);
    let quit = handle_event(app, holly, &mut attention, ev).await.unwrap();
    assert!(!quit);
}

#[tokio::test]
async fn approval_esc_clears_the_active_detached_card() {
    let sid = SessionId::new("s");
    let mut app = App::new_for_test(sid.clone());
    app.handle_out_event(detached_card(&sid));
    press(&mut app, &engine(), key(KeyCode::Esc)).await;
    assert!(!has_card(&app, &sid));
    assert!(matches!(app.approval_mode(), ApprovalMode::Normal));
}

#[tokio::test]
async fn question_esc_clears_the_active_detached_card() {
    let sid = SessionId::new("s");
    let mut app = App::new_for_test(sid.clone());
    app.handle_out_event(detached_card(&sid));
    app.handle_out_event(OutEvent::UserQuestion {
        session: sid.clone(),
        seq: 2,
        request_id: "q".into(),
        questions: Questions(vec![Question {
            question: "Which?".into(),
            options: vec![QuestionOption {
                label: "A".into(),
                description: None,
            }],
            multi_select: false,
        }]),
    });
    assert!(app.is_asking());
    press(&mut app, &engine(), key(KeyCode::Esc)).await;
    assert!(!app.is_asking());
    assert!(!has_card(&app, &sid));
}

#[tokio::test]
async fn slash_stop_all_clears_a_background_sessions_detached_card() {
    let background = SessionId::new("bg-session");
    let mut app = App::new_for_test(background.clone());
    let active = app.create_session();
    app.handle_out_event(detached_card(&background));
    assert!(has_card(&app, &background));
    app.set_input_text("/stop --all".to_string());
    press(&mut app, &engine(), key(KeyCode::Enter)).await;
    assert!(!has_card(&app, &background));
    assert!(!has_card(&app, &active));
}

#[tokio::test]
async fn sessions_modal_s_clears_the_highlighted_sessions_detached_card() {
    let background = SessionId::new("bg-session");
    let mut app = App::new_for_test(background.clone());
    app.create_session();
    app.handle_out_event(detached_card(&background));
    app.toggle_sessions_modal();
    for _ in 0..app.sessions().len() {
        if app.modal_selected_session_id().as_ref() == Some(&background) {
            break;
        }
        app.sessions_modal_next();
    }
    assert_eq!(app.modal_selected_session_id(), Some(background.clone()));
    press(&mut app, &engine(), key(KeyCode::Char('s'))).await;
    assert!(!has_card(&app, &background));
}
