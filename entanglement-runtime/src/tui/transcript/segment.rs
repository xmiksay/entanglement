use std::hash::{Hash, Hasher};

use crate::tui::session_view::TranscriptEntry;

/// One renderable unit of the transcript — the granularity the render cache
/// keys on ([`super::cache`]). Consecutive `TextDelta`s coalesce into one
/// [`Block::Text`] and consecutive `ReasoningDelta`s into one
/// [`Block::Reasoning`]; every other entry is its own self-contained block, so a
/// change to one entry invalidates only its block. Content is owned so a block's
/// hash and its rendered lines are independent of the live transcript.
pub(super) enum Block {
    Text {
        run: String,
        /// A committed run gets the left-bar padding; a trailing streaming run
        /// renders bar-less (`false`). Folded into the key so the transition
        /// re-renders the block.
        with_padding: bool,
    },
    Reasoning {
        run: String,
        /// Transcript index of the run's first `ReasoningDelta` — its stable
        /// clickable id and cache provenance.
        block_id: usize,
        expanded: bool,
    },
    User {
        text: String,
        pending: bool,
    },
    ToolCall {
        tool: String,
        input: String,
        output: Option<String>,
        block_id: usize,
        expanded: bool,
    },
    ToolOutput {
        tool: Option<String>,
        output: String,
    },
    Error {
        message: String,
    },
    Done,
}

impl Block {
    /// The clickable-block provenance stamped on every line this block renders
    /// (`None` for lines outside a collapsible block).
    pub(super) fn provenance(&self) -> Option<usize> {
        match self {
            Block::Reasoning { block_id, .. } | Block::ToolCall { block_id, .. } => Some(*block_id),
            _ => None,
        }
    }

    /// Content hash keyed on `(kind, content, expanded/padding flags)`. A cache
    /// slot with a matching key renders identically, so its lines are reused
    /// without re-parsing markdown. A discriminant byte keeps distinct kinds
    /// from colliding.
    pub(super) fn key(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        match self {
            Block::Text { run, with_padding } => {
                0u8.hash(&mut h);
                run.hash(&mut h);
                with_padding.hash(&mut h);
            }
            Block::Reasoning { run, expanded, .. } => {
                1u8.hash(&mut h);
                run.hash(&mut h);
                expanded.hash(&mut h);
            }
            Block::User { text, pending } => {
                2u8.hash(&mut h);
                text.hash(&mut h);
                pending.hash(&mut h);
            }
            Block::ToolCall {
                tool,
                input,
                output,
                expanded,
                ..
            } => {
                3u8.hash(&mut h);
                tool.hash(&mut h);
                input.hash(&mut h);
                output.hash(&mut h);
                expanded.hash(&mut h);
            }
            Block::ToolOutput { tool, output } => {
                4u8.hash(&mut h);
                tool.hash(&mut h);
                output.hash(&mut h);
            }
            Block::Error { message } => {
                5u8.hash(&mut h);
                message.hash(&mut h);
            }
            Block::Done => {
                6u8.hash(&mut h);
            }
        }
        h.finish()
    }
}

/// Segments the transcript into cache-aligned [`Block`]s, coalescing streamed
/// deltas: consecutive `TextDelta`s become one text block and consecutive
/// `ReasoningDelta`s one reasoning block, a text↔reasoning switch flushes the
/// accumulator in arrival order, and the trailing text run renders bar-less
/// (still streaming). `expanded` reports a block's collapse state by its stable
/// id.
pub(super) fn segment(
    transcript: &[TranscriptEntry],
    expanded: impl Fn(usize) -> bool,
) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut pending_text = String::new();
    let mut pending_reasoning = String::new();
    let mut reasoning_start: Option<usize> = None;

    for (idx, entry) in transcript.iter().enumerate() {
        if let TranscriptEntry::TextDelta { text } = entry {
            if !pending_reasoning.is_empty() {
                let block_id = reasoning_start.take().unwrap_or(idx);
                blocks.push(Block::Reasoning {
                    run: std::mem::take(&mut pending_reasoning),
                    block_id,
                    expanded: expanded(block_id),
                });
            }
            pending_text.push_str(text);
            continue;
        }
        if let TranscriptEntry::ReasoningDelta { text } = entry {
            if !pending_text.is_empty() {
                blocks.push(Block::Text {
                    run: std::mem::take(&mut pending_text),
                    with_padding: true,
                });
            }
            if pending_reasoning.is_empty() {
                reasoning_start = Some(idx);
            }
            pending_reasoning.push_str(text);
            continue;
        }
        // Non-delta boundary: at most one accumulator is non-empty.
        if !pending_text.is_empty() {
            blocks.push(Block::Text {
                run: std::mem::take(&mut pending_text),
                with_padding: true,
            });
        }
        if !pending_reasoning.is_empty() {
            let block_id = reasoning_start.take().unwrap_or(idx);
            blocks.push(Block::Reasoning {
                run: std::mem::take(&mut pending_reasoning),
                block_id,
                expanded: expanded(block_id),
            });
        }

        match entry {
            TranscriptEntry::TextDelta { .. } | TranscriptEntry::ReasoningDelta { .. } => {
                unreachable!()
            }
            TranscriptEntry::User { text, pending } => blocks.push(Block::User {
                text: text.clone(),
                pending: *pending,
            }),
            TranscriptEntry::ToolCall {
                tool,
                input,
                output,
                ..
            } => blocks.push(Block::ToolCall {
                tool: tool.clone(),
                input: input.clone(),
                output: output.clone(),
                block_id: idx,
                expanded: expanded(idx),
            }),
            TranscriptEntry::ToolOutput { tool, output } => blocks.push(Block::ToolOutput {
                tool: tool.clone(),
                output: output.clone(),
            }),
            TranscriptEntry::Error { message } => blocks.push(Block::Error {
                message: message.clone(),
            }),
            TranscriptEntry::Done => blocks.push(Block::Done),
        }
    }

    // End of stream: at most one accumulator survives. The trailing text run is
    // still being streamed, so it renders bar-less.
    if !pending_text.is_empty() {
        blocks.push(Block::Text {
            run: pending_text,
            with_padding: false,
        });
    } else if !pending_reasoning.is_empty() {
        let block_id = reasoning_start.unwrap_or(transcript.len());
        blocks.push(Block::Reasoning {
            run: pending_reasoning,
            block_id,
            expanded: expanded(block_id),
        });
    }

    blocks
}
