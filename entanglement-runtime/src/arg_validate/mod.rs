//! Pre-dispatch argument validation against a tool's advertised [`ToolSpec`]
//! schema (#560, ADR-0196 §6, carried forward by ADR-0226) — the three-way
//! error taxonomy:
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
//! problem) and the delivered-schema dedup,
//! [`crate::tool_advertising::DeliveredSchemas`] (shared with `mcp_tools`,
//! ADR-0226), so a repeat violation never re-sends a schema already in
//! context. Both guard the model's context, so a script's call ([`Reader`])
//! touches neither (#844). An MCP call through the `mcp` envelope is
//! validated here like any native call: a violation never reaches the server.

use entanglement_core::ToolSpec;
use serde_json::{json, Value};

use crate::tools::closest_name;

mod duplicate_keys;
mod loop_breaker;

pub use loop_breaker::{LoopBreaker, LOOP_BREAKER_NOTE};

/// Duplicate JSON object keys in `input`, at any nesting depth (#560
/// remainder). Exposed for [`crate::tool_runner::dispatch::execute`]: an
/// `mcp`-unwrapped call's dispatched input already lost this information
/// (core had to parse the outer envelope to extract `args`, which collapses
/// a duplicate the same way `validate` below does) — the caller re-scans the
/// envelope's raw, still-duplicated text instead.
pub(crate) fn find_duplicate_keys(input: &str) -> Vec<String> {
    duplicate_keys::duplicate_keys(input)
}

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
    /// Keys repeated within the same JSON object, at any nesting depth
    /// (#560 remainder): `serde_json::Value` silently keeps only the later
    /// value, so e.g. a duplicated `path` can present as a missing
    /// `content` with no clue that the *real* mistake was writing `path`
    /// twice. An addition to the violation, never a replacement — the
    /// missing/unexpected/type findings above still stand.
    pub duplicate_keys: Vec<String>,
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
            && self.duplicate_keys.is_empty()
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
        for key in &self.duplicate_keys {
            out.push(format!(
                "duplicate key \"{key}\" (the later value silently replaced the earlier one)"
            ));
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

    // Scanned from the raw text, not `obj` above — by the time `obj` exists
    // the duplicate is already gone, collapsed to its last value.
    let mut violation = Violation {
        duplicate_keys: duplicate_keys::duplicate_keys(trimmed),
        ..Default::default()
    };
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

/// Who a schema-violation decline is written for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reader<'a> {
    /// The model; `schema_delivered` when this session's context already
    /// holds the tool's schema (the delivered-schema dedup, ADR-0196 §6).
    Model { schema_delivered: bool },
    /// A running script (ADR-0227), which never sees the model's context.
    /// `rhai_path` is the `mcp::<server>::<tool>` path the script called an
    /// MCP tool by — the binding's own, since a server's name can't be split
    /// back out of the registered `mcp__<server>__<tool>` reliably.
    Script { rhai_path: Option<&'a str> },
}

/// Build the full schema-violation decline: what was wrong, plus — unless
/// the model already holds it — the tool's full schema
/// ([`crate::discover::spec_to_json`], the one rendering `mcp_tools` also
/// shows) and one minimal example call. An MCP tool's example is shaped as
/// the call that reaches it from `reader` — the `mcp` envelope for the
/// model, the rhai path for a script; a native-shaped one would teach a call
/// neither can make.
pub(crate) fn decline_text(spec: &ToolSpec, violation: &Violation, reader: Reader) -> String {
    let mut msg = format!(
        "schema violation calling `{}`: {}",
        spec.name,
        violation.lines().join("; ")
    );
    if let Reader::Model {
        schema_delivered: true,
    } = reader
    {
        msg.push_str("\n\nschema already provided above for this session — not repeating it.");
        return msg;
    }
    let pretty = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_default();
    msg.push_str("\n\ncorrect usage:\n");
    msg.push_str(&pretty(&crate::discover::spec_to_json(spec)));
    let args = minimal_example(&spec.schema);
    let (label, example) = match reader {
        Reader::Script { rhai_path } => match rhai_path {
            Some(path) => (
                "example call (in rhai)",
                format!("{path}({})", script_args(&args)),
            ),
            // A native binding takes positional arguments; this is the
            // tool input it builds from them.
            None => ("example input", pretty(&args)),
        },
        Reader::Model { .. } => {
            match crate::tool_advertising::mcp_example(&spec.name, args.clone()) {
                Some(call) => ("example call (through mcp)", pretty(&call)),
                None => ("example call", pretty(&args)),
            }
        }
    };
    msg.push_str(&format!("\n\n{label}:\n{example}"));
    msg
}

#[cfg(feature = "rhai")]
fn script_args(args: &Value) -> String {
    crate::rhai_ident::rhai_literal(args)
}

// Without the script engine nothing builds a `Reader::Script`.
#[cfg(not(feature = "rhai"))]
fn script_args(args: &Value) -> String {
    args.to_string()
}

#[cfg(test)]
mod tests;
