//! Warning for a `config.yml` rule whose *scope* can never match a call
//! (#962) — split from [`super::mode_warn`], which checks rule *names* at
//! config load. Whether a tool takes an argument (`tool(pattern)`, #878) or
//! a workdir (`tool{pattern}`, #881) is the registered tool's answer
//! ([`crate::permission::permission_scope`]), so this check runs where the
//! registry is known — at engine build, through
//! [`Config::warn_unscopable_rules`] — not at config load:
//! - an argument scope on a tool with no argument, `mcp__gh__x(foo)`;
//! - a workdir scope on a tool with no workdir, `read{/tmp/*}`;
//! - a scope the matcher can't read at all, `bash(git *){/x}` (one scope
//!   per key, closed at its end).

use entanglement_core::{Permission, PermissionProfile};

use super::Config;
use crate::mode::{capability_class, rule_tool_name};
use crate::permission::permission_scope;
use crate::tools::{ArgKind, PermissionScope, ToolRegistry};

/// A rule key that carries a scope, with the file that wrote it and its
/// scope (a mode name, or `permissions` for the ceiling) — collected per
/// layer at config load for [`Config::warn_unscopable_rules`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedRule {
    pub source: String,
    pub scope: String,
    pub entry: String,
}

impl Config {
    /// Log one warning per scoped rule key that can never match a call
    /// against `tools` — the registry the engine dispatches against. Call it
    /// once the registry is populated; returns how many it logged.
    /// Non-fatal, like the name check at load: the rule degrades loudly
    /// rather than aborting startup.
    pub fn warn_unscopable_rules(&self, tools: &ToolRegistry) -> usize {
        let mut logged = 0;
        for (rule, reason) in unscopable_rules(&self.scoped_rules, tools) {
            logged += 1;
            tracing::warn!(
                source = %rule.source,
                scope = %rule.scope,
                entry = %rule.entry,
                "config.yml rule can never match a call — it has no effect: {reason}"
            );
        }
        logged
    }
}

/// Every rule in `rules` whose scope can never match, with why.
pub(super) fn unscopable_rules<'a>(
    rules: &'a [ScopedRule],
    tools: &ToolRegistry,
) -> Vec<(&'a ScopedRule, String)> {
    rules
        .iter()
        .filter_map(|rule| Some((rule, scope_reason(&rule.entry, tools)?)))
        .collect()
}

/// Whether `entry` carries a scope at all — what config load collects.
pub(super) fn is_scoped(entry: &str) -> bool {
    key_scope(entry) != KeyScope::None
}

/// Why `entry`'s scope never matches a call to its tool, or `None` when it
/// can.
fn scope_reason(entry: &str, tools: &ToolRegistry) -> Option<String> {
    let tool = rule_tool_name(entry);
    let scope = permission_scope(tools, tool);
    match key_scope(entry) {
        KeyScope::Malformed => Some(format!(
            "`{entry}` is no rule key the matcher reads — a key carries at most one \
             scope, closed at its very end; {}",
            scope_forms(tool, scope)
        )),
        KeyScope::Arg if tool != "*" && scope.arg == ArgKind::None => Some(format!(
            "`{tool}` has no argument to scope — `{entry}` never matches a call; {}",
            instead(tool, scope)
        )),
        KeyScope::Workdir if tool != "*" && !scope.workdir => Some(format!(
            "`{tool}` has no workdir to scope — `{entry}` never matches a call; {}",
            instead(tool, scope)
        )),
        _ => None,
    }
}

/// The scopes `tool` really takes, so a malformed key's hint never points at
/// a form that would only trip another of these warnings.
fn scope_forms(tool: &str, scope: PermissionScope) -> String {
    match (
        scope.arg != ArgKind::None || tool == "*",
        scope.workdir || tool == "*",
    ) {
        (true, true) => format!("write `{tool}(pattern)` or `{tool}{{workdir}}`"),
        (true, false) => format!("write `{tool}(pattern)`"),
        (false, true) => format!("write `{tool}{{workdir}}`"),
        (false, false) => format!("`{tool}` takes no scope"),
    }
}

