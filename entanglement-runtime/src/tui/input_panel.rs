use entanglement_core::AgentState;
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::tui::app::App;
use crate::tui::modals;
use crate::tui::progress;
use crate::tui::session_view::ApprovalMode;

mod status_usage;

pub fn draw_top_padding(f: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let paragraph = Paragraph::new("").style(Style::default().bg(theme.input_bg));
    f.render_widget(paragraph, area);
}

pub fn draw_profile_badge(f: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let user_input = theme.user_input_colors(app.profile_color_for(app.agent()));

    let agent_color = app.profile_color_for(app.agent());
    let state_color = match app.state() {
        AgentState::Idle => Color::Green,
        AgentState::Thinking => Color::Yellow,
        AgentState::Working => Color::Yellow,
        AgentState::WaitingAgent => Color::Magenta,
        AgentState::WaitingApproval => Color::Cyan,
        AgentState::WaitingAnswer => Color::Cyan,
        AgentState::Paused => Color::Magenta,
        AgentState::Done => Color::Blue,
        AgentState::Error => Color::Red,
    };

    let state_text = match app.state() {
        AgentState::Idle => "Idle",
        AgentState::Thinking => "Thinking",
        AgentState::Working => "Working",
        AgentState::WaitingAgent => "WaitingAgent",
        AgentState::WaitingApproval => "WaitingApproval",
        AgentState::WaitingAnswer => "WaitingAnswer",
        AgentState::Paused => "Paused",
        AgentState::Done => "Done",
        AgentState::Error => "Error",
    };

    let badge_top = Line::from(vec![Span::styled(
        app.agent(),
        Style::default().fg(agent_color).bold(),
    )]);

    let badge_bottom = Line::from(vec![Span::styled(
        state_text,
        Style::default().fg(state_color),
    )]);

    let vertical_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    let top_badge = Paragraph::new(badge_top)
        .alignment(Alignment::Center)
        .style(Style::default().bg(user_input.bg));
    f.render_widget(top_badge, vertical_chunks[0]);

    if let Some(since) = app.thinking_since() {
        progress::draw_ship_cruise(
            f,
            vertical_chunks[1],
            since,
            app.profile_color_for(app.agent()),
            app.theme(),
        );
    } else {
        let bottom_badge = Paragraph::new(badge_bottom)
            .alignment(Alignment::Center)
            .style(Style::default().bg(user_input.bg));
        f.render_widget(bottom_badge, vertical_chunks[1]);
    }
}

