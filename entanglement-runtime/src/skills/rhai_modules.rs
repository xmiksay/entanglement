//! Importable Rhai library modules (ADR-0248): `import "json_schema" as js;`
//! in any `rhai` script. A module is a `<name>.rhai` file of functions,
//! compiled once at discovery like a skill's `lib.rhai` and live-reloaded
//! with the skill registry it rides in.
//!
//! Layers, later wins on a name: built-in (embedded in the binary) <
//! `${config_dir}/entanglement/rhai/modules/` < `<root>/.entanglement/rhai/modules/`.
//! A sub-directory makes a slash name (`a/b.rhai` is `"a/b"`). A module may
//! `import` others at its top level, resolved at discovery ([`graph`]). A
//! file that fails to compile, or carries any other top-level statement,
//! still claims its name — shadowing a lower layer — and makes it
//! unimportable, so a broken override is loud rather than silently falling
//! back to the module it meant to replace.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use super::lib_rhai::SkillLib;
use crate::layers::{Layer, UserLayer};

#[cfg(feature = "rhai")]
mod graph;

/// Embedded modules: `(name, source)`.
#[cfg_attr(not(feature = "rhai"), allow(dead_code))]
const BUILT_INS: &[(&str, &str)] =
    &[("json_schema", include_str!("rhai_modules/json_schema.rhai"))];

/// The module directory below a layer's `entanglement` dir.
const MODULES_SUBDIR: &str = "rhai/modules";

/// One discovered module: the winning file for its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RhaiModule {
    pub layer: Layer,
    /// `built-in` or the file path.
    pub source: String,
    /// The modules its top level imports, by name.
    pub imports: Vec<String>,
    /// The compiled module, or why it was refused.
    pub lib: Result<SkillLib, String>,
}

/// Every importable module by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RhaiModules(BTreeMap<String, RhaiModule>);

impl RhaiModules {
    pub fn get(&self, name: &str) -> Option<&RhaiModule> {
        self.0.get(name)
    }

    /// Name-sorted.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &RhaiModule)> {
        self.0.iter()
    }

    /// The `load_skill rhai` block: each module, its winning layer, and its
    /// public functions with their docs — `None` when there are none.
    pub(crate) fn listing(&self) -> Option<String> {
        if self.0.is_empty() {
            return None;
        }
        let mut out = String::from(
            "modules (`import \"<name>\" as m;` then `m::fn(…)`; \
             built-in < user < project, by name):",
        );
        for (name, module) in &self.0 {
            let mut layer = module.layer.label().to_string();
            if !module.imports.is_empty() {
                layer = format!("{layer}; imports {}", module.imports.join(", "));
            }
            match &module.lib {
                Ok(lib) => {
                    out.push_str(&format!("\n  {name} ({layer})"));
                    for f in &lib.functions {
                        out.push_str(&format!("\n    {}", f.signature));
                        for line in f.doc.lines() {
                            out.push_str(&format!("\n        {line}"));
                        }
                    }
                }
                Err(why) => out.push_str(&format!("\n  {name} ({layer}) — not importable: {why}")),
            }
        }
        Some(out)
    }
}

/// Whether `name` is an importable module name: `[a-z0-9_]+` segments
/// joined by `/` — so no `..`, no absolute path, no extension.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('/').all(|seg| {
            !seg.is_empty()
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

/// The user and project module dirs, existing or not, lowest first — what
/// discovery reads and the write floor protects. An explicit
/// [`UserLayer::Dir`] (a test or an embedder staying off the real user
/// dirs) has no user module layer.
pub fn module_dirs(root: &Path, user: &UserLayer) -> Vec<(Layer, PathBuf)> {
    let mut dirs = Vec::new();
    if let UserLayer::FromEnv = user {
        if let Some(config) = crate::user_dirs::config_dir() {
            dirs.push((
                Layer::User,
                config.join("entanglement").join(MODULES_SUBDIR),
            ));
        }
    }
    dirs.push((
        Layer::Project,
        root.join(".entanglement").join(MODULES_SUBDIR),
    ));
    dirs
}

/// Discover and compile every module for `root`.
pub fn load(root: &Path, user: &UserLayer) -> RhaiModules {
    load_from(&module_dirs(root, user))
}

/// The built-ins, then each of `dirs` in order, later winning on a name;
/// then the winners' imports resolved. An unreadable dir or file is warned
/// and skipped: a module dir must not stop the skill registry loading.
#[cfg(feature = "rhai")]
pub(crate) fn load_from(dirs: &[(Layer, PathBuf)]) -> RhaiModules {
    let mut winners: BTreeMap<String, (Layer, String, Result<String, String>)> = BUILT_INS
        .iter()
        .map(|(name, text)| {
            let winner = (Layer::BuiltIn, "built-in".to_string(), Ok(text.to_string()));
            (name.to_string(), winner)
        })
        .collect();
    for (layer, dir) in dirs {
        for (name, path) in scan(dir) {
            let text = std::fs::read_to_string(&path).map_err(|e| format!("unreadable: {e}"));
            winners.insert(name, (*layer, path.display().to_string(), text));
        }
    }
    let (mut out, mut pending) = (BTreeMap::new(), BTreeMap::new());
    for (name, (layer, source, text)) in winners {
        match text.and_then(|text| graph::parse(&text).map(|parsed| (text, parsed))) {
            Ok((text, (ast, imports, aliases))) => {
                let parsed = graph::Parsed {
                    layer,
                    source,
                    text,
                    ast,
                    imports,
                    aliases,
                };
                pending.insert(name, parsed);
            }
            Err(why) => {
                tracing::warn!(module = %name, source = %source, "rhai module not importable: {why}");
                let imports = Vec::new();
                let lib = Err(why);
                out.insert(
                    name,
                    RhaiModule {
                        layer,
                        source,
                        imports,
                        lib,
                    },
                );
            }
        }
    }
    graph::resolve(pending, &mut out);
    RhaiModules(out)
}

#[cfg(not(feature = "rhai"))]
pub(crate) fn load_from(_dirs: &[(Layer, PathBuf)]) -> RhaiModules {
    RhaiModules::default()
}

/// Every `*.rhai` under `dir` by module name, path-sorted; a file whose
/// name is not a valid module name is warned and skipped.
#[cfg_attr(not(feature = "rhai"), allow(dead_code))]
fn scan(dir: &Path) -> Vec<(String, PathBuf)> {
    if !dir.is_dir() {
        return Vec::new();
    }
    let mut found = Vec::new();
    let is_rhai = |p: &Path| p.extension().and_then(|e| e.to_str()) == Some("rhai");
    if let Err(e) = crate::layers::walk_files(dir, &mut found, &mut HashSet::new(), &is_rhai) {
        tracing::warn!(dir = %dir.display(), "skipping unreadable rhai module dir: {e:#}");
        return Vec::new();
    }
    found.sort();
    found
        .into_iter()
        .filter_map(|path| {
            let rel = path.strip_prefix(dir).ok()?.with_extension("");
            let name = rel
                .components()
                .map(|c| c.as_os_str().to_str())
                .collect::<Option<Vec<_>>>()
                .map(|segs| segs.join("/"));
            match name {
                Some(name) if valid_name(&name) => Some((name, path)),
                _ => {
                    tracing::warn!(path = %path.display(), "skipping rhai module: its name must be [a-z0-9_] segments");
                    None
                }
            }
        })
        .collect()
}

#[cfg(all(test, feature = "rhai"))]
mod tests;
