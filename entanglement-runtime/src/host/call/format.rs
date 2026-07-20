//! Output rendering for `call`: line tailing + the `[exit N]`/`[killed: …]`
//! header assembly, split out of `mod.rs` (issue #451) since it has no
//! dependency on process spawning — pure string formatting over already
//! captured stdout/stderr bytes.

use crate::host::truncate_output;

/// Keep only the last `tail` lines of `s`. `tail == 0` disables line cutting
/// (the byte cap still applies downstream). When lines are dropped, prepend a
/// self-correction notice (ADR-0016) naming the count and `tail=0` escape hatch.
pub(super) fn tail_lines(s: &str, tail: u32) -> String {
    if tail == 0 || s.is_empty() {
        return s.to_string();
    }
    let lines: Vec<&str> = s.lines().collect();
    let tail = tail as usize;
    if lines.len() <= tail {
        return s.to_string();
    }
    let omitted = lines.len() - tail;
    let mut out = format!(
        "(… {omitted} earlier lines omitted, tail={tail} — rerun with tail=0 for full output)\n"
    );
    out.push_str(&lines[lines.len() - tail..].join("\n"));
    out.push('\n');
    out
}

/// Assemble `[exit N]` + tailed stdout + a tailed `[stderr]` block, then apply
/// the 32 KiB byte cap (ADR-0008) as the outer bound. The line tail and byte cap
/// are independent limits — either may fire, and the byte-cap notice names the
/// byte limit explicitly.
#[allow(clippy::too_many_arguments)]
pub(super) fn format_call_output(
    code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
    tail: u32,
    output_rel: &str,
    artifact_notice: Option<String>,
) -> String {
    format_call_streams(
        &format!("[exit {}]\n", code.unwrap_or(-1)),
        stdout,
        stderr,
        tail,
        output_rel,
        artifact_notice,
    )
}

/// `header` + tailed stdout + a tailed `[stderr]` block, byte-capped. Shared by
/// the exit path (`[exit N]`) and the timeout path (`[killed: …]`, #169). Also
/// names the durable artifact holding the *full* (untailed) output (#381).
#[allow(clippy::too_many_arguments)]
pub(super) fn format_call_streams(
    header: &str,
    stdout: &[u8],
    stderr: &[u8],
    tail: u32,
    output_rel: &str,
    artifact_notice: Option<String>,
) -> String {
    let mut out = String::from(header);
    out.push_str(&format!(
        "[output: {output_rel}] [stderr: {output_rel}.stderr]\n"
    ));
    if let Some(notice) = artifact_notice {
        out.push_str(&notice);
    }
    let stdout_str = String::from_utf8_lossy(stdout);
    let stdout_tailed = tail_lines(&stdout_str, tail);
    if !stdout_tailed.is_empty() {
        out.push_str(&stdout_tailed);
    }
    let stderr_str = String::from_utf8_lossy(stderr);
    let stderr_tailed = tail_lines(&stderr_str, tail);
    if !stderr_tailed.is_empty() {
        out.push_str("[stderr]\n");
        out.push_str(&stderr_tailed);
    }
    // Belt-and-suspenders: with tail=0 (or very long lines) the assembled output
    // can still exceed the context budget, so the 32 KiB byte cap
    // ([`MAX_OUTPUT_BYTES`]) remains the outer bound. Its notice
    // (`... [truncated: N bytes total]`) names the byte limit.
    truncate_output(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_last_n_and_notes_omitted() {
        let body: String = (1..=100).map(|i| format!("line{i}\n")).collect();
        let out = tail_lines(&body, 30);
        assert!(
            out.starts_with(
                "(… 70 earlier lines omitted, tail=30 — rerun with tail=0 for full output)\n"
            ),
            "got: {out}"
        );
        assert!(out.contains("line100"), "keeps the last line: {out}");
        assert!(out.contains("line71"), "keeps 30th-from-end: {out}");
        assert!(!out.contains("line70\n"), "drops the 31st-from-end: {out}");
    }

    #[test]
    fn tail_zero_is_full_output() {
        let body: String = (1..=100).map(|i| format!("line{i}\n")).collect();
        let out = tail_lines(&body, 0);
        assert_eq!(out, body);
        assert!(!out.contains("omitted"), "no notice with tail=0");
    }

    #[test]
    fn tail_under_threshold_is_untouched() {
        let body = "a\nb\nc\n";
        assert_eq!(tail_lines(body, 30), body);
    }

    #[test]
    fn format_renders_exit_and_separate_stderr() {
        let out = format_call_output(Some(2), b"hello\n", b"boom\n", 30, "out.stdout", None);
        assert!(out.starts_with("[exit 2]\n"), "got: {out}");
        assert!(out.contains("hello\n"), "got: {out}");
        assert!(out.contains("[stderr]\nboom\n"), "got: {out}");
        assert!(
            out.contains("[output: out.stdout] [stderr: out.stdout.stderr]"),
            "got: {out}"
        );
    }

    #[test]
    fn format_tails_both_streams_independently() {
        let big: String = (1..=50).map(|i| format!("o{i}\n")).collect();
        let err: String = (1..=50).map(|i| format!("e{i}\n")).collect();
        let out = format_call_output(
            Some(0),
            big.as_bytes(),
            err.as_bytes(),
            5,
            "out.stdout",
            None,
        );
        assert!(
            out.contains("o50") && !out.contains("o40\n"),
            "stdout tailed: {out}"
        );
        assert!(
            out.contains("e50") && !out.contains("e40\n"),
            "stderr tailed: {out}"
        );
        // Two omission notices — one per stream.
        assert_eq!(
            out.matches("earlier lines omitted").count(),
            2,
            "got: {out}"
        );
    }
}
