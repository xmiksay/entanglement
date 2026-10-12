//! ADR-0226's `mcp` envelope, core half. When a round advertised the
//! `mcp {server, tool, args}` kernel spec, an `mcp` call is unwrapped into
//! its inner `mcp__<server>__<tool>` call before any event is emitted, so
//! dispatch, gates, hooks and heads see an ordinary tool call. Only the
//! *events* change: `Context` keeps the call exactly as the model emitted
//! it, because rewriting the model-facing history to the inner name would
//! move the cached prefix (the ADR-0204 probe result ADR-0226 carries
//! forward). Replay rebuilds that emitted call from the carried
//! [`ToolEnvelope`].

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::protocol::ToolEnvelope;
use entanglement_provider::{mcp_tool_name, ToolCall, ToolSpec, MCP_TOOL};

/// Inner names never unwrapped: an envelope inside an envelope.
const RESERVED: [&str; 1] = [MCP_TOOL];

/// A round's calls in dispatch form, plus the envelope of every call that was
/// unwrapped (keyed by call id). Unwrapping happens only when `specs`
/// advertise `mcp`; otherwise an `mcp` call stays an unknown tool.
pub(super) fn unwrap_batch(
    calls: &[ToolCall],
    specs: &[ToolSpec],
) -> (Vec<ToolCall>, HashMap<String, ToolEnvelope>) {
    let mut envelopes = HashMap::new();
    if !specs.iter().any(|s| s.name == MCP_TOOL) {
        return (calls.to_vec(), envelopes);
    }
    let dispatch = calls
        .iter()
        .map(|call| match unwrap_call(call) {
            Some((inner, envelope)) => {
                envelopes.insert(call.id.clone(), envelope);
                inner
            }
            None => call.clone(),
        })
        .collect();
    (dispatch, envelopes)
}

/// Edge rules (ADR-0204's, carried forward): `server` and `tool` non-empty,
/// non-reserved strings; `args` missing → `{}`, a JSON string → parsed (must
/// yield an object), an object → as is. Anything else is left for the runtime
/// to decline with `mcp`'s schema.
fn unwrap_call(call: &ToolCall) -> Option<(ToolCall, ToolEnvelope)> {
    if call.name != MCP_TOOL {
        return None;
    }
    let Value::Object(mut outer) = serde_json::from_str(&call.input).ok()? else {
        return None;
    };
    let string_field = |key: &str| -> Option<String> {
        match outer.get(key) {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            _ => None,
        }
    };
    let server = string_field("server")?;
    let tool = string_field("tool").filter(|t| !RESERVED.contains(&t.as_str()))?;
    let args = match outer.remove("args") {
        None => Value::Object(Map::new()),
        Some(Value::String(raw)) => serde_json::from_str::<Value>(&raw)
            .ok()
            .filter(Value::is_object)?,
        Some(args @ Value::Object(_)) => args,
        Some(_) => return None,
    };
    let inner = ToolCall {
        id: call.id.clone(),
        name: mcp_tool_name(&server, &tool),
        input: serde_json::to_string(&args).ok()?,
        provider_meta: call.provider_meta.clone(),
    };
    let envelope = ToolEnvelope {
        tool: call.name.clone(),
        input: call.input.clone(),
    };
    Some((inner, envelope))
}

