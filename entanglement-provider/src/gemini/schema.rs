//! JSON Schema → Gemini `parameters` translation. Gemini's parser accepts only
//! an OpenAPI-ish subset: no `$ref`/`$defs`, no meta keywords, no union types.
//! Tool schemas from schemars/rmcp (draft 2020-12) routinely use all three.

use serde_json::{Map, Value};

/// Max nested `$ref` expansions on one path. Real tool schemas nest a handful
/// of levels; the cap bounds output size for pathological (but acyclic) chains.
const MAX_REF_DEPTH: usize = 32;

/// Inline every `$ref` into the root `$defs`/`definitions` with its target
/// (recursively) and drop those maps. Keys beside a `$ref` (e.g. a
/// `description`) are merged over the inlined schema, sibling winning. A cyclic
/// ref, one past [`MAX_REF_DEPTH`], or an unresolvable one becomes `{}` (plus
/// its siblings): Gemini cannot express recursion, and `{}` is what the old
/// strip-only behaviour produced.
fn resolve_refs(root: &Value) -> Value {
    let Value::Object(map) = root else {
        return root.clone();
    };
    let mut body = map.clone();
    body.remove("$defs");
    body.remove("definitions");
    inline(&Value::Object(body), root, &mut Vec::new())
}

fn inline(value: &Value, root: &Value, stack: &mut Vec<String>) -> Value {
    match value {
        Value::Object(map) => {
            let Some(reference) = map.get("$ref").and_then(Value::as_str) else {
                return Value::Object(
                    map.iter()
                        .map(|(k, v)| (k.clone(), inline(v, root, stack)))
                        .collect(),
                );
            };
            let target = (!stack.iter().any(|r| r == reference) && stack.len() < MAX_REF_DEPTH)
                .then(|| lookup(root, reference))
                .flatten();
            let mut out = match target {
                Some(target) => {
                    stack.push(reference.to_string());
                    let resolved = inline(target, root, stack);
                    stack.pop();
                    match resolved {
                        Value::Object(m) => m,
                        // A boolean schema (`true`) carries no keywords to keep.
                        _ => Map::new(),
                    }
                }
                None => Map::new(),
            };
            for (k, v) in map.iter().filter(|(k, _)| k.as_str() != "$ref") {
                out.insert(k.clone(), inline(v, root, stack));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(|v| inline(v, root, stack)).collect()),
        _ => value.clone(),
    }
}

/// Resolve `#/$defs/Name` or `#/definitions/Name` against the root schema.
fn lookup<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let (section, name) = reference
        .strip_prefix("#/$defs/")
        .map(|n| ("$defs", n))
        .or_else(|| {
            reference
                .strip_prefix("#/definitions/")
                .map(|n| ("definitions", n))
        })?;
    // JSON Pointer escapes; `~1` must be decoded before `~0` (RFC 6901 §4).
    let name = name.replace("~1", "/").replace("~0", "~");
    root.get(section)?.get(name.as_str())
}

/// Gemini rejects JSON Schema meta-fields (`$schema`, `additionalProperties`,
/// `$ref`, `title`, …) and the union `type: ["string","null"]` form. Strip the
/// unsupported keys recursively, flatten a nullable union to `type` + `nullable`,
/// and drop `required` entries not present in `properties` (Gemini 400s otherwise).
/// Local `$ref`s are inlined first (see [`resolve_refs`]) so stripping `$ref`
/// does not throw the referenced subschema away.
pub(super) fn sanitize_schema(value: &Value) -> Value {
    clean(&resolve_refs(value))
}

fn clean(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            let mut nullable_from_type = false;
            for (k, v) in map {
                if matches!(
                    k.as_str(),
                    "$schema"
                        | "$ref"
                        | "$id"
                        | "$defs"
                        | "definitions"
                        | "additionalProperties"
                        | "title"
                        | "examples"
                        | "default"
                        | "const"
                        | "oneOf"
                        | "anyOf"
                        | "allOf"
                        | "not"
                        | "format"
                ) {
                    continue;
                }
                if k == "type" {
                    if let Value::Array(types) = v {
                        let mut primary: Option<String> = None;
                        for t in types.iter().filter_map(|t| t.as_str()) {
                            if t == "null" {
                                nullable_from_type = true;
                            } else if primary.is_none() {
                                primary = Some(t.to_string());
                            }
                        }
                        if let Some(t) = primary {
                            out.insert("type".into(), Value::String(t));
                        }
                        continue;
                    }
                }
                out.insert(k.clone(), clean(v));
            }
            if nullable_from_type {
                out.insert("nullable".into(), Value::Bool(true));
            }
            prune_required(&mut out);
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(clean).collect()),
        _ => value.clone(),
    }
}

