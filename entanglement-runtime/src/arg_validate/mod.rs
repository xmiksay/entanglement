//! Pre-dispatch argument validation against a tool's advertised [`ToolSpec`]
//! schema (#560, ADR-0196 §6) — the three-way error taxonomy `tool_search.md`
//! §7 settles:
//!
//! | case | reply |
//! | --- | --- |
//! | schema violation (malformed call, missing/unexpected params) | schema + exactly what was wrong |
//! | parameter error / command failure (valid shape, bad value or non-zero exit) | the normal result, untouched — never flagged |
//!
//! [`validate`] is the pragmatic JSON-Schema subset this repo's tool schemas
//! actually use: `required`, `properties` (+ each property's `type`/`enum`),
//! `additionalProperties`. It runs **before** a call ever reaches
//! [`Tool::run`][crate::tools::Tool::run] — a violation never executes the
//! tool at all, so "parameter error" (a value the tool itself rejects at
//! runtime — file not found, out of range) and "command failure" (a
//! non-zero exit) are untouched by this module: they deserialize fine here
//! and fail, or succeed, inside the tool.
//!
//! Two guards ride alongside the taxonomy: [`LoopBreaker`] (two identical
//! failing calls in a row get an explicit note — the schema was never the
//! problem) and the delivered-schema dedup, which reuses
//! [`crate::tool_advertising::DiscoveredSet`] (shared with the P3 `describe`
//! tracking, ADR-0196 §4) so a repeat violation never re-sends a schema
//! already in context.

use std::collections::HashMap;
use std::sync::Mutex;

use entanglement_core::{SessionId, ToolSpec};
use serde_json::{json, Value};

use crate::tools::closest_name;

/// One schema violation found in a call's input against the tool's schema.
/// `Default` is "no violation" — [`validate`] returns `None` rather than an
/// empty one, but the builder assembles into this shape.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Violation {
    /// The input string wasn't valid JSON at all (parse error text).
    pub malformed_json: Option<String>,
    /// The input parsed, but isn't a JSON object.
    pub not_object: bool,
    pub missing: Vec<String>,
    pub unexpected: Vec<UnexpectedProp>,
    pub type_errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnexpectedProp {
    pub name: String,
    pub hint: Option<String>,
}

impl Violation {
    fn is_empty(&self) -> bool {
        self.malformed_json.is_none()
            && !self.not_object
            && self.missing.is_empty()
            && self.unexpected.is_empty()
            && self.type_errors.is_empty()
    }

    /// Render "what specifically was wrong", one line per finding — the text
    /// the schema-violation decline (`decline_text`) leads with.
    pub fn lines(&self) -> Vec<String> {
        if let Some(err) = &self.malformed_json {
            return vec![format!("malformed JSON input: {err}")];
        }
        if self.not_object {
            return vec!["input must be a JSON object".to_string()];
        }
        let mut out = Vec::new();
        if !self.missing.is_empty() {
            out.push(format!("missing required: {}", self.missing.join(", ")));
        }
        for u in &self.unexpected {
            match &u.hint {
                Some(h) => out.push(format!("unexpected: {} — did you mean `{h}`?", u.name)),
                None => out.push(format!("unexpected: {}", u.name)),
            }
        }
        out.extend(self.type_errors.iter().cloned());
        out
    }
}

/// Validate `input` (the model's raw JSON-object argument string) against
/// `schema` (a tool's advertised `input_schema`). `None` means the call is
/// schema-clean — it may still fail at runtime (a parameter error or command
/// failure), which is not this function's concern. An empty/whitespace-only
/// `input` is treated as `{}` (a no-arg call), matching
/// [`crate::mcp::tool::McpTool::run`]'s convention.
pub fn validate(schema: &Value, input: &str) -> Option<Violation> {
    // A schema with no declared shape — the `Tool::schema` default, doc'd
    // there as deliberately "permissive" for a tool with nothing structured
    // to say — makes no promise about the input's format at all. Nothing to
    // validate against, so even non-JSON/non-object input isn't a violation:
    // enforcing "must be a JSON object" against a schema that never asked
    // for one would contradict what "permissive" means. Every real
    // structured tool declares at least one of `properties`/`required`.
    let has_properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|p| !p.is_empty());
    let has_required = schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|r| !r.is_empty());
    if !has_properties && !has_required {
        return None;
    }
    let trimmed = input.trim();
    let value: Value = if trimmed.is_empty() {
        json!({})
    } else {
        match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                return Some(Violation {
                    malformed_json: Some(e.to_string()),
                    ..Default::default()
                })
            }
        }
    };
    let Some(obj) = value.as_object() else {
        return Some(Violation {
            not_object: true,
            ..Default::default()
        });
    };

    let mut violation = Violation::default();
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for r in required.iter().filter_map(Value::as_str) {
            if !obj.contains_key(r) {
                violation.missing.push(r.to_string());
            }
        }
    }

    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        // Additional properties are rejected unless the schema explicitly
        // opts in — the pragmatic default this subset applies whenever a
        // schema bothers to list `properties` at all (design decision 8).
        let additional_ok = schema
            .get("additionalProperties")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !additional_ok {
            let prop_names: Vec<&str> = props.keys().map(String::as_str).collect();
            for key in obj.keys() {
                if !props.contains_key(key) {
                    let hint = closest_name(key, &prop_names).map(str::to_string);
                    violation.unexpected.push(UnexpectedProp {
                        name: key.clone(),
                        hint,
                    });
                }
            }
        }
        for (key, val) in obj {
            let Some(prop_schema) = props.get(key) else {
                continue;
            };
            if let Some(expected) = prop_schema.get("type").and_then(Value::as_str) {
                if !type_matches(expected, val) {
                    violation.type_errors.push(format!(
                        "wrong type for `{key}`: expected {expected}, got {}",
                        json_type_name(val)
                    ));
                    continue; // skip the enum check below on an already-wrong-typed value
                }
            }
            if let Some(enum_vals) = prop_schema.get("enum").and_then(Value::as_array) {
                if !enum_vals.contains(val) {
                    let allowed: Vec<String> = enum_vals.iter().map(Value::to_string).collect();
                    violation.type_errors.push(format!(
                        "invalid value for `{key}`: must be one of [{}]",
                        allowed.join(", ")
                    ));
                }
            }
        }
    }

    if violation.is_empty() {
        None
    } else {
        Some(violation)
    }
}

