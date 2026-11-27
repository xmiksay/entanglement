//! The argument an argument-scoped `tool(pattern)` rule (#173) matches, and
//! the workdir a workdir-scoped `tool{pattern}` rule (#425) matches. A
//! registered tool answers both through
//! [`Tool::permission_scope`][crate::tools::Tool::permission_scope] and
//! [`Tool::permission_arg`][crate::tools::Tool::permission_arg] (#962); only
//! the executor's own routes, which no registry holds, are answered here.

use crate::path_grading::str_arg;
use crate::tool_names::{AGENT_SEND_TOOL, AGENT_TOOL, RHAI_TOOL};
use crate::tools::{ArgKind, PermissionScope, ToolRegistry};

/// What `tool`'s calls offer the scoped rules: an executor route's fixed
/// shape, else what the registered tool declares. An unregistered tool (an
/// MCP tool not yet connected, a typo) offers nothing.
pub fn permission_scope(tools: &ToolRegistry, tool: &str) -> PermissionScope {
    match tool {
        RHAI_TOOL => PermissionScope::arg(ArgKind::Path),
        AGENT_TOOL | AGENT_SEND_TOOL => PermissionScope::arg(ArgKind::Text),
        _ => tools
            .get(tool)
            .map(|t| t.permission_scope())
            .unwrap_or_default(),
    }
}

/// The argument string an argument-scoped permission rule matches, as the
/// call spells it (see [`crate::permission_path::grading_arg`] for the
/// root-relative form grading uses). `None` for a tool with none, a call
/// that carries none, or malformed input: an argument-scoped rule then never
/// matches, so resolution falls through to the tool's bare rules.
///
/// The executor routes: `rhai` takes its script `path` (ADR-0228; an inline
/// script has none, so only a bare `rhai` rule matches it). `agent` takes
/// the profile the spawn will actually run (ADR-0235): an omitted, empty or
/// non-JSON input resolves to the default profile exactly as the spawn
/// does, so `agent(general)` also covers a call that names no agent.
/// `agent_send` takes its `agent_id` handle, trimmed: the id the model
/// addresses, which survives the child's compactions (ADR-0234).
pub fn permission_arg(tools: &ToolRegistry, tool: &str, input: &str) -> Option<String> {
    match tool {
        RHAI_TOOL => str_arg(input, "path"),
        AGENT_TOOL => Some(crate::subagent::target_agent(input)),
        AGENT_SEND_TOOL => {
            Some(str_arg(input, "agent_id")?.trim().to_string()).filter(|id| !id.is_empty())
        }
        // The declared scope decides, so grading never matches a rule that
        // `warn_unscopable_rules` reported as one that never matches.
        _ => {
            let t = tools.get(tool)?;
            if t.permission_scope().arg == ArgKind::None {
                return None;
            }
            t.permission_arg(input)
        }
    }
}

