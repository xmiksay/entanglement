use super::SimpleInput;

/// The exact repro from issue #101: a single multibyte char followed by the
/// mention recompute that slices the line — must not split the code point.
#[test]
fn multibyte_insert_then_before_cursor_slice() {
    let mut input = SimpleInput::default();
    input.insert_char('é');
    assert_eq!(input.lines(), &["é".to_string()]);
    assert_eq!(input.cursor(), (0, 'é'.len_utf8()));
    // Previously panicked: byte index 1 is not a char boundary.
    assert_eq!(input.current_line_before_cursor(), "é");
}

#[test]
fn multibyte_str_insert_advances_by_bytes() {
    let mut input = SimpleInput::default();
    input.insert_str("aé🚀c");
    assert_eq!(input.lines(), &["aé🚀c".to_string()]);
    assert_eq!(input.cursor_col(), "aé🚀c".len());
    assert_eq!(input.current_line_before_cursor(), "aé🚀c");
}

#[test]
fn multibyte_delete_removes_whole_char() {
    let mut input = SimpleInput::default();
    input.insert_str("aé");
    input.delete_char();
    assert_eq!(input.lines(), &["a".to_string()]);
    assert_eq!(input.cursor_col(), 1);
    input.delete_char();
    assert_eq!(input.lines(), &[String::new()]);
    assert_eq!(input.cursor_col(), 0);
}

#[test]
fn multibyte_left_right_step_over_code_points() {
    let mut input = SimpleInput::default();
    input.insert_str("é🚀");
    input.move_cursor_left();
    assert_eq!(input.cursor_col(), 'é'.len_utf8());
    input.move_cursor_left();
    assert_eq!(input.cursor_col(), 0);
    input.move_cursor_left(); // clamped at head
    assert_eq!(input.cursor_col(), 0);
    input.move_cursor_right();
    assert_eq!(input.cursor_col(), 'é'.len_utf8());
    input.move_cursor_right();
    assert_eq!(input.cursor_col(), "é🚀".len());
    input.move_cursor_right(); // clamped at end
    assert_eq!(input.cursor_col(), "é🚀".len());
}

#[test]
fn multibyte_head_end_and_display_col() {
    let mut input = SimpleInput::default();
    input.insert_str("é🚀"); // é width 1, 🚀 width 2
    assert_eq!(input.cursor_display_col(), 3);
    input.move_cursor_to_head();
    assert_eq!(input.cursor_col(), 0);
    assert_eq!(input.cursor_display_col(), 0);
    input.move_cursor_to_end();
    assert_eq!(input.cursor_col(), "é🚀".len());
    assert_eq!(input.cursor_display_col(), 3);
}

#[test]
fn newline_splits_on_char_boundary() {
    let mut input = SimpleInput::default();
    input.insert_str("aé🚀c");
    input.move_cursor_left(); // between 🚀 and c
    input.insert_newline();
    assert_eq!(input.lines(), &["aé🚀".to_string(), "c".to_string()]);
    assert_eq!(input.cursor(), (1, 0));
}

#[test]
fn move_up_down_clamps_to_char_boundary() {
    let mut input = SimpleInput::default();
    input.insert_str("aaaa");
    input.insert_newline();
    input.insert_str("é"); // row 1, cursor past the é (col 2)
    input.move_cursor_up(); // row 0, col floored to a boundary
    let (row, col) = input.cursor();
    assert_eq!(row, 0);
    assert!(input.lines()[0].is_char_boundary(col));
    input.move_cursor_down();
    let (row, col) = input.cursor();
    assert_eq!(row, 1);
    assert!(input.lines()[1].is_char_boundary(col));
}

#[test]
fn delete_word_on_multibyte_line() {
    let mut input = SimpleInput::default();
    input.insert_str("héllo   ");
    input.delete_word();
    assert_eq!(input.lines(), &["héllo".to_string()]);
    assert_eq!(input.cursor_col(), "héllo".len());
}

// Issue #101 cluster 2: editing keys on a fresh (empty `lines`) buffer must
// not index-out-of-bounds.
#[test]
fn empty_buffer_edit_keys_do_not_panic() {
    SimpleInput::default().insert_newline();
    SimpleInput::default().delete_line_by_end();
    SimpleInput::default().delete_line_by_head();
    SimpleInput::default().delete_word();
    SimpleInput::default().delete_char();
    SimpleInput::default().move_cursor_left();
    SimpleInput::default().move_cursor_right();
    SimpleInput::default().move_cursor_up();
    SimpleInput::default().move_cursor_down();
    SimpleInput::default().move_cursor_to_end();
    assert_eq!(SimpleInput::default().current_line_before_cursor(), "");
    assert_eq!(SimpleInput::default().cursor_display_col(), 0);
}

#[test]
fn empty_buffer_newline_then_type() {
    let mut input = SimpleInput::default();
    input.insert_newline();
    assert_eq!(input.cursor(), (1, 0));
    input.insert_char('x');
    assert_eq!(input.lines(), &[String::new(), "x".to_string()]);
}

