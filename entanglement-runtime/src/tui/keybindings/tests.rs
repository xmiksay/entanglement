use super::*;

#[test]
fn test_key_sequence_display() {
    let seq = KeySequence::ctrl(Key::Char('x'));
    assert_eq!(format!("{}", seq), "Ctrl+x");
}

#[test]
fn test_key_sequence_starts_with() {
    let leader = KeySequence::ctrl(Key::Char('x'));
    let extended = leader.extend_with(Key::Char('q'));
    assert!(extended.starts_with(&leader));
}

#[test]
fn test_keymap_quit_binding() {
    let keymap = KeyMap::new();
    let leader = KeySequence::ctrl(Key::Char('x'));
    let quit_seq = leader.extend_with(Key::Char('q'));
    assert_eq!(keymap.get(&quit_seq), Some(&Action::Quit));
}

#[test]
fn test_leader_handler_pending_state() {
    let mut handler = LeaderKeyHandler::new();
    let leader_event = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);

    assert!(matches!(handler.state(), LeaderState::Idle));
    assert!(handler.handle_key(&leader_event).is_none());
    assert!(matches!(handler.state(), LeaderState::Pending { .. }));
}

#[test]
fn test_leader_handler_dispatches_action() {
    let mut handler = LeaderKeyHandler::new();
    let leader_event = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
    let quit_event = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty());

    handler.handle_key(&leader_event);
    let action = handler.handle_key(&quit_event);
    assert_eq!(action, Some(Action::Quit));
    assert!(matches!(handler.state(), LeaderState::Idle));
}

#[test]
fn test_leader_handler_esc_cancels() {
    let mut handler = LeaderKeyHandler::new();
    let leader_event = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
    let esc_event = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());

    handler.handle_key(&leader_event);
    handler.handle_key(&esc_event);
    assert!(matches!(handler.state(), LeaderState::Idle));
}

#[test]
fn test_action_category() {
    assert_eq!(Action::Quit.category(), "General");
    assert_eq!(Action::NewSession.category(), "Sessions");
    assert_eq!(Action::PickAgent.category(), "Agent");
    assert_eq!(Action::ScrollUp.category(), "Navigation");
}

#[test]
fn test_leader_handler_timeout() {
    let mut handler = LeaderKeyHandler::new();
    let leader_event = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);

    handler.handle_key(&leader_event);
    assert!(matches!(handler.state(), LeaderState::Pending { .. }));

    std::thread::sleep(handler.timeout() + std::time::Duration::from_millis(100));
    handler.check_timeout();
    assert!(matches!(handler.state(), LeaderState::Idle));
}

#[test]
fn test_keymap_multiple_candidates() {
    let keymap = KeyMap::new();
    let leader = KeySequence::ctrl(Key::Char('x'));
    let candidates = keymap.get_candidates(&leader);
    assert!(!candidates.is_empty());
    assert!(candidates
        .iter()
        .any(|(_, action)| matches!(action, Action::Quit)));
    assert!(candidates
        .iter()
        .any(|(_, action)| matches!(action, Action::NewSession)));
}
