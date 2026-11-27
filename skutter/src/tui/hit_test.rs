//! Pure geometry helpers for mouse hit-testing of the TUI's modal/popup lists
//! (Issue 1 — make everything reasonable clickable).
//!
//! Ratatui exposes no hit-testing: a `List` rendered into a `Rect` has no way
//! to map a terminal `(column, row)` back to the item under the cursor. The
//! drawers in `modals/` each compute their `Rect` at draw time, so the mouse
//! handler mirrors that geometry and uses the helpers here to turn a click into
//! a list index — the same index `ListState::select(...)` would move the
//! keyboard highlight to, before dispatching the row's `Enter` action.
//!
//! The list widgets all render inside a `Block::default().borders(Borders::ALL)`
//! frame, which consumes a 1-cell border on every side. So the first item row
//! sits at `area.y + 1` (not `area.y`), and a click on the border itself is not
//! a list row. [`list_row_index`] encodes that convention once.

use ratatui::layout::Rect;

/// The inner content area of a `Block::default().borders(Borders::ALL)`-drawn
/// widget — one cell inset on every side. Click hit-testing operates on this
/// inner rect, since the border holds no list items.
pub fn inner_rect(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

/// Whether a terminal `(column, row)` lands inside `area` (inclusive of the
/// border, since a click on the border still targets the modal, just not a row
/// — the caller decides what a border click means).
pub fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    area.width > 0
        && area.height > 0
        && column >= area.x
        && column < area.x + area.width
        && row >= area.y
        && row < area.y + area.height
}

/// Map a click `row` to the list item index under it, given the widget's outer
/// `Rect` (border included) and the number of items `len`.
///
/// Returns `None` when the click is outside the inner content area (above/below
/// the list, or on the top/bottom border). A click past the last item clamps to
/// the last valid index — mirroring how ratatui's `List` leaves the highlight on
/// the last row when the item count shrinks below the selected index, so a click
/// in the empty trailing space still lands on the final row (the intuitive
/// "click near the bottom hits the last entry" UX). `len == 0` always returns
/// `None` (no items to select).
pub fn list_row_index(area: Rect, row: u16, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let inner = inner_rect(area);
    if inner.height == 0 || row < inner.y || row >= inner.y + inner.height {
        return None;
    }
    let idx = (row - inner.y) as usize;
    Some(idx.min(len - 1))
}

/// Map a click `row` to a grouped item index where each group occupies `rows_per`
/// terminal rows — the MCP panel renders two lines per server (a header line
/// plus a tools/error sub-line), so a click anywhere in either line selects that
/// server. Same border/contain/clamp conventions as [`list_row_index`].
pub fn grouped_row_index(area: Rect, row: u16, len: usize, rows_per: u16) -> Option<usize> {
    if len == 0 || rows_per == 0 {
        return None;
    }
    let inner = inner_rect(area);
    if inner.height == 0 || row < inner.y || row >= inner.y + inner.height {
        return None;
    }
    let idx = (row - inner.y) / rows_per;
    Some((idx as usize).min(len - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Rect` at `(x=10, y=5, w=40, h=10)` — the spec's canonical fixture.
    /// Its bordered inner area starts at row 6 and holds up to 8 list rows
    /// (height 10 − 2 borders).
    fn fixture() -> Rect {
        Rect::new(10, 5, 40, 10)
    }

    #[test]
    fn inner_rect_insets_one_cell_each_side() {
        let inner = inner_rect(fixture());
        assert_eq!(inner.x, 11);
        assert_eq!(inner.y, 6);
        assert_eq!(inner.width, 38);
        assert_eq!(inner.height, 8);
    }

    #[test]
    fn click_on_first_item_row_maps_to_index_zero() {
        // Row 6 is the first content row (y=5 + 1 border).
        assert_eq!(list_row_index(fixture(), 6, 5), Some(0));
    }

    #[test]
    fn click_on_second_item_row_maps_to_index_one() {
        assert_eq!(list_row_index(fixture(), 7, 5), Some(1));
    }

    #[test]
    fn click_above_the_modal_is_outside() {
        // Row 4 is above the modal's top border (y=5).
        assert_eq!(list_row_index(fixture(), 4, 5), None);
    }

    #[test]
    fn click_on_top_border_is_not_a_row() {
        // Row 5 is the border itself — not a content row.
        assert_eq!(list_row_index(fixture(), 5, 5), None);
    }

    #[test]
    fn click_past_the_last_item_clamps_to_last_index() {
        // 5 items occupy rows 6–10; rows 11–13 are trailing empty space inside
        // the inner area (height 8). A click there clamps to index 4.
        assert_eq!(list_row_index(fixture(), 11, 5), Some(4));
        assert_eq!(list_row_index(fixture(), 13, 5), Some(4));
    }

    #[test]
    fn click_on_bottom_border_is_outside() {
        // Row 14 is the bottom border (y=5 + height 10 − 1).
        assert_eq!(list_row_index(fixture(), 14, 5), None);
    }

    #[test]
    fn empty_list_never_returns_an_index() {
        assert_eq!(list_row_index(fixture(), 6, 0), None);
    }

    #[test]
    fn contains_is_inclusive_of_the_border() {
        let area = fixture();
        // Corners and border edges count as "inside the modal".
        assert!(rect_contains(area, 10, 5)); // top-left border corner
        assert!(rect_contains(area, 49, 14)); // bottom-right border corner
                                              // Just outside.
        assert!(!rect_contains(area, 9, 5));
        assert!(!rect_contains(area, 50, 14));
        assert!(!rect_contains(area, 10, 4));
        assert!(!rect_contains(area, 10, 15));
    }

    #[test]
    fn zero_rect_contains_nothing() {
        assert!(!rect_contains(Rect::default(), 0, 0));
    }

    #[test]
    fn grouped_two_line_rows_select_the_right_group() {
        // MCP panel: 2 rows per server. Inner rows 6–13 → servers 0..3.
        let area = fixture();
        assert_eq!(grouped_row_index(area, 6, 4, 2), Some(0));
        assert_eq!(grouped_row_index(area, 7, 4, 2), Some(0)); // sub-line of server 0
        assert_eq!(grouped_row_index(area, 8, 4, 2), Some(1));
        assert_eq!(grouped_row_index(area, 9, 4, 2), Some(1));
        assert_eq!(grouped_row_index(area, 13, 4, 2), Some(3)); // clamps to last
    }

    #[test]
    fn grouped_row_past_items_clamps_to_last() {
        // 2 servers occupy inner rows 6–9; rows 10–13 are empty trailing space.
        assert_eq!(grouped_row_index(fixture(), 12, 2, 2), Some(1));
    }

    #[test]
    fn grouped_row_on_border_is_outside() {
        assert_eq!(grouped_row_index(fixture(), 5, 4, 2), None); // top border
    }
}