/// The fix for a scope `tool` can't carry. A bare class name (`read`,
/// `write`) names the whole capability class, so suggesting it would widen a
/// rule that matched nothing into one that matches every tool in the class;
/// the argument scope is offered instead when the tool takes one.
fn instead(tool: &str, scope: PermissionScope) -> String {
    let arg = (scope.arg != ArgKind::None).then(|| format!("`{tool}(pattern)`"));
    match arg {
        Some(arg) if capability_class(tool).is_some() => format!(
            "scope its argument, {arg}, instead (a bare `{tool}` names its whole capability class)"
        ),
        Some(arg) => format!("write bare `{tool}` or scope its argument, {arg}, instead"),
        None => format!("write bare `{tool}` instead"),
    }
}

/// Which scope a rule key carries, as core's matcher reads it.
#[derive(Debug, PartialEq)]
enum KeyScope {
    None,
    Arg,
    Workdir,
    /// A scope the matcher folds into the tool part (`bash(x){/y}`, an
    /// unclosed `read{x`), so the key matches no call at all.
    Malformed,
}

/// Probed through the public single-rule [`PermissionProfile`] — the same
/// door `mode::rules` grades through — rather than re-parsing core's private
/// `split_rule_key`: a glob always matches itself, so a well-formed key
/// matches its own scope text offered as the argument or as the workdir,
/// and a malformed one matches neither.
fn key_scope(entry: &str) -> KeyScope {
    let tool = rule_tool_name(entry);
    if tool.len() == entry.len() {
        return KeyScope::None;
    }
    // `None` when the key's last char isn't a one-byte closer — core reads
    // no scope there either.
    let Some(inner) = entry.get(tool.len() + 1..entry.len() - 1) else {
        return KeyScope::Malformed;
    };
    let matches = |arg, workdir| {
        PermissionProfile::new(Permission::Deny)
            .with(entry, Permission::Allow)
            .resolve_scoped(tool, arg, workdir)
            == Permission::Allow
    };
    match (matches(Some(inner), None), matches(None, Some(inner))) {
        (true, _) => KeyScope::Arg,
        (false, true) => KeyScope::Workdir,
        (false, false) => KeyScope::Malformed,
    }
}

#[cfg(test)]
mod tests {
    use super::super::mode_warn::scoped_rules;
    use super::super::{ConfigLayer, RawLayer};
    use super::*;

    /// The real `http_tool::HttpTool` is crate-private to the runtime and
    /// registered only by the executor build (with its gate), so this
    /// stand-in mirrors its `permission_scope` (a `METHOD url` text
    /// argument). Drift is caught against the real tool by
    /// `services::tests::the_scope_check_sees_the_http_tool_the_executor_registers`.
    struct Http;