#[test]
fn delete_char_joins_lines() {
    let mut input = SimpleInput::default();
    input.insert_str("aé");
    input.insert_newline();
    input.insert_str("bc");
    input.move_cursor_to_head();
    input.delete_char(); // join line 1 back onto line 0
    assert_eq!(input.lines(), &["aébc".to_string()]);
    assert_eq!(input.cursor(), (0, "aé".len()));
}

#[test]
fn ctrl_k_and_ctrl_u_on_multibyte() {
    let mut input = SimpleInput::default();
    input.insert_str("aébéc");
    input.move_cursor_left(); // before final c
    input.move_cursor_left(); // between é and c → after "aéb"
    input.delete_line_by_end();
    assert_eq!(input.lines(), &["aéb".to_string()]);

    let mut input = SimpleInput::default();
    input.insert_str("aébéc");
    input.move_cursor_left();
    input.move_cursor_left(); // after "aéb"
    input.delete_line_by_head();
    assert_eq!(input.lines(), &["éc".to_string()]);
    assert_eq!(input.cursor_col(), 0);
}

// --- New: word-jump and document-home/end movement ---

#[test]
fn move_word_left_jumps_over_whitespace_then_word() {
    // "foo  bar baz" (col indices: foo=0..3, two spaces=3..5, bar=5..8,
    // space=8, baz=9..12). Cursor starts at the end (col 12).
    let mut input = SimpleInput::default();
    input.insert_str("foo  bar baz");
    input.move_word_left(); // end → start of "baz" (col 9)
    assert_eq!(input.cursor_col(), 9);
    input.move_word_left(); // → start of "bar" (col 5)
    assert_eq!(input.cursor_col(), 5);
    input.move_word_left(); // → start of "foo" (col 0)
    assert_eq!(input.cursor_col(), 0);
    input.move_word_left(); // clamped at head
    assert_eq!(input.cursor_col(), 0);
}

#[test]
fn move_word_right_jumps_to_start_of_next_word() {
    // "foo  bar baz"; cursor starts at col 0.
    let mut input = SimpleInput::default();
    input.insert_str("foo  bar baz");
    assert_eq!(input.cursor_col(), "foo  bar baz".len());
    for _ in 0.."foo  bar baz".len() {
        input.move_cursor_left();
    }
    assert_eq!(input.cursor_col(), 0);
    input.move_word_right(); // → start of "bar" (col 5)
    assert_eq!(input.cursor_col(), 5);
    input.move_word_right(); // → start of "baz" (col 9)
    assert_eq!(input.cursor_col(), 9);
    input.move_word_right(); // past "baz" to end (col 12)
    assert_eq!(input.cursor_col(), "foo  bar baz".len());
    input.move_word_right(); // clamped at end
    assert_eq!(input.cursor_col(), "foo  bar baz".len());
}

#[test]
fn move_word_right_mid_word_only_skips_remainder() {
    // Cursor in the middle of a word only crosses the rest of that word + the
    // gap, not the whole word run — standard Ctrl+Right behavior.
    let mut input = SimpleInput::default();
    input.insert_str("hello world");
    // Park the cursor after "he" (col 2).
    for _ in 0.."world".len() {
        input.move_cursor_left();
    }
    input.move_cursor_left(); // col 6? recompute below instead
    input.move_cursor_to_head();
    input.move_cursor_right();
    input.move_cursor_right(); // col 2, after "he"
    assert_eq!(input.cursor_col(), 2);
    input.move_word_right(); // → start of "world" (col 6)
    assert_eq!(input.cursor_col(), 6);
}

#[test]
fn move_word_steps_over_multibyte_chars() {
    let mut input = SimpleInput::default();
    input.insert_str("é héllo"); // é<space>héllo
                                 // Cursor at end → jump left once lands at start of "héllo" (after the space).
    input.move_word_left();
    assert_eq!(input.cursor_col(), "é ".len());
    // Back to end, then jump left again reaches start of "é".
    input.move_cursor_to_end();
    input.move_word_left();
    input.move_word_left();
    assert_eq!(input.cursor_col(), 0);
}

#[test]
fn move_to_doc_home_and_end_across_lines() {
    let mut input = SimpleInput::default();
    input.insert_str("aaa");
    input.insert_newline();
    input.insert_str("bbb");
    input.insert_newline();
    input.insert_str("ccc");
    // Cursor currently at end of line 2.
    assert_eq!(input.cursor(), (2, 3));

    input.move_to_doc_home();
    assert_eq!(input.cursor(), (0, 0));

    input.move_to_doc_end();
    assert_eq!(input.cursor_row(), 2);
    assert_eq!(input.cursor_col(), "ccc".len());
}

#[test]
fn doc_end_clamps_to_last_line_boundary() {
    let mut input = SimpleInput::default();
    input.insert_str("x");
    input.insert_newline();
    // Park the cursor mid-line on row 1, then jump to doc end.
    input.insert_str("ab");
    input.move_cursor_left();
    input.move_to_doc_end();
    assert_eq!(input.cursor_row(), 1);
    assert_eq!(input.cursor_col(), "ab".len());
}

#[test]
fn empty_buffer_word_and_doc_moves_do_not_panic() {
    let mut input = SimpleInput::default();
    input.move_word_left();
    input.move_word_right();
    input.move_to_doc_home();
    input.move_to_doc_end();
    assert_eq!(input.cursor(), (0, 0));
}
