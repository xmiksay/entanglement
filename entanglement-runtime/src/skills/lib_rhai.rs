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

/// One skill's compiled library.
#[derive(Clone)]
pub struct SkillLib {
    /// `skill::<skill>::<fn>(<params>)` per public function, name-sorted —
    /// what `load_skill` lists.
    pub signatures: Vec<String>,
    #[cfg(feature = "rhai")]
    pub(crate) module: rhai::Shared<rhai::Module>,
}

impl fmt::Debug for SkillLib {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SkillLib")
            .field("signatures", &self.signatures)
            .finish_non_exhaustive()
    }
}

// Two libs are the same when they expose the same functions — the module
// itself has no meaningful equality.
impl PartialEq for SkillLib {
    fn eq(&self, other: &Self) -> bool {
        self.signatures == other.signatures
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
    let mut engine = rhai::Engine::new_raw();
    engine.disable_symbol("eval");
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
    let mut signatures: Vec<String> = ast
        .iter_functions()
        .filter(|f| f.access == rhai::FnAccess::Public)
        .map(|f| format!("{ns}::{}({})", f.name, f.params.join(", ")))
        .collect();
    signatures.sort();
    // Encapsulated: a lib function resolves its sibling functions inside the
    // lib, and the engine's bindings (`read`, `http`, `mcp::…`) at call time.
    let module = rhai::Module::eval_ast_as_new(rhai::Scope::new(), &ast, &engine)
        .map_err(|e| format!("does not load: {e}"))?;
    Ok(SkillLib {
        signatures,
        module: rhai::Shared::new(module),
    })
}

#[cfg(all(test, feature = "rhai"))]
mod tests {
    use super::*;

    #[test]
    fn a_fn_only_lib_compiles_and_lists_public_signatures() {
        let lib = compile(
            "git-tools",
            "fn changed(dir) { dir }\nprivate fn helper() { 1 }\nfn count() { helper() }",
        )
        .unwrap();
        assert_eq!(
            lib.signatures,
            vec![
                "skill::git_tools::changed(dir)",
                "skill::git_tools::count()"
            ]
        );
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
            load("s", Some(dir.path())).unwrap().signatures,
            vec!["skill::s::hi()"]
        );
        std::fs::write(dir.path().join(LIB_FILE), "print(1);").unwrap();
        assert!(load("s", Some(dir.path())).is_none());
        assert!(load("s", None).is_none(), "a built-in has no directory");
    }
}
