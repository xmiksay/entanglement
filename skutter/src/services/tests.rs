use std::sync::Arc;

use entanglement_core::{EngineConfig, Holly};
use entanglement_runtime::http_gate::HttpGate;

use super::spawn_then_check_scopes;
use crate::config::Config;
use crate::engine_setup::default_tools::startup_registry;
use crate::tool_runner::ToolExecutor;

/// A config whose only user layer holds `allow` in the `build` mode.
fn config_allowing(dir: &std::path::Path, allow: &[&str]) -> Config {
    let list = allow
        .iter()
        .map(|k| format!("\"{k}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let file = dir.join("config.yml");
    std::fs::write(&file, format!("modes:\n  build:\n    allow: [{list}]\n")).unwrap();
    Config::load_with(dir, Some(&file)).unwrap()
}

/// #962: skutter's startup order — the startup registry, then the executor
/// build (which registers the script-only `http` tool) — checks scoped rules
/// only once `http` is registered, so an `http(GET …)` rule stays silent
/// while a scope no tool takes still warns.
#[tokio::test]
async fn the_scope_check_sees_the_http_tool_the_executor_registers() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_allowing(
        dir.path(),
        &["http(GET https://api.github.com/*)", "mcp__gh__create(foo)"],
    );
    let tools = startup_registry().shared();
    // The ordering hazard this guards: before the build, `http` is unknown.
    assert_eq!(
        config.warn_unscopable_rules(&tools.read().unwrap()),
        2,
        "`http` is registered by the executor build, not at startup"
    );

    let holly = Holly::spawn(EngineConfig::default());
    let executor = ToolExecutor::builder(&holly, tools.clone())
        .http_gate(Arc::new(HttpGate::new(Default::default())));
    let (handle, warned) = spawn_then_check_scopes(executor, &tools, &config);
    handle.abort();
    assert!(tools.read().unwrap().contains("http"));
    assert_eq!(warned, 1, "only `mcp__gh__create(foo)` can never match");
}
