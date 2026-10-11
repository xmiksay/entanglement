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
    // Arming the leader consumes the key — it must NOT fall through and leak an
    // `x` into the input (#326).
    assert_eq!(handler.handle_key(&leader_event), LeaderResult::Consumed);
    assert!(matches!(handler.state(), LeaderState::Pending { .. }));
}

#[test]
fn test_leader_handler_non_leader_key_is_not_mine() {
    let mut handler = LeaderKeyHandler::new();
    let other_event = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty());

    assert_eq!(handler.handle_key(&other_event), LeaderResult::NotMine);
    assert!(matches!(handler.state(), LeaderState::Idle));
}

#[test]
fn test_leader_handler_dispatches_action() {
    let mut handler = LeaderKeyHandler::new();
    let leader_event = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
    let quit_event = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty());

    handler.handle_key(&leader_event);
    let result = handler.handle_key(&quit_event);
    assert_eq!(result, LeaderResult::Action(Action::Quit));
    assert!(matches!(handler.state(), LeaderState::Idle));
}

#[test]
fn test_leader_handler_invalid_chord_consumed_and_reset() {
    let mut handler = LeaderKeyHandler::new();
    let leader_event = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
    // `z` is not bound to any chord — the second key must be swallowed too, not
    // leaked into the input, and the handler must reset to idle (#326).
    let invalid_event = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::empty());

    handler.handle_key(&leader_event);
    assert_eq!(handler.handle_key(&invalid_event), LeaderResult::Consumed);
    assert!(matches!(handler.state(), LeaderState::Idle));
}

#[test]
fn test_leader_handler_esc_cancels() {
    let mut handler = LeaderKeyHandler::new();
    let leader_event = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
    let esc_event = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());

    handler.handle_key(&leader_event);
    assert_eq!(handler.handle_key(&esc_event), LeaderResult::Consumed);
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

fn remapped(yaml: &str) -> KeyMap {
    let cfg: crate::config::keybindings::KeybindingsConfig = serde_yaml::from_str(yaml).unwrap();
    KeyMap::from_resolved(&cfg.resolve().unwrap())
}

fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}

/// #735: the default direct chords are the pre-#735 hardcoded ones, matched
/// the way crossterm reports them.
#[test]
fn direct_defaults_match_the_pre_735_keys() {
    let km = KeyMap::new();
    let (none, ctrl) = (KeyModifiers::NONE, KeyModifiers::CONTROL);
    let act = |code, mods| km.direct_action(&ev(code, mods));
    assert_eq!(act(KeyCode::Enter, none), Some(Action::Submit));
    assert_eq!(
        act(KeyCode::Enter, KeyModifiers::SHIFT),
        Some(Action::Newline)
    );
    assert_eq!(
        act(KeyCode::Enter, KeyModifiers::ALT),
        Some(Action::Newline)
    );
    assert_eq!(act(KeyCode::Char('j'), ctrl), Some(Action::Newline));
    assert_eq!(act(KeyCode::Tab, none), Some(Action::CycleMode));
    // crossterm may or may not flag SHIFT on BackTab (#322).
    assert_eq!(act(KeyCode::BackTab, none), Some(Action::CycleModeBack));
    assert_eq!(
        act(KeyCode::BackTab, KeyModifiers::SHIFT),
        Some(Action::CycleModeBack)
    );
    assert_eq!(act(KeyCode::PageUp, none), Some(Action::ScrollUp));
    assert_eq!(act(KeyCode::PageDown, none), Some(Action::ScrollDown));
    assert_eq!(act(KeyCode::Char(' '), ctrl), Some(Action::PauseToggle));
    assert_eq!(act(KeyCode::Char('g'), ctrl), Some(Action::JumpAttention));
    assert_eq!(act(KeyCode::Char('l'), ctrl), Some(Action::ListSessions));
    assert_eq!(act(KeyCode::Char('a'), ctrl), Some(Action::PickAgent));
    assert_eq!(act(KeyCode::Char('p'), ctrl), Some(Action::CommandPalette));
    assert_eq!(act(KeyCode::Char('a'), none), None, "typing is never bound");
    assert_eq!(act(KeyCode::Char('c'), ctrl), None, "reserved");
    let leader_e = km.leader_sequence().extend_with(Key::Char('E'));
    assert_eq!(km.get(&leader_e), Some(&Action::Export));
}

/// A modified PageUp/PageDown still scrolls (no exact binding needed), but
/// an exact binding for the modified chord wins.
#[test]
fn scrolling_ignores_modifiers_unless_the_chord_is_bound() {
    let shift = KeyModifiers::SHIFT;
    let km = KeyMap::new();
    assert_eq!(
        km.direct_action(&ev(KeyCode::PageUp, shift)),
        Some(Action::ScrollUp)
    );
    assert_eq!(
        km.direct_action(&ev(KeyCode::PageDown, KeyModifiers::CONTROL)),
        Some(Action::ScrollDown)
    );
    assert_eq!(
        km.direct_action(&ev(KeyCode::Enter, KeyModifiers::CONTROL)),
        None,
        "Enter stays exact"
    );
    let km = remapped("{pick_model: shift+pageup}");
    assert_eq!(
        km.direct_action(&ev(KeyCode::PageUp, shift)),
        Some(Action::PickModel)
    );
    let km = remapped("{scroll_up: []}");
    assert_eq!(
        km.direct_action(&ev(KeyCode::PageUp, shift)),
        None,
        "unbound stays unbound"
    );
}

