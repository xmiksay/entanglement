//! Composing a script's tool output: captured prints plus the serialized
//! return value or error.

use rhai::{Dynamic, EvalAltResult};

use crate::host::truncate_head_tail;

use super::spec;

/// Compose the tool output: captured `print` lines, then the serialized return
/// value (or the error), bounded to [`crate::host::MAX_OUTPUT_BYTES`] with a
/// head+tail split (#622) — the return value/error is the load-bearing part at
/// the end, the same shape `bash`/`call`/`agent` now share instead of the
/// head-only truncation this used to get.
pub(super) fn format_output(
    prints: String,
    eval_result: Result<Result<Dynamic, Box<EvalAltResult>>, tokio::task::JoinError>,
) -> (String, bool) {
    let mut out = String::new();
    if !prints.is_empty() {
        out.push_str(&prints);
        if !prints.ends_with('\n') {
            out.push('\n');
        }
    }
    let (line, is_error) = result_line(eval_result);
    out.push_str(&line);
    (truncate_head_tail(out), is_error)
}

/// The final `=> <value>` / `rhai error: …` line for an eval outcome — shared
/// by the blocking composition above and the background path (#637), which
/// streams prints as they happen and appends only this line at finish.
///
/// A script that named a function/variable the engine does not bind gets the
/// binding reference appended ([`spec::binding_hint`]): that is the "guessed
/// the wrong binding" failure, and the catalogue in the result lets the model
/// self-correct on its next call instead of guessing again. It rides the tail
/// of the output, which survives `truncate_head_tail`.
pub(super) fn result_line(
    eval_result: Result<Result<Dynamic, Box<EvalAltResult>>, tokio::task::JoinError>,
) -> (String, bool) {
    match eval_result {
        Ok(Ok(value)) => (format!("=> {}", serialize_return(&value)), false),
        Ok(Err(e)) => {
            let hint = spec::binding_hint(&e).unwrap_or_default();
            (format!("rhai error: {e}{hint}"), true)
        }
        Err(join) => (format!("rhai error: script task failed: {join}"), true),
    }
}

/// Serialize a script's return value. Prefer JSON (arrays/maps/numbers/strings
/// round-trip cleanly); fall back to Rhai's display form for values JSON can't
/// represent. `()` is rendered explicitly so an empty return is unambiguous.
pub(super) fn serialize_return(value: &Dynamic) -> String {
    if value.is_unit() {
        return "()".to_string();
    }
    match serde_json::to_string(value) {
        Ok(s) => s,
        Err(_) => value.to_string(),
    }
}
