//! ADR-0227: every script binding climbs the same dispatch seam as a direct
//! call — hooks and `FileChange` are observable from a script, generated
//! `mcp::` namespaces reach a real (stub) MCP server graded under the real
//! tool name, and `http()` grades `{METHOD} {url}` with the credentialed
//! once-per-host approval.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use entanglement_core::{OutEvent, Permission};
use entanglement_runtime::http_gate::HttpGate;
use entanglement_runtime::mcp::AvailableMcp;
use entanglement_runtime::tool_names::RHAI_TOOL;

use super::unified_harness::{run, Setup};
use super::TempDir;

fn rhai_result(events: &[OutEvent]) -> String {
    events
        .iter()
        .find_map(|e| match e {
            OutEvent::ToolOutput { tool, output, .. } if tool == RHAI_TOOL => Some(output.clone()),
            _ => None,
        })
        .expect("a rhai result")
}

/// `(tool, shown input)` of every nested approval card raised (the `rhai`
/// launch's own card excluded).
fn requests(events: &[OutEvent]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolRequest { tool, input, .. } if tool != RHAI_TOOL => {
                Some((tool.clone(), input.clone()))
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn hooks_and_file_change_are_observable_from_a_script() {
    let dir = TempDir::new("unified-hooks");
    let marker = dir.path.join("post.marker");
    let mut setup = Setup::mode(Permission::Allow, &[], 0);
    setup.hooks = serde_yaml::from_str(&format!(
        "pre_tool_use:\n  - command: \"echo vetoed by policy >&2; exit 1\"\n    tools: [edit]\n\
         post_tool_use:\n  - command: \"touch '{}'\"\n    tools: [write]\n",
        marker.display()
    ))
    .unwrap();
    let events = run(
        r#"write("a.txt", "x");
           let r = "not vetoed";
           try { edit("a.txt", "x", "y"); } catch (e) { r = e; }
           r"#,
        &dir.path,
        setup,
    )
    .await;
    let out = rhai_result(&events);
    assert!(out.contains("vetoed"), "the veto reaches the script: {out}");
    assert_eq!(
        std::fs::read_to_string(dir.path.join("a.txt")).unwrap(),
        "x"
    );
    assert!(marker.exists(), "post_tool_use ran for the script's write");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::FileChange { path, .. } if path.ends_with("a.txt"))),
        "the script's write emitted a FileChange"
    );
}

#[tokio::test]
async fn http_grades_method_and_url_against_a_scoped_rule() {
    let dir = TempDir::new("unified-http-rule");
    // Loopback is refused by the SSRF guard after grading, so nothing is
    // ever sent — the test only watches which call needed approval.
    let setup = Setup::mode(Permission::Ask, &["http(GET https://127.0.0.1/*)"], 0);
    let events = run(
        r#"let a = ""; let b = "";
           try { http("GET", "https://127.0.0.1/r"); } catch (e) { a = e; }
           try { http("POST", "https://127.0.0.1/r"); } catch (e) { b = e; }
           a + " | " + b"#,
        &dir.path,
        setup,
    )
    .await;
    let reqs = requests(&events);
    assert_eq!(reqs.len(), 1, "only the POST asks: {reqs:?}");
    assert_eq!(reqs[0].0, "http (rhai)");
}

fn credentialed() -> Arc<HttpGate> {
    Arc::new(HttpGate::new(
        serde_yaml::from_str("127.0.0.1: {headers: {X-Key: s3cret}}").unwrap(),
    ))
}

#[tokio::test]
async fn a_credentialed_call_asks_once_per_host_even_when_allowed() {
    let dir = TempDir::new("unified-http-host");
    let mut setup = Setup::mode(Permission::Allow, &[], 0);
    setup.gate = credentialed();
    let events = run(
        r#"let n = 0;
           for i in 0..2 { try { http("GET", "https://127.0.0.1/" + i); } catch (e) { n += 1; } }
           try { http("GET", "http://127.0.0.1/plain"); } catch (e) { n += 1; }
           n"#,
        &dir.path,
        setup,
    )
    .await;
    let reqs = requests(&events);
    assert_eq!(reqs.len(), 1, "one approval covers the host: {reqs:?}");
    assert!(
        reqs[0].1.contains("authenticated for 127.0.0.1"),
        "{reqs:?}"
    );
    assert!(
        !rhai_result(&events).contains("s3cret"),
        "a credential never reaches the script"
    );
}

