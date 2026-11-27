//! What argument- and workdir-scoped permission rules can see of a tool's
//! calls (#962). Each tool declares its own shape through
//! [`Tool::permission_scope`][super::Tool::permission_scope] and extracts
//! the argument through [`Tool::permission_arg`][super::Tool::permission_arg],
//! so the permission pipeline never names a host tool.

/// The kind of argument a tool's `permission_arg` yields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ArgKind {
    /// No argument: a `tool(pattern)` rule never matches a call, and config
    /// warns about one.
    #[default]
    None,
    /// A filesystem path (or path glob) resolved against the project root:
    /// graded root-relative (ADR-0125), and an approval card names the root.
    Path,
    /// A shell command line. It may carry a credential, so a script's
    /// activity line shows only the program it runs.
    Command,
    /// Any other string (a URL, an agent name), graded as extracted.
    Text,
}

/// What a tool's calls offer the scoped permission rules: an argument for
/// `tool(pattern)` (#173), and a `workdir` for `tool{pattern}` (#425).
/// `Default` offers neither, so only the tool's bare rules apply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PermissionScope {
    pub arg: ArgKind,
    /// The call's input carries an optional `workdir` string that a
    /// workdir-scoped rule matches.
    pub workdir: bool,
}

impl PermissionScope {
    /// A scope with an argument of `kind` and no workdir.
    pub const fn arg(kind: ArgKind) -> Self {
        Self {
            arg: kind,
            workdir: false,
        }
    }
}
