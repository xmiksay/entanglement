//! `import` through the run's resolver, on the sandboxed engine — and the
//! built-in `json_schema` module run for real.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rhai::{Dynamic, Engine};

use super::super::{configure_engine, data};
use super::Resolver;
use crate::layers::Layer;
use crate::skills::rhai_modules::load_from;
use crate::skills::RhaiModules;

/// The run's engine minus the bridge bindings: sandbox, converters, and a
/// resolver over the built-ins plus `files` as a project module dir.
fn engine_with(files: &[(&str, &str)]) -> (Engine, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    for (rel, source) in files {
        let path = tmp.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    let modules = load_from(&[(Layer::Project, tmp.path().to_path_buf())]);
    (engine_over(modules), tmp)
}

fn engine_over(modules: RhaiModules) -> Engine {
    let mut engine = Engine::new_raw();
    configure_engine(
        &mut engine,
        Duration::from_secs(5),
        Instant::now(),
        |_| {},
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    data::register_data_functions(&mut engine);
    engine.set_module_resolver(Resolver(Arc::new(modules)));
    engine
}

fn eval(engine: &Engine, script: &str) -> Result<String, String> {
    engine
        .eval::<Dynamic>(script)
        .map(|d| d.to_string())
        .map_err(|e| e.to_string())
}

#[test]
fn an_unknown_or_bad_name_is_refused() {
    let (engine, _tmp) = engine_with(&[]);
    let err = eval(&engine, r#"import "nope" as m; 1"#).unwrap_err();
    assert!(err.contains("no such module"), "{err}");
    assert!(err.contains("load_skill rhai"), "{err}");
    for bad in [
        "../etc/passwd",
        "/abs/x",
        "json_schema.rhai",
        "Json",
        "a//b",
    ] {
        let err = eval(&engine, &format!(r#"import "{bad}" as m; 1"#)).unwrap_err();
        assert!(err.contains("not a module name"), "{bad}: {err}");
    }
}

/// A name never reaches a file: a `.rhai` on disk beside the module dir is
/// unreachable by any spelling.
#[test]
fn a_file_outside_the_discovered_set_is_unreachable() {
    let (engine, tmp) = engine_with(&[("ok.rhai", "fn f() { 1 }")]);
    std::fs::write(tmp.path().join("../stray.rhai"), "fn f() { 2 }").unwrap();
    assert_eq!(eval(&engine, r#"import "ok" as m; m::f()"#).unwrap(), "1");
    assert!(eval(&engine, r#"import "stray" as m; m::f()"#).is_err());
}

#[test]
fn a_project_module_shadows_the_built_in_and_slash_names_import() {
    let (engine, _tmp) = engine_with(&[
        ("json_schema.rhai", "fn validate(v, s) { \"mine\" }"),
        ("net/util.rhai", "fn twice(x) { x * 2 }"),
    ]);
    let out = eval(
        &engine,
        r#"import "json_schema" as js; import "net/util" as u; js::validate(1, #{}) + u::twice(21)"#,
    );
    assert_eq!(out.unwrap(), "mine42");
}

#[test]
fn a_rejected_module_is_not_importable() {
    let (engine, _tmp) = engine_with(&[("bad.rhai", "let leak = 1;\nfn f() { 1 }")]);
    let err = eval(&engine, r#"import "bad" as m; m::f()"#).unwrap_err();
    assert!(err.contains("not importable"), "{err}");
}

/// A skill lib has no top-level import, but an `import` inside one of its
/// functions resolves per run through the run's resolver (ADR-0248 §3).
#[test]
fn a_skill_lib_function_imports_through_the_runs_resolver() {
    let (mut engine, _tmp) = engine_with(&[]);
    let lib = crate::skills::lib_rhai::compile(
        "s",
        r#"fn ok() { import "json_schema" as js; js::validate(1, #{type: "integer"}).len() }"#,
    )
    .unwrap();
    engine.register_static_module("skill::s", lib.module.clone());
    assert_eq!(eval(&engine, "skill::s::ok()").unwrap(), "0");
}

/// Module code resolves the engine's functions at call time — the hook a
/// real run's bindings (and so their grading) hang off.
#[test]
fn a_module_function_calls_the_engines_bindings() {
    let (mut engine, _tmp) = engine_with(&[("peek.rhai", r#"fn peek(p) { read(p) }"#)]);
    engine.register_fn("read", |p: &str| format!("read:{p}"));
    let out = eval(&engine, r#"import "peek" as m; m::peek("a.txt")"#);
    assert_eq!(out.unwrap(), "read:a.txt");
}

/// A module's top-level import was resolved at discovery: calling into
/// it costs no `import` at run time, however often. The run's own
/// `import`s are capped, one inside a function counting on every call.
#[test]
fn module_imports_are_settled_and_the_runs_imports_are_capped() {
    let (engine, _tmp) = engine_with(&[
        ("inner.rhai", "fn one() { 1 }"),
        (
            "outer.rhai",
            "import \"inner\" as i;\nfn two() { i::one() + 1 }",
        ),
    ]);
    let looped = r#"import "outer" as o; let n = 0; for i in 0..100 { n += o::two(); } n"#;
    assert_eq!(eval(&engine, looped).unwrap(), "200");
    let max = super::super::MAX_MODULES;
    let ok = format!("{}1", r#"import "inner" as i; "#.repeat(max));
    assert_eq!(eval(&engine, &ok).unwrap(), "1");
    let over = format!("{}1", r#"import "inner" as i; "#.repeat(max + 1));
    let err = eval(&engine, &over).unwrap_err();
    assert!(err.to_lowercase().contains("module"), "{err}");
    let per_call =
        r#"fn f() { import "inner" as i; i::one() } let n = 0; for i in 0..100 { n += f(); } n"#;
    assert!(
        eval(&engine, per_call).is_err(),
        "an import per call is capped"
    );
}

/// Run `js::validate(<value>, <schema>)` on JSON literals.
fn validate(value: &str, schema: &str) -> Vec<String> {
    let engine = engine_over(load_from(&[]));
    let script = format!(
        "import \"json_schema\" as js; js::validate(parse_json({value:?}), parse_json({schema:?}))"
    );
    let out = engine.eval::<rhai::Array>(&script).unwrap();
    out.into_iter().map(|d| d.to_string()).collect()
}

#[test]
fn json_schema_accepts_a_valid_value() {
    let schema = r#"{"type":"object","required":["name","tags"],"additionalProperties":false,
        "properties":{"name":{"type":"string","minLength":1,"maxLength":10},
        "age":{"type":"integer","minimum":0,"maximum":150},
        "score":{"type":["number","null"]},
        "tags":{"type":"array","items":{"enum":["a","b"]},"minItems":1,"maxItems":2},
        "kind":{"const":{"v":[1,2]}}}}"#;
    let value = r#"{"name":"x","age":3.0,"score":null,"tags":["a"],"kind":{"v":[1,2.0]}}"#;
    assert_eq!(validate(value, schema), Vec::<String>::new());
}

#[test]
fn json_schema_reports_each_problem_with_its_path() {
    let schema = r#"{"type":"object","required":["name","id"],"additionalProperties":false,
        "properties":{"name":{"type":"string","minLength":3},
        "age":{"type":"integer","minimum":0},
        "tags":{"type":"array","items":{"type":"string"},"maxItems":2},
        "kind":{"enum":["a","b"]},"v":{"const":1}}}"#;
    let value = r#"{"name":"ab","age":-1.5,"tags":["x",2,"z"],"kind":"c","v":2,"a/b":0}"#;
    let mut got = validate(value, schema);
    got.sort();
    let mut want = vec![
        "/: missing required property `id`",
        "/name: length 2 is shorter than minLength 3",
        "/age: expected type integer, got number",
        "/tags: 3 items, more than maxItems 2",
        "/tags/1: expected type string, got integer",
        "/kind: value is not one of the enum values",
        "/v: value does not equal the const value",
        "/a~1b: additional property is not allowed",
    ];
    want.sort();
    assert_eq!(got, want);
}

#[test]
fn json_schema_type_lists_numbers_and_bool_schemas() {
    assert_eq!(
        validate("true", r#"{"type":["string","null"]}"#),
        ["/: expected type string or null, got boolean"]
    );
    assert!(validate("2.5", r#"{"type":"number","maximum":3}"#).is_empty());
    assert_eq!(
        validate("4", r#"{"type":"number","maximum":3}"#),
        ["/: 4 is greater than the maximum 3"]
    );
    assert!(validate("[1]", r#"{"items":true}"#).is_empty());
    assert_eq!(
        validate("[1]", r#"{"items":false}"#),
        ["/0: no value is allowed here"]
    );
    assert_eq!(
        validate("1", r#""string""#),
        ["/: schema must be a map or a bool, got string"]
    );
}

/// Each bound is inclusive: equal passes, one past fails.
#[test]
fn json_schema_bounds_are_inclusive() {
    for (value, schema) in [
        ("3", r#"{"minimum":3,"maximum":3}"#),
        (r#""abc""#, r#"{"minLength":3,"maxLength":3}"#),
        ("[1,2]", r#"{"minItems":2,"maxItems":2}"#),
    ] {
        assert!(validate(value, schema).is_empty(), "{value} {schema}");
    }
    assert_eq!(
        validate("2", r#"{"minimum":3}"#),
        ["/: 2 is less than the minimum 3"]
    );
    assert_eq!(
        validate(r#""abcd""#, r#"{"maxLength":3}"#),
        ["/: length 4 is longer than maxLength 3"]
    );
    assert_eq!(
        validate(r#""ab""#, r#"{"minLength":3}"#),
        ["/: length 2 is shorter than minLength 3"]
    );
    assert_eq!(
        validate("[1,2,3]", r#"{"maxItems":2}"#),
        ["/: 3 items, more than maxItems 2"]
    );
    assert_eq!(
        validate("[1]", r#"{"minItems":2}"#),
        ["/: 1 items, fewer than minItems 2"]
    );
}

/// Lengths count characters, not bytes.
#[test]
fn json_schema_lengths_count_characters() {
    assert!(validate(r#""é😀""#, r#"{"minLength":2,"maxLength":2}"#).is_empty());
    assert_eq!(
        validate(r#""é😀""#, r#"{"minLength":3}"#),
        ["/: length 2 is shorter than minLength 3"]
    );
}

/// `enum`/`const` equality is deep and exact: an extra key or item differs.
#[test]
fn json_schema_enum_and_const_need_exact_structure() {
    let extra_key = r#"{"const":{"a":1}}"#;
    assert!(validate(r#"{"a":1}"#, extra_key).is_empty());
    assert_eq!(
        validate(r#"{"a":1,"b":2}"#, extra_key),
        ["/: value does not equal the const value"]
    );
    assert_eq!(
        validate(r#"{"a":1}"#, r#"{"const":{"a":1,"b":2}}"#),
        ["/: value does not equal the const value"]
    );
    assert_eq!(
        validate("[1,2,3]", r#"{"enum":[[1,2]]}"#),
        ["/: value is not one of the enum values"]
    );
    assert_eq!(
        validate("[1]", r#"{"enum":[[1,2]]}"#),
        ["/: value is not one of the enum values"]
    );
}

/// A key's `~` and `/` are escaped in the path (`~0`, `~1`).
#[test]
fn json_schema_escapes_tilde_in_paths() {
    assert_eq!(
        validate(
            r#"{"a~b":1}"#,
            r#"{"properties":{"a~b":{"type":"string"}}}"#
        ),
        ["/a~0b: expected type string, got integer"]
    );
}

/// F6: a supported keyword of the wrong type is reported, not ignored.
#[test]
fn json_schema_reports_a_malformed_keyword() {
    assert_eq!(
        validate("5", r#"{"minimum":"0"}"#),
        ["/: schema keyword `minimum` must be a number"]
    );
    assert_eq!(
        validate(
            "{}",
            r#"{"properties":{"x":{"maxItems":true}},"required":"x"}"#
        ),
        ["/: schema keyword `required` must be an array"]
    );
    assert_eq!(
        validate(r#"{"x":[]}"#, r#"{"properties":{"x":{"maxItems":true}}}"#),
        ["/x: schema keyword `maxItems` must be a number"]
    );
    for (schema, want) in [
        (
            r#"{"enum":"a"}"#,
            "/: schema keyword `enum` must be an array",
        ),
        (
            r#"{"properties":[]}"#,
            "/: schema keyword `properties` must be an object",
        ),
        (
            r#"{"additionalProperties":"no"}"#,
            "/: schema keyword `additionalProperties` must be a boolean",
        ),
        (
            r#"{"required":["a",1]}"#,
            "/: schema keyword `required` must list property names (strings)",
        ),
        (
            r#"{"type":"text"}"#,
            "/: schema keyword `type` must be a type name or an array of them",
        ),
        (
            r#"{"type":["string",3]}"#,
            "/: schema keyword `type` must be a type name or an array of them",
        ),
        (
            r#"{"type":[]}"#,
            "/: schema keyword `type` must be a type name or an array of them",
        ),
        (
            r#"{"items":[{"type":"string"}]}"#,
            "/: schema keyword `items` must be an object or a boolean",
        ),
    ] {
        assert_eq!(validate("{}", schema), [want], "{schema}");
    }
}

/// The module runs on the caller's engine, so its limits bind it too.
#[test]
fn json_schema_respects_the_sandbox_limits() {
    let mut engine = engine_over(load_from(&[]));
    engine.set_max_operations(500);
    let value = format!("[{}]", vec!["1"; 200].join(","));
    let script = format!(
        "import \"json_schema\" as js; js::validate(parse_json({value:?}), #{{items: #{{type: \"integer\"}}}})"
    );
    let err = engine.eval::<Dynamic>(&script).unwrap_err().to_string();
    assert!(err.to_lowercase().contains("operations"), "{err}");
}
