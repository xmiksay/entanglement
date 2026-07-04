//! Tool registry. Concrete tools (`ReadFile`, `WriteFile`, `Bash`) land in a
//! later phase; the trait + registry are in place so the engine can already
//! dispatch, advertise tools to the model, and report unknown tools.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

use crate::llm::{ToolCall, ToolSpec};

/// A single capability the engine can execute on the host.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;

    /// Short description surfaced to the model. Default empty so simple tools
    /// don't have to bother.
    fn description(&self) -> &str {
        ""
    }

    async fn run(&self, input: &str) -> anyhow::Result<String>;
}

/// Named lookup of tools. Cloning is cheap (tools are shared behind `Arc`), so
/// one registry built in config is cloned into every session.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<&'static str, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: impl Tool + 'static) {
        let name = tool.name();
        self.tools.insert(name, Arc::new(tool));
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Specs advertised to the model (for the `tools` field of an LLM request).
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .values()
            .map(|t| ToolSpec::new(t.name(), t.description()))
            .collect()
    }

    /// Execute a model-requested [`ToolCall`]. Unknown tools yield a string the
    /// engine feeds back to the model rather than erroring the run.
    pub async fn execute(&self, call: &ToolCall) -> String {
        match self.tools.get(call.name.as_str()) {
            Some(tool) => match tool.run(&call.input).await {
                Ok(out) => out,
                Err(e) => format!("tool `{}` failed: {e}", call.name),
            },
            None => format!("unknown tool: `{}`", call.name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;
    #[async_trait]
    impl Tool for Echo {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn description(&self) -> &str {
            "echo its input"
        }
        async fn run(&self, input: &str) -> anyhow::Result<String> {
            Ok(input.to_string())
        }
    }

    #[tokio::test]
    async fn runs_registered_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(Echo);
        let out = reg
            .execute(&ToolCall {
                id: "1".into(),
                name: "echo".into(),
                input: "hi".into(),
            })
            .await;
        assert_eq!(out, "hi");
    }

    #[tokio::test]
    async fn unknown_tool_is_reported_not_fatal() {
        let reg = ToolRegistry::new();
        let out = reg
            .execute(&ToolCall {
                id: "1".into(),
                name: "nope".into(),
                input: "".into(),
            })
            .await;
        assert!(out.contains("unknown tool"));
    }

    #[test]
    fn specs_advertise_name_and_description() {
        let mut reg = ToolRegistry::new();
        reg.register(Echo);
        let specs = reg.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "echo");
        assert_eq!(specs[0].description, "echo its input");
    }
}
