//! Full-body pager for a parked tool-call approval (#B2): `v` in
//! `ApprovalMode::WaitingForApproval` opens a scrollable modal showing the
//! same header + per-tool rendering the transcript's parked tail shows
//! (`transcript::render_approval_tool_body`), so a diff/plan body that
//! scrolled off the transcript viewport is still fully readable without
//! leaving the approval parked. `y`/`s`/`a`/`d`/`n`/`e` keep resolving the
//! approval while it's open — `event_loop`'s `WaitingForApproval` arm only
//! gains pager-aware navigation (`j`/`k`/`PageUp`/`PageDown`/`Esc`), the
//! decision keys are untouched.

use super::App;

/// Pager visibility + scroll offset. Always reset on close so a reopen starts
/// at the top rather than resuming a stale scroll position.
#[derive(Default)]
pub struct ApprovalPagerState {
    visible: bool,
    scroll: u16,
}

impl App {
    pub fn showing_approval_pager(&self) -> bool {
        self.approval_pager.visible
    }

    pub fn approval_pager_scroll(&self) -> u16 {
        self.approval_pager.scroll
    }

    pub fn open_approval_pager(&mut self) {
        self.approval_pager.visible = true;
        self.approval_pager.scroll = 0;
        self.mark_dirty();
    }

    /// Closes the pager back to the parked approval prompt. Also called from
    /// `advance_approval`/`clear_approval` so a resolved or dropped approval
    /// never leaves the pager showing stale content over whatever comes next.
    pub fn close_approval_pager(&mut self) {
        if self.approval_pager.visible {
            self.approval_pager.visible = false;
            self.approval_pager.scroll = 0;
            self.mark_dirty();
        }
    }

    pub fn approval_pager_scroll_up(&mut self, n: u16) {
        self.approval_pager.scroll = self.approval_pager.scroll.saturating_sub(n);
        self.mark_dirty();
    }

    pub fn approval_pager_scroll_down(&mut self, n: u16) {
        self.approval_pager.scroll = self.approval_pager.scroll.saturating_add(n);
        self.mark_dirty();
    }
}
