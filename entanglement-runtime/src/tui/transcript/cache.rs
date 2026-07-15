use std::hash::{Hash, Hasher};

use ratatui::text::Line;

use crate::tui::markdown::MarkdownRenderer;
use crate::tui::theme::{RoleColors, Theme};

use super::block::render_block;
use super::segment::segment;
use crate::tui::session_view::TranscriptEntry;

/// One memoized block: its content hash, its clickable-block provenance, and the
/// owned lines to splice back in when the hash still matches.
struct CachedBlock {
    key: u64,
    block_id: Option<usize>,
    lines: Vec<Line<'static>>,
}

/// Positionally-aligned render memo for a session's transcript body (#342).
///
/// A redraw fires on every keystroke, scroll, mouse move, and streaming delta,
/// but almost none of them change the *content* of more than one block. This
/// caches each block's rendered lines keyed by a content hash, so an idle redraw
/// re-parses zero markdown and only clones owned lines. A `width`/`theme_fp`
/// mismatch (resize or theme swap) drops the whole memo and rebuilds once.
pub(crate) struct RenderCache {
    width: u16,
    theme_fp: u64,
    blocks: Vec<CachedBlock>,
    /// Blocks re-rendered on the last [`Self::render`] pass — a test hook to
    /// assert incrementality (`0` on an unchanged redraw).
    last_rebuilt: usize,
}

impl RenderCache {
    pub(crate) fn new() -> Self {
        Self {
            width: 0,
            theme_fp: 0,
            blocks: Vec::new(),
            last_rebuilt: 0,
        }
    }

    /// Blocks re-rendered on the last pass (0 when everything was reused). A
    /// test-only incrementality hook; the field it reads is always maintained so
    /// the getter stays a trivial read.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn last_rebuilt(&self) -> usize {
        self.last_rebuilt
    }

    /// Renders the transcript body, reusing every block whose content hash is
    /// unchanged. Returns the assembled lines plus per-line block provenance
    /// (for click hit-testing). `expanded` reports a block's collapse state.
    pub(crate) fn render(
        &mut self,
        transcript: &[TranscriptEntry],
        expanded: impl Fn(usize) -> bool,
        md: &MarkdownRenderer,
        theme: Theme,
        user: RoleColors,
        width: u16,
    ) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
        let fp = theme_fingerprint(theme, user);
        if self.width != width || self.theme_fp != fp {
            self.blocks.clear();
            self.width = width;
            self.theme_fp = fp;
        }

        let blocks = segment(transcript, expanded);
        let mut rebuilt = 0;
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut line_blocks: Vec<Option<usize>> = Vec::new();

        for (i, blk) in blocks.iter().enumerate() {
            let key = blk.key();
            let reuse = self.blocks.get(i).is_some_and(|c| c.key == key);
            if !reuse {
                let rendered = render_block(blk, md, theme, user, width);
                let cached = CachedBlock {
                    key,
                    block_id: blk.provenance(),
                    lines: rendered,
                };
                if i < self.blocks.len() {
                    self.blocks[i] = cached;
                } else {
                    self.blocks.push(cached);
                }
                rebuilt += 1;
            }
            let cached = &self.blocks[i];
            for line in &cached.lines {
                lines.push(line.clone());
                line_blocks.push(cached.block_id);
            }
        }

        // Drop stale trailing slots (transcript shrank / block coalesced away).
        self.blocks.truncate(blocks.len());
        self.last_rebuilt = rebuilt;
        (lines, line_blocks)
    }
}

impl Default for RenderCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Fingerprints the theme colors + agent role color that every block bakes into
/// its spans. A change here means cached lines carry stale colors, so the whole
/// memo must rebuild.
fn theme_fingerprint(theme: Theme, user: RoleColors) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    theme.bar_glyph.hash(&mut h);
    theme.assistant_fg.hash(&mut h);
    theme.tool_req_fg.hash(&mut h);
    theme.tool_out_fg.hash(&mut h);
    theme.error_fg.hash(&mut h);
    theme.message_bg.hash(&mut h);
    user.fg.hash(&mut h);
    user.bg.hash(&mut h);
    h.finish()
}
