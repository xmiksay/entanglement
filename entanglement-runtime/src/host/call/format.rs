//! Output rendering for `call`: line tailing + the `[exit N]`/`[killed: …]`
//! header assembly, split out of `mod.rs` (issue #451) since it has no
//! dependency on process spawning — pure string formatting over already
//! captured stdout/stderr bytes.
//!
//! A truncated result mints a retained-output handle (#608, ADR-0161 §7)
//! instead of naming a scratch-artifact path: the operation registry that
//! already exists now holds the full text too, and `poll` pages it. An
//! explicit `output_file` is untouched — it still gets its own path named —
//! but also gets a handle once truncated, so `poll` can report that path back
//! without the model having to remember it.

use crate::host::{bounded_result, tail_lines, MAX_OUTPUT_BYTES};
use crate::retained_output::RetainedOutputRegistry;
use entanglement_core::SessionId;

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
    output_file: Option<&str>,
    retained: &RetainedOutputRegistry,
    owner: Option<&SessionId>,
) -> String {
    format_call_streams(
        &format!("[exit {}]\n", code.unwrap_or(-1)),
        stdout,
        stderr,
        tail,
        output_file,
        retained,
        owner,
    )
}

/// `header` + tailed stdout + a tailed `[stderr]` block, byte-capped. Shared by
/// the exit path (`[exit N]`) and the timeout path (`[killed: …]`, #169).
///
/// A result that overflows its tail/byte cap keeps its handle (#608, ADR-0161
/// §7, generalizing "backgrounded work has a handle" to "work you might still
/// have questions about has a handle"): with no `output_file`, the full body
/// is retained in memory and a fresh handle is minted for `poll` to page;
/// with an explicit `output_file` the file already holds the full text, so
/// the handle just lets `poll` report that path back rather than duplicating
/// it in memory.
#[allow(clippy::too_many_arguments)]
pub(super) fn format_call_streams(
    header: &str,
    stdout: &[u8],
    stderr: &[u8],
    tail: u32,
    output_file: Option<&str>,
    retained: &RetainedOutputRegistry,
    owner: Option<&SessionId>,
) -> String {
    let full_stdout = String::from_utf8_lossy(stdout);
    let full_stderr = String::from_utf8_lossy(stderr);
    let (stdout_tailed, stdout_dropped) = tail_lines(&full_stdout, tail);
    let (stderr_tailed, stderr_dropped) = tail_lines(&full_stderr, tail);
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

    match output_file {
        Some(path) => {
            // Explicit `output_file` is untouched: always named, regardless
            // of truncation — a real file the model asked for.
            if stderr.is_empty() {
                status.push_str(&format!("[output: {path}]\n"));
            } else {
                status.push_str(&format!("[output: {path}] [stderr: {path}.stderr]\n"));
            }
            if truncated {
                let handle = retained.register_file(owner.cloned(), path.to_string());
                status.push_str(&format!("[handle: {handle}]\n"));
            }
        }
        None if truncated => {
            let mut full_body = String::with_capacity(full_stdout.len() + full_stderr.len() + 8);
            full_body.push_str(&full_stdout);
            if !full_stderr.is_empty() {
                full_body.push_str("[stderr]\n");
                full_body.push_str(&full_stderr);
            }
            let handle = retained.register_text(owner.cloned(), full_body);
            status.push_str(&format!(
                "[output truncated — poll(handle=\"{handle}\") for the rest]\n"
            ));
        }
        None => {}
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

    fn retained() -> RetainedOutputRegistry {
        RetainedOutputRegistry::new()
    }

    #[test]
    fn format_renders_exit_and_separate_stderr() {
        let out = format_call_output(
            Some(2),
            b"hello\n",
            b"boom\n",
            30,
            Some("out.stdout"),
            &retained(),
            None,
        );
        assert!(out.starts_with("[exit 2]\n"), "got: {out}");
        assert!(out.contains("hello\n"), "got: {out}");
        assert!(out.contains("[stderr]\nboom\n"), "got: {out}");
        assert!(
            out.contains("[output: out.stdout] [stderr: out.stdout.stderr]"),
            "explicit output_file keeps the artifact header: {out}"
        );
        assert!(!out.contains("[handle:"), "no truncation, no handle: {out}");
    }

    #[test]
    fn small_default_output_names_no_artifact_and_no_handle() {
        let out = format_call_output(Some(0), b"hello\n", b"", 30, None, &retained(), None);
        assert_eq!(
            out, "[exit 0]\nhello\n",
            "no artifact header, no [stderr], no handle: {out}"
        );
    }

    #[test]
    fn tailed_default_output_mints_a_pollable_handle_not_a_path() {
        let big: String = (1..=50).map(|i| format!("o{i}\n")).collect();
        let reg = retained();
        let out = format_call_output(Some(0), big.as_bytes(), b"", 5, None, &reg, None);
        assert!(
            out.contains("poll(handle=\"o-"),
            "tail-truncation mints a retained-output handle: {out}"
        );
        assert!(!out.contains(".stdout]"), "no scratch path named: {out}");
        assert!(
            !out.contains("[stderr:"),
            "empty stderr omits sibling mention: {out}"
        );
    }

    #[test]
    fn explicit_with_empty_stderr_omits_sibling_mention() {
        let out = format_call_output(
            Some(0),
            b"hi\n",
            b"",
            30,
            Some("out.stdout"),
            &retained(),
            None,
        );
        assert!(out.contains("[output: out.stdout]\n"), "got: {out}");
        assert!(!out.contains("[stderr:"), "got: {out}");
    }

    #[test]
    fn explicit_output_file_truncated_also_gets_a_handle() {
        let big: String = (1..=50).map(|i| format!("o{i}\n")).collect();
        let out = format_call_output(
            Some(0),
            big.as_bytes(),
            b"",
            5,
            Some("out.stdout"),
            &retained(),
            None,
        );
        assert!(
            out.contains("[output: out.stdout]\n"),
            "path still named: {out}"
        );
        assert!(out.contains("[handle: o-"), "also gets a handle: {out}");
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
            None,
            &retained(),
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
            out.contains("poll(handle=\"o-"),
            "truncated default output mints a handle: {out}"
        );
        // Two omission notices — one per stream.
        assert_eq!(
            out.matches("earlier lines omitted").count(),
            2,
            "got: {out}"
        );
    }

    #[test]
    fn retained_handle_is_scoped_to_the_given_owner() {
        let reg = retained();
        let owner = SessionId::new("s1");
        let big: String = (1..=50).map(|i| format!("o{i}\n")).collect();
        let out = format_call_output(Some(0), big.as_bytes(), b"", 5, None, &reg, Some(&owner));
        let handle = out
            .lines()
            .find_map(|l| l.split("handle=\"").nth(1))
            .and_then(|s| s.split('"').next())
            .expect("handle in output");
        assert!(reg.page(handle, &owner, 0, 30).is_some());
        assert!(reg
            .page(handle, &SessionId::new("stranger"), 0, 30)
            .is_none());
    }
}