/// The `workdir` a call would run in, for a workdir-scoped rule — distinct
/// from [`permission_arg`]. `None` for a tool whose scope has no workdir, an
/// absent `workdir` (the tool then defaults to root), or malformed input: a
/// workdir-scoped rule then never matches, falling through to the tool's
/// other rules.
pub fn permission_workdir(tools: &ToolRegistry, tool: &str, input: &str) -> Option<String> {
    if !permission_scope(tools, tool).workdir {
        return None;
    }
    str_arg(input, "workdir")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_names::{is_non_maskable, known_tool_names};

    /// One input carrying every field any tool's argument or workdir reads,
    /// so each tool that declares one yields it from this input.
    const EVERY_FIELD: &str = r#"{"agent":"plan","agent_id":"s-1","command":"x",
        "path":"src/a.rs","pattern":"*.rs","url":"https://a.example/","method":"GET",
        "workdir":"/tmp"}"#;

    /// #878/#881/#962: what a tool declares agrees with what it extracts,
    /// for every known tool registered the way a head registers it.
    #[test]
    fn the_declared_scope_matches_what_is_extracted() {
        let tools = crate::host::every_tool("/p".into());
        for tool in known_tool_names()
            .iter()
            .copied()
            .chain(["mcp__gh__create"])
        {
            let scope = permission_scope(&tools, tool);
            assert_eq!(
                scope.arg != ArgKind::None,
                permission_arg(&tools, tool, EVERY_FIELD).is_some(),
                "{tool}"
            );
            assert_eq!(
                scope.workdir,
                permission_workdir(&tools, tool, EVERY_FIELD).is_some(),
                "{tool}"
            );
        }
        assert_eq!(
            permission_scope(&tools, "mcp__gh__create"),
            PermissionScope::default()
        );
    }

    /// Every gradable known tool is scopable today; only the non-maskable
    /// ones (never graded under their own name) take no argument. A new
    /// maskable tool that forgets its `permission_arg` would grade every
    /// scoped rule as a silent no-op, so this trips until the omission is
    /// deliberate.
    #[test]
    fn every_maskable_known_tool_takes_an_argument() {
        let tools = crate::host::every_tool("/p".into());
        for tool in known_tool_names() {
            assert_eq!(
                permission_scope(&tools, tool).arg != ArgKind::None,
                !is_non_maskable(tool),
                "{tool}: maskable but declares no argument — implement \
                 `Tool::permission_scope`, or exempt it here if it truly takes none"
            );
        }
    }

    /// #962: an embedder tool becomes the target of `tool(path)` rules by
    /// implementing the trait methods, with no name list to join.
    #[test]
    fn an_embedder_tool_declares_its_own_argument() {
        struct Fetch;
        #[async_trait::async_trait]
        impl crate::tools::Tool for Fetch {
            fn name(&self) -> std::borrow::Cow<'static, str> {
                "fetch_doc".into()
            }
            async fn run(&self, _input: &str) -> anyhow::Result<String> {
                Ok(String::new())
            }
            fn permission_scope(&self) -> PermissionScope {
                PermissionScope::arg(ArgKind::Path)
            }
            fn permission_arg(&self, input: &str) -> Option<String> {
                str_arg(input, "doc")
            }
        }
        let mut tools = ToolRegistry::new();
        tools.register(Fetch);
        assert_eq!(
            permission_arg(&tools, "fetch_doc", r#"{"doc":"a/b.md"}"#).as_deref(),
            Some("a/b.md")
        );
        assert_eq!(permission_scope(&tools, "fetch_doc").arg, ArgKind::Path);
    }

    /// An override of `permission_arg` without a declared argument is
    /// ignored: grading follows the declared scope, the same answer the
    /// startup warning for unscopable rules gives.
    #[test]
    fn an_argument_from_a_tool_declaring_none_is_ignored() {
        struct Undeclared;
        #[async_trait::async_trait]
        impl crate::tools::Tool for Undeclared {
            fn name(&self) -> std::borrow::Cow<'static, str> {
                "fetch_doc".into()
            }
            async fn run(&self, _input: &str) -> anyhow::Result<String> {
                Ok(String::new())
            }
            fn permission_arg(&self, input: &str) -> Option<String> {
                str_arg(input, "doc")
            }
        }
        let mut tools = ToolRegistry::new();
        tools.register(Undeclared);
        assert_eq!(permission_scope(&tools, "fetch_doc").arg, ArgKind::None);
        assert_eq!(
            permission_arg(&tools, "fetch_doc", r#"{"doc":"a/b.md"}"#),
            None
        );
    }

    /// A path tool's argument defaults to its escape target, so a tool that
    /// declares `ArgKind::Path` and an `escape_target` is scopable with no
    /// third method to keep in step; any other kind defaults to none.
    #[test]
    fn a_path_scope_defaults_its_argument_to_the_escape_target() {
        struct Note(ArgKind);
        #[async_trait::async_trait]
        impl crate::tools::Tool for Note {
            fn name(&self) -> std::borrow::Cow<'static, str> {
                match self.0 {
                    ArgKind::Path => "note".into(),
                    _ => "note_text".into(),
                }
            }
            async fn run(&self, _input: &str) -> anyhow::Result<String> {
                Ok(String::new())
            }
            fn escape_target(&self, input: &str) -> Option<String> {
                str_arg(input, "file")
            }
            fn permission_scope(&self) -> PermissionScope {
                PermissionScope::arg(self.0)
            }
        }
        let mut tools = ToolRegistry::new();
        tools.register(Note(ArgKind::Path));
        tools.register(Note(ArgKind::Text));
        let input = r#"{"file":"notes/a.md"}"#;
        assert_eq!(
            permission_arg(&tools, "note", input).as_deref(),
            Some("notes/a.md")
        );
        assert_eq!(permission_arg(&tools, "note_text", input), None);
    }

    /// ADR-0228: a `path` run is graded and escape-checked on its path; an
    /// inline script has no argument, so only a bare `rhai` rule matches it.
    #[test]
    fn rhai_path_is_its_permission_argument_and_escape_target() {
        use crate::path_grading::escape_target;
        let none = crate::tools::ToolRegistry::new();
        let path = r#"{"path":"scripts/x.rhai","args":{"n":1}}"#;
        assert_eq!(
            permission_arg(&none, "rhai", path).as_deref(),
            Some("scripts/x.rhai")
        );
        assert_eq!(
            escape_target("rhai", &none, path).as_deref(),
            Some("scripts/x.rhai")
        );
        let inline = r#"{"script":"read(\"/etc/x\")"}"#;
        assert_eq!(escape_target("rhai", &none, inline), None);
        assert_eq!(permission_arg(&none, "rhai", inline), None);
    }

    /// ADR-0235: `agent` grades on the profile the spawn runs — the default
    /// when the input names none or isn't JSON — and `agent_send` on its
    /// trimmed handle.
    #[test]
    fn spawning_pair_permission_args_are_their_targets() {
        let none = ToolRegistry::new();
        let agent = |input| permission_arg(&none, AGENT_TOOL, input);
        assert_eq!(
            agent(r#"{"agent":"plan","prompt":"x"}"#).as_deref(),
            Some("plan")
        );
        for defaulted in [
            r#"{"prompt":"x"}"#,
            r#"{"agent":"","prompt":"x"}"#,
            "raw prompt",
        ] {
            assert_eq!(agent(defaulted).as_deref(), Some("general"), "{defaulted}");
        }
        let send = |input| permission_arg(&none, AGENT_SEND_TOOL, input);
        assert_eq!(
            send(r#"{"agent_id":" s-1 ","prompt":"x"}"#).as_deref(),
            Some("s-1")
        );
        assert_eq!(send(r#"{"agent_id":"  ","prompt":"x"}"#), None);
        assert_eq!(send(r#"{"prompt":"x"}"#), None);
        assert_eq!(send("not json"), None);
    }

    /// The scoped rule reaches the mode matcher: `agent(plan)` grades a
    /// `plan` spawn and leaves every other target to the rest of the table.
    #[test]
    fn a_scoped_agent_rule_grades_only_its_target() {
        use crate::mode::Rules;
        let rules = Rules::from_lists(
            &["agent(plan)".to_string()],
            &[],
            &["agent_send(acme:*)".to_string()],
        );
        let control = [crate::capability::Capability::Control];
        let none = ToolRegistry::new();
        let grade = |tool: &str, input: &str| {
            let arg = permission_arg(&none, tool, input);
            crate::mode::rules::resolve(
                &rules,
                entanglement_core::Permission::Allow,
                tool,
                &control,
                arg.as_deref(),
                None,
            )
        };
        assert_eq!(
            grade("agent", r#"{"agent":"plan"}"#),
            entanglement_core::Permission::Deny
        );
        assert_eq!(
            grade("agent", r#"{"agent":"general"}"#),
            entanglement_core::Permission::Allow
        );
        assert_eq!(
            grade("agent_send", r#"{"agent_id":"acme:s-1"}"#),
            entanglement_core::Permission::Ask
        );
        assert_eq!(
            grade("agent_send", r#"{"agent_id":"s-1"}"#),
            entanglement_core::Permission::Allow
        );
    }
}
