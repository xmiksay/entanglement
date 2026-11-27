//! Stub tools for the library's unit tests: the argument shapes a head's
//! tools declare — a path, a shell command — with nothing behind them.

use std::borrow::Cow;

use async_trait::async_trait;

use crate::capability::Capability;
use crate::path_grading::str_arg;
use crate::tools::{ArgKind, PermissionScope, Tool, ToolRegistry};

/// A tool whose argument is its input's `path`.
pub(crate) struct PathTool(pub &'static str, pub &'static [Capability]);

#[async_trait]
impl Tool for PathTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(self.0)
    }
    fn capabilities(&self) -> &'static [Capability] {
        self.1
    }
    fn permission_scope(&self) -> PermissionScope {
        PermissionScope::arg(ArgKind::Path)
    }
    fn escape_target(&self, input: &str) -> Option<String> {
        str_arg(input, "path")
    }
    fn join_workdir(
        &self,
        args: &mut serde_json::Map<String, serde_json::Value>,
        base: &str,
    ) -> bool {
        crate::path_grading::join(args, "path", base)
    }
    async fn run(&self, _input: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }
}

/// An `Exec` tool whose argument is its input's `command`, run in an
/// optional `workdir`.
pub(crate) struct CommandTool(pub &'static str);

#[async_trait]
impl Tool for CommandTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(self.0)
    }
    fn capabilities(&self) -> &'static [Capability] {
        &[Capability::Exec]
    }
    fn permission_scope(&self) -> PermissionScope {
        PermissionScope {
            arg: ArgKind::Command,
            workdir: true,
        }
    }
    fn permission_arg(&self, input: &str) -> Option<String> {
        str_arg(input, "command")
    }
    async fn run(&self, _input: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }
}

/// `peek` (read a path), `put` (write a path) and `sh` (run a command).
pub(crate) fn stub_tools() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(PathTool("peek", &[Capability::Read]));
    reg.register(PathTool("put", &[Capability::Write]));
    reg.register(CommandTool("sh"));
    reg
}
