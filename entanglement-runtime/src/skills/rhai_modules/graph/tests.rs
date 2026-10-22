//! The discovery-time import graph (ADR-0248): cycles, depth, diamonds,
//! shadowing at every level, and the cost across an import lattice.

use std::path::Path;
use std::time::{Duration, Instant};

use rhai::module_resolvers::StaticModuleResolver;
use rhai::{Engine, Shared};

use super::super::{load_from, RhaiModules};
use crate::layers::Layer;

fn write(dir: &Path, rel: &str, source: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
}

fn project(files: &[(&str, &str)]) -> (RhaiModules, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    for (rel, source) in files {
        write(tmp.path(), rel, source);
    }
    let modules = load_from(&[(Layer::Project, tmp.path().to_path_buf())]);
    (modules, tmp)
}

fn why(modules: &RhaiModules, name: &str) -> String {
    modules.get(name).unwrap().lib.clone().unwrap_err()
}

fn importable(modules: &RhaiModules, name: &str) -> bool {
    modules.get(name).is_some_and(|m| m.lib.is_ok())
}

fn module(modules: &RhaiModules, name: &str) -> Shared<rhai::Module> {
    modules
        .get(name)
        .unwrap()
        .lib
        .as_ref()
        .unwrap()
        .module
        .clone()
}

/// Run `script` on an engine importing the discovered modules by name.
fn run(modules: &RhaiModules, script: &str) -> String {
    try_run(modules, script).unwrap()
}

fn try_run(modules: &RhaiModules, script: &str) -> Result<String, String> {
    let mut resolver = StaticModuleResolver::new();
    for (name, m) in modules.iter() {
        if let Ok(lib) = &m.lib {
            resolver.insert(name.as_str(), (*lib.module).clone());
        }
    }
    let mut engine = Engine::new_raw();
    engine.set_max_call_levels(64);
    engine.set_module_resolver(resolver);
    engine
        .eval::<rhai::Dynamic>(script)
        .map(|d| d.to_string())
        .map_err(|e| e.to_string())
}

#[test]
fn a_three_module_cycle_is_refused_naming_its_path() {
    let (modules, _tmp) = project(&[
        ("a.rhai", "import \"b\" as b;\nfn f() { 1 }"),
        ("b.rhai", "import \"c\" as c;\nfn f() { 1 }"),
        ("c.rhai", "import \"a\" as a;\nfn f() { 1 }"),
        ("user.rhai", "import \"b\" as b;\nfn f() { 1 }"),
        ("free.rhai", "fn f() { 1 }"),
    ]);
    for name in ["a", "b", "c"] {
        assert_eq!(why(&modules, name), "import cycle a → b → c → a");
    }
    assert_eq!(
        why(&modules, "user"),
        "imports \"b\" (project), which is not importable"
    );
    assert!(importable(&modules, "free"));
}

/// Reached from a module outside it, the cycle is named from where it
/// closes — the importer is refused for its dependency, not as a member.
#[test]
fn a_cycle_reached_from_a_non_member_names_only_the_loop() {
    let (modules, _tmp) = project(&[
        ("a.rhai", "import \"b\" as b;\nfn f() { 1 }"),
        ("b.rhai", "import \"c\" as c;\nfn f() { 1 }"),
        ("c.rhai", "import \"b\" as b;\nfn f() { 1 }"),
    ]);
    assert_eq!(why(&modules, "b"), "import cycle b → c → b");
    assert_eq!(why(&modules, "c"), "import cycle b → c → b");
    assert_eq!(
        why(&modules, "a"),
        "imports \"b\" (project), which is not importable"
    );
}

#[test]
fn a_self_import_is_refused() {
    let (modules, _tmp) = project(&[("me.rhai", "import \"me\" as m;\nfn f() { 1 }")]);
    assert_eq!(why(&modules, "me"), "import cycle me → me");
}

