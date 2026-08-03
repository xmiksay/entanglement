//! Output rendering for `call`: line tailing + the `[exit N]`/`[killed: …]`
//! header assembly, split out of `mod.rs` (issue #451) since it has no
//! dependency on process spawning — pure string formatting over already
//! captured stdout/stderr bytes.

use crate::host::{bounded_result, tail_lines, MAX_OUTPUT_BYTES};

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

    let mut status = String::from(header);
    let truncated =
        stdout_dropped || stderr_dropped || header.len() + body.len() > MAX_OUTPUT_BYTES;
    if explicit || truncated {
        // Right after the exit header, before the body, so it always survives
        // the body's byte cap. The `.stderr` sibling is always written; it is
        // only worth naming when there is stderr to read.
        if stderr.is_empty() {
            status.push_str(&format!("[output: {output_rel}]\n"));
        } else {
            status.push_str(&format!(
                "[output: {output_rel}] [stderr: {output_rel}.stderr]\n"
            ));
        }
    }
    if let Some(notice) = artifact_notice {
        status.push_str(&notice);
    }
    // Belt-and-suspenders: with tail=0 (or very long lines) the assembled body
    // can still exceed the context budget, so the head+tail byte cap
    // ([`bounded_result`]) remains the outer bound — the same shape every
    // exec/agent tool now shares (#622), so a trailing error in a huge single
    // line isn't dropped the way head-only truncation would drop it.
    bounded_result(&status, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    // `tail_lines` itself now lives (and is tested) in `crate::host` — shared
    // with `bash` (#622).

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
