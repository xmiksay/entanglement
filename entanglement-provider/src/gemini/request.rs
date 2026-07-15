//! Gemini request-body assembly: `Message` history → `contents`, tool specs →
//! `functionDeclarations`, generation knobs → `generationConfig`, and the JSON
//! Schema sanitization Gemini's stricter parser needs. Split out of the parent
//! `gemini` module to keep each file under the size cap; the response/stream side
//! lives there.

use serde_json::{json, Map, Value};

use super::THOUGHT_SIGNATURE_KEY;
use crate::{ContentPart, GenerationParams, ImageSource, Message, MessageRole, ToolCall, ToolSpec};

pub(super) fn build_body(
    system: &str,
    messages: &[Message],
    tools: &[ToolSpec],
    generation: Option<GenerationParams>,
) -> Value {
    let mut body = json!({ "contents": convert_messages(messages) });
    if !system.is_empty() {
        body["systemInstruction"] = json!({ "parts": [{ "text": system }] });
    }
    if let Some(decls) = convert_tools(tools) {
        body["tools"] = json!([{ "functionDeclarations": decls }]);
    }
    if let Some(cfg) = generation_config(generation) {
        body["generationConfig"] = cfg;
    }
    body
}

/// Map generation knobs to Gemini's `generationConfig`. Temperature and
/// `maxOutputTokens` map directly; a thinking budget becomes
/// `thinkingConfig.thinkingBudget` with `includeThoughts` so reasoning parts
/// stream back. Returns `None` when nothing is set.
fn generation_config(generation: Option<GenerationParams>) -> Option<Value> {
    let g = generation?;
    let mut cfg = Map::new();
    if let Some(temp) = g.temperature {
        cfg.insert("temperature".into(), json!(temp));
    }
    if let Some(max) = g.max_output_tokens {
        cfg.insert("maxOutputTokens".into(), json!(max));
    }
    if let Some(budget) = g.thinking_budget_tokens {
        cfg.insert(
            "thinkingConfig".into(),
            json!({ "thinkingBudget": budget, "includeThoughts": true }),
        );
    }
    if cfg.is_empty() {
        None
    } else {
        Some(Value::Object(cfg))
    }
}

/// Map entanglement's `Message` history to Gemini `contents`. User messages →
/// `role: "user"`; assistant → `role: "model"` (text + `functionCall` parts, each
/// restoring its stashed `thoughtSignature`); tool results → a `user` turn of
/// `functionResponse` parts keyed by the call name (#309).
fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        match m.role {
            MessageRole::User => {
                if !m.content.is_empty() {
                    out.push(json!({ "role": "user", "parts": content_parts(&m.content) }));
                }
            }
            MessageRole::Assistant => {
                let mut parts = content_parts(&m.content);
                for tc in &m.tool_calls {
                    parts.push(tool_call_part(tc));
                }
                if !parts.is_empty() {
                    out.push(json!({ "role": "model", "parts": parts }));
                }
            }
            MessageRole::Tool => {
                let name = m.tool_call_id.clone().unwrap_or_default();
                out.push(json!({
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "name": name,
                            "response": { "result": m.text() },
                        }
                    }],
                }));
            }
        }
    }
    out
}

/// One assistant `functionCall` part, restoring the opaque `thoughtSignature`
/// stashed in `provider_meta` so a thinking model accepts the replayed turn (#309).
fn tool_call_part(tc: &ToolCall) -> Value {
    let args: Value = serde_json::from_str(&tc.input).unwrap_or_else(|_| json!({}));
    let mut part = json!({ "functionCall": { "name": tc.name, "args": args } });
    if let Some(sig) = tc
        .provider_meta
        .as_ref()
        .and_then(|m| m.get(THOUGHT_SIGNATURE_KEY))
        .and_then(|v| v.as_str())
    {
        part["thoughtSignature"] = json!(sig);
    }
    part
}

