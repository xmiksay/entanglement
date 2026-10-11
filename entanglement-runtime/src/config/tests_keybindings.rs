use super::*;

fn parse(yaml: &str) -> KeybindingsConfig {
    serde_yaml::from_str(yaml).unwrap()
}

fn chord(key: KeyName, ctrl: bool, alt: bool, shift: bool) -> KeyChord {
    KeyChord::new(key, ctrl, alt, shift)
}

fn bound(keys: &ResolvedKeys, action: KeyAction) -> Vec<KeyBinding> {
    keys.bindings
        .iter()
        .filter(|(a, _)| *a == action)
        .map(|(_, b)| *b)
        .collect()
}

#[test]
fn chords_parse_modifiers_named_keys_and_chars() {
    assert_eq!(
        parse_chord("ctrl+x"),
        Ok(chord(KeyName::Char('x'), true, false, false))
    );
    assert_eq!(
        parse_chord("Ctrl+Alt+PageUp"),
        Ok(chord(KeyName::PageUp, true, true, false))
    );
    assert_eq!(
        parse_chord("shift+enter"),
        Ok(chord(KeyName::Enter, false, false, true))
    );
    assert_eq!(
        parse_chord("shift+tab"),
        Ok(chord(KeyName::BackTab, false, false, false))
    );
    assert_eq!(
        parse_chord("shift+e"),
        parse_chord("E"),
        "shift lives in the case"
    );
    assert_eq!(parse_chord("ctrl+space").unwrap().key, KeyName::Char(' '));
    assert_eq!(parse_chord("ctrl++").unwrap().key, KeyName::Char('+'));
    assert_eq!(parse_chord("f5").unwrap().key, KeyName::F(5));
    for bad in ["ctrl+", "hyper+x", "ctrl+banana", "f99", ""] {
        assert!(parse_chord(bad).is_err(), "{bad}");
    }
}

#[test]
fn bindings_parse_direct_and_leader_sequences() {
    assert_eq!(
        parse_binding("leader k"),
        Ok(KeyBinding::Leader(chord(
            KeyName::Char('k'),
            false,
            false,
            false
        )))
    );
    assert_eq!(
        parse_binding("leader E"),
        Ok(KeyBinding::Leader(chord(
            KeyName::Char('E'),
            false,
            false,
            false
        )))
    );
    assert!(parse_binding("leader ctrl+k").is_err());
    assert!(
        parse_binding("ctrl+x k").is_err(),
        "only `leader` starts a sequence"
    );
    assert!(parse_binding("leader").is_err());
}

#[test]
fn the_defaults_resolve_to_the_pre_735_map() {
    let keys = ResolvedKeys::defaults();
    assert_eq!(keys.leader, chord(KeyName::Char('x'), true, false, false));
    let leader = |c| KeyBinding::Leader(chord(KeyName::Char(c), false, false, false));
    assert_eq!(
        bound(&keys, KeyAction::Help),
        vec![leader('?'), leader('h')]
    );
    assert_eq!(bound(&keys, KeyAction::Export), vec![leader('E')]);
    assert_eq!(
        bound(&keys, KeyAction::ListSessions),
        vec![
            leader('l'),
            KeyBinding::Direct(chord(KeyName::Char('l'), true, false, false))
        ]
    );
    assert_eq!(bound(&keys, KeyAction::Newline).len(), 3);
    // Every action except the leader itself has at least one default key.
    for a in KeyAction::ALL.iter().filter(|a| **a != KeyAction::Leader) {
        assert!(!bound(&keys, *a).is_empty(), "{}", a.name());
    }
}

#[test]
fn an_override_replaces_that_actions_list_and_keeps_the_rest() {
    let keys = parse("{leader: ctrl+b, pick_model: [ctrl+o, leader o], export: []}")
        .resolve()
        .unwrap();
    assert_eq!(keys.leader, chord(KeyName::Char('b'), true, false, false));
    assert_eq!(
        bound(&keys, KeyAction::PickModel),
        vec![
            KeyBinding::Direct(chord(KeyName::Char('o'), true, false, false)),
            KeyBinding::Leader(chord(KeyName::Char('o'), false, false, false)),
        ]
    );
    assert!(bound(&keys, KeyAction::Export).is_empty(), "`[]` unbinds");
    assert_eq!(bound(&keys, KeyAction::Help).len(), 2, "untouched default");
}

#[test]
fn a_conflict_names_both_actions_and_rejects_the_map() {
    let err = parse("{pick_model: ctrl+g}").resolve().unwrap_err();
    assert!(
        err.contains("`jump_attention`") && err.contains("`pick_model`"),
        "{err}"
    );
    assert!(err.contains("keeping the previous key map"), "{err}");
    let err = parse("{inspect: leader n}").resolve().unwrap_err();
    assert!(
        err.contains("`new_session`") && err.contains("`inspect`"),
        "{err}"
    );
    let err = parse("{export: ctrl+x}").resolve().unwrap_err();
    assert!(
        err.contains("`leader`") && err.contains("`export`"),
        "{err}"
    );
}

#[test]
fn the_same_key_in_different_contexts_is_not_a_conflict() {
    // `leader l` and a direct `l`-chord live in separate contexts.
    assert!(parse("{pick_model: ctrl+n, new_session: leader m}")
        .resolve()
        .is_ok());
    assert!(parse("{pick_model: [leader m, ctrl+o]}").resolve().is_ok());
}

#[test]
fn reserved_printable_and_malformed_keys_are_rejected() {
    for (yaml, needle) in [
        ("{export: ctrl+c}", "reserved"),
        ("{export: ctrl+q}", "reserved"),
        ("{export: esc}", "reserved"),
        ("{export: leader esc}", "reserved"),
        ("{export: x}", "stop that key typing"),
        ("{export: shift+x}", "stop that key typing"),
        ("{export: ctrl+nope}", "unknown key"),
        ("{exprot: ctrl+o}", "unknown action `exprot`"),
        ("{leader: leader x}", "can't start with `leader`"),
        ("{leader: []}", "exactly one key"),
        ("{submit: []}", "at least one key"),
    ] {
        let err = parse(yaml).resolve().unwrap_err();
        assert!(err.contains(needle), "{yaml}: {err}");
    }
}

#[test]
fn every_problem_is_reported_in_one_message() {
    let err = parse("{exprot: ctrl+o, help: ctrl+c}")
        .resolve()
        .unwrap_err();
    assert!(err.contains("exprot") && err.contains("ctrl+c"), "{err}");
    assert_eq!(parse("{exprot: ctrl+o}").warnings().len(), 1);
    assert!(KeybindingsConfig::default().warnings().is_empty());
}

#[test]
fn chords_display_like_the_help_dialog_expects() {
    assert_eq!(parse_chord("ctrl+x").unwrap().to_string(), "Ctrl+x");
    assert_eq!(parse_chord("shift+tab").unwrap().to_string(), "Shift+Tab");
    assert_eq!(parse_chord("ctrl+space").unwrap().to_string(), "Ctrl+Space");
    assert_eq!(parse_chord("alt+enter").unwrap().to_string(), "Alt+Enter");
    assert_eq!(parse_chord("pagedown").unwrap().to_string(), "PageDown");
}
