//! `glob`/`grep` output rendering (#520). Split out of `tool_render.rs` to
//! keep that file under the 400-line cap. Both the standalone `ToolOutput`
//! block (`render_tool_output`) and the structured expansion bodies
//! (`expansion::render_glob_expansion`/`render_grep_expansion`) share these —
//! fixing the summary here fixes both call sites.

use ratatui::{
    style::{Color, Style},
    text::{Line, Span, Text},
};

use crate::tui::theme::Theme;

/// `host::glob::GlobTool::run` returns either a newline-delimited file list
/// (each line a matched path, possibly followed by a `... [truncated: …]`
/// notice appended by `truncate_output`) or, when nothing matched, a single
/// free-text hint starting with this prefix (the "matched N directories but
/// no files" / "N entries skipped due to read errors" messages) — never both.
const GLOB_HINT_PREFIX: &str = "pattern `";

pub(super) fn render_glob_output(
    output: &str,
    _theme: Theme,
    _available_width: u16,
) -> Text<'static> {
    if output.trim().is_empty() {
        return Text::from(vec![Line::from(vec![
            Span::styled("✗ ", Style::default().fg(Color::Red)),
            Span::raw("No matches found"),
        ])]);
    }

    // The "no files" hints are a single prose message, not a file list —
    // render the tool's own wording verbatim instead of re-deriving a count
    // by scraping digits out of it (#520).
    if output.starts_with(GLOB_HINT_PREFIX) {
        return Text::from(vec![Line::from(vec![
            Span::styled("⚠ ", Style::default().fg(Color::Yellow)),
            Span::styled(output.to_string(), Style::default().fg(Color::Yellow)),
        ])]);
    }

    let mut file_lines = Vec::new();
    let mut notice_lines = Vec::new();
    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with("... [truncated:") {
            notice_lines.push(line);
        } else {
            file_lines.push(line);
        }
    }

    let header = Line::from(vec![
        Span::styled("✓ ", Style::default().fg(Color::Green)),
        Span::styled(
            format!("{} files matched", file_lines.len()),
            Style::default().fg(Color::Cyan),
        ),
    ]);

    let mut result = vec![header];
    result.extend(file_lines.into_iter().map(|l| Line::from(format!("  {l}"))));
    // Surfaced distinctly (#520): the host cap firing must stay visible, not
    // fold silently into the file list or vanish.
    result.extend(notice_lines.into_iter().map(|l| {
        Line::from(vec![
            Span::styled("⚠ ", Style::default().fg(Color::Yellow)),
            Span::styled(l.to_string(), Style::default().fg(Color::Yellow)),
        ])
    }));
    Text::from(result)
}

/// Parse a `host::grep::GrepTool` match line (`path:lineno:content`). `None`
/// for anything else — a skip/cap notice (`host/grep.rs`'s `append_skip_notice`,
/// ADR-0091) or the `truncate_output` cap notice — so those render verbatim
/// via the fallback arm in [`render_grep_output`] instead of being dropped or
/// misparsed as a match (#520).
fn parse_grep_match(line: &str) -> Option<(&str, &str, &str)> {
    let (file, rest) = line.split_once(':')?;
    let (lineno, content) = rest.split_once(':')?;
    lineno.parse::<u64>().ok()?;
    Some((file, lineno, content))
}