    #[async_trait::async_trait]
    impl entanglement_runtime::Tool for Http {
        fn name(&self) -> std::borrow::Cow<'static, str> {
            "http".into()
        }
        async fn run(&self, _input: &str) -> anyhow::Result<String> {
            unreachable!("never run")
        }
        fn permission_scope(&self) -> PermissionScope {
            PermissionScope::arg(ArgKind::Text)
        }
    }

    /// Every registry tool `skutter` registers (the sextet, `bash`, the
    /// script-only read variants, `web_fetch` and `http`) — the tools asked,
    /// not a name list (#962).
    fn tools() -> ToolRegistry {
        use crate::host::{
            host_tools, BashTool, GlobJsonTool, GrepJsonTool, ReadRawTool, WebFetchTool,
        };
        let root = std::path::PathBuf::from("/p");
        let mut reg = host_tools(root.clone());
        reg.register(ReadRawTool::new(root.clone()));
        reg.register(GlobJsonTool::new(root.clone()));
        reg.register(GrepJsonTool::new(root.clone()));
        reg.register(BashTool::new(root));
        reg.register(WebFetchTool::new().expect("web_fetch client builds"));
        reg.register(Http);
        reg
    }

    /// `(scope, entry)` of every scoped key in `yaml` whose scope never
    /// matches a call against the host registry.
    fn unscopable(yaml: &str) -> Vec<(String, String)> {
        let layers = [RawLayer {
            layer: ConfigLayer::User,
            source: "test (user)".into(),
            doc: serde_yaml::from_str(yaml).unwrap(),
        }];
        let rules = scoped_rules(&layers);
        unscopable_rules(&rules, &tools())
            .into_iter()
            .map(|(rule, _)| (rule.scope.clone(), rule.entry.clone()))
            .collect()
    }

    fn entries(yaml: &str) -> Vec<String> {
        unscopable(yaml)
            .into_iter()
            .map(|(_, entry)| entry)
            .collect()
    }

    fn reason(entry: &str) -> String {
        scope_reason(entry, &tools()).expect("an unscopable key")
    }

    /// #878: a scoped key on a tool that never has an argument warns — an
    /// MCP tool (connected or not), and in the ceiling too — naming the fix.
    #[test]
    fn a_scoped_rule_on_an_argument_less_tool_warns() {
        let found = unscopable(
            "modes:\n  build:\n    deny: [\"mcp__gh__create(foo)\"]\n\
             permissions:\n  prompt: [\"mcp__docs__search(*)\"]\n",
        );
        assert_eq!(
            found,
            vec![
                ("build".to_string(), "mcp__gh__create(foo)".to_string()),
                (
                    "permissions".to_string(),
                    "mcp__docs__search(*)".to_string()
                ),
            ]
        );
        let why = reason("mcp__gh__create(foo)");
        assert!(why.contains("no argument to scope"), "{why}");
        assert!(why.contains("write bare `mcp__gh__create`"), "{why}");
    }

    /// #878: every tool that does take an argument keeps its scoped rules
    /// silent — the wildcard tool and a workdir scope too.
    #[test]
    fn scoped_rules_on_argument_taking_tools_do_not_warn() {
        let keys = [
            "bash(cargo *)",
            "agent(plan)",
            "agent_send(acme:*)",
            "read(src/*)",
            "write(docs/*)",
            "edit(src/*)",
            "apply_patch(src/*)",
            "glob(src/**)",
            "grep(src/*)",
            "rhai(scripts/*)",
            "web_fetch(https://docs.rs/*)",
            "http(GET https://api.github.com/*)",
            "*(*.pem)",
            "bash{/tmp/*}",
            "mcp__gh__create",
        ];
        let list = keys.map(|k| format!("\"{k}\"")).join(", ");
        let found = entries(&format!("modes:\n  build:\n    allow: [{list}]\n"));
        assert!(found.is_empty(), "{found:?}");
    }

    /// #962: the registry decides — an embedder tool that declares an
    /// argument takes a scoped rule, the same name unregistered does not.
    #[test]
    fn the_registered_tool_decides_whether_a_scope_can_match() {
        struct Doc;
        #[async_trait::async_trait]
        impl crate::tools::Tool for Doc {
            fn name(&self) -> std::borrow::Cow<'static, str> {
                "mcp__kb__doc".into()
            }
            async fn run(&self, _input: &str) -> anyhow::Result<String> {
                Ok(String::new())
            }
            fn permission_scope(&self) -> PermissionScope {
                PermissionScope::arg(ArgKind::Path)
            }
        }
        let rules = [ScopedRule {
            source: "test".into(),
            scope: "build".into(),
            entry: "mcp__kb__doc(notes/*)".into(),
        }];
        assert_eq!(unscopable_rules(&rules, &ToolRegistry::new()).len(), 1);
        let mut tools = ToolRegistry::new();
        tools.register(Doc);
        assert!(unscopable_rules(&rules, &tools).is_empty());
    }

    /// A hint never suggests a bare class name (it would match the whole
    /// class) nor a scope the tool can't carry (it would only warn again).
    #[test]
    fn hints_suggest_only_forms_that_match() {
        let why = reason("read{/tmp/*}");
        assert!(why.contains("`read(pattern)`"), "{why}");
        assert!(!why.contains("write bare `read`"), "{why}");
        let why = reason("read(a){b}");
        assert!(why.contains("`read(pattern)`"), "{why}");
        assert!(!why.contains("read{workdir}"), "{why}");
        let why = reason("bash(git *){/x}");
        assert!(why.contains("`bash(pattern)` or `bash{workdir}`"), "{why}");
    }

    /// #881: a workdir scope on a tool without one warns — a known tool
    /// and an MCP tool alike, in a mode rule and in the ceiling.
    #[test]
    fn a_workdir_scope_on_a_tool_without_a_workdir_warns() {
        let found = entries(
            "modes:\n  build:\n    deny: [\"read{/tmp/*}\", \"mcp__x__y{/a}\"]\n\
             permissions:\n  prompt: [\"grep{src}\"]\n",
        );
        assert_eq!(found, ["read{/tmp/*}", "mcp__x__y{/a}", "grep{src}"]);
        let why = reason("read{/tmp/*}");
        assert!(why.contains("no workdir to scope"), "{why}");
        assert!(why.contains("`read(pattern)`"), "{why}");
        let why = reason("mcp__x__y{/a}");
        assert!(why.contains("write bare `mcp__x__y` instead"), "{why}");
    }

    /// #881: a tool with a workdir and the `*` wildcard keep their workdir
    /// scopes silent, as do braces or parens nested inside a well-formed
    /// scope.
    #[test]
    fn well_formed_workdir_scopes_do_not_warn() {
        let keys = [
            "bash{/tmp/*}",
            "*{/tmp/*}",
            "bash{a(b)}",
            "read(a{b})",
            "bash()",
        ];
        let list = keys.map(|k| format!("\"{k}\"")).join(", ");
        let found = entries(&format!("modes:\n  build:\n    allow: [{list}]\n"));
        assert!(found.is_empty(), "{found:?}");
    }

    /// #881: core's matcher reads one closing scope per key, so a combined,
    /// reordered, unclosed or trailing-text key matches nothing and warns —
    /// a multi-byte last char included, without panicking on the slice.
    #[test]
    fn a_malformed_scope_warns() {
        let keys = [
            "bash(git *){/x}",
            "bash{/x}(git *)",
            "read{/tmp/*",
            "bash(git *",
            "bash(",
            "bash{",
            "bash(x)y",
            "read(é",
            "*(x){y}",
        ];
        let list = keys.map(|k| format!("\"{k}\"")).join(", ");
        let found = entries(&format!("modes:\n  build:\n    deny: [{list}]\n"));
        assert_eq!(found, keys);
        let why = reason("bash(git *){/x}");
        assert!(why.contains("at most one scope"), "{why}");
    }

    /// The probe agrees with core's matcher on every shape above.
    #[test]
    fn key_scope_reads_keys_like_the_matcher() {
        assert_eq!(key_scope("bash"), KeyScope::None);
        assert_eq!(key_scope("bash(git *)"), KeyScope::Arg);
        assert_eq!(key_scope("read(a{b})"), KeyScope::Arg);
        assert_eq!(key_scope("bash{/tmp/*}"), KeyScope::Workdir);
        assert_eq!(key_scope("bash{a(b)}"), KeyScope::Workdir);
        assert_eq!(key_scope("*{/x}"), KeyScope::Workdir);
        assert_eq!(key_scope("bash(git *){/x}"), KeyScope::Malformed);
        assert_eq!(key_scope("read(é"), KeyScope::Malformed);
    }
}
