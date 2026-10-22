//! The run's module resolver (ADR-0248): `import "<name>"` resolves by bare
//! name against the discovered, precompiled modules only — never a file
//! path, so neither an inline nor a `path` script can reach the filesystem
//! through `import`.

use std::sync::Arc;

use rhai::{Engine, EvalAltResult, Module, ModuleResolver, Position, Shared};

use crate::skills::rhai_modules::valid_name;
use crate::skills::RhaiModules;

pub(super) struct Resolver(pub Arc<RhaiModules>);

impl ModuleResolver for Resolver {
    fn resolve(
        &self,
        _engine: &Engine,
        _source: Option<&str>,
        path: &str,
        pos: Position,
    ) -> Result<Shared<Module>, Box<EvalAltResult>> {
        let refuse = |msg: String| Err(Box::new(EvalAltResult::ErrorRuntime(msg.into(), pos)));
        if !valid_name(path) {
            return refuse(format!(
                "import \"{path}\": not a module name — import by bare name, e.g. \
                 \"json_schema\" ([a-z0-9_] segments joined by `/`; no path, no extension)"
            ));
        }
        match self.0.get(path).map(|m| &m.lib) {
            Some(Ok(lib)) => Ok(lib.module.clone()),
            Some(Err(why)) => refuse(format!("import \"{path}\": module not importable: {why}")),
            None => refuse(format!(
                "import \"{path}\": no such module — `load_skill rhai` lists them"
            )),
        }
    }
}

#[cfg(test)]
mod tests;