pub(super) fn render_grep_output(
    output: &str,
    _theme: Theme,
    _available_width: u16,
) -> Text<'static> {
    let mut lines = Vec::new();
    let mut match_count = 0;

    for line in output.lines() {
        match parse_grep_match(line) {
            Some((file, lineno, content)) => {
                match_count += 1;
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {file}:{lineno}:"),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::raw(content.to_string()),
                ]));
            }
            None => {
                if line.trim().is_empty() {
                    continue;
                }
                // Skip/cap notice or truncation marker — always visible,
                // never silently dropped (#520).
                lines.push(Line::from(Span::styled(
                    format!("  {line}"),
                    Style::default().fg(Color::Yellow),
                )));
            }
        }
    }

    let msg = format!("{} matches found", match_count);
    let header = Line::from(vec![
        Span::styled("✓ ", Style::default().fg(Color::Green)),
        Span::styled(msg, Style::default().fg(Color::Cyan)),
    ]);

    let mut result = vec![header];
    result.extend(lines);
    Text::from(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flatten(text: &Text<'_>) -> String {
        text.lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn test_glob_with_hyphenated_paths() {
        let output = "docs/adr/0001-actor-model-abi.md\ndocs/adr/0002-protocol.md\nsrc/main.rs\n";
        let result = render_glob_output(output, Theme::default(), 80);
        let text = flatten(&result);
        assert!(
            text.contains("0001-actor-model-abi.md"),
            "Should render hyphenated paths, got: {text}"
        );
        assert!(
            text.contains("0002-protocol.md"),
            "Should render hyphenated paths, got: {text}"
        );
        assert!(
            text.contains("3 files matched"),
            "Should show file count, got: {text}"
        );
    }

    #[test]
    fn test_glob_with_directories_only_renders_hint_verbatim() {
        // The tool's own wording is rendered as-is — no re-deriving a count by
        // scraping digits out of the prose (#520).
        let output = "pattern `**` matched 3 directories but no files (files are filtered out). Try `**/*` to list files inside those directories.";
        let result = render_glob_output(output, Theme::default(), 80);
        let text = flatten(&result);
        assert!(
            text.contains(output),
            "Should render the tool's hint verbatim, got: {text}"
        );
    }

    #[test]
    fn test_glob_skipped_errors_hint_renders_verbatim_not_as_a_file() {
        let output =
            "pattern `broken/*` matched no files; 2 entries were skipped due to read errors \
             (see engine logs with `RUST_LOG=entanglement_core::host=warn`).";
        let result = render_glob_output(output, Theme::default(), 80);
        let text = flatten(&result);
        assert!(
            text.contains("skipped due to read errors"),
            "should render the hint verbatim: {text}"
        );
        assert!(
            !text.contains("1 files matched"),
            "must not misparse the hint as a single matched file: {text}"
        );
    }

    #[test]
    fn test_glob_output_truncation_marker_is_visible() {
        let output = "a.rs\nb.rs\n... [truncated: 99999 bytes total]";
        let result = render_glob_output(output, Theme::default(), 80);
        let text = flatten(&result);
        assert!(text.contains("2 files matched"), "{text}");
        assert!(
            text.contains("truncated: 99999 bytes total"),
            "truncation notice must survive verbatim: {text}"
        );
    }

    #[test]
    fn test_grep_with_matches() {
        let output = "src/main.rs:10: fn main() {\nsrc/main.rs:20: println!(\"Hello\");\n";
        let result = render_grep_output(output, Theme::default(), 80);
        let text = flatten(&result);
        assert!(text.contains("2 matches found"), "Should show match count");
        assert!(text.contains("src/main.rs:10:"), "Should show file:line");
        assert!(text.contains("src/main.rs:20:"), "Should show file:line");
    }

    #[test]
    fn test_grep_skip_notice_passes_through_verbatim() {
        // host/grep.rs's append_skip_notice (ADR-0091) lines have no
        // `path:lineno:content` shape — they must survive, not vanish (#520).
        let output = "src/main.rs:10:fn main() {\n\
             \n[skipped 1 file(s) over the 1024 KiB scan cap:\n\
             \x20 huge.txt (2097152 bytes)\n]";
        let result = render_grep_output(output, Theme::default(), 80);
        let text = flatten(&result);
        assert!(text.contains("1 matches found"), "{text}");
        assert!(
            text.contains("skipped 1 file(s) over the 1024 KiB scan cap"),
            "skip notice header must survive: {text}"
        );
        assert!(
            text.contains("huge.txt (2097152 bytes)"),
            "skip notice detail must survive: {text}"
        );
    }

    #[test]
    fn test_grep_truncation_marker_is_visible() {
        let output = "src/main.rs:10:fn main() {\n... [truncated: 99999 bytes total]";
        let result = render_grep_output(output, Theme::default(), 80);
        let text = flatten(&result);
        assert!(text.contains("1 matches found"), "{text}");
        assert!(
            text.contains("truncated: 99999 bytes total"),
            "truncation notice must survive verbatim: {text}"
        );
    }
}