pub fn draw_input(f: &mut Frame, area: Rect, app: &mut App) {
    let approval_mode = app.approval_mode().clone();
    let theme = app.theme();

    // Approval + question prompts render fully in the transcript (view area):
    // the `?` tool header, args, `[y]/[n]` hint, and numbered choices all live
    // there. The input box is only an active text field in the two modes where
    // the user is actually typing — a rejection reason or a free-form answer —
    // so it only carries a placeholder hint then. Otherwise it mirrors the
    // user's pending text (or stays blank), never duplicating the view prompt.
    let placeholder_text = if app.is_asking() {
        if app
            .pending_question()
            .map(|q| q.entering_free_form)
            .unwrap_or(false)
        {
            "Type your answer... (Enter to submit, Esc to go back)"
        } else {
            ""
        }
    } else {
        match &approval_mode {
            ApprovalMode::Normal => {
                "Type a message... | Enter: send | Alt/Ctrl+J/Shift+Enter: newline | Ctrl+\u{2190}/\u{2192} word | Home/End line | Ctrl+Home/End doc"
            }
            ApprovalMode::WaitingForApproval { .. } => "",
            ApprovalMode::EnteringRejectReason { .. } => {
                "Enter rejection reason... (Enter to send, Esc to cancel)"
            }
        }
    };

    let input_text = app.input_text();

    // Issue 2 whisper: when the input starts with `/`, show a dimmed one-line
    // usage hint for the best-matching command (preferring a name-prefix match
    // over a looser description match, so `/co` hints `compact` not `resume`)
    // instead of the generic placeholder. When input is empty, keep the current
    // placeholder. When input is `/` alone or `/xyz` with no match, show nothing
    // extra (the slash popup handles "no match").
    let slash_whisper = if !input_text.is_empty() && input_text.starts_with('/') {
        // The prefix is the text after `/` up to the first space.
        let prefix = input_text[1..].split_whitespace().next().unwrap_or("");
        let best = crate::tui::commands::filter_commands(prefix)
            .into_iter()
            // Name-prefix matches win over description-only matches so `/co`
            // hints `compact` (name starts with "co") not `resume` (whose
            // description merely contains "co").
            .min_by_key(|cmd| !cmd.name().starts_with(prefix))
            .map(|cmd| cmd.help_text());
        best.and_then(|help| help.lines().next().map(|line| line.trim().to_string()))
            .filter(|line| !line.is_empty())
    } else {
        None
    };

    // Build one `Line` per buffer row (A1/A4): `app.input_text()` above joins
    // the rows with a literal `\n` for the empty/prefix checks, but ratatui
    // never splits a `Span`/`Line` on an embedded `\n` — feeding that joined
    // string to a single-`Line` `Paragraph` drew every row garbled onto row 0
    // while the cursor math below (already row/col-aware) pointed at wherever
    // the *real* cursor row was. Empty input still renders as one placeholder
    // line; the slash whisper still trails the (always single-row, since `/`
    // commands are one line) typed text.
    let display_lines: Vec<Line> = if input_text.is_empty() {
        vec![Line::from(Span::styled(
            placeholder_text,
            Style::default().fg(Color::DarkGray).bg(theme.input_bg),
        ))]
    } else {
        let rows = app.input().lines().to_vec();
        let last = rows.len().saturating_sub(1);
        rows.into_iter()
            .enumerate()
            .map(|(i, row)| {
                let mut spans = vec![Span::styled(
                    row,
                    Style::default().fg(Color::White).bg(theme.input_bg),
                )];
                if i == last {
                    if let Some(hint) = &slash_whisper {
                        spans.push(Span::styled(
                            format!("  {hint}"),
                            Style::default().fg(Color::DarkGray).bg(theme.input_bg),
                        ));
                    }
                }
                Line::from(spans)
            })
            .collect()
    };

    // The cursor (row, col) and a vertical/horizontal scroll that keep it in
    // view when content overflows the now-dynamic input box. We compute both
    // from `app.input()` once and freeze them before the mutable `&mut App`
    // borrow hands off to the modals below.
    let (cursor_row, _cursor_col) = app.input().cursor();
    let cursor_col = app.input().cursor_display_col();

    // Vertical: keep the cursor's row on a visible line by scrolling it up once
    // it would fall past the last visible row (`height - 1`).
    let vscroll = cursor_row.saturating_sub(area.height.saturating_sub(1) as usize) as u16;
    // Horizontal (cursor-following): advance the left column so the cursor stays
    // no further right than the last visible column (`width - 1`). No `.wrap()`
    // is applied (A4): each row is its own `Line`, already clipped to `area` by
    // the widget, so a long line scrolls horizontally with the cursor instead of
    // wrapping into (and overflowing past) the box.
    let hscroll = cursor_col.saturating_sub(area.width.saturating_sub(1) as usize) as u16;

    let paragraph = Paragraph::new(display_lines)
        .style(Style::default().fg(Color::White).bg(theme.input_bg))
        .scroll((vscroll, hscroll));
    f.render_widget(paragraph, area);

    // Always place the terminal cursor; the scroll math above guarantees it
    // lands inside `area` by construction (cursor row/col are in-view).
    let cursor_x = area.x + (cursor_col - hscroll as usize) as u16;
    let cursor_y = area.y + (cursor_row - vscroll as usize) as u16;
    f.set_cursor_position((cursor_x, cursor_y));

    if matches!(approval_mode, ApprovalMode::Normal) && !app.is_asking() {
        // Capture the input box rect so the slash/mention popup geometry —
        // which anchors above the input — can be reproduced identically on a
        // click (Issue 1). Set before the popups draw so it's available even
        // when a popup is hidden (a click then misses its zero rect).
        app.set_input_area(area);
        modals::draw_slash_autocomplete(f, app, area);
        modals::draw_mention_popup(f, app, area);
    }
}

