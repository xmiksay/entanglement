//! A skill's `lib.rhai` (ADR-0227): the file beside `SKILL.md` that extends
//! what a script can call. Compiled once at skill discovery — any layer,
//! foreign skill dirs included (`root_dir` is pre-resolved by both parsers)
//! — into a static module every `rhai` run registers as `skill::<skill>`, so
//! `skill::git::changed_files()` calls it. Live-reloaded with the skill
//! registry, since a reload re-runs discovery.
//!
//! Only `fn` definitions are allowed: a top-level statement would run on
//! every script launch with nobody having asked for it, so a lib carrying
//! one is rejected whole (with a warning), never partially loaded.

use std::fmt;

/// The `lib.rhai` file name.
pub const LIB_FILE: &str = "lib.rhai";

/// One public function of a lib, as `load_skill` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibFn {
    /// `skill::<skill>::<fn>(<params>)`.
    pub signature: String,
    /// The function's `///`/`/** */` doc comment, comment leaders stripped;
    /// empty when it has none.
    pub doc: String,
}

/// One skill's compiled library.
#[derive(Clone)]
pub struct SkillLib {
    /// Every public function, signature-sorted — what `load_skill` lists.
    pub functions: Vec<LibFn>,
    #[cfg(feature = "rhai")]
    pub(crate) module: rhai::Shared<rhai::Module>,
}

impl fmt::Debug for SkillLib {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SkillLib")
            .field("functions", &self.functions)
            .finish_non_exhaustive()
    }
}

// Two libs are the same when they expose the same functions — the module
// itself has no meaningful equality.
impl PartialEq for SkillLib {
    fn eq(&self, other: &Self) -> bool {
        self.functions == other.functions
    }
}
impl Eq for SkillLib {}

/// The namespace segment a skill's lib registers under.
#[cfg(feature = "rhai")]
pub fn namespace(skill: &str) -> String {
    format!("skill::{}", crate::rhai_ident::rhai_ident(skill))
}

/// Compile `source` into a lib for `skill`. `Err` names why it was refused.
#[cfg(feature = "rhai")]
pub fn compile(skill: &str, source: &str) -> Result<SkillLib, String> {
    let engine = compile_engine();
    let ast = engine
        .compile(source)
        .map_err(|e| format!("does not compile: {e}"))?;
    if !ast.statements().is_empty() {
        return Err(
            "only `fn` definitions are allowed — a top-level statement rejects the \
                    whole lib"
                .to_string(),
        );
    }
    let ns = namespace(skill);
    let docs = super::lib_docs::fn_docs(source);
    let mut functions: Vec<LibFn> = ast
        .iter_functions()
        .filter(|f| f.access == rhai::FnAccess::Public)
        .map(|f| LibFn {
            signature: format!("{ns}::{}({})", f.name, f.params.join(", ")),
            doc: docs
                .iter()
                .find(|d| d.name == f.name && d.arity.is_none_or(|n| n == f.params.len()))
                .map(|d| d.doc.clone())
                .unwrap_or_default(),
        })
        .collect();
    functions.sort_by(|a, b| a.signature.cmp(&b.signature));
    // Encapsulated: a lib function resolves its sibling functions inside the
    // lib, and the engine's bindings (`read`, `http`, `mcp::…`) at call time.
    let mut module = rhai::Module::eval_ast_as_new(rhai::Scope::new(), &ast, &engine)
        .map_err(|e| format!("does not load: {e}"))?;
    // Indexed once here, so a static registration does not re-index a copy
    // on every run.
    module.build_index();
    Ok(SkillLib {
        functions,
        module: rhai::Shared::new(module),
    })
}

/// The engine a lib is parsed and loaded with.
#[cfg(feature = "rhai")]
fn compile_engine() -> rhai::Engine {
    let mut engine = rhai::Engine::new_raw();
    engine.disable_symbol("eval");
    // Rhai halves its parse-depth defaults in debug builds; pin the release
    // values so a file compiles the same in every build.
    engine.set_max_expr_depths(64, 32);
    engine
}

#[cfg(all(test, feature = "rhai"))]
mod tests {
    use super::*;

    fn signatures(lib: &SkillLib) -> Vec<&str> {
        lib.functions.iter().map(|f| f.signature.as_str()).collect()
    }

    #[test]
    fn a_fn_only_lib_compiles_and_lists_public_signatures() {
        let lib = compile(
            "git-tools",
            "fn changed(dir) { dir }\nprivate fn helper() { 1 }\nfn count() { helper() }",
        )
        .unwrap();
        assert_eq!(
            signatures(&lib),
            vec![
                "skill::git_tools::changed(dir)",
                "skill::git_tools::count()"
            ]
        );
    }

    /// ADR-0228: both doc-comment forms reach the listing, leaders stripped;
    /// a plain `//` comment is not a doc comment.
    #[test]
    fn doc_comments_of_both_forms_are_captured() {
        let lib = compile(
            "s",
            "/// Count lines.\n/// Returns an int.\nfn count(text) { 1 }\n\
             /**\n * Greet someone.\n * Politely.\n */\nfn greet(name) { name }\n\
             // not a doc\nfn plain() { 1 }",
        )
        .unwrap();
        let doc = |sig: &str| {
            lib.functions
                .iter()
                .find(|f| f.signature == sig)
                .map(|f| f.doc.clone())
                .unwrap()
        };
        assert_eq!(
            doc("skill::s::count(text)"),
            "Count lines.\nReturns an int."
        );
        assert_eq!(doc("skill::s::greet(name)"), "Greet someone.\nPolitely.");
        assert_eq!(doc("skill::s::plain()"), "");
    }

    /// A file-header block comment and a stray `///` above a non-`fn` line
    /// are ordinary comments — the lib still loads (ADR-0228).
    #[test]
    fn a_header_comment_does_not_reject_the_lib() {
        let lib = compile(
            "s",
            "/** Helpers for s. */\n\n/// Hi.\nfn hi() { 1 }\n/// trailing doc",
        )
        .unwrap();
        assert_eq!(lib.functions[0].doc, "Hi.");
    }

    #[test]
    fn a_top_level_statement_rejects_the_whole_lib() {
        let err = compile("x", "fn ok() { 1 }\nlet leak = read(\"/etc/passwd\");").unwrap_err();
        assert!(err.contains("top-level statement"), "{err}");
    }

    #[test]
    fn a_syntax_error_is_reported_not_panicked() {
        assert!(compile("x", "fn broken( {").is_err());
    }
}
