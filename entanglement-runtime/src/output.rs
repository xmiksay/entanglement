//! Output bounding shared by every tool result that can grow without limit
//! (ADR-0008, #170, #622): host tools, `poll`, `rhai`, `agent` answers and
//! MCP results all cap through here, so one budget and one truncation shape
//! hold everywhere.

/// Hard cap on a single tool's textual output, in bytes. Larger output is
/// truncated with a notice. Picked generously below the context budget so a
/// normal source file fits, but a minified bundle or huge directory listing
/// can't blow the window. See ADR-0008.
pub const MAX_OUTPUT_BYTES: usize = 32 * 1024;

/// Default line-tail depth for `bash` (and `poll`'s retained-output pages)
/// when the model doesn't override it (#622).
pub const DEFAULT_TAIL: u32 = 30;

/// Cap `s` at [`MAX_OUTPUT_BYTES`] on a UTF-8 boundary, appending a notice of
/// the original size so the model knows data was dropped.
pub fn truncate_output(s: String) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s;
    }
    let mut cut = MAX_OUTPUT_BYTES;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::from(&s[..cut]);
    out.push_str(&format!("\n... [truncated: {} bytes total]", s.len()));
    out
}

/// Byte-cap `s` at `budget` keeping a **head + tail** slice, with a notice
/// naming the omitted middle. For build/test output the *tail* (the error, the
/// failing assertion, the summary line) is the load-bearing part, so head-only
/// truncation ([`truncate_output`]) throws away exactly what the model needs —
/// the tail gets three-quarters of the budget, the head one quarter for the
/// invocation context (#170). Cuts land on UTF-8 boundaries. Shared by
/// [`truncate_head_tail`] (budget == [`MAX_OUTPUT_BYTES`]) and [`bounded_result`]
/// (budget reduced by an always-kept status line, #622).
fn truncate_head_tail_within(s: String, budget: usize) -> String {
    if s.len() <= budget {
        return s;
    }
    let head_budget = budget / 4;
    let tail_budget = budget - head_budget;
    let mut head_end = head_budget;
    while head_end > 0 && !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len() - tail_budget;
    while tail_start < s.len() && !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = tail_start - head_end;
    let mut out = String::with_capacity(head_end + (s.len() - tail_start) + 64);
    out.push_str(&s[..head_end]);
    out.push_str(&format!(
        "\n... [truncated: {omitted} bytes omitted from the middle; {} bytes total]\n",
        s.len()
    ));
    out.push_str(&s[tail_start..]);
    out
}

/// Byte-cap `s` at [`MAX_OUTPUT_BYTES`] keeping a head + tail slice. See
/// [`truncate_head_tail_within`] for the shape and rationale.
pub fn truncate_head_tail(s: String) -> String {
    truncate_head_tail_within(s, MAX_OUTPUT_BYTES)
}

/// Keep only the last `tail` lines of `s`, reporting whether any were dropped.
/// `tail == 0` disables line cutting (the byte cap still applies downstream).
/// When lines are dropped, prepend a self-correction notice (ADR-0016) naming
/// the count and the `tail=0` escape hatch. Used by `bash`'s `tail` knob
/// (#622).
pub fn tail_lines(s: &str, tail: u32) -> (String, bool) {
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

/// Assemble a bounded tool result: `status` kept verbatim and uncounted
/// against the cap (the exit/job header, an artifact pointer — never worth
/// dropping), followed by `body` byte-capped under [`MAX_OUTPUT_BYTES`] with
/// the same head+tail split as [`truncate_head_tail`] so a trailing
/// error/answer survives. The one shape every result-bounded exec/agent tool
/// now shares — `bash`, `poll`, `rhai`, and `agent` —
/// replacing the previous per-tool pick between head-only
/// ([`truncate_output`]) and head+tail truncation with no stated rule (#622).
pub fn bounded_result(status: &str, body: String) -> String {
    let budget = MAX_OUTPUT_BYTES.saturating_sub(status.len());
    let mut out = String::with_capacity(status.len() + body.len().min(budget));
    out.push_str(status);
    out.push_str(&truncate_head_tail_within(body, budget));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_head_tail_keeps_both_ends_under_cap() {
        // A body far over the cap, with unique head and tail markers.
        let mut s = String::from("HEAD_MARKER");
        s.push_str(&"x".repeat(MAX_OUTPUT_BYTES * 2));
        s.push_str("TAIL_MARKER");
        let out = truncate_head_tail(s.clone());
        assert!(
            out.len() < MAX_OUTPUT_BYTES + 128,
            "over cap: {}",
            out.len()
        );
        assert!(out.starts_with("HEAD_MARKER"), "head lost");
        assert!(out.ends_with("TAIL_MARKER"), "tail lost");
        assert!(out.contains("omitted from the middle"), "notice missing");
    }

    #[test]
    fn truncate_head_tail_passes_through_small_input() {
        let s = "small output\n".to_string();
        assert_eq!(truncate_head_tail(s.clone()), s);
    }

    #[test]
    fn tail_lines_keeps_last_n_and_notes_omitted() {
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
        assert!(!out.contains("line70\n"), "drops the 31st-from-end: {out}");
    }

    #[test]
    fn tail_lines_zero_is_full_output() {
        let body: String = (1..=100).map(|i| format!("line{i}\n")).collect();
        let (out, dropped) = tail_lines(&body, 0);
        assert_eq!(out, body);
        assert!(!dropped, "no drop with tail=0");
    }

    #[test]
    fn bounded_result_keeps_status_verbatim_and_uncounted() {
        // The status line survives in full even though the body alone blows the
        // cap — it must never compete with the body for budget.
        let status = "[exit 0]\n";
        let mut body = String::from("HEAD_MARKER");
        body.push_str(&"x".repeat(MAX_OUTPUT_BYTES * 2));
        body.push_str("TAIL_MARKER");
        let out = bounded_result(status, body);
        assert!(out.starts_with(status), "status dropped: {out}");
        assert!(out.contains("HEAD_MARKER"), "head lost: {out}");
        assert!(out.ends_with("TAIL_MARKER"), "tail lost: {out}");
        assert!(out.contains("omitted from the middle"), "notice missing");
        assert!(
            out.len() < MAX_OUTPUT_BYTES + status.len() + 128,
            "over cap: {}",
            out.len()
        );
    }

    #[test]
    fn bounded_result_passes_through_small_body() {
        let out = bounded_result("[exit 0]\n", "hi\n".to_string());
        assert_eq!(out, "[exit 0]\nhi\n");
    }
}
