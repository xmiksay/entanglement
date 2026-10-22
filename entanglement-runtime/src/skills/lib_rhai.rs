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
use std::path::Path;

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

/// One skill's compiled library — also the compiled form of an importable
/// module (ADR-0248).
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

/// Load and compile `<root_dir>/lib.rhai`, or `None` when there is none, it
/// fails to compile, or it carries a top-level statement (each warned).
#[cfg(feature = "rhai")]
pub(super) fn load(skill: &str, root_dir: Option<&Path>) -> Option<SkillLib> {
    let path = root_dir?.join(LIB_FILE);
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(skill, path = %path.display(), "skipping unreadable skill lib.rhai: {e}");
            return None;
        }
    };
    match compile(skill, &source) {
        Ok(lib) => Some(lib),
        Err(why) => {
            tracing::warn!(skill, path = %path.display(), "skipping skill lib.rhai: {why}");
            None
        }
    }
}

#[cfg(not(feature = "rhai"))]
pub(super) fn load(_skill: &str, _root_dir: Option<&Path>) -> Option<SkillLib> {
    None
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
    build(&format!("{}::", namespace(skill)), source, &ast, &[])
}

/// The engine a lib or module (ADR-0248) is parsed and loaded with.
#[cfg(feature = "rhai")]
pub(crate) fn compile_engine() -> rhai::Engine {
    let mut engine = rhai::Engine::new_raw();
    engine.disable_symbol("eval");
    // Rhai halves its parse-depth defaults in debug builds; pin the release
    // values so a file compiles the same in every build.
    engine.set_max_expr_depths(64, 32);
    engine
}

/// A module's top-level imports (ADR-0248): `(alias, module)`.
#[cfg(feature = "rhai")]
pub(crate) type Imports = [(String, rhai::Shared<rhai::Module>)];

/// Load a checked `ast` of `source` as a module, listing each public
/// function as `<prefix><fn>(…)`; `imports` are the already-loaded modules
/// its top-level `import`s name.
#[cfg(feature = "rhai")]
pub(crate) fn build(
    prefix: &str,
    source: &str,
    ast: &rhai::AST,
    imports: &Imports,
) -> Result<SkillLib, String> {
    let docs = super::lib_docs::fn_docs(source);
    let mut functions: Vec<LibFn> = ast
        .iter_functions()
        .filter(|f| f.access == rhai::FnAccess::Public)
        .map(|f| LibFn {
            signature: format!("{prefix}{}({})", f.name, f.params.join(", ")),
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
    let mut module = if imports.is_empty() {
        rhai::Module::eval_ast_as_new(rhai::Scope::new(), ast, &compile_engine())
            .map_err(|e| format!("does not load: {e}"))?
    } else {
        with_imports(ast, imports)
    };
    // Indexed once here, so neither a static registration nor an `import`
    // re-indexes a copy on every run.
    module.build_index();
    Ok(SkillLib {
        functions,
        module: rhai::Shared::new(module),
    })
}

/// The module `eval_ast_as_new` would build, minus its sub-modules: it
/// makes every import a sub-module, and indexing recurses through every
/// sub-module path — exponential across an import lattice even when the
/// shared modules are one instance. Here the imports live only in each
/// function's encapsulated environ, which is all `alias::f()` needs.
#[cfg(feature = "rhai")]
fn with_imports(ast: &rhai::AST, imports: &Imports) -> rhai::Module {
    let env = rhai::Shared::new(rhai::EncapsulatedEnviron {
        lib: std::iter::once(ast.shared_lib().clone()).collect(),
        imports: imports
            .iter()
            .map(|(alias, module)| (alias.as_str().into(), module.clone()))
            .collect(),
        // A module has no top-level `let`/`const`, so no global constants.
        constants: None,
    });
    let mut module = rhai::Module::new();
    for f in ast
        .iter_fn_def()
        .filter(|f| f.access == rhai::FnAccess::Public)
    {
        // Script functions are looked up by name and arity; the parameter
        // types only size the registration.
        let arity = vec![std::any::TypeId::of::<rhai::Dynamic>(); f.params.len()];
        let func = rhai::RhaiFunc::Script {
            fn_def: f.clone(),
            env: Some(env.clone()),
        };
        rhai::FuncRegistration::new(f.name.as_str()).set_into_module_raw(&mut module, arity, func);
    }
    module
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

    #[test]
    fn load_reads_lib_rhai_beside_skill_md_and_skips_a_bad_one() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load("s", Some(dir.path())).is_none(), "no lib.rhai, no lib");
        std::fs::write(dir.path().join(LIB_FILE), "fn hi() { \"hi\" }").unwrap();
        assert_eq!(
            signatures(&load("s", Some(dir.path())).unwrap()),
            vec!["skill::s::hi()"]
        );
        std::fs::write(dir.path().join(LIB_FILE), "print(1);").unwrap();
        assert!(load("s", Some(dir.path())).is_none());
        assert!(load("s", None).is_none(), "a built-in has no directory");
    }
}
