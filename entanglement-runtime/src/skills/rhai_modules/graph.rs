//! A module's top-level `import`s, resolved once at discovery (ADR-0248):
//! modules load in dependency order, each handed exactly its already-loaded
//! imports, so a shared import (a diamond) is one `Shared<Module>` every
//! importer holds. Names resolve through the
//! winning layer at every level. A cycle, a missing or unimportable
//! import, or a chain deeper than [`MAX_IMPORT_DEPTH`] makes a module
//! unimportable, and that rejection travels up to its importers.
//!
//! Iterative, not recursive, and linear: modules load in Kahn's order; when
//! none is ready there is a cycle, found by walking unsettled imports — at
//! most one step per pending module.

use std::collections::{BTreeMap, HashMap, VecDeque};

use super::super::lib_rhai::{build, compile_engine, SkillLib};
use super::{valid_name, RhaiModule};
use crate::layers::Layer;

/// The longest import chain below a module.
pub(super) const MAX_IMPORT_DEPTH: usize = 8;

/// A parsed module waiting for its imports.
pub(super) struct Parsed {
    pub layer: Layer,
    pub source: String,
    pub text: String,
    pub ast: rhai::AST,
    /// The imported module names, each once, in source order.
    pub imports: Vec<String>,
    /// Every `import "<name>" as <alias>`: `(alias, name)`.
    pub aliases: Vec<(String, String)>,
}

/// What [`parse`] returns: the AST, the imported names, the aliases.
pub(super) type ParseOutput = (rhai::AST, Vec<String>, Vec<(String, String)>);

/// Parse a module: `fn` definitions plus top-level `import "<name>" as x;`
/// — any other top-level statement, an `import` inside a function (which
/// would skip this graph), or a typed method refuses it. A typed method is
/// refused in every module because an importing module's functions are
/// registered by name and arity (`lib_rhai::with_imports`), which leaves a
/// `fn T.f()` uncallable in every form — and one rule for all modules beats
/// one that flips when a module gains its first import.
pub(super) fn parse(text: &str) -> Result<ParseOutput, String> {
    let ast = compile_engine()
        .compile(text)
        .map_err(|e| format!("does not compile: {e}"))?;
    let (mut imports, mut aliases) = (Vec::<String>::new(), Vec::new());
    for stmt in ast.statements() {
        let rhai::Stmt::Import(import, _) = stmt else {
            return Err(
                "only `fn` definitions and `import \"<name>\" as x;` are allowed — \
                 a top-level statement rejects the whole module"
                    .into(),
            );
        };
        let rhai::Expr::StringConstant(name, _) = &import.0 else {
            return Err("an `import` takes a literal module name".into());
        };
        if !valid_name(name) {
            return Err(format!("import \"{name}\": not a module name"));
        }
        if import.1.name.is_empty() {
            return Err(format!("import \"{name}\" needs `as <alias>`"));
        }
        if !imports.iter().any(|i| i == name.as_str()) {
            imports.push(name.to_string());
        }
        let alias = import.1.name.to_string();
        // A module's environ would answer a repeated alias with the last
        // import, a script's scope with the first: refuse the ambiguity.
        if aliases.iter().any(|(a, _)| *a == alias) {
            return Err(format!("alias `{alias}` is imported twice"));
        }
        aliases.push((alias, name.to_string()));
    }
    if let Some(f) = ast.iter_fn_def().find(|f| f.this_type.is_some()) {
        let ty = f.this_type.as_deref().unwrap_or_default();
        return Err(format!(
            "a typed method (`fn {ty}.{}`) is not allowed in a module — make it a plain \
             `fn {}(x, …)`",
            f.name, f.name
        ));
    }
    let mut all_imports = 0;
    ast.walk(&mut |path: &[rhai::ASTNode]| {
        if let Some(rhai::ASTNode::Stmt(rhai::Stmt::Import(..))) = path.last() {
            all_imports += 1;
        }
        true
    });
    if all_imports > aliases.len() {
        return Err(
            "an `import` inside a function is not allowed in a module — \
             import at the top level, where discovery resolves it"
                .into(),
        );
    }
    Ok((ast, imports, aliases))
}