/// A diamond's shared module is loaded once: discovery's own copy plus
/// one handle per importer, and calls through both sides reach it.
#[test]
fn a_diamond_shares_one_module_instance() {
    let (modules, _tmp) = project(&[
        (
            "top.rhai",
            "import \"l\" as l;\nimport \"r\" as r;\nfn f() { l::f() + r::f() }",
        ),
        ("l.rhai", "import \"base\" as b;\nfn f() { b::one() }"),
        ("r.rhai", "import \"base\" as b;\nfn f() { b::one() + 1 }"),
        (
            "base.rhai",
            "fn one() { 1 }\nfn unused() { helper() }\nprivate fn helper() { 2 }",
        ),
    ]);
    let base = &modules.get("base").unwrap().lib.as_ref().unwrap().module;
    assert_eq!(
        Shared::strong_count(base),
        3,
        "one instance, held by l and r"
    );
    assert_eq!(modules.get("top").unwrap().imports, ["l", "r"]);
    assert_eq!(run(&modules, r#"import "top" as t; t::f()"#), "3");
}

#[test]
fn an_import_chain_over_the_depth_cap_is_refused() {
    // m0 imports m1 … imports m<n>: m0's chain is n deep.
    let n = super::MAX_IMPORT_DEPTH + 1;
    let files: Vec<(String, String)> = (0..=n)
        .map(|i| {
            let body = if i == n {
                "fn f() { 1 }".to_string()
            } else {
                format!("import \"m{}\" as next;\nfn f() {{ next::f() }}", i + 1)
            };
            (format!("m{i}.rhai"), body)
        })
        .collect();
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let (modules, _tmp) = project(&refs);
    assert!(importable(&modules, "m1"), "exactly at the cap");
    assert!(why(&modules, "m0").contains("over the cap of 8"));
    assert_eq!(run(&modules, r#"import "m1" as m; m::f()"#), "1");
}

/// F1 regression: every module of a level importing all `K` of the next,
/// 9 levels deep. Rhai's `eval_ast_as_new` made each import a sub-module
/// and indexed every sub-module path — `K^depth` entries, seconds at K=6.
#[test]
fn an_import_lattice_loads_in_linear_time() {
    const K: usize = 7;
    const LEVELS: usize = 9;
    let mut files = Vec::new();
    for level in 0..LEVELS {
        for i in 0..K {
            let body = if level + 1 == LEVELS {
                "fn f() { 1 }".to_string()
            } else {
                let imports: String = (0..K)
                    .map(|j| format!("import \"l{}_{j}\" as m{j};\n", level + 1))
                    .collect();
                format!("{imports}fn f() {{ m0::f() + 1 }}")
            };
            files.push((format!("l{level}_{i}.rhai"), body));
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    for (rel, body) in &files {
        write(tmp.path(), rel, body);
    }
    let start = Instant::now();
    let modules = load_from(&[(Layer::Project, tmp.path().to_path_buf())]);
    let took = start.elapsed();
    assert!(took < Duration::from_millis(500), "discovery took {took:?}");
    assert!(importable(&modules, "l0_0"));
    assert_eq!(run(&modules, r#"import "l0_3" as t; t::f()"#), "9");
}

/// A name resolves through the layers at every level: a user module
/// importing `strings` gets the project's copy.
#[test]
fn a_project_shadow_serves_a_lower_layers_importer() {
    let tmp = tempfile::tempdir().unwrap();
    let (user, proj) = (tmp.path().join("user"), tmp.path().join("project"));
    write(
        &user,
        "fmt.rhai",
        "import \"strings\" as s;\nfn who() { s::who() }",
    );
    write(&user, "strings.rhai", "fn who() { \"user\" }");
    write(&proj, "strings.rhai", "fn who() { \"project\" }");
    let modules = load_from(&[(Layer::User, user), (Layer::Project, proj)]);
    assert_eq!(modules.get("strings").unwrap().layer, Layer::Project);
    assert_eq!(run(&modules, r#"import "fmt" as f; f::who()"#), "project");
    assert_eq!(Shared::strong_count(&module(&modules, "strings")), 3);
}

/// A broken shadow breaks its importers, naming the dependency's layer.
#[test]
fn a_broken_shadow_refuses_its_importers() {
    let tmp = tempfile::tempdir().unwrap();
    let (user, proj) = (tmp.path().join("user"), tmp.path().join("project"));
    write(
        &user,
        "fmt.rhai",
        "import \"strings\" as s;\nfn who() { s::who() }",
    );
    write(&user, "strings.rhai", "fn who() { 1 }");
    write(&proj, "strings.rhai", "print(1);");
    let modules = load_from(&[(Layer::User, user), (Layer::Project, proj)]);
    assert_eq!(
        why(&modules, "fmt"),
        "imports \"strings\" (project), which is not importable"
    );
    let listing = modules.listing().unwrap();
    assert!(
        listing.contains("fmt (user; imports strings) — not importable"),
        "{listing}"
    );
}

#[test]
fn a_missing_alias_less_or_non_literal_import_is_refused() {
    let (modules, _tmp) = project(&[
        ("lost.rhai", "import \"nowhere\" as n;\nfn f() { 1 }"),
        ("bad.rhai", "import \"../x\" as n;\nfn f() { 1 }"),
        ("dyn.rhai", "import name() as n;\nfn name() { \"a\" }"),
        ("bare.rhai", "import \"lost\";\nfn f() { 1 }"),
    ]);
    assert_eq!(
        why(&modules, "lost"),
        "imports \"nowhere\", which is no module"
    );
    assert!(why(&modules, "bad").contains("not a module name"));
    assert!(why(&modules, "dyn").contains("literal module name"));
    assert!(why(&modules, "bare").contains("needs `as <alias>`"));
}

/// F5: an `import` in a function body would skip this graph — no listing,
/// no cycle check, a failure only at call time — so it refuses the module.
#[test]
fn an_import_inside_a_module_function_is_refused() {
    let (modules, _tmp) = project(&[
        ("inner.rhai", "fn one() { 1 }"),
        (
            "lazy.rhai",
            "fn two() { import \"inner\" as i; i::one() + 1 }",
        ),
        (
            "deep.rhai",
            "fn f(x) { if x { import \"inner\" as i; return i::one(); } 0 }",
        ),
    ]);
    assert!(why(&modules, "lazy").contains("inside a function"));
    assert!(why(&modules, "deep").contains("inside a function"));
    assert!(importable(&modules, "inner"));
}

/// Discovery re-runs on a reload: breaking a cycle's edge makes its
/// modules importable, closing it again refuses them.
#[test]
fn rediscovery_tracks_a_cycle_breaking_and_closing() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = || vec![(Layer::Project, tmp.path().to_path_buf())];
    write(
        tmp.path(),
        "a.rhai",
        "import \"b\" as b;\nfn f() { b::f() }",
    );
    write(tmp.path(), "b.rhai", "import \"a\" as a;\nfn f() { 1 }");
    assert!(!importable(&load_from(&dir()), "a"));
    write(tmp.path(), "b.rhai", "fn f() { 1 }");
    let fixed = load_from(&dir());
    assert!(importable(&fixed, "a") && importable(&fixed, "b"));
    write(tmp.path(), "b.rhai", "import \"a\" as a;\nfn f() { 1 }");
    assert!(!importable(&load_from(&dir()), "b"));
}

/// An importing module (built by `with_imports`) keeps its private
/// functions private: not callable through its alias, nor by an importer
/// through the imported module's alias.
#[test]
fn an_importing_modules_private_fns_stay_private() {
    let (modules, _tmp) = project(&[
        ("base.rhai", "fn one() { 1 }"),
        (
            "p.rhai",
            "import \"base\" as b;\nfn open() { secret() }\nprivate fn secret() { b::one() + 41 }",
        ),
        (
            "q.rhai",
            "import \"p\" as p;\nfn reach() { p::secret() }\nfn ok() { p::open() }",
        ),
    ]);
    assert!(importable(&modules, "q"), "{}", why(&modules, "q"));
    assert_eq!(run(&modules, r#"import "p" as p; p::open()"#), "42");
    assert!(try_run(&modules, r#"import "p" as p; p::secret()"#).is_err());
    assert_eq!(run(&modules, r#"import "q" as q; q::ok()"#), "42");
    let err = try_run(&modules, r#"import "q" as q; q::reach()"#).unwrap_err();
    assert!(err.contains("secret"), "{err}");
}

/// An importing module's functions see the module's own functions: a
/// private helper, recursion, and a closure over a private function.
#[test]
fn an_importing_module_sees_its_own_functions() {
    let (modules, _tmp) = project(&[
        ("base.rhai", "fn one() { 1 }"),
        (
            "m.rhai",
            "import \"base\" as b;\n\
             fn helped(x) { twice(x) + b::one() }\n\
             fn fact(n) { if n <= 1 { 1 } else { n * fact(n - 1) } }\n\
             fn adder() { |x| twice(x) }\n\
             private fn twice(x) { x * 2 }",
        ),
    ]);
    assert_eq!(run(&modules, r#"import "m" as m; m::helped(20)"#), "41");
    assert_eq!(run(&modules, r#"import "m" as m; m::fact(10)"#), "3628800");
    assert_eq!(
        run(
            &modules,
            r#"import "m" as m; let f = m::adder(); f.call(21)"#
        ),
        "42"
    );
}

/// F3: a typed method is refused in every module — with or without imports
/// — rather than silently uncallable once the module imports.
#[test]
fn a_typed_method_refuses_any_module() {
    let (modules, _tmp) = project(&[
        ("base.rhai", "fn one() { 1 }"),
        ("plain.rhai", "fn int.dbl() { this * 2 }\nfn f() { 1 }"),
        (
            "importing.rhai",
            "import \"base\" as b;\nfn int.dbl() { this * 2 }",
        ),
    ]);
    for name in ["plain", "importing"] {
        let why = why(&modules, name);
        assert!(why.contains("typed method (`fn i64.dbl`)"), "{name}: {why}");
    }
    let listing = modules.listing().unwrap();
    assert!(!listing.contains("\n    dbl("), "{listing}");
}

/// Depth is the deepest import, not the first: `top` imports a leaf and a
/// chain 8 deep, so its own chain is 9 — over the cap.
#[test]
fn import_depth_is_the_deepest_import() {
    let mut files: Vec<(String, String)> = vec![
        ("leaf.rhai".into(), "fn f() { 1 }".into()),
        (
            "top.rhai".into(),
            "import \"c0\" as c;\nimport \"leaf\" as l;\nfn f() { c::f() }".into(),
        ),
    ];
    for i in 0..=8 {
        let body = if i == 8 {
            "fn f() { 1 }".to_string()
        } else {
            format!("import \"c{}\" as n;\nfn f() {{ n::f() }}", i + 1)
        };
        files.push((format!("c{i}.rhai"), body));
    }
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let (modules, _tmp) = project(&refs);
    assert!(importable(&modules, "c0"));
    assert!(why(&modules, "top").contains("9 deep, over the cap of 8"));
}

#[test]
fn a_repeated_alias_refuses_the_module() {
    let (modules, _tmp) = project(&[
        ("one.rhai", "fn f() { 1 }"),
        ("two.rhai", "fn f() { 2 }"),
        (
            "dup.rhai",
            "import \"one\" as x;\nimport \"two\" as x;\nfn f() { x::f() }",
        ),
        (
            "same.rhai",
            "import \"one\" as x;\nimport \"one\" as y;\nfn f() { x::f() + y::f() }",
        ),
    ]);
    assert_eq!(why(&modules, "dup"), "alias `x` is imported twice");
    assert_eq!(run(&modules, r#"import "same" as s; s::f()"#), "2");
}