#[tokio::test]
async fn unattended_refuses_a_credentialed_call_unless_a_rule_allows_it() {
    let dir = TempDir::new("unified-http-auto");
    let mut setup = Setup::mode(Permission::Allow, &["exec"], 60);
    setup.gate = credentialed();
    let script =
        r#"let r = ""; try { http("GET", "https://127.0.0.1/x"); } catch (e) { r = e; } r"#;
    let events = run(script, &dir.path, setup).await;
    assert!(requests(&events).is_empty(), "nobody to ask");
    let out = rhai_result(&events);
    assert!(out.contains("unattended"), "{out}");

    let mut setup = Setup::mode(Permission::Allow, &["http(GET https://127.0.0.1/*)"], 60);
    setup.gate = credentialed();
    let events = run(script, &dir.path, setup).await;
    assert!(requests(&events).is_empty());
    let out = rhai_result(&events);
    assert!(
        !out.contains("unattended"),
        "an explicit rule lets it through to the network layer: {out}"
    );
}

#[cfg(all(feature = "mcp-http", feature = "serve"))]
mod mcp_namespace {
    use super::*;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::{Json, Router};
    use entanglement_core::Catalog;
    use entanglement_runtime::mcp::{McpServerConfig, McpToolCache};
    use serde_json::{json, Value};

