//! The host tools' `rhai` script functions (#933): the library's engine
//! binds no file or process function of its own, so each tool declares the
//! plain function it is callable as ([`Tool::script_binding`]). Every call
//! climbs the same dispatch seam as a direct one (ADR-0227). The `doc` lines
//! are what the `rhai` spec lists under "Host bindings".
//!
//! [`Tool::script_binding`]: entanglement_runtime::Tool::script_binding

use entanglement_runtime::host_ns::{ScriptArg, ScriptBinding};

use ScriptArg::{Bool, Int, Str, StrList};

const fn text(
    overloads: &'static [&'static [(&'static str, ScriptArg)]],
    doc: &'static str,
) -> ScriptBinding {
    ScriptBinding {
        overloads,
        json: false,
        budget_key: None,
        doc,
    }
}

pub const READ: ScriptBinding = text(
    &[
        &[("path", Str)],
        &[("path", Str), ("offset", Int), ("limit", Int)],
    ],
    // The escape-root note rides on the `read`/`read_raw` lines, the file
    // bindings a model reaches for first: the library's intro only says "any
    // check the host adds", since the gate is skutter's (#968).
    r#"read(path) / read(path, offset, limit) -> "{lineno}: {line}" text, NOT parseable as JSON/YAML
a path outside the project root asks first (the escape-root gate), as edit/write do"#,
);

/// Graded as `read` inside the root; out of it the call fails with no
/// escape-root card ([`super::read::ReadRawTool`]).
pub const READ_RAW: ScriptBinding = text(
    &[&[("path", Str)]],
    "read_raw(path) -> exact file content, no line prefix; use before parse_json/parse_yaml (graded as `read`); project root only: a path outside it fails with no escape-root card, use read() there",
);

pub const GLOB: ScriptBinding = text(
    &[&[("pattern", Str)]],
    "glob(pattern) -> newline-joined text of matching paths",
);

pub const GREP: ScriptBinding = text(
    &[&[("pattern", Str)], &[("pattern", Str), ("path", Str)]],
    "grep(pattern) / grep(pattern, path) -> newline-joined text of matching lines",
);

/// The structured search variants (ADR-0206): the reply is parsed into a
/// Rhai map, not a newline-joined string a script would hand-parse.
pub const GLOB_JSON: ScriptBinding = ScriptBinding {
    json: true,
    ..text(
        &[&[("pattern", Str)], &[("pattern", Str), ("exclude", StrList)]],
        "glob_json(pattern) / glob_json(pattern, exclude) -> #{files: path-string array, notices: string array}; same walk as glob, structured (empty files + a notice is the zero-match shape); grades as glob — prefer it over hand-splitting glob's newline text",
    )
};

pub const GREP_JSON: ScriptBinding = ScriptBinding {
    json: true,
    ..text(
        &[&[("pattern", Str)], &[("pattern", Str), ("path", Str)]],
        "grep_json(pattern) / grep_json(pattern, path) -> #{matches: {path, lineno, line} record array, notices: string array}; same scan as grep, structured; grades as grep — prefer it over hand-splitting grep's newline text",
    )
};

pub const EDIT: ScriptBinding = text(
    &[
        &[("path", Str), ("oldString", Str), ("newString", Str)],
        &[
            ("path", Str),
            ("oldString", Str),
            ("newString", Str),
            ("replaceAll", Bool),
        ],
    ],
    "edit(path, old, new) / edit(path, old, new, replace_all); never cut by the script budget",
);

pub const WRITE: ScriptBinding = text(
    &[&[("path", Str), ("content", Str)]],
    "write(path, content); never cut by the script budget",
);

/// `timeout` carries the script's remaining budget (#419): the engine's
/// interrupt cannot reach a child process in flight. The `workdir` overload
/// fills the same field a `bash{pattern}` rule and the escape-root check
/// read (ADR-0130).
pub const BASH: ScriptBinding = ScriptBinding {
    budget_key: Some("timeout"),
    ..text(
        &[&[("command", Str)], &[("command", Str), ("workdir", Str)]],
        "bash(command) / bash(command, workdir) -> sh -c, bounded by the script's remaining budget; workdir is what a workdir-scoped permission rule matches",
    )
};