/// Drop `required` names not present in `properties`, removing it when empty.
fn prune_required(out: &mut Map<String, Value>) {
    let prop_keys: Option<Vec<String>> = out
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|m| m.keys().cloned().collect());
    let Some(Value::Array(arr)) = out.get("required").cloned() else {
        out.remove("required");
        return;
    };
    let allowed: Vec<Value> = arr
        .into_iter()
        .filter(|v| match (v.as_str(), &prop_keys) {
            (Some(name), Some(keys)) => keys.iter().any(|k| k == name),
            _ => false,
        })
        .collect();
    if allowed.is_empty() {
        out.remove("required");
    } else {
        out.insert("required".into(), Value::Array(allowed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sanitize_strips_meta_fields_and_prunes_required() {
        let schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "additionalProperties": false,
            "properties": { "a": { "type": ["string", "null"], "format": "uri" } },
            "required": ["a", "ghost"],
        });
        let out = sanitize_schema(&schema);
        assert!(out.get("$schema").is_none());
        assert!(out.get("additionalProperties").is_none());
        assert_eq!(out["properties"]["a"]["type"], "string");
        assert_eq!(out["properties"]["a"]["nullable"], true);
        assert!(out["properties"]["a"].get("format").is_none());
        // "ghost" is not a property, so it is pruned; "a" survives.
        assert_eq!(out["required"], json!(["a"]));
    }

    #[test]
    fn ref_in_array_items_is_inlined_and_defs_dropped() {
        let schema = json!({
            "type": "object",
            "properties": {
                "items": { "type": "array", "items": { "$ref": "#/$defs/ItemBody" } }
            },
            "$defs": {
                "ItemBody": {
                    "type": "object",
                    "properties": { "qty": { "type": "integer" } },
                    "required": ["qty"]
                }
            }
        });
        let out = sanitize_schema(&schema);
        assert!(out.get("$defs").is_none());
        let item = &out["properties"]["items"]["items"];
        assert_eq!(item["type"], "object");
        assert_eq!(item["properties"]["qty"]["type"], "integer");
        assert_eq!(item["required"], json!(["qty"]));
    }

    #[test]
    fn same_def_referenced_twice_is_inlined_at_both_sites() {
        let schema = json!({
            "type": "object",
            "properties": {
                "from": { "$ref": "#/definitions/Addr" },
                "to": { "$ref": "#/definitions/Addr" }
            },
            "definitions": { "Addr": { "type": "string", "title": "Addr" } }
        });
        let out = sanitize_schema(&schema);
        assert!(out.get("definitions").is_none());
        assert_eq!(out["properties"]["from"], json!({ "type": "string" }));
        assert_eq!(out["properties"]["to"], json!({ "type": "string" }));
    }

    #[test]
    fn ref_inside_a_def_is_resolved() {
        let schema = json!({
            "type": "object",
            "properties": { "line": { "$ref": "#/$defs/Line" } },
            "$defs": {
                "Line": {
                    "type": "object",
                    "properties": { "price": { "$ref": "#/$defs/Money" } }
                },
                "Money": { "type": "string" }
            }
        });
        let out = sanitize_schema(&schema);
        assert_eq!(
            out["properties"]["line"]["properties"]["price"],
            json!({ "type": "string" })
        );
    }

    #[test]
    fn self_recursive_def_terminates_with_empty_schema() {
        let schema = json!({
            "type": "object",
            "properties": { "root": { "$ref": "#/$defs/Node" } },
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": {
                        "children": { "type": "array", "items": { "$ref": "#/$defs/Node" } }
                    }
                }
            }
        });
        let out = sanitize_schema(&schema);
        let node = &out["properties"]["root"];
        assert_eq!(node["type"], "object");
        assert_eq!(node["properties"]["children"]["items"], json!({}));
    }

    #[test]
    fn unknown_ref_becomes_empty_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "a": { "$ref": "#/$defs/Missing" },
                "b": { "$ref": "https://example.com/schema.json" }
            }
        });
        let out = sanitize_schema(&schema);
        assert_eq!(out["properties"]["a"], json!({}));
        assert_eq!(out["properties"]["b"], json!({}));
    }

    #[test]
    fn sibling_keys_merge_over_the_inlined_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "item": { "$ref": "#/$defs/Item", "description": "the line item" }
            },
            "$defs": {
                "Item": { "type": "object", "description": "generic", "properties": {} }
            }
        });
        let out = sanitize_schema(&schema);
        let item = &out["properties"]["item"];
        assert_eq!(item["type"], "object");
        assert_eq!(item["description"], "the line item");
    }

    #[test]
    fn ref_chain_deeper_than_cap_is_cut_off() {
        let mut defs = Map::new();
        for i in 0..40 {
            defs.insert(
                format!("D{i}"),
                json!({ "type": "object", "properties": { "n": { "$ref": format!("#/$defs/D{}", i + 1) } } }),
            );
        }
        let schema = json!({ "$ref": "#/$defs/D0", "$defs": defs });
        let out = sanitize_schema(&schema);
        let mut node = &out;
        for _ in 0..MAX_REF_DEPTH {
            assert_eq!(node["type"], "object");
            node = &node["properties"]["n"];
        }
        assert_eq!(node, &json!({}));
    }
}