/// Load every `pending` module into `out` (which holds the already-refused
/// ones), importers after their imports.
pub(super) fn resolve(
    mut pending: BTreeMap<String, Parsed>,
    out: &mut BTreeMap<String, RhaiModule>,
) {
    // Kahn's order: each module waits on its pending imports and is ready
    // once none is left, so a chain costs one step per module.
    let mut waiting: HashMap<String, usize> = HashMap::new();
    let mut importers: HashMap<String, Vec<String>> = HashMap::new();
    for (name, p) in &pending {
        let deps: Vec<&String> = p
            .imports
            .iter()
            .filter(|i| pending.contains_key(*i))
            .collect();
        waiting.insert(name.clone(), deps.len());
        for dep in deps {
            importers.entry(dep.clone()).or_default().push(name.clone());
        }
    }
    let mut ready: VecDeque<String> = pending
        .keys()
        .filter(|name| waiting.get(*name) == Some(&0))
        .cloned()
        .collect();
    let mut depth: HashMap<String, usize> = HashMap::new();
    while !pending.is_empty() {
        let settled = match ready.pop_front() {
            Some(name) => {
                let Some(p) = pending.remove(&name) else {
                    continue;
                };
                let lib = load(&name, &p, out, &mut depth);
                if let Err(why) = &lib {
                    tracing::warn!(module = %name, source = %p.source, "rhai module not importable: {why}");
                }
                out.insert(name.clone(), module(p, lib));
                vec![name]
            }
            None => reject_cycle(&mut pending, out),
        };
        for name in settled {
            for importer in importers.get(&name).into_iter().flatten() {
                let Some(left) = waiting.get_mut(importer) else {
                    continue;
                };
                *left = left.saturating_sub(1);
                if *left == 0 && pending.contains_key(importer) {
                    ready.push_back(importer.clone());
                }
            }
        }
    }
}

fn module(p: Parsed, lib: Result<SkillLib, String>) -> RhaiModule {
    RhaiModule {
        layer: p.layer,
        source: p.source,
        imports: p.imports,
        lib,
    }
}

/// Load one module whose imports are all settled in `out`.
fn load(
    name: &str,
    p: &Parsed,
    out: &BTreeMap<String, RhaiModule>,
    depth: &mut HashMap<String, usize>,
) -> Result<SkillLib, String> {
    let mut deepest = 0;
    for import in &p.imports {
        let dep = out
            .get(import)
            .ok_or_else(|| format!("imports \"{import}\", which is no module"))?;
        if dep.lib.is_err() {
            return Err(format!(
                "imports \"{import}\" ({}), which is not importable",
                dep.layer.label()
            ));
        }
        deepest = deepest.max(1 + depth.get(import).copied().unwrap_or(0));
    }
    if deepest > MAX_IMPORT_DEPTH {
        return Err(format!(
            "its import chain is {deepest} deep, over the cap of {MAX_IMPORT_DEPTH}"
        ));
    }
    // Every alias names a module checked importable just above.
    let imports: Vec<(String, rhai::Shared<rhai::Module>)> = p
        .aliases
        .iter()
        .filter_map(|(alias, import)| {
            let lib = out.get(import)?.lib.as_ref().ok()?;
            Some((alias.clone(), lib.module.clone()))
        })
        .collect();
    let lib = build("", &p.text, &p.ast, &imports)?;
    depth.insert(name.to_string(), deepest);
    Ok(lib)
}

/// Nothing is ready, so every pending module waits on another pending one:
/// walk those waits from the first until a module repeats, and refuse the
/// loop that closes — one warning naming its path. Returns the refused.
fn reject_cycle(
    pending: &mut BTreeMap<String, Parsed>,
    out: &mut BTreeMap<String, RhaiModule>,
) -> Vec<String> {
    let Some(start) = pending.keys().next().cloned() else {
        return Vec::new();
    };
    let mut seen_at: HashMap<String, usize> = HashMap::from([(start.clone(), 0)]);
    let mut walk = vec![start];
    let cycle = loop {
        let Some(cur) = walk.last().cloned() else {
            break Vec::new();
        };
        let next = pending
            .get(&cur)
            .and_then(|p| p.imports.iter().find(|i| pending.contains_key(*i)))
            .cloned();
        // Unreachable while nothing is ready; refusing `cur` still ends the loop.
        let Some(next) = next else {
            break vec![cur];
        };
        if let Some(&at) = seen_at.get(&next) {
            break walk.split_off(at);
        }
        seen_at.insert(next.clone(), walk.len());
        walk.push(next);
    };
    let path = cycle
        .iter()
        .chain(cycle.first())
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" → ");
    tracing::warn!("rhai module import cycle {path}: none of its modules is importable");
    for name in &cycle {
        if let Some(p) = pending.remove(name) {
            out.insert(name.clone(), module(p, Err(format!("import cycle {path}"))));
        }
    }
    cycle
}

#[cfg(test)]
mod tests;
