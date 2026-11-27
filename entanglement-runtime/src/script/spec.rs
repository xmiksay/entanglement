//! The model-facing `rhai` tool spec and its binding reference.
//!
//! Split out of the (grandfathered over-cap) `script.rs` along a natural seam:
//! everything here is *what the model is told*, nothing here runs a script.
//!
//! The spec description carries the **full signature list** ([`reference`]).
//! It used to be a deliberate stub pointing at the embedded `rhai` skill (#619),
//! on the grounds that a per-request re-send of the catalogue was waste — but the
//! tool surface is now advertised universally and cache-stably, so spec *content*
//! is cached, costs nothing per turn, and is the model's primary guidance. The
//! stub's observed failure mode was models skipping (or half-remembering) the
//! `load_skill` step and writing scripts against guessed binding names. The
//! signatures of the host's bindings come from the registered tools
//! themselves ([`ScriptBinding::doc`], #933), so the list names exactly what
//! a run binds.
//!
//! The same reference is appended to an *unknown name* script error
//! ([`binding_hint`]), so a model that still guesses wrong self-corrects from the
//! tool result instead of burning another turn.

use entanglement_core::ToolSpec;
use rhai::EvalAltResult;

use crate::host_ns::ScriptBinding;
use crate::tool_names::RHAI_TOOL;
use crate::tools::ToolRegistry;

/// The reference's opening: what Rhai is and how every binding is checked.
const INTRO: &str = "Rhai is Rust-like (fn, let) but is NOT Rust: no use/crates/std; its own stdlib (strings, arrays, maps, math, loops) is built in; import/eval are disabled. Only the functions below exist; anything else throws.

Every binding call passes the same checks as the equivalent direct tool call (permission mode, approval, and any check the host adds); a refusal throws, catchable with try/catch.

Host bindings:";

/// The library's own `http()` line and the budget rule for every nested call.
const HTTP_AND_BUDGET: &str = r#"  http(method, url) / http(method, url, #{headers: #{...}, body: text-or-map}) -> response body text; a non-2xx status throws; graded as `http(METHOD url)`; credentials for a host come from the user's config, never from the script (an Authorization/Cookie/Host header or user@ in the URL is refused); a map/array body is sent as JSON
  nested calls are clamped to the script's remaining budget (a cut call throws: its outcome is unknown); a call that bounds itself or must not be cut runs to its end."#;

