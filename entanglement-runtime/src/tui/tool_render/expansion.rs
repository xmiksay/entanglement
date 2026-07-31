//! Per-tool approval/expansion bodies that show the **full**, untruncated
//! decision-relevant detail — the location, diff, or command — rather than
//! relying on the one-line collapsed header, which truncates its primary arg
//! to fit the terminal width (#519). Split out of `tool_render.rs` to keep
//! that file under the 400-line cap; `render_expansion`'s dispatcher (the
//! parent module) calls into these per-tool bodies, and shares its low-level
//! `render_*_output` helpers + `collect_line` back down via `pub(super)`.

use std::path::Path;

use entanglement_runtime::permission;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span, Text},
};

use crate::tui::diff::DiffRenderer;
use crate::tui::theme::Theme;
use crate::tui::wrap;

use super::collect_line;

/// A full, untruncated `  {label}: {value}` line for a path/command tool's
/// body (#519) — reuses [`permission::permission_arg`], the same value the
/// collapsed header truncates to fit `available_width` and an
/// argument-scoped permission rule matches against, so the body can never
/// show something other than what was actually graded. `None` when the input
/// carries nothing for this tool (malformed JSON, missing field).
pub(super) fn location_line(tool: &str, input: &str, label: &str) -> Option<Line<'static>> {
    permission::permission_arg(tool, input).map(|arg| Line::from(format!("  {label}: {arg}")))
}

/// A `read` body: the full path (+ `offset`/`limit` when set) so the target is
/// never left to the truncatable header alone, followed by the file contents
/// once the call has run (#519).
pub(super) fn render_read_expansion(
    input: &str,
    output: &str,
    theme: Theme,
    available_width: u16,
) -> Text<'static> {
    let v: serde_json::Value = serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
    let mut lines = Vec::new();
    lines.extend(location_line("read", input, "path"));
    if let Some(offset) = v.get("offset").and_then(|o| o.as_u64()) {
        lines.push(Line::from(format!("  offset: {offset}")));
    }
    if let Some(limit) = v.get("limit").and_then(|o| o.as_u64()) {
        lines.push(Line::from(format!("  limit: {limit}")));
    }
    lines.extend(super::render_read_output(output, theme, available_width).lines);
    Text::from(lines)
}

/// A `write` body: the full path plus the proposed content. Used for both the
/// approval preview and the post-execution expansion; the approval tail
/// (`transcript.rs`) additionally diffs against the on-disk file via
/// [`render_write_approval_body`] — a diff this function can't do safely
/// once the call has actually run (the disk already equals `content` by
/// then).
pub(super) fn render_write_expansion(input: &str) -> Text<'static> {
    let v: serde_json::Value = serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
    let content = v.get("content").and_then(|s| s.as_str()).unwrap_or("");
    let mut lines = Vec::new();
    lines.extend(location_line("write", input, "path"));
    lines.extend(
        content
            .lines()
            .map(|line| Line::from(format!("  {line}")))
            .collect::<Vec<_>>(),
    );
    Text::from(lines)
}

/// The `write` **approval** body (#519): unlike [`render_write_expansion`],
/// this reads the file currently on disk at render time (head-side, no
/// protocol change) and diffs it against the proposed `content` via
/// [`DiffRenderer`], so an overwrite's destroyed content is visible before
/// approval instead of only showing the new content blind. Falls back to
/// "(new file)" + the full content when the target doesn't exist (or isn't
/// readable UTF-8) yet.
pub fn render_write_approval_body(input: &str, root: &Path) -> Text<'static> {
    let v: serde_json::Value = serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
    let path = v.get("path").and_then(|s| s.as_str()).unwrap_or("");
    let content = v.get("content").and_then(|s| s.as_str()).unwrap_or("");
    let mut lines = vec![Line::from(format!("  path: {path}"))];
    match std::fs::read_to_string(root.join(path)) {
        Ok(old) => lines.extend(DiffRenderer::render_change(&old, content).lines),
        Err(_) => {
            lines.push(Line::from(vec![Span::styled(
                "  (new file)",
                Style::default().fg(Color::Green),
            )]));
            lines.extend(content.lines().map(|line| Line::from(format!("  {line}"))));
        }
    }
    Text::from(lines)
}

