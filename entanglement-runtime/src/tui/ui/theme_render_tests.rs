//! #735: rendered-frame checks for the theme. The dark default must draw the
//! exact pre-#735 colours (the values were hardcoded at each call site
//! before), `light` must recolour the same cells, and `NO_COLOR` must leave
//! every cell on the terminal default.

use entanglement_core::{OutEvent, SessionId};
use ratatui::{
    backend::TestBackend,
    buffer::{Buffer, Cell},
    style::{Color, Modifier},
    Terminal,
};

use super::draw_body;
use crate::config::TuiConfig;
use crate::tui::app::App;

const W: u16 = 100;
const H: u16 = 30;

/// An app parked on an `edit` approval: a diff body, the yellow rules and the
/// coloured `[y]`/`[n]` footer keys all render in the body.
fn parked_edit(theme_yaml: &str, no_color: bool) -> Buffer {
    let sid = SessionId::new("s1");
    let mut app = App::new_for_test(sid.clone());
    let cfg: TuiConfig = serde_yaml::from_str(&format!("{{theme: {theme_yaml}}}")).unwrap();
    app.apply_appearance(&cfg, no_color, true);
    app.handle_out_event(OutEvent::ToolRequest {
        session: sid,
        seq: 1,
        request_id: "r1".to_string(),
        tool: "edit".to_string(),
        input: r#"{"path":"a.txt","oldString":"old line","newString":"new line"}"#.to_string(),
        reason: None,
        scopes: Vec::new(),
    });
    let mut terminal = Terminal::new(TestBackend::new(W, H)).unwrap();
    terminal.draw(|f| draw_body(f, f.area(), &mut app)).unwrap();
    terminal.backend().buffer().clone()
}

/// The fg of the first cell of the first occurrence of `needle` on screen.
fn fg_of(buf: &Buffer, needle: &str) -> Color {
    cell_of(buf, needle).fg
}

fn cell_of<'b>(buf: &'b Buffer, needle: &str) -> &'b Cell {
    for y in 0..H {
        let row: Vec<&str> = (0..W).map(|x| buf[(x, y)].symbol()).collect();
        let text = row.concat();
        if let Some(byte) = text.find(needle) {
            // Every cell here is one symbol; map the byte offset to a column.
            let col = text[..byte].chars().count() as u16;
            return &buf[(col, y)];
        }
    }
    panic!("`{needle}` not rendered:\n{}", dump(buf));
}

fn dump(buf: &Buffer) -> String {
    (0..H)
        .map(|y| (0..W).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_dark_default_draws_the_pre_735_colours() {
    let buf = parked_edit("dark", false);
    assert_eq!(fg_of(&buf, "──────"), Color::Yellow, "approval rule");
    assert_eq!(fg_of(&buf, "[y]"), Color::Green);
    assert_eq!(fg_of(&buf, "[n]"), Color::Red);
    assert_eq!(fg_of(&buf, "[e]"), Color::Yellow);
    assert_eq!(fg_of(&buf, "[v]"), Color::Cyan);
    assert_eq!(fg_of(&buf, "[Esc]"), Color::Gray);
    assert_eq!(fg_of(&buf, "+ new line"), Color::Green, "diff insert");
    assert_eq!(fg_of(&buf, "- old line"), Color::Red, "diff delete");
    assert_eq!(fg_of(&buf, "▾"), Color::Yellow, "approval header arrow");
    let (x, y) = (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .find(|&(x, y)| buf[(x, y)].symbol() == "▾")
        .unwrap();
    assert_eq!(buf[(x, y)].bg, Color::Rgb(30, 30, 35), "message background");
}

#[test]
fn light_recolours_the_same_cells() {
    let buf = parked_edit("light", false);
    assert_eq!(fg_of(&buf, "──────"), Color::Rgb(140, 85, 0));
    assert_eq!(fg_of(&buf, "[y]"), Color::Rgb(30, 110, 40));
    assert_eq!(fg_of(&buf, "[n]"), Color::Rgb(180, 30, 30));
    assert_eq!(fg_of(&buf, "+ new line"), Color::Rgb(30, 110, 40));
    assert_eq!(fg_of(&buf, "- old line"), Color::Rgb(180, 30, 30));
}

#[test]
fn a_slot_override_reaches_the_rendered_cell() {
    let buf = parked_edit(
        "{base: dark, slots: {diff.add: '#2e7d32', approval: 33}}",
        false,
    );
    assert_eq!(fg_of(&buf, "+ new line"), Color::Rgb(0x2e, 0x7d, 0x32));
    assert_eq!(fg_of(&buf, "──────"), Color::Indexed(33));
    assert_eq!(fg_of(&buf, "- old line"), Color::Red, "untouched slot");
}

#[test]
fn no_color_leaves_every_cell_on_the_terminal_default() {
    let buf = parked_edit("light", true);
    for y in 0..H {
        for x in 0..W {
            let cell = &buf[(x, y)];
            assert_eq!(
                (cell.fg, cell.bg),
                (Color::Reset, Color::Reset),
                "({x},{y})"
            );
        }
    }
    // Emphasis survives: the footer keys stay bold.
    assert!(cell_of(&buf, "[y]").modifier.contains(Modifier::BOLD));
}