/// Everything after the host bindings: namespaces, input, converters.
const NAMESPACES_AND_REST: &str = r#"Generated namespaces:
  mcp::<server>::<tool>(#{arg: value, ...}) / mcp::<server>::<tool>() -> the MCP tool's text result (parse_json it if JSON); a failure throws. Names map to identifiers: other characters become _, a name not starting with a letter gets a t_ prefix (2fa -> t_2fa), a reserved word a _ suffix — mcp_tools shows each tool's rhai name.
  <host>::<fn>(#{arg: value, ...}) / <host>::<fn>() -> a host-registered namespace; each call dispatches to the backing tool with {"tool": "<fn>", "args": {...}} and is graded as the target function. Available functions depend on the session.
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

Every function is also callable as a method: parse_json(text) is text.parse_json().
Result: the value of the last expression, serialized, with any print(x) output returned above it (a background run streams prints to `poll` as they happen)."#;

/// Every registered tool that binds itself into scripts, name-sorted: what a
/// run registers and what the reference lists.
pub(crate) fn tool_bindings(tools: &ToolRegistry) -> Vec<(String, ScriptBinding)> {
    let mut out: Vec<_> = tools
        .names()
        .into_iter()
        .filter_map(|name| {
            let binding = tools.get(&name)?.script_binding()?;
            Some((name, binding))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Every function a `rhai` script run over `tools` can call,
/// signature-per-line. Ground truth is the registration code —
/// [`super::register_bindings`] binds exactly [`tool_bindings`] plus `http`,
/// [`super::data::register_data_functions`] the converters.
pub(crate) fn reference(tools: &ToolRegistry) -> String {
    let mut out = INTRO.to_string();
    for (_, binding) in tool_bindings(tools) {
        for line in binding.doc.lines() {
            out.push_str("\n  ");
            out.push_str(line);
        }
    }
    format!("{out}\n{HTTP_AND_BUDGET}\n\n{NAMESPACES_AND_REST}")
}

/// The `rhai` tool schema advertised to the model (an executor route, always
/// advertised, ADR-0226), listing the bindings `tools` adds.
pub fn rhai_spec(tools: &ToolRegistry) -> ToolSpec {
    ToolSpec::with_schema(
        RHAI_TOOL,
        format!(
            "Run a Rhai script (https://rhai.rs) in a capability-sandboxed \
             engine — multi-step logic (loops, branching, JSON/YAML parsing) \
             in one call instead of several separate tool calls or shelling \
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
             {{\"days\": 7}}}}.\n\n{}\n\n\
             Load the `rhai` skill for worked examples and detail.",
            reference(tools)
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
                        next operation, after any in-flight binding call \
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
pub(super) fn binding_hint(err: &EvalAltResult, tools: &ToolRegistry) -> Option<String> {
    unknown_name(err).then(|| format!("\n\nAvailable script functions:\n{}", reference(tools)))
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
    use std::borrow::Cow;
    use std::time::{Duration, Instant};

    use rhai::{Dynamic, Engine};
    use tokio::sync::mpsc;

    use super::*;
    use crate::host_ns::ScriptArg;
    use crate::script::data::DATA_FUNCTIONS;
    use crate::script::{register_bindings, result_line, BindingCall};
    use crate::tools::Tool;

    /// A stub tool binding itself as `<name>(path)`.
    struct Bound(&'static str, &'static str);

    #[async_trait::async_trait]
    impl Tool for Bound {
        fn name(&self) -> Cow<'static, str> {
            self.0.into()
        }
        async fn run(&self, _input: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
        fn script_binding(&self) -> Option<ScriptBinding> {
            Some(ScriptBinding {
                overloads: &[&[("path", ScriptArg::Str)]],
                json: false,
                budget_key: None,
                doc: self.1,
            })
        }
    }

    /// The library's own functions: `http` and the pure converters.
    fn library_names() -> Vec<&'static str> {
        let mut names = vec!["http"];
        names.extend(DATA_FUNCTIONS);
        names
    }

    /// A minimal well-typed call for one library function — the probe that
    /// proves the documented name is really registered.
    fn probe(name: &str) -> &'static str {
        match name {
            "http" => r#"http("GET", "https://a.io")"#,
            "parse_json" => r#"parse_json("{}")"#,
            "to_json" => r#"to_json("x")"#,
            "parse_yaml" => r#"parse_yaml("a: 1")"#,
            "to_yaml" => r#"to_yaml("x")"#,
            other => panic!("no probe for `{other}` — add one here and a line in the reference"),
        }
    }

    /// The spec description is the model's only always-present binding
    /// reference, so it must name every function the engine registers.
    #[test]
    fn spec_description_lists_every_library_function() {
        let desc = rhai_spec(&ToolRegistry::new()).description;
        for name in library_names() {
            assert!(
                desc.contains(&format!("{name}(")),
                "spec description never mentions `{name}`"
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
        let schema = rhai_spec(&ToolRegistry::new()).schema;
        for key in ["script", "path", "args", "timeout", "background"] {
            assert!(schema["properties"][key].is_object(), "{key} missing");
        }
        assert_eq!(schema["required"], serde_json::json!([]));
    }

    /// #933: a registered tool's binding joins the host bindings, name-sorted;
    /// a tool without one adds nothing.
    #[test]
    fn registered_bindings_join_the_reference_sorted() {
        let mut tools = ToolRegistry::new();
        tools.register(Bound("zz_put", "zz_put(path) -> stores it"));
        tools.register(Bound("aa_get", "aa_get(path) -> its text\nsecond line"));
        let desc = rhai_spec(&tools).description;
        let (a, z) = (
            desc.find("  aa_get(path)").unwrap(),
            desc.find("  zz_put(path)").unwrap(),
        );
        assert!(a < z && z < desc.find("  http(").unwrap(), "{desc}");
        assert!(desc.contains("\n  second line\n"), "{desc}");
    }

    /// The other direction: nothing in the reference is invented — each
    /// library name resolves against a real engine built by the registration
    /// code.
    #[test]
    fn every_documented_library_function_is_actually_registered() {
        let (tx, mut rx) = mpsc::unbounded_channel::<BindingCall>();
        let mut engine = Engine::new_raw();
        register_bindings(
            &mut engine,
            tx,
            &[],
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

        for name in library_names() {
            if let Err(e) = engine.eval::<Dynamic>(probe(name)) {
                assert!(
                    !matches!(*e, EvalAltResult::ErrorFunctionNotFound(..)),
                    "documented function `{name}` is not registered: {e}"
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
        let mut tools = ToolRegistry::new();
        tools.register(Bound("fetch_doc", "fetch_doc(path) -> its text"));
        let engine = Engine::new_raw();
        let err = engine
            .eval::<Dynamic>(r#"read_file("Cargo.toml")"#)
            .unwrap_err();
        let (line, is_error) = result_line(Ok(Err(err)), &tools);
        assert!(is_error);
        assert!(line.contains("Function not found"), "{line}");
        assert!(line.contains("Available script functions"), "{line}");
        assert!(line.contains("fetch_doc(path)"), "{line}");
        assert!(line.contains("parse_json(text)"), "{line}");
    }

    #[test]
    fn an_unknown_variable_error_carries_the_binding_reference() {
        let engine = Engine::new_raw();
        let err = engine.eval::<Dynamic>("no_such_binding").unwrap_err();
        let (line, _) = result_line(Ok(Err(err)), &ToolRegistry::new());
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
        let (line, _) = result_line(Ok(Err(err)), &ToolRegistry::new());
        assert!(line.contains("Available script functions"), "{line}");
    }

    /// Every other failure keeps the bare message: the catalogue would be noise
    /// where the script's own names were fine.
    #[test]
    fn an_ordinary_script_error_stays_bare() {
        let engine = Engine::new_raw();
        let err = engine.eval::<Dynamic>(r#"throw "boom""#).unwrap_err();
        let (line, is_error) = result_line(Ok(Err(err)), &ToolRegistry::new());
        assert!(is_error);
        assert!(
            !line.contains("Available script functions"),
            "a thrown value got the binding catalogue: {line}"
        );
    }
}