/// The call as the model emitted it: rebuilt from `envelope` when the call was
/// unwrapped, else `call` itself.
pub(super) fn emitted_call(call: &ToolCall, envelope: Option<&ToolEnvelope>) -> ToolCall {
    match envelope {
        Some(env) => ToolCall {
            id: call.id.clone(),
            name: env.tool.clone(),
            input: env.input.clone(),
            provider_meta: call.provider_meta.clone(),
        },
        None => call.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, name: &str, input: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            input: input.into(),
            provider_meta: None,
        }
    }

    fn advertised() -> Vec<ToolSpec> {
        vec![ToolSpec::new(MCP_TOOL, "call an MCP tool")]
    }

    /// Unwrap one `mcp` call with `input` under an advertising round;
    /// `Some((inner name, inner input))` when it was unwrapped.
    fn unwrap_one(input: &str) -> Option<(String, String)> {
        let (dispatch, envelopes) = unwrap_batch(&[call("c1", MCP_TOOL, input)], &advertised());
        let env = envelopes.get("c1")?;
        assert_eq!(env.tool, MCP_TOOL);
        assert_eq!(env.input, input, "envelope keeps the raw emitted input");
        Some((dispatch[0].name.clone(), dispatch[0].input.clone()))
    }

    #[test]
    fn object_args_unwrap_to_the_namespaced_inner_call() {
        assert_eq!(
            unwrap_one(r#"{"server":"gh","tool":"search","args":{"q":"x"}}"#),
            Some(("mcp__gh__search".into(), r#"{"q":"x"}"#.into()))
        );
    }

    #[test]
    fn names_are_sanitized_to_the_registered_spelling() {
        assert_eq!(
            unwrap_one(r#"{"server":"my docs","tool":"get.page"}"#),
            Some(("mcp__my_docs__get_page".into(), "{}".into()))
        );
    }

    #[test]
    fn missing_args_become_an_empty_object() {
        assert_eq!(
            unwrap_one(r#"{"server":"gh","tool":"search"}"#),
            Some(("mcp__gh__search".into(), "{}".into()))
        );
    }

    #[test]
    fn stringified_object_args_are_parsed() {
        assert_eq!(
            unwrap_one(r#"{"server":"gh","tool":"search","args":"{\"q\":\"x\"}"}"#),
            Some(("mcp__gh__search".into(), r#"{"q":"x"}"#.into()))
        );
    }

    #[test]
    fn stringified_non_object_or_invalid_args_are_not_unwrapped() {
        assert_eq!(
            unwrap_one(r#"{"server":"gh","tool":"t","args":"[1,2]"}"#),
            None
        );
        assert_eq!(
            unwrap_one(r#"{"server":"gh","tool":"t","args":"not json"}"#),
            None
        );
    }

    #[test]
    fn non_object_args_are_not_unwrapped() {
        for args in ["[]", "3", "null", "true"] {
            let input = format!(r#"{{"server":"gh","tool":"t","args":{args}}}"#);
            assert_eq!(unwrap_one(&input), None, "args = {args}");
        }
    }

    #[test]
    fn bad_server_or_tool_are_not_unwrapped() {
        for input in [
            r#"{"tool":"t","args":{}}"#,
            r#"{"server":"gh","args":{}}"#,
            r#"{"server":"","tool":"t","args":{}}"#,
            r#"{"server":"gh","tool":"","args":{}}"#,
            r#"{"server":7,"tool":"t","args":{}}"#,
            r#"{"server":"gh","tool":"mcp","args":{}}"#,
        ] {
            assert_eq!(unwrap_one(input), None, "input = {input}");
        }
    }

    #[test]
    fn malformed_input_is_not_unwrapped() {
        for input in ["", "not json", "[]", r#""read""#] {
            assert_eq!(unwrap_one(input), None, "input = {input:?}");
        }
    }

    #[test]
    fn unwrap_keeps_id_and_provider_meta() {
        let mut c = call("c9", MCP_TOOL, r#"{"server":"gh","tool":"t","args":{}}"#);
        c.provider_meta = Some(serde_json::json!({"sig": "abc"}));
        let (dispatch, _) = unwrap_batch(std::slice::from_ref(&c), &advertised());
        assert_eq!(dispatch[0].id, "c9");
        assert_eq!(dispatch[0].provider_meta, c.provider_meta);
    }

    #[test]
    fn nothing_unwraps_when_mcp_is_not_advertised() {
        let calls = [call("c1", MCP_TOOL, r#"{"server":"gh","tool":"t"}"#)];
        let specs = [ToolSpec::new("read", "read a file")];
        let (dispatch, envelopes) = unwrap_batch(&calls, &specs);
        assert_eq!(dispatch, calls.to_vec());
        assert!(envelopes.is_empty());
    }

    #[test]
    fn a_batch_unwraps_each_call_independently() {
        let calls = [
            call(
                "a",
                MCP_TOOL,
                r#"{"server":"gh","tool":"t","args":{"x":1}}"#,
            ),
            call("b", "read", r#"{"path":"a"}"#),
            call("c", MCP_TOOL, r#"{"server":"gh"}"#),
        ];
        let (dispatch, envelopes) = unwrap_batch(&calls, &advertised());
        let names: Vec<&str> = dispatch.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["mcp__gh__t", "read", MCP_TOOL]);
        assert_eq!(envelopes.len(), 1);
        assert!(envelopes.contains_key("a"));
    }

    #[test]
    fn emitted_call_round_trips_an_unwrapped_call() {
        let original = call("a", MCP_TOOL, r#"{"server":"gh","tool":"t"}"#);
        let (dispatch, envelopes) = unwrap_batch(std::slice::from_ref(&original), &advertised());
        assert_eq!(emitted_call(&dispatch[0], envelopes.get("a")), original);
        let plain = call("b", "read", "{}");
        assert_eq!(emitted_call(&plain, None), plain);
    }
}
