//! Pure JSON/YAML (de)serialization functions for `rhai` scripts. Unlike the
//! host quintet these are *not* bindings — no IO, no permission check, no
//! bridge round-trip — since they only transform a value already in the
//! script's own memory (typically the output of `read()`).

use rhai::serde::{from_dynamic, to_dynamic};
use rhai::{Dynamic, Engine, EvalAltResult};

use super::runtime_err;

/// Register the four functions. Built on Rhai's own `serde` bridge
/// (`rhai::serde::{to_dynamic, from_dynamic}`, already enabled via the crate's
/// `serde` feature), so the JSON/YAML-Value <-> Dynamic mapping is Rhai's own
/// tested behavior, not a hand-rolled converter: `null` -> `()`; an integer
/// outside `i64` range silently widens to an approximate `FLOAT` (Rhai's serde
/// serializer falls back i64 -> decimal (off by default) -> float, same as
/// JS's `JSON.parse` — well-formed JSON already encodes such values as strings
/// to avoid exactly this, so scripts should too). Rhai's UFCS means each is
/// also callable as a method, e.g. `read(path).parse_json()`.
pub(super) fn register_data_functions(engine: &mut Engine) {
    engine.register_fn("parse_json", parse_json);
    engine.register_fn("to_json", to_json);
    engine.register_fn("parse_yaml", parse_yaml);
    engine.register_fn("to_yaml", to_yaml);
}

fn parse_json(text: &str) -> Result<Dynamic, Box<EvalAltResult>> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| runtime_err(&format!("invalid JSON: {e}")))?;
    to_dynamic(value)
        .map_err(|e| runtime_err(&format!("JSON value not representable in Rhai: {e}")))
}

fn to_json(value: Dynamic) -> Result<String, Box<EvalAltResult>> {
    let json: serde_json::Value = from_dynamic(&value)
        .map_err(|e| runtime_err(&format!("value not JSON-serializable: {e}")))?;
    serde_json::to_string(&json).map_err(|e| runtime_err(&format!("failed to stringify JSON: {e}")))
}

fn parse_yaml(text: &str) -> Result<Dynamic, Box<EvalAltResult>> {
    let value: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|e| runtime_err(&format!("invalid YAML: {e}")))?;
    to_dynamic(value)
        .map_err(|e| runtime_err(&format!("YAML value not representable in Rhai: {e}")))
}

fn to_yaml(value: Dynamic) -> Result<String, Box<EvalAltResult>> {
    let yaml: serde_yaml::Value = from_dynamic(&value)
        .map_err(|e| runtime_err(&format!("value not YAML-serializable: {e}")))?;
    serde_yaml::to_string(&yaml).map_err(|e| runtime_err(&format!("failed to stringify YAML: {e}")))
}