pub fn draw_input_info(f: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let model_info = app.model_info();

    let model_display = if model_info.id.is_empty() {
        "unknown".to_string()
    } else {
        model_info.display_name.clone()
    };
    // Provider name comes from the resolved catalog entry / `ModelChanged`;
    // show it beside the model when known.
    let provider_display = app.active_provider().to_string();
    // `provider · model` pair, skipping the provider segment + separator when
    // it's unknown so we never leave a dangling `·`.
    let mut pm_spans: Vec<Span> = Vec::new();
    if !provider_display.is_empty() {
        pm_spans.push(Span::styled(
            provider_display,
            Style::default().fg(Color::Magenta),
        ));
        pm_spans.push(Span::raw(" · "));
    }
    pm_spans.push(Span::styled(
        model_display,
        Style::default().fg(Color::Cyan),
    ));

    // The keybinding hints that used to sit here duplicated the input-box
    // placeholder, so this segment now carries only transient status: a pending
    // two-stage quit (ADR-0087), else a short-lived toast (e.g. the drag-copy
    // notice), else a rate-limit throttle indicator that shows *only* while an
    // endpoint is backing off (quiet otherwise).
    let mut spans: Vec<Span> = pm_spans;
    // Context, spend, and the last round's cache share (ADR-0202 §7).
    spans.extend(status_usage::usage_spans(
        app.cost(),
        app.model_info().context_window,
    ));
    if app.quit_pending() {
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(
            "Press Ctrl+C again to quit",
            Style::default().fg(Color::Yellow).bold(),
        ));
    } else if let Some(msg) = app.toast() {
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(
            msg.to_string(),
            Style::default().fg(Color::Green),
        ));
    } else if let Some(status) = app.throttle_status() {
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(
            throttle_label(&status),
            Style::default().fg(Color::Red).bold(),
        ));
    }
    let info_line = Line::from(spans);

    let paragraph = Paragraph::new(info_line)
        .alignment(Alignment::Right)
        .style(Style::default().bg(theme.input_bg));
    f.render_widget(paragraph, area);
}

/// A compact one-line label for a throttled endpoint: `⚠ host throttled · retry
/// Ns · in/cap` for an active 429/`Retry-After` cool-down (in-process or, since
/// #552, a peer's own — `backoff_remaining` already folds in the shared file),
/// `pacing · next Ns` when only the adaptive gate has slowed (#517: the AIMD
/// gate's next-slot countdown, previously computed but never surfaced), or
/// `busy` when just the in-flight cap is full. When a per-model cap (#521) is
/// the binding constraint, the model id rides alongside the host so a
/// saturated `GLM-4.7-Flash` slot doesn't read as a bare, unexplained
/// endpoint-wide cap. `occupancy` gains a `shared X/cap` suffix (#552) only
/// when the cross-process lease count disagrees with this process's own
/// `in_flight` — otherwise it would just repeat the same number — and a
/// `· Nq` suffix while callers are queued behind this endpoint's own permit
/// (#517).
fn throttle_label(status: &entanglement_provider::ThrottleStatus) -> String {
    let host = short_host(&status.endpoint);
    let label = match &status.model {
        Some(model) => format!("{host} ({model})"),
        None => host,
    };
    let mut occupancy = format!("{}/{}", status.in_flight, status.cap);
    if let Some(shared) = status.shared_leases {
        if shared != status.in_flight {
            occupancy = format!("{occupancy} (shared {shared}/{})", status.cap);
        }
    }
    if status.waiters > 0 {
        occupancy = format!("{occupancy} · {}q", status.waiters);
    }
    match status.backoff_remaining {
        // Round the wait up to whole seconds so a sub-second tail never shows "0s".
        Some(remaining) => {
            let secs = remaining.as_millis().div_ceil(1000).max(1);
            format!("⚠ {label} throttled · retry {secs}s · {occupancy}")
        }
        None if status.penalized => match status.next_request_in {
            Some(next) => {
                format!(
                    "⚠ {label} pacing · next {:.1}s · {occupancy}",
                    next.as_secs_f64()
                )
            }
            None => format!("⚠ {label} pacing · {occupancy}"),
        },
        None => format!("⚠ {label} busy · {occupancy}"),
    }
}

