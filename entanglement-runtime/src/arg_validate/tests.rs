use super::*;
use entanglement_core::SessionId;

const MODEL: Reader = Reader::Model {
    schema_delivered: false,
};

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "limit": { "type": "integer" },
            "mode": { "type": "string", "enum": ["a", "b"] }
        },
        "required": ["path"]
    })
}

#[test]
fn valid_call_has_no_violation() {
    assert!(validate(&schema(), r#"{"path":"x.rs"}"#).is_none());
}

#[test]
fn empty_input_treated_as_empty_object() {
    let v = validate(&schema(), "").unwrap();
    assert_eq!(v.missing, vec!["path".to_string()]);
}

#[test]
fn missing_required_is_reported() {
    let v = validate(&schema(), "{}").unwrap();
    assert_eq!(v.lines(), vec!["missing required: path".to_string()]);
}

#[test]
fn unexpected_property_gets_a_closest_match_hint() {
    let v = validate(&schema(), r#"{"path":"x","pathh":"y"}"#).unwrap();
    assert_eq!(v.unexpected.len(), 1);
    assert_eq!(v.unexpected[0].name, "pathh");
    assert_eq!(v.unexpected[0].hint.as_deref(), Some("path"));
    assert!(v.lines()[0].contains("unexpected: pathh — did you mean `path`?"));
}

#[test]
fn additional_properties_true_allows_unlisted_keys() {
    let mut s = schema();
    s.as_object_mut()
        .unwrap()
        .insert("additionalProperties".to_string(), json!(true));
    assert!(validate(&s, r#"{"path":"x","extra":1}"#).is_none());
}

#[test]
fn type_mismatch_is_reported() {
    let v = validate(&schema(), r#"{"path":"x","limit":"not a number"}"#).unwrap();
    assert!(v.type_errors[0].contains("wrong type for `limit`"), "{v:?}");
}

#[test]
fn enum_violation_is_reported() {
    let v = validate(&schema(), r#"{"path":"x","mode":"c"}"#).unwrap();
    assert!(
        v.type_errors[0].contains("invalid value for `mode`"),
        "{v:?}"
    );
}

#[test]
fn malformed_json_short_circuits_other_checks() {
    let v = validate(&schema(), "{not json").unwrap();
    assert!(v.malformed_json.is_some());
    assert!(v.missing.is_empty());
}

#[test]
fn non_object_input_is_reported() {
    let v = validate(&schema(), "[1,2,3]").unwrap();
    assert!(v.not_object);
}

/// A schema with no declared shape (the `Tool::schema` default) makes no
/// promise about the input's format — not even that it's valid JSON — so
/// nothing is a violation against it (#448 regression: a test-double tool
/// with this schema historically received arbitrary non-JSON text).
#[test]
fn permissive_empty_schema_never_flags_a_violation() {
    let permissive = json!({ "type": "object", "properties": {} });
    assert!(validate(&permissive, "not json at all").is_none());
    assert!(validate(&permissive, "[1,2,3]").is_none());
    assert!(validate(&permissive, "").is_none());

    let no_properties_key = json!({ "type": "object" });
    assert!(validate(&no_properties_key, "not json either").is_none());
}

#[test]
fn minimal_example_covers_only_required_fields() {
    let example = minimal_example(&schema());
    let obj = example.as_object().unwrap();
    assert_eq!(obj.len(), 1);
    assert!(obj.contains_key("path"));
}

#[test]
fn decline_text_includes_schema_and_example_when_not_yet_delivered() {
    let spec = ToolSpec::with_schema("read", "read a file", schema());
    let v = validate(&schema(), "{}").unwrap();
    let msg = decline_text(&spec, &v, MODEL);
    assert!(msg.contains("missing required: path"), "{msg}");
    assert!(msg.contains("correct usage"), "{msg}");
    assert!(msg.contains("\"name\": \"read\""), "{msg}");
    assert!(msg.contains("example call"), "{msg}");
}

#[test]
fn an_mcp_tools_example_is_the_mcp_envelope_call() {
    let spec = ToolSpec::with_schema("mcp__gh__read", "read a file", schema());
    let v = validate(&schema(), "{}").unwrap();
    let msg = decline_text(&spec, &v, MODEL);
    let example = msg
        .split("example call (through mcp):\n")
        .nth(1)
        .expect("example section");
    let example: Value = serde_json::from_str(example).expect("example is JSON");
    assert_eq!(
        example,
        json!({"server": "gh", "tool": "read", "args": {"path": "example"}})
    );
    let native = decline_text(
        &ToolSpec::with_schema("read", "read a file", schema()),
        &v,
        MODEL,
    );
    assert!(native.contains("example call:\n"), "{native}");
}

/// A script never sees the model's context, so it always gets the schema,
/// and an MCP example in the rhai form it called the tool by (#844) — the
/// binding's own path, since a server named `a__b` makes the registered
/// `mcp__a__b__x` ambiguous to split.
#[test]
fn a_script_decline_carries_the_schema_and_a_rhai_example() {
    let spec = ToolSpec::with_schema("mcp__a__b__x", "search", schema());
    let v = validate(&schema(), "{}").unwrap();
    let reader = Reader::Script {
        rhai_path: Some("mcp::a__b::x"),
    };
    let msg = decline_text(&spec, &v, reader);
    assert!(msg.contains("correct usage"), "{msg}");
    assert!(!msg.contains("already provided"), "{msg}");
    assert!(!msg.contains("through mcp"), "{msg}");
    assert!(!msg.contains(r#""server""#), "envelope form: {msg}");
    #[cfg(feature = "rhai")]
    assert!(
        msg.contains("example call (in rhai):\nmcp::a__b::x(#{path: \"example\"})"),
        "{msg}"
    );
    let native = decline_text(
        &ToolSpec::with_schema("read", "read a file", schema()),
        &v,
        Reader::Script { rhai_path: None },
    );
    assert!(native.contains("example input:\n"), "{native}");
}

#[test]
fn decline_text_omits_schema_when_already_delivered() {
    let spec = ToolSpec::with_schema("read", "read a file", schema());
    let v = validate(&schema(), "{}").unwrap();
    let msg = decline_text(
        &spec,
        &v,
        Reader::Model {
            schema_delivered: true,
        },
    );
    assert!(msg.contains("missing required: path"), "{msg}");
    assert!(msg.contains("already provided above"), "{msg}");
    assert!(!msg.contains("correct usage"), "{msg}");
}

#[test]
fn loop_breaker_flags_the_second_identical_failure() {
    let lb = LoopBreaker::new();
    let s = SessionId::new("s1");
    assert!(
        !lb.note(&s, "read", r#"{}"#, true),
        "first failure isn't a repeat"
    );
    assert!(
        lb.note(&s, "read", r#"{}"#, true),
        "second identical failure is"
    );
}

#[test]
fn loop_breaker_resets_on_a_different_call() {
    let lb = LoopBreaker::new();
    let s = SessionId::new("s1");
    assert!(!lb.note(&s, "read", r#"{"path":"a"}"#, true));
    assert!(!lb.note(&s, "read", r#"{"path":"b"}"#, true));
}

#[test]
fn loop_breaker_resets_on_a_success_in_between() {
    let lb = LoopBreaker::new();
    let s = SessionId::new("s1");
    assert!(!lb.note(&s, "read", r#"{}"#, true));
    assert!(!lb.note(&s, "read", r#"{}"#, false)); // success in between
    assert!(
        !lb.note(&s, "read", r#"{}"#, true),
        "streak reset, not a repeat yet"
    );
}

#[test]
fn loop_breaker_never_flags_a_success() {
    let lb = LoopBreaker::new();
    let s = SessionId::new("s1");
    assert!(!lb.note(&s, "read", r#"{}"#, false));
    assert!(!lb.note(&s, "read", r#"{}"#, false));
}

#[test]
fn forget_drops_the_session() {
    let lb = LoopBreaker::new();
    let s = SessionId::new("s1");
    lb.note(&s, "read", "{}", true);
    lb.forget(&s);
    assert!(!lb.note(&s, "read", "{}", true), "streak cleared by forget");
}
