//! What a tool call returns: a tool's own [`ToolRun`] and the registry's
//! [`ToolExecution`] (split out of `tools.rs` at the file cap).

use entanglement_core::ContentPart;

/// A tool's successful result plus its structured metadata (#681, ADR-0186):
/// the numeric exit status when the call ran a process to completion
/// (`bash` foreground), `None` for every other tool — and for a
/// signal-killed or timed-out process, where no code exists (the `[exit -1]`
/// text keeps its lossy sentinel; the field stays honest).
///
/// Build it with [`ToolRun::new`] plus struct-update syntax
/// (`ToolRun { exit_code, ..ToolRun::new(content) }`) so a later metadata
/// field doesn't break every implementor again, as `is_error` did (#727).
#[derive(Debug, Default)]
pub struct ToolRun {
    pub content: Vec<ContentPart>,
    pub exit_code: Option<i32>,
    /// The tool ran but reports its own failure in-band — an MCP server's
    /// `isError: true` (#727) — so `content` is the failure text, not an
    /// answer. `false` for every built-in tool, which fails via `Err` instead.
    pub is_error: bool,
}

impl ToolRun {
    /// A plain successful result: `content`, no exit code, not an error.
    pub fn new(content: Vec<ContentPart>) -> Self {
        Self {
            content,
            ..Self::default()
        }
    }
}

/// One text part for a non-empty string, none for an empty one — the same fold
/// [`entanglement_core::Message::tool`] applies, so a text-only result keeps its
/// exact prior shape.
pub(crate) fn text_parts(text: String) -> Vec<ContentPart> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![ContentPart::text(text)]
    }
}

/// Outcome of [`ToolRegistry::execute`][super::ToolRegistry::execute] (#636, ADR-0176): `content` plus
/// whether the tool failed to run at all — an `Err` from [`Tool::run_for_session`][super::Tool::run_for_session]
/// or an unknown name — as a real `bool` instead of only a `` tool `X` failed: … ``
/// marker buried inside `content`'s text. This is the boundary where the
/// structured side channel originates; `is_error` rides from here through
/// [`crate::tool_runner::run_and_reply`]'s [`crate::seam::reply_content`] call up
/// to [`entanglement_core::OutEvent::ToolOutput`].
pub struct ToolExecution {
    pub content: Vec<ContentPart>,
    pub is_error: bool,
    /// The process exit status for a process-running tool (#681, ADR-0186);
    /// `None` for every other tool, an errored dispatch, or a killed process.
    pub exit_code: Option<i32>,
}

impl ToolExecution {
    pub(super) fn ok(run: ToolRun) -> Self {
        Self {
            content: run.content,
            is_error: run.is_error,
            exit_code: run.exit_code,
        }
    }

    pub(super) fn err(content: Vec<ContentPart>) -> Self {
        Self {
            content,
            is_error: true,
            exit_code: None,
        }
    }
}
