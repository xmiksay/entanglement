//! Output rendering for `call`: line tailing + the `[exit N]`/`[killed: …]`
//! header assembly, split out of `mod.rs` (issue #451) since it has no
//! dependency on process spawning — pure string formatting over already
//! captured stdout/stderr bytes.

use crate::host::{truncate_output, MAX_OUTPUT_BYTES};

/// Keep only the last `tail` lines of `s`, reporting whether any were dropped.
/// `tail == 0` disables line cutting (the byte cap still applies downstream).
/// When lines are dropped, prepend a self-correction notice (ADR-0016) naming
/// the count and `tail=0` escape hatch.
pub(super) fn tail_lines(s: &str, tail: u32) -> (String, bool) {
    if tail == 0 || s.is_empty() {
        return (s.to_string(), false);
    }
    let lines: Vec<&str> = s.lines().collect();
    let tail = tail as usize;
    if lines.len() <= tail {
        return (s.to_string(), false);
    }
    let omitted = lines.len() - tail;
    let mut out = format!(
        "(… {omitted} earlier lines omitted, tail={tail} — rerun with tail=0 for full output)\n"
    );
    out.push_str(&lines[lines.len() - tail..].join("\n"));
    out.push('\n');
    (out, true)
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
    explicit: bool,
    artifact_notice: Option<String>,
) -> String {
    format_call_streams(
        &format!("[exit {}]\n", code.unwrap_or(-1)),
        stdout,
        stderr,
        tail,
        output_rel,
        explicit,
        artifact_notice,
    )
}

/// `header` + tailed stdout + a tailed `[stderr]` block, byte-capped. Shared by
/// the exit path (`[exit N]`) and the timeout path (`[killed: …]`, #169). Names
/// the durable artifact holding the *full* (untailed) output (#381) only when
/// the model can't already see it all — i.e. something was tailed/byte-capped —
/// or when the caller pinned it with an explicit `output_file`; otherwise the
/// path (an absolute scratch path in the default case) is pure token overhead.
#[allow(clippy::too_many_arguments)]
pub(super) fn format_call_streams(
    header: &str,
    stdout: &[u8],
    stderr: &[u8],
    tail: u32,
    output_rel: &str,
    explicit: bool,
    artifact_notice: Option<String>,
) -> String {
    let (stdout_tailed, stdout_dropped) = tail_lines(&String::from_utf8_lossy(stdout), tail);
    let (stderr_tailed, stderr_dropped) = tail_lines(&String::from_utf8_lossy(stderr), tail);
    let mut body = String::new();
    if !stdout_tailed.is_empty() {
        body.push_str(&stdout_tailed);
    }
    if !stderr_tailed.is_empty() {
        body.push_str("[stderr]\n");
        body.push_str(&stderr_tailed);
    }

    let mut out = String::from(header);
    let truncated =
        stdout_dropped || stderr_dropped || header.len() + body.len() > MAX_OUTPUT_BYTES;
    if explicit || truncated {
        // Right after the exit header, before the body, so the head-keeping
        // byte cap can never drop it. The `.stderr` sibling is always written;
        // it is only worth naming when there is stderr to read.
        if stderr.is_empty() {
            out.push_str(&format!("[output: {output_rel}]\n"));
        } else {
            out.push_str(&format!(
                "[output: {output_rel}] [stderr: {output_rel}.stderr]\n"
            ));
        }
    }
    if let Some(notice) = artifact_notice {
        out.push_str(&notice);
    }
    out.push_str(&body);
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
        let (out, dropped) = tail_lines(&body, 30);
        assert!(dropped);
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
        let (out, dropped) = tail_lines(&body, 0);
        assert_eq!(out, body);
        assert!(!dropped, "no drop with tail=0");
        assert!(!out.contains("omitted"), "no notice with tail=0");
    }

    #[test]
    fn tail_under_threshold_is_untouched() {
        let body = "a\nb\nc\n";
        assert_eq!(tail_lines(body, 30), (body.to_string(), false));
    }

    #[test]
    fn format_renders_exit_and_separate_stderr() {
        let out = format_call_output(Some(2), b"hello\n", b"boom\n", 30, "out.stdout", true, None);
        assert!(out.starts_with("[exit 2]\n"), "got: {out}");
        assert!(out.contains("hello\n"), "got: {out}");
        assert!(out.contains("[stderr]\nboom\n"), "got: {out}");
        assert!(
            out.contains("[output: out.stdout] [stderr: out.stdout.stderr]"),
            "explicit output_file keeps the artifact header: {out}"
        );
    }

    #[test]
    fn small_default_output_names_no_artifact() {
        let out = format_call_output(
            Some(0),
            b"hello\n",
            b"",
            30,
            "/scratch/abs.stdout",
            false,
            None,
        );
        assert_eq!(
            out, "[exit 0]\nhello\n",
            "no artifact header, no [stderr]: {out}"
        );
    }

    #[test]
    fn tailed_default_output_names_the_artifact() {
        let big: String = (1..=50).map(|i| format!("o{i}\n")).collect();
        let out = format_call_output(
            Some(0),
            big.as_bytes(),
            b"",
            5,
            "/scratch/abs.stdout",
            false,
            None,
        );
        assert!(
            out.contains("[output: /scratch/abs.stdout]\n"),
            "tail-truncation names the artifact: {out}"
        );
        assert!(
            !out.contains("[stderr:"),
            "empty stderr omits the sibling mention: {out}"
        );
    }

    #[test]
    fn explicit_with_empty_stderr_omits_sibling_mention() {
        let out = format_call_output(Some(0), b"hi\n", b"", 30, "out.stdout", true, None);
        assert!(out.contains("[output: out.stdout]\n"), "got: {out}");
        assert!(!out.contains("[stderr:"), "got: {out}");
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
            false,
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
        assert!(
            out.contains("[output: out.stdout] [stderr: out.stdout.stderr]"),
            "truncated default output names both artifacts: {out}"
        );
        // Two omission notices — one per stream.
        assert_eq!(
            out.matches("earlier lines omitted").count(),
            2,
            "got: {out}"
        );
    }
}
