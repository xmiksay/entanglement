//! [`AliasTool`] — a skill-declared alias: a renamed/preset-args wrapper over
//! an existing tool (`skills::tools::SkillToolDef::Alias`), or the
//! rewrite-to-`rhai` a rhai-backed skill tool
//! ([`SkillToolDef::Rhai`][crate::skills::tools::SkillToolDef::Rhai]) is
//! sugar for. Both share one property this type exists to guarantee: an
//! alias must not launder permission through its own namespaced name — it
//! grades and executes exactly as if the model had called the tool it wraps
//! directly.
//!
//! The mechanism is [`Tool::alias_rewrite`]: `crate::tool_runner::dispatch`
//! consults it *before* permission resolution and, when it returns
//! `Some((target, merged_input))`, rewrites the in-flight `(tool, input)`
//! pair to the wrapped tool's own name and its preset-args-merged input —
//! every downstream decision (grading, grant lookup/record, the escape-root
//! gate, the `ToolExec`/`ToolRequest` the user approves, execution) then
//! proceeds exactly as it would for a direct call to that tool. `run` below
//! is a defensive fallback only (a real registry tool held as `delegate`
//! forwards to it; a pseudo-tool target like `rhai` — never a
//! [`crate::tools::ToolRegistry`] entry — has no delegate and errors,
//! documenting that this path should be unreachable in normal dispatch).

use std::borrow::Cow;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::capability::{runtime_owned, Capability};
use crate::tools::Tool;

pub struct AliasTool {
    name: String,
    description: String,
    schema: Value,
    target: String,
    preset: Map<String, Value>,
    /// The target's own `Arc<dyn Tool>`, when it's a real registry entry at
    /// alias-construction time — `None` for a pseudo-tool target (`rhai`,
    /// `poll`, …) that dispatches by name rather than living in the
    /// registry.
    delegate: Option<Arc<dyn Tool>>,
}

impl AliasTool {
    pub fn new(
        name: String,
        description: String,
        schema: Value,
        target: String,
        preset: Map<String, Value>,
        delegate: Option<Arc<dyn Tool>>,
    ) -> Self {
        Self {
            name,
            description,
            schema,
            target,
            preset,
            delegate,
        }
    }
}

/// Merge `preset` onto the caller's own JSON object input, preset winning on
/// a key collision (a preset arg is a fixed override, not a mere default).
/// Malformed/empty/non-object input degrades to "no caller args" rather than
/// erroring — a missing *required* param the preset doesn't cover is already
/// caught by P4's pre-dispatch schema validation before this ever runs.
fn merge_input(input: &str, preset: &Map<String, Value>) -> String {
    let mut obj: Map<String, Value> = if input.trim().is_empty() {
        Map::new()
    } else {
        serde_json::from_str::<Value>(input)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default()
    };
    for (k, v) in preset {
        obj.insert(k.clone(), v.clone());
    }
    Value::Object(obj).to_string()
}