    /// A streamable-HTTP MCP server with one `call` tool (a Rhai reserved
    /// word) echoing its `msg`.
    async fn stub(Json(req): Json<Value>) -> Response {
        let id = req.get("id").cloned();
        match req.get("method").and_then(Value::as_str).unwrap_or("") {
            "initialize" => Json(json!({"jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": "2025-03-26", "serverInfo": {"name": "stub"}}}))
            .into_response(),
            "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
            "tools/list" => Json(json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [
                {"name": "call", "description": "echo", "inputSchema": {"type": "object",
                 "properties": {"msg": {"type": "string"}}, "required": ["msg"]}}]}}))
            .into_response(),
            "tools/call" => {
                let msg = req["params"]["arguments"]["msg"].as_str().unwrap_or("?");
                Json(json!({"jsonrpc": "2.0", "id": id, "result": {
                    "content": [{"type": "text", "text": format!("echo:{msg}")}]}}))
                .into_response()
            }
            _ => (StatusCode::BAD_REQUEST, "unknown").into_response(),
        }
    }

    /// An unconnected server whose last-seen tools generate `mcp::stub_srv::
    /// call_`; the first call connects it lazily and is graded (and carded)
    /// under its real `mcp__stub-srv__call` name.
    #[tokio::test]
    async fn a_namespaced_call_connects_lazily_and_grades_under_the_real_name() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, Router::new().route("/mcp", post(stub))).await;
        });
        let dir = TempDir::new("unified-mcp");
        let cache = McpToolCache::at(Some(dir.path.join("mcp-tools.yml")));
        cache
            .record("stub-srv", &url, &["call".to_string()])
            .unwrap();
        let cfg = McpServerConfig {
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some(url),
            headers: HashMap::new(),
            disabled: false,
            capabilities: HashMap::new(),
            oauth: None,
            description: None,
        };
        let mut setup = Setup::mode(Permission::Ask, &[], 0);
        setup.avail = AvailableMcp::from_config(
            &Catalog { providers: vec![] },
            &HashMap::from([("stub-srv".to_string(), cfg)]),
            vec![],
        )
        .with_tool_cache(cache);
        let events = tokio::time::timeout(
            Duration::from_secs(20),
            run(r#"mcp::stub_srv::call_(#{msg: "hi"})"#, &dir.path, setup),
        )
        .await
        .unwrap();
        let reqs = requests(&events);
        assert_eq!(reqs.len(), 1, "{reqs:?}");
        assert_eq!(reqs[0].0, "mcp__stub-srv__call (rhai)");
        assert!(rhai_result(&events).contains("echo:hi"), "{events:?}");
        // ADR-0233: the namespace call is a nested call like any binding —
        // reported under its real name, its args as key names only.
        let nested: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                OutEvent::NestedCall { tool, args, .. } => Some((tool.as_str(), args.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(nested, [("mcp__stub-srv__call", "msg")]);
    }
}

/// B1: a credential's `${VAR}` is hidden from every child — a script's
/// `bash` and a hook both see it empty.
#[tokio::test]
async fn an_http_auth_secret_never_reaches_a_child_process() {
    let var = "ENTANGLEMENT_TEST_UNIFIED_GH_TOKEN";
    std::env::set_var(var, "s3cret-value");
    let dir = TempDir::new("unified-secret");
    let leaked = dir.path.join("hook.out");
    let mut setup = Setup::mode(Permission::Allow, &[], 0);
    setup.bash = true;
    setup.gate = Arc::new(HttpGate::new(
        serde_yaml::from_str(&format!(
            "api.example.com: {{headers: {{X-Key: \"${{{var}}}\"}}}}"
        ))
        .unwrap(),
    ));
    setup.hooks = serde_yaml::from_str(&format!(
        "post_tool_use:\n  - command: \"printf '%s' \\\"${var}\\\" > '{}'\"\n",
        leaked.display()
    ))
    .unwrap();
    let events = run(
        &format!(r#"bash("printf '[%s]' \"${var}\"")"#),
        &dir.path,
        setup,
    )
    .await;
    let out = rhai_result(&events);
    assert!(out.contains("[]"), "bash saw the secret: {out}");
    assert!(!out.contains("s3cret"), "{out}");
    assert_eq!(
        std::fs::read_to_string(&leaked).unwrap(),
        "",
        "the hook saw it"
    );
}

/// S6: a nested call outliving the script's budget is cut into a catchable
/// error that says the outcome is unknown.
#[tokio::test]
async fn the_bridge_clock_cuts_a_slow_nested_call_into_a_catchable_error() {
    let dir = TempDir::new("unified-slow");
    let mut setup = Setup::mode(Permission::Allow, &[], 0);
    setup.slow_grep = true;
    setup.timeout = 2;
    let events = run(
        r#"let r = "not cut"; try { grep("x"); } catch (e) { r = "caught: " + e; } r"#,
        &dir.path,
        setup,
    )
    .await;
    let out = rhai_result(&events);
    assert!(
        out.contains("caught: ") && out.contains("may or may not"),
        "{out}"
    );
}

/// S3: a project-layer `http(...)` allow grades but never unlocks a
/// credentialed call in an unattended mode.
#[tokio::test]
async fn a_project_layer_http_rule_does_not_unlock_a_credentialed_call() {
    let dir = TempDir::new("unified-http-project");
    let rule = "http(GET https://127.0.0.1/*)";
    let mut setup = Setup::mode(Permission::Allow, &[rule], 60);
    setup.gate = Arc::new(
        HttpGate::new(serde_yaml::from_str("127.0.0.1: {headers: {X-Key: k}}").unwrap())
            .with_untrusted_rules(
                [(
                    "build".to_string(),
                    [(rule.to_string(), String::new())].into(),
                )]
                .into(),
            ),
    );
    let script =
        r#"let r = ""; try { http("GET", "https://127.0.0.1/x"); } catch (e) { r = e; } r"#;
    let out = rhai_result(&run(script, &dir.path, setup).await);
    assert!(out.contains("unattended"), "{out}");
}

/// S9: approving the host card for a credentialed call the mode grades
/// `Ask` also covers an identical repeat in the same run.
#[tokio::test]
async fn a_host_approval_also_covers_the_repeat_of_an_ask_graded_call() {
    let dir = TempDir::new("unified-http-host-ask");
    let mut setup = Setup::mode(Permission::Ask, &[], 0);
    setup.gate = credentialed();
    let events = run(
        r#"for i in 0..2 { try { http("GET", "https://127.0.0.1/same"); } catch (e) {} } 1"#,
        &dir.path,
        setup,
    )
    .await;
    let reqs = requests(&events);
    assert_eq!(reqs.len(), 1, "{reqs:?}");
}
