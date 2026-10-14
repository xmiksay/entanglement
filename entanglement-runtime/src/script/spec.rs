//! The model-facing `rhai` tool spec and its binding reference.
//!
//! Split out of the (grandfathered over-cap) `script.rs` along a natural seam:
//! everything here is *what the model is told*, nothing here runs a script.
//!
//! The spec description carries the **full signature list** ([`BINDING_REFERENCE`]).
//! It used to be a deliberate stub pointing at the embedded `rhai` skill (#619),
//! on the grounds that a per-request re-send of the catalogue was waste — but the
//! tool surface is now advertised universally and cache-stably, so spec *content*
//! is cached, costs nothing per turn, and is the model's primary guidance. The
//! stub's observed failure mode was models skipping (or half-remembering) the
//! `load_skill` step and writing scripts against guessed binding names. The skill
//! (`skills/rhai.md`) keeps the long-form prose and the worked examples; the spec
//! keeps the signatures.
//!
//! The same reference is appended to an *unknown name* script error
//! ([`binding_hint`]), so a model that still guesses wrong self-corrects from the
//! tool result instead of burning another turn.

use entanglement_core::ToolSpec;
use rhai::EvalAltResult;

use crate::tool_names::RHAI_TOOL;

/// Every function a `rhai` script can call, signature-per-line. Ground truth is
/// the registration code — [`super::register_bindings`] for the host bindings
/// and [`super::data::register_data_functions`] for the converters — not the
/// skill text; `spec_description_lists_every_registered_binding` pins the two
/// together.
const BINDING_REFERENCE: &str = r#"Rhai is Rust-like (fn, let) but is NOT Rust: no use/crates/std; its own stdlib (strings, arrays, maps, math, loops) is built in; import/eval are disabled. Only the functions below exist; anything else throws.

Every binding call passes the same checks as the equivalent direct tool call (permission mode, approval, escape-root gate for a path outside the project root, hooks); a refusal throws, catchable with try/catch.

Host bindings:
  read(path) / read(path, offset, limit) -> "{lineno}: {line}" text, NOT parseable as JSON/YAML
  read_raw(path) -> exact file content, no line prefix; use before parse_json/parse_yaml (graded as `read`)
  glob(pattern) -> newline-joined text of matching paths
  grep(pattern) / grep(pattern, path) -> newline-joined text of matching lines
  glob_json(pattern) / glob_json(pattern, exclude) -> #{files: path-string array, notices: string array}; same walk as glob, structured (empty files + a notice is the zero-match shape)
  grep_json(pattern) / grep_json(pattern, path) -> #{matches: {path, lineno, line} record array, notices: string array}; same scan as grep, structured
  edit(path, old, new) / edit(path, old, new, replace_all)
  write(path, content)
  exec(command) / exec(command, args) / exec(command, args, workdir) -> argv exec, no shell; graded as the `call` tool (spelled `exec` because `call` is a reserved Rhai keyword)
  bash(command) / bash(command, workdir) -> sh -c; bound only when the host `bash` tool is enabled, else an unknown-function error
  http(method, url) / http(method, url, #{headers: #{...}, body: text-or-map}) -> response body text; a non-2xx status throws; graded as `http(METHOD url)`; credentials for a host come from the user's config, never from the script (an Authorization/Cookie/Host header or user@ in the URL is refused); a map/array body is sent as JSON
  workdir is what a workdir-scoped permission rule matches; nested calls are clamped to the script's remaining budget (a cut call throws: its outcome is unknown); write/edit are never cut.
  glob_json/grep_json grade as glob/grep — prefer them over hand-splitting glob/grep's newline text.

Generated namespaces:
  mcp::<server>::<tool>(#{arg: value, ...}) / mcp::<server>::<tool>() -> the MCP tool's text result (parse_json it if JSON); a failure throws. Names map to identifiers: other characters become _, a name not starting with a letter gets a t_ prefix (2fa -> t_2fa), a reserved word a _ suffix — mcp_tools shows each tool's rhai name.
  skill::<skill>::<fn>(...) -> a function from a skill's lib.rhai; load_skill lists them.
  Nested rhai, the mcp envelope and the session tools (agent, ask_user, poll, list_*, load_skill, ...) cannot be called from a script.

Input:
  args -> the call's `args` object as a constant map (#{} when none): read args.n or args["n"]; assigning to it throws. Functions defined in the script do not see it — pass values in.

Pure converters — no IO, no permission check:
  parse_json(text) / to_json(value) / parse_yaml(text) / to_yaml(value); parse_* throws on invalid input; JSON/YAML null becomes ().

Chain calls in ONE script — pass one call's result to the next instead of spending a round per call:
  let hits = mcp::docs::search(#{query: "retry policy"}).parse_json();
  let page = mcp::docs::read(#{id: hits[0].id});
  page

Every binding is also callable as a method: read_raw(p).parse_json().
Result: the value of the last expression, serialized, with any print(x) output returned above it (a background run streams prints to `poll` as they happen)."#;

/// The `rhai` tool schema advertised to the model (a kernel tool, ADR-0226).
pub fn rhai_spec() -> ToolSpec {
    ToolSpec::with_schema(
        RHAI_TOOL,
        format!(
            "Run a Rhai script (https://rhai.rs) in a capability-sandboxed \
             engine — multi-step logic (loops, branching, JSON/YAML parsing) \
             in one call instead of several read/edit calls or shelling \
             out to python/node. Prefer it whenever a task needs more than one \
             conditional or a transform over structured data; prefer a direct \
             tool call for a single simple operation.\n\n\
             Pass exactly one of `script` (inline source) or `path` (a predefined \
             .rhai file — a project script, or one a skill ships; the runtime reads \
             it). The launch is permission-graded like any tool (on its `path`); \
             a script in a folder the user trusts runs without asking. When it \
             asks, the user runs it whole, has each command asked, or rejects \
             it. `args` is bound in the script as the constant map `args`, for \
             either kind: {{\"path\": \"scripts/report.rhai\", \"args\": \
             {{\"days\": 7}}}}.\n\n{BINDING_REFERENCE}\n\n\
             Load the `rhai` skill for worked examples and detail."
        ),
        serde_json::json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "description": "Inline Rhai source. The value of its last expression \
                        is returned. Exactly one of script/path."
                },
                "path": {
                    "type": "string",
                    "description": "A .rhai script file to run instead of inline source \
                        (project-relative or absolute). Exactly one of script/path."
                },
                "args": {
                    "type": "object",
                    "description": "Named values the script reads as the constant map \
                        `args` (args.name). Works with script and path."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Wall-clock budget in seconds (default 5, max 30; \
                        with background: true, default 120, max 600)."
                },
                "background": {
                    "type": "boolean",
                    "description": "Run detached and return a handle immediately \
                        instead of the result — join with `poll`, which drains \
                        print output incrementally and reports the final value. \
                        kill via poll is cooperative: the script stops at its \
                        next operation, after any in-flight exec/bash binding \
                        finishes. Default false."
                }
            },
            "required": []
        }),
    )
}