/// An `apply_patch` body: the full path plus the patch rendered as a real
/// `+`/`-` diff via [`DiffRenderer::render_unified`] instead of the escaped
/// raw-JSON fallback every other unknown-shaped tool gets (#519). The patch
/// text itself is the diff — no disk read needed, so this renders identically
/// for the approval preview and the post-execution expansion. Falls back to
/// the raw patch text (still never empty) if it fails to parse as a unified
/// diff.
pub(super) fn render_apply_patch_expansion(input: &str) -> Text<'static> {
    let v: serde_json::Value = serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
    let patch = v.get("patch").and_then(|s| s.as_str()).unwrap_or("");
    let mut lines = Vec::new();
    lines.extend(location_line("apply_patch", input, "path"));
    let diff = DiffRenderer::render_unified(patch);
    if diff.lines.is_empty() {
        lines.extend(patch.lines().map(|line| Line::from(format!("  {line}"))));
    } else {
        lines.extend(diff.lines);
    }
    Text::from(lines)
}

/// A `bash`/`call` body: the full command (multiline-safe, word-wrapped —
/// never left to the truncated one-line header, #519) plus `workdir` when set
/// (permission-relevant since #425), followed by the command output once the
/// call has run.
pub(super) fn render_command_expansion(
    tool: &str,
    input: &str,
    output: &str,
    theme: Theme,
    available_width: u16,
) -> Text<'static> {
    let mut lines = Vec::new();
    if let Some(command) = permission::permission_arg(tool, input) {
        lines.extend(render_wrapped_labeled("command", &command, available_width));
    }
    if let Some(workdir) = permission::permission_workdir(tool, input) {
        lines.push(Line::from(format!("  workdir: {workdir}")));
    }
    lines.extend(super::render_plain_output(output, theme, available_width).lines);
    Text::from(lines)
}

/// A `glob` body: the full pattern (+ `exclude` list when set) as labeled
/// lines instead of an empty/raw-JSON body (#519).
pub(super) fn render_glob_expansion(
    input: &str,
    output: &str,
    theme: Theme,
    available_width: u16,
) -> Text<'static> {
    let v: serde_json::Value = serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
    let mut lines = Vec::new();
    lines.extend(location_line("glob", input, "pattern"));
    if let Some(exclude) = non_empty_str_array(&v, "exclude") {
        lines.push(Line::from(format!("  exclude: {}", exclude.join(", "))));
    }
    lines.extend(super::render_glob_output(output, theme, available_width).lines);
    Text::from(lines)
}

/// A `grep` body: the regex `pattern`, the optional file-filter `path`, and
/// any `exclude` list as labeled lines (#519) — `permission_arg` alone only
/// ever surfaces one of `pattern`/`path`, but a reviewer needs both.
pub(super) fn render_grep_expansion(
    input: &str,
    output: &str,
    theme: Theme,
    available_width: u16,
) -> Text<'static> {
    let v: serde_json::Value = serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
    let mut lines = Vec::new();
    if let Some(pattern) = v.get("pattern").and_then(|s| s.as_str()) {
        lines.push(Line::from(format!("  pattern: {pattern}")));
    }
    if let Some(path) = v.get("path").and_then(|s| s.as_str()) {
        lines.push(Line::from(format!("  path: {path}")));
    }
    if let Some(exclude) = non_empty_str_array(&v, "exclude") {
        lines.push(Line::from(format!("  exclude: {}", exclude.join(", "))));
    }
    lines.extend(super::render_grep_output(output, theme, available_width).lines);
    Text::from(lines)
}

/// A non-empty `Vec<String>` from a JSON array field, or `None` for an
/// absent/empty/non-string-array field — shared by the `glob`/`grep` bodies'
/// `exclude` line.
fn non_empty_str_array(value: &serde_json::Value, field: &str) -> Option<Vec<String>> {
    let items: Vec<String> = value
        .get(field)?
        .as_array()?
        .iter()
        .filter_map(|e| e.as_str())
        .map(String::from)
        .collect();
    (!items.is_empty()).then_some(items)
}

