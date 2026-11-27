//! Path grading: which filesystem path a call names, before anything grades
//! it. The generic pipeline asks; the tools that own the path arguments
//! answer through [`crate::tools::Tool::join_workdir`] (and, for an
//! embedder's own containment check, [`crate::tools::Tool::escape_target`]),
//! so the executor, the script gate and the binding bridge never name a host
//! tool.
//!
//! [`rewrite`] turns a call's relative path arguments into root-relative
//! ones for a session working in a workdir (#764, ADR-0224), so permission
//! rules, grants and a head's own containment check see exactly what they
//! would see with no workdir. `rhai`'s script `path` (ADR-0228) is answered
//! here: the script tool is the executor's own route, not a registry tool.
//! The library keeps no root containment of its own (#968).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use entanglement_core::SessionId;
use serde_json::{Map, Value};

use crate::permission_path::normalize_lexical;
use crate::tool_names::RHAI_TOOL;
use crate::tools::ToolRegistry;

/// What `tool`'s input becomes for a session working in `workdir` (absolute)
/// under `root`, or `None` when nothing changes — no workdir, a workdir equal
/// to the root, a tool with no path argument (or not in `tools`), or only
/// absolute paths. Public so an embedder observing `ToolExec` (skutter's
/// `post_tool_use` hook) sees the call's paths as the executor ran them.
pub fn rewrite(
    tools: &ToolRegistry,
    root: &Path,
    workdir: Option<&Path>,
    tool: &str,
    input: &str,
) -> Option<String> {
    let base = base(root, workdir?)?;
    let mut value: Value = serde_json::from_str(input).ok()?;
    let args = value.as_object_mut()?;
    let changed = if tool == RHAI_TOOL {
        join(args, "path", &base)
    } else {
        tools.get(tool)?.join_workdir(args, &base)
    };
    changed.then(|| value.to_string())
}

/// The string `key` of a JSON-object `input` — the shape every path
/// argument has, for a [`crate::tools::Tool::escape_target`].
pub fn str_arg(input: &str, key: &str) -> Option<String> {
    let value: Value = serde_json::from_str(input).ok()?;
    value.get(key)?.as_str().map(String::from)
}

/// The prefix a relative argument joins onto: the workdir relative to the
/// root (so a joined path stays root-relative, as the model would have
/// spelled it), or absolute for a workdir outside the root.
fn base(root: &Path, workdir: &Path) -> Option<String> {
    match workdir.strip_prefix(root) {
        Ok(rel) if rel.as_os_str().is_empty() => None,
        Ok(rel) => Some(rel.to_string_lossy().into_owned()),
        Err(_) => Some(workdir.to_string_lossy().into_owned()),
    }
}

/// Join a present, relative string `key` onto `base`.
pub fn join(args: &mut Map<String, Value>, key: &str, base: &str) -> bool {
    let Some(Value::String(rel)) = args.get(key) else {
        return false;
    };
    if Path::new(rel).is_absolute() {
        return false;
    }
    let joined = normalize_lexical(&format!("{base}/{rel}"));
    args.insert(key.to_string(), Value::String(joined));
    true
}

/// [`join`], or set `key` to `base` when the call left it out.
pub fn join_or_default(args: &mut Map<String, Value>, key: &str, base: &str) -> bool {
    if args.contains_key(key) {
        return join(args, key, base);
    }
    args.insert(key.to_string(), Value::String(base.to_string()));
    true
}

/// Each session's working directory as the executor last saw it on a
/// `ToolExec` — the call carries it, so this is always current for the call
/// being run. Shared with the search tools, which print paths relative to it.
#[derive(Clone, Default)]
pub struct SessionWorkdirs(Arc<RwLock<HashMap<SessionId, PathBuf>>>);

impl SessionWorkdirs {
    /// Record the workdir a `ToolExec` carried (`None`: the project root).
    pub(crate) fn observe(&self, session: &SessionId, workdir: Option<&str>) {
        let mut map = self.0.write().expect("session-workdir lock poisoned");
        match workdir {
            Some(dir) => map.insert(session.clone(), PathBuf::from(dir)),
            None => map.remove(session),
        };
    }

    /// The session's workdir, `None` for the project root.
    pub(crate) fn get(&self, session: &SessionId) -> Option<PathBuf> {
        self.0
            .read()
            .expect("session-workdir lock poisoned")
            .get(session)
            .cloned()
    }

    pub(crate) fn forget(&self, session: &SessionId) {
        self.0
            .write()
            .expect("session-workdir lock poisoned")
            .remove(session);
    }

    /// The directory a search tool prints paths relative to: the session's
    /// workdir, else `root`. A path outside it prints absolute, which the
    /// tools accept back unchanged.
    pub fn display_root(this: Option<&Self>, session: &SessionId, root: &Path) -> PathBuf {
        this.and_then(|w| w.get(session))
            .unwrap_or_else(|| root.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rhai(workdir: &str, input: Value) -> Option<Value> {
        rewrite(
            &ToolRegistry::new(),
            Path::new("/p"),
            Some(Path::new(workdir)),
            RHAI_TOOL,
            &input.to_string(),
        )
        .map(|s| serde_json::from_str(&s).expect("rewritten input is JSON"))
    }

    /// ADR-0228: a script file's `path` joins like a file tool's; an inline
    /// script has no path to rewrite.
    #[test]
    fn a_rhai_script_path_joins_and_an_inline_script_is_untouched() {
        assert_eq!(
            rhai("/p/sub", serde_json::json!({"path": "a.rs", "x": 1})),
            Some(serde_json::json!({"path": "sub/a.rs", "x": 1}))
        );
        assert_eq!(rhai("/p/sub", serde_json::json!({"script": "1"})), None);
    }

    /// A tool the registry doesn't hold has no path arguments to rewrite.
    #[test]
    fn an_unregistered_tool_is_untouched() {
        let input = serde_json::json!({"path": "a.rs"}).to_string();
        let empty = ToolRegistry::new();
        let got = rewrite(
            &empty,
            Path::new("/p"),
            Some(Path::new("/p/sub")),
            "read",
            &input,
        );
        assert_eq!(got, None);
    }

    #[test]
    fn session_workdirs_track_and_forget() {
        let w = SessionWorkdirs::default();
        let s = SessionId::new("s1");
        let root = Path::new("/p");
        assert_eq!(SessionWorkdirs::display_root(Some(&w), &s, root), root);
        w.observe(&s, Some("/p/sub"));
        assert_eq!(w.get(&s), Some(PathBuf::from("/p/sub")));
        assert_eq!(
            SessionWorkdirs::display_root(Some(&w), &s, root),
            PathBuf::from("/p/sub")
        );
        w.observe(&s, None);
        assert_eq!(w.get(&s), None);
        w.observe(&s, Some("/p/sub"));
        w.forget(&s);
        assert_eq!(w.get(&s), None);
        assert_eq!(SessionWorkdirs::display_root(None, &s, root), root);
    }
}