/// Render message content to Gemini parts: text → `{ text }`, image → base64
/// `{ inlineData: { mimeType, data } }` (#221).
fn content_parts(content: &[ContentPart]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text } if text.is_empty() => None,
            ContentPart::Text { text } => Some(json!({ "text": text })),
            ContentPart::Image {
                source: ImageSource::Base64 { media_type, data },
            } => Some(json!({ "inlineData": { "mimeType": media_type, "data": data } })),
        })
        .collect()
}

fn convert_tools(tools: &[ToolSpec]) -> Option<Vec<Value>> {
    if tools.is_empty() {
        return None;
    }
    Some(
        tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": sanitize_schema(&t.schema),
                })
            })
            .collect(),
    )
}

/// Gemini rejects JSON Schema meta-fields (`$schema`, `additionalProperties`,
/// `$ref`, `title`, …) and the union `type: ["string","null"]` form. Strip the
/// unsupported keys recursively, flatten a nullable union to `type` + `nullable`,
/// and drop `required` entries not present in `properties` (Gemini 400s otherwise).
fn sanitize_schema(value: &Value) -> Value {
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
                out.insert(k.clone(), sanitize_schema(v));
            }
            if nullable_from_type {
                out.insert("nullable".into(), Value::Bool(true));
            }
            prune_required(&mut out);
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(sanitize_schema).collect()),
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
mod request_tests {
    use super::*;

    #[test]
    fn body_has_contents_and_omits_empties() {
        let body = build_body("", &[Message::user("hi")], &[], None);
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["contents"][0]["parts"][0]["text"], "hi");
        assert!(body.get("systemInstruction").is_none());
        assert!(body.get("tools").is_none());
        assert!(body.get("generationConfig").is_none());
    }

    #[test]
    fn system_and_tools_and_generation_present_when_set() {
        let spec = ToolSpec::new("greet", "say hi");
        let g = GenerationParams {
            temperature: Some(0.3),
            max_output_tokens: Some(2048),
            thinking_budget_tokens: Some(1024),
        };
        let body = build_body("be nice", &[Message::user("hi")], &[spec], Some(g));
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be nice");
        assert_eq!(body["tools"][0]["functionDeclarations"][0]["name"], "greet");
        assert!((body["generationConfig"]["temperature"].as_f64().unwrap() - 0.3).abs() < 1e-6);
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 2048);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            1024
        );
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );
    }

    #[test]
    fn assistant_tool_call_restores_thought_signature() {
        // A thinking model's function call round-trips its opaque signature (#309):
        // provider_meta on the persisted ToolCall must reappear as thoughtSignature.
        let tc = ToolCall {
            id: "search".into(),
            name: "search".into(),
            input: r#"{"q":"rust"}"#.into(),
            provider_meta: Some(json!({ THOUGHT_SIGNATURE_KEY: "SIG-abc" })),
        };
        let msg = Message::assistant("", vec![tc]);
        let contents = convert_messages(&[msg]);
        let part = &contents[0]["parts"][0];
        assert_eq!(part["functionCall"]["name"], "search");
        assert_eq!(part["functionCall"]["args"]["q"], "rust");
        assert_eq!(part["thoughtSignature"], "SIG-abc");
    }

    #[test]
    fn tool_call_without_meta_omits_signature() {
        let tc = ToolCall::new("t", "t", "{}");
        let part = tool_call_part(&tc);
        assert!(part.get("thoughtSignature").is_none());
    }

    #[test]
    fn tool_result_becomes_function_response_keyed_by_name() {
        let msg = Message::tool("search", "42 results");
        let contents = convert_messages(&[msg]);
        let fr = &contents[0]["parts"][0]["functionResponse"];
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(fr["name"], "search");
        assert_eq!(fr["response"]["result"], "42 results");
    }

    #[test]
    fn image_content_becomes_inline_data() {
        let msg = Message::user_content(vec![ContentPart::image("image/png", "AAAA")]);
        let contents = convert_messages(&[msg]);
        let inline = &contents[0]["parts"][0]["inlineData"];
        assert_eq!(inline["mimeType"], "image/png");
        assert_eq!(inline["data"], "AAAA");
    }

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
}