#[async_trait]
impl Tool for AliasTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Owned(self.name.clone())
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    async fn run(&self, input: &str) -> Result<String> {
        match &self.delegate {
            Some(target) => target.run(&merge_input(input, &self.preset)).await,
            None => anyhow::bail!(
                "alias `{}` targets `{}`, which has no registry entry to delegate to \
                 directly — it should only ever run via crate::tool_runner::dispatch's \
                 alias_rewrite, which rewrites the call before this is reached",
                self.name,
                self.target
            ),
        }
    }

    fn alias_rewrite(&self, input: &str) -> Option<(String, String)> {
        Some((self.target.clone(), merge_input(input, &self.preset)))
    }

    /// The **target's** capability, never the alias's own name — `alias_rewrite`
    /// resolves before grading, so if this returned a capability of its own it
    /// could launder a call through a namespaced alias name (ADR-0207 §3
    /// forbids exactly that). `runtime_owned` covers a pseudo-tool target
    /// (`rhai`, which has no registry entry); `delegate` covers a real
    /// registry tool. Neither reachable is the fail-safe `Write` default —
    /// documenting, not guessing, an alias whose target genuinely can't be
    /// resolved at construction time.
    fn capabilities(&self) -> &'static [Capability] {
        if let Some(bits) = runtime_owned(&self.target) {
            return bits;
        }
        match &self.delegate {
            Some(target) => target.capabilities(),
            None => &[Capability::Write],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_names::POLL_TOOL;
    use crate::tools::ToolRegistry;

    struct Echo;
    #[async_trait]
    impl Tool for Echo {
        fn name(&self) -> Cow<'static, str> {
            Cow::Borrowed("echo")
        }
        fn schema(&self) -> Value {
            serde_json::json!({"type":"object","properties":{"path":{"type":"string"},"extra":{"type":"string"}},"required":["path","extra"]})
        }
        async fn run(&self, input: &str) -> Result<String> {
            Ok(input.to_string())
        }
    }

    fn preset(pairs: &[(&str, &str)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
            .collect()
    }

    #[test]
    fn merge_input_preset_wins_on_collision() {
        let out = merge_input(r#"{"path":"model.txt"}"#, &preset(&[("path", "README.md")]));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["path"], "README.md");
    }

    #[test]
    fn merge_input_tolerates_empty_and_malformed_input() {
        let out = merge_input("", &preset(&[("path", "README.md")]));
        assert_eq!(out, r#"{"path":"README.md"}"#);
        let out2 = merge_input("not json", &preset(&[("path", "README.md")]));
        assert_eq!(out2, r#"{"path":"README.md"}"#);
    }

    #[tokio::test]
    async fn alias_rewrite_targets_the_underlying_tool_with_merged_input() {
        let alias = AliasTool::new(
            "skill__x__quick_read".to_string(),
            "d".to_string(),
            serde_json::json!({"type":"object","properties":{"extra":{"type":"string"}}}),
            "echo".to_string(),
            preset(&[("path", "README.md")]),
            None,
        );
        let (target, merged) = alias.alias_rewrite(r#"{"extra":"x"}"#).unwrap();
        assert_eq!(target, "echo");
        let v: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(v["path"], "README.md");
        assert_eq!(v["extra"], "x");
    }

    #[test]
    fn capability_resolves_a_runtime_owned_pseudo_tool_target_with_no_delegate() {
        let alias = AliasTool::new(
            "skill__x__wait".to_string(),
            "d".to_string(),
            serde_json::json!({"type":"object"}),
            POLL_TOOL.to_string(),
            Map::new(),
            None,
        );
        assert_eq!(alias.capabilities(), &[Capability::Control]);
    }

    #[test]
    fn capability_resolves_a_real_registry_delegate_never_its_own() {
        let mut registry = ToolRegistry::new();
        registry.register(Echo);
        let delegate = registry.get("echo");
        let alias = AliasTool::new(
            "skill__x__quick_read".to_string(),
            "d".to_string(),
            serde_json::json!({"type":"object"}),
            "echo".to_string(),
            Map::new(),
            delegate,
        );
        // Echo doesn't override `capabilities`, so it rides the trait's
        // fail-safe `Write` default — asserted here so a change to Echo's
        // own capability (or the trait default) breaks this test loudly
        // instead of silently proving nothing.
        assert_eq!(alias.capabilities(), &[Capability::Write]);
    }

    #[test]
    fn capability_falls_back_to_write_for_an_unresolvable_target() {
        let alias = AliasTool::new(
            "skill__x__mystery".to_string(),
            "d".to_string(),
            serde_json::json!({"type":"object"}),
            "not_a_real_tool".to_string(),
            Map::new(),
            None,
        );
        assert_eq!(alias.capabilities(), &[Capability::Write]);
    }

    #[tokio::test]
    async fn run_delegates_to_a_real_registry_target_when_present() {
        let mut registry = ToolRegistry::new();
        registry.register(Echo);
        let delegate = registry.get("echo");
        let alias = AliasTool::new(
            "skill__x__quick_read".to_string(),
            "d".to_string(),
            serde_json::json!({"type":"object","properties":{}}),
            "echo".to_string(),
            preset(&[("path", "README.md")]),
            delegate,
        );
        let out = alias.run(r#"{"extra":"x"}"#).await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["path"], "README.md");
    }

    #[tokio::test]
    async fn run_errors_when_the_target_has_no_registry_delegate() {
        let alias = AliasTool::new(
            "skill__x__run_lint".to_string(),
            "d".to_string(),
            serde_json::json!({"type":"object","properties":{}}),
            "rhai".to_string(),
            preset(&[("script", "// x")]),
            None,
        );
        let err = alias.run("{}").await.unwrap_err();
        assert!(format!("{err:#}").contains("rhai"), "{err:#}");
    }
}
