use super::*;

fn write(dir: &Path, rel: &str, source: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
}

fn layer_of(modules: &RhaiModules, name: &str) -> Layer {
    modules.get(name).unwrap().layer
}

#[test]
fn names_are_bare_lowercase_segments() {
    for ok in ["json_schema", "a/b", "x1/y_2/z"] {
        assert!(valid_name(ok), "{ok}");
    }
    for bad in [
        "", "../x", "/abs", "a//b", "a/", "x.rhai", "Upper", "a-b", "./x", "a\\b", "é",
    ] {
        assert!(!valid_name(bad), "{bad}");
    }
}

#[test]
fn the_built_in_json_schema_compiles_with_docs() {
    let modules = load_from(&[]);
    let module = modules.get("json_schema").unwrap();
    assert_eq!(module.layer, Layer::BuiltIn);
    let lib = module.lib.as_ref().unwrap();
    assert_eq!(lib.functions.len(), 1, "private helpers stay unlisted");
    assert_eq!(lib.functions[0].signature, "validate(value, schema)");
    assert!(lib.functions[0].doc.starts_with("Validate `value`"));
}

/// built-in < user < project, by name; a sub-dir makes a slash name.
#[test]
fn layers_shadow_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let (user, project) = (tmp.path().join("user"), tmp.path().join("project"));
    write(&user, "json_schema.rhai", "fn validate(v, s) { [] }");
    write(&user, "both.rhai", "fn who() { \"user\" }");
    write(&user, "mine.rhai", "fn who() { \"user\" }");
    write(&project, "both.rhai", "fn who() { \"project\" }");
    write(&project, "net/http_util.rhai", "fn get(u) { u }");
    let modules = load_from(&[(Layer::User, user), (Layer::Project, project)]);
    assert_eq!(layer_of(&modules, "json_schema"), Layer::User);
    assert_eq!(layer_of(&modules, "mine"), Layer::User);
    assert_eq!(layer_of(&modules, "both"), Layer::Project);
    assert_eq!(layer_of(&modules, "net/http_util"), Layer::Project);
}

/// A bad file name is skipped; a non-`.rhai` file is not a module.
#[test]
fn bad_file_names_are_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "Bad-Name.rhai", "fn f() { 1 }");
    write(tmp.path(), "notes.md", "fn f() { 1 }");
    write(tmp.path(), "ok.rhai", "fn f() { 1 }");
    let modules = load_from(&[(Layer::Project, tmp.path().to_path_buf())]);
    let names: Vec<&String> = modules.iter().map(|(n, _)| n).collect();
    assert_eq!(names, ["json_schema", "ok"]);
}

/// A top-level statement rejects the module — and it still claims its
/// name, so a broken override does not fall back to the built-in.
#[test]
fn a_top_level_statement_rejects_and_shadows() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "json_schema.rhai",
        "let x = 1;\nfn validate(v, s) { [] }",
    );
    let modules = load_from(&[(Layer::Project, tmp.path().to_path_buf())]);
    let module = modules.get("json_schema").unwrap();
    assert_eq!(module.layer, Layer::Project);
    assert!(module
        .lib
        .as_ref()
        .unwrap_err()
        .contains("top-level statement"));
}

#[test]
fn listing_names_each_layer_signature_doc_and_rejection() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "util.rhai", "/// Twice.\nfn twice(x) { x * 2 }");
    write(tmp.path(), "broken.rhai", "print(1);");
    let listing = load_from(&[(Layer::Project, tmp.path().to_path_buf())])
        .listing()
        .unwrap();
    assert!(listing.contains("import \"<name>\" as m;"), "{listing}");
    assert!(listing.contains("\n  json_schema (built-in)\n    validate(value, schema)"));
    assert!(listing.contains("\n  util (project)\n    twice(x)\n        Twice."));
    assert!(listing.contains("broken (project) — not importable: only `fn` definitions"));
    assert_eq!(RhaiModules::default().listing(), None);
}

#[test]
fn module_dirs_without_a_user_layer_is_the_project_dir() {
    let dirs = module_dirs(Path::new("/proj"), &UserLayer::Dir(PathBuf::from("/u")));
    assert_eq!(
        dirs,
        [(
            Layer::Project,
            PathBuf::from("/proj/.entanglement/rhai/modules")
        )]
    );
}