/// The binding reference to append to a failed script's result, or `None` when
/// the failure has nothing to do with a name.
///
/// Only the "guessed the wrong binding" error kinds qualify — an unknown
/// function/method (Rhai's UFCS makes `x.foo()` a function call too) or an
/// unknown variable. Every other failure (a thrown value, a denial, a timeout,
/// an operation-cap trip) already says what went wrong, and repeating the
/// catalogue there would be noise on the model's next read.
pub(super) fn binding_hint(err: &EvalAltResult) -> Option<String> {
    unknown_name(err).then(|| format!("\n\nAvailable script functions:\n{BINDING_REFERENCE}"))
}

fn unknown_name(err: &EvalAltResult) -> bool {
    match err {
        EvalAltResult::ErrorFunctionNotFound(..) | EvalAltResult::ErrorVariableNotFound(..) => true,
        // A failure raised inside a script-defined function is wrapped.
        EvalAltResult::ErrorInFunctionCall(.., inner, _) => unknown_name(inner),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use rhai::{Dynamic, Engine};
    use tokio::sync::mpsc;

    use super::*;
    use crate::script::data::DATA_FUNCTIONS;
    use crate::script::{register_bindings, result_line, BindingCall};
    use crate::tool_names::BINDING_TOOLS;

    /// Every script-facing name the engine registers, derived from the same
    /// tables the registration code reads: the graded host bindings
    /// ([`BINDING_TOOLS`] — `call` binds as `exec`, since `call` is a reserved
    /// Rhai keyword), the never-advertised `read_raw` alias of `read`, `http`,
    /// and the pure converters.
    fn registered_names() -> Vec<&'static str> {
        let mut names: Vec<&'static str> = BINDING_TOOLS
            .into_iter()
            .map(|tool| if tool == "call" { "exec" } else { tool })
            .collect();
        names.push("read_raw");
        names.extend(["glob_json", "grep_json", "http"]);
        names.extend(DATA_FUNCTIONS);
        names
    }

    /// A minimal well-typed call for one binding — the probe that proves the
    /// documented name is really registered. A new binding lands here in the
    /// same change that adds it to [`BINDING_TOOLS`].
    fn probe(name: &str) -> &'static str {
        match name {
            "read" => r#"read("f")"#,
            "read_raw" => r#"read_raw("f")"#,
            "glob" => r#"glob("*")"#,
            "grep" => r#"grep("x")"#,
            "glob_json" => r#"glob_json("*")"#,
            "grep_json" => r#"grep_json("x")"#,
            "edit" => r#"edit("f", "a", "b")"#,
            "write" => r#"write("f", "c")"#,
            "exec" => r#"exec("echo")"#,
            "bash" => r#"bash("echo")"#,
            "http" => r#"http("GET", "https://a.io")"#,
            "parse_json" => r#"parse_json("{}")"#,
            "to_json" => r#"to_json("x")"#,
            "parse_yaml" => r#"parse_yaml("a: 1")"#,
            "to_yaml" => r#"to_yaml("x")"#,
            other => panic!(
                "no probe for binding `{other}` — add one here and a line in BINDING_REFERENCE"
            ),
        }
    }

    /// The spec description is the model's only always-present binding
    /// reference, so it must name every function the engine registers.
    #[test]
    fn spec_description_lists_every_registered_binding() {
        let desc = rhai_spec().description;
        for name in registered_names() {
            assert!(
                desc.contains(&format!("{name}(")),
                "spec description never mentions binding `{name}`"
            );
        }
        // Return-value semantics + print streaming, the two things a model
        // gets wrong without a signature to look at.
        assert!(desc.contains("print(x)"), "print output undocumented");
        assert!(
            desc.contains("last expression"),
            "return value undocumented"
        );
        // The skill pointer survives, demoted to examples-and-detail.
        assert!(desc.contains("`rhai` skill"), "skill pointer lost");
        // #826: without an explicit chained example GLM models call one tool
        // per round even inside a script (ADR-0227).
        assert!(
            desc.contains("Chain calls in ONE script"),
            "chained example lost"
        );
        assert!(
            desc.contains("mcp::<server>::<tool>("),
            "mcp namespace undocumented"
        );
        assert!(
            desc.contains("skill::<skill>::<fn>("),
            "skill namespace undocumented"
        );
        // ADR-0228: the two run kinds and the constant `args` map.
        assert!(desc.contains("exactly one of `script`"), "XOR undocumented");
        assert!(desc.contains("constant map"), "args undocumented");
        assert!(
            desc.contains("trusts runs without asking") && desc.contains("graded like any tool"),
            "trusted folders undocumented"
        );
        let schema = rhai_spec().schema;
        for key in ["script", "path", "args", "timeout", "background"] {
            assert!(schema["properties"][key].is_object(), "{key} missing");
        }
        assert_eq!(schema["required"], serde_json::json!([]));
    }

    /// The other direction: nothing in the reference is invented — each name
    /// resolves against a real engine built by the registration code.
    #[test]
    fn every_documented_binding_is_actually_registered() {
        let (tx, mut rx) = mpsc::unbounded_channel::<BindingCall>();
        let mut engine = Engine::new_raw();
        register_bindings(
            &mut engine,
            tx,
            true,
            true,
            Instant::now(),
            Duration::from_secs(5),
        );
        crate::script::data::register_data_functions(&mut engine);
        // Answer every bridge call so a probe fails only on a missing name.
        let responder = std::thread::spawn(move || {
            while let Some(call) = rx.blocking_recv() {
                let _ = call.reply.send(Ok(String::new()));
            }
        });

        for name in registered_names() {
            if let Err(e) = engine.eval::<Dynamic>(probe(name)) {
                assert!(
                    !matches!(*e, EvalAltResult::ErrorFunctionNotFound(..)),
                    "documented binding `{name}` is not registered: {e}"
                );
            }
        }

        // Dropping the engine drops every closure's sender, ending the loop.
        drop(engine);
        responder.join().unwrap();
    }

    /// The guessed-wrong-binding case: the failing result carries the reference
    /// so the next call can be right without another lookup turn.
    #[test]
    fn an_unknown_function_error_carries_the_binding_reference() {
        let engine = Engine::new_raw();
        let err = engine
            .eval::<Dynamic>(r#"read_file("Cargo.toml")"#)
            .unwrap_err();
        let (line, is_error) = result_line(Ok(Err(err)));
        assert!(is_error);
        assert!(line.contains("Function not found"), "{line}");
        assert!(line.contains("Available script functions"), "{line}");
        assert!(line.contains("read_raw(path)"), "{line}");
        assert!(line.contains("parse_json(text)"), "{line}");
    }

    #[test]
    fn an_unknown_variable_error_carries_the_binding_reference() {
        let engine = Engine::new_raw();
        let err = engine.eval::<Dynamic>("no_such_binding").unwrap_err();
        let (line, _) = result_line(Ok(Err(err)));
        assert!(line.contains("Available script functions"), "{line}");
    }

    /// An unknown name nested inside a script-defined function is wrapped by
    /// Rhai — the hint still fires.
    #[test]
    fn a_wrapped_unknown_function_error_carries_the_binding_reference() {
        let engine = Engine::new_raw();
        let err = engine
            .eval::<Dynamic>(r#"fn helper() { read_file("x") } helper()"#)
            .unwrap_err();
        let (line, _) = result_line(Ok(Err(err)));
        assert!(line.contains("Available script functions"), "{line}");
    }

    /// Every other failure keeps the bare message: the catalogue would be noise
    /// where the script's own names were fine.
    #[test]
    fn an_ordinary_script_error_stays_bare() {
        let engine = Engine::new_raw();
        let err = engine.eval::<Dynamic>(r#"throw "boom""#).unwrap_err();
        let (line, is_error) = result_line(Ok(Err(err)));
        assert!(is_error);
        assert!(
            !line.contains("Available script functions"),
            "a thrown value got the binding catalogue: {line}"
        );
    }
}