/// The bare host of an endpoint URL (`https://api.z.ai/v4` → `api.z.ai`), so the
/// status label stays short.
fn short_host(endpoint: &str) -> String {
    let no_scheme = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .unwrap_or(endpoint);
    no_scheme.split('/').next().unwrap_or(no_scheme).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::App;
    use entanglement_core::SessionId;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn short_host_strips_scheme_and_path() {
        assert_eq!(
            short_host("https://api.z.ai/api/coding/paas/v4"),
            "api.z.ai"
        );
        assert_eq!(short_host("http://localhost:11434/v1"), "localhost:11434");
        assert_eq!(short_host("api.custom"), "api.custom");
    }

    #[test]
    fn throttle_label_reflects_the_backoff_kind() {
        use entanglement_provider::ThrottleStatus;
        use std::time::Duration;
        // An active cool-down window rounds its wait up to whole seconds.
        let parked = ThrottleStatus {
            endpoint: "https://api.z.ai/v4".to_string(),
            in_flight: 2,
            cap: 3,
            backoff_remaining: Some(Duration::from_millis(7200)),
            penalized: true,
            model: None,
            next_request_in: None,
            waiters: 0,
            shared_leases: None,
        };
        assert_eq!(
            throttle_label(&parked),
            "⚠ api.z.ai throttled · retry 8s · 2/3"
        );
        // Penalized pacing with no cool-down window, and no live next-slot
        // countdown (already elapsed) — falls back to the bare label (#517).
        let paced = ThrottleStatus {
            backoff_remaining: None,
            penalized: true,
            next_request_in: None,
            ..parked.clone()
        };
        assert_eq!(throttle_label(&paced), "⚠ api.z.ai pacing · 2/3");
        // Penalized pacing *with* a live next-slot countdown (#517).
        let paced_with_countdown = ThrottleStatus {
            next_request_in: Some(Duration::from_millis(1200)),
            ..paced.clone()
        };
        assert_eq!(
            throttle_label(&paced_with_countdown),
            "⚠ api.z.ai pacing · next 1.2s · 2/3"
        );
        // Only the in-flight cap is full (no 429, no penalty).
        let busy = ThrottleStatus {
            backoff_remaining: None,
            penalized: false,
            in_flight: 3,
            next_request_in: None,
            ..parked.clone()
        };
        assert_eq!(throttle_label(&busy), "⚠ api.z.ai busy · 3/3");
        // A binding per-model cap (#521) rides alongside the host.
        let model_busy = ThrottleStatus {
            backoff_remaining: None,
            penalized: false,
            in_flight: 1,
            cap: 1,
            model: Some("glm-4.7-flash".to_string()),
            next_request_in: None,
            ..parked
        };
        assert_eq!(
            throttle_label(&model_busy),
            "⚠ api.z.ai (glm-4.7-flash) busy · 1/1"
        );
    }

    #[test]
    fn throttle_label_surfaces_shared_leases_and_queued_waiters() {
        use entanglement_provider::ThrottleStatus;
        let base = ThrottleStatus {
            endpoint: "https://api.z.ai/v4".to_string(),
            in_flight: 1,
            cap: 3,
            backoff_remaining: None,
            penalized: false,
            model: None,
            next_request_in: None,
            waiters: 0,
            shared_leases: None,
        };
        // A peer holds leases this process's own in-flight count can't see
        // (#552) — shown as a `(shared X/cap)` suffix.
        let shared_ahead = ThrottleStatus {
            shared_leases: Some(3),
            ..base.clone()
        };
        assert_eq!(
            throttle_label(&shared_ahead),
            "⚠ api.z.ai busy · 1/3 (shared 3/3)"
        );
        // A shared lease count that just repeats `in_flight` is redundant —
        // no suffix.
        let shared_agrees = ThrottleStatus {
            shared_leases: Some(1),
            ..base.clone()
        };
        assert_eq!(throttle_label(&shared_agrees), "⚠ api.z.ai busy · 1/3");
        // Callers queued behind this endpoint's own permit (#517) show as
        // a `· Nq` suffix.
        let queued = ThrottleStatus { waiters: 2, ..base };
        assert_eq!(throttle_label(&queued), "⚠ api.z.ai busy · 1/3 · 2q");
    }

    /// Reads back one rendered row of a `TestBackend` buffer as a plain
    /// string, `width` columns wide — used to prove buffer *content*, not
    /// just cursor position (A1: ratatui never splits a `Span`/`Line` on an
    /// embedded `\n`, so a single-`Line` render of a joined multi-row buffer
    /// drew every row garbled onto row 0 even though the cursor math already
    /// pointed at the right row/col).
    fn row_text(buf: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    /// D2 + cursor-Y fix: with a 3-line input the terminal cursor must land on
    /// the cursor's row (here the last), not be pinned to the top row. Draw the
    /// input box alone into a TestBackend at a known area and read back the
    /// cursor position the backend recorded.
    #[test]
    fn multiline_input_places_cursor_on_cursor_row() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        // "line1\nline2\nline3" — cursor ends on row 2, col 5.
        app.input().insert_str("line1");
        app.input().insert_newline();
        app.input().insert_str("line2");
        app.input().insert_newline();
        app.input().insert_str("line3");
        assert_eq!(app.input().cursor(), (2, "line3".len()));

        // A 1-row-tall area to prove the cursor Y is driven by `cursor_row`
        // (with vscroll) rather than a hardcoded `area.y`.
        let mut terminal = Terminal::new(TestBackend::new(40, 1)).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, 1);
                draw_input(f, area, &mut app);
            })
            .unwrap();

        // The only visible row (area.y=0) shows the scrolled-to line, and the
        // terminal cursor sits on that same row at the cursor's column.
        let pos = terminal.backend().cursor_position();
        assert_eq!(pos.y, 0, "cursor Y should be on the visible (scrolled) row");
        assert_eq!(
            pos.x as usize,
            "line3".len(),
            "cursor X should be at the end of line3"
        );
        // Buffer content: the visible row must render "line3" cleanly, not
        // "line1line2line3" (or any garbled join) crammed onto row 0.
        let buf = terminal.backend().buffer();
        let row = row_text(buf, 0, 40);
        assert!(
            row.starts_with("line3"),
            "visible row should render only line3, got {row:?}"
        );
        assert!(
            !row.contains("line1") && !row.contains("line2"),
            "scrolled-off rows must not bleed into the visible row: {row:?}"
        );
    }

    /// Cursor on the middle row of a tall-enough box renders on that row, not
    /// the first — the core of the bug from complaint #2.
    #[test]
    fn cursor_second_row_renders_on_second_visible_row() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.input().insert_str("aaa");
        app.input().insert_newline();
        app.input().insert_str("bbb");
        // cursor on row 1, col 2 (after the first two 'b's of "bbb")
        app.input().move_cursor_left();
        assert_eq!(app.input().cursor(), (1, 2));

        let mut terminal = Terminal::new(TestBackend::new(40, 2)).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, 2);
                draw_input(f, area, &mut app);
            })
            .unwrap();

        let pos = terminal.backend().cursor_position();
        assert_eq!(pos.y, 1, "cursor on row 1 must render on the second row");
        assert_eq!(pos.x, 2, "cursor X tracks its column");
        // Buffer content: each buffer row must render on its *own* visual
        // row — proof the fix builds one `Line` per row instead of joining
        // both rows with `\n` into a single `Line` drawn entirely on row 0.
        let buf = terminal.backend().buffer();
        assert!(
            row_text(buf, 0, 40).starts_with("aaa"),
            "row 0 should render \"aaa\""
        );
        assert!(
            row_text(buf, 1, 40).starts_with("bbb"),
            "row 1 should render \"bbb\", not be blank or share row 0"
        );
    }

    /// A3: with a buffer taller than the input box, the vertical scroll keeps
    /// following the cursor's row and lands it inside the visible window
    /// (falls out of A1's per-row rendering + the existing cursor-follow
    /// vscroll math — no new scroll state needed).
    #[test]
    fn tall_buffer_scrolls_to_keep_cursor_row_visible() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        // 12 rows ("row0".."row11"), cursor ends on the last (row 11).
        for i in 0..12 {
            if i > 0 {
                app.input().insert_newline();
            }
            app.input().insert_str(&format!("row{i}"));
        }
        assert_eq!(app.input().cursor().0, 11);

        // An 8-row box: rows 4..=11 should be visible (vscroll = 11 - 7 = 4),
        // with the cursor landing on the box's last visual row.
        let mut terminal = Terminal::new(TestBackend::new(10, 8)).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 10, 8);
                draw_input(f, area, &mut app);
            })
            .unwrap();

        let pos = terminal.backend().cursor_position();
        assert_eq!(pos.y, 7, "cursor should land on the box's last visual row");
        let buf = terminal.backend().buffer();
        assert!(
            row_text(buf, 0, 10).starts_with("row4"),
            "top visible row should be row4 (vscroll=4)"
        );
        assert!(
            row_text(buf, 7, 10).starts_with("row11"),
            "bottom visible row should be the cursor's row11"
        );
    }

    /// A4: a line longer than the box's width stays clipped inside it — it
    /// scrolls horizontally following the cursor instead of wrapping onto
    /// (and overflowing past) the next visual row.
    #[test]
    fn long_line_stays_clipped_and_does_not_wrap_to_next_row() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        let long = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"; // 36 chars, cursor at end.
        app.input().insert_str(long);
        assert_eq!(app.input().cursor(), (0, long.len()));

        // A 10-wide, 2-tall box: if `draw_input` wrapped instead of
        // horizontally scrolling (the A4 bug), the line would spill onto row 1.
        let mut terminal = Terminal::new(TestBackend::new(10, 2)).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 10, 2);
                draw_input(f, area, &mut app);
            })
            .unwrap();

        let buf = terminal.backend().buffer();
        let row0 = row_text(buf, 0, 10);
        let row1 = row_text(buf, 1, 10);

        assert!(
            row1.trim().is_empty(),
            "a long single row must not wrap onto the next visual row: {row1:?}"
        );
        let visible = row0.trim_end();
        assert!(
            !visible.is_empty() && visible.len() <= 10,
            "visible slice must be non-empty and bounded by the box width: {row0:?}"
        );
        assert!(
            long.contains(visible),
            "visible text must be a contiguous, unclipped-content slice of the line: {row0:?}"
        );
        assert!(
            visible.ends_with('Z'),
            "cursor-following scroll should show the line's tail (cursor is at the end): {row0:?}"
        );
    }

    /// Issue 2 whisper: typing `/co` renders a dimmed usage hint for the
    /// best-matching command (`compact`) alongside the typed text. Draw into a
    /// TestBackend and read back the buffer to confirm the hint text landed.
    #[test]
    fn slash_input_renders_whisper_for_best_match() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.input().insert_str("/co");

        let mut terminal = Terminal::new(TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 1);
                draw_input(f, area, &mut app);
            })
            .unwrap();

        // The rendered line contains the typed `/co` plus the compact usage
        // hint's first line (which names --keep).
        let buf = terminal.backend().buffer();
        let line: String = (0..80)
            .map(|x| buf[(x, 0)].symbol().chars().next().unwrap())
            .collect();
        assert!(
            line.contains("--keep"),
            "expected --keep whisper in /co render: {line:?}"
        );
    }

    /// Issue 2 whisper: an empty input shows the generic placeholder, not a
    /// slash hint (the whisper only fires for `/…` input).
    #[test]
    fn empty_input_shows_generic_placeholder_not_whisper() {
        let mut app = App::new_for_test(SessionId::new("s1"));

        let mut terminal = Terminal::new(TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 1);
                draw_input(f, area, &mut app);
            })
            .unwrap();

        let buf = terminal.backend().buffer();
        let line: String = (0..80)
            .map(|x| buf[(x, 0)].symbol().chars().next().unwrap())
            .collect();
        assert!(
            line.contains("Type a message"),
            "expected generic placeholder: {line:?}"
        );
    }
}