/// #795: the modifier fallback is PageUp/PageDown only — scroll remapped to
/// Home/an arrow still scrolls on the bare key, but its modified chords
/// (Ctrl+Home, Ctrl+Left) stay free for the fixed document/word jumps.
#[test]
fn scroll_modifier_fallback_is_limited_to_page_keys() {
    let ctrl = KeyModifiers::CONTROL;
    let km = remapped("{scroll_up: home, scroll_down: left}");
    assert_eq!(
        km.direct_action(&ev(KeyCode::Home, KeyModifiers::NONE)),
        Some(Action::ScrollUp)
    );
    assert_eq!(km.direct_action(&ev(KeyCode::Home, ctrl)), None);
    assert_eq!(km.direct_action(&ev(KeyCode::Left, ctrl)), None);
    let km = remapped("{scroll_up: pageup}");
    assert_eq!(
        km.direct_action(&ev(KeyCode::PageUp, ctrl)),
        Some(Action::ScrollUp)
    );
}

/// The one-line fields (reject reason, free-text answer) submit on the
/// `submit` action, and on plain Enter unless `newline` took Enter.
#[test]
fn field_submit_follows_the_remap() {
    let none = KeyModifiers::NONE;
    let km = KeyMap::new();
    assert!(km.submits_field(&ev(KeyCode::Enter, none)));
    assert!(!km.submits_field(&ev(KeyCode::Enter, KeyModifiers::SHIFT)));
    let km = remapped("{submit: ctrl+s, newline: enter}");
    assert!(km.submits_field(&ev(KeyCode::Char('s'), KeyModifiers::CONTROL)));
    assert!(!km.submits_field(&ev(KeyCode::Enter, none)));
    let km = remapped("{submit: ctrl+s}");
    assert!(
        km.submits_field(&ev(KeyCode::Enter, none)),
        "Enter is a fallback when free"
    );
}

#[test]
fn a_remap_moves_the_leader_and_direct_keys() {
    let km = remapped("{leader: ctrl+b, submit: ctrl+s, pick_model: [ctrl+o, leader o]}");
    let ctrl = KeyModifiers::CONTROL;
    assert_eq!(km.leader_sequence(), &KeySequence::ctrl(Key::Char('b')));
    assert_eq!(
        km.direct_action(&ev(KeyCode::Char('s'), ctrl)),
        Some(Action::Submit)
    );
    assert_eq!(
        km.direct_action(&ev(KeyCode::Enter, KeyModifiers::NONE)),
        None
    );
    assert_eq!(
        km.direct_action(&ev(KeyCode::Char('o'), ctrl)),
        Some(Action::PickModel)
    );
    let leader_o = KeySequence::ctrl(Key::Char('b')).extend_with(Key::Char('o'));
    assert_eq!(km.get(&leader_o), Some(&Action::PickModel));
    let old_leader_m = KeySequence::ctrl(Key::Char('x')).extend_with(Key::Char('m'));
    assert_eq!(km.get(&old_leader_m), None);

    let mut handler = LeaderKeyHandler::new();
    handler.set_keymap(km);
    assert_eq!(
        handler.handle_key(&ev(KeyCode::Char('x'), ctrl)),
        LeaderResult::NotMine
    );
    assert_eq!(
        handler.handle_key(&ev(KeyCode::Char('b'), ctrl)),
        LeaderResult::Consumed
    );
    assert_eq!(
        handler.handle_key(&ev(KeyCode::Char('o'), KeyModifiers::NONE)),
        LeaderResult::Action(Action::PickModel)
    );
}

/// The help dialog lists the effective bindings, the leader prefix included.
#[test]
fn the_help_listing_shows_effective_bindings() {
    let km = remapped("{leader: ctrl+b, export: [], pick_model: [ctrl+o, leader o]}");
    assert_eq!(km.keys_for(Action::PickModel), "Ctrl+o/Ctrl+b o");
    assert_eq!(km.keys_for(Action::Help), "Ctrl+b ?/Ctrl+b h");
    assert_eq!(km.keys_for(Action::Leader), "Ctrl+b");
    assert_eq!(km.keys_for(Action::Export), "", "unbound actions drop out");
    assert_eq!(km.first_key(Action::JumpAttention), Some("Ctrl+g"));
    let categories: Vec<&str> = km
        .all_bindings()
        .iter()
        .map(|(_, a)| a.category())
        .collect();
    let mut sorted = categories.clone();
    sorted.sort_unstable();
    assert_eq!(categories, sorted, "grouped by category");
    assert_eq!(
        KeyMap::new().keys_for(Action::Newline),
        "Shift+Enter/Alt+Enter/Ctrl+j"
    );
}