fn type_matches(expected: &str, val: &Value) -> bool {
    match expected {
        "string" => val.is_string(),
        "integer" => val.is_i64() || val.is_u64(),
        "number" => val.is_number(),
        "boolean" => val.is_boolean(),
        "array" => val.is_array(),
        "object" => val.is_object(),
        "null" => val.is_null(),
        // An unrecognized/unsupported type keyword: this subset deliberately
        // doesn't know every JSON-Schema type, so it never false-positives
        // on one it can't check (e.g. a union type string).
        _ => true,
    }
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// One property=>example-value pick for [`minimal_example`].
fn example_value(prop_schema: &Value) -> Value {
    if let Some(first) = prop_schema
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
    {
        return first.clone();
    }
    match prop_schema.get("type").and_then(Value::as_str) {
        Some("integer") | Some("number") => json!(1),
        Some("boolean") => json!(true),
        Some("array") => json!([]),
        Some("object") => json!({}),
        _ => json!("example"),
    }
}

/// A minimal call satisfying `schema`'s required properties — one example
/// value per required field, nothing else. The "one minimal example call"
/// the schema-violation decline shows alongside the full schema.
pub fn minimal_example(schema: &Value) -> Value {
    let mut obj = serde_json::Map::new();
    if let (Some(props), Some(required)) = (
        schema.get("properties").and_then(Value::as_object),
        schema.get("required").and_then(Value::as_array),
    ) {
        for name in required.iter().filter_map(Value::as_str) {
            if let Some(prop_schema) = props.get(name) {
                obj.insert(name.to_string(), example_value(prop_schema));
            }
        }
    }
    Value::Object(obj)
}

/// Build the full schema-violation decline: what was wrong, plus — unless
/// `schema_already_delivered` (the delivered-schema dedup guard, ADR-0196
/// §6) — the tool's full schema (byte-identical to `describe`'s rendering,
/// [`crate::discover::spec_to_json`]) and one minimal example call.
pub fn decline_text(
    spec: &ToolSpec,
    violation: &Violation,
    schema_already_delivered: bool,
) -> String {
    let mut msg = format!(
        "schema violation calling `{}`: {}",
        spec.name,
        violation.lines().join("; ")
    );
    if schema_already_delivered {
        msg.push_str("\n\nschema already provided above for this session — not repeating it.");
    } else {
        let schema_json = crate::discover::spec_to_json(spec);
        msg.push_str("\n\ncorrect usage:\n");
        msg.push_str(&serde_json::to_string_pretty(&schema_json).unwrap_or_default());
        msg.push_str("\n\nexample call:\n");
        msg.push_str(
            &serde_json::to_string_pretty(&minimal_example(&spec.schema)).unwrap_or_default(),
        );
    }
    msg
}

/// The explicit note appended when [`LoopBreaker::note`] reports a repeat —
/// two identical failing calls in a row. Verbatim text (design decision 8).
pub const LOOP_BREAKER_NOTE: &str =
    "same call failed twice — the schema is not the problem; change the arguments or use a \
     different tool";

#[derive(Clone, PartialEq)]
struct LastCall {
    tool: String,
    input: String,
    is_error: bool,
}

/// Per-session "was the immediately preceding call this exact same
/// `(tool, input)`, and did it also fail?" tracker — the loop-breaker guard
/// (design decision 8): a model retrying the identical failing shape twice in
/// a row gets an explicit nudge to change the call instead of the schema
/// alone. Deliberately generic across every failure kind (schema violation,
/// MCP required-param, or a runtime tool error) — the note fires on any
/// `is_error` repeat, not only schema violations.
#[derive(Default)]
pub struct LoopBreaker {
    last: Mutex<HashMap<SessionId, LastCall>>,
}

impl LoopBreaker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record this call's outcome for `session`, returning whether it
    /// repeats the immediately preceding call's exact `(tool, input)` with
    /// both calls having failed. Always records — a success in between
    /// resets the streak, matching "two *consecutive* identical failures".
    pub fn note(&self, session: &SessionId, tool: &str, input: &str, is_error: bool) -> bool {
        let mut map = self.last.lock().expect("loop-breaker mutex poisoned");
        let repeat = is_error
            && map
                .get(session)
                .is_some_and(|prev| prev.is_error && prev.tool == tool && prev.input == input);
        map.insert(
            session.clone(),
            LastCall {
                tool: tool.to_string(),
                input: input.to_string(),
                is_error,
            },
        );
        repeat
    }

    /// Release an ended/hibernated session's entry — mirrors
    /// [`crate::tool_advertising::DiscoveredSet::forget`].
    pub fn forget(&self, session: &SessionId) {
        self.last
            .lock()
            .expect("loop-breaker mutex poisoned")
            .remove(session);
    }
}

#[cfg(test)]
mod tests;