/// A `  {label}: {value}` body line, word-wrapped at `available_width - 4` and
/// indented so a long or multiline value (a shell command, a workdir) always
/// renders in full regardless of terminal width (#519) — unlike the
/// collapsed header, which truncates the same value to fit one line.
/// Continuation lines (wrapped or from an embedded newline) align under a
/// plain 4-space indent rather than the label's width, matching
/// `render_prompt_body`'s indentation.
fn render_wrapped_labeled(label: &str, value: &str, available_width: u16) -> Vec<Line<'static>> {
    let wrap_width = available_width.saturating_sub(4).max(1);
    let mut lines = Vec::new();
    let mut first = true;
    for raw in value.lines() {
        let wrapped = wrap::wrap_line(Line::from(raw.to_string()), wrap_width);
        if wrapped.is_empty() {
            lines.push(Line::from(if first {
                format!("  {label}:")
            } else {
                String::new()
            }));
            first = false;
            continue;
        }
        for wline in wrapped {
            let prefix = if first {
                format!("  {label}: ")
            } else {
                "    ".to_string()
            };
            first = false;
            lines.push(Line::from(format!("{prefix}{}", collect_line(&wline))));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(format!("  {label}:")));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::markdown::MarkdownRenderer;

    fn flatten(text: &Text<'_>) -> String {
        text.lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect()
    }

    // #519: approval prompts must show the full location/diff/command, never
    // an empty or truncated body. These exercise the shared `render_expansion`
    // dispatcher (the public entry point both the approval tail and the
    // expanded transcript block use) so the per-tool wiring is covered
    // end-to-end, not just this module's helpers in isolation.

    #[test]
    fn test_expansion_read_shows_full_path_and_offset_limit() {
        let result = super::super::render_expansion(
            Some("read"),
            r#"{"path":"src/very/long/nested/module/path/main.rs","offset":10,"limit":50}"#,
            "",
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(
            text.contains("src/very/long/nested/module/path/main.rs"),
            "read approval body must show the full path: {text:?}"
        );
        assert!(text.contains("offset: 10"), "{text:?}");
        assert!(text.contains("limit: 50"), "{text:?}");
    }

    #[test]
    fn test_expansion_edit_shows_full_path() {
        let result = super::super::render_expansion(
            Some("edit"),
            r#"{"path":"src/main.rs","oldString":"a","newString":"b"}"#,
            "",
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(
            text.contains("path: src/main.rs"),
            "edit approval body must show the full path: {text:?}"
        );
    }

    #[test]
    fn test_expansion_grep_shows_pattern_and_path_structured() {
        let result = super::super::render_expansion(
            Some("grep"),
            r#"{"pattern":"TODO.*fixme","path":"src/*.rs"}"#,
            "",
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(text.contains("pattern: TODO.*fixme"), "{text:?}");
        assert!(text.contains("path: src/*.rs"), "{text:?}");
        assert!(!text.contains('{'), "must not dump raw JSON: {text:?}");
    }

    #[test]
    fn test_expansion_glob_shows_pattern_and_exclude() {
        let result = super::super::render_expansion(
            Some("glob"),
            r#"{"pattern":"**/*.rs","exclude":["target/*","*.bak"]}"#,
            "",
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(text.contains("pattern: **/*.rs"), "{text:?}");
        assert!(text.contains("target/*"), "{text:?}");
        assert!(text.contains("*.bak"), "{text:?}");
    }

    #[test]
    fn test_expansion_bash_shows_full_command_never_empty() {
        // An empty approval body degrades consent to "trust the truncated
        // header"; the full command must survive even when no output exists yet.
        let result = super::super::render_expansion(
            Some("bash"),
            r#"{"command":"git status"}"#,
            "",
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        assert!(
            !result.lines.is_empty(),
            "bash approval body must not be empty"
        );
        let text = flatten(&result);
        assert!(text.contains("git status"), "{text:?}");
    }

    #[test]
    fn test_expansion_bash_wraps_a_long_command_in_full() {
        let long_command = format!("echo {}", "x".repeat(200));
        let result = super::super::render_expansion(
            Some("bash"),
            &serde_json::json!({ "command": long_command }).to_string(),
            "",
            Theme::default(),
            60,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        // Every character of the command must survive somewhere across the
        // wrapped lines — independent of the 60-column terminal width, unlike
        // the truncated collapsed header. The wrap itself wraps mid-token, so
        // rather than expect one contiguous 200-`x` run, count survivors.
        assert_eq!(
            text.chars().filter(|c| *c == 'x').count(),
            200,
            "long command must render every character, not truncated: {text:?}"
        );
        assert!(
            result.lines.len() > 1,
            "a long command must wrap across multiple lines, got {} lines",
            result.lines.len()
        );
    }

    #[test]
    fn test_expansion_bash_multiline_command_shows_every_line() {
        let result = super::super::render_expansion(
            Some("bash"),
            r#"{"command":"echo one\necho two\necho three"}"#,
            "",
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(text.contains("echo one"), "{text:?}");
        assert!(text.contains("echo two"), "{text:?}");
        assert!(text.contains("echo three"), "{text:?}");
    }

    #[test]
    fn test_expansion_call_shows_workdir() {
        let result = super::super::render_expansion(
            Some("call"),
            r#"{"command":"git","args":["status","-s"],"workdir":"sub/dir"}"#,
            "",
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(text.contains("git status -s"), "{text:?}");
        assert!(text.contains("workdir: sub/dir"), "{text:?}");
    }

    #[test]
    fn test_expansion_apply_patch_renders_diff_not_json() {
        let patch = "@@ -1,2 +1,2 @@\n-one\n+ONE\n two\n";
        let result = super::super::render_expansion(
            Some("apply_patch"),
            &serde_json::json!({ "path": "a.txt", "patch": patch }).to_string(),
            "",
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(text.contains("path: a.txt"), "{text:?}");
        let has_delete = result
            .lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content == "- "));
        let has_insert = result
            .lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content == "+ "));
        assert!(
            has_delete && has_insert,
            "apply_patch approval body must render a `-`/`+` diff pair: {text:?}"
        );
        assert!(
            !text.contains("\"patch\""),
            "apply_patch approval body must not dump raw JSON: {text:?}"
        );
    }

    #[test]
    fn test_expansion_apply_patch_falls_back_to_raw_on_unparseable_patch() {
        let result = super::super::render_expansion(
            Some("apply_patch"),
            &serde_json::json!({ "path": "a.txt", "patch": "not a real patch" }).to_string(),
            "",
            Theme::default(),
            80,
            &MarkdownRenderer::new(),
        );
        let text = flatten(&result);
        assert!(
            !result.lines.is_empty(),
            "apply_patch approval body must never be empty"
        );
        assert!(text.contains("not a real patch"), "{text:?}");
    }

    #[test]
    fn test_write_approval_body_diffs_against_disk_content() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("a.txt"), "old line\n").expect("seed file");
        let input = serde_json::json!({ "path": "a.txt", "content": "new line\n" }).to_string();

        let result = render_write_approval_body(&input, dir.path());
        let text = flatten(&result);
        assert!(text.contains("path: a.txt"), "{text:?}");
        let has_delete = result
            .lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content == "- "));
        let has_insert = result
            .lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content == "+ "));
        assert!(
            has_delete && has_insert,
            "an overwrite must show a `-`/`+` diff against the on-disk content: {text:?}"
        );
        assert!(text.contains("old line"), "{text:?}");
        assert!(text.contains("new line"), "{text:?}");
    }

    #[test]
    fn test_write_approval_body_new_file_shows_full_content() {
        let dir = tempfile::tempdir().expect("temp dir");
        let input =
            serde_json::json!({ "path": "brand-new.txt", "content": "hello\nworld" }).to_string();

        let result = render_write_approval_body(&input, dir.path());
        let text = flatten(&result);
        assert!(text.contains("new file"), "{text:?}");
        assert!(text.contains("hello"), "{text:?}");
        assert!(text.contains("world"), "{text:?}");
    }
}
